//! Per-shell, per-session JSON key-value storage.
//!
//! Backed by a single JSON object at
//! `~/.config/thclaws/gui-shell/<shellId>/state/<sessionId>.json`.
//! Always user-level — a shell installed at project level still
//! stores its state per-user (state is the user's, not the repo's).
//! This means uninstalling a project-installed shell does NOT delete
//! the user's accumulated state, and reinstalling preserves it.
//!
//! Atomic per-write: load → mutate → write-temp → rename. Simple
//! enough for Tier 2's scale (KB of state per shell per session).
//! If we grow into MB-scale per-shell state later, swap for SQLite.

use crate::error::{Error, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Validate (shell, session) ids — shared by both the user-level
/// and override-rooted path resolvers below.
fn validate_ids(shell_id: &str, session_id: &str) -> Result<()> {
    if shell_id.is_empty()
        || session_id.is_empty()
        || shell_id.contains('/')
        || shell_id.contains('\\')
        || shell_id.contains("..")
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id.contains("..")
    {
        return Err(Error::Tool(format!(
            "gui-shell: invalid storage id (shell='{shell_id}', session='{session_id}')"
        )));
    }
    Ok(())
}

/// Resolve the on-disk file backing a (shell, session) storage map.
/// Single-tenant default — user-level path under `~/.config/`.
/// Creates parent directories on demand at write time, not here.
pub fn storage_path(shell_id: &str, session_id: &str) -> Result<PathBuf> {
    let home = crate::util::home_dir()
        .ok_or_else(|| Error::Config("HOME not set; cannot resolve shell storage path".into()))?;
    validate_ids(shell_id, session_id)?;
    Ok(home
        .join(".config")
        .join("thclaws")
        .join("gui-shell")
        .join(shell_id)
        .join("state")
        .join(format!("{session_id}.json")))
}

/// Same as [`storage_path`] but rooted at `override_root` —
/// `<override_root>/<shell_id>/<session_id>.json`. Used by
/// multi-tenant `--serve` so two users hosted in the same pod write
/// to separate `<project>/.thclaws/users/<user_id>/storage/` trees.
pub fn storage_path_in(override_root: &Path, shell_id: &str, session_id: &str) -> Result<PathBuf> {
    validate_ids(shell_id, session_id)?;
    Ok(override_root
        .join(shell_id)
        .join(format!("{session_id}.json")))
}

/// Read the whole storage map for this (shell, session). Returns an
/// empty map if the file doesn't exist yet — first-touch is implicit.
pub fn load_all(shell_id: &str, session_id: &str) -> Result<BTreeMap<String, Value>> {
    let path = storage_path(shell_id, session_id)?;
    load_at(&path)
}

/// Like [`load_all`] but reads from an explicit override root via
/// [`storage_path_in`]. Multi-tenant variant — keeps the disk-shape
/// validation identical to single-tenant.
pub fn load_all_in(
    override_root: &Path,
    shell_id: &str,
    session_id: &str,
) -> Result<BTreeMap<String, Value>> {
    let path = storage_path_in(override_root, shell_id, session_id)?;
    load_at(&path)
}

fn load_at(path: &Path) -> Result<BTreeMap<String, Value>> {
    match std::fs::read_to_string(path) {
        Ok(body) => serde_json::from_str(&body).map_err(|e| {
            Error::Tool(format!(
                "gui-shell: storage file '{}' corrupt: {e}",
                path.display()
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(Error::Tool(format!(
            "gui-shell: cannot read storage '{}': {e}",
            path.display()
        ))),
    }
}

/// Fetch a single key. Returns `Value::Null` for missing keys so the
/// bridge can distinguish "no such key" from "key explicitly set to
/// null" at the API surface if it ever needs to.
pub fn get(shell_id: &str, session_id: &str, key: &str) -> Result<Value> {
    let map = load_all(shell_id, session_id)?;
    Ok(map.get(key).cloned().unwrap_or(Value::Null))
}

/// Override-rooted [`get`] — reads from `<override_root>/<shell_id>/<session_id>.json`.
pub fn get_in(override_root: &Path, shell_id: &str, session_id: &str, key: &str) -> Result<Value> {
    let map = load_all_in(override_root, shell_id, session_id)?;
    Ok(map.get(key).cloned().unwrap_or(Value::Null))
}

/// Set a single key. Loads the existing map, replaces the key,
/// writes the whole map back atomically (temp + rename).
pub fn set(shell_id: &str, session_id: &str, key: &str, value: Value) -> Result<()> {
    let path = storage_path(shell_id, session_id)?;
    let mut map = load_all(shell_id, session_id)?;
    write_at(&path, &mut map, key, value)
}

/// Override-rooted [`set`] — writes to `<override_root>/<shell_id>/<session_id>.json`.
pub fn set_in(
    override_root: &Path,
    shell_id: &str,
    session_id: &str,
    key: &str,
    value: Value,
) -> Result<()> {
    let path = storage_path_in(override_root, shell_id, session_id)?;
    let mut map = load_all_in(override_root, shell_id, session_id)?;
    write_at(&path, &mut map, key, value)
}

fn write_at(path: &Path, map: &mut BTreeMap<String, Value>, key: &str, value: Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::Tool(format!(
                "gui-shell: cannot create storage dir '{}': {e}",
                parent.display()
            ))
        })?;
    }
    map.insert(key.to_string(), value);
    let body = serde_json::to_string_pretty(map)
        .map_err(|e| Error::Tool(format!("gui-shell: serialize storage: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| {
        Error::Tool(format!(
            "gui-shell: cannot write storage temp '{}': {e}",
            tmp.display()
        ))
    })?;
    std::fs::rename(&tmp, path).map_err(|e| {
        Error::Tool(format!(
            "gui-shell: cannot rename storage temp '{}' -> '{}': {e}",
            tmp.display(),
            path.display()
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `storage_path` rejects ids containing path-traversal characters
    /// before they ever hit the filesystem.
    #[test]
    fn rejects_path_traversal_in_ids() {
        assert!(storage_path("../etc/passwd", "sess").is_err());
        assert!(storage_path("good-id", "..").is_err());
        assert!(storage_path("a/b", "sess").is_err());
        assert!(storage_path("good", "sess/with/slash").is_err());
        assert!(storage_path("", "sess").is_err());
        assert!(storage_path("good", "").is_err());
    }

    #[test]
    fn missing_file_returns_null() {
        // Use a definitely-nonexistent id pair.
        let v = get("__test-missing__", "__never-existed__", "anything").unwrap();
        assert!(v.is_null());
    }

    /// Override-rooted variant — files land under the supplied root
    /// and DO NOT bleed into the user-level `~/.config/` location.
    #[test]
    fn set_in_then_get_in_roundtrips_under_override_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let shell_id = "chatbot";
        let session_id = "sess-1";
        set_in(root, shell_id, session_id, "k", serde_json::json!("v")).unwrap();
        let path = storage_path_in(root, shell_id, session_id).unwrap();
        assert!(path.starts_with(root), "must land under override root");
        assert!(path.exists(), "set_in must create the file");
        assert_eq!(
            get_in(root, shell_id, session_id, "k").unwrap(),
            serde_json::json!("v")
        );
        // Missing key still null-returns.
        assert!(get_in(root, shell_id, session_id, "missing")
            .unwrap()
            .is_null());
    }

    /// Two override roots are fully isolated — alice's writes don't
    /// surface for bob, even with the same (shell_id, session_id).
    /// This is the multi-tenant isolation guarantee at the storage layer.
    #[test]
    fn override_roots_isolate_users() {
        let dir = tempfile::tempdir().unwrap();
        let alice = dir.path().join("alice/storage");
        let bob = dir.path().join("bob/storage");
        set_in(
            &alice,
            "chatbot",
            "sess",
            "secret",
            serde_json::json!("alice"),
        )
        .unwrap();
        set_in(&bob, "chatbot", "sess", "secret", serde_json::json!("bob")).unwrap();
        assert_eq!(
            get_in(&alice, "chatbot", "sess", "secret").unwrap(),
            serde_json::json!("alice")
        );
        assert_eq!(
            get_in(&bob, "chatbot", "sess", "secret").unwrap(),
            serde_json::json!("bob")
        );
    }

    #[test]
    fn storage_path_in_still_rejects_traversal_ids() {
        let dir = tempfile::tempdir().unwrap();
        assert!(storage_path_in(dir.path(), "../etc", "sess").is_err());
        assert!(storage_path_in(dir.path(), "good", "..").is_err());
        assert!(storage_path_in(dir.path(), "", "sess").is_err());
    }

    #[test]
    fn set_then_get_roundtrips() {
        let shell_id = "__test-rt__";
        let session_id = "__sess-rt__";
        // Clean up any prior state.
        if let Ok(p) = storage_path(shell_id, session_id) {
            let _ = std::fs::remove_file(&p);
        }
        set(shell_id, session_id, "answer", serde_json::json!(42)).unwrap();
        let v = get(shell_id, session_id, "answer").unwrap();
        assert_eq!(v, serde_json::json!(42));
        // Set a second key; first key should still be readable.
        set(shell_id, session_id, "greeting", serde_json::json!("hello")).unwrap();
        assert_eq!(
            get(shell_id, session_id, "answer").unwrap(),
            serde_json::json!(42)
        );
        assert_eq!(
            get(shell_id, session_id, "greeting").unwrap(),
            serde_json::json!("hello")
        );
        // Cleanup
        if let Ok(p) = storage_path(shell_id, session_id) {
            let _ = std::fs::remove_file(&p);
        }
    }
}
