//! Manifest schema for GUI Shells.
//!
//! Every shell — built-in, user-installed, project-installed — ships a
//! `manifest.json` with these fields. The picker (Tier 2) reads them
//! for display; the bridge (Tier 3) reads `permissions` for gating.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub entry: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Bridge ABI version the shell was written against. Tier 1 ships
    /// version "1"; bumps happen if a method is ever removed (we plan
    /// to keep the surface additive — semver-minor for new methods,
    /// semver-major only for removals).
    #[serde(default = "default_bridge_version")]
    pub min_bridge_version: String,
    /// Coarse permission strings. Examples: `"agent.run"`,
    /// `"tools.invoke:image_gen"`, `"session.read"`,
    /// `"fs.shell-scoped"`, `"network.outbound:example.com"`. Tier 1
    /// stores but does not enforce; Tier 3 enforces.
    #[serde(default)]
    pub permissions: Vec<String>,
}

fn default_bridge_version() -> String {
    "1".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialises_minimal_manifest() {
        let json = r#"{
            "id": "session-explorer",
            "name": "Session Explorer",
            "version": "0.1.0",
            "description": "Tree-view past sessions.",
            "entry": "index.html",
            "permissions": ["agent.run", "session.read"]
        }"#;
        let m: ShellManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.id, "session-explorer");
        assert_eq!(m.min_bridge_version, "1");
        assert_eq!(m.permissions.len(), 2);
    }
}
