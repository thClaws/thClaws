//! On-disk binding config at `~/.config/thclaws/messenger.json`.
//!
//! Same model as the LINE bridge (`crate::line::config`): the
//! high-value secrets — the Page Access Token + App Secret — live on
//! the relay (k3s Secret), never here. The desktop only stores the
//! binding JWT the relay's `POST /pair` hands back plus the relay URL,
//! then reconnects the WebSocket on each launch.
//!
//! Tier 1 (dev-plan/31) shares the LINE relay deployment, so the
//! default server URL is the same `line.thclaws.ai` host with a
//! `/messenger/webhook` ingest path. The rename to a neutral gateway
//! host is dev-plan/31 open-question #1 (deferred to Tier 3).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default relay when `server_url` isn't set. Shared with the LINE
/// relay in Tier 1; override in dev via `THCLAWS_MESSENGER_SERVER`.
pub const DEFAULT_SERVER_URL: &str = "https://line.thclaws.ai";

#[derive(Debug, thiserror::Error)]
pub enum MessengerConfigError {
    #[error("home directory not resolvable on this platform")]
    NoHome,
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("json error in {path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessengerConfig {
    /// HS256 JWT issued by the relay's `POST /pair`.
    pub binding_token: String,
    /// Override URL for the relay. Falls back to
    /// `$THCLAWS_MESSENGER_SERVER` or `DEFAULT_SERVER_URL`.
    #[serde(default)]
    pub server_url: Option<String>,
    /// Facebook Page name cached at pair time, for the GUI status
    /// pill label. `None` when the relay couldn't fetch it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_name: Option<String>,
    /// Page id this binding is scoped to (the recipient id Meta puts
    /// on inbound webhook events). Cached for display + sanity checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_id: Option<String>,
}

impl MessengerConfig {
    /// Canonical on-disk path. `None` only when no home directory can
    /// be resolved (headless/CI environments).
    pub fn path() -> Result<PathBuf, MessengerConfigError> {
        let home = crate::util::home_dir().ok_or(MessengerConfigError::NoHome)?;
        Ok(home.join(".config").join("thclaws").join("messenger.json"))
    }

    /// Read from disk. `Ok(None)` when the file is absent — the
    /// "Messenger bridge not configured" default for a fresh install.
    pub fn load() -> Result<Option<Self>, MessengerConfigError> {
        let path = Self::path()?;
        match std::fs::read_to_string(&path) {
            Ok(body) => {
                serde_json::from_str(&body)
                    .map(Some)
                    .map_err(|source| MessengerConfigError::Json {
                        path: path.clone(),
                        source,
                    })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(MessengerConfigError::Io { path, source }),
        }
    }

    /// Persist atomically — write a sibling `.tmp` then rename, so a
    /// crash mid-write can't leave a half-written file.
    pub fn save(&self) -> Result<(), MessengerConfigError> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| MessengerConfigError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let body =
            serde_json::to_string_pretty(self).map_err(|source| MessengerConfigError::Json {
                path: path.clone(),
                source,
            })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(|source| MessengerConfigError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, &path).map_err(|source| MessengerConfigError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(())
    }

    /// Delete the file (GUI "Disconnect"). Idempotent.
    pub fn delete() -> Result<(), MessengerConfigError> {
        let path = Self::path()?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(MessengerConfigError::Io { path, source }),
        }
    }

    /// Resolve the relay URL. Precedence: explicit `server_url` →
    /// `THCLAWS_MESSENGER_SERVER` env → `DEFAULT_SERVER_URL`.
    pub fn resolved_server_url(&self) -> String {
        if let Some(url) = self.server_url.as_deref() {
            return url.trim_end_matches('/').to_string();
        }
        if let Ok(url) = std::env::var("THCLAWS_MESSENGER_SERVER") {
            if !url.trim().is_empty() {
                return url.trim_end_matches('/').to_string();
            }
        }
        DEFAULT_SERVER_URL.to_string()
    }

    /// Build the `wss://…/ws?token=<jwt>` URL the WS client opens.
    pub fn ws_url(&self) -> String {
        let base = self.resolved_server_url();
        let scheme = if base.starts_with("http://") {
            "ws://"
        } else {
            "wss://"
        };
        let host = base
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        format!(
            "{scheme}{host}/ws?token={}",
            urlencoding::encode(&self.binding_token)
        )
    }

    /// Build the absolute `POST /messenger/reply/<request_id>` URL. The
    /// relay resolves the recipient PSID from the inbound event keyed
    /// by `request_id` and calls the Graph API Send API.
    pub fn reply_url(&self, request_id: &str) -> String {
        format!(
            "{}/messenger/reply/{}",
            self.resolved_server_url(),
            urlencoding::encode(request_id)
        )
    }

    /// Build the absolute `POST /messenger/push` URL — unsolicited
    /// messages (approval prompts, timeout notices) that have no
    /// inbound event to reply to. The relay targets the Page's most-
    /// recent inbound PSID (Tier-1 approximation).
    pub fn push_url(&self) -> String {
        format!("{}/messenger/push", self.resolved_server_url())
    }

    /// Build the absolute `POST /unpair` URL.
    pub fn unpair_url(&self) -> String {
        format!("{}/unpair", self.resolved_server_url())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_url_precedence_config_over_env_over_default() {
        let mut c = MessengerConfig {
            binding_token: "t".into(),
            server_url: Some("https://custom.example/".into()),
            ..Default::default()
        };
        assert_eq!(c.resolved_server_url(), "https://custom.example");

        std::env::set_var("THCLAWS_MESSENGER_SERVER", "https://env.example/");
        c.server_url = None;
        assert_eq!(c.resolved_server_url(), "https://env.example");

        std::env::remove_var("THCLAWS_MESSENGER_SERVER");
        assert_eq!(c.resolved_server_url(), DEFAULT_SERVER_URL);
    }

    #[test]
    fn ws_url_uses_wss_for_https() {
        let c = MessengerConfig {
            binding_token: "abc".into(),
            server_url: Some("https://line.thclaws.ai".into()),
            ..Default::default()
        };
        assert_eq!(c.ws_url(), "wss://line.thclaws.ai/ws?token=abc");
    }

    #[test]
    fn ws_url_uses_ws_for_http() {
        let c = MessengerConfig {
            binding_token: "abc".into(),
            server_url: Some("http://localhost:8080".into()),
            ..Default::default()
        };
        assert_eq!(c.ws_url(), "ws://localhost:8080/ws?token=abc");
    }

    #[test]
    fn reply_url_escapes_request_id() {
        let c = MessengerConfig {
            binding_token: "t".into(),
            server_url: Some("https://line.thclaws.ai".into()),
            ..Default::default()
        };
        assert_eq!(
            c.reply_url("a b/c"),
            "https://line.thclaws.ai/messenger/reply/a%20b%2Fc"
        );
    }
}
