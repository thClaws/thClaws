//! Shared bridge transport primitives (dev-plan/44 Tier 0).
//!
//! The LINE and Messenger bridges (and the future phone-home channel)
//! all dial **out** to a cloud relay over a WebSocket and POST replies
//! back — the same transport, differing only in a handful of constants
//! (env-var name, default relay host, route prefix) and the
//! platform-specific payload shapes. [`BridgeConfig`] captures the
//! constants and builds every relay URL **once**, so each channel's
//! config only declares its constants instead of copy-pasting the
//! `ws_url` / `reply_url` / `push_url` logic.

pub mod client;
pub mod protocol;

pub use client::{BridgeClient, BridgeClientError, BridgeEnvelopeSink};
pub use protocol::{parse_envelope, QuickReplyButton, ReplyBody, WsEnvelope, WsIncoming};

/// Whether chat-bridge bindings in `dir`'s `.thclaws/` may connect on
/// their own and carry a relay override. They have no fingerprint, so
/// they follow the project `.env`: a folder the user trusted.
pub fn folder_trusted(dir: &std::path::Path) -> bool {
    crate::config::trust::is_folder_trusted(dir)
}

/// Refusal for starting chat bridge `channel` under org policy.
/// `runtime.allow_remote = false` covers every channel that lets someone
/// off this machine drive the agent, not only thClaws Remote.
pub fn policy_refusal(channel: &str) -> Option<String> {
    policy_refusal_for(crate::policy::remote_allowed(), channel)
}

fn policy_refusal_for(allowed: bool, channel: &str) -> Option<String> {
    (!allowed).then(|| {
        format!("{channel} is disabled by org policy (policies.runtime.allow_remote = false)")
    })
}

/// Per-channel relay config the shared bridge transport needs. A
/// channel (LINE, Messenger, …) implements the four accessors +
/// optional [`route_prefix`](BridgeConfig::route_prefix); the URL
/// builders below are derived once for all channels.
pub trait BridgeConfig {
    /// The binding JWT — sent as the `?token=` query param on the WS
    /// (browser WS APIs can't set auth headers) and as the
    /// `Authorization: Bearer` on reply/push POSTs.
    fn binding_token(&self) -> &str;

    /// An explicit relay base saved in the channel's config, if any.
    /// Highest precedence in [`resolved_server_url`](Self::resolved_server_url).
    fn server_url_override(&self) -> Option<&str>;

    /// Env var consulted for the relay base when no override is saved
    /// (e.g. `THCLAWS_LINE_SERVER`).
    fn server_env_var(&self) -> &'static str;

    /// Relay base used when neither an override nor the env var is set.
    fn default_server(&self) -> &'static str;

