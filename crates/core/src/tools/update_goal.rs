//! M6.29 + Phase C1: model-callable tools for /goal lifecycle.
//!
//! Authority is split across three tools — one for non-terminal audit
//! checkpoints, two for the terminal transitions. This mirrors codex's
//! `update_goal` design (which separates create/update by intent) and
//! makes it harder for the model to slip into "mark complete to escape
//! the loop" — `MarkGoalCompleteTool` requires an audit string and is
//! distinct from the routine progress checkpoint, so the model can't
//! accidentally fire it.
//!
//!   RecordGoalProgressTool  → status stays Active, just stashes audit
//!   MarkGoalCompleteTool    → status → Complete  (audit required)
//!   MarkGoalBlockedTool     → status → Blocked   (reason required)
//!
//! All three live-mutate the global goal state via
//! `crate::goal_state::apply`, which fires the broadcaster (worker
//! subscribes → persists snapshot to session JSONL + auto-stops the
//! loop on terminal status + refreshes the goal sidebar).
//!
//! Approval: not required. Each call is a small in-memory state change;
//! the worker validates that a goal is actually active before allowing
//! the call to take effect. If no goal is active the call returns an
//! explanatory error — same shape as KmsRead against an unknown KMS.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

// ── Helpers ───────────────────────────────────────────────────────────

/// Validate a goal is active and capture `now` for terminal transitions.
/// Returns `(now_secs)` so terminal callers can stamp `completed_at`.
fn require_active_goal() -> Result<u64> {
    if crate::goal_state::current().is_none() {
        return Err(Error::Tool(
            "no active goal — call /goal start first".into(),
        ));
    }
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0))
}

// ── RecordGoalProgressTool — non-terminal checkpoint ─────────────────

pub struct RecordGoalProgressTool;

#[async_trait]
impl Tool for RecordGoalProgressTool {
    fn name(&self) -> &'static str {
        "RecordGoalProgress"
    }

    fn description(&self) -> &'static str {
        "Record an audit checkpoint on the active /goal without marking it \
         terminal. Status stays Active and the next /goal continue iteration \
         will fire normally. Use this mid-loop to capture what was just \
         verified — the summary is carried into future iterations as the \
         `prior_audit` hint so they don't re-audit from scratch. Cannot \
         mark the goal complete or blocked (use MarkGoalComplete / \
         MarkGoalBlocked for those)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "audit": {
                    "type": "string",
                    "description": "Short summary of the audit: what was checked, what evidence was found. Carried into future iterations as the prior_audit hint."
                }
            },
            "required": ["audit"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        false
    }

    async fn call(&self, input: Value) -> Result<String> {
        let audit = req_str(&input, "audit")?.to_string();
        require_active_goal()?;
        let changed = crate::goal_state::apply(|g| {
            g.last_audit = Some(audit.clone());
            true
        });
        if !changed {
            return Err(Error::Tool("goal state apply failed".into()));
        }
        Ok("goal progress recorded (status stays active)".into())
    }
}

// ── MarkGoalCompleteTool — terminal Complete ─────────────────────────

pub struct MarkGoalCompleteTool;

#[async_trait]
impl Tool for MarkGoalCompleteTool {
    fn name(&self) -> &'static str {
        "MarkGoalComplete"
    }

    fn description(&self) -> &'static str {
        "Mark the active /goal as COMPLETE. Call ONLY after running the \
         completion audit and verifying every requirement against actual \
         current state (files, command output, test results). Requires an \
         `audit` summary documenting what was checked + the evidence. \
         Optionally include a `reason` summary surfaced to the user. \
         Ending the loop on insufficient evidence is the worst failure \
         mode — when uncertain, use RecordGoalProgress to checkpoint and \
         keep working instead."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "audit": {
                    "type": "string",
                    "description": "Required. Summary of the completion audit: what requirements were checked, what evidence confirms each one. Stored as last_audit for the persistence layer."
                },
                "reason": {
                    "type": "string",
                    "description": "Optional. Short user-facing message about why the goal is being marked complete (surfaced in the sidebar)."
                }
            },
            "required": ["audit"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        false
    }

    async fn call(&self, input: Value) -> Result<String> {
        let audit = req_str(&input, "audit")?.to_string();
        let reason = input
            .get("reason")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        let now = require_active_goal()?;
        let changed = crate::goal_state::apply(|g| {
            g.status = crate::goal_state::GoalStatus::Complete;
            g.last_audit = Some(audit.clone());
            if let Some(r) = &reason {
                g.last_message = Some(r.clone());
            }
            g.completed_at = Some(now);
            true
        });
        if !changed {
            return Err(Error::Tool("goal state apply failed".into()));
        }
        Ok(format!(
            "goal marked complete{}",
            if reason.is_some() {
                " (reason recorded)"
            } else {
                ""
            }
        ))
    }
}

// ── MarkGoalBlockedTool — terminal Blocked ───────────────────────────

pub struct MarkGoalBlockedTool;

