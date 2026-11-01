//! Workspace sync (dev-plan/51): tar/untar the working directory for the
//! `/cloud push|pull` round-trip between the desktop app and a hosted cloud
//! workspace.
//!
//! Unlike catalog publish (`pack::is_strippable`), `/cloud push|pull` is a
//! FULL directory teleport: the working tree AND all runtime state under
//! `.thclaws/state/` (sessions, kms, browser profile, workflow run-state, …)
//! ride along so work resumes on the other end mid-session. The ONLY things
//! dropped are regenerable, machine/arch-specific build dirs that would
//! corrupt the destination or waste the payload (`node_modules/`, `target/`,
//! `.venv/`, …) — see [`SYNC_STRIP_DIRS`]. Both ends stream the tarball
//! through a temp file (tar→disk on pack, disk→untar on apply) so memory
//! stays flat regardless of payload size. Sync-specific pieces:
//!   - a 10 GiB payload cap (`MAX_SYNC_BYTES`, the PVC quota),
//!   - `--delete` mirroring that moves removed files to `.sync-trash/<ts>/`
//!     (recoverable, not a hard delete),
//!   - traversal-safe extraction: rejects `..` / absolute entry paths,
//!     skips symlinks when collecting, resolves each destination and
//!     refuses one that lands outside the root (a symlink already in the
//!     workspace would otherwise redirect a write), and caps the
//!     DECOMPRESSED stream — the transport cap bounds what arrives, not
//!     what a gzip expands to,
//!   - the UUID binding file `.thclaws/cloud-sync.json` that ties a local folder
//!     to exactly one hosted workspace.
//!
//! v1 is whole-tarball (`.tar.gz`, matching `pack.rs`); the incremental
//! manifest-diff path layers on top in P2.

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Hard cap on a single sync payload (uncompressed sum of synced files).
/// Matches the hosted workspace PVC quota; sync streams via temp files so the
/// cap tracks disk, not memory.
pub const MAX_SYNC_BYTES: u64 = 10 * 1024 * 1024 * 1024;

const BINDING_REL: &str = ".thclaws/cloud-sync.json";
const SETTINGS_REL: &str = ".thclaws/settings.json";
const SYNCIGNORE_REL: &str = ".thclaws/syncignore";
const TRASH_PREFIX: &str = ".sync-trash";
/// Local-only divergence watermark: the content fingerprint of the last
/// SUCCESSFUL sync. Excluded from the payload (never travels — each side keeps
/// its own), so a peer's tarball can't clobber it. Used to warn before a
/// push/pull overwrites work the other end did since that agreed state.
const SYNC_BASE_REL: &str = ".thclaws/cloud-sync-base.json";

/// Paths kept OUT of the divergence fingerprint: the sync plumbing itself
/// differs per-end by design (binding carries per-folder timestamps; settings
/// gets the gateway overlay injected on cloud), so hashing them would read as
/// "always diverged". Real work (sources, sessions, state) still counts.
const FINGERPRINT_SKIP: &[&str] = &[BINDING_REL, SETTINGS_REL, SYNCIGNORE_REL, SYNC_BASE_REL];

/// Records which hosted workspace a folder is paired with. Lives at
/// `.thclaws/cloud-sync.json` on both ends (dev-plan/51 decision #5).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Binding {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_push: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_pull: Option<String>,
    /// Monotonic counter of COMPLETED syncs for this folder↔workspace
    /// pairing, bumped once per successful `/cloud push|pull` (dry runs and
    /// failed syncs don't move it). Both ends record the same number, so
    /// "rev 7" names one agreed state a user can point at in a bug report or
    /// a handoff. Absent on a binding written before revisions existed —
    /// treated as 0, so the next sync lands rev 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

/// The revision a sync that is about to complete should record: one past the
/// highest either end has seen, so the counter never repeats or walks back
/// even when one end synced without the other (a `--force`, a second machine,
/// a run that died after the far end committed).
///
/// The local count only carries forward while the folder stays bound to the
/// SAME workspace — re-pointing a folder (`--force-rebind`) starts a new
/// pairing, so it picks up from the cloud's count rather than an unrelated
/// local one.
pub fn next_revision(prev: &Binding, workspace_id: &str, remote: Option<u64>) -> u64 {
    let local = if prev.workspace_id.as_deref() == Some(workspace_id) {
        prev.revision.unwrap_or(0)
    } else {
        0
    };
    local.max(remote.unwrap_or(0)) + 1
}

#[derive(Debug, Clone, Default)]
pub struct SyncStat {
    pub file_count: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct UntarResult {
    pub written: usize,
    pub deleted: usize,
    pub trash_dir: Option<PathBuf>,
    /// Repo-controlled code a pull declined to write (see [`git_guard`]).
    pub notes: Vec<String>,
}

fn norm(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// Regenerable, machine/arch-specific dirs that must NEVER ride a workspace
/// teleport: they're rebuilt on demand and are platform-bound, so shipping a
/// macOS `target/` or a `.venv/` with absolute interpreter paths onto a Linux
/// runner (or vice-versa) corrupts the destination — and they dwarf the real
/// work in size. Matched as a path SEGMENT anywhere in the tree (not just at
/// root), so a monorepo's `frontend/node_modules/` is dropped too. Everything
/// NOT in this list teleports verbatim — sessions/state, `.git/`, secrets —
/// which is the whole point of push|pull vs. a catalog publish.
/// Always-stripped tool dirs — unambiguously regenerable caches.
pub const SYNC_STRIP_DIRS: &[&str] = &["node_modules", ".venv", "__pycache__", ".next"];

/// Rendered `.pptx` previews. Unlike the rest of `.thclaws/state/` this
/// is derived output, not work: several MB per deck (a 23-slide deck
/// lands at ~7.5 MB of PDF + PNGs), and the far end re-renders on
/// demand. Carrying it would inflate every push for nothing.
const PPTX_CACHE_PREFIX: &str = crate::tools::slide_render::PPTX_CACHE_REL;

/// Conditionally-stripped names: real toolchain OUTPUT only when the
/// marker file that generates them sits beside them. `build/` in a JS
/// project (sibling `package.json`) is regenerable; `build/` in a
/// book-production workspace (sibling `book.yaml`, no `package.json`)
/// is the DELIVERABLES — epub/pdf/rendered slides/TTS audio that cost
/// real money to produce — and was being silently dropped from
/// /cloud push (544MB of a 1GB workspace in the reported case).
pub const SYNC_STRIP_IF_MARKER: &[(&str, &str)] = &[
    ("target", "Cargo.toml"),
    ("build", "package.json"),
    ("dist", "package.json"),
];

fn in_stripped_dir(root: &Path, rel: &Path) -> bool {
    let mut parent = PathBuf::new();
    for c in rel.components() {
        if let Component::Normal(seg) = c {
            if let Some(s) = seg.to_str() {
                if SYNC_STRIP_DIRS.contains(&s) {
                    return true;
                }
                for (name, marker) in SYNC_STRIP_IF_MARKER {
                    if s == *name && root.join(&parent).join(marker).is_file() {
                        return true;
                    }
                }
            }
        }
        parent.push(c);
    }
    false
}

/// Inside the sync exclude set? Only the regenerable build dirs
/// ([`SYNC_STRIP_DIRS`] + marker-confirmed [`SYNC_STRIP_IF_MARKER`])
/// plus the `.sync-trash/` tree itself (never sync the trash). NOT
/// `pack::is_strippable` — push|pull keeps runtime state.
fn excluded(root: &Path, rel: &Path) -> bool {
    let s = norm(rel);
    s == SYNC_BASE_REL
        || s == TRASH_PREFIX
        || s.starts_with(&format!("{TRASH_PREFIX}/"))
        || s == PPTX_CACHE_PREFIX
        || s.starts_with(&format!("{PPTX_CACHE_PREFIX}/"))
        || in_stripped_dir(root, rel)
}

/// Collect files relative to `root`. `keep` decides inclusion; symlinks are
/// always skipped (never followed — traversal safety).
fn walk(root: &Path, keep: &dyn Fn(&Path) -> bool) -> Result<Vec<PathBuf>, String> {
    Ok(walk_with_dirs(root, keep)?.0)
}

/// Like [`walk`] but also returns every kept DIRECTORY (for empty-dir
/// preservation: dirs with no synced file beneath still ride the tar as
/// directory entries so scaffolding like `media/screenshots/` survives
/// a push).
fn walk_with_dirs(
    root: &Path,
    keep: &dyn Fn(&Path) -> bool,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let mut out = Vec::new();
    let mut dirs = Vec::new();
    walk_inner(root, root, keep, &mut out, &mut dirs)?;
    out.sort();
    dirs.sort();
    Ok((out, dirs))
}

/// Kept directories with no synced file beneath them.
fn empty_dirs_for(root: &Path, keep: &dyn Fn(&Path) -> bool) -> Result<Vec<PathBuf>, String> {
    let (files, dirs) = walk_with_dirs(root, keep)?;
    let file_norms: Vec<String> = files.iter().map(|f| norm(f)).collect();
    Ok(dirs
        .into_iter()
        .filter(|d| {
            let prefix = format!("{}/", norm(d));
            !file_norms.iter().any(|f| f.starts_with(&prefix))
        })
        .collect())
}

/// Empty dirs under the standard synced view (strip set + syncignore).
fn empty_synced_dirs(root: &Path) -> Result<Vec<PathBuf>, String> {
    let ignores = load_syncignore(root);
    empty_dirs_for(root, &|rel| {
        !excluded(root, rel) && !ignored_by(&norm(rel), &ignores)
    })
}

fn walk_inner(
    root: &Path,
    dir: &Path,
    keep: &dyn Fn(&Path) -> bool,
    out: &mut Vec<PathBuf>,
    dirs: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read_dir {}: {}", dir.display(), e)),
    };
    for ent in rd {
        let ent = ent.map_err(|e| format!("dir entry: {}", e))?;
        let path = ent.path();
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        if !keep(&rel) {
            continue;
        }
        let ft = ent.file_type().map_err(|e| format!("file_type: {}", e))?;
        if ft.is_symlink() {
            continue; // never follow or sync symlinks
        } else if ft.is_dir() {
            dirs.push(rel.clone());
            walk_inner(root, &path, keep, out, dirs)?;
        } else if ft.is_file() {
            out.push(rel);
        }
    }
    Ok(())
}

/// User exclude patterns from `.thclaws/syncignore` (dev-plan/51 open
/// question, resolved 2026-07-04): one path per line, `/`-separated,
/// `#` comments + blank lines skipped, trailing `/` tolerated. A line
/// matches its exact rel path or anything under it (prefix at a `/`
/// boundary). Deliberately NOT a glob engine — plain prefixes cover the
/// real use (keep big data/build dirs out of the sync) with zero
/// pattern-language surprises. The file itself lives inside the synced
/// set, so a push propagates the ignore list to the other side.
fn load_syncignore(root: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(root.join(SYNCIGNORE_REL)) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            l.trim_start_matches("./")
                .trim_end_matches('/')
                .replace('\\', "/")
        })
        .filter(|l| !l.is_empty())
        .collect()
}

