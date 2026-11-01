//! Agent package tar pack/unpack.
//!
//! `pack(folder)` produces a gzipped tarball with strip rules applied
//! (sessions, secrets, build artefacts dropped). `unpack(bytes, target)`
//! is the reverse — extracts into a target directory, refusing
//! path-traversal entries.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use sha2::{Digest, Sha256};

pub const STRIP_PREFIXES: &[&str] = &[
    // Workspace v2: ALL runtime state lives under `.thclaws/state/` and is
    // never part of a published agent — sessions, kms, usage/telemetry,
    // team coordination, workflow run-state, and the managed browser's
    // chromium profile (cookies/tokens must NEVER ride along). Authored
    // workflow scripts live in `.thclaws/agent_workflow/` and are kept.
    ".thclaws/state/",
    // dev-plan/59: installed bots live at `.thclaws/bots/<slug>/`, each a
    // complete agent folder with its OWN `.thclaws/state/` — sessions, KMS,
    // and a chromium profile holding real logins. Matching here is
    // root-anchored, so a nested `.thclaws/bots/x/.thclaws/state/…` does not
    // match the rule above; without this entry the whole shelf would be
    // packed into a published agent and uploaded. Stripped whole rather than
    // per-bot: a published agent carries no other agents, not even their
    // definitions.
    ".thclaws/bots/",
    // Hosted runners put the engine's $HOME on the workspace PVC under
    // `.home/` so `/schedule` and friends survive a pause. It is mounted
    // into the pod at /home/thclaws, but the PVC is also mounted whole
    // at /workspace, so the same bytes are visible here — and they are
    // user credentials (gui-shell tokens, provider config), never agent
    // content. Harmless on desktop, where nothing writes it.
    ".home/",
    // playwright-mcp's automatic output: an accessibility snapshot and a
    // console log per navigation, i.e. the full text of pages the user was
    // on. `browser_mcp_config` now points `--output-dir` inside
    // `.thclaws/state/`, which the first rule already covers — this is the
    // backstop for a workspace that still has the old directory, and for a
    // `THCLAWS_BROWSER_MCP_CMD` override that pins its own. `.log` is caught
    // by the suffix rule; `page-*.yml` was caught by nothing.
    ".playwright-mcp/",
    ".git/",
    "node_modules/",
    "target/",
    "__pycache__/",
    ".venv/",
    ".next/",
    "dist/",
    "build/",
];

pub const STRIP_SUFFIXES: &[&str] = &[".env", ".key", ".pyc", ".log"];

/// Exact relative paths to drop — runtime artifacts not covered by a prefix or
/// suffix. Parity with publish.py's STRIP_EXACT, plus `usage.jsonl` (the
/// telemetry log that sits beside, not inside, `.thclaws/usage/`).
pub const STRIP_EXACT: &[&str] = &[
    ".thclaws/audit-findings.json",
    // Both of these moved under `state/` and are caught by STRIP_PREFIXES
    // on a migrated workspace. The entries stay as the transitional net
    // for a workspace that has not been opened since the move.
    ".thclaws/usage.jsonl",
    ".thclaws/media-jobs.jsonl",
];

pub fn is_strippable(rel: &Path) -> bool {
    let s = rel.to_string_lossy();
    let s = s.trim_start_matches("./");
    if STRIP_EXACT.contains(&s) {
        return true;
    }
    if STRIP_PREFIXES.iter().any(|p| s.starts_with(p)) {
        return true;
    }
    if STRIP_SUFFIXES.iter().any(|sx| s.ends_with(sx)) {
        return true;
    }
    if s.to_lowercase().contains("_secret") {
        return true;
    }
    false
}

pub struct PackResult {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub included: Vec<String>,
    pub stripped: Vec<String>,
}