#[async_trait]
impl Tool for MarkGoalBlockedTool {
    fn name(&self) -> &'static str {
        "MarkGoalBlocked"
    }

    fn description(&self) -> &'static str {
        "Mark the active /goal as BLOCKED — work cannot continue \
         productively without external input (missing API key, ambiguous \
         spec, requires user decision, external dependency unavailable). \
         The loop pauses; the user reads the `reason` and either resolves \
         the blocker + runs /goal continue manually or runs /goal abandon. \
         Requires a `reason` describing what's needed. Optional `audit` \
         summarizes work done before hitting the blocker."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "Required. What's blocking progress, what input or decision is needed from the user."
                },
                "audit": {
                    "type": "string",
                    "description": "Optional. Summary of work completed before the blocker — useful when resuming."
                }
            },
            "required": ["reason"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        false
    }

    async fn call(&self, input: Value) -> Result<String> {
        let reason = req_str(&input, "reason")?.to_string();
        let audit = input
            .get("audit")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        let now = require_active_goal()?;
        let changed = crate::goal_state::apply(|g| {
            g.status = crate::goal_state::GoalStatus::Blocked;
            g.last_message = Some(reason.clone());
            if let Some(a) = &audit {
                g.last_audit = Some(a.clone());
            }
            g.completed_at = Some(now);
            true
        });
        if !changed {
            return Err(Error::Tool("goal state apply failed".into()));
        }
        Ok(format!(
            "goal marked blocked{}",
            if audit.is_some() {
                " (audit recorded)"
            } else {
                ""
            }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal_state::{self, GoalState, GoalStatus};

    /// Tests serialize on the global goal state — share a mutex.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset() {
        goal_state::set(None);
    }

    #[tokio::test]
    async fn record_progress_keeps_active_and_stores_audit() {
        let _g = lock();
        reset();
        goal_state::set(Some(GoalState::new("ship X".into(), None, None, false)));
        let r = RecordGoalProgressTool
            .call(json!({"audit": "parser done; type-checker pending"}))
            .await
            .unwrap();
        assert!(r.contains("active"));
        let g = goal_state::current().unwrap();
        assert_eq!(g.status, GoalStatus::Active);
        assert_eq!(
            g.last_audit.as_deref(),
            Some("parser done; type-checker pending")
        );
        assert!(g.completed_at.is_none());
        reset();
    }

    #[tokio::test]
    async fn record_progress_requires_audit() {
        let _g = lock();
        reset();
        goal_state::set(Some(GoalState::new("ship X".into(), None, None, false)));
        let err = RecordGoalProgressTool.call(json!({})).await.unwrap_err();
        assert!(format!("{err}").contains("audit"));
        reset();
    }

    #[tokio::test]
    async fn mark_complete_transitions_terminal_with_audit() {
        let _g = lock();
        reset();
        goal_state::set(Some(GoalState::new("ship X".into(), None, None, false)));
        let r = MarkGoalCompleteTool
            .call(json!({
                "audit": "all 5 spec items verified against test output",
                "reason": "shipped"
            }))
            .await
            .unwrap();
        assert!(r.contains("complete"));
        let g = goal_state::current().unwrap();
        assert_eq!(g.status, GoalStatus::Complete);
        assert!(g.completed_at.is_some());
        assert_eq!(g.last_message.as_deref(), Some("shipped"));
        assert!(g.last_audit.as_deref().unwrap().contains("verified"));
        reset();
    }

    #[tokio::test]
    async fn mark_complete_requires_audit_field() {
        let _g = lock();
        reset();
        goal_state::set(Some(GoalState::new("ship X".into(), None, None, false)));
        let err = MarkGoalCompleteTool
            .call(json!({"reason": "feels done"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("audit"));
        // Status must NOT have changed.
        assert_eq!(goal_state::current().unwrap().status, GoalStatus::Active);
        reset();
    }

    #[tokio::test]
    async fn mark_blocked_transitions_terminal_with_reason() {
        let _g = lock();
        reset();
        goal_state::set(Some(GoalState::new("ship X".into(), None, None, false)));
        MarkGoalBlockedTool
            .call(json!({"reason": "need API key from user"}))
            .await
            .unwrap();
        let g = goal_state::current().unwrap();
        assert_eq!(g.status, GoalStatus::Blocked);
        assert_eq!(g.last_message.as_deref(), Some("need API key from user"));
        assert!(g.completed_at.is_some());
        reset();
    }

    #[tokio::test]
    async fn mark_blocked_requires_reason() {
        let _g = lock();
        reset();
        goal_state::set(Some(GoalState::new("ship X".into(), None, None, false)));
        let err = MarkGoalBlockedTool.call(json!({})).await.unwrap_err();
        assert!(format!("{err}").contains("reason"));
        reset();
    }

    #[tokio::test]
    async fn all_three_tools_error_when_no_active_goal() {
        let _g = lock();
        reset();
        for err in [
            RecordGoalProgressTool
                .call(json!({"audit": "x"}))
                .await
                .unwrap_err(),
            MarkGoalCompleteTool
                .call(json!({"audit": "x"}))
                .await
                .unwrap_err(),
            MarkGoalBlockedTool
                .call(json!({"reason": "x"}))
                .await
                .unwrap_err(),
        ] {
            assert!(
                format!("{err}").contains("no active goal"),
                "expected 'no active goal' error, got: {err}"
            );
        }
    }

    #[test]
    fn none_require_approval() {
        assert!(!RecordGoalProgressTool.requires_approval(&json!({})));
        assert!(!MarkGoalCompleteTool.requires_approval(&json!({})));
        assert!(!MarkGoalBlockedTool.requires_approval(&json!({})));
    }
}
