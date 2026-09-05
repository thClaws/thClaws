//! `LineApprover` — implements `ApprovalSink` by routing the
//! prompt through LINE.
//!
//! Plan-07 Phase 1.2. When `PermissionMode::LineGated` is active,
//! the agent's permission gate calls `LineApprover::approve(req)`
//! which:
//! 1. Registers a fresh `request_id` against a `oneshot::Sender`.
//! 2. Posts a plain-text approval prompt (tool name + preview of
//!    input + "reply approve / deny") back to the LINE chat via
//!    the relay's `POST /reply/{outbound_request_id}` endpoint,
//!    *or* — when no fresh `replyToken` is available — via the
//!    push API path the relay falls back to.
//! 3. Awaits the user's reply (text-based for Phase 1.2; Quick
//!    Reply buttons are a Phase 1.2.b protocol extension).
//! 4. On timeout (default 60 s) auto-denies + sends a follow-up
//!    notice so the user isn't left wondering.
//!
//! Phase 1.2 routes the user's text answer through
//! `LineSession::handle_message` which detects a pending approval
//! and forwards `record_decision_from_text` instead of running an
//! agent turn. The session also resolves postback envelopes via
//! `record_decision_from_postback` for the Phase 1.2.b Quick Reply
//! upgrade once it lands.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::permissions::{ApprovalDecision, ApprovalRequest, ApprovalSink};

use super::client::LineClient;
use super::protocol::QuickReplyButton;

/// Cap on how long we'll wait for the user to respond before
/// auto-denying. 60 s matches plan-07. Configurable in the future
/// via settings if it becomes annoying.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// First N chars of a tool's input preview rendered in LINE. Long
/// inputs are noisy; the user only needs enough to recognise what
/// the agent's about to do.
const INPUT_PREVIEW_CHARS: usize = 200;

/// What the user typed (or tapped) in response to an approval
/// prompt. Postback `data` strings shape as `tool:<verb>:<req_id>`
/// so the Phase 1.2.b Quick Reply upgrade lands without protocol
/// churn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalReply {
    Allow,
    Deny,
    /// User typed something we can't classify. Caller decides
    /// whether to re-prompt or treat as Deny.
    Unrecognised,
}

impl ApprovalReply {
    /// Liberal parse of free-form user text — case-insensitive,
    /// whitespace-trimmed, accepts common short forms. Anything
    /// else → `Unrecognised` so the caller can choose to re-prompt.
    pub fn parse_text(input: &str) -> Self {
        let t = input.trim().to_lowercase();
        // Keep the table small + decisive — fancier NLP belongs in
        // the model, not here.
        match t.as_str() {
            "y" | "yes" | "ok" | "approve" | "approved" | "allow" | "a" | "ใช่" | "อนุญาต" => {
                Self::Allow
            }
            "n" | "no" | "deny" | "denied" | "block" | "reject" | "d" | "ไม่" | "ปฏิเสธ" => {
                Self::Deny
            }
            _ => Self::Unrecognised,
        }
    }

    /// Parse a postback `data` field. Plan-07 shape is
    /// `tool:<verb>:<request_id>`; we accept that *and* the
    /// shorter `<verb>:<request_id>` so any future relay-side
    /// shorthand doesn't break us.
    pub fn parse_postback(data: &str) -> (Self, Option<String>) {
        let parts: Vec<&str> = data.split(':').collect();
        let (verb, req_id) = match parts.as_slice() {
            ["tool", verb, req] => (*verb, Some((*req).to_string())),
            [verb, req] => (*verb, Some((*req).to_string())),
            [verb] => (*verb, None),
            _ => return (Self::Unrecognised, None),
        };
        let decision = match verb.to_lowercase().as_str() {
            "allow" | "approve" | "yes" => Self::Allow,
            "deny" | "reject" | "no" => Self::Deny,
            _ => Self::Unrecognised,
        };
        (decision, req_id)
    }
}

