//! Message-window compaction with summarization support.
//!
//! Two strategies:
//! 1. **`compact()`** — fast, synchronous drop-oldest. Used as a fallback.
//! 2. **`compact_with_summary()`** — async, uses the provider to summarize
//!    older messages into a compact text block before dropping. Preserves
//!    more context quality at the cost of an extra API call.
//!
//! The agent loop calls `compact_with_summary()` when a provider is
//! available, falling back to `compact()` on error.

use crate::tokens::estimate_tokens;
use crate::types::{ContentBlock, Message, Role, ToolResultContent};

/// Plan-mode tool names whose tool_result content is always preserved
/// during step-boundary compaction. Their outputs are short (a status
/// confirmation, an updated plan id) and they're the breadcrumbs the
/// model relies on to know what's done — dropping them would force the
/// model to re-discover plan state every turn.
const PLAN_PRESERVE_TOOLS: &[&str] = &[
    "SubmitPlan",
    "UpdatePlanStep",
    "EnterPlanMode",
    "ExitPlanMode",
];

/// Per-step prompt prefixes the M6.1 driver and the sidebar IPC
/// handlers inject to wake the agent loop on each step boundary. Used
/// to identify "the most recent step boundary" in history when
/// deciding which messages are eligible for compaction.
///
/// M6.9 (Bug F1): added `"Step ("` (Skip handler) and `"Continue with
/// the plan"` (stalled-Continue handler). Earlier versions only listed
/// the driver-injected prefixes, so user-initiated Skip / Continue
/// woke the agent without registering as a boundary — the next
/// compaction either used a stale boundary or no-op'd entirely.
const STEP_BOUNDARY_PREFIXES: &[&str] = &[
    "Continue plan execution",   // M6.1 driver per-step prompt
    "Begin executing the plan.", // sidebar Approve auto-nudge
    "Retry the failed step",     // sidebar Retry IPC
    "Step (",                    // sidebar Skip IPC ("Step (\"...\") was skipped...")
    "Continue with the plan",    // stalled-Continue IPC ("Continue with the plan. ...")
];

/// Approximate the token cost of a single message.
pub fn estimate_message_tokens(m: &Message) -> usize {
    let mut chunks: Vec<String> = Vec::new();
    for block in &m.content {
        match block {
            ContentBlock::Text { text } => chunks.push(text.clone()),
            ContentBlock::Thinking { content, .. } => chunks.push(content.clone()),
            ContentBlock::ToolUse { name, input, .. } => {
                chunks.push(name.clone());
                chunks.push(input.to_string());
            }
            ContentBlock::ToolResult { content, .. } => chunks.push(content.to_text()),
            // Vision input billing on Anthropic / OpenAI / Gemini lands
            // around ~258 tokens for a typical screenshot-sized image
            // (1568px max edge). We bake that as a fixed estimate so
            // the compactor's budget math doesn't ignore attached
            // images entirely — accuracy isn't critical here, just
            // "does it cross the threshold."
            ContentBlock::Image { .. } => chunks.push("x".repeat(258 * 4)),
        }
    }
    estimate_tokens(&chunks.join(" "))
}

/// Sum message tokens across a slice.
pub fn estimate_messages_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

/// Fast synchronous compaction: drop oldest messages until under budget.
/// Preserves tool_use/tool_result pairs — never splits a tool call from its
/// result, as that would confuse the provider.
///
/// M6.17 BUG M1: when only one message remains and it still exceeds the
/// budget (rare — typically a huge tool_result or a pasted-in user
/// message larger than the model's context window), the over-budget
/// content is truncated in-place via [`truncate_oversized_message`]
/// rather than sent through to the provider, which would respond with
/// a 400 "context length exceeded" error. The model sees the truncation
/// notice in the message body and can ask the user / re-fetch.
pub fn compact(messages: &[Message], budget_tokens: usize) -> Vec<Message> {
    if messages.is_empty() {
        return Vec::new();
    }
    let mut start = 0;
    while start < messages.len().saturating_sub(1)
        && estimate_messages_tokens(&messages[start..]) > budget_tokens
    {
        start += 1;
        // Don't split tool_use from its tool_result: if message at `start`
        // contains a ToolResult, skip it too (drop the orphaned result).
        if start < messages.len() {
            let has_tool_result = messages[start]
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
            if has_tool_result {
                start += 1;
            }
        }
    }
    let mut result = messages[start..].to_vec();
    // M6.17 BUG M1: only-one-message-and-still-too-big rescue.
    if result.len() == 1 && estimate_message_tokens(&result[0]) > budget_tokens {
        truncate_oversized_message(&mut result[0], budget_tokens);
    }
    result
}

