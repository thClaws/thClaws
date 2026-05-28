//! Wire shapes for the relay ↔ thClaws-client Messenger channel.
//!
//! These deserialise the JSON frames the relay pushes over the
//! WebSocket (`WsEnvelope`) and serialise the body posted back to
//! `POST /reply/{request_id}` (`ReplyBody`). They MUST stay
//! byte-compatible with the relay's Messenger broker envelope.
//!
//! Unlike the LINE channel, the inbound user is identified by a
//! **PSID** (Page-Scoped ID) rather than a LINE user id, and the
//! outbound rich affordance is a Messenger **quick reply** rather
//! than a LINE Quick Reply chip — but the request/reply shape is
//! deliberately the same so the relay machinery is shared.

use serde::{Deserialize, Serialize};

/// Inbound envelope — what the relay pushes us. `kind`-tagged so
/// future additions (attachment, referral, etc.) deserialise
/// side-by-side without breaking existing variants.
/// `kind` tags are namespaced `messenger_*` to match the relay's
/// shared broker enum (`thclaws_line_server::broker::WsEnvelope`),
/// which also carries LINE variants on the same `/ws` stream.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind")]
pub enum WsEnvelope {
    #[serde(rename = "messenger_user_message")]
    UserMessage {
        text: String,
        /// Page-Scoped ID of the sender. Kept on the wire so the
        /// session can key per-PSID state without a protocol bump.
        #[serde(default)]
        psid: String,
        request_id: String,
        /// Facebook message id (`mid`) — reserved for dedup; the
        /// relay may send it empty.
        #[serde(default)]
        mid: String,
    },
    /// Quick-reply tap or button postback — `payload` is the
    /// developer-set string we attached (e.g. `tool:allow:<id>`).
    #[serde(rename = "messenger_postback")]
    Postback { payload: String },
    /// Relay-pushed notice (paired, reconnected, …) — logged
    /// locally, not forwarded to the agent.
    #[serde(rename = "messenger_notice")]
    Notice { text: String },
}

/// Container that tags the WS frame variant we received before
/// pattern-matching, useful for logging.
#[derive(Debug)]
pub enum WsIncoming {
    Envelope(WsEnvelope),
    /// Frame was valid JSON but didn't match any known variant.
    Unknown(String),
    /// Server closed the WS cleanly.
    Closed,
}

/// Body of `POST /reply/{request_id}`. The binding JWT goes in the
/// `Authorization: Bearer` header, NOT this body.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ReplyBody {
    pub text: String,
    /// Optional quick replies. When present, the relay attaches them
    /// to the Send API message so the user sees tappable chips. The
    /// approver uses these for `[Approve] / [Deny]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quick_replies: Option<Vec<QuickReply>>,
}

/// One Messenger quick reply. The relay expands this into the Send
/// API `quick_replies[]` shape (`content_type: "text"`, `title`,
/// `payload`). Messenger allows ≤13 per message and `title` ≤20 chars.
///
/// `payload` is what the user's tap lands as on the relay's webhook
/// (`message.quick_reply.payload`); see
/// [`super::approver::ApprovalReply::parse_postback`] for the
/// expected `tool:<verb>:<request_id>` shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickReply {
    /// Chip label shown to the user. ≤20 chars per Meta docs.
    pub title: String,
    /// Postback `payload` the chip carries.
    pub payload: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_round_trips() {
        let json = r#"{"kind":"messenger_user_message","text":"hi","psid":"PSID1","request_id":"r1","mid":""}"#;
        let env: WsEnvelope = serde_json::from_str(json).unwrap();
        match env {
            WsEnvelope::UserMessage {
                text,
                psid,
                request_id,
                ..
            } => {
                assert_eq!(text, "hi");
                assert_eq!(psid, "PSID1");
                assert_eq!(request_id, "r1");
            }
            _ => panic!("expected UserMessage"),
        }
    }

    #[test]
    fn missing_mid_and_psid_default_to_empty() {
        // Relay may omit optional fields — serde defaults them.
        let json = r#"{"kind":"messenger_user_message","text":"hi","request_id":"r1"}"#;
        let env: WsEnvelope = serde_json::from_str(json).unwrap();
        match env {
            WsEnvelope::UserMessage { mid, psid, .. } => {
                assert!(mid.is_empty());
                assert!(psid.is_empty());
            }
            _ => panic!("expected UserMessage"),
        }
    }

    #[test]
    fn postback_decodes() {
        let json = r#"{"kind":"messenger_postback","payload":"tool:allow:abc"}"#;
        let env: WsEnvelope = serde_json::from_str(json).unwrap();
        match env {
            WsEnvelope::Postback { payload } => assert_eq!(payload, "tool:allow:abc"),
            _ => panic!("expected Postback"),
        }
    }

    #[test]
    fn notice_decodes() {
        let json = r#"{"kind":"messenger_notice","text":"connected"}"#;
        let env: WsEnvelope = serde_json::from_str(json).unwrap();
        assert!(matches!(env, WsEnvelope::Notice { .. }));
    }

    #[test]
    fn reply_body_with_quick_replies_serialises() {
        let body = ReplyBody {
            text: "approve?".into(),
            quick_replies: Some(vec![
                QuickReply {
                    title: "Allow".into(),
                    payload: "tool:allow:abc".into(),
                },
                QuickReply {
                    title: "Deny".into(),
                    payload: "tool:deny:abc".into(),
                },
            ]),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["text"], "approve?");
        let qr = &json["quick_replies"];
        assert!(qr.is_array());
        assert_eq!(qr[0]["title"], "Allow");
        assert_eq!(qr[0]["payload"], "tool:allow:abc");
    }

    #[test]
    fn reply_body_without_quick_replies_omits_field() {
        let body = ReplyBody {
            text: "plain".into(),
            quick_replies: None,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(!json.contains("quick_replies"), "got: {json}");
    }
}
