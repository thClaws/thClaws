//! WebSocket client — connect to the relay, deserialise envelopes,
//! POST `/reply/<id>` for outbound text. Reconnect with exponential
//! backoff.
//!
//! Phase 1.1 scope: the public API is `LineClient::run(config)`
//! which loops forever — opens the WS, hands envelopes to a
//! consumer callback, posts replies on demand. Caller spawns it
//! on a background tokio task. Cancellation is via the standard
//! `CancelToken` so the shutdown path matches the rest of thClaws.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::config::LineConfig;
use super::protocol::{QuickReplyButton, ReplyBody, WsEnvelope, WsIncoming};

#[derive(Debug, thiserror::Error)]
pub enum LineClientError {
    #[error("websocket connect: {0}")]
    Connect(String),
    #[error("websocket transport: {0}")]
    Transport(String),
    #[error("reply HTTP: {0}")]
    ReplyHttp(String),
    #[error("reply server returned {status}: {body}")]
    ReplyStatus { status: u16, body: String },
    #[error("cancelled")]
    Cancelled,
}

/// Trait the session implements so the client can hand it
/// envelopes. Kept tiny so testing the client doesn't need to
/// stand up an agent.
#[async_trait::async_trait]
pub trait LineEnvelopeSink: Send + Sync + 'static {
    async fn on_envelope(&self, envelope: WsEnvelope);
}

pub struct LineClient {
    config: LineConfig,
    http: reqwest::Client,
    cancel: Option<crate::cancel::CancelToken>,
}

impl LineClient {
    pub fn new(config: LineConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::builder()
                .user_agent(concat!("thclaws-core/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(15))
                .build()
                .expect("reqwest client build"),
            cancel: None,
        }
    }

    pub fn with_cancel(mut self, token: crate::cancel::CancelToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Send a final agent reply for a given LINE message. The
    /// binding JWT is added as `Authorization: Bearer` from the
    /// stored config.
    pub async fn send_reply(
        &self,
        request_id: &str,
        text: impl Into<String>,
    ) -> Result<(), LineClientError> {
        self.send_reply_inner(
            request_id,
            ReplyBody {
                text: text.into(),
                quick_reply: None,
            },
        )
        .await
    }

    /// Send a reply with Quick Reply buttons attached. The relay
    /// expands these into LINE's `quickReply.items[]` shape, so a
    /// tap fires a `Postback` envelope back over the WS carrying
    /// the button's `data` string (see
    /// [`super::approver::LineApprover::record_decision_from_postback`]).
    pub async fn send_reply_with_buttons(
        &self,
        request_id: &str,
        text: impl Into<String>,
        buttons: Vec<QuickReplyButton>,
    ) -> Result<(), LineClientError> {
        self.send_reply_inner(
            request_id,
            ReplyBody {
                text: text.into(),
                quick_reply: Some(buttons),
            },
        )
        .await
    }

    /// Send an unsolicited push message to the bound LINE user.
    /// Used for the approval prompt and timeout notices fired by
    /// `LineApprover` — these have no inbound webhook event, so
    /// `/reply/:id` would 404. `/push` reads the recipient from the
    /// JWT's `sub` claim and calls LINE's push API directly. Costs
    /// against the channel's monthly push quota — keep usage to
    /// genuinely unsolicited messages (approvals + errors), use
    /// `send_reply` for replies to incoming user messages.
    pub async fn push(&self, text: impl Into<String>) -> Result<(), LineClientError> {
        self.push_inner(ReplyBody {
            text: text.into(),
            quick_reply: None,
        })
        .await
    }

    /// Push variant with Quick Reply chips, same wire shape as
    /// `send_reply_with_buttons`.
    pub async fn push_with_buttons(
        &self,
        text: impl Into<String>,
        buttons: Vec<QuickReplyButton>,
    ) -> Result<(), LineClientError> {
        self.push_inner(ReplyBody {
            text: text.into(),
            quick_reply: Some(buttons),
        })
        .await
    }

    async fn push_inner(&self, body: ReplyBody) -> Result<(), LineClientError> {
        let url = self.config.push_url();
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.config.binding_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| LineClientError::ReplyHttp(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LineClientError::ReplyStatus { status, body });
        }
        Ok(())
    }

    /// Push a single browser-chat `ViewEvent` to the relay's
    /// fan-out endpoint. The relay routes to `Channel::Browser`
    /// via the broker — if no browser is currently connected,
    /// the route is a silent drop. Best-effort: a network error
    /// is logged at the caller; we don't retry.
    ///
    /// The `event` value is the JSON envelope the browser SPA
    /// dispatches by `type` — `assistant_delta`, `tool_call_start`,
    /// `turn_done`, etc. The relay doesn't introspect.
    pub async fn push_chat_event(&self, event: serde_json::Value) -> Result<(), LineClientError> {
        let url = self.config.chat_bridge_event_url();
        let body = serde_json::json!({ "event": event });
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.config.binding_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| LineClientError::ReplyHttp(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LineClientError::ReplyStatus { status, body });
        }
        Ok(())
    }

