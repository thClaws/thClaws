//! Permission / approval infrastructure for tool execution.
//!
//! Design:
//! - [`PermissionMode`] in `AppConfig` picks the overall policy: `Auto` (never
//!   prompt), `Ask` (prompt whenever a tool's `requires_approval` returns true).
//! - Each [`Tool`][crate::tools::Tool] can override `requires_approval` to
//!   declare itself mutating. Read-only tools default to `false`.
//! - The agent loop consults the active mode + tool flag before calling, and
//!   asks an [`ApprovalSink`] for a decision if necessary. Sinks are pluggable:
//!   the REPL wires one that prompts on stdin, tests wire a scripted one.
//! - [`ApprovalDecision::AllowForSession`] is the "yolo" case — future calls
//!   from the same sink auto-approve. Tracking lives inside the sink so the
//!   agent just sees Allow/Deny.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    /// Never prompt; every tool call is auto-approved. Matches the pre-Phase-11
    /// behavior. Useful for non-interactive runs and tests.
    Auto,
    /// Prompt on any tool whose `requires_approval` returns true.
    Ask,
    /// Plan mode (M2+) — read-only exploration. Any tool whose
    /// `requires_approval` returns true is BLOCKED at dispatch with a
    /// structured tool_result telling the model "use Read/Grep/Glob, not
    /// Write/Edit/Bash; when ready, call SubmitPlan". The model self-
    /// corrects on the next turn. The user retains the sidebar Cancel
    /// button as a per-plan escape hatch.
    Plan,
}

impl Default for PermissionMode {
    fn default() -> Self {
        PermissionMode::Ask
    }
}

/// Process-wide current permission mode. Reads are dynamic so tools
/// that mutate the mode mid-turn (EnterPlanMode, ExitPlanMode, the
/// sidebar Approve button, the `/plan` slash command) take effect on
/// the very next tool dispatch — not on the next user message. The
/// agent loop consults `current_mode()` at each `requires_approval`
/// gate. Initialised by the worker at startup from config; cleared on
/// session swap.
fn current_mode_slot() -> &'static Mutex<PermissionMode> {
    static SLOT: std::sync::OnceLock<Mutex<PermissionMode>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(PermissionMode::default()))
}

/// Snapshot of the active mode. Cheap — just a Mutex read.
pub fn current_mode() -> PermissionMode {
    current_mode_slot()
        .lock()
        .map(|g| *g)
        .unwrap_or(PermissionMode::Ask)
}

/// Set the active mode. Used by `EnterPlanMode` / `ExitPlanMode`,
/// `/plan` slash command, sidebar Approve / Cancel, and the worker's
/// startup-from-config init.
pub fn set_current_mode(mode: PermissionMode) {
    if let Ok(mut g) = current_mode_slot().lock() {
        *g = mode;
    }
}

/// Stash for "the mode we were in before EnterPlanMode flipped us into
/// Plan". `ExitPlanMode` and the sidebar Cancel button pop this so the
/// user lands back where they were (Ask → Plan → Ask, not Ask → Plan
/// → Auto). `Some(mode)` only while a plan-mode session is active.
fn pre_plan_mode_slot() -> &'static Mutex<Option<PermissionMode>> {
    static SLOT: std::sync::OnceLock<Mutex<Option<PermissionMode>>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub fn stash_pre_plan_mode(mode: PermissionMode) {
    if let Ok(mut g) = pre_plan_mode_slot().lock() {
        *g = Some(mode);
    }
}

pub fn take_pre_plan_mode() -> Option<PermissionMode> {
    pre_plan_mode_slot().lock().ok().and_then(|mut g| g.take())
}

/// Broadcaster registered by the GUI worker — fires on every
/// `set_current_mode` so the sidebar / status pill reflects the
/// change live without polling. Same pattern as
/// `crate::tools::plan_state::set_broadcaster`.
type ModeBroadcaster = Box<dyn Fn(PermissionMode) + Send + Sync>;

