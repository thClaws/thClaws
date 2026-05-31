//! Shell discovery + resolution.
//!
//! Tier 1 shipped embedded built-ins only. Tier 2 adds FS discovery:
//! `~/.config/thclaws/gui-shell/<id>/` (user, cross-project) and
//! `./.thclaws/gui-shell/<id>/` (project-scoped). Project overrides
//! user overrides built-in by id, so a team can ship a customised
//! shell in the repo without forking the user's installed copy.

use super::manifest::ShellManifest;
use crate::error::{Error, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Embedded asset entry — one file inside a built-in shell's folder.
#[derive(Debug, Clone)]
pub struct EmbeddedAsset {
    pub bytes: &'static [u8],
    pub mime: &'static str,
}

/// A built-in shell baked into the binary.
#[derive(Debug, Clone)]
pub struct EmbeddedShell {
    pub manifest: ShellManifest,
    pub assets: HashMap<&'static str, EmbeddedAsset>,
}

/// Where a shell lives. `Embedded` is compiled into the binary;
/// `OnDisk` is a folder under `~/.config/thclaws/gui-shell/` or
/// `./.thclaws/gui-shell/`.
#[derive(Debug, Clone)]
pub enum ShellRef {
    Embedded(EmbeddedShell),
    OnDisk {
        manifest: ShellManifest,
        root: PathBuf,
        /// Tracks origin (`"user"` or `"project"`) for the picker
        /// source badge + permission grant scoping.
        source: ShellSource,
    },
}

/// Origin of a shell — used by the picker for the source badge and
/// (Tier 3) for permission-grant scoping (project shells are usually
/// trusted within the team, user shells are per-user trust).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellSource {
    Builtin,
    User,
    Project,
}

impl ShellSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            ShellSource::Builtin => "builtin",
            ShellSource::User => "user",
            ShellSource::Project => "project",
        }
    }
}

impl ShellRef {
    pub fn manifest(&self) -> &ShellManifest {
        match self {
            ShellRef::Embedded(e) => &e.manifest,
            ShellRef::OnDisk { manifest, .. } => manifest,
        }
    }

    pub fn source(&self) -> ShellSource {
        match self {
            ShellRef::Embedded(_) => ShellSource::Builtin,
            ShellRef::OnDisk { source, .. } => *source,
        }
    }

    /// Resolved root directory for this shell. For embedded shells the
    /// concept doesn't really apply (assets are in-binary); returns
    /// None. Used by Mode B's Axum router + Tier 3's `fs.shell-scoped`
    /// permission to scope FS access.
    pub fn root(&self) -> Option<&Path> {
        match self {
            ShellRef::Embedded(_) => None,
            ShellRef::OnDisk { root, .. } => Some(root.as_path()),
        }
    }

    /// Materialise all of an embedded shell's assets to a per-user
    /// shadow directory at `~/.cache/thclaws/gui-shell/<id>/` and
    /// return the path. For `OnDisk` shells, returns the existing
    /// root (no-op — the files are already on disk).
    ///
    /// Used by Mode B serve startup so the agent's project-root
    /// loaders (AGENTS.md, `.thclaws/settings.json`, KMS, etc.)
    /// have a real directory to read from when the shell itself was
    /// compiled into the binary.
    ///
    /// Idempotent: if a shadow file already matches the embedded
    /// bytes, it's left alone. Otherwise it's rewritten so binary
    /// upgrades pick up new versions of bundled assets.
    pub fn ensure_shadow_root(&self) -> Result<PathBuf> {
        match self {
            ShellRef::OnDisk { root, .. } => Ok(root.clone()),
            ShellRef::Embedded(shell) => {
                let shadow = shadow_dir_for(&shell.manifest.id)?;
                std::fs::create_dir_all(&shadow).map_err(|e| {
                    Error::Tool(format!(
                        "gui-shell: cannot create shadow dir {}: {e}",
                        shadow.display()
                    ))
                })?;
                for (rel, asset) in &shell.assets {
                    let dst = shadow.join(rel);
                    if let Some(parent) = dst.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            Error::Tool(format!(
                                "gui-shell: cannot create shadow subdir {}: {e}",
                                parent.display()
                            ))
                        })?;
                    }
                    if let Ok(existing) = std::fs::read(&dst) {
                        if existing == asset.bytes {
                            continue;
                        }
                    }
                    std::fs::write(&dst, asset.bytes).map_err(|e| {
                        Error::Tool(format!(
                            "gui-shell: cannot write shadow asset {}: {e}",
                            dst.display()
                        ))
                    })?;
                }
                Ok(shadow)
            }
        }
    }

    /// Look up an asset relative to the shell root. For embedded shells
    /// the path set is fixed at compile time. For on-disk shells the
    /// path is validated through `Sandbox::check_in(&root, rel)` —
    /// same algorithm Sandbox::check uses for workspace files — to
    /// catch `..` escapes and symlinks pointing outside the shell
    /// folder.
    pub fn read_asset(&self, rel: &str) -> Result<(Vec<u8>, &'static str)> {
        match self {
            ShellRef::Embedded(e) => e
                .assets
                .get(rel)
                .map(|a| (a.bytes.to_vec(), a.mime))
                .ok_or_else(|| {
                    Error::Tool(format!(
                        "gui-shell: asset '{}' not found in shell '{}'",
                        rel, e.manifest.id
                    ))
                }),
            ShellRef::OnDisk { manifest, root, .. } => {
                let resolved = crate::sandbox::Sandbox::check_in(root, rel)?;
                let bytes = std::fs::read(&resolved).map_err(|e| {
                    Error::Tool(format!(
                        "gui-shell: asset '{}' in shell '{}' not readable: {e}",
                        rel, manifest.id
                    ))
                })?;
                let mime = mime_for_path(&resolved);
                Ok((bytes, mime))
            }
        }
    }
}

