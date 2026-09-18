//! Manifest schema for GUI Shells.
//!
//! Every shell — built-in, user-installed, project-installed — ships a
//! manifest with these fields. The picker (Tier 2) reads them for
//! display; the bridge (Tier 3) reads `permissions` for gating.
//!
//! ## Two filenames, one schema
//!
//! The runtime registry discovers shells by `manifest.json`; the
//! authoring CLI (`shell new` / `check` / `pack` / `publish`) writes and
//! reads `shell.json`. Both parse [`ShellManifest`] — only the name
//! differed, which meant a folder scaffolded by `shell new` linted clean
//! and then never appeared in the picker, with nothing erroring
//! anywhere. [`find_manifest`] is the shared resolver: either name
//! works on either side.

use serde::{Deserialize, Serialize};

/// Accepted manifest filenames, in preference order.
pub const MANIFEST_NAMES: [&str; 2] = ["manifest.json", "shell.json"];

/// The manifest inside `dir`, whichever of [`MANIFEST_NAMES`] is
/// present. `None` when neither is.
pub fn find_manifest(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    MANIFEST_NAMES
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
}

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

/// Permission strings the authoring tools accept. `tools.invoke:<t>` and
/// `network.outbound:<host>` are prefix forms (any suffix allowed).
const ALLOWED_PERMISSION_PREFIXES: &[&str] = &[
    "agent.run",
    "team.read",
    "team.message",
    "team.control",
    "team.manage",
    "session.read",
    "session.list",
    // Sessions bridge (thclaws.sessions.*): write = new/rename/delete a
    // session. read/list gate the read side.
    "session.write",
    // Memory bridge (thclaws.memory.*): read = view core memory (MEMORY.md);
    // write = overwrite it.
    "memory.read",
    "memory.write",
    // Schedule bridge (thclaws.schedule.* + thclaws.heartbeat.*): read =
    // list; write = create/delete/toggle + heartbeat interval.
    "schedule.read",
    "schedule.write",
    // Skills bridge (thclaws.skills.*): read = list/view; write =
    // save/delete PROJECT skills.
    "skills.read",
    "skills.write",
    // KMS write side (thclaws.knowledge.*): create a KMS + ingest docs.
    // (Read side is the existing `kms.read`.)
    "kms.write",
    // Composer mode selector (thclaws.mode.*): switch auto/ask.
    "mode.write",
    // Profile panel (thclaws.profile.*): engine-side identity facts.
    "profile.read",
    "fs.shell-scoped",
    "tools.invoke:",
    "network.outbound:",
    // Model widget (thclaws.model.*): read = view current + list;
    // write = switch the active model. Absent ⇒ the bridge rejects the
    // call and <thc-model> renders nothing.
    "model.read",
    "model.write",
    // Deterministic data APIs (no LLM): thclaws.kms.* / thclaws.research.*
    // let a shell read the knowledge base + research-job registry directly
    // instead of prompting the model for it.
    "kms.read",
    "research.read",
    // BYOK from a shell's settings surface (thclaws.keys.set): store a
    // provider API key. Write-only — there is no getter, so a shell can
    // set a key it was given but never read back one already stored.
    "keys.write",
    // Connectors = MCP servers (thclaws.connectors.*): read = list them
    // with live status; write = add an HTTP connector / remove one.
    // stdio connectors are never addable from a shell (that names a
    // command the engine spawns) — desktop `/mcp add` only.
    "connectors.read",
    "connectors.write",
    // One-shot completion on the active model (thclaws.llm.complete):
    // a language-model step in service of the shell's own UI, not the
    // user's conversation. Spends credits, hence its own permission.
    "llm.complete",
    // Plugins (thclaws.plugins.*): read = list installed bundles with
    // what they contribute; write = enable/disable/uninstall. Installing
    // is deliberately absent — it fetches and unpacks code.
    "plugins.read",
    "plugins.write",
    // dev-plan/39 Tier 3: the shell hosts its own approve/deny widget
    // (thclaws.approvals.*) for mutating tool calls instead of the
    // full-screen system modal. Declarative signal for the marketplace
    // install screen; the functional route is driven per-call by the
    // bridge sending `preferInline` once an approval handler is registered.
    "approval.inline",
];

impl ShellManifest {
    /// Validate a manifest built from tool input before it's written to
    /// disk. Returns a human-readable error on the first problem so the
    /// model can fix and retry, rather than producing a shell that fails
    /// silently at load time.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !is_kebab_id(&self.id) {
            return Err(format!(
                "invalid id '{}': must be lowercase kebab-case (a-z, 0-9, '-'), no slashes",
                self.id
            ));
        }
        for (field, val) in [
            ("name", &self.name),
            ("version", &self.version),
            ("entry", &self.entry),
        ] {
            if val.trim().is_empty() {
                return Err(format!("'{field}' must not be empty"));
            }
        }
        if self.entry.contains('/') || self.entry.contains("..") {
            return Err(format!(
                "entry '{}' must be a bare filename inside the shell folder",
                self.entry
            ));
        }
        for p in &self.permissions {
            let ok = ALLOWED_PERMISSION_PREFIXES.iter().any(|prefix| {
                if prefix.ends_with(':') {
                    p.starts_with(prefix) && p.len() > prefix.len()
                } else {
                    p == prefix
                }
            });
            if !ok {
                return Err(format!(
                    "unknown permission '{p}'. Allowed: agent.run, session.read, session.list, \
                     fs.shell-scoped, tools.invoke:<tool>, network.outbound:<host>, model.read, \
                     model.write, kms.read, research.read, keys.write, connectors.read, \
                     connectors.write, plugins.read, plugins.write, llm.complete, \
                     approval.inline"
                ));
            }
        }
        Ok(())
    }
}

/// Lowercase kebab id: non-empty, no leading/trailing dash, only
/// `[a-z0-9-]`. Keeps ids safe as a single path segment.
fn is_kebab_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
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

    fn manifest_with(perms: &[&str]) -> ShellManifest {
        ShellManifest {
            id: "demo".into(),
            name: "Demo".into(),
            version: "0.1.0".into(),
            description: "d".into(),
            entry: "index.html".into(),
            icon: None,
            min_bridge_version: "1".into(),
            permissions: perms.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn validate_accepts_model_flags() {
        assert!(manifest_with(&["model.read", "model.write"])
            .validate()
            .is_ok());
    }

    #[test]
    fn validate_accepts_data_api_flags() {
        assert!(manifest_with(&["kms.read", "research.read"])
            .validate()
            .is_ok());
    }

    #[test]
    fn validate_rejects_unknown_permission() {
        assert!(manifest_with(&["model.delete"]).validate().is_err());
    }
}