/// Pack `folder` into a gzipped tarball. Strips files per the rules.
/// Requires `folder` to contain `AGENTS.md` at its root.
///
/// When `manifest_override` is `Some(bytes)`, that JSON blob is written
/// to the tarball as `manifest.json` instead of whatever exists on disk
/// — used by `cloud publish` to ship the fused identity-plus-catalog
/// manifest while keeping the local `manifest.json` slim (no identity
/// fields) per dev-plan/34 Option A. When `None`, the on-disk
/// `manifest.json` is tarred verbatim (required).
pub fn pack(folder: &Path, manifest_override: Option<&[u8]>) -> Result<PackResult, String> {
    let folder = folder
        .canonicalize()
        .map_err(|e| format!("canonicalize {}: {}", folder.display(), e))?;
    if !folder.is_dir() {
        return Err(format!("{} is not a directory", folder.display()));
    }
    if !folder.join("AGENTS.md").exists() {
        return Err("missing AGENTS.md in folder".into());
    }
    if manifest_override.is_none() && !folder.join("manifest.json").exists() {
        return Err("missing manifest.json in folder".into());
    }

    let mut included = Vec::new();
    let mut stripped = Vec::new();
    let buf: Vec<u8> = Vec::new();
    let enc = GzEncoder::new(buf, Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.follow_symlinks(false);

    for entry in walkdir::WalkDir::new(&folder)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let rel = path.strip_prefix(&folder).unwrap();
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        if is_strippable(rel) {
            stripped.push(rel_str);
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }

        // When an override is supplied, skip the on-disk manifest.json
        // — we'll append the synthesized version below.
        if manifest_override.is_some() && rel == std::path::Path::new("manifest.json") {
            continue;
        }

        // `.thclaws/settings.json` carries publisher-only fields
        // (the `agent` block — id/name/description/uuid — that binds
        // this folder to its catalog row). The installer's workspace
        // shouldn't inherit that binding, or a subsequent `cloud
        // publish` from their copy would silently clobber the
        // original. Strip the `agent` block before packing; everything
        // else (guiShell, model, etc.) is user-facing config and
        // stays.
        if rel == std::path::Path::new(".thclaws/settings.json") {
            let raw = std::fs::read(path).map_err(|e| format!("read settings.json: {e}"))?;
            let cleaned = strip_publisher_fields(&raw)?;
            let mut header = tar::Header::new_gnu();
            header.set_size(cleaned.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            tar.append_data(&mut header, rel, cleaned.as_slice())
                .map_err(|e| format!("tar append settings.json: {e}"))?;
            included.push(rel_str);
            continue;
        }

        let metadata = entry
            .metadata()
            .map_err(|e| format!("stat {}: {}", path.display(), e))?;
        let mut f =
            std::fs::File::open(path).map_err(|e| format!("open {}: {}", path.display(), e))?;
        let mut header = tar::Header::new_gnu();
        header.set_size(metadata.len());
        // Normalised to 0644/0755 rather than copied verbatim: an agent's
        // hooks / scripts / helper binaries are useless without the executable
        // bit, but a published bundle has no business carrying setuid or
        // group/world-writable modes from the publisher's machine.
        header.set_mode(exec_normalised_mode(&metadata));
        header.set_mtime(
            metadata
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
        header.set_cksum();

        tar.append_data(&mut header, rel, &mut f)
            .map_err(|e| format!("tar append {}: {}", path.display(), e))?;
        included.push(rel_str);
    }

    if let Some(override_bytes) = manifest_override {
        let mut header = tar::Header::new_gnu();
        header.set_size(override_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        tar.append_data(
            &mut header,
            std::path::Path::new("manifest.json"),
            override_bytes,
        )
        .map_err(|e| format!("tar append manifest override: {}", e))?;
        included.push("manifest.json".to_string());
    }

    let enc = tar.into_inner().map_err(|e| format!("tar finish: {}", e))?;
    let bytes = enc.finish().map_err(|e| format!("gzip finish: {}", e))?;
    let sha = Sha256::digest(&bytes);
    Ok(PackResult {
        bytes,
        sha256: hex_encode(&sha),
        included,
        stripped,
    })
}

/// Remove publisher-only fields from a packed `.thclaws/settings.json`.
/// Currently drops the `agent` block (publish-binding identity that
/// shouldn't carry over to installers). Falls back to passing the
/// original bytes through unchanged if the file isn't valid JSON
/// (don't fail the whole publish over a settings parse error).
fn strip_publisher_fields(raw: &[u8]) -> Result<Vec<u8>, String> {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return Ok(raw.to_vec());
    };
    if let Some(obj) = v.as_object_mut() {
        obj.remove("agent");
    }
    serde_json::to_vec_pretty(&v).map_err(|e| format!("re-serialize settings.json: {e}"))
}

/// 0755 when the source file is executable, 0644 otherwise. See the call site
/// for why the mode is normalised instead of copied.
fn exec_normalised_mode(metadata: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 {
            return 0o755;
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    0o644
}

/// Apply an archived mode on extract, with the same 0755/0644 normalisation
/// `pack` applies — so a tarball can't land setuid or world-writable files.
#[cfg(unix)]
fn apply_mode(path: &Path, mode: Option<u32>) {
    use std::os::unix::fs::PermissionsExt;
    let Some(mode) = mode else { return };
    let perms = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(perms));
}

#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: Option<u32>) {}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Peek at the agent UUID inside a gzipped tarball's manifest.json
/// without unpacking. Used by `cloud get` to safety-check before
/// overwriting an existing folder.
pub fn peek_manifest_uuid(bytes: &[u8]) -> Result<Option<String>, String> {
    use std::io::Read;
    let dec = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(dec);
    for entry in archive
        .entries()
        .map_err(|e| format!("read archive: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("read entry: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("entry path: {e}"))?
            .into_owned();
        if path == Path::new("manifest.json") {
            let mut content = String::new();
            entry
                .read_to_string(&mut content)
                .map_err(|e| format!("read manifest.json: {e}"))?;
            let v: serde_json::Value =
                serde_json::from_str(&content).map_err(|e| format!("parse manifest.json: {e}"))?;
            return Ok(v
                .get("uuid")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()));
        }
    }
    Err("manifest.json not found in tarball".into())
}

/// Verify gzipped tarball matches `expected_sha256` (hex).
pub fn verify_sha256(bytes: &[u8], expected_sha256: &str) -> Result<(), String> {
    let actual = hex_encode(&Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected_sha256) {
        Ok(())
    } else {
        Err(format!(
            "sha256 mismatch: got {}, expected {}",
            actual, expected_sha256
        ))
    }
}

/// Unpack a gzipped tarball into `target`. Refuses to overwrite existing
/// files unless `force` is true. Refuses path-traversal entries.
pub fn unpack(bytes: &[u8], target: &Path, force: bool) -> Result<Vec<PathBuf>, String> {
    if target.exists() && !target.is_dir() {
        return Err(format!(
            "{} exists and is not a directory",
            target.display()
        ));
    }
    std::fs::create_dir_all(target).map_err(|e| format!("mkdir {}: {}", target.display(), e))?;

    let dec = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(dec);
    let mut extracted = Vec::new();

    let canonical_target = target
        .canonicalize()
        .map_err(|e| format!("canonicalize target {}: {}", target.display(), e))?;

    for entry in archive
        .entries()
        .map_err(|e| format!("read archive: {}", e))?
    {
        let mut entry = entry.map_err(|e| format!("read entry: {}", e))?;
        // Agents are unpacked on Windows too, so the Windows rules apply on
        // every OS: a bundle that is unsafe anywhere is refused everywhere.
        let raw = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        if crate::sandbox::is_unsafe_archive_entry(&raw, true) {
            return Err(format!("refused unsafe entry path: {raw}"));
        }
        let path = entry
            .path()
            .map_err(|e| format!("entry path: {}", e))?
            .into_owned();
        let out = canonical_target.join(&path);
        // Entry type, before anything tries to open `out` as a file. A tar
        // may carry directory entries (`./`, `.thclaws/`) — every agent
        // published through a packer that walks directories does, including
        // the first-party `hello-world` — and `File::create` on one fails with
        // "Is a directory", leaving the install half-extracted. Found by
        // installing a real catalogue agent, not by reading this.
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("mkdir {}: {}", out.display(), e))?;
            continue;
        }
        if !kind.is_file() {
            // Symlinks and hardlinks are the interesting ones: their target
            // lives in the header, not the body, so the path checks above
            // never see it. Nothing this packer produces has them
            // (`follow_symlinks(false)` plus explicit `append_data`), so an
            // archive that does is not one of ours.
            return Err(format!(
                "refused non-regular archive entry {} ({:?})",
                path.display(),
                kind
            ));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
        }
        if out.exists() && !force {
            return Err(format!(
                "refusing to overwrite existing file {} (use --force)",
                out.display()
            ));
        }
        let mode = entry.header().mode().ok();
        let mut f =
            std::fs::File::create(&out).map_err(|e| format!("create {}: {}", out.display(), e))?;
        std::io::copy(&mut entry, &mut f).map_err(|e| format!("write {}: {}", out.display(), e))?;
        drop(f);
        apply_mode(&out, mode);
        extracted.push(out);
    }
    Ok(extracted)
}