fn ignored_by(rel: &str, patterns: &[String]) -> bool {
    // The sync plumbing itself can't be ignored away — losing the
    // binding / settings / the ignore file mid-round-trip is a foot-gun.
    // Ancestor DIRS are exempt too (the walk prunes whole subtrees, so
    // an ignored `.thclaws` must still descend far enough to keep them).
    const PLUMBING: &[&str] = &[BINDING_REL, SETTINGS_REL, SYNCIGNORE_REL];
    if PLUMBING
        .iter()
        .any(|p| rel == *p || p.starts_with(&format!("{rel}/")))
    {
        return false;
    }
    patterns
        .iter()
        .any(|p| rel == p || rel.starts_with(&format!("{p}/")))
}

/// Synced (non-excluded) files, relative to `root`. Applies the strip
/// set plus the user's `.thclaws/syncignore`.
fn walk_synced(root: &Path) -> Result<Vec<PathBuf>, String> {
    let ignores = load_syncignore(root);
    walk(root, &|rel| {
        !excluded(root, rel) && !ignored_by(&norm(rel), &ignores)
    })
}

pub fn stat_workspace(root: &Path) -> Result<SyncStat, String> {
    let files = walk_synced(root)?;
    let mut bytes = 0u64;
    for rel in &files {
        bytes += std::fs::metadata(root.join(rel))
            .map(|m| m.len())
            .unwrap_or(0);
    }
    Ok(SyncStat {
        file_count: files.len(),
        bytes,
    })
}

/// "Empty" for the binding guard: no synced files other than a bare
/// `.thclaws/settings.json` (a fresh workspace ships only settings).
pub fn is_empty(root: &Path) -> Result<bool, String> {
    Ok(walk_synced(root)?.iter().all(|r| norm(r) == SETTINGS_REL))
}

pub fn read_binding(root: &Path) -> Binding {
    std::fs::read(root.join(BINDING_REL))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn write_binding(root: &Path, b: &Binding) -> Result<(), String> {
    let p = root.join(BINDING_REL);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
    }
    let data = serde_json::to_vec_pretty(b).map_err(|e| format!("serialize binding: {}", e))?;
    std::fs::write(&p, data).map_err(|e| format!("write binding: {}", e))
}

/// Tar+gzip a list of rel paths under `root` into `w`. `empty_dirs`
/// ride as directory entries (~0 bytes) so scaffold folders survive;
/// receivers that predate dir handling skip them harmlessly.
fn write_tar<W: Write>(
    root: &Path,
    files: &[PathBuf],
    empty_dirs: &[PathBuf],
    w: W,
) -> Result<(), String> {
    let enc = GzEncoder::new(w, Compression::default());
    let mut tar = tar::Builder::new(enc);
    for rel in empty_dirs {
        let abs = root.join(rel);
        if abs.is_dir() {
            tar.append_dir(rel, &abs)
                .map_err(|e| format!("tar append dir {}: {}", rel.display(), e))?;
        }
    }
    for rel in files {
        let abs = root.join(rel);
        let mut f =
            std::fs::File::open(&abs).map_err(|e| format!("open {}: {}", abs.display(), e))?;
        tar.append_file(rel, &mut f)
            .map_err(|e| format!("tar append {}: {}", rel.display(), e))?;
    }
    let enc = tar.into_inner().map_err(|e| format!("tar finish: {}", e))?;
    enc.finish().map_err(|e| format!("gz finish: {}", e))?;
    Ok(())
}

/// Tar+gzip the synced files under `root` into `w`. `include_runtime` bypasses
/// the strip set (still skips `.sync-trash/`). Enforces `MAX_SYNC_BYTES`.
/// Returns the uncompressed byte total.
pub fn tar_workspace_to<W: Write>(root: &Path, include_runtime: bool, w: W) -> Result<u64, String> {
    let runtime_keep = |rel: &Path| {
        let s = norm(rel);
        s != TRASH_PREFIX && !s.starts_with(&format!("{TRASH_PREFIX}/"))
    };
    let (files, empty_dirs) = if include_runtime {
        (
            walk(root, &runtime_keep)?,
            empty_dirs_for(root, &runtime_keep)?,
        )
    } else {
        (walk_synced(root)?, empty_synced_dirs(root)?)
    };
    let total: u64 = files
        .iter()
        .map(|r| {
            std::fs::metadata(root.join(r))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .sum();
    if total > MAX_SYNC_BYTES {
        return Err(format!(
            "workspace is {} MB, over the {} MB sync cap",
            total / 1_048_576,
            MAX_SYNC_BYTES / 1_048_576
        ));
    }
    write_tar(root, &files, &empty_dirs, w)?;
    Ok(total)
}

/// In-memory wrapper over [`tar_workspace_to`] (back-compat / tests).
pub fn tar_workspace(root: &Path, include_runtime: bool) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    tar_workspace_to(root, include_runtime, &mut buf)?;
    Ok(buf)
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Canonicalize `root`, creating it if missing.
fn canonical_root(root: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("mkdir {}: {}", root.display(), e))?;
    root.canonicalize()
        .map_err(|e| format!("canonicalize {}: {}", root.display(), e))
}

/// Reject a destination whose *resolved* parent escapes `root`.
///
/// [`is_unsafe_entry`] only sees the path string in the archive, so it stops
/// `../` and absolute entries but not a symlink already sitting in the
/// workspace: with `logs -> /var/log` on disk, the innocuous-looking entry
/// `logs/app.txt` writes outside the root. Sync never *transports* symlinks
/// (they are skipped when collecting), so one has to arrive some other way —
/// a user, or an agent, creating it. Cheap to close, and the caller can't
/// know it happened.
/// `dir` must exist; the file it will hold does not have to.
fn guard_within_root(root: &Path, dir: &Path, what: &Path) -> Result<(), String> {
    let real = dir
        .canonicalize()
        .map_err(|e| format!("resolve {}: {}", dir.display(), e))?;
    if !real.starts_with(root) {
        return Err(format!(
            "refused entry escaping the workspace: {} resolves under {}",
            what.display(),
            real.display()
        ));
    }
    Ok(())
}

/// `create_dir_all(dir)`, guarded: the deepest part of `dir` that already
/// exists is resolved and checked first, so a symlinked folder in the
/// workspace can't have directories created through it before the guard
/// notices, and the created tree is checked again.
fn mkdir_within_root(root: &Path, dir: &Path, what: &Path) -> Result<(), String> {
    let mut existing = dir;
    while !existing.exists() {
        match existing.parent() {
            Some(p) => existing = p,
            None => break,
        }
    }
    guard_within_root(root, existing, what)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {}", what.display(), e))?;
    guard_within_root(root, dir, what)
}

/// Extract a `.tar.gz` into the (canonical) `root`, overwriting in place.
/// Traversal-safe. Returns (files written, set of incoming relative paths).
///
/// Both limits below are on the *decompressed* stream. The transport caps
/// what arrives (`MAX_SYNC_BYTES` as the HTTP body limit) and the sender caps
/// what it collects, but neither bounds what a gzip expands to — a modest
/// upload can inflate without limit and fill the volume.
/// `.git/` teleports (history, branches, objects), but a desktop pull must
/// not bring in code that the user's own `git` would then run: hooks are
/// skipped, and a `.git/config` that changes an exec-capable key keeps the
/// local file (or, with none, lands without those keys). The cloud side
/// applies a push verbatim — that content is the user's own.
fn git_guard(rel: &Path) -> Option<GitEntry> {
    let s = norm(rel);
    let segs: Vec<&str> = s.split('/').collect();
    let g = segs.iter().position(|p| p.eq_ignore_ascii_case(".git"))?;
    match segs.get(g + 1).map(|p| p.to_ascii_lowercase()) {
        Some(h) if h == "hooks" && segs.len() > g + 2 => Some(GitEntry::Hook),
        Some(c) if c == "config" && segs.len() == g + 2 => Some(GitEntry::Config),
        _ => None,
    }
}

enum GitEntry {
    Hook,
    Config,
}

fn git_key_runs_code(section: &str, key: &str, value: &str) -> bool {
    let sec = section.to_ascii_lowercase();
    let key = key.to_ascii_lowercase();
    let base = sec.split_whitespace().next().unwrap_or("");
    match base {
        "core" => matches!(
            key.as_str(),
            "fsmonitor" | "hookspath" | "sshcommand" | "pager" | "editor" | "askpass" | "gitproxy"
        ),
        "alias" => value.trim_start().starts_with('!'),
        "filter" => matches!(key.as_str(), "clean" | "smudge" | "process"),
        "diff" => matches!(key.as_str(), "external" | "textconv" | "command"),
        "merge" | "mergetool" | "difftool" => matches!(key.as_str(), "driver" | "cmd" | "path"),
        "credential" => key == "helper",
        "gpg" => key == "program",
        "sequence" => key == "editor",
        "include" | "includeif" => key == "path",
        "uploadpack" => key == "packobjectshook",
        _ => false,
    }
}

/// Exec-capable `(section, key, value)` entries of a git config, and the
/// text with those lines removed.
fn git_config_exec_keys(text: &str) -> (Vec<(String, String, String)>, String) {
    let mut section = String::new();
    let mut found = Vec::new();
    let mut kept = String::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            section = t
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_string();
        } else if !t.is_empty() && !t.starts_with('#') && !t.starts_with(';') {
            let (k, v) = t.split_once('=').unwrap_or((t, ""));
            let (k, v) = (k.trim(), v.trim());
            if git_key_runs_code(&section, k, v) {
                found.push((
                    section.to_ascii_lowercase(),
                    k.to_ascii_lowercase(),
                    v.to_string(),
                ));
                continue;
            }
        }
        kept.push_str(line);
        kept.push('\n');
    }
    (found, kept)
}

