//! `POST /v1/deploy*` — agent-bundle deploy (dev-plan/28).
//!
//! Lets a laptop ship `.thclaws/` (skills, MCP, plugins, data/,
//! agent_workflow/, AGENTS.md, settings.json, …) to a running pod and
//! have the pod's next agent turn pick up the new state. All runtime
//! state lives under `.thclaws/state/` on the pod side (sessions, team,
//! kms, usage, workflow run-state) and is preserved across deploys.
//!
//! Three endpoints:
//!
//! - `POST /v1/deploy/manifest` — client sends `{files: [{path,sha256}]}`,
//!   server replies with `{missing: [paths]}` so the client can ship a
//!   diff tar instead of the whole bundle.
//! - `POST /v1/deploy/files` — accepts a streaming tar (any subset of
//!   the workspace `.thclaws/`), extracts to a scratch dir, atomically
//!   swaps into the live `.thclaws/`, preserves state/ and memory/.
//!   Phase-1 clients hit this directly with a full bundle; Phase-2
//!   clients call /manifest first and ship only `missing`.
//! - `POST /v1/deploy` — alias for `/v1/deploy/files` for orchestrators
//!   that only want one URL.
//!
//! Response on `/files` is SSE: `event: extracted` → `event: reloaded`
//! → `event: done` (with stats payload). Errors surface as
//! `event: error`.
//!
//! Auth via the same Bearer token as the rest of `/v1/*`.

use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::convert::Infallible;
use std::path::{Path, PathBuf};

use super::errors::OpenAiError;
use super::AuthOk;

/// Top-level entries from `.thclaws/` the deploy is allowed to touch.
/// Anything else in the uploaded tar is rejected before extraction.
const ALLOWED_TOP_LEVEL: &[&str] = &[
    "settings.json",
    "mcp.json",
    "AGENTS.md",
    "agents",
    "skills",
    "commands",
    "plugins",
    "plugins.json",
    "prompt",
    "rules",
    "data",
    "agent_workflow",
];

/// Tar-path prefix marking entries that land at the workspace root
/// (alongside project code) rather than inside `.thclaws/`. Client
/// sets this via `deploy_client::ROOT_PREFIX`. Only the names in
/// [`PROJECT_ROOT_ALLOWED`] are accepted under it.
const ROOT_PREFIX: &str = "__root__";

/// Files the deploy is allowed to write at the workspace root (one
/// level outside `.thclaws/`). Keep this tight — anything here can
/// influence the pod's system prompt assembly without sitting in the
/// atomically-swapped `.thclaws/` tree.
const PROJECT_ROOT_ALLOWED: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// Top-level entries the deploy must NEVER touch — these live on the
/// pod side and survive a deploy. Even if the client uploads them, the
/// extract phase skips them. (Defense in depth — the client should
/// already be filtering them out per dev-plan/28's "what gets uploaded
/// vs skipped" table.)
const PRESERVE_ON_POD: &[&str] = &["state", "memory", ".env"];

/// Maximum tar size we'll accept on `/v1/deploy/files`. 100 MB matches
/// the plan's quota guard.
const MAX_DEPLOY_BYTES: usize = 100 * 1024 * 1024;

// ── /manifest ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ManifestRequest {
    pub files: Vec<ManifestEntry>,
}

#[derive(Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    pub sha256: String,
}

