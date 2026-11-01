//! On-disk binding config at `./.thclaws/line.json` (dev-plan/33
//! Tier 2 — project-scoped, mirrors the Telegram per-project move).
//!
//! Written once when the user redeems a pairing code via the GUI
//! Line Connect modal (Phase 1.3) or the `--line-pair <code>` CLI
//! flag. Subsequent thClaws launches in the same project read it to
//! find the binding JWT and the relay URL, then auto-reconnect the
//! WebSocket.
//!
//! Schema is intentionally minimal — anything else (machine
//! label, cwd, last-active timestamp) lives inside the JWT's
//! claims, which the server is the source of truth for.
//!
//! Legacy `~/.config/thclaws/line.json` is consulted as a fallback
//! only when the env var `THCLAWS_LINE_USER_CONFIG=1` is set, so
//! pre-Tier 2 installs keep working until the user migrates by
//! moving the file into a project's `.thclaws/`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bridge::BridgeConfig;

/// Default server when `server_url` isn't set explicitly. Override
/// in dev via `THCLAWS_LINE_SERVER`.
pub const DEFAULT_SERVER_URL: &str = "https://line.thclaws.ai";

/// Env opt-in for the legacy `~/.config/thclaws/line.json` fallback
/// path. Without this, only `./.thclaws/line.json` is consulted —
/// each project owns its own LINE binding.
pub const USER_FALLBACK_ENV: &str = "THCLAWS_LINE_USER_CONFIG";

