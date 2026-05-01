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

/// Max tool result size kept in context. Excess is saved to disk with a preview.
pub const TOOL_RESULT_CONTEXT_LIMIT: usize = 50_000;

/// Default output token cap (keeps normal responses lean).
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
/// Escalated cap when the model hits the output limit.
pub const ESCALATED_MAX_TOKENS: u32 = 64000;

pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
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
        }
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
        let system = self.system.clone();
        let budget_tokens = self.budget_tokens;
        let base_max_tokens = self.max_tokens;
        let max_iterations = self.max_iterations;
        let max_retries = self.max_retries;
        let thinking_budget = self.thinking_budget;
        let permission_mode = self.permission_mode;
        let approver = self.approver.clone();
        let history = self.history.clone();

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
                    compact(&h, budget_tokens)
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
                let raw = {
                    let mut last_err = None;
                    let mut stream_result = None;
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
                                    tokio::time::sleep(delay).await;
                                }
                                last_err = Some(e);
                                if is_config { break; }
                            }
                        }
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
                        AssembledEvent::ToolUse(block) => {
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
                    let ContentBlock::ToolUse { id, name, input } = tu else { continue };

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

                    yield AgentEvent::ToolCallStart {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    };

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
    let tmp_dir = std::env::temp_dir().join("thclaws-tool-output");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let filename = format!("tool-{}.txt", std::process::id());
    let path = tmp_dir.join(&filename);
    let _ = std::fs::write(&path, content);

    // Return a preview + pointer to the full file.
    let preview_end = content
        .char_indices()
        .nth(2000)
        .map(|(i, _)| i)
        .unwrap_or(content.len().min(2000));
    format!(
        "{}\n\n... [truncated: {} total bytes — full output saved to {}]",
        &content[..preview_end],
        content.len(),
        path.display()
    )
}

/// Drain an agent stream into a blocking result. Useful for tests and for
/// non-interactive consumers.
pub async fn collect_agent_turn<S>(stream: S) -> Result<AgentTurnOutcome>
where
    S: Stream<Item = Result<AgentEvent>> + Send,
{
    let mut out = AgentTurnOutcome::default();
    let mut stream = Box::pin(stream);
    while let Some(ev) = stream.next().await {
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
        assert!(outcome.tool_calls.is_empty());

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
}