/// `POST /v1/deploy/manifest`
///
/// Compare the client's `{path, sha256}` list against what currently
/// sits under the daemon's `.thclaws/`. Return the list of paths the
/// pod is missing or has at a different hash — that's what the client
/// should include in its follow-up `/v1/deploy/files` tar.
///
/// Files the pod has but the client didn't list (e.g. pod-authored KMS
/// pages from auto-learn) are left untouched by deploy — diff is
/// one-way "what does the pod need from me", not "make pod match
/// client exactly". The plan calls this out explicitly.
pub async fn deploy_manifest(
    _auth: AuthOk,
    Json(req): Json<ManifestRequest>,
) -> Result<Response, Response> {
    if req.files.len() > 10_000 {
        return Err(bad_request(
            "manifest exceeds 10000 entries — split the deploy or trim the bundle",
            "manifest_too_large",
        ));
    }

    let workspace = resolve_workspace().map_err(internal_err)?;
    let thclaws_root = workspace.join(".thclaws");

    let mut missing: Vec<String> = Vec::with_capacity(req.files.len());
    for entry in &req.files {
        // Path validation — same rules the extract step enforces.
        // Reject early so the client can see a clear error from the
        // manifest call rather than hitting it deeper in the pipeline.
        if !is_safe_rel_path(&entry.path) {
            return Err(bad_request(
                format!(
                    "manifest path '{}' is unsafe (absolute, contains '..', or hits a preserved dir)",
                    entry.path
                ),
                "invalid_path",
            ));
        }
        let abs = match resolve_entry_path(&workspace, &thclaws_root, &entry.path) {
            Ok(p) => p,
            Err(msg) => return Err(bad_request(msg, "invalid_path")),
        };
        match file_sha256(&abs) {
            Ok(Some(have)) if have.eq_ignore_ascii_case(&entry.sha256) => {
                // Pod already has the file at the same hash — skip.
            }
            Ok(_) | Err(_) => {
                missing.push(entry.path.clone());
            }
        }
    }

    Ok(Json(json!({
        "missing": missing,
        "manifest_size": req.files.len(),
    }))
    .into_response())
}

// ── /deploy + /deploy/files ───────────────────────────────────────────

/// `POST /v1/deploy` and `POST /v1/deploy/files`
///
/// Body: any `application/x-tar` payload (uncompressed) — full bundle
/// (Phase 1) or just the `missing` files from a prior `/manifest`
/// response (Phase 2). Same handler either way.
///
/// Atomically swaps into `<workspace>/.thclaws/`, preserving
/// sessions/team/memory from the live dir. Response is SSE.
///
/// Pruning: `.thclaws.prev-*` directories older than 7 days are best-
/// effort-deleted after the swap. Not configurable here (kept simple
/// for Phase 1 — bump to a config knob if the default isn't right).
pub async fn deploy_files(_auth: AuthOk, body: Bytes) -> Result<Response, Response> {
    if body.len() > MAX_DEPLOY_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(OpenAiError::invalid_request(
                format!(
                    "deploy bundle exceeds {} MB limit (got {} bytes)",
                    MAX_DEPLOY_BYTES / (1024 * 1024),
                    body.len()
                ),
                "bundle_too_large",
            )),
        )
            .into_response());
    }

    let workspace = resolve_workspace().map_err(internal_err)?;

    let stream = async_stream::stream! {
        // 1a) Seed scratch with the current live .thclaws so files NOT in
        //     this deploy's tar (because the manifest handshake said the
        //     pod already has them) survive the swap. Without this seed
        //     step, a Phase-2 diff deploy that only contains one
        //     changed file would wipe everything else. Skipped on the
        //     first-ever deploy when live doesn't exist yet.
        let scratch_id = uuid::Uuid::new_v4().to_string();
        let scratch = workspace.join(format!(".thclaws.deploy-{scratch_id}"));
        let live = workspace.join(".thclaws");
        if live.exists() {
            if let Err(e) = seed_scratch_from_live(&live, &scratch).await {
                yield ok_event("error", json!({
                    "stage": "seed_scratch",
                    "message": format!("{e}"),
                }));
                return;
            }
        }

        // 1b) Extract the tar on top of the seeded scratch. Tar entries
        //     overwrite same-path files; everything else stays as it was
        //     in the live snapshot.
        match extract_tar(&body, &scratch).await {
            Ok(stats) => {
                yield ok_event("extracted", json!({
                    "files": stats.files,
                    "bytes": stats.bytes,
                    "scratch": scratch.display().to_string(),
                }));
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&scratch);
                yield ok_event("error", json!({
                    "stage": "extract",
                    "message": format!("{e}"),
                }));
                return;
            }
        }

        // 2) swap — scratch already has the merged tree (live + diff),
        //    so this step is just the two-rename atomic move. Live →
        //    prev for rollback; scratch → live to commit.
        let prev = workspace.join(format!(".thclaws.prev-{scratch_id}"));
        match swap_dir(&scratch, &live, &prev).await {
            Ok(()) => {
                yield ok_event("swapped", json!({
                    "live": live.display().to_string(),
                    "prev": prev.display().to_string(),
                }));
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&scratch);
                yield ok_event("error", json!({
                    "stage": "swap",
                    "message": format!("{e}"),
                }));
                return;
            }
        }

        // 3) prune old .thclaws.prev-* dirs (best effort, 7 day default)
        let prune_age = 7 * 24 * 3600;
        let pruned = prune_prev_dirs(&workspace, prune_age).unwrap_or(0);
        if pruned > 0 {
            yield ok_event("pruned", json!({ "count": pruned }));
        }

        // 4) signal reload to the running runtime. /agent/run builds
        //    its runtime per-request via build_runtime_for_workspace,
        //    so skills + MCP + KMS pick up the new state on the next
        //    request automatically. Nothing else is currently cached
        //    above that boundary in --serve mode; if/when we cache a
        //    process-wide runtime, this is the hook to invalidate.
        yield ok_event("reloaded", json!({
            "skills": count_dir(&live.join("skills")),
            "mcp": mcp_count(&live),
            "kms": kms_names(&live),
        }));

        yield ok_event("done", json!({ "ok": true }));
    };

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::new())
        .into_response())
}

