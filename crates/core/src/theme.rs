//! Theme persistence — `light` / `dark` / `system` mode stored at
//! `~/.config/thclaws/theme.json`. Lifted from `gui.rs` to an
//! always-on module in M6.36 SERVE9c so the WS transport's
//! `theme_get` / `theme_set` IPC arms can call into it from the
//! transport-agnostic `crate::ipc::handle_ipc`.

/// Path to the persisted theme file. `None` when no home directory is
/// available (sandboxed CI / minimal containers).
pub fn theme_path() -> Option<std::path::PathBuf> {
    crate::util::home_dir().map(|h| h.join(".config/thclaws/theme.json"))
}

/// Coerce a mode string to one of the three legal values. Any unknown
/// input falls back to `"system"` (auto-detect via the OS).
pub fn normalize_theme(raw: &str) -> &'static str {
    match raw {
        "light" => "light",
        "dark" => "dark",
        _ => "system",
    }
}

/// Read the persisted mode, defaulting to `"system"` when the file is
/// missing / unreadable / malformed.
pub fn load_theme() -> String {
    let Some(path) = theme_path() else {
        return "system".to_string();
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return "system".to_string();
    };
    let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap_or_default();
    let mode = parsed
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("system");
    normalize_theme(mode).to_string()
}

/// Persist a normalized mode. No-op when home dir isn't resolvable.
pub fn save_theme(mode: &str) {
    let Some(path) = theme_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let payload = serde_json::json!({ "mode": normalize_theme(mode) });
    let _ = std::fs::write(
        path,
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_theme_accepts_known_values() {
        assert_eq!(normalize_theme("light"), "light");
        assert_eq!(normalize_theme("dark"), "dark");
        assert_eq!(normalize_theme("system"), "system");
    }

    #[test]
    fn normalize_theme_falls_back_to_system_for_unknown() {
        assert_eq!(normalize_theme("solarized"), "system");
        assert_eq!(normalize_theme(""), "system");
        assert_eq!(normalize_theme("LIGHT"), "system"); // case-sensitive
    }
}
