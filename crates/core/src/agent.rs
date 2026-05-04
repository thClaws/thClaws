//! Agent loop: ties providers + tools + context + compaction together.
//!
//! `Agent::run_turn(user_msg)` returns a live stream of [`AgentEvent`]s that
//! the REPL/UI consumes as the turn unfolds. The loop:
//!
//! 1. Append user message to history.
//! 2. Compact history if over token budget.
//! 3. Call `provider.stream()` → `assemble` → drain events, collecting
//!    streaming text (yielded as `AgentEvent::Text`) and complete tool_use
//!    blocks.
//! 4. Persist the assistant message (text + tool_use blocks).
//! 5. If any tool_use blocks: execute each via the registry, persist a user
//!    message with the tool_result blocks, then loop back to step 3.
//! 6. Otherwise: yield `AgentEvent::Done` and return.
//!
//! A `max_iterations` cap prevents runaway tool-call loops.

use crate::compaction::compact;
use crate::error::{Error, Result};
use crate::permissions::{
    ApprovalDecision, ApprovalRequest, ApprovalSink, AutoApprover, PermissionMode,
};
use crate::providers::{assemble, AssembledEvent, Provider, StreamRequest, Usage};
use crate::tools::ToolRegistry;
use crate::types::{ContentBlock, Message, Role};
use async_stream::try_stream;
use futures::{Stream, StreamExt};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Emitted at the start of each provider iteration (0-indexed).
    IterationStart { iteration: usize },
    /// A chunk of assistant text — for live streaming.
    Text(String),
    /// Tool is about to be called.
    ToolCallStart {
        id: String,
        name: String,
        input: Value,
    },
    /// Tool finished. `output` uses `std::result::Result<String, String>` so
    /// the event stays `Clone` (our `crate::Error` isn't `Clone`).
    ToolCallResult {
        id: String,
        name: String,
        output: std::result::Result<String, String>,
        /// MCP-Apps widget the chat surface should embed inline below
        /// this tool's text result. `Some` only when the tool's
        /// upstream MCP server declared a `ui.resourceUri` and the
        /// resource fetch succeeded. Plain tools (Read, Bash, …)
        /// always have `None`.
        ui_resource: Option<crate::tools::UiResource>,
    },
    /// Tool was denied by the approver. No call was made.
    ToolCallDenied { id: String, name: String },
    /// Turn is complete. No further events follow.
    Done {
        stop_reason: Option<String>,
        usage: Usage,
    },
}

/// Build the dynamic plan-mode reminder appended to the system prompt
/// at the start of each turn. Returns `None` when no reminder applies
/// (mode != Plan and no active plan).
///
/// State machine:
///
///   (Plan, no plan)         → exploration phase: tell model to use
///                             read-only tools, then SubmitPlan.
///   (Plan, plan submitted)  → user-approval window: model waits.
///   (not-Plan, plan exists) → execution phase: Layer-2 narrowed view.
///                             Only the **current step's description**
///                             is shown; remaining steps appear as
///                             titles only so the model can't reason
///                             ahead and start debating future work
///                             (M4.1). Already-done steps reduced to a
///                             single comma-separated line.
///   (not-Plan, no plan)     → no reminder.
///
/// Extracted to a free function so the reminder shape is unit-testable
/// without spinning up the agent loop. Pure — reads `mode` and `plan`,
/// returns the formatted string.
pub fn build_plan_reminder(
    mode: crate::permissions::PermissionMode,
    plan: Option<&crate::tools::plan_state::Plan>,
) -> Option<String> {
    use crate::permissions::PermissionMode;

    match (mode, plan) {
        (PermissionMode::Plan, None) => Some(
            "## Plan mode is active\n\n\
             Mutating tools (Write, Edit, Bash, document editors, etc.) \
             are BLOCKED. Use Read / Grep / Glob / Ls to explore the \
             codebase. When you have enough context, call SubmitPlan \
             with an ordered list of concrete, testable steps. The \
             user will review the plan in the right-side sidebar and \
             approve before execution.\n\n\
             Do NOT call TodoWrite in plan mode — SubmitPlan is the \
             structured replacement that the user can see live.\n\n\
             ### What makes a good plan\n\n\
             **Step count: as many as needed; no caps.** A plan is \
             correctly decomposed when EVERY step satisfies all three:\n\
             1. ONE action — a single command, edit, or generation. \
             \"Scaffold the project\" is one action; \"scaffold AND \
             install deps\" is two.\n\
             2. ONE shell-runnable verification — exit code, file \
             existence, regex match, HTTP probe, or test runner. If \
             you wrote two verifications (build exits 0 AND tests \
             pass), it's two steps.\n\
             3. PRESERVED across the next step — if step N's output \
             gets overwritten or replaced by step N+1, merge them. \
             Throwaway work shouldn't be a step.\n\n\
             If a 30-step plan satisfies all three rules, ship it. The \
             driver runs one step at a time with bounded retries; long \
             plans don't burn context because history is compacted at \
             every step boundary. Bundling steps to fit a small count \
             makes the plan WORSE — when a combined step fails, the \
             user can't tell which half failed.\n\n\
             Floor: a plan needs at least 2 steps. A 1-step plan isn't \
             a plan, it's a tool call — just do the work without \
             entering plan mode.\n\n\
             **Build-and-run, implement-and-test, refactor-and-smoke- \
             test are all TWO steps each.** When a combined step \
             fails, the user can't tell which half failed; the sidebar \
             just shows ✕ on a vague label. Keep one action per step.\n\n\
             **Verifications must be shell-runnable.** Every \
             verification is one of:\n\
             - Shell exit code: `cargo build --release` exits 0\n\
             - File existence: `test -f target/release/foo` or `ls \
             dist/index.html`\n\
             - Regex match: `grep -q 'useState' src/App.tsx`\n\
             - HTTP probe: `curl -fsS localhost:8080/healthz`\n\
             - Test runner: `pnpm test`, `cargo test --test integration`\n\n\
             NO human-eye checks: \"in browser\", \"visually clear\", \
             \"feels right\", \"the UI works\" — an autonomous agent \
             can't verify these. If a step's only check is human-eye, \
             reframe it as \"file matches grep pattern X\" or \"build \
             exits 0\" or split it so a different step has the runnable \
             check.\n\n\
             **Long-running processes are NOT verifications.** \
             `pnpm run dev` / `cargo run --release` / `python -m \
             http.server` are servers — they don't exit. The Bash tool \
             can't sit on them. Use the equivalent build/test command \
             instead: `pnpm run build` (static check), `cargo build \
             --release` (compile only), `python -c \"import …\"` \
             (import smoke).\n\n\
             **No bootstrap-then-overwrite steps.** If step N+1 will \
             replace what step N produced, merge them. Test: would \
             step N's output survive into step N+1? If no, step N is \
             throwaway. Example: \"step 1: pnpm init\" + \"step 2: \
             pnpm create vite . --force\" — step 2 overwrites step 1's \
             package.json, so step 1 is throwaway. Use the canonical \
             scaffolder as one step.\n\n\
             **Default to canonical scaffolders.** When the goal needs \
             a Vite / Next / Nuxt / cargo project, use the official \
             non-interactive scaffolder as ONE step (`pnpm create \
             vite@latest <dir> --template react-ts`, `cargo new <dir> \
             --bin`, etc.) — don't decompose it into \"init package, \
             add deps, configure tooling\". Scaffolders exist for a \
             reason. Avoid commands that require an interactive TTY \
             prompt; the Bash tool can't answer them.\n\n\
             **Name cross-step artifacts in descriptions.** When step \
             N+1 depends on a file step N produced, BOTH descriptions \
             must name the file. \"Step 3 edits src/App.tsx\" requires \
             step 2's description to name `src/App.tsx` as an output. \
             Surfaces implicit deps and lets the user audit the chain \
             before approving.\n\n\
             **When executing the step, ACTUALLY RUN the verification \
             before marking it done.** The flow is:\n\
             1. UpdatePlanStep(id, \"in_progress\")\n\
             2. Perform the step's main action (Edit / Write / Bash)\n\
             3. **Run the verification** (Bash / Read / curl — whatever \
             you specified)\n\
             4. If verification passes → UpdatePlanStep(id, \"done\")\n\
             5. If verification fails → UpdatePlanStep(id, \"failed\", \
             note: \"<what failed, ideally one line>\")\n\n\
             Do NOT skip the verification and call \"done\" anyway. The \
             gate will accept it (the agent can't tell whether you \
             actually ran the check), but the user is going to find out \
             on the next step or in production, and trust degrades.\n\n\
             On failure, mark the step Failed — don't paper over a \
             broken build by marking it done and moving on. The user \
             has Retry / Skip / Abort buttons in the sidebar; that's \
             the right path when something legitimately broke.\n\n\
             **Titles: imperative verb + outcome.** \"Add /healthz \
             endpoint to web server\", not \"Endpoint work\" or \"I \
             will add an endpoint\". User skims titles; make them \
             scannable.\n\n\
             **Descriptions: name the files, the operations, and the \
             verification.** \"Edit src/routes.rs: add GET /healthz \
             handler returning {ok: true}. Verify: `curl \
             localhost:8080/healthz | jq .ok` returns true.\"\n\n\
             **Order by dependency, then by risk.** Step N+1 must be \
             safe to start only after step N succeeds — that's how the \
             gate works. Within dependency constraints, tackle \
             risky / uncertain work early so failures surface before \
             half the plan is committed. Don't put \"deploy to \
             production\" before \"run the test suite\".\n\n\
             **Read before planning.** Drafted-from-imagination plans \
             miss real constraints — function signatures, build \
             configs, existing patterns. If your plan involves editing \
             a file you haven't read, read it now before calling \
             SubmitPlan.\n\n\
             **Ask before submitting if the stack is undetermined.** \
             AskUserQuestion is REQUIRED — not optional — when a key \
             decision can't be resolved from the code or the user's \
             prompt alone (which framework? which storage backend? \
             which deployment target?). A wrong assumption forces a \
             full replan, wastes the user's review time, and burns \
             execution attempts on the wrong target. Ask BEFORE \
             SubmitPlan.\n\n\
             **No \"maybe\" / \"optional\" steps.** Either a step is \
             needed and you'll do it, or it isn't and you skip it. The \
             gate has Done / Failed, not Maybe.\n\n\
             ### Audit BEFORE calling SubmitPlan\n\n\
             After drafting the plan, run this self-audit. If any \
             check fails, revise — don't submit a plan you wouldn't \
             approve yourself.\n\n\
             1. **Goal coverage** — Read the user's original ask. Does \
             executing every step in order actually deliver what they \
             asked for? If a step is missing (a feature, a build \
             check, a final smoke test), add it.\n\
             2. **Atomic steps** — Apply the three-rule test (one \
             action, one shell-runnable verification, output preserved \
             across the next step) to EVERY step. Split any that fail.\n\
             3. **Bash-runnable verifications** — Re-read every \
             `Verify:` line. Could you run that as a shell command \
             right now? If a verification mentions \"browser\", \
             \"visually\", or \"feels\", replace it with a grep / curl \
             / build check, or split the step.\n\
             4. **Cross-step dependencies named** — For every step \
             that edits a file, confirm the file's existence is \
             established by an earlier step's description (or by the \
             current code). If step 3 needs `src/App.tsx` and the \
             plan never names where that comes from, fix step 2's \
             description.\n\
             5. **No throwaway steps** — Walk pairs (step N, step \
             N+1). Will step N+1 overwrite or replace step N's \
             output? If yes, merge them.\n\
             6. **No long-running servers as verifications** — Search \
             your verification lines for `pnpm run dev`, `cargo run`, \
             `npm start`, `python -m http.server`. Replace each with \
             the build-only equivalent.\n\
             7. **Stack decisions made** — Did you assume a framework \
             / storage / deploy target the user didn't specify? If so, \
             call AskUserQuestion BEFORE SubmitPlan.\n\n\
             Only call SubmitPlan once all seven checks pass. The user \
             trusts the plan you submit; submitting a plan you'd flag \
             as weak yourself wastes their approval and your retry \
             budget."
                .to_string(),
        ),
        (PermissionMode::Plan, Some(p)) => {
            // M6.9 (Bug C1): if the prior plan finished and the user
            // re-entered plan mode for a NEW task, the slot still
            // holds the all-done plan. Treat that case as "plan mode
            // is active, no plan yet" so the model gets the right
            // exploration/SubmitPlan reminder instead of "awaiting
            // approval" of a finished plan. The sidebar's Approve
            // button is also gated on !allDone so it doesn't show
            // either.
            use crate::tools::plan_state::StepStatus;
            let all_done = p.steps.iter().all(|s| s.status == StepStatus::Done);
            if all_done {
                // Recurse with effectively-no-plan to get the
                // (Plan, None) reminder. Cheaper than duplicating
                // the long string here.
                return build_plan_reminder(mode, None);
            }
            Some(format!(
                "## Plan mode — awaiting user approval\n\n\
                 You have submitted a plan ({} steps). The user is reviewing \
                 it in the right-side sidebar. They will click **Approve** to \
                 begin execution, or **Cancel** to discard the plan.\n\n\
                 While waiting for approval, do NOT:\n\
                 - Call **any other tools** — not Read, Grep, Edit, Bash, \
                 UpdatePlanStep, or ExitPlanMode. The agent loop will block \
                 these and surface a tool-result error if you try.\n\
                 - Call **SubmitPlan again** unless the user explicitly asks \
                 for a different plan.\n\
                 - Tell the user to **type anything** to start (\"type 'go' \
                 / 'start' / 'begin' to proceed\"). The buttons are the \
                 contract — there is no chat-input route to approve. If you \
                 tell them to type something, they will, and the system will \
                 block your follow-up tool calls anyway.\n\n\
                 Just emit a brief one-line confirmation that the plan is \
                 ready for review (or stay silent) and stop. The sidebar \
                 buttons are how the user moves the workflow forward, not \
                 chat messages.",
                p.steps.len(),
            ))
        }
        (_, Some(p)) => Some(build_execution_reminder(p)),
        (_, None) => None,
    }
}