    /// Path prefix for the reply/push routes — `""` for LINE,
    /// `"/messenger"` for Messenger. The relay namespaces each channel's
    /// outbound routes under this.
    fn route_prefix(&self) -> &'static str {
        ""
    }

    // ── Derived (one implementation for every channel) ──────────────

    /// Resolve the relay base URL. Precedence: explicit `server_url`
    /// override → `server_env_var` → `default_server`. Trailing `/`
    /// stripped.
    fn resolved_server_url(&self) -> String {
        if let Some(url) = self.server_url_override() {
            return url.trim_end_matches('/').to_string();
        }
        if let Ok(url) = std::env::var(self.server_env_var()) {
            if !url.trim().is_empty() {
                return url.trim_end_matches('/').to_string();
            }
        }
        self.default_server().to_string()
    }

    /// Build the `wss://…/ws?token=<jwt>` URL the WS client opens
    /// (`ws://` for a plain-HTTP relay base, used in local dev).
    fn ws_url(&self) -> String {
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
            urlencoding::encode(self.binding_token())
        )
    }

    /// Build the absolute `POST {prefix}/reply/<request_id>` URL.
    fn reply_url(&self, request_id: &str) -> String {
        format!(
            "{}{}/reply/{}",
            self.resolved_server_url(),
            self.route_prefix(),
            urlencoding::encode(request_id)
        )
    }

    /// Build the absolute `POST {prefix}/push` URL — unsolicited
    /// messages (approval prompts, timeout notices) that have no
    /// inbound event to reply to.
    fn push_url(&self) -> String {
        format!("{}{}/push", self.resolved_server_url(), self.route_prefix())
    }

    /// Build the absolute `POST /unpair` URL. Not channel-prefixed —
    /// the relay unbinds by token regardless of source.
    fn unpair_url(&self) -> String {
        format!("{}/unpair", self.resolved_server_url())
    }

    /// Build the absolute `POST {prefix}/event` URL — the engine fans its
    /// live conversation (chat envelopes) here so a connected surface sees
    /// the same stream (dev-plan/44 streaming).
    fn event_url(&self) -> String {
        format!(
            "{}{}/event",
            self.resolved_server_url(),
            self.route_prefix()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{policy_refusal_for, BridgeConfig};

    struct TestCfg {
        token: String,
        override_url: Option<String>,
        prefix: &'static str,
    }
    impl BridgeConfig for TestCfg {
        fn binding_token(&self) -> &str {
            &self.token
        }
        fn server_url_override(&self) -> Option<&str> {
            self.override_url.as_deref()
        }
        fn server_env_var(&self) -> &'static str {
            "THCLAWS_BRIDGE_TEST_SERVER"
        }
        fn default_server(&self) -> &'static str {
            "https://line.thclaws.ai"
        }
        fn route_prefix(&self) -> &'static str {
            self.prefix
        }
    }

    fn cfg(prefix: &'static str, override_url: Option<&str>) -> TestCfg {
        TestCfg {
            token: "abc".into(),
            override_url: override_url.map(str::to_string),
            prefix,
        }
    }

    #[test]
    fn policy_without_remote_refuses_every_bridge() {
        assert!(policy_refusal_for(true, "LINE").is_none());
        let msg = policy_refusal_for(false, "Telegram").unwrap();
        assert!(msg.starts_with("Telegram is disabled by org policy"));
    }

    #[test]
    fn untrusted_project_binding_loses_its_relay_override() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".thclaws/state")).unwrap();
        let body = r#"{"binding_token":"t","server_url":"https://evil.example"}"#;
        for f in ["line.json", "messenger.json", "state/phone-home.json"] {
            std::fs::write(dir.path().join(".thclaws").join(f), body).unwrap();
        }
        let d = dir.path();
        let line = |t| crate::line::LineConfig::load_in(d, t).unwrap().unwrap();
        let msgr = |t| {
            crate::messenger::MessengerConfig::load_in(d, t)
                .unwrap()
                .unwrap()
        };
        let ph = |t| {
            crate::phone_home::PhoneHomeConfig::load_in(d)
                .unwrap()
                .unwrap()
                .gated(t)
        };
        assert_eq!(line(false).server_url, None);
        assert_eq!(msgr(false).server_url, None);
        assert_eq!(ph(false).server_url, None);
        assert_eq!(line(false).binding_token, "t");
        let evil = Some("https://evil.example".to_string());
        assert_eq!(line(true).server_url, evil);
        assert_eq!(msgr(true).server_url, evil);
        assert_eq!(ph(true).server_url, evil);
    }

    #[test]
    fn ws_url_uses_wss_and_encodes_token() {
        assert_eq!(cfg("", None).ws_url(), "wss://line.thclaws.ai/ws?token=abc");
    }

    #[test]
    fn ws_url_downgrades_to_ws_for_plain_http() {
        assert_eq!(
            cfg("", Some("http://localhost:8080")).ws_url(),
            "ws://localhost:8080/ws?token=abc"
        );
    }

    #[test]
    fn override_wins_and_strips_trailing_slash() {
        assert_eq!(
            cfg("", Some("https://custom.example/")).resolved_server_url(),
            "https://custom.example"
        );
    }

    #[test]
    fn route_prefix_applies_to_reply_and_push_only() {
        let line = cfg("", None);
        assert_eq!(line.reply_url("r1"), "https://line.thclaws.ai/reply/r1");
        assert_eq!(line.push_url(), "https://line.thclaws.ai/push");
        assert_eq!(line.unpair_url(), "https://line.thclaws.ai/unpair");

        let msgr = cfg("/messenger", None);
        assert_eq!(
            msgr.reply_url("r1"),
            "https://line.thclaws.ai/messenger/reply/r1"
        );
        assert_eq!(msgr.push_url(), "https://line.thclaws.ai/messenger/push");
        // unpair stays un-prefixed for every channel.
        assert_eq!(msgr.unpair_url(), "https://line.thclaws.ai/unpair");
    }

    #[test]
    fn reply_url_encodes_request_id() {
        assert_eq!(
            cfg("", None).reply_url("a b/c"),
            "https://line.thclaws.ai/reply/a%20b%2Fc"
        );
    }
}