fn extract_tarball<R: Read>(
    reader: R,
    root: &Path,
    max_bytes: u64,
) -> Result<(usize, BTreeSet<PathBuf>, Vec<String>), String> {
    let guard_git = !crate::config::trust::hosted();
    let mut notes = Vec::new();
    let mut written = 0usize;
    let mut total: u64 = 0;
    let mut incoming: BTreeSet<PathBuf> = BTreeSet::new();
    let mut archive = tar::Archive::new(GzDecoder::new(reader));
    for entry in archive
        .entries()
        .map_err(|e| format!("read archive: {}", e))?
    {
        let mut entry = entry.map_err(|e| format!("read entry: {}", e))?;
        let raw = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        if crate::sandbox::is_unsafe_archive_entry(&raw, cfg!(windows)) {
            return Err(format!("refused unsafe entry path: {raw}"));
        }
        let path = entry
            .path()
            .map_err(|e| format!("entry path: {}", e))?
            .into_owned();
        if entry.header().entry_type().is_dir() {
            // Empty-dir preservation: materialize directory entries so
            // scaffold folders (media/screenshots/, output/, …) survive.
            mkdir_within_root(root, &root.join(&path), &path)?;
            continue;
        }
        let out = root.join(&path);
        let git = if guard_git { git_guard(&path) } else { None };
        if let Some(GitEntry::Hook) = git {
            notes.push(format!("skipped git hook {}", norm(&path)));
            incoming.insert(path);
            continue;
        }
        if let Some(GitEntry::Config) = git {
            let mut buf = Vec::new();
            entry
                .by_ref()
                .take(1_048_576)
                .read_to_end(&mut buf)
                .map_err(|e| format!("read {}: {}", path.display(), e))?;
            let theirs = String::from_utf8_lossy(&buf).into_owned();
            let (their_keys, stripped) = git_config_exec_keys(&theirs);
            let local = std::fs::read_to_string(&out).ok();
            let local_keys = local.as_deref().map(|l| git_config_exec_keys(l).0);
            if !their_keys.is_empty() && local_keys.as_ref() != Some(&their_keys) {
                let keys: Vec<String> = their_keys
                    .iter()
                    .map(|(s, k, _)| format!("{s}.{k}"))
                    .collect();
                if local.is_some() {
                    notes.push(format!(
                        "kept local {} — the cloud copy changes {}",
                        norm(&path),
                        keys.join(", ")
                    ));
                } else {
                    if let Some(parent) = out.parent() {
                        mkdir_within_root(root, parent, &path)?;
                    }
                    std::fs::write(&out, stripped)
                        .map_err(|e| format!("write {}: {}", out.display(), e))?;
                    notes.push(format!("wrote {} without {}", norm(&path), keys.join(", ")));
                    written += 1;
                }
                incoming.insert(path);
                continue;
            }
            if let Some(parent) = out.parent() {
                mkdir_within_root(root, parent, &path)?;
            }
            std::fs::write(&out, &buf).map_err(|e| format!("write {}: {}", out.display(), e))?;
            incoming.insert(path);
            written += 1;
            continue;
        }
        if let Some(parent) = out.parent() {
            mkdir_within_root(root, parent, &path)?;
        }
        let mode = entry.header().mode().ok();
        let mut f =
            std::fs::File::create(&out).map_err(|e| format!("create {}: {}", out.display(), e))?;
        // Read one byte past what the budget allows: if the entry supplies it,
        // the archive is over the cap and we stop rather than fill the disk.
        let budget = max_bytes.saturating_sub(total);
        let n = std::io::copy(&mut entry.by_ref().take(budget + 1), &mut f)
            .map_err(|e| format!("write {}: {}", out.display(), e))?;
        drop(f);
        total = total.saturating_add(n);
        if total > max_bytes {
            let _ = std::fs::remove_file(&out);
            return Err(format!(
                "archive expands past the {} MiB limit — refusing to continue",
                max_bytes / 1_048_576
            ));
        }
        apply_mode(&out, mode);
        incoming.insert(path);
        written += 1;
    }
    Ok((written, incoming, notes))
}

/// Extract a full `.tar.gz` (streamed from `reader`) into `root`, overwriting in
/// place. When `delete` is set, synced files not present in the tarball are
/// moved to `.sync-trash/<ts>/` (recoverable mirror).
pub fn untar_workspace_from<R: Read>(
    reader: R,
    root: &Path,
    delete: bool,
) -> Result<UntarResult, String> {
    let root = canonical_root(root)?;
    let (written, incoming, notes) = extract_tarball(reader, &root, MAX_SYNC_BYTES)?;
    let trash = root.join(TRASH_PREFIX).join(unix_secs().to_string());
    let mut trash_used = false;
    let mut deleted = 0usize;
    if delete {
        for rel in walk_synced(&root)? {
            if !incoming.contains(&rel) {
                move_to_trash(&root, &trash, &rel, &mut trash_used)?;
                deleted += 1;
            }
        }
    }
    Ok(UntarResult {
        written,
        deleted,
        trash_dir: trash_used.then_some(trash),
        notes,
    })
}

/// In-memory wrapper over [`untar_workspace_from`] (back-compat / tests).
pub fn untar_workspace(bytes: &[u8], root: &Path, delete: bool) -> Result<UntarResult, String> {
    untar_workspace_from(std::io::Cursor::new(bytes), root, delete)
}

/// Restore the archived file mode. A teleport that drops the executable bit
/// silently breaks every `scripts/*.sh`, git hook, and helper binary on the
/// far end — `File::create` alone lands 0644. Only the permission bits are
/// honoured, and only the executable bit is allowed to vary (0755 vs 0644) so
/// a hostile or corrupt archive can't land setuid or world-writable files.
#[cfg(unix)]
fn apply_mode(path: &Path, mode: Option<u32>) {
    use std::os::unix::fs::PermissionsExt;
    let Some(mode) = mode else { return };
    let perms = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(perms));
}