#[allow(dead_code)]
fn ensure_read<R: Read>(r: R) -> R {
    r
}

#[allow(dead_code)]
fn ensure_write<W: Write>(w: W) -> W {
    w
}

#[cfg(test)]
mod unpack_tests {
    use super::unpack;
    use std::io::Write;

    /// Audit 2026-11-01 #11: `is_absolute()` is false for `\Users\…` and
    /// `C:foo`, so on Windows these escaped the target. Refused on every OS,
    /// before anything is created.
    #[test]
    fn windows_rooted_and_drive_entries_are_refused_everywhere() {
        for name in [
            r"\Users\Public\x.bat",
            "C:foo",
            "a/b:stream",
            "con.txt",
            "x./y",
        ] {
            let mut tar = tar::Builder::new(Vec::new());
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(1);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, name, &b"x"[..]).unwrap();
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            gz.write_all(&tar.into_inner().unwrap()).unwrap();
            let bytes = gz.finish().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let err = unpack(&bytes, dir.path(), true).unwrap_err();
            assert!(err.contains("refused unsafe entry"), "{name}: {err}");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "{name}");
        }
    }

    /// Build a tarball the way a directory-walking packer does: a `./` entry,
    /// directory entries, then the files. Every first-party agent published
    /// that way — `hello-world` among them — looks like this.
    fn tar_with_dir_entries() -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for dir in ["./", "./.thclaws/", "./.thclaws/skills/"] {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            h.set_mode(0o755);
            h.set_cksum();
            tar.append_data(&mut h, dir, std::io::empty()).unwrap();
        }
        for (name, body) in [
            ("./AGENTS.md", "# hello\n"),
            ("./.thclaws/settings.json", "{}"),
            ("./.thclaws/skills/a.md", "skill"),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, name, body.as_bytes()).unwrap();
        }
        let raw = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&raw).unwrap();
        gz.finish().unwrap()
    }

    /// `/cloud get hello-world` failed with "create …/./: Is a directory" —
    /// `unpack` called `File::create` on every entry without looking at its
    /// type, so any archive carrying directory entries was un-installable.
    #[test]
    fn extracts_an_archive_that_carries_directory_entries() {
        let dir = tempfile::tempdir().unwrap();
        let files = unpack(&tar_with_dir_entries(), dir.path(), true).expect("extracts");
        assert_eq!(files.len(), 3, "directories are not counted as files");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
            "# hello\n"
        );
        assert!(dir.path().join(".thclaws/skills/a.md").is_file());
        // Re-running over the same folder is what an update does.
        unpack(&tar_with_dir_entries(), dir.path(), true).expect("re-extracts");
    }

    /// A symlink's target lives in the header, not the body, so the path
    /// checks never see it. This packer never emits one.
    #[test]
    fn refuses_a_symlink_entry() {
        let mut tar = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_cksum();
        tar.append_link(&mut h, "escape", "/etc/passwd").unwrap();
        let raw = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&raw).unwrap();
        let bytes = gz.finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let err = unpack(&bytes, dir.path(), true).unwrap_err();
        assert!(err.contains("non-regular"), "{err}");
        assert!(!dir.path().join("escape").exists());
    }
}

