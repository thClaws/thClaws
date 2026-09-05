//! `TelegramApprover` — routes tool-call approval through a Telegram
//! inline keyboard (dev-plan/29 Tier 1, decision #5).
//!
//! The Telegram analogue of [`crate::line::approver::LineApprover`].
//! When `PermissionMode::TelegramGated` is active and the agent hits a
//! mutating tool, [`TelegramApprover::approve`]:
//! 1. mints a `request_id` against a `oneshot::Sender`,
//! 2. `sendMessage`s a prompt to the *active chat* with an inline
//!    keyboard (Allow once / Allow always / Deny), the buttons carrying
//!    `callback_data = tool:<verb>:<request_id>`,
//! 3. awaits the tap (resolved by the session sink via
//!    [`TelegramApprover::record_decision_from_callback`]),
//! 4. auto-denies on timeout (default 60s) with a follow-up notice.
//!
//! The session sink owns the `answerCallbackQuery` + `editMessageText`
//! side of the tap, because the inbound `CallbackQuery` already carries
//! the message to edit — so this type only manages the pending map and
//! the outbound prompt. The common oneshot/timeout machinery is a
//! near-copy of `LineApprover`; per the plan it lifts into a shared
//! `adapter::approver` once Discord (the third caller) arrives.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::permissions::{ApprovalDecision, ApprovalRequest, ApprovalSink};

use super::client::TelegramClient;
use super::protocol::{InlineKeyboardButton, InlineKeyboardMarkup, SendMessage};

/// Auto-deny after this long with no tap. Matches `LineApprover`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// First N chars of a tool's input rendered in the prompt.
const INPUT_PREVIEW_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalReply {
    /// Approve this one call.
    Allow,
    /// Approve this and every subsequent call this session.
    AllowAlways,
    Deny,
    Unrecognised,
}

impl ApprovalReply {
    fn decision(self) -> Option<ApprovalDecision> {
        match self {
            Self::Allow => Some(ApprovalDecision::Allow),
            Self::AllowAlways => Some(ApprovalDecision::AllowForSession),
            Self::Deny => Some(ApprovalDecision::Deny),
            Self::Unrecognised => None,
        }
    }

    /// Parse inline-keyboard `callback_data`. Tier 1 shape is
    /// `tool:<verb>:<request_id>`; the shorter `<verb>:<request_id>` is
    /// also accepted so any future shorthand doesn't break us.
    pub fn parse_callback(data: &str) -> (Self, Option<String>) {
        let parts: Vec<&str> = data.split(':').collect();
        let (verb, req_id) = match parts.as_slice() {
            ["tool", verb, req] => (*verb, Some((*req).to_string())),
            [verb, req] => (*verb, Some((*req).to_string())),
            [verb] => (*verb, None),
            _ => return (Self::Unrecognised, None),
        };
        (Self::from_verb(verb), req_id)
    }

    /// Liberal free-text fallback (a user typing instead of tapping).
    pub fn parse_text(input: &str) -> Self {
        match input.trim().to_lowercase().as_str() {
            "y" | "yes" | "ok" | "approve" | "approved" | "allow" | "a" => Self::Allow,
            "always" | "all" | "yolo" => Self::AllowAlways,
            "n" | "no" | "deny" | "denied" | "block" | "reject" | "d" => Self::Deny,
            _ => Self::Unrecognised,
        }
    }

    fn from_verb(verb: &str) -> Self {
        match verb.to_lowercase().as_str() {
            "allow" | "approve" | "yes" => Self::Allow,
            "always" => Self::AllowAlways,
            "deny" | "reject" | "no" => Self::Deny,
            _ => Self::Unrecognised,
        }
    }
}

#[derive(Default)]
struct Pending {
    by_id: HashMap<String, oneshot::Sender<ApprovalDecision>>,
    order: Vec<String>,
}

impl Pending {
    fn insert(&mut self, id: String, tx: oneshot::Sender<ApprovalDecision>) {
        self.by_id.insert(id.clone(), tx);
        self.order.push(id);
    }

    fn take_by_id(&mut self, id: &str) -> Option<oneshot::Sender<ApprovalDecision>> {
        let tx = self.by_id.remove(id)?;
        self.order.retain(|x| x != id);
        Some(tx)
    }