fn user_fallback_enabled() -> bool {
    std::env::var(USER_FALLBACK_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        .unwrap_or(false)
}

#[derive(Debug, thiserror::Error)]
pub enum LineConfigError {
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
pub struct LineConfig {
    /// HS256 JWT issued by the relay's `POST /pair`.
    pub binding_token: String,
    /// Override URL for the relay. Falls back to
    /// `$THCLAWS_LINE_SERVER` or `DEFAULT_SERVER_URL`.
    #[serde(default)]
    pub server_url: Option<String>,
    /// LINE display name cached at pair time. `None` when the
    /// relay couldn't fetch it (rate limit / older relay version).
    /// Surfaced to the GUI via the `line_status` broadcast for the
    /// sidebar pill label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

impl LineConfig {
    /// Project-scoped path: `./.thclaws/line.json` — resolved against
    /// the current working directory at call time. dev-plan/33 Tier 2
    /// moved this off the user-level path so each project owns its own
    /// LINE binding.
    pub fn path() -> Result<PathBuf, LineConfigError> {
        let cwd = std::env::current_dir().map_err(|source| LineConfigError::Io {
            path: PathBuf::from("."),
            source,
        })?;
        Ok(cwd.join(".thclaws").join("line.json"))
    }

    /// Legacy user-level path (`~/.config/thclaws/line.json`). Only
    /// consulted as a fallback when `THCLAWS_LINE_USER_CONFIG=1` is
    /// set — pre-Tier 2 installs had their binding here.
    pub fn legacy_user_path() -> Result<PathBuf, LineConfigError> {
        let home = crate::util::home_dir().ok_or(LineConfigError::NoHome)?;
        Ok(home
            .join(".config")
            .join(crate::profile::app_dir_name())
            .join("line.json"))
    }

    /// Read from disk. Project path first; legacy user path as
    /// opt-in fallback. `Ok(None)` when both are absent (the
    /// default state for a fresh install).
    pub fn load() -> Result<Option<Self>, LineConfigError> {
        let cwd = std::env::current_dir().map_err(|source| LineConfigError::Io {
            path: PathBuf::from("."),
            source,
        })?;
        Self::load_in(&cwd, crate::bridge::folder_trusted(&cwd))
    }

    /// [`load`](Self::load) against `dir`. A relay override in the
    /// project file of a folder that isn't `trusted` is dropped, so a
    /// cloned repo can't point the bridge at its own relay.
    pub fn load_in(dir: &Path, trusted: bool) -> Result<Option<Self>, LineConfigError> {
        let project_path = dir.join(".thclaws").join("line.json");
        match std::fs::read_to_string(&project_path) {
            Ok(body) => {
                return serde_json::from_str::<Self>(&body)
                    .map(|mut c| {
                        if !trusted {
                            c.server_url = None;
                        }
                        Some(c)
                    })
                    .map_err(|source| LineConfigError::Json {
                        path: project_path,
                        source,
                    });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LineConfigError::Io {
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
                    .map_err(|source| LineConfigError::Json {
                        path: user_path,
                        source,
                    })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(LineConfigError::Io {
                path: user_path,
                source,
            }),
        }
    }

    /// Persist atomically — write to a sibling `.tmp` first then
    /// rename, so a crash mid-write can't leave a half-written
    /// file that the next launch would fail to parse.
    pub fn save(&self) -> Result<(), LineConfigError> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LineConfigError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let body = serde_json::to_string_pretty(self).map_err(|source| LineConfigError::Json {
            path: path.clone(),
            source,
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(|source| LineConfigError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, &path).map_err(|source| LineConfigError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(())
    }

    /// Delete the file (used by `Line Disconnect` in the GUI and
    /// the `/disconnect` LINE command). Idempotent — missing file
    /// is treated as success.
    pub fn delete() -> Result<(), LineConfigError> {
        let path = Self::path()?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(LineConfigError::Io { path, source }),
        }
    }

    // `resolved_server_url` / `ws_url` / `reply_url` / `push_url` /
    // `unpair_url` now come from the shared `BridgeConfig` trait
    // (dev-plan/44 Tier 0) — see the `impl BridgeConfig for LineConfig`
    // below. LINE-only routes (chat-bridge) stay here.

    /// Build the absolute `POST /chat-bridge/event` URL. Used to
    /// fan out per-turn `ViewEvent`s (assistant text deltas, tool
    /// call indicators, turn-done) to the plan-10 browser chat
    /// when it's connected.
    pub fn chat_bridge_event_url(&self) -> String {
        format!("{}/chat-bridge/event", self.resolved_server_url())
    }

    /// `GET /chat-bridge/has-browser` — server tells us whether
    /// any browser chat session is currently connected for our
    /// `sub`. Used by `LineApprover` to choose between browser
    /// modal and LINE OA push.
    pub fn chat_bridge_has_browser_url(&self) -> String {
        format!("{}/chat-bridge/has-browser", self.resolved_server_url())
    }
}

impl BridgeConfig for LineConfig {
    fn binding_token(&self) -> &str {
        &self.binding_token
    }
    fn server_url_override(&self) -> Option<&str> {
        self.server_url.as_deref()
    }
    fn server_env_var(&self) -> &'static str {
        "THCLAWS_LINE_SERVER"
    }
    fn default_server(&self) -> &'static str {
        DEFAULT_SERVER_URL
    }
    // route_prefix defaults to "" — LINE's outbound routes are
    // `/reply`, `/push` (no channel namespace).
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::BridgeConfig;

    #[test]
    fn server_url_precedence_config_over_env_over_default() {
        // Config wins
        let mut c = LineConfig {
            binding_token: "t".into(),
            server_url: Some("https://custom.example/".into()),
            ..Default::default()
        };
        assert_eq!(c.resolved_server_url(), "https://custom.example");

        // Env wins over default
        std::env::set_var("THCLAWS_LINE_SERVER", "https://env.example/");
        c.server_url = None;
        assert_eq!(c.resolved_server_url(), "https://env.example");

        std::env::remove_var("THCLAWS_LINE_SERVER");
        assert_eq!(c.resolved_server_url(), DEFAULT_SERVER_URL);
    }

    #[test]
    fn ws_url_uses_wss_for_https() {
        let c = LineConfig {
            binding_token: "abc".into(),
            server_url: Some("https://line.thclaws.ai".into()),
            ..Default::default()
        };
        assert_eq!(c.ws_url(), "wss://line.thclaws.ai/ws?token=abc");
    }

    #[test]
    fn ws_url_uses_ws_for_http() {
        let c = LineConfig {
            binding_token: "abc".into(),
            server_url: Some("http://localhost:8080".into()),
            ..Default::default()
        };
        assert_eq!(c.ws_url(), "ws://localhost:8080/ws?token=abc");
    }

    #[test]
    fn reply_url_escapes_request_id() {
        let c = LineConfig {
            binding_token: "t".into(),
            server_url: Some("https://line.thclaws.ai".into()),
            ..Default::default()
        };
        // Spaces / slashes in a request_id are rare but possible
        // for synthesized uuids on weird platforms; URL-escape
        // defensively.
        assert_eq!(
            c.reply_url("a b/c"),
            "https://line.thclaws.ai/reply/a%20b%2Fc"
        );
    }
}