    /// GET `/chat-bridge/has-browser`. Returns true when a
    /// plan-10 browser chat session is currently open for this
    /// binding's user. `LineApprover` calls this to choose
    /// between the browser approval modal (free, rich UI) and
    /// LINE OA push (Quick Reply chips, quota-using). On any
    /// transport error we treat as "no browser" so the approver
    /// falls back to LINE OA — never leaves the user without an
    /// approval surface.
    pub async fn has_browser_connected(&self) -> bool {
        let url = self.config.chat_bridge_has_browser_url();
        let resp = match self
            .http
            .get(&url)
            .bearer_auth(&self.config.binding_token)
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => return false,
        };
        if !resp.status().is_success() {
            return false;
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => return false,
        };
        body.get("browser_connected")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Tell the relay to drop our binding. Used by the GUI's
    /// "Disconnect" path — without this, the server still thinks
    /// the user is paired and routes their next LINE message into
    /// a dead WS (silent drop). Best-effort: a network failure
    /// here doesn't block the local cleanup.
    pub async fn unpair(&self) -> Result<(), LineClientError> {
        let url = self.config.unpair_url();
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.config.binding_token)
            .send()
            .await
            .map_err(|e| LineClientError::ReplyHttp(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LineClientError::ReplyStatus { status, body });
        }
        Ok(())
    }

    async fn send_reply_inner(
        &self,
        request_id: &str,
        body: ReplyBody,
    ) -> Result<(), LineClientError> {
        let url = self.config.reply_url(request_id);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.config.binding_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| LineClientError::ReplyHttp(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LineClientError::ReplyStatus { status, body });
        }
        Ok(())
    }

    /// Run until cancelled. Opens the WS, dispatches envelopes to
    /// `sink`, and reconnects with exponential backoff on any
    /// transport error. Exits early with `Cancelled` when the
    /// `CancelToken` is fired (and only then).
    pub async fn run<S: LineEnvelopeSink>(&self, sink: S) -> Result<(), LineClientError> {
        let ws_url = self.config.ws_url();
        eprintln!("[line] client starting → {}", redact_token(&ws_url));

        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(60);
        loop {
            if self.is_cancelled() {
                return Err(LineClientError::Cancelled);
            }
            match self.connect_and_pump(&ws_url, &sink).await {
                Ok(()) => {
                    eprintln!("[line] ws closed cleanly; reconnecting");
                    backoff = Duration::from_secs(1);
                    // Pause briefly before reconnecting. A relay that
                    // repeatedly closes the connection cleanly (e.g. on
                    // restart, idle timeout, or an immediate close on
                    // connect) would otherwise spin this loop in an
                    // unthrottled connect/close reconnect storm.
                    if self.sleep_with_cancel(Duration::from_secs(1)).await {
                        return Err(LineClientError::Cancelled);
                    }
                }
                Err(LineClientError::Cancelled) => return Err(LineClientError::Cancelled),
                Err(e) => {
                    eprintln!("[line] ws failed: {e}; backoff {backoff:?}");
                    if self.sleep_with_cancel(backoff).await {
                        return Err(LineClientError::Cancelled);
                    }
                    // Exponential backoff with a 60 s cap; jitter
                    // skipped for now (single client per user means
                    // no thundering herd).
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    }

    async fn connect_and_pump<S: LineEnvelopeSink>(
        &self,
        ws_url: &str,
        sink: &S,
    ) -> Result<(), LineClientError> {
        let (ws, resp) = connect_async(ws_url)
            .await
            .map_err(|e| LineClientError::Connect(e.to_string()))?;
        eprintln!("[line] ws connected (status {})", resp.status());
        let (mut sink_ws, mut stream_ws) = ws.split();

        loop {
            tokio::select! {
                _ = self.cancelled() => {
                    let _ = sink_ws.send(Message::Close(None)).await;
                    return Err(LineClientError::Cancelled);
                }
                msg = stream_ws.next() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        match parse_envelope(&text) {
                            WsIncoming::Envelope(env) => {
                                sink.on_envelope(env).await;
                            }
                            WsIncoming::Unknown(raw) => {
                                eprintln!("[line] unknown envelope: {raw}");
                            }
                            WsIncoming::Closed => return Ok(()),
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        if sink_ws.send(Message::Pong(p)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) => {
                        eprintln!("[line] ws received close");
                        return Ok(());
                    }
                    Some(Ok(_)) => { /* ignore binary/pong/etc */ }
                    Some(Err(e)) => {
                        return Err(LineClientError::Transport(e.to_string()));
                    }
                    None => return Ok(()),
                }
            }
        }
        #[allow(unreachable_code)]
        Ok(())
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|t| t.is_cancelled())
    }

    async fn cancelled(&self) {
        if let Some(t) = self.cancel.as_ref() {
            t.cancelled().await;
        } else {
            std::future::pending::<()>().await;
        }
    }

    /// Sleep `dur`, but return `true` early on cancellation.
    async fn sleep_with_cancel(&self, dur: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(dur) => false,
            _ = self.cancelled() => true,
        }
    }
}