fn mime_for_path(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "application/octet-stream",
    }
}

/// Discovery registry. Holds embedded built-ins plus FS-discovered
/// user (`~/.config/thclaws/gui-shell/`) and project
/// (`./.thclaws/gui-shell/`) shells. Resolution precedence:
/// project > user > builtin (so a project ships an override of a
/// public shell by reusing its id).
pub struct ShellRegistry {
    builtin: HashMap<String, EmbeddedShell>,
    user: HashMap<String, OnDiskEntry>,
    project: HashMap<String, OnDiskEntry>,
}

#[derive(Debug, Clone)]
struct OnDiskEntry {
    manifest: ShellManifest,
    root: PathBuf,
}

impl ShellRegistry {
    /// Build a registry with built-ins + FS scan of both user and
    /// project shell directories. Project dir is resolved relative to
    /// the current working directory at call time.
    pub fn new() -> Self {
        let builtin = Self::builtin_only_map();
        let user = scan_dir(&user_shell_dir());
        let project = scan_dir(&project_shell_dir());
        Self {
            builtin,
            user,
            project,
        }
    }

    /// Built-ins-only constructor — used by tests that want a
    /// deterministic registry without touching the user's real
    /// `~/.config/thclaws/`.
    pub fn builtin_only() -> Self {
        Self {
            builtin: Self::builtin_only_map(),
            user: HashMap::new(),
            project: HashMap::new(),
        }
    }

    fn builtin_only_map() -> HashMap<String, EmbeddedShell> {
        let mut builtin = HashMap::new();
        for shell in [session_explorer(), chatbot()] {
            builtin.insert(shell.manifest.id.clone(), shell);
        }
        builtin
    }

    /// Rescan the on-disk dirs. Built-ins never change. Called by the
    /// frontend's "Refresh shells" button (Tier 2 has no live FS watcher
    /// — Tier 3 adds one).
    pub fn rescan(&mut self) {
        self.user = scan_dir(&user_shell_dir());
        self.project = scan_dir(&project_shell_dir());
    }

    pub fn resolve(&self, id: &str) -> Option<ShellRef> {
        if let Some(e) = self.project.get(id) {
            return Some(ShellRef::OnDisk {
                manifest: e.manifest.clone(),
                root: e.root.clone(),
                source: ShellSource::Project,
            });
        }
        if let Some(e) = self.user.get(id) {
            return Some(ShellRef::OnDisk {
                manifest: e.manifest.clone(),
                root: e.root.clone(),
                source: ShellSource::User,
            });
        }
        self.builtin.get(id).cloned().map(ShellRef::Embedded)
    }

    /// Merged list across all sources, deduped by id with the same
    /// precedence as `resolve`. Each entry carries its source so the
    /// picker can render the badge.
    pub fn list(&self) -> Vec<(ShellSource, ShellManifest)> {
        let mut out: Vec<(ShellSource, ShellManifest)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for e in self.project.values() {
            seen.insert(e.manifest.id.clone());
            out.push((ShellSource::Project, e.manifest.clone()));
        }
        for e in self.user.values() {
            if seen.insert(e.manifest.id.clone()) {
                out.push((ShellSource::User, e.manifest.clone()));
            }
        }
        for e in self.builtin.values() {
            if seen.insert(e.manifest.id.clone()) {
                out.push((ShellSource::Builtin, e.manifest.clone()));
            }
        }
        out
    }
}

