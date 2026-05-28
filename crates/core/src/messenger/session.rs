//! Bridge between the Messenger WS client and the agent loop.
//!
//! `MessengerSession` is what the GUI worker / headless mode spawns
//! when a binding token is present. Mirrors
//! [`crate::line::session::LineSession`]: a `UserMessage` envelope is
//! pushed as a user turn into the shared session via the
//! `MessengerMessageHandler` trait; the final assistant text is
//! chunked to Messenger's 2 000-char limit and sent back through the
//! relay. Quick-reply / postback taps resolve a pending tool approval.
//!
//! Concrete handler impls live in `bootstrap.rs` (GUI worker, Tier 1
//! follow-up) and the headless `--messenger` mode.

use std::sync::Arc;

use async_trait::async_trait;

use super::approver::{ApprovalReply, MessengerApprover};
use super::client::{MessengerClient, MessengerClientError, MessengerEnvelopeSink};
use super::config::MessengerConfig;
use super::filter::chunks_for_messenger;
use super::protocol::WsEnvelope;

/// Pluggable handler — what to do when a Messenger user message
/// arrives. Returns the final assistant text; `None` skips the reply
/// (e.g. a command handled inline).
#[async_trait]
pub trait MessengerMessageHandler: Send + Sync + 'static {
    async fn handle_message(&self, text: String) -> Option<String>;

    /// Quick-reply / postback taps not consumed by the approver.
    /// Default no-op.
    async fn handle_postback(&self, _payload: String) {}
}

pub struct MessengerSession {
    client: Arc<MessengerClient>,
    handler: Arc<dyn MessengerMessageHandler>,
    /// When `Some`, inbound text + postbacks route to the approver
    /// first; an approval reply short-circuits the agent turn.
    approver: Option<Arc<MessengerApprover>>,
}

impl MessengerSession {
    pub fn new(config: MessengerConfig, handler: Arc<dyn MessengerMessageHandler>) -> Self {
        Self {
            client: Arc::new(MessengerClient::new(config)),
            handler,
            approver: None,
        }
    }

    pub fn with_approver(mut self, approver: Arc<MessengerApprover>) -> Self {
        self.approver = Some(approver);
        self
    }

    pub fn with_cancel(mut self, token: crate::cancel::CancelToken) -> Self {
        let client = Arc::try_unwrap(self.client)
            .map(|c| c.with_cancel(token.clone()))
            .unwrap_or_else(|arc| {
                let _ = arc;
                MessengerClient::new(MessengerConfig::default()).with_cancel(token)
            });
        self.client = Arc::new(client);
        self
    }

    /// Drive the WS loop forever. Returns only on cancellation or a
    /// permanent error (reconnect handles transient failures).
    pub async fn run(self: Arc<Self>) -> Result<(), MessengerClientError> {
        let sink = SessionSink {
            client: self.client.clone(),
            handler: self.handler.clone(),
            approver: self.approver.clone(),
        };
        self.client.run(sink).await
    }
}

struct SessionSink {
    client: Arc<MessengerClient>,
    handler: Arc<dyn MessengerMessageHandler>,
    approver: Option<Arc<MessengerApprover>>,
}

impl SessionSink {
    /// Send each chunk of an agent reply in order. Stops on the first
    /// transport error (logged) — partial delivery beats none.
    async fn send_reply_chunked(client: &MessengerClient, request_id: &str, reply: &str) {
        for chunk in chunks_for_messenger(reply) {
            if let Err(e) = client.send_reply(request_id, chunk).await {
                eprintln!("[messenger] reply failed (request_id={request_id}): {e}");
                break;
            }
        }
    }
}

#[async_trait]
impl MessengerEnvelopeSink for SessionSink {
    async fn on_envelope(&self, envelope: WsEnvelope) {
        match envelope {
            WsEnvelope::UserMessage {
                text,
                psid,
                request_id,
                ..
            } => {
                eprintln!(
                    "[messenger] user message ({} chars, psid={}, request_id={})",
                    text.chars().count(),
                    psid,
                    request_id
                );
                // Never block this method on the agent turn: the WS
                // recv loop awaits `on_envelope` in line, so a turn
                // that pauses on an approval prompt would deadlock the
                // delivery of the Postback that resolves it. Each
                // message spawns a detached task; turns are still
                // serialised at the worker channel layer.
                //
                // The approval-text short-circuit runs synchronously
                // BEFORE the spawn so a resolve race can't leak into a
                // half-spawned turn — its confirmation send is spawned
                // to keep `on_envelope` non-blocking.
                if let Some(approver) = &self.approver {
                    if approver.has_pending() {
                        if let Some(reply_kind) = approver.record_decision_from_text(&text) {
                            let msg = match reply_kind {
                                ApprovalReply::Allow => "✅ Approved — running tool now.",
                                ApprovalReply::Deny => "🚫 Denied — agent will not run the tool.",
                                ApprovalReply::Unrecognised => {
                                    "I didn't catch that. Please reply 'approve' or 'deny'."
                                }
                            };
                            let client = self.client.clone();
                            let request_id = request_id.clone();
                            let msg = msg.to_string();
                            tokio::spawn(async move {
                                if let Err(e) = client.send_reply(&request_id, msg).await {
                                    eprintln!("[messenger] approval confirm reply failed: {e}");
                                }
                            });
                            return;
                        }
                    }
                }

                let handler = self.handler.clone();
                let client = self.client.clone();
                tokio::spawn(async move {
                    if let Some(reply) = handler.handle_message(text).await {
                        Self::send_reply_chunked(&client, &request_id, &reply).await;
                    }
                });
            }
            WsEnvelope::Postback { payload } => {
                eprintln!("[messenger] postback: {payload}");
                // Resolution is sync (resolves a oneshot); returning
                // quickly unblocks the approval-waiting agent turn.
                if let Some(approver) = &self.approver {
                    if approver.record_decision_from_postback(&payload).is_some() {
                        return;
                    }
                }
                let handler = self.handler.clone();
                tokio::spawn(async move {
                    handler.handle_postback(payload).await;
                });
            }
            WsEnvelope::Notice { text } => {
                eprintln!("[messenger] notice: {text}");
            }
        }
    }
}