/// In-place truncation of every Text / ToolResult block in `msg` so the
/// total estimated tokens fits under `budget_tokens`. Conservative: each
/// block is truncated independently to roughly `budget * 3` chars (~1
/// token = 3-4 chars, generous head-room since estimate_tokens already
/// rounds up). Truncation is char-boundary-safe and appends a notice
/// the model can read. M6.17 BUG M1 helper.
fn truncate_oversized_message(msg: &mut Message, budget_tokens: usize) {
    let target_chars = budget_tokens.saturating_mul(3).max(1024);
    let notice = format!(
        "\n\n[...truncated by thClaws: original content exceeded the {} token context budget]",
        budget_tokens
    );
    for block in &mut msg.content {
        match block {
            ContentBlock::Text { text } => {
                if text.len() > target_chars {
                    *text = format!("{}{notice}", char_safe_head(text, target_chars));
                }
            }
            ContentBlock::ToolResult { content, .. } => {
                let s = content.to_text();
                if s.len() > target_chars {
                    let truncated = format!("{}{notice}", char_safe_head(&s, target_chars));
                    *content = crate::types::ToolResultContent::Text(truncated);
                }
            }
            // Thinking blocks pass through (provider drops them on echo
            // anyway). Image / ToolUse blocks are bounded in size.
            _ => {}
        }
    }
}

/// Slice the longest prefix of `s` that's ≤ `max_bytes` AND lands on a
/// UTF-8 char boundary. `&s[..n]` panics on a non-boundary byte index;
/// this picks the largest safe `n`.
fn char_safe_head(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Summarize older messages into a compact text block, then keep only
/// the summary + recent messages. Returns the compacted message list.
///
/// Strategy:
/// 1. Split messages into "old" (to be summarized) and "recent" (to keep).
///    Keep at least the last 4 messages (2 turns) untouched.
/// 2. Render old messages into a text block for the summarizer.
/// 3. Call the provider to generate a summary (max 2K tokens output).
/// 4. Prepend a synthetic user message with the summary, then append recent.
/// 5. If the API call fails, fall back to drop-oldest.
pub async fn compact_with_summary(
    messages: &[Message],
    budget_tokens: usize,
    provider: &dyn crate::providers::Provider,
    model: &str,
) -> Vec<Message> {
    if messages.is_empty() {
        return Vec::new();
    }

    let total = estimate_messages_tokens(messages);
    if total <= budget_tokens {
        return messages.to_vec();
    }

    // Keep at least the last 4 messages (2 user-assistant turns).
    let keep_recent = messages.len().min(4).max(1);
    let split_at = messages.len().saturating_sub(keep_recent);
    if split_at == 0 {
        return compact(messages, budget_tokens);
    }

    let old = &messages[..split_at];
    let recent = &messages[split_at..];

    // Render old messages into a summarizable text.
    let rendered = render_for_summary(old);
    if rendered.is_empty() {
        return compact(messages, budget_tokens);
    }

    // Ask the provider to summarize.
    let summary_prompt = crate::prompts::render_named(
        "compaction",
        crate::prompts::defaults::COMPACTION,
        &[("conversation", &rendered)],
    );
    let summary_system = crate::prompts::load(
        "compaction_system",
        crate::prompts::defaults::COMPACTION_SYSTEM,
    );

    let req = crate::providers::StreamRequest {
        model: model.to_string(),
        system: Some(summary_system),
        messages: vec![Message::user(summary_prompt)],
        tools: vec![],
        max_tokens: 2048,
        thinking_budget: None,
    };

    match provider.stream(req).await {
        Ok(stream) => {
            let result = crate::providers::collect_turn(crate::providers::assemble(stream)).await;
            match result {
                Ok(turn) if !turn.text.is_empty() => {
                    let mut out = Vec::with_capacity(1 + recent.len());
                    // Synthetic summary message as a system-context user message.
                    out.push(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text {
                            text: format!(
                                "[Conversation summary — earlier messages were compacted]\n\n{}",
                                turn.text
                            ),
                        }],
                    });
                    out.extend_from_slice(recent);
                    out
                }
                _ => compact(messages, budget_tokens),
            }
        }
        Err(_) => compact(messages, budget_tokens),
    }
}

