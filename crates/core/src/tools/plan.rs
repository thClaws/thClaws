//! Plan mode tools: EnterPlanMode / ExitPlanMode + the structured-plan
//! tools `SubmitPlan` and `UpdatePlanStep` that drive the right-side
//! sidebar UI.
//!
//! `EnterPlanMode` / `ExitPlanMode` are hint-based — they return a
//! message that tells the model to switch behaviour. The structured
//! tools below own the actual plan state (see [`plan_state`]) and apply
//! Layer-1 transition gating so the model can't claim step N is
//! in_progress while step N-1 is still todo. The state module returns
//! human-readable error strings the tool surface echoes back as
//! `tool_result` content; the model reads them on its next turn and
//! self-corrects.

use super::plan_state::{PlanStep, StepStatus};
use super::{plan_state, Tool};
use crate::error::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

pub struct EnterPlanModeTool;

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn name(&self) -> &'static str {
        "EnterPlanMode"
    }
    fn description(&self) -> &'static str {
        "Enter plan mode for tasks that benefit from explicit planning before \
         action. While active, mutating tools (Write, Edit, Bash, document \
         editors) are BLOCKED — only read-only tools (Read, Grep, Glob, Ls) \
         work. Use the read-only tools to explore, then call SubmitPlan with \
         an ordered list of steps. The user reviews the plan in the right \
         sidebar and approves before execution begins. Use this when the user \
         asks for a plan, when the task is non-trivial, or when you'd benefit \
         from structuring a multi-step approach for review."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn call(&self, _input: Value) -> Result<String> {
        // Stash the prior mode so ExitPlanMode / sidebar Cancel can
        // restore it cleanly. Idempotent — re-entering plan mode while
        // already in it is a no-op for the stash (current mode is Plan,
        // pre-plan mode stays whatever it was before the first entry).
        let prior = crate::permissions::current_mode();
        if !matches!(prior, crate::permissions::PermissionMode::Plan) {
            crate::permissions::stash_pre_plan_mode(prior);
        }
        crate::permissions::set_current_mode_and_broadcast(
            crate::permissions::PermissionMode::Plan,
        );
        Ok(
            "Plan mode activated. Mutating tools are now blocked — use Read / \
             Grep / Glob / Ls to explore. When you have enough context, call \
             SubmitPlan with an ordered list of concrete, testable steps. The \
             user will review the plan in the right-side sidebar and approve \
             before execution begins."
                .into(),
        )
    }
}

pub struct ExitPlanModeTool;

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn name(&self) -> &'static str {
        "ExitPlanMode"
    }
    fn description(&self) -> &'static str {
        "Exit plan mode and resume normal tool dispatch. Typically called via \
         the sidebar Approve button after the user reviews a SubmitPlan-issued \
         plan; the model can also call it directly to abort plan mode without \
         submitting a plan. Restores whichever permission mode was active \
         before EnterPlanMode flipped us into Plan."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn call(&self, _input: Value) -> Result<String> {
        // Restore the prior mode, defaulting to `Ask` if nothing was
        // stashed (entering plan mode via /plan or sidebar without
        // a stash should still leave us in a sane state).
        let restored = crate::permissions::take_pre_plan_mode()
            .unwrap_or(crate::permissions::PermissionMode::Ask);
        crate::permissions::set_current_mode_and_broadcast(restored);
        Ok(format!(
            "Plan mode deactivated. Permission mode restored to {restored:?}. \
             Proceeding with execution — call UpdatePlanStep before / after each \
             step you work on so the user can track progress in the sidebar."
        ))
    }
}

/// Input shape for `SubmitPlan`. Steps don't carry a `status` because
/// every step starts as `Todo` — letting the model spell that out would
/// invite "the model said InProgress at submit time" race conditions.
/// `id` must be unique within the plan and stable across `UpdatePlanStep`
/// calls; the model can pick anything (`step-1`, `s0`, `parse-config`).
#[derive(Debug, Deserialize)]
struct SubmitPlanInput {
    steps: Vec<SubmitPlanStep>,
}

#[derive(Debug, Deserialize)]
struct SubmitPlanStep {
    id: String,
    title: String,
    #[serde(default)]
    description: String,
}

pub struct SubmitPlanTool;