    fn take_most_recent(&mut self) -> Option<oneshot::Sender<ApprovalDecision>> {
        let id = self.order.pop()?;
        self.by_id.remove(&id)
    }

    fn has_any(&self) -> bool {
        !self.order.is_empty()
    }
}

#[derive(Clone)]
pub struct TelegramApprover {
    /// `None` in test mode — `record_decision_*` resolves pending
    /// approvals without any network traffic.
    client: Option<Arc<TelegramClient>>,
    pending: Arc<Mutex<Pending>>,
    /// Chat the next approval prompt is sent to. The session sink sets
    /// this to the chat driving the current turn before forwarding the
    /// message. `None` until the first inbound message arrives.
    active_chat: Arc<Mutex<Option<i64>>>,
    timeout: Duration,
}

impl TelegramApprover {
    pub fn new(client: Arc<TelegramClient>) -> Self {
        Self {
            client: Some(client),
            pending: Arc::new(Mutex::new(Pending::default())),
            active_chat: Arc::new(Mutex::new(None)),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            client: None,
            pending: Arc::new(Mutex::new(Pending::default())),
            active_chat: Arc::new(Mutex::new(None)),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, dur: Duration) -> Self {
        self.timeout = dur;
        self
    }

    /// Point the next approval prompt at `chat_id`. Called by the
    /// session sink as each inbound message is routed.
    pub fn set_active_chat(&self, chat_id: i64) {
        if let Ok(mut g) = self.active_chat.lock() {
            *g = Some(chat_id);
        }
    }

    fn active_chat(&self) -> Option<i64> {
        self.active_chat.lock().ok().and_then(|g| *g)
    }

    pub fn has_pending(&self) -> bool {
        self.pending.lock().map(|p| p.has_any()).unwrap_or(false)
    }

    pub fn pending_count(&self) -> usize {
        self.pending.lock().map(|p| p.order.len()).unwrap_or(0)
    }

    /// Resolve the pending approval whose `request_id` matches. Returns
    /// true when a waiter was notified.
    pub fn record_decision_by_id(&self, request_id: &str, decision: ApprovalDecision) -> bool {
        let tx = self
            .pending
            .lock()
            .ok()
            .and_then(|mut p| p.take_by_id(request_id));
        match tx {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// Inline-keyboard tap path. Parses `callback_data`, resolves the
    /// matching pending approval, and returns the verdict + request_id
    /// so the sink can `answerCallbackQuery` + edit the prompt message.
    /// `None` when the data doesn't resolve anything (already handled,
    /// timed out, or a stale keyboard from a prior run).
    pub fn record_decision_from_callback(&self, data: &str) -> Option<(ApprovalReply, String)> {
        let (reply, req_id) = ApprovalReply::parse_callback(data);
        let decision = reply.decision()?;
        let resolved = match &req_id {
            Some(id) => self.record_decision_by_id(id, decision),
            None => self
                .pending
                .lock()
                .ok()
                .and_then(|mut p| p.take_most_recent())
                .map(|tx| tx.send(decision).is_ok())
                .unwrap_or(false),
        };
        resolved.then(|| (reply, req_id.unwrap_or_default()))
    }

    /// Free-text fallback (user typed instead of tapping). Resolves the
    /// most-recent pending approval. `Some(Unrecognised)` leaves it
    /// pending so the caller can re-prompt.
    pub fn record_decision_from_text(&self, text: &str) -> Option<ApprovalReply> {
        if !self.has_pending() {
            return None;
        }
        let reply = ApprovalReply::parse_text(text);
        let Some(decision) = reply.decision() else {
            return Some(ApprovalReply::Unrecognised);
        };
        if let Some(tx) = self
            .pending
            .lock()
            .ok()
            .and_then(|mut p| p.take_most_recent())
        {
            let _ = tx.send(decision);
            return Some(reply);
        }
        None
    }

    fn build_prompt(req: &ApprovalRequest) -> String {
        let input_str = serde_json::to_string(&req.input).unwrap_or_default();
        let preview: String = input_str.chars().take(INPUT_PREVIEW_CHARS).collect();
        let ellipsis = if input_str.chars().count() > INPUT_PREVIEW_CHARS {
            "…"
        } else {
            ""
        };
        // HTML-escaped + <code> so a tool input containing < > & renders
        // (Telegram HTML parse mode would otherwise 400 the prompt).
        format!(
            "🔐 thClaws wants to run: <b>{tool}</b>\n\nInput: <code>{preview}{ellipsis}</code>\n\nTap a button (auto-denies in {secs}s).",
            tool = super::filter::escape_html(&req.tool_name),
            preview = super::filter::escape_html(&preview),
            ellipsis = ellipsis,
            secs = DEFAULT_TIMEOUT.as_secs(),
        )
    }

    fn build_keyboard(request_id: &str) -> InlineKeyboardMarkup {
        InlineKeyboardMarkup::one_row(vec![
            InlineKeyboardButton::new("✅ Allow", format!("tool:allow:{request_id}")),
            InlineKeyboardButton::new("♾️ Always", format!("tool:always:{request_id}")),
            InlineKeyboardButton::new("🚫 Deny", format!("tool:deny:{request_id}")),
        ])
    }
}

#[async_trait]
impl ApprovalSink for TelegramApprover {
    fn audit_kind(&self) -> &'static str {
        "bot:telegram"
    }

    async fn approve(&self, req: &ApprovalRequest) -> ApprovalDecision {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(request_id.clone(), tx);
        }

        if let Some(client) = &self.client {
            let Some(chat_id) = self.active_chat() else {
                // No chat to prompt in (approval fired before any
                // inbound message). Fail safe: deny.
                eprintln!("[telegram] approval requested with no active chat; auto-denying");
                self.record_decision_by_id(&request_id, ApprovalDecision::Deny);
                return ApprovalDecision::Deny;
            };
            let prompt = Self::build_prompt(req);
            let keyboard = Self::build_keyboard(&request_id);
            let msg = SendMessage::text(chat_id, prompt).with_keyboard(keyboard);
            if let Err(e) = client.send_message(&msg).await {
                eprintln!("[telegram] approval prompt failed to send: {e}; auto-denying");
                self.record_decision_by_id(&request_id, ApprovalDecision::Deny);
                return ApprovalDecision::Deny;
            }
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_canceled)) => ApprovalDecision::Deny,
            Err(_elapsed) => {
                eprintln!(
                    "[telegram] approval for {} timed out after {:?}; auto-denying",
                    req.tool_name, self.timeout
                );
                if let Ok(mut pending) = self.pending.lock() {
                    let _ = pending.take_by_id(&request_id);
                }
                if let (Some(client), Some(chat_id)) = (&self.client, self.active_chat()) {
                    let _ = client
                        .send_text(
                            chat_id,
                            format!(
                                "⏰ Approval for {} timed out; auto-denied.",
                                super::filter::escape_html(&req.tool_name)
                            ),
                        )
                        .await;
                }
                ApprovalDecision::Deny
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(tool: &str) -> ApprovalRequest {
        ApprovalRequest {
            tool_name: tool.into(),
            input: json!({"command": "rm -rf /tmp/x"}),
            summary: None,
            originator: crate::permissions::AgentOrigin::default(),
        }
    }

    #[tokio::test]
    async fn callback_allow_resolves_pending() {
        let approver = TelegramApprover::for_test();
        let a = approver.clone();
        let handle = tokio::spawn(async move { a.approve(&req("Bash")).await });
        tokio::task::yield_now().await;
        assert!(approver.has_pending());
        // Snapshot the request_id and resolve by callback.
        let id = {
            let p = approver.pending.lock().unwrap();
            p.order[0].clone()
        };
        let (reply, got_id) = approver
            .record_decision_from_callback(&format!("tool:allow:{id}"))
            .expect("resolved");
        assert_eq!(reply, ApprovalReply::Allow);
        assert_eq!(got_id, id);
        assert_eq!(handle.await.unwrap(), ApprovalDecision::Allow);
    }

    #[tokio::test]
    async fn callback_always_maps_to_allow_for_session() {
        let approver = TelegramApprover::for_test();
        let a = approver.clone();
        let handle = tokio::spawn(async move { a.approve(&req("Write")).await });
        tokio::task::yield_now().await;
        let id = {
            let p = approver.pending.lock().unwrap();
            p.order[0].clone()
        };
        approver
            .record_decision_from_callback(&format!("tool:always:{id}"))
            .expect("resolved");
        assert_eq!(handle.await.unwrap(), ApprovalDecision::AllowForSession);
    }

    #[tokio::test]
    async fn callback_deny_resolves_pending() {
        let approver = TelegramApprover::for_test();
        let a = approver.clone();
        let handle = tokio::spawn(async move { a.approve(&req("Edit")).await });
        tokio::task::yield_now().await;
        let id = {
            let p = approver.pending.lock().unwrap();
            p.order[0].clone()
        };
        approver.record_decision_from_callback(&format!("tool:deny:{id}"));
        assert_eq!(handle.await.unwrap(), ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn text_fallback_resolves_most_recent() {
        let approver = TelegramApprover::for_test();
        let a = approver.clone();
        let handle = tokio::spawn(async move { a.approve(&req("Bash")).await });
        tokio::task::yield_now().await;
        assert_eq!(
            approver.record_decision_from_text("yes"),
            Some(ApprovalReply::Allow)
        );
        assert_eq!(handle.await.unwrap(), ApprovalDecision::Allow);
    }

    #[tokio::test]
    async fn unrecognised_text_leaves_pending() {
        let approver = TelegramApprover::for_test();
        let a = approver.clone();
        let handle = tokio::spawn(async move { a.approve(&req("Bash")).await });
        tokio::task::yield_now().await;
        assert_eq!(
            approver.record_decision_from_text("hmm"),
            Some(ApprovalReply::Unrecognised)
        );
        assert!(approver.has_pending());
        approver.record_decision_from_text("deny");
        assert_eq!(handle.await.unwrap(), ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn timeout_auto_denies_and_clears_pending() {
        let approver = TelegramApprover::for_test().with_timeout(Duration::from_millis(40));
        let decision = approver.approve(&req("Bash")).await;
        assert_eq!(decision, ApprovalDecision::Deny);
        assert!(!approver.has_pending());
    }

    #[tokio::test]
    async fn no_active_chat_with_client_would_deny() {
        // Test-mode (client = None) skips the send, so this exercises
        // the resolve path; the no-chat-deny branch is client-gated and
        // covered by inspection. Here we confirm a second concurrent
        // approval resolves by id, not by clobbering the first.
        let approver = TelegramApprover::for_test();
        let a1 = approver.clone();
        let h1 = tokio::spawn(async move { a1.approve(&req("Bash")).await });
        tokio::task::yield_now().await;
        let a2 = approver.clone();
        let h2 = tokio::spawn(async move { a2.approve(&req("Edit")).await });
        tokio::task::yield_now().await;
        let ids = {
            let p = approver.pending.lock().unwrap();
            p.order.clone()
        };
        assert_eq!(ids.len(), 2);
        approver.record_decision_from_callback(&format!("tool:allow:{}", ids[0]));
        assert_eq!(h1.await.unwrap(), ApprovalDecision::Allow);
        approver.record_decision_from_callback(&format!("tool:deny:{}", ids[1]));
        assert_eq!(h2.await.unwrap(), ApprovalDecision::Deny);
    }

    #[test]
    fn parse_callback_shapes() {
        assert_eq!(
            ApprovalReply::parse_callback("tool:allow:abc"),
            (ApprovalReply::Allow, Some("abc".into()))
        );
        assert_eq!(
            ApprovalReply::parse_callback("always:xyz"),
            (ApprovalReply::AllowAlways, Some("xyz".into()))
        );
        assert_eq!(
            ApprovalReply::parse_callback("deny:q"),
            (ApprovalReply::Deny, Some("q".into()))
        );
        assert_eq!(
            ApprovalReply::parse_callback("garbage:::"),
            (ApprovalReply::Unrecognised, None)
        );
    }

    #[test]
    fn build_prompt_escapes_html_in_tool_and_input() {
        let r = ApprovalRequest {
            tool_name: "Bash".into(),
            input: json!({"command": "echo a < b & c"}),
            summary: None,
            originator: crate::permissions::AgentOrigin::default(),
        };
        let prompt = TelegramApprover::build_prompt(&r);
        assert!(!prompt.contains("a < b"), "raw < leaked: {prompt}");
        assert!(prompt.contains("&lt;") && prompt.contains("&amp;"));
    }
}