impl Default for ShellRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// `~/.config/thclaws/gui-shell/` — matches the user-config convention
/// the LINE/Telegram adapters use (see telegram::config::path).
/// Falls back to `./.thclaws/gui-shell/` if HOME is unset (shouldn't
/// happen on any reasonable OS — the project dir scan would catch
/// the same shells anyway in that degenerate case).
fn user_shell_dir() -> PathBuf {
    if let Some(home) = crate::util::home_dir() {
        return home.join(".config").join("thclaws").join("gui-shell");
    }
    PathBuf::from(".thclaws").join("gui-shell")
}

/// `./.thclaws/gui-shell/` — project-scoped, sits next to
/// `./.thclaws/sessions/`. Resolved relative to current cwd at call time.
fn project_shell_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".thclaws")
        .join("gui-shell")
}

/// `~/.cache/thclaws/gui-shell/<id>/` — shadow directory where an
/// embedded built-in's assets are materialised so the agent's
/// project-root loaders (AGENTS.md, .thclaws/settings.json) have a
/// real directory to read from. macOS uses `~/Library/Caches/` per
/// XDG-equivalent; Linux/Windows use the standard cache dir.
fn shadow_dir_for(shell_id: &str) -> Result<PathBuf> {
    let home = crate::util::home_dir()
        .ok_or_else(|| Error::Tool("gui-shell: HOME not set; cannot resolve shadow dir".into()))?;
    // Reject path-traversal in shell id before joining.
    if shell_id.is_empty()
        || shell_id.contains('/')
        || shell_id.contains('\\')
        || shell_id.contains("..")
    {
        return Err(Error::Tool(format!(
            "gui-shell: invalid shell id for shadow dir: '{shell_id}'"
        )));
    }
    let cache_base = if cfg!(target_os = "macos") {
        home.join("Library").join("Caches")
    } else {
        home.join(".cache")
    };
    Ok(cache_base.join("thclaws").join("gui-shell").join(shell_id))
}

/// Walk a discovery directory: each immediate subdirectory that
/// contains a parseable `manifest.json` becomes a registry entry.
/// Silent on errors — a malformed shell folder shouldn't break
/// discovery for siblings.
fn scan_dir(dir: &Path) -> HashMap<String, OnDiskEntry> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("manifest.json");
        let Ok(text) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<ShellManifest>(&text) else {
            continue;
        };
        // Folder name and manifest id can disagree — prefer the id from
        // the manifest (it's what the picker + URL routing use) but
        // require the folder to exist where we expected it.
        out.insert(
            manifest.id.clone(),
            OnDiskEntry {
                manifest,
                root: path,
            },
        );
    }
    out
}

