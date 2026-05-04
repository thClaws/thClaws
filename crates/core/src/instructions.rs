//! Resolve the path to the user-editable `AGENTS.md` instructions file.
//! Two scopes:
//!
//! - `"global"` → `~/.config/thclaws/AGENTS.md` (applies to every project)
//! - anything else (typically `"folder"`) → `<cwd>/AGENTS.md` (project-local)
//!
//! Lifted from `gui.rs` to an always-on module in M6.36 SERVE9d so the
//! WS transport's `instructions_get` / `instructions_save` IPC arms can
//! call it from `crate::ipc::handle_ipc`.

/// Resolve the AGENTS.md path for `scope`. `None` when global scope is
/// requested but no home dir is available (sandboxed CI). Folder scope
/// always resolves (`current_dir().ok()` is the only failure path).
pub fn instructions_path(scope: &str) -> Option<std::path::PathBuf> {
    match scope {
        "global" => crate::util::home_dir().map(|h| h.join(".config/thclaws/AGENTS.md")),
        _ => std::env::current_dir().ok().map(|d| d.join("AGENTS.md")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_scope_resolves_to_cwd_agents_md() {
        let p = instructions_path("folder");
        assert!(p.is_some());
        let p = p.unwrap();
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("AGENTS.md"));
    }

    #[test]
    fn unknown_scope_falls_back_to_folder() {
        // Anything not "global" lands in cwd — preserves prior behavior
        // where the frontend's "scope" string was permissive.
        let p = instructions_path("anything-else");
        assert!(p.is_some());
        assert_eq!(
            p.unwrap().file_name().and_then(|s| s.to_str()),
            Some("AGENTS.md")
        );
    }
}
