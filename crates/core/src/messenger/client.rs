//! WebSocket client — connect to the relay, deserialise envelopes,
//! POST `/reply/<id>` (or `/push`) for outbound text. Reconnect with
//! exponential backoff.
//!
//! Mirrors [`crate::line::client::LineClient`]; the relay machinery
//! (WS frames, `/reply/:id`, `/push`) is shared. The differences are
//! Messenger-specific payload shapes (quick replies instead of LINE
//! Quick Reply chips) carried by [`super::protocol`].

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::config::MessengerConfig;
use super::protocol::{QuickReply, ReplyBody, WsEnvelope, WsIncoming};

#[derive(Debug, thiserror::Error)]
pub enum MessengerClientError {
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

/// Trait the session implements so the client can hand it envelopes.
/// Kept tiny so testing the client doesn't need an agent.
#[async_trait::async_trait]
pub trait MessengerEnvelopeSink: Send + Sync + 'static {
    async fn on_envelope(&self, envelope: WsEnvelope);
}

pub struct MessengerClient {
    config: MessengerConfig,
    http: reqwest::Client,
    cancel: Option<crate::cancel::CancelToken>,
}

impl MessengerClient {
    pub fn new(config: MessengerConfig) -> Self {
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

    /// Send a final agent reply for a given inbound message. The relay
    /// resolves the recipient PSID by `request_id` and calls the Graph
    /// API Send API with `messaging_type: RESPONSE`.
    pub async fn send_reply(
        &self,
        request_id: &str,
        text: impl Into<String>,
    ) -> Result<(), MessengerClientError> {
        self.send_reply_inner(
            request_id,
            ReplyBody {
                text: text.into(),
                quick_replies: None,
            },
        )
        .await
    }

    /// Reply with quick replies attached — a tap fires a `Postback`
    /// envelope back over the WS carrying the chip's `payload`.
    pub async fn send_reply_with_quick_replies(
        &self,
        request_id: &str,
        text: impl Into<String>,
        quick_replies: Vec<QuickReply>,
    ) -> Result<(), MessengerClientError> {
        self.send_reply_inner(
            request_id,
            ReplyBody {
                text: text.into(),
                quick_replies: Some(quick_replies),
            },
        )
        .await
    }

    /// Unsolicited push to the bound PSID — used for approval prompts
    /// and timeout notices, which have no inbound event to reply to.
    /// Subject to Messenger's 24-hour window on the relay side.
    pub async fn push(&self, text: impl Into<String>) -> Result<(), MessengerClientError> {
        self.push_inner(ReplyBody {
            text: text.into(),
            quick_replies: None,
        })
        .await
    }

    /// Push variant with quick replies, same wire shape as
    /// `send_reply_with_quick_replies`.
    pub async fn push_with_quick_replies(
        &self,
        text: impl Into<String>,
        quick_replies: Vec<QuickReply>,
    ) -> Result<(), MessengerClientError> {
        self.push_inner(ReplyBody {
            text: text.into(),
            quick_replies: Some(quick_replies),
        })
        .await
    }

    /// Tell the relay to drop our binding (GUI "Disconnect").
    /// Best-effort: a network failure doesn't block local cleanup.
    pub async fn unpair(&self) -> Result<(), MessengerClientError> {
        let url = self.config.unpair_url();
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.config.binding_token)
            .send()
            .await
            .map_err(|e| MessengerClientError::ReplyHttp(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(MessengerClientError::ReplyStatus { status, body });
        }
        Ok(())
    }

    async fn send_reply_inner(
        &self,
        request_id: &str,
        body: ReplyBody,
    ) -> Result<(), MessengerClientError> {
        let url = self.config.reply_url(request_id);
        self.post_json(&url, &body).await
    }

    async fn push_inner(&self, body: ReplyBody) -> Result<(), MessengerClientError> {
        let url = self.config.push_url();
        self.post_json(&url, &body).await
    }

    async fn post_json(&self, url: &str, body: &ReplyBody) -> Result<(), MessengerClientError> {
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.config.binding_token)
            .json(body)
            .send()
            .await
            .map_err(|e| MessengerClientError::ReplyHttp(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(MessengerClientError::ReplyStatus { status, body });
        }
        Ok(())
    }

    /// Run until cancelled. Opens the WS, dispatches envelopes to
    /// `sink`, reconnects with exponential backoff on transport error.
    pub async fn run<S: MessengerEnvelopeSink>(&self, sink: S) -> Result<(), MessengerClientError> {
        let ws_url = self.config.ws_url();
        eprintln!("[messenger] client starting → {}", redact_token(&ws_url));

        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(60);
        loop {
            if self.is_cancelled() {
                return Err(MessengerClientError::Cancelled);
            }
            match self.connect_and_pump(&ws_url, &sink).await {
                Ok(()) => {
                    eprintln!("[messenger] ws closed cleanly; reconnecting");
                    backoff = Duration::from_secs(1);
                    if self.sleep_with_cancel(Duration::from_secs(1)).await {
                        return Err(MessengerClientError::Cancelled);
                    }
                }
                Err(MessengerClientError::Cancelled) => {
                    return Err(MessengerClientError::Cancelled)
                }
                Err(e) => {
                    eprintln!("[messenger] ws failed: {e}; backoff {backoff:?}");
                    if self.sleep_with_cancel(backoff).await {
                        return Err(MessengerClientError::Cancelled);
                    }
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    }

    async fn connect_and_pump<S: MessengerEnvelopeSink>(
        &self,
        ws_url: &str,
        sink: &S,
    ) -> Result<(), MessengerClientError> {
        let (ws, resp) = connect_async(ws_url)
            .await
            .map_err(|e| MessengerClientError::Connect(e.to_string()))?;
        eprintln!("[messenger] ws connected (status {})", resp.status());
        let (mut sink_ws, mut stream_ws) = ws.split();

        loop {
            tokio::select! {
                _ = self.cancelled() => {
                    let _ = sink_ws.send(Message::Close(None)).await;
                    return Err(MessengerClientError::Cancelled);
                }
                msg = stream_ws.next() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        match parse_envelope(&text) {
                            WsIncoming::Envelope(env) => sink.on_envelope(env).await,
                            WsIncoming::Unknown(raw) => {
                                eprintln!("[messenger] unknown envelope: {raw}");
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
                        eprintln!("[messenger] ws received close");
                        return Ok(());
                    }
                    Some(Ok(_)) => { /* ignore binary/pong/etc */ }
                    Some(Err(e)) => return Err(MessengerClientError::Transport(e.to_string())),
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
        let raw = r#"{"kind":"messenger_user_message","text":"hi","psid":"P","request_id":"r"}"#;
        match parse_envelope(raw) {
            WsIncoming::Envelope(WsEnvelope::UserMessage { text, .. }) => assert_eq!(text, "hi"),
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
}