fn parse_envelope(text: &str) -> WsIncoming {
    match serde_json::from_str::<WsEnvelope>(text) {
        Ok(env) => WsIncoming::Envelope(env),
        Err(_) => WsIncoming::Unknown(text.to_string()),
    }
}

/// Strip the JWT from a `wss://…/ws?token=<jwt>` URL for logs.
fn redact_token(ws_url: &str) -> String {
    if let Some(idx) = ws_url.find("token=") {
        let mut out = ws_url[..idx + "token=".len()].to_string();
        out.push_str("<redacted>");
        return out;
    }
    ws_url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_envelope_round_trips_user_message() {
        let raw = r#"{"kind":"user_message","text":"hi","reply_token":"rt","request_id":"r"}"#;
        match parse_envelope(raw) {
            WsIncoming::Envelope(WsEnvelope::UserMessage { text, .. }) => {
                assert_eq!(text, "hi");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_envelope_handles_unknown() {
        let raw = r#"{"kind":"future_kind_we_dont_know"}"#;
        match parse_envelope(raw) {
            WsIncoming::Unknown(s) => assert!(s.contains("future_kind")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn redact_token_strips_jwt() {
        let url = "wss://line.thclaws.ai/ws?token=abc.def.ghi";
        assert_eq!(
            redact_token(url),
            "wss://line.thclaws.ai/ws?token=<redacted>"
        );
    }

    #[test]
    fn redact_token_passes_through_when_no_token() {
        assert_eq!(
            redact_token("wss://line.thclaws.ai/ws"),
            "wss://line.thclaws.ai/ws"
        );
    }
}