// ── extract + swap helpers ────────────────────────────────────────────

struct ExtractStats {
    files: usize,
    bytes: u64,
}

async fn extract_tar(body: &Bytes, dest: &Path) -> std::io::Result<ExtractStats> {
    let body = body.clone();
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || extract_tar_blocking(body.as_ref(), &dest))
        .await
        .unwrap_or_else(|e| Err(std::io::Error::new(std::io::ErrorKind::Other, e)))
}

fn extract_tar_blocking(body: &[u8], dest: &Path) -> std::io::Result<ExtractStats> {
    std::fs::create_dir_all(dest)?;
    // `dest` is the scratch `.thclaws.deploy-<id>` dir. Workspace root
    // (where __root__/ entries land) is its parent.
    let workspace_root = dest.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "extract destination has no parent (workspace root)",
        )
    })?;
    let mut archive = tar::Archive::new(body);
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let path_str = path.to_string_lossy().to_string();
        if !is_safe_rel_path(&path_str) || !is_plain_rel_path(&path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("tar entry '{path_str}' is unsafe"),
            ));
        }
        let kind = entry.header().entry_type();
        if !matches!(kind, tar::EntryType::Regular | tar::EntryType::Directory) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("tar entry '{path_str}' is a {kind:?}; only files and directories deploy"),
            ));
        }
        let top = path
            .components()
            .next()
            .and_then(|c| c.as_os_str().to_str())
            .unwrap_or("");

        // Project-root files: write directly to /workspace/<name>,
        // bypassing the .thclaws/ scratch + swap. These survive a
        // deploy the same way pod-side state does — they live outside
        // the swap region. Per-file write-then-rename gives atomicity.
        if top == ROOT_PREFIX {
            let name = path
                .components()
                .nth(1)
                .and_then(|c| c.as_os_str().to_str())
                .unwrap_or("");
            if path.components().count() != 2 || !PROJECT_ROOT_ALLOWED.contains(&name) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "tar entry '{path_str}' under '{ROOT_PREFIX}/' must be one of {:?} (no subdirs)",
                        PROJECT_ROOT_ALLOWED
                    ),
                ));
            }
            if kind == tar::EntryType::Regular {
                let final_path = workspace_root.join(name);
                let tmp_path = workspace_root.join(format!(".{name}.deploy-tmp"));
                // A leftover tmp that is a symlink would be written through.
                match std::fs::remove_file(&tmp_path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
                entry.unpack(&tmp_path)?;
                std::fs::rename(&tmp_path, &final_path)?;
                files += 1;
                bytes += entry.header().size().unwrap_or(0);
            }
            continue;
        }

        if !ALLOWED_TOP_LEVEL.contains(&top) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "tar entry '{path_str}' targets unrecognised top-level dir '{top}' \
                     (allowed: {:?})",
                    ALLOWED_TOP_LEVEL
                ),
            ));
        }
        // `unpack_in` refuses any entry whose parent resolves outside
        // `dest`, so a directory symlink already in the tree cannot carry
        // a write elsewhere.
        if !entry.unpack_in(dest)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("tar entry '{path_str}' escapes the deploy directory"),
            ));
        }
        if kind == tar::EntryType::Regular {
            files += 1;
            bytes += entry.header().size().unwrap_or(0);
        }
    }
    Ok(ExtractStats { files, bytes })
}