#[cfg(test)]
mod strip_tests {
    use super::is_strippable;
    use std::path::Path;

    /// dev-plan/59: a workspace may hold installed bots at
    /// `.thclaws/bots/<slug>/`, each a complete agent folder with its own
    /// `.thclaws/state/` — sessions, KMS, and a chromium profile holding
    /// real logins.
    ///
    /// `is_strippable` matches by root-anchored prefix, so a nested
    /// `.thclaws/bots/x/.thclaws/state/…` does NOT match `.thclaws/state/`.
    /// Without a rule of its own the whole shelf would be packed into a
    /// published agent and uploaded to the catalogue — every bot's cookies
    /// with it. A published agent carries no other agents, not even their
    /// definitions, so the shelf is stripped whole.
    #[test]
    fn strips_the_whole_bot_shelf_so_a_publish_never_carries_another_agent() {
        for p in [
            // The reason this rule exists: a sibling bot's live cookie jar.
            ".thclaws/bots/research/.thclaws/state/browser-profile/Cookies",
            ".thclaws/bots/research/.thclaws/state/sessions/sess-1.jsonl",
            ".thclaws/bots/research/.thclaws/state/kms/vault/note.md",
            // Definitions too — an agent does not ship other agents.
            ".thclaws/bots/research/.thclaws/settings.json",
            ".thclaws/bots/research/AGENTS.md",
            ".thclaws/bots/book-author/manifest.json",
            // The host's own agent after the v3 migration.
            ".thclaws/bots/main/.thclaws/state/browser-profile/Cookies",
        ] {
            assert!(is_strippable(Path::new(p)), "should strip {p}");
        }
        // A file that merely mentions bots is not the shelf.
        for p in ["bots/README.md", "docs/bots.md", ".thclaws/agents/bots.md"] {
            assert!(!is_strippable(Path::new(p)), "should keep {p}");
        }
    }

