//! `MessengerApprover` — implements `ApprovalSink` by routing the
//! tool-approval prompt through Messenger quick replies.
//!
//! Mirrors [`crate::line::approver::LineApprover`] (dev-plan/31 reuses
//! the LINE design): on `approve(req)` it registers a `request_id`
//! against a `oneshot`, pushes a prompt with `[Approve] / [Deny]`
//! quick replies, and awaits the user's tap (or free-text reply).
//! Auto-denies on a 60 s timeout with a follow-up notice. The common
//! oneshot-map + timeout machinery lifts into `adapter::approver` when
//! dev-plan/30 Tier 0 matures the framework.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::permissions::{ApprovalDecision, ApprovalRequest, ApprovalSink};

use super::client::MessengerClient;
use super::protocol::QuickReply;

/// Cap on how long we wait before auto-denying. 60 s matches LINE.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// First N chars of a tool's input preview rendered in the prompt.
const INPUT_PREVIEW_CHARS: usize = 200;

/// What the user typed (or tapped) in response to an approval prompt.
/// Postback `payload` strings shape as `tool:<verb>:<req_id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalReply {
    Allow,
    Deny,
    /// User typed something we can't classify.
    Unrecognised,
}

impl ApprovalReply {
    /// Liberal parse of free-form text — case-insensitive, trimmed.
    pub fn parse_text(input: &str) -> Self {
        let t = input.trim().to_lowercase();
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

    /// Parse a quick-reply / postback `payload`. Accepts the canonical
    /// `tool:<verb>:<request_id>` and the shorter `<verb>:<request_id>`.
    pub fn parse_postback(payload: &str) -> (Self, Option<String>) {
        let parts: Vec<&str> = payload.split(':').collect();
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
pub struct MessengerApprover {
    /// `None` puts the approver in test mode — `record_decision_*`
    /// resolves pending approvals without any network traffic.
    client: Option<Arc<MessengerClient>>,
    pending: Arc<Mutex<Pending>>,
    timeout: Duration,
}

impl MessengerApprover {
    pub fn new(client: Arc<MessengerClient>) -> Self {
        Self {
            client: Some(client),
            pending: Arc::new(Mutex::new(Pending::default())),
            timeout: DEFAULT_TIMEOUT,
        }
    }

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
    pub fn has_pending(&self) -> bool {
        self.pending.lock().map(|p| p.has_any()).unwrap_or(false)
    }

    /// Number of approvals currently awaiting a reply. Surfaced to the
    /// GUI status pill.
    pub fn pending_count(&self) -> usize {
        self.pending.lock().map(|p| p.order.len()).unwrap_or(0)
    }

    /// Resolve the pending approval whose `request_id` matches.
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

    /// Free-text reply path — resolves the most-recent pending approval.
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

    /// Quick-reply / postback path.
    pub fn record_decision_from_postback(&self, payload: &str) -> Option<ApprovalReply> {
        let (reply, req_id) = ApprovalReply::parse_postback(payload);
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

    fn build_prompt(req: &ApprovalRequest) -> String {
        let input_str = serde_json::to_string(&req.input).unwrap_or_default();
        let preview: String = input_str.chars().take(INPUT_PREVIEW_CHARS).collect();
        let ellipsis = if input_str.chars().count() > INPUT_PREVIEW_CHARS {
            "…"
        } else {
            ""
        };
        format!(
            "🔐 thClaws wants to run: {tool}\n\nInput: {preview}{ellipsis}\n\nTap Approve or Deny (auto-denies in 60s).",
            tool = req.tool_name,
        )
    }

    /// Two quick replies whose `payload` matches what
    /// `ApprovalReply::parse_postback` expects.
    fn build_quick_replies(request_id: &str) -> Vec<QuickReply> {
        vec![
            QuickReply {
                title: "✅ Approve".into(),
                payload: format!("tool:allow:{request_id}"),
            },
            QuickReply {
                title: "🚫 Deny".into(),
                payload: format!("tool:deny:{request_id}"),
            },
        ]
    }
}

#[async_trait]
impl ApprovalSink for MessengerApprover {
    async fn approve(&self, req: &ApprovalRequest) -> ApprovalDecision {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(request_id.clone(), tx);
        }

        if let Some(client) = &self.client {
            let prompt = Self::build_prompt(req);
            let quick_replies = Self::build_quick_replies(&request_id);
            if let Err(e) = client.push_with_quick_replies(prompt, quick_replies).await {
                eprintln!("[messenger] approval prompt failed to send: {e}; auto-denying");
                self.record_decision_by_id(&request_id, ApprovalDecision::Deny);
                return ApprovalDecision::Deny;
            }
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_canceled)) => ApprovalDecision::Deny,
            Err(_elapsed) => {
                eprintln!(
                    "[messenger] approval for {} timed out after {:?}; auto-denying",
                    req.tool_name, self.timeout
                );
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
        let approver = MessengerApprover::for_test();
        let a = approver.clone();
        let handle = tokio::spawn(async move { a.approve(&req("Bash")).await });
        tokio::task::yield_now().await;
        assert!(approver.has_pending());
        let reply = approver.record_decision_from_text("approve");
        assert_eq!(reply, Some(ApprovalReply::Allow));
        assert_eq!(handle.await.unwrap(), ApprovalDecision::Allow);
    }

    #[tokio::test]
    async fn postback_with_request_id_resolves_specific_entry() {
        let approver = MessengerApprover::for_test();
        let a1 = approver.clone();
        let h1 = tokio::spawn(async move { a1.approve(&req("Bash")).await });
        tokio::task::yield_now().await;
        let a2 = approver.clone();
        let h2 = tokio::spawn(async move { a2.approve(&req("Edit")).await });
        tokio::task::yield_now().await;

        let ids: Vec<String> = {
            let p = approver.pending.lock().unwrap();
            p.order.clone()
        };
        assert_eq!(ids.len(), 2);

        let raw = format!("tool:allow:{}", ids[0]);
        assert_eq!(
            approver.record_decision_from_postback(&raw),
            Some(ApprovalReply::Allow)
        );
        assert_eq!(h1.await.unwrap(), ApprovalDecision::Allow);

        assert!(approver.has_pending());
        approver.record_decision_from_text("deny");
        assert_eq!(h2.await.unwrap(), ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn timeout_auto_denies() {
        let approver = MessengerApprover::for_test().with_timeout(Duration::from_millis(50));
        let a = approver.clone();
        assert_eq!(a.approve(&req("Bash")).await, ApprovalDecision::Deny);
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