/// Resolve a client-supplied relative path to its on-pod absolute
/// destination. `__root__/<NAME>` → `<workspace>/<NAME>` (with name in
/// [`PROJECT_ROOT_ALLOWED`]); everything else → `<workspace>/.thclaws/<path>`.
fn resolve_entry_path(workspace: &Path, thclaws_root: &Path, rel: &str) -> Result<PathBuf, String> {
    if let Some(stripped) = rel.strip_prefix(&format!("{ROOT_PREFIX}/")) {
        if stripped.contains('/') || !PROJECT_ROOT_ALLOWED.contains(&stripped) {
            return Err(format!(
                "path '{rel}' under '{ROOT_PREFIX}/' must be one of {:?} (no subdirs)",
                PROJECT_ROOT_ALLOWED
            ));
        }
        return Ok(workspace.join(stripped));
    }
    Ok(thclaws_root.join(rel))
}

/// Seed `scratch` with a copy of the live `.thclaws/` so that files
/// the new tar doesn't touch survive the swap. This is what makes
/// Phase-2 diff deploys correct: a tar of only changed files lands on
/// top of a full live snapshot, so anything not in the tar stays put.
///
/// Sessions / team / memory are part of the live tree and ride along
/// for free — no separate special-case needed once we copy the whole
/// thing.
async fn seed_scratch_from_live(live: &Path, scratch: &Path) -> std::io::Result<()> {
    let live = live.to_path_buf();
    let scratch = scratch.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&scratch)?;
        copy_dir_contents(&live, &scratch)
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::new(std::io::ErrorKind::Other, e)))
}

/// Atomically commit `scratch` into `live`. Old live moves to `prev`
/// for rollback; scratch moves to live. Both renames are within the
/// same directory, so they're atomic on POSIX.
async fn swap_dir(scratch: &Path, live: &Path, prev: &Path) -> std::io::Result<()> {
    let scratch = scratch.to_path_buf();
    let live = live.to_path_buf();
    let prev = prev.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if live.exists() {
            std::fs::rename(&live, &prev)?;
        }
        std::fs::rename(&scratch, &live)?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::new(std::io::ErrorKind::Other, e)))
}

/// Copy every entry under `src` into `dst`. `dst` must exist.
/// Symlinks are skipped intentionally (defensive against tar bombs in
/// the live tree from a previous bad deploy).
fn copy_dir_contents(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ft.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_dir_contents(&src_path, &dst_path)?;
        } else if ft.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn prune_prev_dirs(workspace: &Path, max_age_secs: u64) -> std::io::Result<usize> {
    let now = std::time::SystemTime::now();
    let mut pruned = 0usize;
    for entry in std::fs::read_dir(workspace)? {
        let entry = entry?;
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if !s.starts_with(".thclaws.prev-") {
            continue;
        }
        let path = entry.path();
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if age >= max_age_secs {
            if std::fs::remove_dir_all(&path).is_ok() {
                pruned += 1;
            }
        }
    }
    Ok(pruned)
}