/// Layer-2 narrowed view (M4.1). The model sees:
///   - the current step's title + full description
///   - remaining steps as titles only (no descriptions)
///   - completed steps as a single comma-separated tally
///   - per-step protocol + the M3 "execute autonomously" wording
///
/// Goal: focus the model on the one step it's working on. Hiding step
/// descriptions for upcoming work prevents the model from coordinating
/// across step boundaries (e.g. "let me also do step 5 while I'm here")
/// and reduces the urge to ask the user "shall I proceed?" between
/// steps — there's nothing to debate when only one step is visible in
/// detail.
fn build_execution_reminder(plan: &crate::tools::plan_state::Plan) -> String {
    use crate::tools::plan_state::StepStatus;

    let total = plan.steps.len();

    // Pick the focus step:
    //   1. The InProgress step, if any (model already started)
    //   2. The Failed step, if any (recovery state)
    //   3. The first Todo step (about to start)
    //   4. None — all steps Done
    let focus_idx = plan
        .steps
        .iter()
        .position(|s| s.status == StepStatus::InProgress)
        .or_else(|| {
            plan.steps
                .iter()
                .position(|s| s.status == StepStatus::Failed)
        })
        .or_else(|| plan.steps.iter().position(|s| s.status == StepStatus::Todo));

    let mut out = String::new();
    out.push_str("## Executing approved plan");
    if let Some(idx) = focus_idx {
        out.push_str(&format!(" — step {} of {}\n\n", idx + 1, total));
        let step = &plan.steps[idx];

        // Heading line: title + status hint when relevant.
        let status_hint = match step.status {
            StepStatus::InProgress => "  Current step:",
            StepStatus::Failed => "  Failed step (awaiting user retry / skip / abort):",
            StepStatus::Todo => "  Next step (call UpdatePlanStep with \"in_progress\" to begin):",
            StepStatus::Done => "  Step:",
        };
        out.push_str(&format!("{status_hint} \"{}\"\n\n", step.title));

        if !step.description.trim().is_empty() {
            // Indent the description so it visually clusters with the
            // current step block.
            for line in step.description.lines() {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
            out.push('\n');
        }

        if let Some(note) = &step.note {
            if !note.trim().is_empty() {
                out.push_str(&format!("  (note: {note})\n\n"));
            }
        }

        // Already complete: short tally line.
        let done_titles: Vec<String> = plan
            .steps
            .iter()
            .enumerate()
            .filter(|(_, s)| s.status == StepStatus::Done)
            .map(|(i, s)| format!("{}. {}", i + 1, s.title))
            .collect();
        if !done_titles.is_empty() {
            out.push_str("Already complete: ");
            out.push_str(&done_titles.join(" · "));
            out.push_str("\n\n");
        }

        // Remaining steps: titles only, NO descriptions. The whole
        // point of Layer-2 — model can see the shape of what's coming
        // but can't preview details and start working on later steps.
        let remaining: Vec<String> = plan
            .steps
            .iter()
            .enumerate()
            .skip(idx + 1)
            .filter(|(_, s)| s.status != StepStatus::Done)
            .map(|(i, s)| format!("  • {}. {}", i + 1, s.title))
            .collect();
        if !remaining.is_empty() {
            out.push_str("Remaining (titles only, do NOT preview):\n");
            out.push_str(&remaining.join("\n"));
            out.push_str("\n\n");
        }

        // M6.3: surface cross-step outputs from completed steps so the
        // model can read prior data (generated ids, hashes, paths)
        // even after step-boundary compaction has trimmed the chat
        // history. Only completed Done steps with a populated `output`
        // field are listed.
        let prior_outputs = collect_prior_step_outputs(plan, &step.id);
        if !prior_outputs.is_empty() {
            out.push_str("Outputs from prior steps (use these instead of guessing):\n");
            for (i, title, output) in &prior_outputs {
                out.push_str(&format!("  - Step {i} ({title}): {output}\n"));
            }
            out.push('\n');
        }

        // Per-step protocol — references the focus step's id directly.
        let protocol = match step.status {
            StepStatus::InProgress => format!(
                "Per-step protocol:\n\
                 - You're already on this step. Focus on completing it.\n\
                 - **Run the verification before marking done.** The plan's description \
                 for this step should specify what to check (build exits 0, test passes, \
                 endpoint returns 200, etc.). Actually run that check.\n\
                 - If verification passes: UpdatePlanStep(\"{id}\", \"done\")\n\
                 - If verification fails: UpdatePlanStep(\"{id}\", \"failed\", note: \"<one-line reason>\")\n\
                 - Don't paper over a failure by marking it done — the user has Retry / \
                 Skip / Abort buttons in the sidebar for the Failed path.",
                id = step.id,
            ),
            StepStatus::Todo => format!(
                "Per-step protocol:\n\
                 - Begin: UpdatePlanStep(\"{id}\", \"in_progress\")\n\
                 - Perform the step's main action.\n\
                 - **Run the verification** specified in the step's description.\n\
                 - If verification passes: UpdatePlanStep(\"{id}\", \"done\")\n\
                 - If verification fails: UpdatePlanStep(\"{id}\", \"failed\", note: \"<one-line reason>\")",
                id = step.id,
            ),
            StepStatus::Failed => "Per-step protocol:\n\
                 - This step is in Failed state. The user has Retry / Skip / Abort \
                 buttons in the sidebar. Do NOT call UpdatePlanStep again unless \
                 the user explicitly retries — wait for their next message."
                .to_string(),
            StepStatus::Done => String::new(),
        };
        if !protocol.is_empty() {
            out.push_str(&protocol);
            out.push_str("\n\n");
        }
    } else {
        // All steps Done.
        out.push_str(" — all steps complete\n\n");
        out.push_str(
            "Every step is Done. Wrap up the conversation with a brief \
             summary of what was accomplished. Do not call any further \
             tools unless the user requests follow-up work.\n\n",
        );
    }

    // Trailing autonomous-execution wording — same as M3.
    out.push_str(
        "**Execute autonomously.** The user has already approved this \
         plan by clicking the sidebar Approve button — do NOT pause to \
         ask \"shall I proceed?\" / \"should I do X?\" / \"continue with \
         the next step?\". Step transitions are the contract; the user \
         monitors progress via the sidebar checkmarks, not via chat \
         confirmations. Run end-to-end without intermediate user \
         checkpoints.\n\n\
         Only stop to ask the user (via AskUserQuestion or plain chat) \
         if you hit a genuine blocker — missing credentials, an ambiguous \
         decision the plan didn't resolve, or a destructive action that \
         materially exceeds the plan's scope. Otherwise: keep going.\n\n\
         Plans are strictly sequential — focus only on the current \
         step. The gate will reject any out-of-order transition.",
    );
    out
}

/// M6.1 Ralph-loop per-step continuation prompt. Used by the worker's
/// plan-execution driver to wake the agent loop with a focused user
/// message after a turn ends mid-plan. Terse on purpose — the heavy
/// lifting (full per-step protocol, sequential-gate rules, autonomous-
/// execution wording) is already in the system reminder built by
/// `build_execution_reminder`. This message just says "you have one
/// step to work on right now, go".
///
/// `attempt` is 1 on the first nudge for a step, 2 on the second
/// retry, etc. Higher attempt numbers escalate the wording so the
/// model knows it's been spinning and should commit to a transition.
pub fn build_step_continuation_prompt(
    plan: &crate::tools::plan_state::Plan,
    step: &crate::tools::plan_state::PlanStep,
    attempt: usize,
) -> String {
    use crate::tools::plan_state::StepStatus;

    let total = plan.steps.len();
    let position = plan
        .steps
        .iter()
        .position(|s| s.id == step.id)
        .map(|i| i + 1)
        .unwrap_or(0);

    let action_hint = match step.status {
        StepStatus::Todo => format!(
            "Begin: UpdatePlanStep(\"{}\", \"in_progress\"). Then perform the step's action, run the verification, and finish with UpdatePlanStep(\"{}\", \"done\") or UpdatePlanStep(\"{}\", \"failed\", note: \"<reason>\").",
            step.id, step.id, step.id,
        ),
        StepStatus::InProgress => format!(
            "You're already on this step. Run the verification specified in the step's description, then call UpdatePlanStep(\"{}\", \"done\") if it passed, or UpdatePlanStep(\"{}\", \"failed\", note: \"<reason>\") if it didn't.",
            step.id, step.id,
        ),
        StepStatus::Failed | StepStatus::Done => {
            // Driver shouldn't be picking these as the next-actionable
            // step, but be defensive — return a no-op-ish prompt.
            return format!(
                "Plan step \"{}\" is in state {:?}. Wait for the user to retry, skip, or abort via the sidebar.",
                step.title, step.status,
            );
        }
    };

    // M6.3: surface prior step outputs so the model can read data from
    // earlier steps without relying on chat history (which gets
    // compacted between steps in M6.2). Only Done steps with a
    // populated `output` field are listed.
    let prior_outputs = collect_prior_step_outputs(plan, &step.id);
    let outputs_block = if prior_outputs.is_empty() {
        String::new()
    } else {
        let mut s = String::from("\n\nOutputs from prior steps:\n");
        for (i, title, out) in prior_outputs {
            s.push_str(&format!("  - Step {i} ({title}): {out}\n"));
        }
        s
    };

    if attempt <= 1 {
        format!(
            "Continue plan execution. Focus: step {position}/{total} \"{}\".\n\n{}{outputs_block}",
            step.title, action_hint,
        )
    } else {
        // Escalate. The model has had at least one turn on this step
        // without committing; remind it that the retry budget is
        // bounded and that "failed" with a one-line note is a valid
        // outcome the user can recover from.
        format!(
            "Continue plan execution — attempt {attempt} on step {position}/{total} \"{}\". \
             You have at most {} attempts per step; after that the driver \
             will mark this step Failed automatically. Commit to a \
             transition this turn: do the work and call \
             UpdatePlanStep(\"{}\", \"done\"), OR if the step can't be \
             finished, call UpdatePlanStep(\"{}\", \"failed\", note: \
             \"<one-line reason>\") so the user can retry / skip / \
             abort via the sidebar.\n\n{}",
            step.title,
            crate::tools::plan_state::MAX_RETRIES_PER_STEP,
            step.id,
            step.id,
            action_hint,
        )
    }
}

/// Read `.thclaws/todos.md` from the working directory and, if it
/// exists and has any incomplete items (`[ ]` pending or `[-]`
/// in_progress), return a system-reminder string surfacing the list.
/// Returns `None` if the file is missing, empty, or has only completed
/// items — no point nagging the model with a fully-checked list.
///
/// This is the programmatic counterpart to the system-prompt directive
/// that says "check `.thclaws/todos.md` BEFORE asking for context."
/// Real-world testing showed that prompt-only guidance isn't enough on
/// some models — gpt-4.1 in particular still asks the user instead of
/// reading the file. Auto-injecting the contents removes the model's
/// option to ignore the rule.
///
/// Mirrors Claude Code's `todo_reminder` attachment shape (see
/// claude-code-src/utils/messages.ts:3663) but always-on instead of
/// every-N-turns — we don't have the turn-count tracking, and the
/// content is small enough (~200 bytes for a typical 3–5 item list)
/// that always-on is acceptable.
pub fn build_todos_reminder() -> Option<String> {
    let path = std::env::current_dir()
        .ok()?
        .join(".thclaws")
        .join("todos.md");
    let raw = std::fs::read_to_string(&path).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    // Quick check: any incomplete checkbox (`[ ]` pending or `[-]`
    // in_progress)? If everything is `[x]` completed, skip the
    // reminder — the model doesn't need to be reminded about a
    // closed-out list.
    let has_incomplete = raw
        .lines()
        .any(|l| l.trim_start().starts_with("- [ ]") || l.trim_start().starts_with("- [-]"));
    if !has_incomplete {
        return None;
    }
    // M6.18 BUG M6: cap todos.md so an unmaintained list doesn't burn
    // unbounded tokens every turn. 80 lines / 6 KB is generous for a
    // typical scratchpad — headers + bullets average ~50 bytes/line.
    let bounded =
        crate::memory::truncate_for_prompt(raw.trim_end(), 80, 6_000, ".thclaws/todos.md");
    Some(format!(
        "## Existing todos (.thclaws/todos.md)\n\n\
         A scratchpad todo list from a prior session is present in this \
         workspace. Surface this to the user before asking what to work \
         on, and offer to resume incomplete items (`[ ]` pending or \
         `[-]` in_progress) or replace the list. Don't ask \"what \
         should we do?\" while these answers are sitting in front of \
         you.\n\n\
         Current contents:\n\n\
         ```markdown\n{bounded}```\n\n\
         If the user wants to resume, mark the next pending item as \
         `in_progress` via TodoWrite (passing the full list with that \
         one item flipped) and start work on it. If they want a fresh \
         start, write an updated list via TodoWrite that reflects the \
         new direction."
    ))
}

/// Collect (position, title, output) tuples for completed steps that
/// have a populated `output` field, for steps prior to `current_step_id`.
/// Used by both the per-step continuation prompt and the system
/// reminder to expose the cross-step data channel (M6.3).
fn collect_prior_step_outputs(
    plan: &crate::tools::plan_state::Plan,
    current_step_id: &str,
) -> Vec<(usize, String, String)> {
    use crate::tools::plan_state::StepStatus;
    let current_idx = plan
        .steps
        .iter()
        .position(|s| s.id == current_step_id)
        .unwrap_or(plan.steps.len());
    plan.steps
        .iter()
        .enumerate()
        .take(current_idx)
        .filter_map(|(i, s)| {
            if s.status != StepStatus::Done {
                return None;
            }
            let out = s.output.as_ref()?.trim();
            if out.is_empty() {
                return None;
            }
            Some((i + 1, s.title.clone(), out.to_string()))
        })
        .collect()
}

/// Max tool result size kept in context. Excess is saved to disk with a preview.
pub const TOOL_RESULT_CONTEXT_LIMIT: usize = 50_000;

/// Default output token cap (keeps normal responses lean).
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
/// Escalated cap when the model hits the output limit.
pub const ESCALATED_MAX_TOKENS: u32 = 64000;

pub struct Agent {
    provider: Arc<dyn Provider>,
    pub(crate) tools: ToolRegistry,
    model: String,
    system: String,
    pub budget_tokens: usize,
    pub max_tokens: u32,
    pub max_iterations: usize,
    pub max_retries: usize,
    pub thinking_budget: Option<u32>,
    pub permission_mode: PermissionMode,
    approver: Arc<dyn ApprovalSink>,
    history: Arc<Mutex<Vec<Message>>>,
    /// Cooperative cancel signal shared with the worker / driver. M6.17
    /// BUGs H1 + M3: pre-fix the agent's retry-backoff sleeps blocked
    /// uninterruptibly (1+2+4 = 7 s worst case), so a Cancel during a
    /// transient-error retry was silently waited out. With a cancel
    /// token wired in, the sleep `tokio::select!`s against
    /// `cancelled().await` and exits with a synthetic error mid-wait.
    /// `None` for tests / non-interactive consumers that don't want
    /// cancellation plumbing.
    pub(crate) cancel: Option<crate::cancel::CancelToken>,
    /// M6.35 HOOK1+HOOK3: lifecycle hooks. Pre-fix the `crate::hooks`
    /// module existed and had a documented user-manual chapter
    /// (`ch13-hooks.md`) but was completely orphaned — no production
    /// code path called `fire_*`. Now the dispatch site (around
    /// `tool.call_multimodal`) and the explicit-deny site fire the
    /// configured hooks. `None` keeps tests and standalone consumers
    /// hook-free.
    pub(crate) hooks: Option<std::sync::Arc<crate::hooks::HooksConfig>>,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        model: impl Into<String>,
        system: impl Into<String>,
    ) -> Self {
        let model = model.into();
        // Resolve the model's real context window from the shipped
        // catalogue (user cache → embedded baseline → provider default
        // → global fallback). This drives auto-compact + `/compact` +
        // `/fork` thresholds so they match the model in use rather
        // than a blanket hardcoded number.
        let budget_tokens = crate::model_catalogue::effective_context_window(&model) as usize;
        Self {
            provider,
            tools,
            model,
            system: system.into(),
            budget_tokens,
            max_tokens: 8192,
            max_iterations: 200,
            max_retries: 3,
            thinking_budget: None,
            permission_mode: PermissionMode::Auto,
            approver: Arc::new(AutoApprover),
            history: Arc::new(Mutex::new(Vec::new())),
            cancel: None,
            hooks: None,
        }
    }

    /// Wire in a cancel token so retry sleeps and (future) long awaits
    /// can be interrupted by the worker / driver. Caller is responsible
    /// for `cancel.reset()`-ing between turns. M6.17 BUG H1 + M3.
    pub fn with_cancel(mut self, token: crate::cancel::CancelToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Wire in a HooksConfig so the agent fires user-configured shell
    /// hooks at tool dispatch / permission denial. M6.35 HOOK1.
    pub fn with_hooks(mut self, hooks: std::sync::Arc<crate::hooks::HooksConfig>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    pub fn with_max_iterations(mut self, n: usize) -> Self {
        self.max_iterations = n;
        self
    }

    pub fn with_permission_mode(mut self, mode: PermissionMode) -> Self {
        self.permission_mode = mode;
        self
    }

    pub fn with_approver(mut self, approver: Arc<dyn ApprovalSink>) -> Self {
        self.approver = approver;
        self
    }

    /// Append text to the system prompt.
    pub fn append_system(&mut self, text: &str) {
        self.system.push_str(text);
    }

    pub fn history_snapshot(&self) -> Vec<Message> {
        self.history.lock().expect("history lock").clone()
    }

    pub fn clear_history(&self) {
        self.history.lock().expect("history lock").clear();
    }

    /// Replace the agent's history wholesale — used when loading a saved session.
    pub fn set_history(&self, messages: Vec<Message>) {
        let mut h = self.history.lock().expect("history lock");
        *h = messages;
    }

    /// Run one user turn. The returned stream drives the full provider↔tools
    /// loop and appends to the agent's internal history.
    pub fn run_turn(
        &self,
        user_msg: String,
    ) -> impl Stream<Item = Result<AgentEvent>> + Send + 'static {
        // Common case: a plain text turn. Wrap as a single Text block
        // and delegate to the multipart entry point so the body lives
        // in exactly one place.
        self.run_turn_multipart(vec![ContentBlock::text(user_msg)])
    }

    /// Multipart variant of [`run_turn`]. Accepts an arbitrary list of
    /// content blocks for the user turn — used by the GUI chat composer
    /// to ship a message with both text and pasted/dragged image
    /// attachments (Phase 4: paste/drag → ContentBlock::Image alongside
    /// ContentBlock::Text). Exact same agent loop semantics; the only
    /// difference is what gets pushed onto history at turn start.
    pub fn run_turn_multipart(
        &self,
        user_content: Vec<ContentBlock>,
    ) -> impl Stream<Item = Result<AgentEvent>> + Send + 'static {
        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let model = self.model.clone();
        // Compose the per-turn system prompt: base + dynamic plan-mode
        // reminder + dynamic todos reminder so the model sees fresh
        // state every turn (plan mode active? plan submitted but not
        // approved? existing todos.md from a prior session?). Cheap —
        // just a string concat per turn.
        let system = {
            let base = self.system.clone();
            let mode = crate::permissions::current_mode();
            let active_plan = crate::tools::plan_state::get();
            let plan_reminder = build_plan_reminder(mode, active_plan.as_ref());
            let todos_reminder = build_todos_reminder();
            // Chain reminders. Plan reminder dominates when active —
            // it has the strongest per-turn discipline and would be
            // redundant with todos guidance. Otherwise surface todos
            // when a list exists.
            let chained = match (plan_reminder, todos_reminder) {
                (Some(p), Some(t)) => Some(format!("{p}\n\n{t}")),
                (Some(p), None) => Some(p),
                (None, Some(t)) => Some(t),
                (None, None) => None,
            };
            match chained {
                Some(r) if !base.is_empty() => format!("{base}\n\n{r}"),
                Some(r) => r,
                None => base,
            }
        };
        let budget_tokens = self.budget_tokens;
        let base_max_tokens = self.max_tokens;
        let max_iterations = self.max_iterations;
        let max_retries = self.max_retries;
        let thinking_budget = self.thinking_budget;
        // Captured here only as the *fallback* default when no global mode
        // has been set yet. The actual gate at tool-dispatch time reads
        // `permissions::current_mode()` so EnterPlanMode / ExitPlanMode /
        // `/plan` flips take effect mid-turn rather than next-message.
        let permission_mode_default = self.permission_mode;
        let approver = self.approver.clone();
        let history = self.history.clone();
        let cancel = self.cancel.clone();
        let hooks = self.hooks.clone();

        try_stream! {
            {
                let mut h = history.lock().expect("history lock");
                h.push(Message {
                    role: Role::User,
                    content: user_content,
                });
            }

            let mut current_max_tokens = base_max_tokens;
            let mut cumulative_usage = Usage::default();

            // 0 means unlimited.
            let effective_max = if max_iterations == 0 { usize::MAX } else { max_iterations };
            for iteration in 0..effective_max {
                yield AgentEvent::IterationStart { iteration };

                let messages = {
                    let h = history.lock().expect("history lock");
                    // M6.18 BUG H1: subtract the system-prompt size + a
                    // safety margin for tool definitions from the
                    // budget BEFORE compacting messages. Pre-fix, a
                    // large system prompt (CLAUDE.md cascade + memory
                    // bodies + KMS indices + skills) plus a budget-
                    // filling history could push the total request past
                    // the model's context window even though `compact`
                    // had "fitted" the messages. The provider then
                    // 400'd with "context length exceeded."
                    //
                    // We reserve 4 KiB for tool definitions (typical
                    // catalog of ~30-40 builtins + MCP tools) on top
                    // of the system-prompt deduction — rough but keeps
                    // the request comfortably inside the window.
                    let system_tokens = crate::tokens::estimate_tokens(&system);
                    let tools_reserve_tokens = 1024;
                    let messages_budget = budget_tokens
                        .saturating_sub(system_tokens)
                        .saturating_sub(tools_reserve_tokens);

                    // M6.35 HOOK4: pre_compact / post_compact fire only
                    // when compaction actually trims (history is over
                    // budget). compact() is called every turn but
                    // no-ops when within budget — firing on every turn
                    // would spam audit hooks with empty events.
                    let pre_tokens = crate::compaction::estimate_messages_tokens(&h);
                    let pre_count = h.len();
                    let will_compact = pre_tokens > messages_budget;
                    if will_compact {
                        if let Some(hk) = &hooks {
                            crate::hooks::fire_compact(
                                hk,
                                crate::hooks::HookEvent::PreCompact,
                                pre_count,
                                pre_tokens,
                            );
                        }
                    }
                    let compacted = compact(&h, messages_budget);
                    if will_compact {
                        if let Some(hk) = &hooks {
                            let post_tokens =
                                crate::compaction::estimate_messages_tokens(&compacted);
                            crate::hooks::fire_compact(
                                hk,
                                crate::hooks::HookEvent::PostCompact,
                                compacted.len(),
                                post_tokens,
                            );
                        }
                    }
                    compacted
                };
                let tool_defs = tools.tool_defs();

                let req = StreamRequest {
                    model: model.clone(),
                    system: if system.is_empty() { None } else { Some(system.clone()) },
                    messages,
                    tools: tool_defs,
                    max_tokens: current_max_tokens,
                    thinking_budget,
                };

                // Retry with exponential backoff on transient errors.
                // Config errors (missing API key, bad model name, etc.)
                // won't fix themselves between attempts — skip the retry
                // loop for those and surface the error immediately.
                //
                // M6.17 BUG M3: the backoff sleep `tokio::select!`s
                // against the cancel token (when one is wired in) so a
                // user-triggered Cancel during a 1-2-4s wait short-
                // circuits with a clear error instead of stalling for
                // up to 7 s.
                let raw = {
                    let mut last_err = None;
                    let mut stream_result = None;
                    let mut cancelled_during_retry = false;
                    for attempt in 0..=max_retries {
                        match provider.stream(req.clone()).await {
                            Ok(s) => { stream_result = Some(s); break; }
                            Err(e) => {
                                let is_config = matches!(e, Error::Config(_));
                                if !is_config && attempt < max_retries {
                                    let delay = tokio::time::Duration::from_secs(1 << attempt);
                                    eprintln!(
                                        "\x1b[33m[retry {}/{} after {}s: {}]\x1b[0m",
                                        attempt + 1, max_retries, delay.as_secs(), e
                                    );
                                    if let Some(token) = &cancel {
                                        tokio::select! {
                                            _ = tokio::time::sleep(delay) => {}
                                            _ = token.cancelled() => {
                                                cancelled_during_retry = true;
                                                break;
                                            }
                                        }
                                    } else {
                                        tokio::time::sleep(delay).await;
                                    }
                                }
                                last_err = Some(e);
                                if is_config { break; }
                            }
                        }
                    }
                    if cancelled_during_retry {
                        Err(Error::Provider("cancelled by user during retry backoff".into()))?
                    }
                    match stream_result {
                        Some(s) => s,
                        None => Err(last_err.unwrap())?,
                    }
                };
                let mut assembled = Box::pin(assemble(raw));

                let mut turn_text = String::new();
                let mut turn_thinking = String::new();
                let mut turn_tool_uses: Vec<ContentBlock> = Vec::new();
                // L4 (M6.17): id → parse error message for any tool use
                // whose JSON input failed to parse mid-stream. The per-
                // tool dispatch loop emits a synthetic error tool_result
                // for any id present here, instead of running the tool.
                let mut turn_parse_errors: Vec<(String, String)> = Vec::new();
                let mut turn_stop_reason: Option<String> = None;

                while let Some(ev) = assembled.next().await {
                    match ev? {
                        AssembledEvent::Text(s) => {
                            turn_text.push_str(&s);
                            yield AgentEvent::Text(s);
                        }
                        AssembledEvent::Thinking(s) => {
                            // Capture for persistence so it can be echoed
                            // back next turn (DeepSeek v4 etc. require it).
                            // Not surfaced as a separate AgentEvent yet —
                            // existing UI consumers don't have a thinking
                            // sink. When the GUI gets a "show reasoning"
                            // pane, route a new event through here.
                            turn_thinking.push_str(&s);
                        }
                        AssembledEvent::ToolParseFailed { id, name, error } => {
                            // L4 (M6.17): the provider sent malformed JSON
                            // for this tool use. Synthesize a ToolUse with
                            // empty input so the assistant message stays
                            // well-formed (some providers reject a tool
                            // ID without a matching tool_use), then push
                            // an error tool_result the model reads on the
                            // next iteration. Pre-fix this killed the
                            // entire turn via `?`.
                            let synth_block = ContentBlock::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                input: serde_json::json!({}),
                                thought_signature: None,
                            };
                            yield AgentEvent::ToolCallStart {
                                id: id.clone(),
                                name: name.clone(),
                                input: serde_json::json!({}),
                            };
                            turn_tool_uses.push(synth_block);
                            // Stash the parse error so the per-tool loop
                            // below can emit a matching error result.
                            turn_parse_errors.push((id, error));
                        }
                        AssembledEvent::ToolUse(block) => {
                            // L1 (M6.17): announce the tool call as soon as
                            // it's parsed, BEFORE the per-tool execution
                            // loop's approval / plan-mode / dispatch gates.
                            // Pre-fix the announce came right before the
                            // actual call() — meaning the user saw the
                            // assistant text stream, then a silent pause
                            // (during which the model had decided to call
                            // tools but the UI didn't show it), then the
                            // tool result. Yielding here gives an instant
                            // "[tool: X] queued" indicator the moment the
                            // tool block lands. Approval gating still
                            // happens later — denial / plan-mode block
                            // emit ToolCallDenied / ToolCallResult as
                            // before, so UI consumers don't get orphaned
                            // ToolCallStart events.
                            if let ContentBlock::ToolUse { id, name, input, .. } = &block {
                                yield AgentEvent::ToolCallStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                };
                            }
                            turn_tool_uses.push(block);
                        }
                        AssembledEvent::Done { stop_reason, usage } => {
                            turn_stop_reason = stop_reason;
                            if let Some(u) = &usage {
                                cumulative_usage.accumulate(u);
                            }
                        }
                    }
                }

                // Persist assistant message. Thinking comes FIRST so it
                // mirrors the order the model emitted (reasoning then
                // answer); some providers also expect that order in echo.
                {
                    let mut assistant_content: Vec<ContentBlock> = Vec::new();
                    if !turn_thinking.is_empty() {
                        assistant_content.push(ContentBlock::Thinking {
                            content: turn_thinking.clone(),
                            signature: None,
                        });
                    }
                    if !turn_text.is_empty() {
                        assistant_content.push(ContentBlock::Text { text: turn_text.clone() });
                    }
                    assistant_content.extend(turn_tool_uses.iter().cloned());
                    if !assistant_content.is_empty() {
                        let mut h = history.lock().expect("history lock");
                        h.push(Message {
                            role: Role::Assistant,
                            content: assistant_content,
                        });
                    }
                }

                // No tool uses → turn is over.
                if turn_tool_uses.is_empty() {
                    // Output token escalation: if the model hit the output limit,
                    // escalate max_tokens and retry this iteration.
                    if turn_stop_reason.as_deref() == Some("max_tokens")
                        && current_max_tokens < ESCALATED_MAX_TOKENS
                    {
                        current_max_tokens = ESCALATED_MAX_TOKENS;
                        eprintln!(
                            "\x1b[33m[output limit hit — escalating to {}]\x1b[0m",
                            ESCALATED_MAX_TOKENS
                        );
                        // Skip the tool-result push below — there were no
                        // tool uses, so `result_blocks` would be empty and
                        // Anthropic rejects any user message with empty
                        // content ("messages.N: user messages must have
                        // non-empty content").
                        continue;
                    } else {
                        yield AgentEvent::Done { stop_reason: turn_stop_reason, usage: cumulative_usage.clone() };
                        return;
                    }
                }

                // Execute each tool (after approval, if required) and collect results.
                let mut result_blocks: Vec<ContentBlock> = Vec::new();
                for tu in &turn_tool_uses {
                    let ContentBlock::ToolUse { id, name, input, .. } = tu else { continue };

                    // L4 (M6.17): if this tool's JSON input failed to
                    // parse during assembly, short-circuit with a
                    // synthetic error tool_result instead of dispatching
                    // (the tool would just fail on the empty input
                    // anyway, and the parse-error message is more
                    // actionable for the model).
                    if let Some((_, err)) =
                        turn_parse_errors.iter().find(|(eid, _)| eid == id)
                    {
                        result_blocks.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: err.clone().into(),
                            is_error: true,
                        });
                        yield AgentEvent::ToolCallResult {
                            id: id.clone(),
                            name: name.clone(),
                            output: Err(err.clone()),
                            ui_resource: None,
                        };
                        continue;
                    }

                    let tool = match tools.get(name) {
                        Some(t) => t,
                        None => {
                            let msg = format!("unknown tool: {name}");
                            result_blocks.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: msg.clone().into(),
                                is_error: true,
                            });
                            yield AgentEvent::ToolCallResult {
                                id: id.clone(),
                                name: name.clone(),
                                output: Err(msg),
                                ui_resource: None,
                            };
                            continue;
                        }
                    };

                    // Read the mode dynamically — EnterPlanMode (or the
                    // sidebar Approve / `/plan` slash) may have flipped it
                    // mid-turn, and we want that to take effect on the
                    // very next dispatch. `permission_mode_default` only
                    // matters when nothing has set the global yet.
                    let permission_mode = {
                        let m = crate::permissions::current_mode();
                        if matches!(m, PermissionMode::Ask) && permission_mode_default == PermissionMode::Auto {
                            // Worker startup before init — fall back to
                            // the agent's constructed default rather than
                            // accidentally prompting on a bare-Ask Mutex
                            // default.
                            permission_mode_default
                        } else {
                            m
                        }
                    };

                    // M6.20 BUG M1: TodoWrite block fires BEFORE the
                    // generic mutating-tool block below. Pre-fix the
                    // generic block ran first (because TodoWrite has
                    // requires_approval=true), so the model always saw
                    // the generic "Use Read/Grep/Glob/Ls" message
                    // instead of this specific "Use SubmitPlan" one.
                    // TodoWrite is the casual scratchpad outside plan
                    // mode — letting it coexist with SubmitPlan
                    // confused the model in tests (it would TodoWrite
                    // a draft list AND SubmitPlan the same content).
                    if matches!(permission_mode, PermissionMode::Plan)
                        && name == "TodoWrite"
                    {
                        let blocked = "Blocked: TodoWrite is the casual scratchpad outside plan mode. \
                                       In plan mode, call SubmitPlan to publish your plan to the \
                                       sidebar — UpdatePlanStep tracks progress per step.";
                        result_blocks.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: blocked.to_string().into(),
                            is_error: true,
                        });
                        yield AgentEvent::ToolCallResult {
                            id: id.clone(),
                            name: name.clone(),
                            output: Err(blocked.to_string()),
                            ui_resource: None,
                        };
                        continue;
                    }

                    // Plan-mode block (M2): mutating tools are off-limits
                    // during plan-mode exploration. Return a structured
                    // tool_result the model reads on the next turn and
                    // self-corrects ("oh, I'm in plan mode — call Read
                    // instead, then SubmitPlan when ready"). The whole
                    // dispatch path is short-circuited — no approval
                    // popup, no actual call. Plan tools themselves
                    // (SubmitPlan, UpdatePlanStep, EnterPlanMode,
                    // ExitPlanMode) have requires_approval=false and so
                    // sail through.
                    if matches!(permission_mode, PermissionMode::Plan)
                        && tool.requires_approval(input)
                    {
                        let blocked = format!(
                            "Blocked: {name} is not available in plan mode. \
                             Use Read / Grep / Glob / Ls to explore the codebase. \
                             When you have enough context, call SubmitPlan with an \
                             ordered list of concrete steps. The user will review \
                             and approve before execution."
                        );
                        result_blocks.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: blocked.clone().into(),
                            is_error: true,
                        });
                        yield AgentEvent::ToolCallResult {
                            id: id.clone(),
                            name: name.clone(),
                            output: Err(blocked),
                            ui_resource: None,
                        };
                        continue;
                    }

                    // Approval-window gate (bug fix after M5 testing):
                    // while a plan is submitted but the user hasn't
                    // approved yet, the model must NOT progress steps
                    // (UpdatePlanStep) or unilaterally exit plan mode
                    // (ExitPlanMode). Both bypass the user's review
                    // window — the model could call ExitPlanMode
                    // interpreting a casual "Start" as approval, flip
                    // mode to Auto on its own, and start writing files
                    // before the user has reviewed the plan.
                    //
                    // The sole legal path out of "plan submitted,
                    // awaiting approval" is the user clicking the
                    // sidebar Approve / Cancel button (which fire
                    // plan_approve / plan_cancel IPCs from the GUI).
                    // Re-submitting via SubmitPlan stays allowed —
                    // that's the model's "I changed my mind" channel
                    // and the new plan also waits for approval.
                    if matches!(permission_mode, PermissionMode::Plan)
                        && (name == "UpdatePlanStep" || name == "ExitPlanMode")
                        && crate::tools::plan_state::get().is_some()
                    {
                        let blocked = format!(
                            "Blocked: {name} is not available while waiting for the user to \
                             approve the plan. The user reviews the plan in the right-side \
                             sidebar and clicks Approve to begin execution (or Cancel to \
                             discard it). Do NOT instruct the user to type anything — the \
                             buttons are the contract. While you wait, do not call any other \
                             tools either; just stop emitting tool calls and let the sidebar \
                             speak for itself.",
                        );
                        result_blocks.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: blocked.clone().into(),
                            is_error: true,
                        });
                        yield AgentEvent::ToolCallResult {
                            id: id.clone(),
                            name: name.clone(),
                            output: Err(blocked),
                            ui_resource: None,
                        };
                        continue;
                    }

                    // Approval gate.
                    let needs_approval = matches!(permission_mode, PermissionMode::Ask)
                        && tool.requires_approval(input);
                    if needs_approval {
                        let req = ApprovalRequest {
                            tool_name: name.clone(),
                            input: input.clone(),
                            summary: None,
                        };
                        let decision = approver.approve(&req).await;
                        if matches!(decision, ApprovalDecision::Deny) {
                            // M6.35 HOOK3: surface explicit user denial
                            // to the configured permission_denied hook.
                            // BashTool / sandbox / plan-mode hard-blocks
                            // are NOT denials per this gate (they're
                            // tool-level rejections); only the explicit
                            // approver Deny lands here.
                            if let Some(h) = &hooks {
                                crate::hooks::fire_permission_denied(h, &name);
                            }
                            let denied = format!("denied by user: {name}");
                            result_blocks.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: denied.clone().into(),
                                is_error: true,
                            });
                            yield AgentEvent::ToolCallDenied {
                                id: id.clone(),
                                name: name.clone(),
                            };
                            continue;
                        }
                    }

                    // M6.35 HOOK1: pre_tool_use fires after the approval
                    // gate but before the tool runs. Fire-and-forget so
                    // the hook doesn't block dispatch — pre/post strict
                    // ordering is documented as best-effort, not a
                    // guarantee, in the user manual.
                    if let Some(h) = &hooks {
                        let input_str = serde_json::to_string(input)
                            .unwrap_or_else(|_| "<unserializable>".to_string());
                        crate::hooks::fire_pre_tool_use(h, &name, &input_str);
                    }

                    // ToolCallStart was yielded at parse time (see the
                    // assembled-event loop above) so the UI shows the
                    // tool queued before the approval modal pops. The
                    // dispatch site here just runs the call.
                    let tool_result = tool.call_multimodal(input.clone()).await;

                    let (content, is_error) = match &tool_result {
                        Ok(c) => {
                            // Truncate-to-disk only applies to text payloads;
                            // multimodal blocks (e.g. an image returned by
                            // Read) are passed through unchanged.
                            let truncated = match c {
                                crate::types::ToolResultContent::Text(s) => {
                                    crate::types::ToolResultContent::Text(maybe_truncate_to_disk(s))
                                }
                                crate::types::ToolResultContent::Blocks(_) => c.clone(),
                            };
                            (truncated, false)
                        }
                        Err(e) => (
                            crate::types::ToolResultContent::Text(format!("error: {e}")),
                            true,
                        ),
                    };
                    // Anthropic (and some other providers) reject
                    //   user messages must have non-empty content
                    // when a tool result is empty (e.g. a successful
                    // Write, a Bash with no stdout). Replace empty
                    // text-only results with a minimal marker so the
                    // model still knows the call completed.
                    let content = if content.is_empty() {
                        crate::types::ToolResultContent::Text("(no output)".to_string())
                    } else {
                        content
                    };

                    // M6.35 HOOK1: post_tool_use (or _failure) fires
                    // after we've materialized the result content but
                    // before pushing it into history. The output the
                    // hook sees is the truncated-to-disk variant — same
                    // text the next provider call will see, so audit
                    // hooks log what the model actually consumed.
                    if let Some(h) = &hooks {
                        let preview = match &content {
                            crate::types::ToolResultContent::Text(s) => s.clone(),
                            crate::types::ToolResultContent::Blocks(_) => "<multimodal>".to_string(),
                        };
                        crate::hooks::fire_post_tool_use(h, &name, &preview, is_error);
                    }
                    result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: content.clone(),
                        is_error,
                    });

                    // For MCP-Apps tools, fetch the widget HTML so the
                    // chat surface can mount an iframe alongside the
                    // text result. Only attempted on success; an
                    // errored tool call doesn't produce a widget. The
                    // fetch is best-effort — if it fails the user
                    // still sees the text result.
                    let ui_resource = if matches!(tool_result, Ok(_)) {
                        tool.fetch_ui_resource().await
                    } else {
                        None
                    };

                    yield AgentEvent::ToolCallResult {
                        id: id.clone(),
                        name: name.clone(),
                        output: match tool_result {
                            Ok(c) => Ok(c.to_text()),
                            Err(e) => Err(format!("{e}")),
                        },
                        ui_resource,
                    };
                }

                if !result_blocks.is_empty() {
                    let mut h = history.lock().expect("history lock");
                    h.push(Message {
                        role: Role::User,
                        content: result_blocks,
                    });
                }
            }

            // Hit the iteration cap without a natural stop.
            yield AgentEvent::Done {
                stop_reason: Some("max_iterations".to_string()),
                usage: cumulative_usage,
            };
        }
    }
}