fn broadcaster_slot() -> &'static Mutex<Option<ModeBroadcaster>> {
    static SLOT: std::sync::OnceLock<Mutex<Option<ModeBroadcaster>>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub fn set_mode_broadcaster<F>(f: F)
where
    F: Fn(PermissionMode) + Send + Sync + 'static,
{
    if let Ok(mut g) = broadcaster_slot().lock() {
        *g = Some(Box::new(f));
    }
}

fn fire_mode_changed(mode: PermissionMode) {
    if let Ok(g) = broadcaster_slot().lock() {
        if let Some(f) = g.as_ref() {
            f(mode);
        }
    }
}

/// Convenience: set + broadcast in one call. Most callers want both.
pub fn set_current_mode_and_broadcast(mode: PermissionMode) {
    set_current_mode(mode);
    fire_mode_changed(mode);
}

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool_name: String,
    pub input: Value,
    /// Optional short preview line the sink can show to the user.
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Approve this one call.
    Allow,
    /// Approve this call and every subsequent one from the same sink.
    AllowForSession,
    /// Deny. The agent surfaces this as a ToolResult with is_error=true.
    Deny,
}

#[async_trait]
pub trait ApprovalSink: Send + Sync {
    async fn approve(&self, req: &ApprovalRequest) -> ApprovalDecision;
}

/// Always-allow sink. Matches `PermissionMode::Auto` behavior but can also be
/// used directly when the mode is `Ask` but the caller wants a bypass.
pub struct AutoApprover;

#[async_trait]
impl ApprovalSink for AutoApprover {
    async fn approve(&self, _req: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Allow
    }
}

/// Always-deny sink, for tests.
pub struct DenyApprover;

#[async_trait]
impl ApprovalSink for DenyApprover {
    async fn approve(&self, _req: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Deny
    }
}

/// Scripted sink for integration tests. Plays back a queue of decisions.
/// `AllowForSession` flips an internal flag so subsequent calls auto-approve.
pub struct ScriptedApprover {
    decisions: std::sync::Mutex<std::collections::VecDeque<ApprovalDecision>>,
    session_allowed: AtomicBool,
}

impl ScriptedApprover {
    pub fn new(decisions: Vec<ApprovalDecision>) -> Arc<Self> {
        Arc::new(Self {
            decisions: std::sync::Mutex::new(decisions.into()),
            session_allowed: AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl ApprovalSink for ScriptedApprover {
    async fn approve(&self, _req: &ApprovalRequest) -> ApprovalDecision {
        if self.session_allowed.load(Ordering::Relaxed) {
            return ApprovalDecision::Allow;
        }
        let next = self
            .decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ApprovalDecision::Deny);
        if matches!(next, ApprovalDecision::AllowForSession) {
            self.session_allowed.store(true, Ordering::Relaxed);
            return ApprovalDecision::Allow;
        }
        next
    }
}

/// REPL-backed sink: prints a prompt on stdout and reads a line from stdin.
/// Supports `y/yes`, `n/no`, and `yolo` (= AllowForSession). Uses
/// `tokio::task::spawn_blocking` so the blocking I/O doesn't starve other tasks.
pub struct ReplApprover {
    session_allowed: AtomicBool,
}

impl ReplApprover {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            session_allowed: AtomicBool::new(false),
        })
    }
}

impl Default for ReplApprover {
    fn default() -> Self {
        Self {
            session_allowed: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl ApprovalSink for ReplApprover {
    async fn approve(&self, req: &ApprovalRequest) -> ApprovalDecision {
        if self.session_allowed.load(Ordering::Relaxed) {
            return ApprovalDecision::Allow;
        }
        let preview = req
            .summary
            .clone()
            .unwrap_or_else(|| serde_json::to_string(&req.input).unwrap_or_default());
        let prompt = format!(
            "\n\x1b[33m[approval] {} input={}\x1b[0m\n\x1b[90m[y]es / [n]o / yolo ▸ \x1b[0m",
            req.tool_name, preview
        );
        let answer = tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Write};
            let _ = std::io::stdout().write_all(prompt.as_bytes());
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            line.trim().to_lowercase()
        })
        .await
        .unwrap_or_default();

        match answer.as_str() {
            "y" | "yes" => ApprovalDecision::Allow,
            "yolo" => {
                self.session_allowed.store(true, Ordering::Relaxed);
                ApprovalDecision::Allow
            }
            _ => ApprovalDecision::Deny,
        }
    }
}

