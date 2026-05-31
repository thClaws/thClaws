//! Telegram adapter config — the canonical `TelegramConfig` struct plus
//! the on-disk runtime state.
//!
//! Config lookup (dev-plan/33 Tier 2 — per-project, not per-user):
//! 1. **Project runtime** — `./.thclaws/telegram.json` (preferred).
//!    Each GUI Shell / project owns its own bot config so distributing
//!    a shell folder to a VPS doesn't leak the user's personal bot
//!    settings — and so two shells running side-by-side don't fight
//!    over a single user-level bot binding.
//! 2. **User-level legacy** — `~/.config/thclaws/telegram.json`.
//!    Loaded ONLY when the env var `THCLAWS_TELEGRAM_USER_CONFIG=1` is
//!    set. Pre-Tier 2 installs had their token here; this opt-in
//!    fallback lets existing users keep their setup until they choose
//!    to migrate (move the file into a project's `.thclaws/`).
//!
//! The bot **token** resolves independently of either file via
//! [`TelegramConfig::resolved_token`]: `TELEGRAM_BOT_TOKEN` env beats
//! the config-file `bot_token` beats nothing. Env-wins matches the
//! 12-factor convention for secrets (and Tier 1 acceptance test #8) so
//! a token never has to be written to disk in CI / container runs.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default per-message character ceiling. Telegram hard-rejects text
/// messages over 4096 chars; we chunk below that so an appended
/// "continued" marker always fits.
pub const DEFAULT_OUTPUT_CEILING: u32 = 4000;

/// Env var consulted before the config-file `bot_token`.
pub const BOT_TOKEN_ENV: &str = "TELEGRAM_BOT_TOKEN";

/// Env opt-in for the legacy `~/.config/thclaws/telegram.json` fallback
/// path. Without this, only `./.thclaws/telegram.json` is consulted.
pub const USER_FALLBACK_ENV: &str = "THCLAWS_TELEGRAM_USER_CONFIG";

fn user_fallback_enabled() -> bool {
    std::env::var(USER_FALLBACK_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        .unwrap_or(false)
}

#[derive(Debug, thiserror::Error)]
pub enum TelegramConfigError {
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
    #[error("invalid bot token: {0}")]
    InvalidToken(String),
}

/// Who may DM the bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DmPolicy {
    /// First DM-er gets a 6-digit pairing code; the owner approves via
    /// the GUI, which appends their user id to `allow_from`. Copied from
    /// OpenClaw — lowest setup friction. (Default.)
    #[default]
    Pairing,
    /// Only user ids already in `allow_from` may DM. Unknown senders are
    /// silently ignored (no pairing prompt).
    Allowlist,
}

/// Who may talk to the bot in group chats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupPolicy {
    /// Only group chat ids present in `groups` are served. (Default —
    /// "no groups until you opt one in".)
    #[default]
    Allowlist,
    /// Any group the bot is added to is served. Use with care — anyone
    /// who can add the bot to a group can then talk to it.
    Open,
}

/// Per-group settings. Minimal in Tier 1 (presence in the `groups` map
/// is what allowlists the chat); Tier 2 adds forum-topic routing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TelegramGroupConfig {
    /// Optional human label for the GUI status pill. Telegram doesn't
    /// hand us a stable group name on every update, so the modal lets
    /// the user name it at add-time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Per-channel settings — Tier 2 territory (broadcast post + linked
/// discussion group + topic routing). Defined now so the struct shape
/// is stable across tiers and `settings.json` written by a Tier 2
/// build still loads on a Tier 1 binary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TelegramChannelConfig {
    /// Chat id (string) of the discussion group Telegram auto-links to
    /// this channel. Comments on channel posts arrive here as regular
    /// `message` updates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_discussion_group: Option<String>,
    /// Default agent for this channel + its discussion group. Falls back
    /// to the main agent when unset. References an AgentDef name under
    /// `.thclaws/agents/` (same key the Agent Teams subsystem uses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Per-forum-topic agent overrides, keyed by `message_thread_id` (as
    /// a string). A topic not listed here falls back to `agent_id`. The
    /// "General" topic is thread id `1`.
    pub topic_routing: HashMap<String, TopicRoute>,
}