/// If `content` exceeds `TOOL_RESULT_CONTEXT_LIMIT`, save the full content
/// to a temp file and return a preview + file path. The model sees the preview
/// and can reference the full file if needed.
fn maybe_truncate_to_disk(content: &str) -> String {
    if content.len() <= TOOL_RESULT_CONTEXT_LIMIT {
        return content.to_string();
    }
    // Save full content to a temp file.
    // M2 (M6.17): include a UUID per truncation. Pre-fix the filename
    // was just `tool-<pid>.txt` — every truncation in the same process
    // overwrote the previous one, so the model's "reference the full
    // file" affordance was a lie after the second truncation. Surface
    // any write failure in the truncation message instead of silently
    // promising a file that doesn't exist.
    let tmp_dir = std::env::temp_dir().join("thclaws-tool-output");
    let mkdir_err = std::fs::create_dir_all(&tmp_dir).err();
    let filename = format!(
        "tool-{}-{}.txt",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    );
    let path = tmp_dir.join(&filename);
    let write_err = std::fs::write(&path, content).err();

    let preview_end = content
        .char_indices()
        .nth(2000)
        .map(|(i, _)| i)
        .unwrap_or(content.len().min(2000));
    let footer = match (mkdir_err, write_err) {
        (None, None) => format!(
            "... [truncated: {} total bytes — full output saved to {}]",
            content.len(),
            path.display()
        ),
        (Some(e), _) | (_, Some(e)) => format!(
            "... [truncated: {} total bytes — could not save full output to disk ({}); preview only]",
            content.len(),
            e
        ),
    };
    format!("{}\n\n{}", &content[..preview_end], footer)
}