/// Build an embedded shell from compile-time asset bytes. Used by
/// each built-in's factory to avoid repeating the asset-table
/// boilerplate. The asset slice maps `rel_path → (bytes, mime)`.
fn build_embedded_shell(
    manifest_json: &'static str,
    assets_in: &[(&'static str, &'static [u8], &'static str)],
    failure_label: &'static str,
) -> EmbeddedShell {
    let manifest: ShellManifest = serde_json::from_str(manifest_json)
        .unwrap_or_else(|e| panic!("built-in {failure_label} manifest invalid: {e}"));
    let mut assets = HashMap::new();
    for (rel, bytes, mime) in assets_in {
        assets.insert(*rel, EmbeddedAsset { bytes, mime });
    }
    EmbeddedShell { manifest, assets }
}

/// Session Explorer — Tier 1 demo shell. Read-only browser of past
/// session JSONL files.
fn session_explorer() -> EmbeddedShell {
    build_embedded_shell(
        include_str!("../../assets/gui-shells/session-explorer/manifest.json"),
        &[
            (
                "index.html",
                include_bytes!("../../assets/gui-shells/session-explorer/index.html"),
                "text/html; charset=utf-8",
            ),
            (
                "main.js",
                include_bytes!("../../assets/gui-shells/session-explorer/main.js"),
                "application/javascript; charset=utf-8",
            ),
            (
                "style.css",
                include_bytes!("../../assets/gui-shells/session-explorer/style.css"),
                "text/css; charset=utf-8",
            ),
            (
                "icon.svg",
                include_bytes!("../../assets/gui-shells/session-explorer/icon.svg"),
                "image/svg+xml",
            ),
        ],
        "session-explorer",
    )
}

// Image Generator embedded built-in was removed (b948874+1) because
// its stub `image_gen` tool was registering in the agent's tool
// registry alongside user-configured MCP image generators (e.g.
// pinn.ai's text2image) and confusing model tool-pick decisions.
// Users who want an Image Generator shell install their own project
// shell with their own AGENTS.md pointing at the real provider —
// reference layout is `gui-shell-tests/image-gen/` in the workspace
// (project-scoped, not embedded).
//
// To restore an embedded image-gen demo later: ship a per-user
// THCLAWS_ENABLE_IMAGE_GEN_STUB env-gated tool + re-register
// here. dev-plan/33 Tier 3 marketplace work can revisit.

/// Chatbot — minimal example shell. Sends user messages to the
/// agent loop via `thclaws.run()`, streams replies back via
/// `thclaws.on("text"/"done"/"error")`, persists history with
/// `thclaws.storage`. Declares only `agent.run` permission — no
/// tool dependencies, no MCP requirements. Works with any
/// configured model out of the box. Reference example for shell
/// authors who want a conversational UI.
fn chatbot() -> EmbeddedShell {
    build_embedded_shell(
        include_str!("../../assets/gui-shells/chatbot/manifest.json"),
        &[
            (
                "index.html",
                include_bytes!("../../assets/gui-shells/chatbot/index.html"),
                "text/html; charset=utf-8",
            ),
            (
                "main.js",
                include_bytes!("../../assets/gui-shells/chatbot/main.js"),
                "application/javascript; charset=utf-8",
            ),
            (
                "style.css",
                include_bytes!("../../assets/gui-shells/chatbot/style.css"),
                "text/css; charset=utf-8",
            ),
            (
                "icon.svg",
                include_bytes!("../../assets/gui-shells/chatbot/icon.svg"),
                "image/svg+xml",
            ),
            (
                "AGENTS.md",
                include_bytes!("../../assets/gui-shells/chatbot/AGENTS.md"),
                "text/markdown; charset=utf-8",
            ),
        ],
        "chatbot",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn registry_resolves_session_explorer() {
        let r = ShellRegistry::builtin_only();
        let s = r.resolve("session-explorer").expect("built-in present");
        assert_eq!(s.manifest().id, "session-explorer");
        assert_eq!(s.source(), ShellSource::Builtin);
    }

    #[test]
    fn registry_returns_none_for_unknown() {
        let r = ShellRegistry::builtin_only();
        assert!(r.resolve("does-not-exist").is_none());
    }

    #[test]
    fn registry_lists_all_builtins() {
        let r = ShellRegistry::builtin_only();
        let listed = r.list();
        let names: Vec<_> = listed.iter().map(|(_, m)| m.id.as_str()).collect();
        assert!(names.contains(&"session-explorer"));
        assert!(names.contains(&"chatbot"));
    }

    #[test]
    fn registry_resolves_chatbot() {
        let r = ShellRegistry::builtin_only();
        let s = r.resolve("chatbot").expect("built-in present");
        assert_eq!(s.manifest().id, "chatbot");
        assert_eq!(s.source(), ShellSource::Builtin);
        // Must declare only the minimum permission (agent.run) — no
        // tool / fs perms that could escalate. Important security
        // invariant for the "minimal example" framing.
        assert_eq!(s.manifest().permissions, vec!["agent.run".to_string()]);
    }

    #[test]
    fn chatbot_ships_all_expected_assets() {
        let r = ShellRegistry::builtin_only();
        let s = r.resolve("chatbot").unwrap();
        for rel in [
            "index.html",
            "main.js",
            "style.css",
            "icon.svg",
            "AGENTS.md",
        ] {
            let (bytes, _) = s
                .read_asset(rel)
                .unwrap_or_else(|e| panic!("chatbot asset {rel}: {e}"));
            assert!(!bytes.is_empty(), "{rel} is empty");
        }
    }

    #[test]
    fn embedded_shell_serves_index_html() {
        let r = ShellRegistry::builtin_only();
        let s = r.resolve("session-explorer").unwrap();
        let (bytes, mime) = s.read_asset("index.html").unwrap();
        assert!(!bytes.is_empty());
        assert!(mime.starts_with("text/html"));
    }

    #[test]
    fn embedded_shell_missing_asset_errors() {
        let r = ShellRegistry::builtin_only();
        let s = r.resolve("session-explorer").unwrap();
        assert!(s.read_asset("nope.html").is_err());
    }

    // --- Tier 2 FS discovery tests --------------------------------

    /// Build a fake shell folder under a tempdir with a working manifest
    /// + index.html. Returns the registry that points at it (via
    /// hand-constructed entries — bypasses `new()` so the test doesn't
    /// depend on cwd / HOME state).
    fn registry_with_user_shell(root: &Path, id: &str) -> ShellRegistry {
        let shell_dir = root.join(id);
        std::fs::create_dir_all(&shell_dir).unwrap();
        std::fs::write(
            shell_dir.join("manifest.json"),
            format!(
                r#"{{"id":"{id}","name":"X","version":"0.1.0","description":"","entry":"index.html","permissions":[]}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            shell_dir.join("index.html"),
            b"<!doctype html><body>hi</body>",
        )
        .unwrap();
        let mut r = ShellRegistry::builtin_only();
        r.user = scan_dir(root);
        r
    }

    #[test]
    fn fs_discovery_resolves_user_shell() {
        let tmp = tempdir().unwrap();
        let r = registry_with_user_shell(tmp.path(), "my-user-shell");
        let s = r.resolve("my-user-shell").expect("user shell present");
        assert_eq!(s.source(), ShellSource::User);
        assert_eq!(s.manifest().id, "my-user-shell");
        let (bytes, mime) = s.read_asset("index.html").unwrap();
        assert!(mime.starts_with("text/html"));
        assert!(std::str::from_utf8(&bytes).unwrap().contains("hi"));
    }

    #[test]
    fn fs_discovery_project_overrides_user_by_id() {
        let user_tmp = tempdir().unwrap();
        let proj_tmp = tempdir().unwrap();
        let id = "shared-id";
        // Create user copy.
        let mut r = registry_with_user_shell(user_tmp.path(), id);
        // Create project copy in a different folder, point project map at it.
        std::fs::create_dir_all(proj_tmp.path().join(id)).unwrap();
        std::fs::write(
            proj_tmp.path().join(id).join("manifest.json"),
            format!(
                r#"{{"id":"{id}","name":"PROJECT","version":"9.9.9","description":"","entry":"index.html","permissions":[]}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            proj_tmp.path().join(id).join("index.html"),
            b"<!doctype html>",
        )
        .unwrap();
        r.project = scan_dir(proj_tmp.path());

        let s = r.resolve(id).unwrap();
        assert_eq!(s.source(), ShellSource::Project);
        assert_eq!(s.manifest().name, "PROJECT");
    }

    #[test]
    fn fs_discovery_rejects_path_traversal() {
        let tmp = tempdir().unwrap();
        let r = registry_with_user_shell(tmp.path(), "traversal-test");
        let s = r.resolve("traversal-test").unwrap();
        // `..` escape collapsed by Sandbox::check_in → canonicalize
        // → starts_with(root) check rejects.
        assert!(s.read_asset("../../etc/passwd").is_err());
    }

    #[test]
    fn fs_discovery_ignores_malformed_manifest() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("broken-shell");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), b"not json").unwrap();
        let scanned = scan_dir(tmp.path());
        assert!(
            scanned.is_empty(),
            "malformed manifest should silently skip, got: {:?}",
            scanned.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn list_merges_and_dedupes_by_id() {
        let user_tmp = tempdir().unwrap();
        let proj_tmp = tempdir().unwrap();
        let mut r = ShellRegistry::builtin_only();
        // User has "u1" and "shared".
        for id in ["u1", "shared"] {
            std::fs::create_dir_all(user_tmp.path().join(id)).unwrap();
            std::fs::write(
                user_tmp.path().join(id).join("manifest.json"),
                format!(
                    r#"{{"id":"{id}","name":"U","version":"0.1.0","description":"","entry":"index.html","permissions":[]}}"#
                ),
            )
            .unwrap();
        }
        // Project has "p1" and "shared".
        for id in ["p1", "shared"] {
            std::fs::create_dir_all(proj_tmp.path().join(id)).unwrap();
            std::fs::write(
                proj_tmp.path().join(id).join("manifest.json"),
                format!(
                    r#"{{"id":"{id}","name":"P","version":"0.1.0","description":"","entry":"index.html","permissions":[]}}"#
                ),
            )
            .unwrap();
        }
        r.user = scan_dir(user_tmp.path());
        r.project = scan_dir(proj_tmp.path());

        let listed = r.list();
        let ids: Vec<&str> = listed.iter().map(|(_, m)| m.id.as_str()).collect();
        // session-explorer (builtin), u1 (user), p1 + shared (project)
        assert!(ids.contains(&"session-explorer"));
        assert!(ids.contains(&"u1"));
        assert!(ids.contains(&"p1"));
        assert!(ids.contains(&"shared"));
        // "shared" should appear exactly once, with Project source.
        let shared_entries: Vec<_> = listed.iter().filter(|(_, m)| m.id == "shared").collect();
        assert_eq!(shared_entries.len(), 1);
        assert_eq!(shared_entries[0].0, ShellSource::Project);
    }
}