#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: Option<u32>) {}

/// Reject archive entries that would escape the extraction root.
fn is_unsafe_entry(path: &Path) -> bool {
    crate::sandbox::is_unsafe_archive_entry(&norm(path), cfg!(windows))
}

fn move_to_trash(root: &Path, trash: &Path, rel: &Path, used: &mut bool) -> Result<(), String> {
    let src = root.join(rel);
    if !src.exists() {
        return Ok(());
    }
    let dst = trash.join(rel);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir trash {}: {}", parent.display(), e))?;
    }
    std::fs::rename(&src, &dst)
        .or_else(|_| {
            std::fs::copy(&src, &dst)
                .and_then(|_| std::fs::remove_file(&src))
                .map(|_| ())
        })
        .map_err(|e| format!("trash {}: {}", rel.display(), e))?;
    *used = true;
    Ok(())
}

// ---- P2: incremental manifest-diff ----

/// One file's identity in a sync manifest. `sha256` is the diff key (mtime is
/// unreliable across machines, so we hash content).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes.as_ref() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(Sha256::digest(data))
}

/// Hash one file without holding it in memory. `build_manifest` runs over the
/// whole tree — including multi-GB media under the 10 GiB cap — so reading a
/// file whole would spike RSS by its size on both the desktop and the runner
/// pod (which has a memory limit). Returns `(size, sha256)`.
fn hash_file(path: &Path) -> Result<(u64, String), String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((size, hex(hasher.finalize())))
}

/// Content manifest of the synced files under `root` (the diff input).
pub fn build_manifest(root: &Path) -> Result<Vec<FileEntry>, String> {
    let mut out = Vec::new();
    for rel in walk_synced(root)? {
        let (size, sha256) =
            hash_file(&root.join(&rel)).map_err(|e| format!("read {}: {}", rel.display(), e))?;
        out.push(FileEntry {
            path: norm(&rel),
            size,
            sha256,
        });
    }
    Ok(out)
}

/// Compare a source manifest against a destination manifest. Returns
/// `(to_transfer, extraneous)`: files in `src` that are missing or
/// content-different in `dst` (must be sent src→dst), and files in `dst` not in
/// `src` (candidates for `--delete`). Pure — the unit-testable heart of P2.
pub fn diff(src: &[FileEntry], dst: &[FileEntry]) -> (Vec<String>, Vec<String>) {
    use std::collections::{HashMap, HashSet};
    let dst_hash: HashMap<&str, &str> = dst
        .iter()
        .map(|e| (e.path.as_str(), e.sha256.as_str()))
        .collect();
    let src_paths: HashSet<&str> = src.iter().map(|e| e.path.as_str()).collect();
    let mut transfer: Vec<String> = src
        .iter()
        .filter(|e| {
            dst_hash
                .get(e.path.as_str())
                .map(|h| *h != e.sha256.as_str())
                .unwrap_or(true)
        })
        .map(|e| e.path.clone())
        .collect();
    let mut extraneous: Vec<String> = dst
        .iter()
        .filter(|e| !src_paths.contains(e.path.as_str()))
        .map(|e| e.path.clone())
        .collect();
    transfer.sort();
    extraneous.sort();
    (transfer, extraneous)
}

/// Order-independent content fingerprint of a manifest, over the real work
/// only (plumbing in [`FINGERPRINT_SKIP`] excluded). Two sides that hold the
/// same content produce the same fingerprint regardless of their per-end
/// binding/settings — the basis of the divergence check.
pub fn manifest_fingerprint(entries: &[FileEntry]) -> String {
    let mut parts: Vec<String> = entries
        .iter()
        .filter(|e| !FINGERPRINT_SKIP.contains(&e.path.as_str()))
        .map(|e| format!("{}\0{}", e.path, e.sha256))
        .collect();
    parts.sort();
    sha256_hex(parts.join("\n").as_bytes())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SyncBase {
    /// v1: whole-workspace fingerprint. Still written so an older engine
    /// reading this file keeps its guard instead of seeing "never synced".
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    /// v2: the per-file views each end held at the last successful sync. Two
    /// manifests, not one, because identical work can hash differently per end
    /// when the client's and runner's strip rules disagree — each side is
    /// judged against its OWN recorded view so that skew never reads as drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    local: Option<Vec<FileEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote: Option<Vec<FileEntry>>,
}

fn write_base(root: &Path, b: &SyncBase) -> Result<(), String> {
    let path = root.join(SYNC_BASE_REL);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {}", e))?;
    }
    let body = serde_json::to_vec(b).map_err(|e| format!("encode base: {}", e))?;
    std::fs::write(&path, body).map_err(|e| format!("write base: {}", e))
}

/// The content fingerprint recorded at the last successful sync (the agreed
/// state), or `None` if this folder has never completed one.
pub fn read_sync_base(root: &Path) -> Option<String> {
    std::fs::read(root.join(SYNC_BASE_REL))
        .ok()
        .and_then(|b| serde_json::from_slice::<SyncBase>(&b).ok())
        .and_then(|s| s.base)
}

/// Record the agreed-state fingerprint after a successful sync. Excluded from
/// the payload, so it stays local to this end. Drops any per-file base: the
/// callers that reach for this have no cloud-side view to record, so a stale
/// one would misattribute the next change.
pub fn write_sync_base(root: &Path, fingerprint: &str) -> Result<(), String> {
    write_base(
        root,
        &SyncBase {
            base: Some(fingerprint.to_string()),
            ..Default::default()
        },
    )
}

/// The per-file base views recorded at the last successful sync, `(local,
/// remote)`. `None` when this folder has no v2 base — never synced, or last
/// synced by an engine that only wrote the v1 fingerprint.
pub fn read_sync_base_manifests(root: &Path) -> Option<(Vec<FileEntry>, Vec<FileEntry>)> {
    let s: SyncBase = std::fs::read(root.join(SYNC_BASE_REL))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())?;
    Some((s.local?, s.remote?))
}

/// Record both ends' per-file views after a successful sync, so the NEXT sync
/// can tell WHICH end moved WHAT rather than only that something did.
pub fn write_sync_base_manifests(
    root: &Path,
    local: &[FileEntry],
    remote: &[FileEntry],
) -> Result<(), String> {
    write_base(
        root,
        &SyncBase {
            base: Some(manifest_fingerprint(remote)),
            local: Some(local.to_vec()),
            remote: Some(remote.to_vec()),
        },
    )
}

/// Which end moved each path since the agreed base.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reconcile {
    /// Changed on this machine only — a push carries them, a pull loses them.
    pub push: Vec<String>,
    /// Changed on the cloud only — a pull carries them, a push loses them.
    pub pull: Vec<String>,
    /// Both ends moved to DIFFERENT content: no safe automatic answer.
    pub conflicts: Vec<String>,
}

impl Reconcile {
    /// Nothing either end would destroy — the sync is safe in both directions.
    pub fn is_clean(&self) -> bool {
        self.push.is_empty() && self.pull.is_empty() && self.conflicts.is_empty()
    }
}

/// Three-way compare of the two ends against the base recorded at their last
/// successful sync. Paths where NEITHER end moved are left alone even if their
/// content differs today: that difference predates the base, which is exactly
/// the per-end strip-rule skew case. Clock-free — nothing here reads an mtime,
/// so it holds across machines with unsynced clocks.
/// Path→hash view of a manifest with the per-end plumbing filtered out (the
/// binding/settings/ignore files differ per end by design — see
/// [`FINGERPRINT_SKIP`]).
fn index(es: &[FileEntry]) -> BTreeMap<&str, &str> {
    es.iter()
        .filter(|e| !FINGERPRINT_SKIP.contains(&e.path.as_str()))
        .map(|e| (e.path.as_str(), e.sha256.as_str()))
        .collect()
}

/// Engine runtime state. It rides the teleport on purpose (sessions resume on
/// the other end), but the engine rewrites it just by RUNNING — team status
/// files, logs, locks — so it churns without the user touching anything.
/// Callers reporting "did my work change?" count it apart from real edits.
const STATE_PREFIX: &str = ".thclaws/state/";

/// Is this path engine-written runtime state rather than user work? Nested
/// too: a workspace host keeps each agent's own state under
/// `.thclaws/bots/<id>/.thclaws/state/`, and a workspace push carries those.
pub fn is_runtime_state(path: &str) -> bool {
    state_rest(path).is_some()
}

fn state_rest(path: &str) -> Option<&str> {
    path.strip_prefix(STATE_PREFIX).or_else(|| {
        path.find(&format!("/{STATE_PREFIX}"))
            .map(|i| &path[i + 1 + STATE_PREFIX.len()..])
    })
}