// ── /restart ──────────────────────────────────────────────────────────

/// `POST /v1/restart`
///
/// Schedules a clean process exit ~1 second after the response flushes.
/// Kubernetes (or any supervisor with a restart-on-exit policy) brings
/// the pod back up, re-initialising MCP servers, plugin runtimes, skill
/// caches, system prompts, etc.
///
/// Why a delay rather than exit-in-the-handler: axum's response can't
/// flush after the handler thread has exited the runtime. The delay
/// lets the 200 response reach the client first; the client polls
/// /healthz to know when the pod is back.
///
/// Bearer-auth-gated like the rest of /v1/*.
pub async fn restart(_auth: AuthOk) -> Response {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        eprintln!("[restart] /v1/restart received — exiting process");
        std::process::exit(0);
    });
    (
        StatusCode::OK,
        Json(json!({ "restarting": true, "delay_secs": 1 })),
    )
        .into_response()
}

// ── small helpers ─────────────────────────────────────────────────────

fn is_safe_rel_path(p: &str) -> bool {
    if p.is_empty() || p.starts_with('/') || p.starts_with('\\') {
        return false;
    }
    if p.contains("..") {
        return false;
    }
    // First component must not be one of the preserved-on-pod dirs.
    let top = p.split(['/', '\\']).next().unwrap_or("");
    if PRESERVE_ON_POD.contains(&top) {
        return false;
    }
    true
}

/// Every component is a plain name: no root, drive prefix, `.` or `..`.
fn is_plain_rel_path(p: &Path) -> bool {
    p.components().count() > 0
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn file_sha256(path: &Path) -> std::io::Result<Option<String>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(Some(format!("{:x}", hasher.finalize())))
}

fn resolve_workspace() -> std::io::Result<PathBuf> {
    std::env::current_dir()
}

fn count_dir(p: &Path) -> usize {
    walkdir::WalkDir::new(p)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir() && e.depth() == 1)
        .count()
}

fn mcp_count(thclaws_root: &Path) -> usize {
    let mcp_path = thclaws_root.join("mcp.json");
    let Ok(contents) = std::fs::read_to_string(&mcp_path) else {
        return 0;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return 0;
    };
    v.get("mcpServers")
        .and_then(|m| m.as_object())
        .map(|m| m.len())
        .unwrap_or(0)
}

fn kms_names(thclaws_root: &Path) -> Vec<String> {
    let kms_dir = thclaws_root.join("kms");
    let Ok(rd) = std::fs::read_dir(&kms_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().to_string_lossy().to_string().into())
        .collect();
    names.sort();
    names
}

fn ok_event(name: &str, payload: serde_json::Value) -> Result<Event, Infallible> {
    Ok(Event::default().event(name).data(payload.to_string()))
}

fn bad_request(msg: impl Into<String>, code: &'static str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(OpenAiError::invalid_request(msg.into(), code)),
    )
        .into_response()
}