/// Routing for one forum topic.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TopicRoute {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TelegramConfig {
    pub enabled: bool,
    /// Bot token from `@BotFather`. May be absent here when supplied via
    /// `TELEGRAM_BOT_TOKEN` — see [`Self::resolved_token`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,
    pub dm_policy: DmPolicy,
    /// Telegram user ids (as strings, since they exceed i32 and JSON
    /// loses precision past 2^53) allowed to DM the bot.
    pub allow_from: Vec<String>,
    pub group_policy: GroupPolicy,
    /// Allowlisted group chat ids (negative integers, as strings).
    pub groups: HashMap<String, TelegramGroupConfig>,
    /// Channel bindings (Tier 2).
    pub channels: HashMap<String, TelegramChannelConfig>,
    pub output_ceiling: u32,
    /// Tier 3.1: edit a single message in place as the agent streams,
    /// instead of sending one reply at the end. Opt-in (Telegram throttles
    /// repeated same-message edits, and only the headless `--telegram`
    /// path honours it today). `streamPreview` on the wire.
    pub stream_preview: bool,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: None,
            dm_policy: DmPolicy::default(),
            allow_from: Vec::new(),
            group_policy: GroupPolicy::default(),
            groups: HashMap::new(),
            channels: HashMap::new(),
            output_ceiling: DEFAULT_OUTPUT_CEILING,
            stream_preview: false,
        }
    }
}

impl TelegramConfig {
    /// Project-scoped runtime path: `./.thclaws/telegram.json` —
    /// resolved against the current working directory at call time.
    /// dev-plan/33 Tier 2 moved this off the user-level path so each
    /// GUI Shell / project owns its bot config independently.
    pub fn path() -> Result<PathBuf, TelegramConfigError> {
        let cwd = std::env::current_dir().map_err(|source| TelegramConfigError::Io {
            path: PathBuf::from("."),
            source,
        })?;
        Ok(cwd.join(".thclaws").join("telegram.json"))
    }

    /// Legacy user-level path (`~/.config/thclaws/telegram.json`).
    /// Only consulted as a fallback when the env opt-in is set —
    /// pre-Tier 2 installs had their config here, and we don't
    /// silently delete or migrate user state.
    pub fn legacy_user_path() -> Result<PathBuf, TelegramConfigError> {
        let home = crate::util::home_dir().ok_or(TelegramConfigError::NoHome)?;
        Ok(home.join(".config").join("thclaws").join("telegram.json"))
    }