/// The part of runtime state that churns with no one working: agent-team
/// status/locks/logs and the per-machine bot address files. Both ends rewrite
/// these just by running, so a difference in them is never work at risk.
/// Sessions and KMS are state too, but they are someone's work — not churn.
pub fn is_engine_churn(path: &str) -> bool {
    state_rest(path).is_some_and(|rest| {
        rest.starts_with("team/")
            || rest.starts_with("bots/")
            || rest.ends_with(".log")
            || rest.ends_with(".lock")
    })
}

/// What ONE end changed since the base it recorded: `(changed, removed)`,
/// each sorted. This is the "is my folder dirty?" question — it needs no
/// network and no view of the other end, unlike [`reconcile`]. Same plumbing
/// filter, so per-end churn in the binding never reads as an edit.
pub fn drift_since_base(base: &[FileEntry], current: &[FileEntry]) -> (Vec<String>, Vec<String>) {
    let (b, c) = (index(base), index(current));
    let mut changed = Vec::new();
    for (path, hash) in &c {
        if b.get(path) != Some(hash) {
            changed.push(path.to_string());
        }
    }
    let mut removed = Vec::new();
    for path in b.keys() {
        if !c.contains_key(path) {
            removed.push(path.to_string());
        }
    }
    // BTreeMap iteration is key-ordered, so both lists come out sorted.
    (changed, removed)
}

pub fn reconcile(
    base_local: &[FileEntry],
    base_remote: &[FileEntry],
    local: &[FileEntry],
    remote: &[FileEntry],
) -> Reconcile {
    let (bl, br, l, r) = (
        index(base_local),
        index(base_remote),
        index(local),
        index(remote),
    );
    let mut out = Reconcile::default();
    let paths: BTreeSet<&str> = bl
        .keys()
        .chain(br.keys())
        .chain(l.keys())
        .chain(r.keys())
        .copied()
        .collect();
    for p in paths {
        let (lh, rh) = (l.get(p), r.get(p));
        if lh == rh {
            // Already agree — including both ends making the identical edit.
            continue;
        }
        match (lh != bl.get(p), rh != br.get(p)) {
            (true, true) => out.conflicts.push(p.to_string()),
            (true, false) => out.push.push(p.to_string()),
            (false, true) => out.pull.push(p.to_string()),
            (false, false) => {}
        }
    }
    out
}

/// Has `manifest` drifted from the recorded agreed state? `false` when there
/// is no base yet (first sync — nothing to clobber). Content-only, so it never
/// fires on the per-end plumbing differences and needs no clocks.
pub fn diverged_from_base(root: &Path, manifest: &[FileEntry]) -> bool {
    match read_sync_base(root) {
        Some(base) => manifest_fingerprint(manifest) != base,
        None => false,
    }
}

/// Tar+gzip a specific list of relative paths into `w` (incremental push body /
/// pull export). Skips missing or unsafe paths. Enforces `MAX_SYNC_BYTES`.
/// Returns the uncompressed byte total.
pub fn tar_paths_to<W: Write>(root: &Path, paths: &[String], w: W) -> Result<u64, String> {
    let mut total = 0u64;
    let mut valid: Vec<PathBuf> = Vec::new();
    for p in paths {
        let rel = Path::new(p);
        if is_unsafe_entry(rel) {
            continue;
        }
        if let Ok(m) = std::fs::metadata(root.join(rel)) {
            if m.is_file() {
                total += m.len();
                valid.push(rel.to_path_buf());
            }
        }
    }
    if total > MAX_SYNC_BYTES {
        return Err(format!(
            "changed files total {} MB, over the {} MB sync cap",
            total / 1_048_576,
            MAX_SYNC_BYTES / 1_048_576
        ));
    }
    // Every incremental push also carries the CURRENT empty dirs — dir
    // entries are ~free, create_dir_all on the receiver is idempotent,
    // and the manifest (files-only, fixed wire shape) can't express them.
    let empty_dirs = empty_synced_dirs(root)?;
    write_tar(root, &valid, &empty_dirs, w)?;
    Ok(total)
}

/// In-memory wrapper over [`tar_paths_to`] (back-compat / tests).
pub fn tar_paths(root: &Path, paths: &[String]) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    tar_paths_to(root, paths, &mut buf)?;
    Ok(buf)
}

