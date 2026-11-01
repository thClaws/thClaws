//! On-disk binding for the phone-home channel at `./.thclaws/state/phone-home.json`
//! (dev-plan/44 Tier 1).
//!
//! Unlike LINE/Messenger — whose external identity is a messaging-platform
//! user id paired via an 8-char code — the phone-home channel binds the
//! local engine to a **thClaws.cloud account**. The binding JWT is minted
//! from the user's cloud login (CLI token); this file just persists it +
//! the relay override so subsequent launches auto-reconnect.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::bridge::BridgeConfig;

/// Default relay for the phone-home channel — the same relay that serves
/// LINE/Messenger (it now carries the `/ph/*` routes too). Override in dev
/// via `THCLAWS_PHONE_HOME_SERVER`.
pub const DEFAULT_SERVER_URL: &str = "https://line.thclaws.ai";

/// Relative path under the project dir (runtime state, workspace v2+).
const CONFIG_REL: &str = ".thclaws/state/phone-home.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhoneHomeConfig {
    /// Binding JWT minted from the cloud login. Sent as `?token=` on the
    /// WS and `Bearer` on replies.
    pub binding_token: String,
    /// Explicit relay base override (else env → default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
    /// Human label for this machine, surfaced in the cloud dashboard's
    /// device list. Cosmetic — the binding is keyed by the JWT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_label: Option<String>,
}

impl PhoneHomeConfig {
    /// Path to the per-project binding file under `dir`.
    pub fn path_in(dir: &std::path::Path) -> PathBuf {
        dir.join(CONFIG_REL)
    }

    /// Load the binding from `dir/.thclaws/phone-home.json`, if present.
    /// Returns `Ok(None)` when the file doesn't exist (not yet paired).
    pub fn load_in(dir: &std::path::Path) -> std::io::Result<Option<Self>> {
        let path = Self::path_in(dir);
        match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<Self>(&raw) {
                Ok(cfg) => Ok(Some(cfg)),
                // A malformed file is treated as "not paired" rather than a
                // hard error, mirroring the other bridge configs.
                Err(_) => Ok(None),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Persist the binding to `dir/.thclaws/phone-home.json`.
    pub fn save_in(&self, dir: &std::path::Path) -> std::io::Result<()> {
        let path = Self::path_in(dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Convenience: load from the current dir's `.thclaws/phone-home.json`.
    /// Used by the worker's boot auto-reconnect.
    pub fn load() -> std::io::Result<Option<Self>> {
        let cwd = std::env::current_dir()?;
        Ok(Self::load_in(&cwd)?.map(|c| c.gated(crate::bridge::folder_trusted(&cwd))))
    }

    /// A relay override only counts in a trusted folder, so a cloned
    /// repo can't point the tunnel at its own relay.
    pub fn gated(mut self, trusted: bool) -> Self {
        if !trusted {
            self.server_url = None;
        }
        self
    }

    /// Convenience: save under the current dir.
    pub fn save(&self) -> std::io::Result<()> {
        self.save_in(&std::env::current_dir()?)
    }

    /// Delete the current dir's binding file so the next boot doesn't
    /// auto-reconnect. Absent file is not an error.
    pub fn delete() -> std::io::Result<()> {
        let path = Self::path_in(&std::env::current_dir()?);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl BridgeConfig for PhoneHomeConfig {
    fn binding_token(&self) -> &str {
        &self.binding_token
    }
    fn server_url_override(&self) -> Option<&str> {
        self.server_url.as_deref()
    }
    fn server_env_var(&self) -> &'static str {
        "THCLAWS_PHONE_HOME_SERVER"
    }
    fn default_server(&self) -> &'static str {
        DEFAULT_SERVER_URL
    }
    fn route_prefix(&self) -> &'static str {
        "/ph"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::BridgeConfig;

    fn cfg() -> PhoneHomeConfig {
        PhoneHomeConfig {
            binding_token: "tok".into(),
            server_url: None,
            machine_label: Some("jimmy-macbook".into()),
        }
    }

    #[test]
    fn ph_routes_namespace_under_ph_prefix() {
        let c = cfg();
        assert_eq!(c.ws_url(), "wss://line.thclaws.ai/ws?token=tok");
        assert_eq!(c.reply_url("r1"), "https://line.thclaws.ai/ph/reply/r1");
        assert_eq!(c.push_url(), "https://line.thclaws.ai/ph/push");
        // unpair stays un-prefixed across every channel.
        assert_eq!(c.unpair_url(), "https://line.thclaws.ai/unpair");
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join("thclaws-ph-test-rt");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(PhoneHomeConfig::load_in(&dir).unwrap().is_none());
        cfg().save_in(&dir).unwrap();
        let loaded = PhoneHomeConfig::load_in(&dir).unwrap().unwrap();
        assert_eq!(loaded.binding_token, "tok");
        assert_eq!(loaded.machine_label.as_deref(), Some("jimmy-macbook"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