#[async_trait]
impl Tool for SubmitPlanTool {
    fn name(&self) -> &'static str {
        "SubmitPlan"
    }
    fn description(&self) -> &'static str {
        "Publish a structured ordered plan to the user's right-side sidebar \
         for review and live progress tracking. Use this in plan mode when \
         you have enough context to lay out concrete steps. Each step needs \
         a stable `id`, a short `title`, and an optional `description`. \
         Re-calling SubmitPlan replaces the prior plan wholesale (your \
         channel for 'I changed my mind' is to submit a fresh plan, not \
         amend). After submission the sidebar shows Approve / Cancel \
         buttons; the user reviews, then either approves (you exit plan \
         mode and execute step-by-step, calling UpdatePlanStep per \
         transition) or cancels (plan cleared, normal mode restored). \
         Plans are strictly sequential — step N+1 cannot start until step \
         N is Done. For casual self-tracking that the user doesn't need \
         to watch live, use TodoWrite instead — outside plan mode."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "steps": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "description": "Stable, unique step id (e.g. \"step-1\")" },
                            "title": { "type": "string", "description": "Short imperative title (≤80 chars)" },
                            "description": { "type": "string", "description": "Optional longer detail" }
                        },
                        "required": ["id", "title"]
                    }
                }
            },
            "required": ["steps"]
        })
    }
    async fn call(&self, input: Value) -> Result<String> {
        let parsed: SubmitPlanInput = serde_json::from_value(input)
            .map_err(|e| crate::error::Error::Tool(format!("invalid input: {e}")))?;
        let steps = parsed
            .steps
            .into_iter()
            .map(|s| PlanStep {
                id: s.id,
                title: s.title,
                description: s.description,
                status: StepStatus::Todo,
                note: None,
                output: None,
            })
            .collect();
        match plan_state::submit(steps) {
            Ok(plan) => Ok(format!(
                "Plan submitted ({}). {} step(s) queued. Wait for the user to approve before \
                 starting execution. Once approved, call UpdatePlanStep(\"{}\", \"in_progress\") \
                 to begin step 1.",
                plan.id,
                plan.steps.len(),
                plan.steps[0].id,
            )),
            Err(e) => Err(crate::error::Error::Tool(e)),
        }
    }
}

#[derive(Debug, Deserialize)]
struct UpdatePlanStepInput {
    step_id: String,
    status: StepStatus,
    #[serde(default)]
    note: Option<String>,
    /// M6.3 cross-step data channel. Set on `done` transitions when
    /// later steps need to consume something this step produced — a
    /// generated id, a hash, a file path, a port number. Truncated to
    /// `plan_state::MAX_STEP_OUTPUT_LEN` bytes server-side.
    #[serde(default)]
    output: Option<String>,
}

pub struct UpdatePlanStepTool;

#[async_trait]
impl Tool for UpdatePlanStepTool {
    fn name(&self) -> &'static str {
        "UpdatePlanStep"
    }
    fn description(&self) -> &'static str {
        "Mark a step transition. Legal transitions: \
         todo→in_progress (only when the previous step is done), \
         todo→failed (\"blocked by upstream failure\" — REQUIRES a note \
         explaining why; use this when a prior step's failure makes the \
         current step infeasible to even attempt, e.g. \"step 2 failed; \
         no project files to edit\"), \
         in_progress→done, in_progress→failed (note recommended), \
         failed→in_progress (retry). Done steps cannot be re-opened — \
         submit a new plan if the approach has fundamentally changed. \
         An illegal transition returns an error string; read it and \
         correct on the next turn. \
         On `done`, optionally set `output` with a one-line value that \
         later steps need to consume (generated id, hash, path, port). \
         The chat history is compacted between steps, so `output` is \
         the explicit cross-step data channel — don't rely on prior \
         tool outputs surviving in context."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "step_id": { "type": "string", "description": "Step id from the active plan" },
                "status": {
                    "type": "string",
                    "enum": ["todo", "in_progress", "done", "failed"],
                    "description": "Target status"
                },
                "note": {
                    "type": "string",
                    "description": "Optional context — failure reason, progress detail. \
                                    Persisted on the step and shown in the sidebar."
                },
                "output": {
                    "type": "string",
                    "description": "Optional one-line value to expose to later steps (M6.3). \
                                    Use only on `done` transitions, only when later steps \
                                    actually need this data — generated ids, hashes, paths, \
                                    port numbers. Capped at ~1KB. Surfaced in the next \
                                    step's continuation prompt under \"Outputs from prior \
                                    steps\"."
                }
            },
            "required": ["step_id", "status"]
        })
    }
    async fn call(&self, input: Value) -> Result<String> {
        let parsed: UpdatePlanStepInput = serde_json::from_value(input)
            .map_err(|e| crate::error::Error::Tool(format!("invalid input: {e}")))?;
        // Stash output before the move into update_step swallows the struct.
        let output = parsed.output.clone();
        let was_done_transition = parsed.status == StepStatus::Done;
        match plan_state::update_step(&parsed.step_id, parsed.status, parsed.note) {
            Ok(plan) => {
                // M6.3: persist `output` after the status transition
                // succeeds. Only honored on `done` — outputs on other
                // transitions are noise (intermediate states don't
                // produce stable data for later steps).
                if was_done_transition {
                    if let Some(out) = output {
                        let _ = plan_state::set_step_output(&parsed.step_id, Some(out));
                    }
                }
                let idx = plan
                    .steps
                    .iter()
                    .position(|s| s.id == parsed.step_id)
                    .expect("just-updated step must exist");
                let total = plan.steps.len();
                let next_hint = if parsed.status == StepStatus::Done {
                    if let Some(next) = plan.steps.get(idx + 1) {
                        format!(
                            " Next step: \"{}\" — call UpdatePlanStep(\"{}\", \"in_progress\") to begin.",
                            next.title, next.id
                        )
                    } else {
                        " All steps complete. The plan is finished.".into()
                    }
                } else {
                    String::new()
                };
                Ok(format!(
                    "Step {} of {} ({:?}): \"{}\".{next_hint}",
                    idx + 1,
                    total,
                    parsed.status,
                    plan.steps[idx].title,
                ))
            }
            Err(e) => Err(crate::error::Error::Tool(e)),
        }
    }
}