/// Move a list of relative paths to `.sync-trash/<ts>/` (incremental `--delete`:
/// the partial tarball is applied via `untar_workspace(.., delete=false)`, then
/// the extraneous paths are trashed with this).
pub fn trash_paths(root: &Path, paths: &[String]) -> Result<UntarResult, String> {
    let root = canonical_root(root)?;
    let trash = root.join(TRASH_PREFIX).join(unix_secs().to_string());
    let mut trash_used = false;
    let mut deleted = 0usize;
    for p in paths {
        let rel = Path::new(p);
        if is_unsafe_entry(rel) {
            continue;
        }
        if root.join(rel).exists() {
            move_to_trash(&root, &trash, rel, &mut trash_used)?;
            deleted += 1;
        }
    }
    Ok(UntarResult {
        written: 0,
        deleted,
        trash_dir: trash_used.then_some(trash),
        notes: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_state_includes_a_hosted_agents_own_state() {
        for p in [
            ".thclaws/state/sessions/a.jsonl",
            ".thclaws/bots/main/.thclaws/state/sessions/a.jsonl",
        ] {
            assert!(is_runtime_state(p), "{p}");
        }
        assert!(!is_runtime_state("notes/.thclaws-state.md"));
        assert!(!is_runtime_state("plan.md"));
        for p in [
            ".thclaws/state/team/agents/lead/status.json",
            ".thclaws/bots/main/.thclaws/state/team/agents/lead/output.log",
            ".thclaws/bots/main/.thclaws/state/team/agents/lead/status.lock",
            ".thclaws/state/bots/main.addr",
        ] {
            assert!(is_engine_churn(p), "{p}");
        }
        for p in [
            ".thclaws/bots/main/.thclaws/state/sessions/a.jsonl",
            ".thclaws/state/kms/x/index.md",
            "logs/server.log",
        ] {
            assert!(!is_engine_churn(p), "{p}");
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("wssync-{tag}-{ts}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// dev-plan/59: publish and sync must disagree about `.thclaws/bots/`,
    /// and that disagreement is the whole reason bots live there.
    ///
    /// `pack::is_strippable` drops the shelf so a published agent never
    /// carries another agent. Sync must do the OPPOSITE and carry it, so a
    /// user's bots follow them to every device — `excluded` deliberately
    /// does not consult `is_strippable` ("push|pull keeps runtime state").
    ///
    /// If someone ever "tidies" these two into agreement, one direction
    /// silently uploads cookies to the catalogue and the other silently
    /// stops syncing bots. This pins both.
    #[test]
    fn sync_keeps_the_bot_shelf_that_publish_strips() {
        let root = Path::new("/does/not/need/to/exist");
        for rel in [
            ".thclaws/bots/research/.thclaws/settings.json",
            ".thclaws/bots/research/.thclaws/state/sessions/sess-1.jsonl",
            ".thclaws/bots/research/AGENTS.md",
            ".thclaws/bots/main/.thclaws/state/kms/vault/note.md",
        ] {
            let p = Path::new(rel);
            assert!(
                !excluded(root, p),
                "sync must carry {rel} — bots follow the user across devices"
            );
            assert!(
                crate::cloud::pack::is_strippable(p),
                "publish must drop {rel} — an agent ships no other agents"
            );
        }
    }

    #[test]
    fn roundtrip_and_strip() {
        let src = tmp("src");
        write(&src, "a.txt", "hello");
        write(&src, "sub/b.md", "world");
        write(&src, ".thclaws/settings.json", "{}");
        write(&src, ".thclaws/state/sessions/x.jsonl", "SESSION"); // teleported now
        write(&src, "node_modules/pkg/i.js", "js"); // stripped (regenerable)
        let bytes = tar_workspace(&src, false).unwrap();
        let dst = tmp("dst");
        let r = untar_workspace(&bytes, &dst, false).unwrap();
        assert_eq!(r.written, 4); // a.txt, sub/b.md, settings.json, state/sessions/x.jsonl
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");
        assert_eq!(
            std::fs::read_to_string(dst.join("sub/b.md")).unwrap(),
            "world"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join(".thclaws/state/sessions/x.jsonl")).unwrap(),
            "SESSION",
            "runtime state must teleport with push|pull"
        );
        assert!(
            !dst.join("node_modules/pkg/i.js").exists(),
            "regenerable build dirs must be stripped"
        );
    }

    #[test]
    fn delete_moves_extraneous_to_trash() {
        let dst = tmp("del");
        write(&dst, "keep.txt", "v1");
        write(&dst, "stale.txt", "old"); // not in tarball → should be trashed
        let src = tmp("delsrc");
        write(&src, "keep.txt", "v2");
        let bytes = tar_workspace(&src, false).unwrap();
        let r = untar_workspace(&bytes, &dst, true).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("keep.txt")).unwrap(), "v2");
        assert!(!dst.join("stale.txt").exists(), "extraneous removed");
        assert_eq!(r.deleted, 1);
        let trash = r.trash_dir.expect("trash created");
        assert_eq!(
            std::fs::read_to_string(trash.join("stale.txt")).unwrap(),
            "old",
            "recoverable in trash"
        );
    }

    #[test]
    fn no_delete_keeps_extraneous() {
        let dst = tmp("nodel");
        write(&dst, "stale.txt", "old");
        let src = tmp("nodelsrc");
        write(&src, "a.txt", "x");
        let bytes = tar_workspace(&src, false).unwrap();
        let r = untar_workspace(&bytes, &dst, false).unwrap();
        assert!(
            dst.join("stale.txt").exists(),
            "without --delete, extraneous stays"
        );
        assert_eq!(r.deleted, 0);
    }

    #[test]
    fn is_empty_treats_bare_settings_as_empty() {
        let root = tmp("empty");
        write(&root, ".thclaws/settings.json", "{}");
        assert!(is_empty(&root).unwrap());
        write(&root, "real.txt", "x");
        assert!(!is_empty(&root).unwrap());
    }

    #[test]
    fn binding_roundtrip() {
        let root = tmp("bind");
        let b = Binding {
            workspace_id: Some("ws-123".into()),
            slug: Some("my-agent".into()),
            ..Default::default()
        };
        write_binding(&root, &b).unwrap();
        assert_eq!(read_binding(&root).workspace_id.as_deref(), Some("ws-123"));
        assert_eq!(
            read_binding(&root).revision,
            None,
            "a binding written before revisions reads as unnumbered"
        );
    }

    #[test]
    fn revision_counts_completed_syncs_per_pairing() {
        let bound = |id: &str, rev: Option<u64>| Binding {
            workspace_id: Some(id.to_string()),
            revision: rev,
            ..Default::default()
        };
        // Never synced: the first sync lands rev 1.
        assert_eq!(next_revision(&Binding::default(), "ws-1", None), 1);
        // A pre-revision binding counts as 0, so it also lands rev 1.
        assert_eq!(next_revision(&bound("ws-1", None), "ws-1", None), 1);
        // Steady state: +1 each time.
        assert_eq!(next_revision(&bound("ws-1", Some(7)), "ws-1", Some(7)), 8);
        // The cloud moved on without us (another machine pushed) — go past
        // the highest either end has seen, never reuse a number.
        assert_eq!(next_revision(&bound("ws-1", Some(7)), "ws-1", Some(12)), 13);
        // We moved on without the cloud (a runner that missed the revision
        // call, or a fresh pod) — the local count still wins.
        assert_eq!(next_revision(&bound("ws-1", Some(7)), "ws-1", None), 8);
        // Re-pointing the folder at a DIFFERENT workspace starts that
        // pairing's count, not this folder's unrelated history.
        assert_eq!(next_revision(&bound("ws-1", Some(99)), "ws-2", Some(3)), 4);
        assert_eq!(next_revision(&bound("ws-1", Some(99)), "ws-2", None), 1);
    }

    #[test]
    fn revision_survives_a_binding_round_trip() {
        let root = tmp("bind-rev");
        write_binding(
            &root,
            &Binding {
                workspace_id: Some("ws-9".into()),
                revision: Some(4),
                ..Default::default()
            },
        )
        .unwrap();
        let b = read_binding(&root);
        assert_eq!(b.revision, Some(4));
        assert_eq!(next_revision(&b, "ws-9", Some(4)), 5);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_path_traversal() {
        // The safe tar Builder refuses to even write a `..` entry, so test the
        // guard predicate directly — it's what untar enforces on every entry.
        assert!(is_unsafe_entry(Path::new("../escape.txt")));
        assert!(is_unsafe_entry(Path::new("a/../../escape")));
        assert!(is_unsafe_entry(Path::new("/etc/passwd")));
        assert!(!is_unsafe_entry(Path::new("ok/rel.txt")));
        assert!(!is_unsafe_entry(Path::new("a/b/c.md")));
    }

    #[test]
    fn manifest_and_diff() {
        let a = tmp("mfa");
        write(&a, "same.txt", "x");
        write(&a, "changed.txt", "v1");
        write(&a, "only_a.txt", "a");
        let b = tmp("mfb");
        write(&b, "same.txt", "x");
        write(&b, "changed.txt", "v2");
        write(&b, "only_b.txt", "b");
        let (transfer, extraneous) =
            diff(&build_manifest(&a).unwrap(), &build_manifest(&b).unwrap());
        assert_eq!(
            transfer,
            vec!["changed.txt".to_string(), "only_a.txt".to_string()]
        );
        assert_eq!(extraneous, vec!["only_b.txt".to_string()]);
    }

    #[test]
    fn incremental_push_apply() {
        let dst = tmp("incdst");
        write(&dst, "keep.txt", "old");
        write(&dst, "stale.txt", "bye");
        let src = tmp("incsrc");
        write(&src, "keep.txt", "new");
        write(&src, "added.txt", "hi");
        let (transfer, extraneous) = diff(
            &build_manifest(&src).unwrap(),
            &build_manifest(&dst).unwrap(),
        );
        let tarball = tar_paths(&src, &transfer).unwrap();
        let w = untar_workspace(&tarball, &dst, false).unwrap();
        let t = trash_paths(&dst, &extraneous).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("keep.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("added.txt")).unwrap(),
            "hi"
        );
        assert!(!dst.join("stale.txt").exists(), "extraneous removed");
        assert_eq!(w.written, 2);
        assert_eq!(t.deleted, 1);
    }

    #[test]
    fn syncignore_excludes_prefixes_but_not_plumbing() {
        let root = tmp("ignore");
        write(&root, "keep.txt", "k");
        write(&root, "bigdata/blob.bin", "xxxx");
        write(&root, "bigdata/sub/deep.bin", "yyyy");
        write(&root, "node_modules/pkg/index.js", "js");
        write(&root, ".thclaws/settings.json", "{}");
        write(
            &root,
            ".thclaws/syncignore",
            "# comment\n\nbigdata/\nnode_modules\n.thclaws\n",
        );
        let files: Vec<String> = walk_synced(&root)
            .unwrap()
            .iter()
            .map(|r| norm(r))
            .collect();
        assert!(files.contains(&"keep.txt".to_string()));
        assert!(!files.iter().any(|f| f.starts_with("bigdata")));
        assert!(!files.iter().any(|f| f.starts_with("node_modules")));
        // Plumbing survives even a `.thclaws` wholesale ignore.
        assert!(files.contains(&".thclaws/settings.json".to_string()));
        assert!(files.contains(&".thclaws/syncignore".to_string()));
        // Prefix must respect the `/` boundary: `bigdata2.txt` is NOT
        // covered by the `bigdata` pattern.
        write(&root, "bigdata2.txt", "z");
        let files: Vec<String> = walk_synced(&root)
            .unwrap()
            .iter()
            .map(|r| norm(r))
            .collect();
        assert!(files.contains(&"bigdata2.txt".to_string()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn divergence_tracks_content_not_plumbing() {
        let root = tmp("diverge");
        write(&root, "src/main.rs", "fn main(){}");
        write(&root, ".thclaws/state/sessions/s.jsonl", "turn1");
        write(&root, ".thclaws/settings.json", "{}");
        // No base yet → first sync, nothing to clobber.
        let m0 = build_manifest(&root).unwrap();
        assert!(!diverged_from_base(&root, &m0));
        // Record the agreed state.
        write_sync_base(&root, &manifest_fingerprint(&m0)).unwrap();
        assert!(!diverged_from_base(&root, &build_manifest(&root).unwrap()));
        // Plumbing churn (settings + the moving binding) must NOT read as drift.
        write(&root, ".thclaws/settings.json", "{\"gatewayProxy\":true}");
        write(&root, ".thclaws/cloud-sync.json", "{\"last_push\":\"999\"}");
        assert!(
            !diverged_from_base(&root, &build_manifest(&root).unwrap()),
            "per-end plumbing must not count as divergence"
        );
        // Real work does.
        write(&root, ".thclaws/state/sessions/s.jsonl", "turn2");
        assert!(
            diverged_from_base(&root, &build_manifest(&root).unwrap()),
            "a changed session must read as divergence"
        );
        // The base file itself never travels.
        assert!(excluded(&root, Path::new(".thclaws/cloud-sync-base.json")));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn ent(path: &str, sha: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            size: sha.len() as u64,
            sha256: sha.to_string(),
        }
    }

    #[test]
    fn reconcile_attributes_each_change_to_the_end_that_made_it() {
        // Agreed base: both ends held the same four files.
        let base: Vec<FileEntry> = vec![
            ent("only_local.rs", "a"),
            ent("only_cloud.rs", "b"),
            ent("both.rs", "c"),
            ent("untouched.rs", "d"),
        ];
        let local = vec![
            ent("only_local.rs", "a2"),
            ent("only_cloud.rs", "b"),
            ent("both.rs", "c_local"),
            ent("untouched.rs", "d"),
            ent("new_local.rs", "n"),
        ];
        let remote = vec![
            ent("only_local.rs", "a"),
            ent("only_cloud.rs", "b2"),
            ent("both.rs", "c_cloud"),
            ent("untouched.rs", "d"),
        ];
        let r = reconcile(&base, &base, &local, &remote);
        assert_eq!(r.push, vec!["new_local.rs", "only_local.rs"]);
        assert_eq!(r.pull, vec!["only_cloud.rs"]);
        assert_eq!(r.conflicts, vec!["both.rs"]);
        assert!(!r.is_clean());
    }

    #[test]
    fn reconcile_is_clean_when_nothing_moved_or_both_made_the_same_edit() {
        let base = vec![ent("a.rs", "1"), ent("b.rs", "2")];
        // Identical edit on both ends is already agreed — not a conflict.
        let same = vec![ent("a.rs", "1"), ent("b.rs", "2_edited")];
        assert!(reconcile(&base, &base, &same, &same).is_clean());
        assert!(reconcile(&base, &base, &base, &base).is_clean());
    }

    #[test]
    fn reconcile_ignores_per_end_strip_skew() {
        // The regression 38d16bc4 chased: the client strips `build/` but the
        // runner (older engine) still reports it, so the two ends disagree on a
        // path NEITHER of them touched. Judging each end against its own
        // recorded view must leave it alone — otherwise every sync demands
        // --force forever.
        let base_local = vec![ent("src/main.rs", "m")];
        let base_remote = vec![ent("src/main.rs", "m"), ent("build/out.js", "stale")];
        let local = base_local.clone();
        let remote = base_remote.clone();
        assert!(
            reconcile(&base_local, &base_remote, &local, &remote).is_clean(),
            "pre-existing per-end skew must not read as a change"
        );
        // A real edit on top of that skew is still attributed correctly.
        let local2 = vec![ent("src/main.rs", "m2")];
        let r = reconcile(&base_local, &base_remote, &local2, &remote);
        assert_eq!(r.push, vec!["src/main.rs"]);
        assert!(r.pull.is_empty() && r.conflicts.is_empty());
    }

    #[test]
    fn drift_since_base_answers_dirty_without_the_other_end() {
        let base = vec![
            ent("kept.rs", "k"),
            ent("edited.rs", "e"),
            ent("gone.rs", "g"),
            ent(".thclaws/settings.json", "s"),
        ];
        // Same tree with one edit, one deletion, one addition — plus the
        // per-end settings churn that must NOT count as a local edit.
        let now = vec![
            ent("kept.rs", "k"),
            ent("edited.rs", "e2"),
            ent("added.rs", "a"),
            ent(".thclaws/settings.json", "s_overlay"),
        ];
        let (changed, removed) = drift_since_base(&base, &now);
        assert_eq!(
            changed,
            vec!["added.rs", "edited.rs"],
            "sorted, plumbing out"
        );
        assert_eq!(removed, vec!["gone.rs"]);

        // An untouched tree is clean even though the plumbing moved.
        let (changed, removed) = drift_since_base(&base, &base);
        assert!(changed.is_empty() && removed.is_empty());
        let mut plumbing_only = base.clone();
        plumbing_only[3] = ent(".thclaws/settings.json", "different");
        let (changed, removed) = drift_since_base(&base, &plumbing_only);
        assert!(
            changed.is_empty() && removed.is_empty(),
            "settings overlay is not a local change"
        );
    }

    #[test]
    fn build_manifest_hashes_large_files_without_holding_them() {
        // Guards the streaming hash: a file bigger than the 64 KiB read buffer
        // must hash to the same value as the in-memory digest.
        let root = tmp("bighash");
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(root.join("big.bin"), &body).unwrap();
        let m = build_manifest(&root).unwrap();
        let e = m.iter().find(|e| e.path == "big.bin").unwrap();
        assert_eq!(e.size, body.len() as u64);
        assert_eq!(e.sha256, sha256_hex(&body));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reconcile_tracks_deletions_and_skips_plumbing() {
        let base = vec![ent("gone.rs", "g"), ent(".thclaws/settings.json", "s")];
        // Deleted locally, still on the cloud → a local-side change to push.
        let local: Vec<FileEntry> = vec![ent(".thclaws/settings.json", "s_overlay")];
        let remote = vec![ent("gone.rs", "g"), ent(".thclaws/settings.json", "s")];
        let r = reconcile(&base, &base, &local, &remote);
        assert_eq!(r.push, vec!["gone.rs"]);
        assert!(
            r.pull.is_empty() && r.conflicts.is_empty(),
            "the per-end settings overlay must never count"
        );
    }

    #[test]
    fn per_file_base_round_trips_and_keeps_v1_compat() {
        let root = tmp("base-v2");
        write(&root, "src/main.rs", "fn main(){}");
        let local = build_manifest(&root).unwrap();
        let remote = vec![ent("src/main.rs", "different")];
        // No v2 base yet.
        assert!(read_sync_base_manifests(&root).is_none());
        write_sync_base_manifests(&root, &local, &remote).unwrap();
        let (bl, br) = read_sync_base_manifests(&root).unwrap();
        assert_eq!(bl, local);
        assert_eq!(br, remote);
        // An older engine reading the same file still finds its v1 watermark,
        // and it is the RUNNER's fingerprint (the pre-3-way contract).
        assert_eq!(read_sync_base(&root), Some(manifest_fingerprint(&remote)));
        // Writing a v1 base drops the stale per-file views.
        write_sync_base(&root, &manifest_fingerprint(&local)).unwrap();
        assert!(read_sync_base_manifests(&root).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_teleports_state_and_sessions_but_not_build_dirs() {
        let root = tmp("teleport");
        write(&root, "src/main.rs", "fn main(){}");
        write(&root, ".env", "SECRET=1");
        // Runtime state — the reported bug: sessions moved under state/ and
        // were being stripped. A teleport must carry all of it.
        write(&root, ".thclaws/state/sessions/s1.json", "{}");
        write(&root, ".thclaws/state/kms/key", "k");
        // Regenerable / arch-specific — never ride, incl. a NESTED node_modules.
        write(&root, "node_modules/pkg/index.js", "js");
        write(&root, "frontend/node_modules/x.js", "js");
        write(&root, "Cargo.toml", "[package]");
        write(&root, "target/debug/app", "bin");
        write(&root, "__pycache__/m.pyc", "x");
        // Marker heuristic: a book-style build/ (no package.json) is
        // CONTENT and must ride; a JS build/ (sibling package.json) is
        // regenerable output and must not.
        write(&root, "build/slides/ch01.pdf", "pdf");
        write(&root, "web/package.json", "{}");
        write(&root, "web/build/bundle.js", "js");
        write(&root, "tools/target/data.csv", "csv"); // no Cargo.toml → content
        let files: Vec<String> = walk_synced(&root)
            .unwrap()
            .iter()
            .map(|r| norm(r))
            .collect();
        assert!(files.contains(&".thclaws/state/sessions/s1.json".to_string()));
        assert!(files.contains(&".thclaws/state/kms/key".to_string()));
        assert!(files.contains(&".env".to_string()));
        assert!(files.contains(&"src/main.rs".to_string()));
        assert!(
            !files.iter().any(|f| f.contains("node_modules")),
            "nested/root node_modules dropped"
        );
        assert!(!files.iter().any(|f| f.starts_with("target/")));
        assert!(!files.iter().any(|f| f.contains("__pycache__")));
        assert!(
            files.contains(&"build/slides/ch01.pdf".to_string()),
            "book-style build/ (no package.json) must sync"
        );
        assert!(
            !files.iter().any(|f| f.starts_with("web/build/")),
            "JS build/ beside package.json must strip"
        );
        assert!(
            files.contains(&"tools/target/data.csv".to_string()),
            "target/ without Cargo.toml must sync"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_dirs_survive_roundtrip() {
        let root = tmp("emptydirs-src");
        write(&root, "chapters/ch01.md", "# ch1");
        // Scaffold dirs with no files — must survive a push.
        std::fs::create_dir_all(root.join("media/screenshots")).unwrap();
        std::fs::create_dir_all(root.join("media/generated")).unwrap();
        std::fs::create_dir_all(root.join("output")).unwrap();
        // A stripped empty dir must NOT ride.
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        // A dir that DOES have a file is not an "empty dir" but still lands.
        write(&root, "reports/r.md", "r");

        let empties: Vec<String> = empty_synced_dirs(&root)
            .unwrap()
            .iter()
            .map(|d| norm(d))
            .collect();
        assert!(empties.contains(&"media/screenshots".to_string()));
        assert!(empties.contains(&"media/generated".to_string()));
        assert!(empties.contains(&"output".to_string()));
        assert!(!empties.iter().any(|d| d.contains("node_modules")));
        assert!(
            !empties.contains(&"reports".to_string()),
            "reports has a file"
        );

        // Full round-trip: tar → untar into a fresh root.
        let bytes = tar_workspace(&root, false).unwrap();
        let dst = tmp("emptydirs-dst");
        untar_workspace(&bytes, &dst, false).unwrap();
        assert!(
            dst.join("media/screenshots").is_dir(),
            "empty scaffold dir teleported"
        );
        assert!(dst.join("media/generated").is_dir());
        assert!(dst.join("output").is_dir());
        assert!(dst.join("chapters/ch01.md").is_file());
        assert!(
            !dst.join("node_modules").exists(),
            "stripped dir not teleported"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn syncignore_applies_to_manifest_and_stat() {
        let root = tmp("ignore-manifest");
        write(&root, "a.txt", "a");
        write(&root, "skipme/b.txt", "b");
        write(&root, ".thclaws/syncignore", "skipme\n");
        let m = build_manifest(&root).unwrap();
        assert!(m.iter().any(|e| e.path == "a.txt"));
        assert!(!m.iter().any(|e| e.path.starts_with("skipme")));
        let s = stat_workspace(&root).unwrap();
        assert_eq!(s.file_count, m.len());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Build a `.tar.gz` from `(path, kind, body)` triples. `kind` is "f" for a
    /// regular file, "d" for a directory entry.
    fn tar_gz(entries: &[(&str, &str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for (name, kind, body) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(if *kind == "d" {
                tar::EntryType::Directory
            } else {
                tar::EntryType::Regular
            });
            h.set_cksum();
            b.append_data(&mut h, name, *body).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    /// `is_unsafe_entry` only inspects the archive's path string, so an entry
    /// whose parent is a symlink already on disk passes it and then writes
    /// wherever the link points. Sync never carries symlinks, so one has to be
    /// created locally — the point is that the extractor cannot assume it wasn't.
    #[cfg(unix)]
    #[test]
    fn extraction_refuses_to_write_through_a_preexisting_symlink() {
        let root = tmp("symlink-escape");
        let root = canonical_root(&root).unwrap();
        let outside = tmp("symlink-escape-outside");
        let outside = canonical_root(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("logs")).unwrap();

        let bytes = tar_gz(&[("logs/pwned.txt", "f", b"escaped")]);
        let err = extract_tarball(&bytes[..], &root, MAX_SYNC_BYTES).unwrap_err();

        assert!(err.contains("escaping the workspace"), "{err}");
        assert!(
            !outside.join("pwned.txt").exists(),
            "write escaped the root into {}",
            outside.display()
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// The guard ran after `create_dir_all`, so a directory entry under a
    /// symlinked folder created its tree outside the root before refusing.
    #[cfg(unix)]
    #[test]
    fn extraction_creates_no_directory_through_a_symlink() {
        let root = canonical_root(&tmp("symlink-mkdir")).unwrap();
        let outside = canonical_root(&tmp("symlink-mkdir-outside")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("logs")).unwrap();
        for entry in [
            ("logs/a/b", "d", &b""[..]),
            ("logs/c/d.txt", "f", &b"x"[..]),
        ] {
            let bytes = tar_gz(&[entry]);
            let err = extract_tarball(&bytes[..], &root, MAX_SYNC_BYTES).unwrap_err();
            assert!(err.contains("escaping the workspace"), "{err}");
        }
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// A path that stays inside must still extract — the guard has to reject
    /// escapes without rejecting ordinary nesting.
    #[test]
    fn extraction_still_accepts_ordinary_nested_paths() {
        let root = tmp("nested-ok");
        let root = canonical_root(&root).unwrap();
        let bytes = tar_gz(&[("a/b/c.txt", "f", b"fine"), ("a/empty", "d", b"")]);
        let (written, incoming, _) = extract_tarball(&bytes[..], &root, MAX_SYNC_BYTES).unwrap();
        assert_eq!(written, 1);
        assert!(incoming.contains(Path::new("a/b/c.txt")));
        assert_eq!(std::fs::read(root.join("a/b/c.txt")).unwrap(), b"fine");
        assert!(root.join("a/empty").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A cloud agent can write `.git/hooks/*` or `core.fsmonitor`; on pull
    /// the user's own `git` would run them. History still teleports.
    #[test]
    fn pull_skips_git_hooks_and_keeps_local_exec_config() {
        let root = canonical_root(&tmp("git-guard")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
        let evil = "[core]\n\tbare = false\n\tfsmonitor = touch PWNED\n[alias]\n\tst = status\n";
        let bytes = tar_gz(&[
            (".git/hooks/pre-commit", "f", b"#!/bin/sh\ntouch PWNED\n"),
            (".git/config", "f", evil.as_bytes()),
            (".git/HEAD", "f", b"ref: refs/heads/main\n"),
            ("sub/.git/hooks/post-checkout", "f", b"x"),
        ]);
        let (_, _, notes) = extract_tarball(&bytes[..], &root, MAX_SYNC_BYTES).unwrap();
        assert!(!root.join(".git/hooks/pre-commit").exists());
        assert!(!root.join("sub/.git/hooks/post-checkout").exists());
        assert_eq!(
            std::fs::read_to_string(root.join(".git/config")).unwrap(),
            "[core]\n\tbare = false\n"
        );
        assert_eq!(
            std::fs::read(root.join(".git/HEAD")).unwrap(),
            b"ref: refs/heads/main\n"
        );
        assert!(
            notes.iter().any(|n| n.contains("core.fsmonitor")),
            "{notes:?}"
        );

        std::fs::remove_file(root.join(".git/config")).unwrap();
        extract_tarball(&bytes[..], &root, MAX_SYNC_BYTES).unwrap();
        let fresh = std::fs::read_to_string(root.join(".git/config")).unwrap();
        assert!(
            !fresh.contains("fsmonitor") && fresh.contains("st = status"),
            "{fresh}"
        );

        let benign = "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = x\n";
        extract_tarball(
            &tar_gz(&[(".git/config", "f", benign.as_bytes())])[..],
            &root,
            MAX_SYNC_BYTES,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join(".git/config")).unwrap(),
            benign
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn exec_capable_git_keys() {
        let (keys, _) = git_config_exec_keys(
            "[alias]\n co = checkout\n x = !sh -c evil\n[filter \"lfs\"]\n smudge = git-lfs smudge\n[user]\n name = a\n",
        );
        let names: Vec<String> = keys.iter().map(|(s, k, _)| format!("{s}.{k}")).collect();
        assert_eq!(names, vec!["alias.x", "filter \"lfs\".smudge"]);
    }

    /// Reads through, counting what the extractor actually consumed.
    struct Counted<'a> {
        inner: &'a [u8],
        read: std::rc::Rc<std::cell::Cell<usize>>,
    }
    impl Read for Counted<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read.set(self.read.get() + n);
            Ok(n)
        }
    }

    /// The transport caps the COMPRESSED upload; nothing bounded the expansion,
    /// so a small archive could write without limit and fill the volume.
    ///
    /// Reporting the overrun is the easy half. The half that matters is
    /// *stopping* — an extractor that writes the whole bomb and only then
    /// complains has already done the damage. So this asserts on how much the
    /// extractor consumed, not just on the error: with an incompressible
    /// payload, bytes read off the wire track bytes written to disk.
    #[test]
    fn extraction_stops_reading_once_the_decompressed_stream_passes_the_cap() {
        let root = tmp("bomb");
        let root = canonical_root(&root).unwrap();

        // Incompressible, so gzip can't hide the size and "consumed" is
        // meaningful. Cheap LCG rather than a dev-dependency.
        let mut payload = vec![0u8; 4 * 1024 * 1024];
        let mut x: u32 = 0x1234_5678;
        for b in payload.iter_mut() {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            *b = (x >> 16) as u8;
        }
        let bytes = tar_gz(&[("big.bin", "f", &payload)]);
        assert!(
            bytes.len() > 3 * 1024 * 1024,
            "payload compressed unexpectedly"
        );

        let cap = 64 * 1024;
        let read = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let src = Counted {
            inner: &bytes[..],
            read: read.clone(),
        };
        let err = extract_tarball(src, &root, cap).unwrap_err();

        assert!(err.contains("expands past"), "{err}");
        assert!(
            !root.join("big.bin").exists(),
            "the partial write was left behind"
        );
        // The budget must stop the copy near the cap. Without it the extractor
        // drains all 4 MiB before noticing.
        assert!(
            (read.get() as u64) < cap * 8,
            "read {} bytes for a {} byte cap — the copy was not bounded",
            read.get(),
            cap
        );

        // Same archive under a cap it fits: extraction proceeds normally.
        let (written, _, _) = extract_tarball(&bytes[..], &root, MAX_SYNC_BYTES).unwrap();
        assert_eq!(written, 1);
        assert_eq!(
            std::fs::metadata(root.join("big.bin")).unwrap().len(),
            payload.len() as u64
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