fn internal_err(e: impl std::fmt::Display) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(OpenAiError::server_error(format!("{e}"))),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_safe_rel_path_blocks_traversal_and_preserved_dirs() {
        assert!(is_safe_rel_path("skills/foo/SKILL.md"));
        assert!(is_safe_rel_path("settings.json"));
        assert!(is_safe_rel_path("__root__/AGENTS.md"));
        assert!(is_safe_rel_path("data/kb.md"));
        assert!(is_safe_rel_path("agent_workflow/run.js"));
        assert!(!is_safe_rel_path("../etc/passwd"));
        assert!(!is_safe_rel_path("/etc/passwd"));
        assert!(!is_safe_rel_path("skills/../etc"));
        // Workspace v2: all runtime state is preserved on the pod under
        // state/, so an upload must never overwrite it.
        assert!(!is_safe_rel_path("state/sessions/leaked.jsonl"));
        assert!(!is_safe_rel_path("state/team/workdir"));
        assert!(!is_safe_rel_path("memory/private.md"));
        assert!(!is_safe_rel_path(".env"));
        assert!(!is_safe_rel_path(""));
    }

    fn tar_with(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        build(&mut b);
        b.into_inner().unwrap()
    }

    fn file_header(path: &str, len: usize) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        h.set_path(path).unwrap();
        h.set_size(len as u64);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        h
    }

    fn scratch_in(dir: &Path) -> PathBuf {
        let s = dir.join(".thclaws.deploy-test");
        std::fs::create_dir_all(&s).unwrap();
        s
    }

    #[test]
    fn deploy_extract_accepts_plain_files() {
        let tmp = tempfile::tempdir().unwrap();
        let scratch = scratch_in(tmp.path());
        let body = tar_with(|b| {
            let data = b"hi";
            b.append(&file_header("skills/a/SKILL.md", 2), &data[..])
                .unwrap();
            b.append(&file_header("__root__/AGENTS.md", 2), &data[..])
                .unwrap();
        });
        let stats = extract_tar_blocking(&body, &scratch).unwrap();
        assert_eq!(stats.files, 2);
        assert_eq!(
            std::fs::read(scratch.join("skills/a/SKILL.md")).unwrap(),
            b"hi"
        );
        assert_eq!(std::fs::read(tmp.path().join("AGENTS.md")).unwrap(), b"hi");
    }

    #[test]
    fn deploy_extract_refuses_links() {
        for kind in [tar::EntryType::Symlink, tar::EntryType::Link] {
            let tmp = tempfile::tempdir().unwrap();
            let scratch = scratch_in(tmp.path());
            let body = tar_with(|b| {
                let mut h = tar::Header::new_gnu();
                h.set_path("skills/evil").unwrap();
                h.set_link_name("/etc").unwrap();
                h.set_size(0);
                h.set_entry_type(kind);
                h.set_cksum();
                b.append(&h, std::io::empty()).unwrap();
            });
            let Err(err) = extract_tar_blocking(&body, &scratch) else {
                panic!("{kind:?} entry was extracted");
            };
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{kind:?}");
            assert!(std::fs::symlink_metadata(scratch.join("skills/evil")).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn deploy_extract_will_not_write_through_existing_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let scratch = scratch_in(tmp.path());
        std::os::unix::fs::symlink(outside.path(), scratch.join("skills")).unwrap();
        let body = tar_with(|b| {
            b.append(&file_header("skills/pwn.txt", 1), &b"x"[..])
                .unwrap();
        });
        assert!(extract_tar_blocking(&body, &scratch).is_err());
        assert!(!outside.path().join("pwn.txt").exists());
    }

    #[test]
    fn deploy_plain_rel_path_rejects_odd_components() {
        assert!(is_plain_rel_path(Path::new("skills/a/SKILL.md")));
        assert!(!is_plain_rel_path(Path::new("./skills")));
        assert!(!is_plain_rel_path(Path::new("/abs")));
        assert!(!is_plain_rel_path(Path::new("skills/../x")));
        assert!(!is_plain_rel_path(Path::new("")));
    }

    #[test]
    fn resolve_entry_path_routes_root_prefix() {
        let ws = Path::new("/workspace");
        let tc = ws.join(".thclaws");

        assert_eq!(
            resolve_entry_path(ws, &tc, "__root__/AGENTS.md").unwrap(),
            ws.join("AGENTS.md")
        );
        assert_eq!(
            resolve_entry_path(ws, &tc, "__root__/CLAUDE.md").unwrap(),
            ws.join("CLAUDE.md")
        );
        assert_eq!(
            resolve_entry_path(ws, &tc, "settings.json").unwrap(),
            tc.join("settings.json")
        );
        // unknown name under __root__/ → rejected
        assert!(resolve_entry_path(ws, &tc, "__root__/evil.sh").is_err());
        // subdir under __root__/ → rejected
        assert!(resolve_entry_path(ws, &tc, "__root__/sub/AGENTS.md").is_err());
    }
}