#[derive(Default)]
struct Pending {
    /// All pending approvals keyed by request_id. The session
    /// resolves a postback by looking up its request_id directly;
    /// a free-text reply resolves the most-recent entry.
    by_id: HashMap<String, oneshot::Sender<ApprovalDecision>>,
    /// Insertion order — back is "most recent" for the text-reply
    /// fallback. `VecDeque` would also work; `Vec` keeps it small.
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
pub struct LineApprover {
    /// LINE client used to POST the approval prompt via
    /// `POST /reply/{request_id}` (or the push fallback in the
    /// relay). `None` lets the approver run in test mode where
    /// `record_decision_*` resolves pending approvals without any
    /// network traffic.
    client: Option<Arc<LineClient>>,
    pending: Arc<Mutex<Pending>>,
    timeout: Duration,
}

impl LineApprover {
    pub fn new(client: Arc<LineClient>) -> Self {
        Self {
            client: Some(client),
            pending: Arc::new(Mutex::new(Pending::default())),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Test-mode constructor — no network calls; the caller
    /// drives `approve` and `record_decision_*` directly.
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            client: None,
            pending: Arc::new(Mutex::new(Pending::default())),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, dur: Duration) -> Self {
        self.timeout = dur;
        self
    }

    /// True when at least one approval is waiting for a reply.
    /// `LineSession::handle_message` checks this to decide whether
    /// to treat an inbound text as an approval answer or as a new
    /// user turn.
    pub fn has_pending(&self) -> bool {
        self.pending.lock().map(|p| p.has_any()).unwrap_or(false)
    }

    /// Resolve the pending approval whose `request_id` matches.
    /// Returns `true` when a sender was found and notified; `false`
    /// when the id is unknown (already resolved, timed out, or
    /// just typoed in a postback string).
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

    /// Free-text reply path. Parses the user message into an
    /// `ApprovalReply`, resolves the most-recent pending approval
    /// if the parse succeeded, returns the verdict so the session
    /// can short-circuit `handle_message` instead of running an
    /// agent turn.
    pub fn record_decision_from_text(&self, text: &str) -> Option<ApprovalReply> {
        if !self.has_pending() {
            return None;
        }
        let reply = ApprovalReply::parse_text(text);
        let decision = match reply {
            ApprovalReply::Allow => ApprovalDecision::Allow,
            ApprovalReply::Deny => ApprovalDecision::Deny,
            ApprovalReply::Unrecognised => return Some(ApprovalReply::Unrecognised),
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

    /// Postback (Quick Reply) path. Phase 1.2.b — works today if
    /// the relay forwards the correct `data` shape.
    pub fn record_decision_from_postback(&self, data: &str) -> Option<ApprovalReply> {
        let (reply, req_id) = ApprovalReply::parse_postback(data);
        let decision = match reply {
            ApprovalReply::Allow => ApprovalDecision::Allow,
            ApprovalReply::Deny => ApprovalDecision::Deny,
            ApprovalReply::Unrecognised => return None,
        };
        let resolved = match req_id {
            Some(id) => self.record_decision_by_id(&id, decision),
            None => self
                .pending
                .lock()
                .ok()
                .and_then(|mut p| p.take_most_recent())
                .map(|tx| tx.send(decision).is_ok())
                .unwrap_or(false),
        };
        if resolved {
            Some(reply)
        } else {
            None
        }
    }

    /// Build the human-facing prompt text. Centralised so the
    /// Phase 1.2.b Quick Reply upgrade can reuse the same body
    /// while attaching tappable buttons.
    fn build_prompt(req: &ApprovalRequest) -> String {
        let input_str = serde_json::to_string(&req.input).unwrap_or_else(|_| String::new());
        let preview: String = input_str.chars().take(INPUT_PREVIEW_CHARS).collect();
        let ellipsis = if input_str.chars().count() > INPUT_PREVIEW_CHARS {
            "…"
        } else {
            ""
        };
        format!(
            "🔐 thClaws wants to run: {tool}\n\nInput: {preview}{ellipsis}\n\nTap Approve or Deny (auto-denies in 60s).",
            tool = req.tool_name,
            preview = preview,
            ellipsis = ellipsis,
        )
    }

    /// Two-button Quick Reply for the approval prompt. `data` shape
    /// matches what `ApprovalReply::parse_postback` expects, so the
    /// session resolves the right pending entry by `request_id`.
    fn build_buttons(request_id: &str) -> Vec<QuickReplyButton> {
        vec![
            QuickReplyButton {
                label: "✅ Approve".into(),
                data: format!("tool:allow:{request_id}"),
                display_text: Some("Approve".into()),
            },
            QuickReplyButton {
                label: "🚫 Deny".into(),
                data: format!("tool:deny:{request_id}"),
                display_text: Some("Deny".into()),
            },
        ]
    }
}

#[async_trait]
impl ApprovalSink for LineApprover {
    fn audit_kind(&self) -> &'static str {
        "bot:line"
    }

    async fn approve(&self, req: &ApprovalRequest) -> ApprovalDecision {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(request_id.clone(), tx);
        }

        // Plan-10: if the user has a browser chat open, route the
        // approval prompt there (free + rich-UI modal). Otherwise
        // fall back to LINE OA push (Quick Reply chips, quota-
        // using). Approval prompts are unsolicited — `/reply/:id`
        // would 404 either way, so we use `/push` (LINE OA) or
        // `/chat-bridge/event` (browser).
        if let Some(client) = &self.client {
            let prompt = Self::build_prompt(req);
            let buttons = Self::build_buttons(&request_id);

            if client.has_browser_connected().await {
                // Browser modal. Envelope shape matches what the
                // SPA's onmessage dispatch expects (see
                // static/chat.html `case 'approval_request'`).
                let envelope = serde_json::json!({
                    "type": "approval_request",
                    "id": request_id,
                    "tool_name": req.tool_name,
                    "prompt": prompt,
                    "timeout_secs": self.timeout.as_secs(),
                });
                if let Err(e) = client.push_chat_event(envelope).await {
                    eprintln!("[line] browser approval push failed: {e}; falling back to LINE OA");
                    if let Err(e) = client.push_with_buttons(prompt, buttons).await {
                        eprintln!("[line] LINE OA approval prompt ALSO failed: {e}; auto-denying");
                        self.record_decision_by_id(&request_id, ApprovalDecision::Deny);
                        return ApprovalDecision::Deny;
                    }
                }
            } else if let Err(e) = client.push_with_buttons(prompt, buttons).await {
                eprintln!("[line] approval prompt failed to send: {e}; auto-denying");
                self.record_decision_by_id(&request_id, ApprovalDecision::Deny);
                return ApprovalDecision::Deny;
            }
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_canceled)) => {
                // Sender dropped without sending. Treat as deny.
                ApprovalDecision::Deny
            }
            Err(_elapsed) => {
                eprintln!(
                    "[line] approval for {} timed out after {:?}; auto-denying",
                    req.tool_name, self.timeout
                );
                // Drop the pending entry so a late reply doesn't
                // resurrect an already-denied decision.
                if let Ok(mut pending) = self.pending.lock() {
                    let _ = pending.take_by_id(&request_id);
                }
                if let Some(client) = &self.client {
                    let _ = client
                        .push(format!(
                            "⏰ Approval for {} timed out; auto-denied.",
                            req.tool_name
                        ))
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
            input: json!({"path": "/tmp/x"}),
            summary: None,
            originator: crate::permissions::AgentOrigin::default(),
        }
    }

    #[tokio::test]
    async fn text_reply_resolves_pending_with_allow() {
        let approver = LineApprover::for_test();
        let a = approver.clone();
        let handle = tokio::spawn(async move { a.approve(&req("Bash")).await });
        // Give the approve future a tick to register pending.
        tokio::task::yield_now().await;
        assert!(approver.has_pending());
        let reply = approver.record_decision_from_text("approve");
        assert_eq!(reply, Some(ApprovalReply::Allow));
        let decision = handle.await.unwrap();
        assert_eq!(decision, ApprovalDecision::Allow);
    }

    #[tokio::test]
    async fn text_reply_resolves_pending_with_deny() {
        let approver = LineApprover::for_test();
        let a = approver.clone();
        let handle = tokio::spawn(async move { a.approve(&req("Edit")).await });
        tokio::task::yield_now().await;
        let reply = approver.record_decision_from_text("no");
        assert_eq!(reply, Some(ApprovalReply::Deny));
        assert_eq!(handle.await.unwrap(), ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn unrecognised_text_leaves_pending_for_retry() {
        let approver = LineApprover::for_test();
        let a = approver.clone();
        let handle = tokio::spawn(async move { a.approve(&req("Bash")).await });
        tokio::task::yield_now().await;
        let reply = approver.record_decision_from_text("maybe?");
        assert_eq!(reply, Some(ApprovalReply::Unrecognised));
        // Still pending — second-chance reply resolves it.
        assert!(approver.has_pending());
        approver.record_decision_from_text("yes");
        assert_eq!(handle.await.unwrap(), ApprovalDecision::Allow);
    }

    #[tokio::test]
    async fn postback_with_request_id_resolves_specific_entry() {
        let approver = LineApprover::for_test();
        // Two concurrent pending entries
        let a1 = approver.clone();
        let h1 = tokio::spawn(async move { a1.approve(&req("Bash")).await });
        tokio::task::yield_now().await;
        let a2 = approver.clone();
        let h2 = tokio::spawn(async move { a2.approve(&req("Edit")).await });
        tokio::task::yield_now().await;

        // Snapshot the two pending request_ids in insertion order.
        let ids: Vec<String> = {
            let p = approver.pending.lock().unwrap();
            p.order.clone()
        };
        assert_eq!(ids.len(), 2);

        // Resolve the FIRST entry by id — second remains pending.
        let raw = format!("tool:allow:{}", ids[0]);
        let reply = approver.record_decision_from_postback(&raw);
        assert_eq!(reply, Some(ApprovalReply::Allow));
        assert_eq!(h1.await.unwrap(), ApprovalDecision::Allow);

        // Second still pending — resolve via text.
        assert!(approver.has_pending());
        approver.record_decision_from_text("deny");
        assert_eq!(h2.await.unwrap(), ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn timeout_auto_denies() {
        let approver = LineApprover::for_test().with_timeout(Duration::from_millis(50));
        let a = approver.clone();
        let decision = a.approve(&req("Bash")).await;
        assert_eq!(decision, ApprovalDecision::Deny);
        // Pending entry must be cleared so a late reply doesn't
        // resurrect a different decision.
        assert!(!approver.has_pending());
    }

    #[test]
    fn parse_text_accepts_common_short_forms() {
        for s in ["yes", "Y", " approve ", "OK", "a", "allow"] {
            assert_eq!(ApprovalReply::parse_text(s), ApprovalReply::Allow, "{s}");
        }
        for s in ["no", "N", "deny", "reject", "d"] {
            assert_eq!(ApprovalReply::parse_text(s), ApprovalReply::Deny, "{s}");
        }
        for s in ["maybe", "later", "", "👍"] {
            assert_eq!(
                ApprovalReply::parse_text(s),
                ApprovalReply::Unrecognised,
                "{s}"
            );
        }
    }

    #[test]
    fn parse_postback_accepts_both_shapes() {
        let (r, id) = ApprovalReply::parse_postback("tool:allow:abc123");
        assert_eq!(r, ApprovalReply::Allow);
        assert_eq!(id.as_deref(), Some("abc123"));

        let (r, id) = ApprovalReply::parse_postback("deny:xyz789");
        assert_eq!(r, ApprovalReply::Deny);
        assert_eq!(id.as_deref(), Some("xyz789"));

        let (r, id) = ApprovalReply::parse_postback("garbage");
        assert_eq!(r, ApprovalReply::Unrecognised);
        assert!(id.is_none());
    }
}