/// One pending approval request that the GUI event loop should forward
/// to the frontend. The id is how we pair the user's response (coming
/// back via IPC) with the oneshot responder waiting inside
/// [`GuiApprover::approve`].
#[derive(Debug, Clone, Serialize)]
pub struct GuiApprovalRequest {
    pub id: u64,
    pub tool_name: String,
    pub input: Value,
    pub summary: Option<String>,
}

/// Bridge between the agent's async `approve()` call and the GUI
/// webview. Each approval request:
///   1. registers a oneshot responder keyed by a fresh request id,
///   2. ships a [`GuiApprovalRequest`] over the outbound mpsc so the
///      event loop can render a modal in the frontend,
///   3. awaits the responder — the GUI event loop calls
///      [`GuiApprover::resolve`] when the user clicks a button.
///
/// `unresolved` also keeps the full request so the GUI forwarder can
/// re-dispatch periodically. Necessary because early-startup
/// dispatches can race the webview's React mount: `evaluate_script`
/// runs before `window.__thclaws_dispatch` is defined and the call
/// silently no-ops. Retrying until the user's response arrives
/// (identified by id) avoids that race without complicating the
/// frontend with a "ready" handshake.
pub struct GuiApprover {
    tx: mpsc::UnboundedSender<GuiApprovalRequest>,
    pending: Mutex<HashMap<u64, oneshot::Sender<ApprovalDecision>>>,
    unresolved: Mutex<HashMap<u64, GuiApprovalRequest>>,
    next_id: AtomicU64,
    session_allowed: AtomicBool,
}

impl GuiApprover {
    /// Returns the approver plus the receiver end the GUI event loop
    /// must drain (one request per forwarded frontend dispatch).
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<GuiApprovalRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let approver = Arc::new(Self {
            tx,
            pending: Mutex::new(HashMap::new()),
            unresolved: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            session_allowed: AtomicBool::new(false),
        });
        (approver, rx)
    }

    /// Satisfy the outstanding approve() call for `id`. Called by the
    /// GUI event loop when the user clicks Allow / AllowForSession /
    /// Deny in the approval modal.
    pub fn resolve(&self, id: u64, decision: ApprovalDecision) {
        if let Ok(mut u) = self.unresolved.lock() {
            u.remove(&id);
        }
        let responder = self.pending.lock().ok().and_then(|mut m| m.remove(&id));
        if let Some(responder) = responder {
            let _ = responder.send(decision);
        }
    }

    /// Snapshot of still-unresolved requests. The GUI forwarder polls
    /// this on a timer to redispatch anything the webview may have
    /// missed during its initial load.
    pub fn unresolved_requests(&self) -> Vec<GuiApprovalRequest> {
        self.unresolved
            .lock()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }
}

