//! On-disk binding config at `./.thclaws/messenger.json` (dev-plan/33
//! Tier 2 — project-scoped, mirrors the Telegram + LINE per-project
//! move).
//!
//! Same model as the LINE bridge (`crate::line::config`): the
//! high-value secrets — the Page Access Token + App Secret — live on
//! the relay (k3s Secret), never here. The desktop only stores the
//! binding JWT the relay's `POST /pair` hands back plus the relay URL,
//! then reconnects the WebSocket on each launch — per project.
//!
//! Tier 1 (dev-plan/31) shares the LINE relay deployment, so the
//! default server URL is the same `line.thclaws.ai` host with a
//! `/messenger/webhook` ingest path. The rename to a neutral gateway
//! host is dev-plan/31 open-question #1 (deferred to Tier 3).
//!
//! Legacy `~/.config/thclaws/messenger.json` is consulted as a
//! fallback only when env var `THCLAWS_MESSENGER_USER_CONFIG=1` is
//! set, so pre-Tier 2 installs keep working until the user migrates.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bridge::BridgeConfig;

/// Default relay when `server_url` isn't set. Shared with the LINE
/// relay in Tier 1; override in dev via `THCLAWS_MESSENGER_SERVER`.
pub const DEFAULT_SERVER_URL: &str = "https://line.thclaws.ai";

/// Env opt-in for the legacy `~/.config/thclaws/messenger.json`
/// fallback path. Without this, only `./.thclaws/messenger.json` is
/// consulted — each project owns its own Messenger binding.
pub const USER_FALLBACK_ENV: &str = "THCLAWS_MESSENGER_USER_CONFIG";

fn user_fallback_enabled() -> bool {
    std::env::var(USER_FALLBACK_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        .unwrap_or(false)
}

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
    /// Project-scoped path: `./.thclaws/messenger.json` — resolved
    /// against the current working directory at call time. dev-plan/33
    /// Tier 2 moved this off the user-level path so each project owns
    /// its own Messenger binding.
    pub fn path() -> Result<PathBuf, MessengerConfigError> {
        let cwd = std::env::current_dir().map_err(|source| MessengerConfigError::Io {
            path: PathBuf::from("."),
            source,
        })?;
        Ok(cwd.join(".thclaws").join("messenger.json"))
    }

    /// Legacy user-level path (`~/.config/thclaws/messenger.json`).
    /// Only consulted as a fallback when
    /// `THCLAWS_MESSENGER_USER_CONFIG=1` is set — pre-Tier 2 installs
    /// had their binding here.
    pub fn legacy_user_path() -> Result<PathBuf, MessengerConfigError> {
        let home = crate::util::home_dir().ok_or(MessengerConfigError::NoHome)?;
        Ok(home
            .join(".config")
            .join(crate::profile::app_dir_name())
            .join("messenger.json"))
    }

    /// Read from disk. Project path first; legacy user path as
    /// opt-in fallback. `Ok(None)` when both are absent.
    pub fn load() -> Result<Option<Self>, MessengerConfigError> {
        let cwd = std::env::current_dir().map_err(|source| MessengerConfigError::Io {
            path: PathBuf::from("."),
            source,
        })?;
        Self::load_in(&cwd, crate::bridge::folder_trusted(&cwd))
    }

    /// [`load`](Self::load) against `dir`. A relay override in the
    /// project file of a folder that isn't `trusted` is dropped, so a
    /// cloned repo can't point the bridge at its own relay.
    pub fn load_in(dir: &Path, trusted: bool) -> Result<Option<Self>, MessengerConfigError> {
        let project_path = dir.join(".thclaws").join("messenger.json");
        match std::fs::read_to_string(&project_path) {
            Ok(body) => {
                return serde_json::from_str::<Self>(&body)
                    .map(|mut c| {
                        if !trusted {
                            c.server_url = None;
                        }
                        Some(c)
                    })
                    .map_err(|source| MessengerConfigError::Json {
                        path: project_path,
                        source,
                    });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(MessengerConfigError::Io {
                    path: project_path,
                    source,
                });
            }
        }
        if !user_fallback_enabled() {
            return Ok(None);
        }
        let user_path = Self::legacy_user_path()?;
        match std::fs::read_to_string(&user_path) {
            Ok(body) => {
                serde_json::from_str(&body)
                    .map(Some)
                    .map_err(|source| MessengerConfigError::Json {
                        path: user_path,
                        source,
                    })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(MessengerConfigError::Io {
                path: user_path,
                source,
            }),
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

    // `resolved_server_url` / `ws_url` / `reply_url` / `push_url` /
    // `unpair_url` now come from the shared `BridgeConfig` trait
    // (dev-plan/44 Tier 0). Messenger's `route_prefix` is `/messenger`,
    // so reply/push namespace under it; see the impl below.
}

impl BridgeConfig for MessengerConfig {
    fn binding_token(&self) -> &str {
        &self.binding_token
    }
    fn server_url_override(&self) -> Option<&str> {
        self.server_url.as_deref()
    }
    fn server_env_var(&self) -> &'static str {
        "THCLAWS_MESSENGER_SERVER"
    }
    fn default_server(&self) -> &'static str {
        DEFAULT_SERVER_URL
    }
    fn route_prefix(&self) -> &'static str {
        "/messenger"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::BridgeConfig;

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