    /// dev-plan/59 §5.5: a bot's `$HOME` is `<bot>/.home/`, so when the BOT
    /// itself is published — root is the bot folder, and `.thclaws/bots/`
    /// never matches — the `.home/` rule is the only thing standing between a
    /// publish and the bot's gui-shell tokens and provider config.
    #[test]
    fn a_bot_publishing_itself_does_not_ship_its_own_home() {
        for p in [
            ".home/.config/thclaws/settings.json",
            ".home/.config/thclaws/schedules.json",
            ".home/.cache/thclaws/catalogue.json",
        ] {
            assert!(is_strippable(Path::new(p)), "should strip {p}");
        }
    }

    #[test]
    fn strips_runtime_artifacts_that_leaked_into_a_user_publish() {
        // Regression: publishing from a live workspace must not ship the
        // publisher's runtime telemetry / coordination state.
        for p in [
            ".thclaws/state/usage/deepseek/deepseek-v4-pro.json",
            ".thclaws/state/usage.jsonl",
            ".thclaws/state/team/agents/lead/status.json",
            ".thclaws/audit-findings.json",
            ".thclaws/state/sessions/x.jsonl",
            ".thclaws/state/workflows/wf-1/state.jsonl",
            "secrets_secret.txt",
            "a/.env",
            // Hosted runners put the engine's $HOME on the workspace
            // PVC under `.home/`. It is mounted at /home/thclaws, but
            // the PVC is also mounted whole at /workspace, so these are
            // reachable from the workspace tree — and they are the
            // publisher's credentials, not agent content.
            ".home/.config/thclaws/gui-shell-tokens.json",
            ".home/.config/thclaws/schedules.json",
            // playwright-mcp writes one of these per navigation, unasked:
            // `page-*.yml` is the full text of a page the user was on, and
            // NOTHING caught it — `.log` matched the suffix rule, `.yml` did
            // not. Found by auditing a real browsing session, where 12
            // minutes produced 36 files.
            ".playwright-mcp/page-2026-09-23T14-47-54-631Z.yml",
            ".playwright-mcp/console-2026-09-23T14-46-06-421Z.log",
        ] {
            assert!(is_strippable(Path::new(p)), "should strip {p}");
        }
        // Agent content stays — including authored workflow scripts.
        for p in [
            "AGENTS.md",
            "manifest.json",
            ".thclaws/agent_workflow/image-batch.js",
            ".thclaws/agents/image-smith.md",
            "images/batch/red-panda.png",
        ] {
            assert!(!is_strippable(Path::new(p)), "should keep {p}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn pack_unpack_preserves_the_executable_bit() {
        // An agent's hooks / scripts are useless without +x, and a bundle has
        // no business carrying setuid or world-writable modes across.
        use std::os::unix::fs::PermissionsExt;
        let src = std::env::temp_dir().join(format!(
            "packmode-src-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("AGENTS.md"), "# agent").unwrap();
        std::fs::write(src.join("manifest.json"), "{}").unwrap();
        std::fs::write(src.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(src.join("notes.md"), "plain").unwrap();
        // Executable, and deliberately setuid + world-writable to prove those
        // are dropped rather than carried.
        std::fs::set_permissions(src.join("run.sh"), std::fs::Permissions::from_mode(0o4777))
            .unwrap();

        let packed = super::pack(&src, None).unwrap();
        let dst = src.with_extension("dst");
        super::unpack(&packed.bytes, &dst, true).unwrap();

        let sh = std::fs::metadata(dst.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        let md = std::fs::metadata(dst.join("notes.md"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            sh, 0o755,
            "executable bit survives, setuid/world-write do not"
        );
        assert_eq!(md, 0o644, "plain files stay 0644");

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }
}