/// Render messages into a human-readable text for summarization.
fn render_for_summary(messages: &[Message]) -> String {
    let mut lines = Vec::new();
    for m in messages {
        let role = match m.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => "System",
        };
        let text: String = m
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                // Drop reasoning from compaction summaries — it's
                // model-internal scratch work, not part of the user-visible
                // conversation. The compactor's goal is to preserve "what
                // happened" for later context, and the answer/tool calls
                // capture that without the chain-of-thought noise.
                ContentBlock::Thinking { .. } => None,
                ContentBlock::ToolUse { name, input, .. } => {
                    Some(format!("[Called tool: {name} with {}]", input))
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let prefix = if *is_error { "Error" } else { "Result" };
                    let preview: String = content.to_text().chars().take(500).collect();
                    Some(format!("[{prefix}: {preview}]"))
                }
                // User-attached images are summarized as a placeholder
                // in compaction notes; the actual pixels stay in the
                // un-compacted prefix the model still has access to.
                ContentBlock::Image { source, .. } => match source {
                    crate::types::ImageSource::Base64 { media_type, .. } => {
                        Some(format!("[Image attached: {media_type}]"))
                    }
                },
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            lines.push(format!("{role}: {text}"));
        }
    }
    lines.join("\n\n")
}

/// Step-boundary compaction (M6.2).
///
/// Called by the plan-execution driver when it detects a step
/// transition (the next-actionable step has a different id from the
/// step it was previously iterating on, AND at least one step in the
/// plan is now Done). Walks the message history in place and shrinks
/// the content of every non-plan-tool `ToolResult` block in messages
/// older than the most recent step-boundary user prompt — the bulky
/// outputs of `Edit` / `Write` / `Bash` / `Read` from completed steps
/// are replaced with a short placeholder, keeping the conversation
/// shape (tool_use ↔ tool_result pairing) intact while dropping the
/// token-heavy content.
///
/// Plan-tool results (`SubmitPlan`, `UpdatePlanStep`, `EnterPlanMode`,
/// `ExitPlanMode`) are preserved untouched: they're the breadcrumbs
/// the model uses to track plan progression, and they're already short.
///
/// Idempotent: re-running on already-compacted history is a near no-op
/// (the placeholder is already shorter than typical tool outputs, so
/// the size-check below skips it).
///
/// Returns the approximate number of bytes saved across all rewritten
/// `ToolResult` blocks — useful for the worker's `[compacted: …]`
/// debug notice.
pub fn compact_for_step_boundary(messages: &mut Vec<Message>) -> usize {
    // Find the most recent boundary marker: a User-role message whose
    // first text block starts with one of the driver's per-step
    // prompt prefixes. Anything BEFORE this index is "completed
    // steps' work" and eligible for compaction.
    let boundary_idx = messages.iter().enumerate().rev().find_map(|(i, m)| {
        if m.role != Role::User {
            return None;
        }
        let starts_with_prefix = m.content.iter().any(|b| match b {
            ContentBlock::Text { text } => {
                STEP_BOUNDARY_PREFIXES.iter().any(|p| text.starts_with(p))
            }
            _ => false,
        });
        if starts_with_prefix {
            Some(i)
        } else {
            None
        }
    });

    let Some(boundary) = boundary_idx else {
        // No boundary marker yet (model is still in the SubmitPlan or
        // pre-Approve phase) — nothing to compact.
        return 0;
    };

    // Collect tool_use_ids belonging to plan tools so we can exempt
    // their tool_results from compaction. Walk the entire history
    // (not just pre-boundary) because a plan-tool result might
    // straddle the boundary.
    let mut plan_tool_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in messages.iter() {
        for b in &m.content {
            if let ContentBlock::ToolUse { id, name, .. } = b {
                if PLAN_PRESERVE_TOOLS.contains(&name.as_str()) {
                    plan_tool_ids.insert(id.clone());
                }
            }
        }
    }

    let placeholder = "[compacted at step boundary]";
    let placeholder_len = placeholder.len();
    let mut bytes_saved = 0usize;

    for m in messages.iter_mut().take(boundary) {
        for b in m.content.iter_mut() {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } = b
            {
                if plan_tool_ids.contains(tool_use_id) {
                    continue;
                }
                let prior_len = content.to_text().len();
                if prior_len > placeholder_len {
                    bytes_saved += prior_len - placeholder_len;
                    *content = ToolResultContent::Text(placeholder.to_string());
                }
            }
        }
    }

    bytes_saved
}

