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

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
        Ok(home.join(".config").join("thclaws").join("line.json"))
    }

    /// Read from disk. Project path first; legacy user path as
    /// opt-in fallback. `Ok(None)` when both are absent (the
    /// default state for a fresh install).
    pub fn load() -> Result<Option<Self>, LineConfigError> {
        let project_path = Self::path()?;
        match std::fs::read_to_string(&project_path) {
            Ok(body) => {
                return serde_json::from_str(&body).map(Some).map_err(|source| {
                    LineConfigError::Json {
                        path: project_path,
                        source,
                    }
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

    /// Resolve the relay URL for this binding. Precedence: explicit
    /// `server_url` in the saved config → `THCLAWS_LINE_SERVER` env
    /// → `DEFAULT_SERVER_URL`.
    pub fn resolved_server_url(&self) -> String {
        if let Some(url) = self.server_url.as_deref() {
            return url.trim_end_matches('/').to_string();
        }
        if let Ok(url) = std::env::var("THCLAWS_LINE_SERVER") {
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

    /// Build the absolute `POST /reply/<request_id>` URL.
    pub fn reply_url(&self, request_id: &str) -> String {
        format!(
            "{}/reply/{}",
            self.resolved_server_url(),
            urlencoding::encode(request_id)
        )
    }

    /// Build the absolute `POST /unpair` URL.
    pub fn unpair_url(&self) -> String {
        format!("{}/unpair", self.resolved_server_url())
    }

    /// Build the absolute `POST /push` URL. Used for unsolicited
    /// messages from thClaws — approval prompts, timeout notices.
    /// `/reply/:id` is the wrong primitive for these because there's
    /// no inbound webhook event to provide a `replyToken`.
    pub fn push_url(&self) -> String {
        format!("{}/push", self.resolved_server_url())
    }

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

#[cfg(test)]
mod tests {
    use super::*;

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
