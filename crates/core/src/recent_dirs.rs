//! Recent working directories — persistent at
//! `~/.config/thclaws/recent_dirs.json`. Powers the working-directory
//! picker modal in both the desktop GUI and the `--serve` webapp; the
//! frontend renders the list on the new-session screen so users can
//! one-click back to a prior project.
//!
//! Lifted from `gui.rs` to an always-on module in M6.36 SERVE9d so the
//! WS transport's `get_cwd` / `set_cwd` IPC arms can read + write
//! through `crate::ipc::handle_ipc`.

/// How many recent directories to keep. Older entries are dropped when
/// the list exceeds this — three is enough for a personal workflow
/// without making the picker scroll.
pub const MAX_RECENT_DIRS: usize = 3;

/// Path to the persisted list. `None` when no home directory is
/// available (sandboxed CI / minimal containers).
pub fn recent_dirs_path() -> Option<std::path::PathBuf> {
    crate::util::home_dir().map(|h| h.join(".config/thclaws/recent_dirs.json"))
}

/// Load the persisted list, defaulting to empty on any missing /
/// unparseable / IO error path.
pub fn load_recent_dirs() -> Vec<String> {
    let Some(path) = recent_dirs_path() else {
        return vec![];
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return vec![];
    };
    serde_json::from_str::<Vec<String>>(&contents).unwrap_or_default()
}

/// Push `dir` to the front of the list (de-dup), truncate to
/// `MAX_RECENT_DIRS`, persist. No-op when home dir isn't resolvable.
pub fn save_recent_dir(dir: &str) {
    let Some(path) = recent_dirs_path() else {
        return;
    };
    let mut dirs = load_recent_dirs();
    dirs.retain(|d| d != dir);
    dirs.insert(0, dir.to_string());
    dirs.truncate(MAX_RECENT_DIRS);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        path,
        serde_json::to_string_pretty(&dirs).unwrap_or_default(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MAX_RECENT_DIRS is a UX constant — pin it so a future bump is
    /// intentional, not accidental.
    #[test]
    fn cap_is_three() {
        assert_eq!(MAX_RECENT_DIRS, 3);
    }
}