/// Step-boundary clear (M6.4 opt-in).
///
/// Wipes the agent's chat history, keeping only the User message that
/// triggered the active plan — the prompt immediately preceding the
/// first `SubmitPlan` ToolUse (e.g. "now plan to build a webapp").
/// Everything else — pre-plan chat, plan submission tool_results,
/// prior step work — is dropped.
///
/// Why anchor to the plan trigger rather than `messages[0]`: a user
/// who chatted with the model BEFORE submitting a plan ("tell me
/// about Rust" → "now plan to build a webapp") would otherwise have
/// the unrelated chat preserved and the actual plan-triggering ask
/// dropped. M6.13 fix F2 — found during the M6.9 plan-mode audit.
///
/// The system reminder injects plan structure + current step + prior
/// outputs every turn, so the model has all the *plan* context it
/// needs from those channels. The kept User message provides the
/// *project* framing — "what is this plan for" — that the system
/// reminder doesn't carry.
///
/// Fallback chain when the heuristic can't find an anchor:
///   1. User message immediately preceding the first `SubmitPlan`
///      ToolUse (the happy path; covers >99% of real sessions)
///   2. First User message anywhere in history (no SubmitPlan found —
///      shouldn't happen mid-plan but handle defensively)
///   3. `messages[0]` (no User-role messages at all — shouldn't
///      happen, last-resort fallback)
///
/// Not idempotent in the same way as `compact_for_step_boundary`:
/// this drops messages outright. Re-running on already-cleared history
/// is a no-op (one message, nothing to drop).
///
/// Returns the number of messages dropped — useful for the `[cleared:
/// dropped N messages]` sidebar/CLI notice.
pub fn clear_for_step_boundary(messages: &mut Vec<Message>) -> usize {
    if messages.is_empty() {
        return 0;
    }
    let prior_len = messages.len();
    let keep_idx = pick_plan_trigger_index(messages);
    let kept = messages.remove(keep_idx);
    messages.clear();
    messages.push(kept);
    prior_len.saturating_sub(messages.len())
}

