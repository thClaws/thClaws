//! Tier 3 GUI approval surface (dev-plan/32 Stage J/M follow-up).
//!
//! Mirrors `permissions::GuiApprover` for workflow scripts: the
//! shell-dispatch handler authors a script, emits a
//! `ViewEvent::WorkflowReviewRequest` for the frontend to render as
//! a review bubble, registers a oneshot keyed by the workflow id, and
//! awaits the user's decision. The IPC layer's `workflow_decision`
//! handler looks up the oneshot by id and resolves it when the user
//! clicks a button.
//!
//! `Cancel` is also returned when the receiver is dropped (e.g.,
//! session shutdown), so the handler doesn't need separate timeout
//! plumbing for the common case.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

#[derive(Debug, Clone)]
pub enum WorkflowDecision {
    Approve,
    Cancel,
    /// User wants the script regenerated with this note. The shell
    /// dispatcher loops on `Rework` by re-authoring + re-emitting a
    /// new review request, incrementing the `revision` counter so the
    /// frontend can show "Revision 2" / "Revision 3" labels.
    Rework(String),
}

pub struct WorkflowApprover {
    pending: Mutex<HashMap<String, oneshot::Sender<WorkflowDecision>>>,
}

impl Default for WorkflowApprover {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }
}

impl WorkflowApprover {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a oneshot for `id` and await the user's decision.
    /// Returns `WorkflowDecision::Cancel` when the channel was dropped
    /// without a decision (session shutdown, missing frontend, …) so
    /// the caller doesn't need a separate timeout path.
    pub async fn request(&self, id: String) -> WorkflowDecision {
        let (tx, rx) = oneshot::channel();
        if let Ok(mut p) = self.pending.lock() {
            p.insert(id, tx);
        }
        rx.await.unwrap_or(WorkflowDecision::Cancel)
    }

    /// Resolve the pending request for `id`. Returns `true` if a
    /// matching request existed, `false` otherwise — useful for the
    /// IPC handler so it can log spurious decision messages.
    pub fn resolve(&self, id: &str, decision: WorkflowDecision) -> bool {
        let tx = self.pending.lock().ok().and_then(|mut m| m.remove(id));
        if let Some(tx) = tx {
            let _ = tx.send(decision);
            true
        } else {
            false
        }
    }

    /// Snapshot of currently-pending review ids. Used by the chat
    /// input handler to detect "the user typed a decision while a
    /// review is open" so the Terminal tab can drive approvals via
    /// typed text the way `--cli` mode does.
    pub fn pending_ids(&self) -> Vec<String> {
        self.pending
            .lock()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }
}

/// Parse a typed chat input as a workflow decision. Recognises the
/// same shapes the `--cli` REPL accepts:
///   - "approve" / "a" / "yes" / "y" / "ok"
///   - "cancel" / "c" / "no" / "n" / "abort"
///   - "rework: <note>" / "r: <note>" / "rework <note>" / "r <note>"
/// Returns `None` for anything else — the caller emits a hint instead
/// of forwarding the text to the agent.
pub fn parse_chat_decision(text: &str) -> Option<WorkflowDecision> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();
    match lower.as_str() {
        "approve" | "a" | "yes" | "y" | "ok" => return Some(WorkflowDecision::Approve),
        "cancel" | "c" | "no" | "n" | "abort" => return Some(WorkflowDecision::Cancel),
        _ => {}
    }
    for prefix in ["rework:", "r:", "rework ", "r "] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            let note_lower_start = lower.len() - rest.len();
            let note = trimmed[note_lower_start..].trim().to_string();
            return Some(WorkflowDecision::Rework(note));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approves_when_resolved() {
        let approver = WorkflowApprover::new();
        let approver_clone = approver.clone();
        let join = tokio::spawn(async move { approver_clone.request("wf-1".to_string()).await });
        // Give the request a tick to enter the pending map.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(approver.resolve("wf-1", WorkflowDecision::Approve));
        match join.await.unwrap() {
            WorkflowDecision::Approve => {}
            other => panic!("expected Approve, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_when_channel_dropped() {
        let approver = WorkflowApprover::new();
        let approver_clone = approver.clone();
        let join = tokio::spawn(async move { approver_clone.request("wf-drop".to_string()).await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        // Drop the sender by clearing the map.
        if let Ok(mut p) = approver.pending.lock() {
            p.remove("wf-drop");
        }
        match join.await.unwrap() {
            WorkflowDecision::Cancel => {}
            other => panic!("expected Cancel on drop, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rework_carries_note() {
        let approver = WorkflowApprover::new();
        let approver_clone = approver.clone();
        let join = tokio::spawn(async move { approver_clone.request("wf-r".to_string()).await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(approver.resolve(
            "wf-r",
            WorkflowDecision::Rework("use Read not Bash".to_string())
        ));
        match join.await.unwrap() {
            WorkflowDecision::Rework(note) => assert_eq!(note, "use Read not Bash"),
            other => panic!("expected Rework, got {other:?}"),
        }
    }

    #[test]
    fn resolve_returns_false_for_unknown_id() {
        let approver = WorkflowApprover::new();
        assert!(!approver.resolve("nope", WorkflowDecision::Approve));
    }

    #[test]
    fn parse_decision_approves() {
        for s in ["approve", "Approve", "a", "A", "yes", "Y", "ok"] {
            assert!(matches!(
                parse_chat_decision(s),
                Some(WorkflowDecision::Approve)
            ));
        }
    }

    #[test]
    fn parse_decision_cancels() {
        for s in ["cancel", "Cancel", "c", "no", "n", "abort"] {
            assert!(matches!(
                parse_chat_decision(s),
                Some(WorkflowDecision::Cancel)
            ));
        }
    }

    #[test]
    fn parse_decision_rework_with_note() {
        match parse_chat_decision("rework: use the Read tool not Bash") {
            Some(WorkflowDecision::Rework(n)) => {
                assert_eq!(n, "use the Read tool not Bash");
            }
            other => panic!("expected Rework, got {other:?}"),
        }
        match parse_chat_decision("r: be terse") {
            Some(WorkflowDecision::Rework(n)) => assert_eq!(n, "be terse"),
            other => panic!("expected Rework, got {other:?}"),
        }
        match parse_chat_decision("rework drop the verification step") {
            Some(WorkflowDecision::Rework(n)) => {
                assert_eq!(n, "drop the verification step");
            }
            other => panic!("expected Rework, got {other:?}"),
        }
    }

    #[test]
    fn parse_decision_rejects_unrelated_text() {
        assert!(parse_chat_decision("what does this do?").is_none());
        assert!(parse_chat_decision("approval status").is_none());
        assert!(parse_chat_decision("").is_none());
        assert!(parse_chat_decision("   ").is_none());
    }
}