#[async_trait]
impl ApprovalSink for GuiApprover {
    async fn approve(&self, req: &ApprovalRequest) -> ApprovalDecision {
        if self.session_allowed.load(Ordering::Relaxed) {
            return ApprovalDecision::Allow;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (resp_tx, resp_rx) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, resp_tx);
        }
        let out = GuiApprovalRequest {
            id,
            tool_name: req.tool_name.clone(),
            input: req.input.clone(),
            summary: req.summary.clone(),
        };
        if let Ok(mut u) = self.unresolved.lock() {
            u.insert(id, out.clone());
        }
        if self.tx.send(out).is_err() {
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&id);
            }
            if let Ok(mut u) = self.unresolved.lock() {
                u.remove(&id);
            }
            return ApprovalDecision::Deny;
        }
        match resp_rx.await {
            Ok(ApprovalDecision::AllowForSession) => {
                self.session_allowed.store(true, Ordering::Relaxed);
                ApprovalDecision::Allow
            }
            Ok(d) => d,
            Err(_) => ApprovalDecision::Deny,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auto_approver_always_allows() {
        let a = AutoApprover;
        let req = ApprovalRequest {
            tool_name: "X".into(),
            input: serde_json::json!({}),
            summary: None,
        };
        assert_eq!(a.approve(&req).await, ApprovalDecision::Allow);
    }

    #[tokio::test]
    async fn deny_approver_always_denies() {
        let d = DenyApprover;
        let req = ApprovalRequest {
            tool_name: "X".into(),
            input: serde_json::json!({}),
            summary: None,
        };
        assert_eq!(d.approve(&req).await, ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn scripted_approver_plays_back_queue_and_defaults_to_deny() {
        let a = ScriptedApprover::new(vec![ApprovalDecision::Allow, ApprovalDecision::Deny]);
        let req = ApprovalRequest {
            tool_name: "T".into(),
            input: serde_json::json!({}),
            summary: None,
        };
        assert_eq!(a.approve(&req).await, ApprovalDecision::Allow);
        assert_eq!(a.approve(&req).await, ApprovalDecision::Deny);
        // Queue exhausted → defaults to Deny
        assert_eq!(a.approve(&req).await, ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn allow_for_session_sticks_after_first_call() {
        let a = ScriptedApprover::new(vec![ApprovalDecision::AllowForSession]);
        let req = ApprovalRequest {
            tool_name: "T".into(),
            input: serde_json::json!({}),
            summary: None,
        };
        // First call resolves AllowForSession → Allow (and sets the flag).
        assert_eq!(a.approve(&req).await, ApprovalDecision::Allow);
        // Subsequent calls auto-allow even though the queue is empty.
        assert_eq!(a.approve(&req).await, ApprovalDecision::Allow);
        assert_eq!(a.approve(&req).await, ApprovalDecision::Allow);
    }

    #[tokio::test]
    async fn gui_approver_round_trip() {
        let (approver, mut rx) = GuiApprover::new();
        let req = ApprovalRequest {
            tool_name: "Write".into(),
            input: serde_json::json!({"path": "foo.txt"}),
            summary: Some("Write to foo.txt".into()),
        };
        let approver_for_task = approver.clone();
        let call = tokio::spawn(async move { approver_for_task.approve(&req).await });
        let outbound = rx.recv().await.expect("request forwarded");
        assert_eq!(outbound.tool_name, "Write");
        approver.resolve(outbound.id, ApprovalDecision::Allow);
        assert_eq!(call.await.unwrap(), ApprovalDecision::Allow);
    }

    #[tokio::test]
    async fn gui_approver_allow_for_session_sticks() {
        let (approver, mut rx) = GuiApprover::new();
        let req = ApprovalRequest {
            tool_name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
            summary: None,
        };
        // First call: user picks AllowForSession → Allow + flag flips.
        let approver_c = approver.clone();
        let req_c = req.clone();
        let first = tokio::spawn(async move { approver_c.approve(&req_c).await });
        let outbound = rx.recv().await.unwrap();
        approver.resolve(outbound.id, ApprovalDecision::AllowForSession);
        assert_eq!(first.await.unwrap(), ApprovalDecision::Allow);
        // Second call: auto-allow without forwarding a new request.
        assert_eq!(approver.approve(&req).await, ApprovalDecision::Allow);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn gui_approver_denies_when_receiver_dropped() {
        let (approver, rx) = GuiApprover::new();
        drop(rx);
        let req = ApprovalRequest {
            tool_name: "Write".into(),
            input: serde_json::json!({}),
            summary: None,
        };
        assert_eq!(approver.approve(&req).await, ApprovalDecision::Deny);
    }

    #[test]
    fn permission_mode_default_is_ask() {
        assert_eq!(PermissionMode::default(), PermissionMode::Ask);
    }

    #[test]
    fn permission_mode_serde_lowercase() {
        assert_eq!(
            serde_json::to_string(&PermissionMode::Auto).unwrap(),
            "\"auto\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionMode::Ask).unwrap(),
            "\"ask\""
        );
    }
}