/// Find the index of the User message that triggered the plan — the
/// most recent User-role message BEFORE the first `SubmitPlan`
/// ToolUse. Falls back to the first User message, then `0`.
fn pick_plan_trigger_index(messages: &[Message]) -> usize {
    // Locate the first SubmitPlan ToolUse (the plan-submission point).
    let submit_plan_idx = messages.iter().position(|m| {
        m.content.iter().any(|b| {
            matches!(
                b,
                ContentBlock::ToolUse { name, .. } if name == "SubmitPlan"
            )
        })
    });

    // Walk back from there to find the most-recent User message
    // BEFORE SubmitPlan — that's the prompt that triggered the plan.
    if let Some(sp_idx) = submit_plan_idx {
        if let Some(user_idx) = messages[..sp_idx]
            .iter()
            .rposition(|m| m.role == Role::User)
        {
            return user_idx;
        }
    }

    // Fallback: first User message anywhere. This handles the
    // "no SubmitPlan in history" pathological case (shouldn't happen
    // mid-plan but be defensive).
    messages
        .iter()
        .position(|m| m.role == Role::User)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Role;

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[test]
    fn estimate_message_tokens_counts_text_block() {
        let m = text_msg(Role::User, &"a".repeat(28));
        assert_eq!(estimate_message_tokens(&m), 10);
    }

    #[test]
    fn estimate_message_tokens_sums_blocks() {
        let m = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "aaaaa".into(),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "id".into(),
                    content: "bbbbb".into(),
                    is_error: false,
                },
            ],
        };
        assert_eq!(estimate_message_tokens(&m), 4);
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(compact(&[], 100).is_empty());
    }

    #[test]
    fn under_budget_is_unchanged() {
        let msgs = vec![
            text_msg(Role::User, "hi"),
            text_msg(Role::Assistant, "hello"),
        ];
        let out = compact(&msgs, 10_000);
        assert_eq!(out, msgs);
    }

    #[test]
    fn over_budget_drops_oldest_first() {
        let s = "a".repeat(28);
        let msgs = vec![
            text_msg(Role::User, &s),
            text_msg(Role::Assistant, &s),
            text_msg(Role::User, &s),
            text_msg(Role::Assistant, &s),
        ];
        let out = compact(&msgs, 25);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], msgs[2]);
        assert_eq!(out[1], msgs[3]);
    }

    #[test]
    fn never_drops_below_last_message_and_truncates_oversize() {
        // M6.17 BUG M1: when only one message remains and it still
        // exceeds the budget, compact truncates the over-budget
        // content rather than silently sending a request larger than
        // the model's context window. Pre-fix this test asserted the
        // returned message was identical to the input — which was the
        // bug, because that single message would make the provider
        // 400 with "context length exceeded".
        let huge = "x".repeat(10_000);
        let msgs = vec![
            text_msg(Role::User, &huge),
            text_msg(Role::Assistant, &huge),
            text_msg(Role::User, &huge),
        ];
        let out = compact(&msgs, 1);
        assert_eq!(out.len(), 1, "always preserves at least the last message");
        // After truncation: significantly smaller than the original
        // 10K, with the truncation notice appended.
        let text = match &out[0].content[0] {
            ContentBlock::Text { text } => text.clone(),
            other => panic!("expected Text block, got {other:?}"),
        };
        assert!(
            text.len() < huge.len(),
            "expected truncation; got len={}",
            text.len()
        );
        assert!(
            text.contains("truncated by thClaws"),
            "expected truncation notice; got: {}",
            &text[text.len().saturating_sub(200)..]
        );
    }

    #[test]
    fn preserves_order() {
        let msgs = vec![
            text_msg(Role::User, "one"),
            text_msg(Role::Assistant, "two"),
            text_msg(Role::User, "three"),
            text_msg(Role::Assistant, "four"),
            text_msg(Role::User, "five"),
        ];
        let out = compact(&msgs, estimate_messages_tokens(&msgs[3..]));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], msgs[3]);
        assert_eq!(out[1], msgs[4]);
    }

    #[test]
    fn compaction_reduces_total_tokens_monotonically() {
        let s = "x".repeat(28);
        let msgs: Vec<Message> = (0..10).map(|_| text_msg(Role::User, &s)).collect();
        let before = estimate_messages_tokens(&msgs);
        let out = compact(&msgs, 50);
        let after = estimate_messages_tokens(&out);
        assert!(after <= 50, "after={after} > 50");
        assert!(
            after < before,
            "did not reduce: before={before} after={after}"
        );
    }

    #[test]
    fn render_for_summary_formats_messages() {
        let msgs = vec![
            text_msg(Role::User, "hello"),
            text_msg(Role::Assistant, "hi there"),
        ];
        let rendered = render_for_summary(&msgs);
        assert!(rendered.contains("User: hello"));
        assert!(rendered.contains("Assistant: hi there"));
    }

    // ── M6.2 step-boundary compaction tests ────────────────────────────

    fn assistant_with_tool_use(id: &str, name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input: serde_json::json!({}),
                thought_signature: None,
            }],
        }
    }

    fn user_with_tool_result(id: &str, content: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: ToolResultContent::Text(content.to_string()),
                is_error: false,
            }],
        }
    }

    fn first_tool_result_text(m: &Message) -> Option<String> {
        m.content.iter().find_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content.to_text()),
            _ => None,
        })
    }

    #[test]
    fn step_boundary_compaction_no_op_without_boundary_marker() {
        // No "Begin executing the plan." / "Continue plan execution"
        // user message yet — driver hasn't started, nothing to compact.
        let mut msgs = vec![
            text_msg(Role::User, "build a webapp"),
            assistant_with_tool_use("u1", "Bash"),
            user_with_tool_result("u1", "x".repeat(10_000).as_str()),
        ];
        let saved = compact_for_step_boundary(&mut msgs);
        assert_eq!(saved, 0);
        assert_eq!(
            first_tool_result_text(&msgs[2]).unwrap().len(),
            10_000,
            "tool result must not be touched without a boundary",
        );
    }

    #[test]
    fn step_boundary_compaction_shrinks_non_plan_tool_results_before_boundary() {
        let mut msgs = vec![
            text_msg(Role::User, "build a webapp"),
            assistant_with_tool_use("u1", "Bash"),
            user_with_tool_result("u1", &"x".repeat(10_000)), // bulky bash output
            text_msg(Role::User, "Begin executing the plan."), // boundary
            assistant_with_tool_use("u2", "Edit"),
            user_with_tool_result("u2", &"y".repeat(5_000)), // current step's work — keep
        ];
        let saved = compact_for_step_boundary(&mut msgs);

        // Bytes saved should approximately match the bash output size.
        assert!(saved > 9_000, "expected substantial savings, got {saved}");

        // Pre-boundary non-plan tool result is replaced with placeholder.
        assert_eq!(
            first_tool_result_text(&msgs[2]).unwrap(),
            "[compacted at step boundary]",
        );

        // Post-boundary tool result is untouched.
        assert_eq!(
            first_tool_result_text(&msgs[5]).unwrap().len(),
            5_000,
            "current-step tool result must not be compacted",
        );
    }

    #[test]
    fn step_boundary_compaction_preserves_plan_tool_results() {
        // UpdatePlanStep results are the breadcrumbs the model uses to
        // know what's done — must survive compaction even when older
        // than the boundary.
        let plan_result_body = "step s0 transitioned to done. plan now: s0=done, s1=todo";
        let mut msgs = vec![
            text_msg(Role::User, "build something"),
            assistant_with_tool_use("plan_call", "UpdatePlanStep"),
            user_with_tool_result("plan_call", plan_result_body),
            assistant_with_tool_use("bash_call", "Bash"),
            user_with_tool_result("bash_call", &"z".repeat(8_000)),
            text_msg(
                Role::User,
                "Continue plan execution. Focus: step 2/3 \"Build\".",
            ),
        ];
        compact_for_step_boundary(&mut msgs);

        // UpdatePlanStep result kept verbatim.
        assert_eq!(
            first_tool_result_text(&msgs[2]).unwrap(),
            plan_result_body,
            "plan-tool result must survive compaction",
        );

        // Bash result compacted.
        assert_eq!(
            first_tool_result_text(&msgs[4]).unwrap(),
            "[compacted at step boundary]",
        );
    }

    #[test]
    fn step_boundary_compaction_is_idempotent() {
        let mut msgs = vec![
            text_msg(Role::User, "build"),
            assistant_with_tool_use("u1", "Bash"),
            user_with_tool_result("u1", &"x".repeat(5_000)),
            text_msg(
                Role::User,
                "Continue plan execution. Focus: step 2/3 \"X\".",
            ),
        ];
        let first_pass = compact_for_step_boundary(&mut msgs);
        assert!(first_pass > 0);
        // Run again — nothing more to save (the placeholder is already short).
        let second_pass = compact_for_step_boundary(&mut msgs);
        assert_eq!(second_pass, 0, "second pass must be a no-op");
    }

    // ── M6.4 step-boundary clear tests ─────────────────────────────────

    #[test]
    fn step_boundary_clear_keeps_only_first_user_message() {
        let mut msgs = vec![
            text_msg(Role::User, "build a webapp"), // original ask — keep
            assistant_with_tool_use("u1", "SubmitPlan"),
            user_with_tool_result("u1", "plan submitted"),
            text_msg(Role::User, "Begin executing the plan."),
            assistant_with_tool_use("u2", "Bash"),
            user_with_tool_result("u2", &"x".repeat(5_000)),
            assistant_with_tool_use("u3", "UpdatePlanStep"),
            user_with_tool_result("u3", "step s1 done"),
        ];
        let dropped = clear_for_step_boundary(&mut msgs);
        assert_eq!(dropped, 7, "expected 7 messages dropped, kept 1");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, Role::User);
        // First user message preserved verbatim.
        assert!(matches!(
            &msgs[0].content[0],
            ContentBlock::Text { text } if text == "build a webapp",
        ));
    }

    #[test]
    fn step_boundary_clear_idempotent_after_first_call() {
        let mut msgs = vec![
            text_msg(Role::User, "build"),
            text_msg(Role::Assistant, "ok"),
        ];
        let first = clear_for_step_boundary(&mut msgs);
        assert_eq!(first, 1);
        // Second call: only 1 message, nothing to drop.
        let second = clear_for_step_boundary(&mut msgs);
        assert_eq!(second, 0);
    }

    #[test]
    fn step_boundary_clear_empty_history_is_no_op() {
        let mut msgs: Vec<Message> = vec![];
        let dropped = clear_for_step_boundary(&mut msgs);
        assert_eq!(dropped, 0);
        assert_eq!(msgs.len(), 0);
    }

    #[test]
    fn step_boundary_clear_falls_back_to_first_message_when_no_user_role() {
        // Pathological: no user message at all. Still preserve
        // messages[0] as a fallback rather than wiping everything.
        let mut msgs = vec![
            text_msg(Role::Assistant, "hello"),
            text_msg(Role::Assistant, "world"),
        ];
        let dropped = clear_for_step_boundary(&mut msgs);
        assert_eq!(dropped, 1);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, Role::Assistant);
    }

    #[test]
    fn step_boundary_clear_picks_plan_trigger_when_unrelated_chat_precedes_plan() {
        // M6.13 (F2): if the user chatted with the model BEFORE
        // submitting a plan, the FIRST User message isn't the
        // plan-triggering ask. Walk forward to the SubmitPlan ToolUse
        // and keep the User message immediately preceding it instead.
        // Without this fix, a clear-strategy reset would preserve
        // unrelated chat as the model's "project framing" — exactly
        // the wrong context.
        let mut msgs = vec![
            text_msg(Role::User, "tell me about Rust"), // unrelated pre-plan chat
            text_msg(Role::Assistant, "Rust is a systems language..."),
            text_msg(Role::User, "now plan to build a webapp"), // ← plan trigger
            assistant_with_tool_use("u1", "SubmitPlan"),
            user_with_tool_result("u1", "plan submitted"),
            text_msg(Role::User, "Begin executing the plan."),
            assistant_with_tool_use("u2", "Bash"),
            user_with_tool_result("u2", "(stuff)"),
        ];
        let _dropped = clear_for_step_boundary(&mut msgs);
        assert_eq!(msgs.len(), 1, "clear should keep exactly 1 message");
        // The kept message is the plan-triggering ask, not the
        // unrelated Rust chat at messages[0].
        assert!(
            matches!(
                &msgs[0].content[0],
                ContentBlock::Text { text } if text == "now plan to build a webapp",
            ),
            "expected to keep plan-triggering ask, got: {:?}",
            msgs[0].content,
        );
    }

    #[test]
    fn step_boundary_clear_keeps_first_user_when_plan_starts_at_top_of_session() {
        // The common case: user opens session, immediately says
        // "plan to do X", model SubmitPlans. The User message
        // preceding SubmitPlan IS messages[0]. Behaviour should be
        // identical to the pre-F2 implementation for this path.
        let mut msgs = vec![
            text_msg(Role::User, "plan to build a webapp"), // immediately the trigger
            assistant_with_tool_use("u1", "SubmitPlan"),
            user_with_tool_result("u1", "plan submitted"),
            text_msg(Role::User, "Begin executing the plan."),
            assistant_with_tool_use("u2", "Bash"),
        ];
        clear_for_step_boundary(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(
            &msgs[0].content[0],
            ContentBlock::Text { text } if text == "plan to build a webapp",
        ));
    }

    #[test]
    fn step_boundary_clear_falls_back_when_no_submit_plan_in_history() {
        // Defensive: clear shouldn't normally be called without a
        // SubmitPlan in history (the M6.4 driver only triggers it
        // mid-plan), but if somehow called early, fall back to the
        // first User message.
        let mut msgs = vec![
            text_msg(Role::Assistant, "hello"),
            text_msg(Role::User, "hi there"), // ← first User
            text_msg(Role::Assistant, "what's up"),
        ];
        clear_for_step_boundary(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(
            &msgs[0].content[0],
            ContentBlock::Text { text } if text == "hi there",
        ));
    }

    #[test]
    fn step_boundary_compaction_recognises_skip_user_message_as_boundary() {
        // M6.9 (Bug F1): the sidebar Skip IPC injects a message
        // starting with `Step ("` — the compaction boundary scanner
        // must recognise this as a boundary so subsequent step work
        // gets compacted at the right point.
        let mut msgs = vec![
            text_msg(Role::User, "build webapp"),
            assistant_with_tool_use("u1", "Bash"),
            user_with_tool_result("u1", &"a".repeat(3_000)),
            text_msg(
                Role::User,
                "Step (\"step-1\") was skipped by the user. Continue with the next step.",
            ),
            assistant_with_tool_use("u2", "Edit"),
            user_with_tool_result("u2", &"b".repeat(4_000)),
        ];
        compact_for_step_boundary(&mut msgs);

        // Pre-skip-boundary tool result was compacted.
        assert_eq!(
            first_tool_result_text(&msgs[2]).unwrap(),
            "[compacted at step boundary]",
            "Skip user-message must register as boundary: {msgs:?}",
        );
        // Post-boundary preserved.
        assert_eq!(first_tool_result_text(&msgs[5]).unwrap().len(), 4_000);
    }

    #[test]
    fn step_boundary_compaction_recognises_stalled_continue_as_boundary() {
        // M6.9 (Bug F1): the sidebar stalled-Continue IPC injects a
        // message starting with `Continue with the plan.` — must
        // register as a boundary too.
        let mut msgs = vec![
            text_msg(Role::User, "build webapp"),
            assistant_with_tool_use("u1", "Bash"),
            user_with_tool_result("u1", &"a".repeat(3_000)),
            text_msg(
                Role::User,
                "Continue with the plan. If you're stuck, commit to a UpdatePlanStep transition.",
            ),
            assistant_with_tool_use("u2", "Edit"),
            user_with_tool_result("u2", &"b".repeat(4_000)),
        ];
        compact_for_step_boundary(&mut msgs);

        assert_eq!(
            first_tool_result_text(&msgs[2]).unwrap(),
            "[compacted at step boundary]",
            "stalled-Continue must register as boundary",
        );
        assert_eq!(first_tool_result_text(&msgs[5]).unwrap().len(), 4_000);
    }

    #[test]
    fn step_boundary_compaction_finds_most_recent_boundary() {
        // History contains TWO step-boundary markers (plan ran step 1
        // and step 2). The boundary used must be the LATEST one so
        // step 1's *and* step 2's tool results are both compacted —
        // not just step 1.
        let mut msgs = vec![
            text_msg(Role::User, "build webapp"),
            text_msg(Role::User, "Begin executing the plan."), // boundary 1
            assistant_with_tool_use("step1_bash", "Bash"),
            user_with_tool_result("step1_bash", &"a".repeat(3_000)),
            text_msg(
                Role::User,
                "Continue plan execution. Focus: step 2/3 \"Y\".",
            ), // boundary 2 (latest)
            assistant_with_tool_use("step2_edit", "Edit"),
            user_with_tool_result("step2_edit", &"b".repeat(4_000)),
            // Pretend step 2 is still in flight; no boundary 3 yet.
        ];
        compact_for_step_boundary(&mut msgs);

        // Step 1's bash result was older than boundary 2 → compacted.
        assert_eq!(
            first_tool_result_text(&msgs[3]).unwrap(),
            "[compacted at step boundary]",
        );
        // Step 2's edit result is post-latest-boundary → preserved.
        assert_eq!(first_tool_result_text(&msgs[6]).unwrap().len(), 4_000,);
    }
}