/// Drain an agent stream into a blocking result. Useful for tests and for
/// non-interactive consumers.
pub async fn collect_agent_turn<S>(stream: S) -> Result<AgentTurnOutcome>
where
    S: Stream<Item = Result<AgentEvent>> + Send,
{
    collect_agent_turn_with_cancel(stream, None).await
}

/// M6.33 SUB4: cancel-aware variant of `collect_agent_turn`. When a
/// `CancelToken` is wired in, the loop `tokio::select!`s the next
/// stream event against `cancel.cancelled().await` so a parent ctrl-C
/// short-circuits the subagent's run instead of waiting for the
/// subagent to exhaust its iteration budget.
///
/// Pre-fix subagents had no cancel observation: a runaway 200-iteration
/// subagent could burn 10+ minutes uninterruptibly because the parent's
/// cancel only reached its own retry-backoff sleeps, never propagated
/// down to the child Agent.
pub async fn collect_agent_turn_with_cancel<S>(
    stream: S,
    cancel: Option<crate::cancel::CancelToken>,
) -> Result<AgentTurnOutcome>
where
    S: Stream<Item = Result<AgentEvent>> + Send,
{
    let mut out = AgentTurnOutcome::default();
    let mut stream = Box::pin(stream);
    loop {
        let next = if let Some(c) = &cancel {
            tokio::select! {
                ev = stream.next() => ev,
                _ = c.cancelled() => {
                    return Err(Error::Agent("cancelled by user".into()));
                }
            }
        } else {
            stream.next().await
        };
        let Some(ev) = next else { break };
        match ev? {
            AgentEvent::IterationStart { iteration } => out.iterations = iteration + 1,
            AgentEvent::Text(s) => out.text.push_str(&s),
            AgentEvent::ToolCallStart { name, .. } => out.tool_calls.push(name),
            AgentEvent::ToolCallResult { .. } => {}
            AgentEvent::ToolCallDenied { name, .. } => out.tool_denials.push(name),
            AgentEvent::Done { stop_reason, usage } => {
                out.stop_reason = stop_reason;
                out.usage = Some(usage);
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Default, Clone)]
pub struct AgentTurnOutcome {
    pub text: String,
    pub tool_calls: Vec<String>,
    pub tool_denials: Vec<String>,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
    pub iterations: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::providers::{EventStream, ProviderEvent};
    use async_trait::async_trait;
    use futures::stream;
    use std::collections::VecDeque;
    use tempfile::tempdir;

    // ── Layer-2 plan reminder shape tests (M4.1) ───────────────────────
    //
    // Pure-function tests on `build_plan_reminder` — they don't touch
    // global state, just feed a `Plan` snapshot in and assert the
    // resulting string mentions / hides the right things.

    fn step(
        id: &str,
        title: &str,
        description: &str,
        status: crate::tools::plan_state::StepStatus,
    ) -> crate::tools::plan_state::PlanStep {
        crate::tools::plan_state::PlanStep {
            id: id.into(),
            title: title.into(),
            description: description.into(),
            status,
            note: None,
            output: None,
        }
    }

    fn make_plan(steps: Vec<crate::tools::plan_state::PlanStep>) -> crate::tools::plan_state::Plan {
        crate::tools::plan_state::Plan {
            id: "plan-test".into(),
            steps,
        }
    }

    #[test]
    fn reminder_layer2_hides_descriptions_for_remaining_steps() {
        use crate::tools::plan_state::StepStatus;
        let plan = make_plan(vec![
            step(
                "s1",
                "Scaffold project",
                "scaffold detail",
                StepStatus::Done,
            ),
            step(
                "s2",
                "Install deps",
                "install detail HIDE FROM MODEL",
                StepStatus::InProgress,
            ),
            step(
                "s3",
                "Configure Vite",
                "vite detail SHOULD-NOT-LEAK",
                StepStatus::Todo,
            ),
            step(
                "s4",
                "Add tests",
                "tests detail ALSO-HIDDEN",
                StepStatus::Todo,
            ),
        ]);
        let r = build_plan_reminder(PermissionMode::Auto, Some(&plan)).unwrap();

        // Current step's description IS shown.
        assert!(
            r.contains("install detail HIDE FROM MODEL"),
            "current step description must appear: {r}"
        );

        // Future steps' descriptions are NOT shown (titles only).
        assert!(
            !r.contains("vite detail SHOULD-NOT-LEAK"),
            "step 3 description leaked: {r}"
        );
        assert!(
            !r.contains("tests detail ALSO-HIDDEN"),
            "step 4 description leaked: {r}"
        );

        // Future steps' titles ARE shown so the model knows the shape.
        assert!(r.contains("Configure Vite"), "step 3 title missing: {r}");
        assert!(r.contains("Add tests"), "step 4 title missing: {r}");

        // Already-complete tally line.
        assert!(r.contains("Already complete:"));
        assert!(r.contains("1. Scaffold project"));

        // Per-step protocol references the focus step's id.
        assert!(
            r.contains("UpdatePlanStep(\"s2\""),
            "protocol must name the focus step id: {r}"
        );
    }

    #[test]
    fn reminder_layer2_picks_failed_step_as_focus() {
        use crate::tools::plan_state::StepStatus;
        let mut s2 = step("s2", "Install deps", "doesn't matter", StepStatus::Failed);
        s2.note = Some("ENOTFOUND registry.npmjs.org".into());
        let plan = make_plan(vec![
            step("s1", "Scaffold", "", StepStatus::Done),
            s2,
            step("s3", "Configure", "", StepStatus::Todo),
        ]);
        let r = build_plan_reminder(PermissionMode::Auto, Some(&plan)).unwrap();

        assert!(r.contains("Failed step"), "failed status hint missing: {r}");
        assert!(
            r.contains("ENOTFOUND registry"),
            "failure note missing: {r}"
        );
        // Model is told to wait for user retry/skip/abort, not to call UpdatePlanStep.
        assert!(
            r.contains("user retry / skip / abort"),
            "retry hint missing: {r}"
        );
    }

    #[test]
    fn reminder_layer2_all_done_wraps_up() {
        use crate::tools::plan_state::StepStatus;
        let plan = make_plan(vec![
            step("s1", "A", "", StepStatus::Done),
            step("s2", "B", "", StepStatus::Done),
        ]);
        let r = build_plan_reminder(PermissionMode::Auto, Some(&plan)).unwrap();
        assert!(r.contains("all steps complete"));
        assert!(r.contains("Wrap up"));
        // No per-step protocol — nothing to do.
        assert!(!r.contains("UpdatePlanStep("));
    }

    #[test]
    fn reminder_plan_mode_with_no_plan_tells_model_to_explore() {
        let r = build_plan_reminder(PermissionMode::Plan, None).unwrap();
        assert!(r.contains("Plan mode is active"));
        assert!(r.contains("Read / Grep"));
        assert!(r.contains("SubmitPlan"));
        assert!(r.contains("Do NOT call TodoWrite"));
    }

    #[test]
    fn reminder_plan_mode_includes_step_quality_guidance() {
        // The (Plan, no plan) reminder should give the model concrete
        // guidance on what makes a good plan — this is the cure for
        // the bug where the model wrote a single "Build and run the
        // webapp" step that hid which half failed. Each of these
        // checks asserts a specific guidance dimension is present.
        let r = build_plan_reminder(PermissionMode::Plan, None).unwrap();

        // M6.5: step count is no longer capped at 3–10. The
        // split-until-atomic rule replaces it. The cap was actively
        // making plans worse — models bundled actions to fit the
        // window. Now the test asserts the cap is GONE and the
        // three-rule decomposition test is present.
        assert!(
            !r.contains("3–10") && !r.contains("3-10"),
            "step-count cap should be removed: {r}",
        );
        assert!(
            r.contains("as many as needed"),
            "no-cap step-count guidance missing: {r}",
        );
        assert!(
            r.contains("ONE action") && r.contains("ONE shell-runnable verification"),
            "split-until-atomic three-rule test missing: {r}",
        );
        assert!(
            r.contains("PRESERVED across the next step"),
            "preservation rule missing: {r}",
        );

        // Floor: 1-step plan isn't a plan.
        assert!(
            r.contains("at least 2 steps"),
            "floor of 2 steps missing: {r}",
        );

        // No-combination rule, the user's specific ask: build and run
        // must be separate steps.
        assert!(
            r.contains("Build-and-run") || r.contains("\"Build and run\" is TWO steps"),
            "build-and-run separation rule missing: {r}",
        );

        // M6.5: Bash-runnable verifications.
        assert!(
            r.contains("Verifications must be shell-runnable"),
            "shell-runnable rule missing: {r}",
        );
        assert!(
            r.contains("NO human-eye checks"),
            "no-human-eye rule missing: {r}",
        );

        // M6.5: long-running servers are not verifications.
        assert!(
            r.contains("Long-running processes are NOT verifications"),
            "no-long-running-server rule missing: {r}",
        );

        // M6.5: no bootstrap-then-overwrite.
        assert!(
            r.contains("No bootstrap-then-overwrite"),
            "bootstrap-overwrite rule missing: {r}",
        );

        // M6.5: default to canonical scaffolders.
        assert!(
            r.contains("canonical scaffolders"),
            "canonical-scaffolder rule missing: {r}",
        );
        assert!(
            r.contains("interactive TTY prompt"),
            "interactive-prompt warning missing: {r}",
        );

        // M6.5: cross-step artifacts named.
        assert!(
            r.contains("Name cross-step artifacts"),
            "cross-step-artifacts rule missing: {r}",
        );

        // ACTUALLY RUN the verification.
        assert!(
            r.contains("ACTUALLY RUN the verification"),
            "must actually run verify, not just declare it: {r}",
        );

        // Failure-path guidance: don't paper over.
        assert!(
            r.contains("Don't paper over") || r.contains("don't paper over"),
            "no-paper-over rule missing: {r}",
        );

        // M6.5: ask-when-stack-undetermined is REQUIRED.
        assert!(
            r.contains("AskUserQuestion is REQUIRED"),
            "required-ask rule missing: {r}",
        );
    }

    #[test]
    fn reminder_plan_mode_includes_pre_submission_audit() {
        // M6.5: After drafting the plan, the model should run a
        // self-audit before calling SubmitPlan. This was added because
        // a real test session shipped a plan that bundled actions and
        // had un-shell-runnable verifications — the audit catches
        // those before the user has to.
        let r = build_plan_reminder(PermissionMode::Plan, None).unwrap();

        assert!(
            r.contains("Audit BEFORE calling SubmitPlan"),
            "pre-submission audit section missing: {r}",
        );

        // Each of the seven audit checks should appear by name.
        for needle in [
            "Goal coverage",
            "Atomic steps",
            "Bash-runnable verifications",
            "Cross-step dependencies named",
            "No throwaway steps",
            "No long-running servers as verifications",
            "Stack decisions made",
        ] {
            assert!(
                r.contains(needle),
                "audit checklist item missing: {needle:?} not in: {r}",
            );
        }

        // The audit should be presented as a gate, not advisory.
        assert!(
            r.contains("revise — don't submit a plan you wouldn't approve yourself"),
            "audit gate framing missing: {r}",
        );
    }

    #[test]
    fn reminder_per_step_protocol_includes_verification_step() {
        // The Layer-2 narrowed view used during execution should also
        // remind the model to run the verification before marking
        // done. Surface guidance for both InProgress and Todo states.
        use crate::tools::plan_state::StepStatus;

        let plan_in_progress = make_plan(vec![step(
            "s1",
            "Build the project",
            "cargo build --release. Verify exits 0.",
            StepStatus::InProgress,
        )]);
        let r = build_plan_reminder(PermissionMode::Auto, Some(&plan_in_progress)).unwrap();
        assert!(
            r.contains("Run the verification before marking done"),
            "verify-then-done missing for in_progress: {r}"
        );
        assert!(
            r.contains("Don't paper over"),
            "no-paper-over missing for in_progress: {r}"
        );

        let plan_todo = make_plan(vec![step(
            "s1",
            "Run the tests",
            "cargo test. Verify all pass.",
            StepStatus::Todo,
        )]);
        let r = build_plan_reminder(PermissionMode::Auto, Some(&plan_todo)).unwrap();
        assert!(
            r.contains("Run the verification"),
            "verify-step missing for todo: {r}"
        );
    }

    #[test]
    fn reminder_plan_mode_with_plan_tells_model_to_wait() {
        use crate::tools::plan_state::StepStatus;
        let plan = make_plan(vec![
            step("s1", "A", "", StepStatus::Todo),
            step("s2", "B", "", StepStatus::Todo),
        ]);
        let r = build_plan_reminder(PermissionMode::Plan, Some(&plan)).unwrap();
        assert!(r.contains("awaiting user approval"));
        assert!(r.contains("2 steps"));
    }

    #[test]
    fn reminder_plan_mode_with_all_done_plan_falls_back_to_explore_reminder() {
        // M6.9 (Bug C1): an all-done plan still in the slot when the
        // user re-enters plan mode should NOT trigger "awaiting user
        // approval" — that plan finished. The (Plan, all-done) case
        // recurses to (Plan, None) and returns the
        // explore-then-SubmitPlan reminder so the model treats it as
        // a fresh planning phase.
        use crate::tools::plan_state::StepStatus;
        let plan = make_plan(vec![
            step("s1", "Build", "", StepStatus::Done),
            step("s2", "Test", "", StepStatus::Done),
        ]);
        let r = build_plan_reminder(PermissionMode::Plan, Some(&plan)).unwrap();
        assert!(
            !r.contains("awaiting user approval"),
            "all-done plan must NOT trigger awaiting-approval reminder: {r}",
        );
        assert!(
            r.contains("Plan mode is active"),
            "must fall through to the (Plan, None) explore reminder: {r}",
        );
        assert!(
            r.contains("SubmitPlan"),
            "must invite the model to submit a fresh plan: {r}",
        );
    }

    #[test]
    fn reminder_no_plan_no_mode_returns_none() {
        assert!(build_plan_reminder(PermissionMode::Auto, None).is_none());
        assert!(build_plan_reminder(PermissionMode::Ask, None).is_none());
    }

    // ── M6.1 per-step continuation prompt shape tests ──────────────────

    #[test]
    fn step_continuation_prompt_first_attempt_is_terse_and_directive() {
        use crate::tools::plan_state::StepStatus;
        let plan = make_plan(vec![
            step("s1", "Scaffold project", "", StepStatus::Done),
            step(
                "s2",
                "Install deps",
                "Run `npm install`. Verify: lockfile exists.",
                StepStatus::Todo,
            ),
            step("s3", "Build", "", StepStatus::Todo),
        ]);
        let p = build_step_continuation_prompt(&plan, &plan.steps[1], 1);

        // Names the step position and title so the model knows which
        // step is in scope.
        assert!(p.contains("step 2/3"), "missing position/total: {p}");
        assert!(p.contains("Install deps"), "missing step title: {p}");

        // Includes the in_progress transition for a Todo step.
        assert!(
            p.contains("UpdatePlanStep(\"s2\", \"in_progress\")"),
            "missing begin transition: {p}",
        );
        // Mentions the done/failed terminal transitions.
        assert!(
            p.contains("\"done\"") && p.contains("\"failed\""),
            "missing terminal transitions: {p}",
        );

        // First-attempt prompt should NOT include the escalation
        // language about retry budgets — that's reserved for higher
        // attempt counts.
        assert!(
            !p.contains("attempts per step"),
            "first-attempt prompt should not warn about retry budget: {p}",
        );
    }

    #[test]
    fn step_continuation_prompt_in_progress_step_skips_begin_transition() {
        use crate::tools::plan_state::StepStatus;
        let plan = make_plan(vec![
            step("s1", "Already running", "do work", StepStatus::InProgress),
            step("s2", "Next", "", StepStatus::Todo),
        ]);
        let p = build_step_continuation_prompt(&plan, &plan.steps[0], 1);

        // Don't ask the model to mark in_progress when it's already
        // in_progress — wastes a tool call and confuses the gate.
        assert!(
            !p.contains("\"in_progress\""),
            "in_progress step should not be told to begin again: {p}",
        );
        assert!(
            p.contains("already on this step"),
            "missing in-progress hint: {p}"
        );
        // Still mentions terminal transitions.
        assert!(p.contains("\"done\""));
        assert!(p.contains("\"failed\""));
    }

    #[test]
    fn step_continuation_prompt_escalates_on_repeated_attempts() {
        use crate::tools::plan_state::StepStatus;
        let plan = make_plan(vec![step(
            "s1",
            "Build release",
            "cargo build --release",
            StepStatus::InProgress,
        )]);
        let p2 = build_step_continuation_prompt(&plan, &plan.steps[0], 2);

        // Escalation must mention the attempt number and the bounded
        // retry budget — that's the whole reason this prompt exists.
        assert!(p2.contains("attempt 2"), "missing attempt number: {p2}");
        assert!(
            p2.contains("attempts per step"),
            "missing budget warning: {p2}"
        );
        assert!(
            p2.contains("Failed automatically"),
            "must tell model the driver will force-Failed: {p2}",
        );
        // Retry / Skip / Abort path is the user's recovery contract;
        // the model needs to know its "failed with note" call is the
        // honest path forward.
        assert!(
            p2.contains("retry / skip / abort") || p2.contains("Retry") || p2.contains("user"),
            "must reference user recovery path: {p2}",
        );
    }

    // ── M6.3 step-output surfacing ─────────────────────────────────────

    #[test]
    fn step_continuation_prompt_surfaces_prior_step_outputs() {
        // A 3-step plan where steps 1 and 2 are Done with outputs;
        // step 3 is the focus. Both prior outputs must appear in the
        // prompt so the model can read them without relying on chat
        // history (which gets compacted in M6.2).
        use crate::tools::plan_state::StepStatus;
        let mut s1 = step("s1", "Generate user id", "", StepStatus::Done);
        s1.output = Some("user-id: abc-123".into());
        let mut s2 = step("s2", "Hash password", "", StepStatus::Done);
        s2.output = Some("hash: $argon2id$xyz".into());
        let s3 = step("s3", "Persist to db", "", StepStatus::Todo);
        let plan = make_plan(vec![s1, s2, s3]);

        let p = build_step_continuation_prompt(&plan, &plan.steps[2], 1);

        assert!(
            p.contains("Outputs from prior steps"),
            "section header missing: {p}"
        );
        assert!(p.contains("user-id: abc-123"), "step 1 output missing: {p}");
        assert!(
            p.contains("hash: $argon2id$xyz"),
            "step 2 output missing: {p}"
        );
    }

    #[test]
    fn step_continuation_prompt_omits_outputs_section_when_none() {
        // No prior outputs → no section header in the prompt (clean
        // signal-to-noise: don't surface an empty rubric).
        use crate::tools::plan_state::StepStatus;
        let plan = make_plan(vec![
            step("s1", "Done", "", StepStatus::Done), // no output set
            step("s2", "Focus", "", StepStatus::Todo),
        ]);
        let p = build_step_continuation_prompt(&plan, &plan.steps[1], 1);
        assert!(
            !p.contains("Outputs from prior steps"),
            "section should be elided when empty: {p}",
        );
    }

    #[test]
    fn step_continuation_prompt_excludes_failed_step_outputs() {
        // A Failed step's `output` (if any was set before it failed)
        // is NOT a stable cross-step contract — surface only Done
        // step outputs.
        use crate::tools::plan_state::StepStatus;
        let mut s1 = step("s1", "Tried something", "", StepStatus::Failed);
        s1.output = Some("partial-result-DO-NOT-USE".into());
        let s2 = step("s2", "Focus", "", StepStatus::Todo);
        let plan = make_plan(vec![s1, s2]);

        let p = build_step_continuation_prompt(&plan, &plan.steps[1], 1);
        assert!(
            !p.contains("partial-result-DO-NOT-USE"),
            "failed step output must not leak into prompt: {p}",
        );
    }

    #[test]
    fn execution_reminder_surfaces_prior_step_outputs() {
        // Same M6.3 surfacing in the system reminder that the agent
        // injects every turn — keeps prior outputs visible even when
        // the user prompt slot is occupied by the current turn's
        // dialog.
        use crate::tools::plan_state::StepStatus;
        let mut s1 = step("s1", "Build artifact", "", StepStatus::Done);
        s1.output = Some("path: target/release/foo".into());
        let s2 = step("s2", "Run tests", "", StepStatus::InProgress);
        let plan = make_plan(vec![s1, s2]);

        let r = build_plan_reminder(PermissionMode::Auto, Some(&plan)).unwrap();
        assert!(
            r.contains("Outputs from prior steps"),
            "system reminder must surface outputs: {r}",
        );
        assert!(
            r.contains("path: target/release/foo"),
            "step 1 output missing in reminder: {r}",
        );
    }

    // ── M6.6 todos.md reminder tests ───────────────────────────────────

    /// Helper: run a closure with cwd switched into a temp dir so
    /// build_todos_reminder reads from the test's controlled location.
    /// Restores cwd afterwards. Tests are run sequentially within
    /// agent::tests so cwd contention is bounded; if this becomes a
    /// hot spot, we'd add a Mutex like plan_state's test_lock.
    fn with_cwd<R>(dir: &std::path::Path, f: impl FnOnce() -> R) -> R {
        // Synchronise cwd-touching tests in this module so they don't
        // race when cargo runs them in parallel — `set_current_dir`
        // is process-global and the previous tests in the file don't
        // touch cwd, so a Mutex inside agent::tests is enough.
        static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior = std::env::current_dir().expect("cwd readable");
        std::env::set_current_dir(dir).expect("cwd to test dir");
        let out = f();
        std::env::set_current_dir(prior).expect("cwd restore");
        out
    }

    #[test]
    fn todos_reminder_returns_none_when_file_missing() {
        let tmp = tempdir().unwrap();
        let r = with_cwd(tmp.path(), build_todos_reminder);
        assert!(r.is_none(), "no .thclaws/todos.md → no reminder");
    }

    #[test]
    fn todos_reminder_returns_none_when_file_empty() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".thclaws")).unwrap();
        std::fs::write(tmp.path().join(".thclaws/todos.md"), "").unwrap();
        let r = with_cwd(tmp.path(), build_todos_reminder);
        assert!(r.is_none(), "empty file → no reminder");
    }

    #[test]
    fn todos_reminder_returns_none_when_all_completed() {
        // A list where everything is checked off shouldn't nag the
        // model — there's nothing to resume.
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".thclaws")).unwrap();
        std::fs::write(
            tmp.path().join(".thclaws/todos.md"),
            "# Todos\n\n- [x] Done thing (id: 1)\n- [x] Other done thing (id: 2)\n",
        )
        .unwrap();
        let r = with_cwd(tmp.path(), build_todos_reminder);
        assert!(r.is_none(), "all-completed list → no reminder");
    }

    #[test]
    fn todos_reminder_surfaces_pending_items() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".thclaws")).unwrap();
        std::fs::write(
            tmp.path().join(".thclaws/todos.md"),
            "# Todos\n\n- [ ] Add tests (id: 1)\n- [-] Fix bug (id: 2)\n- [x] Old task (id: 3)\n",
        )
        .unwrap();
        let r = with_cwd(tmp.path(), build_todos_reminder).expect("reminder fires");
        // Header naming the file path so the model sees what the source is.
        assert!(r.contains(".thclaws/todos.md"), "missing file path: {r}");
        // Anti-ask framing — the rule that gpt-4.1 violated in the
        // M6.6 manual test.
        assert!(
            r.contains("before asking the user")
                || r.contains("before asking what")
                || r.contains("Don't ask"),
            "missing anti-ask framing: {r}",
        );
        // Surfaces the actual list contents verbatim.
        assert!(
            r.contains("Add tests"),
            "pending item missing in reminder: {r}"
        );
        assert!(
            r.contains("Fix bug"),
            "in_progress item missing in reminder: {r}"
        );
        // Tells the model how to act — flip via TodoWrite.
        assert!(
            r.contains("TodoWrite"),
            "must point at TodoWrite for the action: {r}"
        );
        assert!(
            r.contains("in_progress") || r.contains("resume"),
            "must mention resume/in_progress flow: {r}",
        );
    }

    #[test]
    fn todos_reminder_fires_when_only_one_in_progress_item() {
        // Edge case: a single `[-]` (in_progress) item with everything
        // else `[x]` should still fire — the user paused mid-task.
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".thclaws")).unwrap();
        std::fs::write(
            tmp.path().join(".thclaws/todos.md"),
            "# Todos\n\n- [x] Done (id: 1)\n- [-] Halfway (id: 2)\n",
        )
        .unwrap();
        let r = with_cwd(tmp.path(), build_todos_reminder).expect("must fire");
        assert!(r.contains("Halfway"), "in_progress item must surface: {r}");
    }

    #[test]
    fn step_continuation_prompt_no_op_for_done_or_failed_steps() {
        // The driver shouldn't be calling this for Done/Failed steps,
        // but be defensive — the prompt must not pretend to drive the
        // model on a step it shouldn't touch.
        use crate::tools::plan_state::StepStatus;
        let plan = make_plan(vec![
            step("s1", "Done step", "", StepStatus::Done),
            step("s2", "Failed step", "", StepStatus::Failed),
        ]);
        let pd = build_step_continuation_prompt(&plan, &plan.steps[0], 1);
        let pf = build_step_continuation_prompt(&plan, &plan.steps[1], 1);

        // Neither prompt should issue a UpdatePlanStep("in_progress")
        // call — Done is terminal, Failed is awaiting user action.
        assert!(
            !pd.contains("\"in_progress\""),
            "Done step should not be re-started: {pd}"
        );
        assert!(
            !pf.contains("\"in_progress\""),
            "Failed step requires explicit user retry: {pf}"
        );
    }

    /// A provider impl that plays back pre-canned event sequences, one per
    /// call to `stream()`. Panics (via error) if the test runs out of scripts.
    struct ScriptedProvider {
        scripts: Arc<Mutex<VecDeque<Vec<ProviderEvent>>>>,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Vec<ProviderEvent>>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Arc::new(Mutex::new(VecDeque::from(scripts))),
            })
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn stream(&self, _req: StreamRequest) -> Result<EventStream> {
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::Provider("no more scripts".into()))?;
            let events: Vec<Result<ProviderEvent>> = script.into_iter().map(Ok).collect();
            Ok(Box::pin(stream::iter(events)))
        }
    }

    fn text_script(chunks: &[&str]) -> Vec<ProviderEvent> {
        let mut out = vec![ProviderEvent::MessageStart {
            model: "test".into(),
        }];
        for c in chunks {
            out.push(ProviderEvent::TextDelta((*c).to_string()));
        }
        out.push(ProviderEvent::ContentBlockStop);
        out.push(ProviderEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        });
        out
    }

    fn tool_script(id: &str, name: &str, args_json: &str) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::MessageStart {
                model: "test".into(),
            },
            ProviderEvent::ToolUseStart {
                id: id.into(),
                name: name.into(),
                thought_signature: None,
            },
            ProviderEvent::ToolUseDelta {
                partial_json: args_json.into(),
            },
            ProviderEvent::ContentBlockStop,
            ProviderEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ]
    }

    #[tokio::test]
    async fn text_only_turn_returns_combined_text() {
        let provider = ScriptedProvider::new(vec![text_script(&["Hello, ", "world!"])]);
        let agent = Agent::new(provider, ToolRegistry::new(), "test-model", "");

        let outcome = collect_agent_turn(agent.run_turn("hi".into()))
            .await
            .unwrap();
        assert_eq!(outcome.text, "Hello, world!");
        assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(outcome.iterations, 1);
        assert!(outcome.tool_calls.is_empty());

        let history = agent.history_snapshot();
        // user → assistant
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[1].role, Role::Assistant);
    }

    #[tokio::test]
    async fn tool_use_executes_and_continues_next_iteration() {
        // Turn 1: assistant requests Read. Turn 2: assistant returns text.
        let dir = tempdir().unwrap();
        let path = dir.path().join("note.txt");
        std::fs::write(&path, "the contents\n").unwrap();

        let args = serde_json::json!({ "path": path.to_string_lossy() }).to_string();
        let provider = ScriptedProvider::new(vec![
            tool_script("toolu_1", "Read", &args),
            text_script(&["I read: the contents."]),
        ]);

        let agent = Agent::new(provider, ToolRegistry::with_builtins(), "test-model", "");

        let outcome = collect_agent_turn(agent.run_turn("read it".into()))
            .await
            .unwrap();
        assert_eq!(outcome.text, "I read: the contents.");
        assert_eq!(outcome.tool_calls, vec!["Read".to_string()]);
        assert_eq!(outcome.iterations, 2);
        assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));

        // History: user(hi), assistant(tool_use), user(tool_result), assistant(text)
        let history = agent.history_snapshot();
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[1].role, Role::Assistant);
        assert!(matches!(
            history[1].content[0],
            ContentBlock::ToolUse { .. }
        ));
        assert_eq!(history[2].role, Role::User);
        assert!(matches!(
            history[2].content[0],
            ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ));
        assert_eq!(history[3].role, Role::Assistant);
    }

    #[tokio::test]
    async fn tool_error_surfaces_as_tool_result_is_error_and_loop_continues() {
        // Tool is Read with a path that doesn't exist → Tool error.
        // Then the scripted provider emits a final text turn acknowledging.
        let provider = ScriptedProvider::new(vec![
            tool_script("toolu_1", "Read", r#"{"path":"/nope/does/not/exist"}"#),
            text_script(&["handled the error"]),
        ]);
        let agent = Agent::new(provider, ToolRegistry::with_builtins(), "test-model", "");

        let outcome = collect_agent_turn(agent.run_turn("try it".into()))
            .await
            .unwrap();
        assert_eq!(outcome.text, "handled the error");
        assert_eq!(outcome.iterations, 2);

        let history = agent.history_snapshot();
        let tool_result_msg = &history[2];
        if let ContentBlock::ToolResult {
            is_error, content, ..
        } = &tool_result_msg.content[0]
        {
            assert!(*is_error, "expected is_error=true for failed tool");
            let text = content.to_text();
            assert!(text.contains("error:"), "got: {text}");
        } else {
            panic!("expected tool_result block");
        }
    }

    #[tokio::test]
    async fn ask_mode_approves_and_runs_mutating_tool() {
        use crate::permissions::{PermissionMode, ScriptedApprover};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let args = serde_json::json!({
            "path": path.to_string_lossy(),
            "content": "hello",
        })
        .to_string();

        let provider = ScriptedProvider::new(vec![
            tool_script("toolu_1", "Write", &args),
            text_script(&["done"]),
        ]);
        let approver = ScriptedApprover::new(vec![ApprovalDecision::Allow]);
        let agent = Agent::new(provider, ToolRegistry::with_builtins(), "test", "")
            .with_permission_mode(PermissionMode::Ask)
            .with_approver(approver);

        let outcome = collect_agent_turn(agent.run_turn("write it".into()))
            .await
            .unwrap();
        assert_eq!(outcome.text, "done");
        assert_eq!(outcome.tool_calls, vec!["Write".to_string()]);
        assert!(outcome.tool_denials.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[tokio::test]
    async fn ask_mode_denies_and_surfaces_error_result() {
        use crate::permissions::{DenyApprover, PermissionMode};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let args = serde_json::json!({
            "path": path.to_string_lossy(),
            "content": "hello",
        })
        .to_string();

        let provider = ScriptedProvider::new(vec![
            tool_script("toolu_1", "Write", &args),
            text_script(&["ack"]),
        ]);
        let agent = Agent::new(provider, ToolRegistry::with_builtins(), "test", "")
            .with_permission_mode(PermissionMode::Ask)
            .with_approver(Arc::new(DenyApprover));

        let outcome = collect_agent_turn(agent.run_turn("write it".into()))
            .await
            .unwrap();

        // Write never executed:
        assert!(!path.exists());
        // Denial was surfaced:
        assert_eq!(outcome.tool_denials, vec!["Write".to_string()]);
        // M6.17 BUG L1: ToolCallStart fires at parse time (before the
        // approval gate), so denied calls now appear in tool_calls
        // alongside tool_denials. Pre-fix this asserted empty because
        // the start event came AFTER approval; the new contract is
        // "tool_calls = parsed-from-model", "tool_denials = of those,
        // which were rejected".
        assert_eq!(outcome.tool_calls, vec!["Write".to_string()]);

        // The tool_result block in history should be is_error=true with a "denied" marker.
        let history = agent.history_snapshot();
        let tool_result_msg = history.iter().find_map(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::ToolResult {
                    content, is_error, ..
                } if *is_error => Some(content.clone()),
                _ => None,
            })
        });
        let content = tool_result_msg.expect("denied tool_result not in history");
        let text = content.to_text();
        assert!(text.contains("denied"), "got: {text}");
    }

    #[tokio::test]
    async fn ask_mode_skips_approval_for_read_only_tools() {
        use crate::permissions::{DenyApprover, PermissionMode};
        use tempfile::tempdir;

        // Write a file so Read has something to see.
        let dir = tempdir().unwrap();
        let path = dir.path().join("x.txt");
        std::fs::write(&path, "payload").unwrap();
        let args = serde_json::json!({ "path": path.to_string_lossy() }).to_string();

        // DenyApprover would deny any tool that requires approval, but Read
        // is read-only so the approver should never be consulted.
        let provider = ScriptedProvider::new(vec![
            tool_script("toolu_1", "Read", &args),
            text_script(&["ok"]),
        ]);
        let agent = Agent::new(provider, ToolRegistry::with_builtins(), "test", "")
            .with_permission_mode(PermissionMode::Ask)
            .with_approver(Arc::new(DenyApprover));

        let outcome = collect_agent_turn(agent.run_turn("read it".into()))
            .await
            .unwrap();
        assert_eq!(outcome.tool_calls, vec!["Read".to_string()]);
        assert!(outcome.tool_denials.is_empty());
    }

    #[tokio::test]
    async fn auto_mode_bypasses_approver_entirely() {
        use crate::permissions::{DenyApprover, PermissionMode};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let path = dir.path().join("auto.txt");
        let args = serde_json::json!({
            "path": path.to_string_lossy(),
            "content": "ok",
        })
        .to_string();
        let provider = ScriptedProvider::new(vec![
            tool_script("toolu_1", "Write", &args),
            text_script(&["done"]),
        ]);
        // DenyApprover would veto — but Auto mode should never consult it.
        let agent = Agent::new(provider, ToolRegistry::with_builtins(), "test", "")
            .with_permission_mode(PermissionMode::Auto)
            .with_approver(Arc::new(DenyApprover));

        let _ = collect_agent_turn(agent.run_turn("write".into()))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ok");
    }

    // M6.20 BUG M1 regression note: the TodoWrite plan-mode block
    // (agent.rs:1133 area) now fires BEFORE the generic mutating-tool
    // block, so the model gets the SubmitPlan-specific message instead
    // of the generic "Use Read/Grep/Glob/Ls" one. A behavioral test
    // would need to set `permissions::current_mode()` to Plan, which
    // races other tests reading the same global slot — so this fix is
    // verified by source inspection + manual repro rather than a
    // dedicated unit test. The cross-test pollution would cause flakes
    // in `permission_denied_in_ask_mode_emits_denial_event` and
    // similar tests that depend on `current_mode() == Ask`.

    #[tokio::test]
    async fn max_iterations_short_circuits_runaway_loops() {
        // Infinite tool loop: every script turn returns a tool_use.
        let loop_script = || tool_script("toolu_loop", "Read", r#"{"path":"/nope"}"#);
        let provider = ScriptedProvider::new(vec![
            loop_script(),
            loop_script(),
            loop_script(),
            loop_script(),
            loop_script(),
        ]);
        let agent = Agent::new(provider, ToolRegistry::with_builtins(), "test-model", "")
            .with_max_iterations(2);

        let outcome = collect_agent_turn(agent.run_turn("loop".into()))
            .await
            .unwrap();
        assert_eq!(outcome.iterations, 2);
        assert_eq!(outcome.stop_reason.as_deref(), Some("max_iterations"));
    }

    /// M6.18 BUG H1: compaction now subtracts the system-prompt size
    /// from the budget before trimming messages, so a large system
    /// prompt + budget-filling history can't push the total request
    /// past the model's context window. Pre-fix `compact()` was
    /// called with the full budget, so a 50K system prompt + 128K
    /// "fitted" messages = 178K request that 400'd on a 128K-context
    /// model.
    ///
    /// We probe the deduction via a fake provider that captures the
    /// inbound StreamRequest's message-token total. The system prompt
    /// is sized to consume most of the budget; messages should be
    /// trimmed accordingly.
    #[tokio::test]
    async fn compact_subtracts_system_prompt_tokens_from_budget() {
        use std::sync::Mutex;

        // Capture the messages count of every StreamRequest the
        // provider receives.
        struct CapturingProvider {
            captured_messages: Arc<Mutex<Vec<usize>>>,
        }
        #[async_trait]
        impl Provider for CapturingProvider {
            async fn stream(&self, req: crate::providers::StreamRequest) -> Result<EventStream> {
                self.captured_messages
                    .lock()
                    .unwrap()
                    .push(req.messages.len());
                // Single-text response, no tool use → end of turn.
                Ok(Box::pin(stream::iter(vec![
                    Ok(ProviderEvent::TextDelta("ok".into())),
                    Ok(ProviderEvent::ContentBlockStop),
                    Ok(ProviderEvent::MessageStop {
                        stop_reason: Some("end_turn".into()),
                        usage: None,
                    }),
                ])))
            }
        }

        let captured: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(CapturingProvider {
            captured_messages: captured.clone(),
        });

        // Build an Agent with a big system prompt and small budget.
        // budget=1000 tokens, system≈800 tokens → messages_budget ≈ 200.
        let big_system = "x".repeat(3200); // ~800 tokens at 4 chars/token
        let mut agent = Agent::new(provider, ToolRegistry::default(), "test-model", big_system);
        agent.budget_tokens = 1000;

        // Pre-load history with a multi-turn conversation that, naively
        // counted, would fit in 1000 tokens but won't fit once we
        // subtract 800 + 1024 reserve. Expect compact to drop most.
        let pre = vec![
            Message::user("a".repeat(200)),
            Message::assistant("b".repeat(200)),
            Message::user("c".repeat(200)),
            Message::assistant("d".repeat(200)),
            Message::user("trigger"),
        ];
        agent.set_history(pre);

        let _ = collect_agent_turn(agent.run_turn("noop".into())).await;

        let counts = captured.lock().unwrap().clone();
        assert!(
            !counts.is_empty(),
            "provider should have received a request"
        );
        // Pre-fix this would be 6 (5 history + 1 new user msg = full
        // history sent unchanged because compact got the full 1000-
        // token budget). Post-fix: messages_budget is negative-clamped
        // to 0, so compact aggressively drops to the minimum (1 msg).
        assert!(
            counts[0] <= 2,
            "expected aggressive compaction (≤2 messages); got {} — system prompt deduction not applied",
            counts[0]
        );
    }

    /// M6.17 BUG H1 + M3: when a cancel token is wired in and gets
    /// fired during the retry-backoff sleep, the agent stream errors
    /// out with a "cancelled by user" message instead of waiting the
    /// full 1+2+4 = 7 s backoff cycle. Pre-fix the sleep blocked
    /// uninterruptibly.
    #[tokio::test]
    async fn cancel_during_retry_sleep_short_circuits() {
        // Provider that always returns a transient error so the agent
        // hits the retry path on every attempt.
        struct AlwaysErrProvider;
        #[async_trait]
        impl Provider for AlwaysErrProvider {
            async fn stream(&self, _req: crate::providers::StreamRequest) -> Result<EventStream> {
                Err(Error::Provider("transient".into()))
            }
        }

        let token = crate::cancel::CancelToken::new();
        let agent = Agent::new(
            Arc::new(AlwaysErrProvider),
            ToolRegistry::default(),
            "test-model",
            "",
        )
        .with_cancel(token.clone());

        // Fire cancel after 100 ms so it hits during the first
        // 1-second backoff sleep (well before the would-be 7s total).
        let token_for_cancel = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            token_for_cancel.cancel();
        });

        let started = std::time::Instant::now();
        let result = collect_agent_turn(agent.run_turn("hi".into())).await;
        let elapsed = started.elapsed();

        assert!(
            result.is_err(),
            "expected agent stream to error after cancellation"
        );
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("cancelled"),
            "expected cancel message, got: {msg}"
        );
        // Without the fix, this would be ~7 s.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "cancel didn't short-circuit the retry backoff (took {elapsed:?})",
        );
    }
}