    /// Read from disk. `Ok(None)` when absent at the project path AND
    /// (when env opt-in is set) at the legacy user path.
    ///
    /// Env opt-in: set `THCLAWS_TELEGRAM_USER_CONFIG=1` to fall back
    /// to `~/.config/thclaws/telegram.json` when no project-level file
    /// exists. Keeps existing users working until they migrate by
    /// moving their `telegram.json` into the target project's
    /// `.thclaws/` folder.
    pub fn load() -> Result<Option<Self>, TelegramConfigError> {
        let project_path = Self::path()?;
        match std::fs::read_to_string(&project_path) {
            Ok(body) => {
                return serde_json::from_str(&body).map(Some).map_err(|source| {
                    TelegramConfigError::Json {
                        path: project_path,
                        source,
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(TelegramConfigError::Io {
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
                    .map_err(|source| TelegramConfigError::Json {
                        path: user_path,
                        source,
                    })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(TelegramConfigError::Io {
                path: user_path,
                source,
            }),
        }
    }

    /// Persist atomically (write `.tmp` then rename) so a crash mid-write
    /// can't leave a half-written file the next launch fails to parse.
    pub fn save(&self) -> Result<(), TelegramConfigError> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| TelegramConfigError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let body =
            serde_json::to_string_pretty(self).map_err(|source| TelegramConfigError::Json {
                path: path.clone(),
                source,
            })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(|source| TelegramConfigError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, &path).map_err(|source| TelegramConfigError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(())
    }

    /// Delete the runtime file (GUI "Disconnect"). Idempotent.
    pub fn delete() -> Result<(), TelegramConfigError> {
        let path = Self::path()?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(TelegramConfigError::Io { path, source }),
        }
    }

    /// Resolve the bot token. `TELEGRAM_BOT_TOKEN` env wins over the
    /// config-file `bot_token`; an empty env var is ignored (treated as
    /// unset) so `TELEGRAM_BOT_TOKEN=` doesn't blank out a configured
    /// token.
    pub fn resolved_token(&self) -> Option<String> {
        if let Ok(env) = std::env::var(BOT_TOKEN_ENV) {
            let env = env.trim();
            if !env.is_empty() {
                return Some(env.to_string());
            }
        }
        self.bot_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    }

    /// True when a DM from `user_id` should be served *without* pairing.
    /// `Pairing` policy: known ids skip pairing, unknown ids trigger it
    /// (handled by the caller — this returns false for unknown).
    /// `Allowlist` policy: only known ids, full stop.
    pub fn allows_dm(&self, user_id: i64) -> bool {
        self.allow_from.iter().any(|id| id == &user_id.to_string())
    }

    /// True when a group `chat_id` should be served.
    pub fn allows_group(&self, chat_id: i64) -> bool {
        match self.group_policy {
            GroupPolicy::Open => true,
            GroupPolicy::Allowlist => self.groups.contains_key(&chat_id.to_string()),
        }
    }

    /// Append a newly-paired user id to `allow_from` (idempotent).
    /// Returns true when the id was newly added.
    pub fn add_allowed_user(&mut self, user_id: i64) -> bool {
        let id = user_id.to_string();
        if self.allow_from.contains(&id) {
            return false;
        }
        self.allow_from.push(id);
        true
    }

    // ── Tier 2: channels + linked discussion groups ──

    /// The channel config for `chat_id`, if `chat_id` is a configured
    /// broadcast channel.
    pub fn channel(&self, chat_id: i64) -> Option<&TelegramChannelConfig> {
        self.channels.get(&chat_id.to_string())
    }

    /// The channel config whose linked discussion group is `chat_id`.
    /// Comments on channel posts arrive in this group, so a message here
    /// is authorized + routed even though the group isn't in `groups`.
    pub fn channel_for_discussion_group(&self, chat_id: i64) -> Option<&TelegramChannelConfig> {
        let id = chat_id.to_string();
        self.channels
            .values()
            .find(|c| c.linked_discussion_group.as_deref() == Some(id.as_str()))
    }

    /// True when `chat_id` is a configured channel or the discussion
    /// group linked to one — i.e. Tier 2 should serve messages from it
    /// regardless of `group_policy`.
    pub fn is_channel_surface(&self, chat_id: i64) -> bool {
        self.channel(chat_id).is_some() || self.channel_for_discussion_group(chat_id).is_some()
    }
}

/// Validate a Bot API token's shape: `<bot_id>:<secret>` where `bot_id`
/// is all digits and `secret` is a non-trivial run of token chars.
/// Deliberately lenient (we don't pin the exact secret length BotFather
/// uses, since it's undocumented and could change) — this catches a
/// pasted username / empty string / obviously-wrong value, not a typo
/// inside an otherwise well-formed token.
pub fn validate_token(token: &str) -> Result<(), TelegramConfigError> {
    let token = token.trim();
    let Some((id, secret)) = token.split_once(':') else {
        return Err(TelegramConfigError::InvalidToken(
            "expected '<bot_id>:<secret>' (missing ':')".into(),
        ));
    };
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TelegramConfigError::InvalidToken(
            "bot id (before ':') must be all digits".into(),
        ));
    }
    if secret.len() < 20
        || !secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(TelegramConfigError::InvalidToken(
            "secret (after ':') is too short or has invalid characters".into(),
        ));
    }
    Ok(())
}

/// Redact a token for logs: keep the bot id, mask the secret. A leaked
/// bot id alone is harmless (it's in `getMe`); the secret is the key.
pub fn redact_token(token: &str) -> String {
    match token.split_once(':') {
        Some((id, _)) => format!("{id}:<redacted>"),
        None => "<redacted>".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_pairing_allowlist_4000() {
        let c = TelegramConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.dm_policy, DmPolicy::Pairing);
        assert_eq!(c.group_policy, GroupPolicy::Allowlist);
        assert_eq!(c.output_ceiling, DEFAULT_OUTPUT_CEILING);
        assert!(c.allow_from.is_empty());
    }

    #[test]
    fn camel_case_round_trip() {
        let json = r#"{
            "enabled": true,
            "botToken": "123456789:AAFmKQk_abcdefghijklmnopqrstuvwxyz",
            "dmPolicy": "allowlist",
            "allowFrom": ["111", "222"],
            "groupPolicy": "open",
            "groups": {},
            "channels": {},
            "outputCeiling": 3000
        }"#;
        let c: TelegramConfig = serde_json::from_str(json).unwrap();
        assert!(c.enabled);
        assert_eq!(c.dm_policy, DmPolicy::Allowlist);
        assert_eq!(c.group_policy, GroupPolicy::Open);
        assert_eq!(c.allow_from, vec!["111", "222"]);
        assert_eq!(c.output_ceiling, 3000);
        // Re-serialise and confirm camelCase keys survive.
        let v = serde_json::to_value(&c).unwrap();
        assert!(v.get("botToken").is_some());
        assert!(v.get("dmPolicy").is_some());
        assert!(v.get("outputCeiling").is_some());
    }

    #[test]
    fn partial_json_fills_defaults() {
        // A `.thclaws/settings.json` that only sets the token should
        // still parse, defaulting policy/ceiling.
        let c: TelegramConfig =
            serde_json::from_str(r#"{"botToken":"1:aaaaaaaaaaaaaaaaaaaaaa"}"#).unwrap();
        assert_eq!(c.dm_policy, DmPolicy::Pairing);
        assert_eq!(c.output_ceiling, DEFAULT_OUTPUT_CEILING);
    }

    #[test]
    fn resolved_token_env_beats_file_beats_none() {
        let mut c = TelegramConfig {
            bot_token: Some("111:fromfileeeeeeeeeeeeeeeee".into()),
            ..Default::default()
        };
        // File wins when env unset.
        std::env::remove_var(BOT_TOKEN_ENV);
        assert_eq!(
            c.resolved_token().as_deref(),
            Some("111:fromfileeeeeeeeeeeeeeeee")
        );

        // Env beats file.
        std::env::set_var(BOT_TOKEN_ENV, "999:fromenvvvvvvvvvvvvvvvvvv");
        assert_eq!(
            c.resolved_token().as_deref(),
            Some("999:fromenvvvvvvvvvvvvvvvvvv")
        );

        // Empty env is ignored (doesn't blank the file token).
        std::env::set_var(BOT_TOKEN_ENV, "   ");
        assert_eq!(
            c.resolved_token().as_deref(),
            Some("111:fromfileeeeeeeeeeeeeeeee")
        );

        // Nothing anywhere → None.
        std::env::remove_var(BOT_TOKEN_ENV);
        c.bot_token = None;
        assert_eq!(c.resolved_token(), None);
    }

    #[test]
    fn allows_dm_matches_string_ids() {
        let c = TelegramConfig {
            allow_from: vec!["111".into()],
            ..Default::default()
        };
        assert!(c.allows_dm(111));
        assert!(!c.allows_dm(222));
    }

    #[test]
    fn allows_group_respects_policy() {
        let mut c = TelegramConfig::default();
        // Allowlist default: unknown group denied.
        assert!(!c.allows_group(-100));
        c.groups
            .insert("-100".into(), TelegramGroupConfig::default());
        assert!(c.allows_group(-100));
        // Open policy: any group allowed.
        c.group_policy = GroupPolicy::Open;
        assert!(c.allows_group(-999));
    }

    #[test]
    fn add_allowed_user_is_idempotent() {
        let mut c = TelegramConfig::default();
        assert!(c.add_allowed_user(111));
        assert!(!c.add_allowed_user(111));
        assert_eq!(c.allow_from, vec!["111"]);
    }

    #[test]
    fn validate_token_accepts_botfather_shape() {
        assert!(validate_token("123456789:AAFmKQk_abcdefghijklmnopqrstuvwxyz").is_ok());
        assert!(validate_token("  123:aaaaaaaaaaaaaaaaaaaaaa  ").is_ok());
    }

    #[test]
    fn validate_token_rejects_garbage() {
        assert!(validate_token("").is_err());
        assert!(validate_token("not-a-token").is_err()); // no ':'
        assert!(validate_token("abc:aaaaaaaaaaaaaaaaaaaaaa").is_err()); // non-digit id
        assert!(validate_token("123:short").is_err()); // secret too short
        assert!(validate_token("123:has spaces in secretttt").is_err()); // bad chars
    }

    #[test]
    fn redact_keeps_bot_id_masks_secret() {
        assert_eq!(
            redact_token("123456789:AAFmKQk_secret"),
            "123456789:<redacted>"
        );
        assert_eq!(redact_token("garbage"), "<redacted>");
    }
}
