//! Stream adapter: fold raw [`ProviderEvent`]s into semantic [`AssembledEvent`]s.
//!
//! - `TextDelta` passes through as `AssembledEvent::Text` (for streaming UI).
//! - `ToolUseStart` + N× `ToolUseDelta` + `ContentBlockStop` collapses into a
//!   single `AssembledEvent::ToolUse` with a fully-parsed JSON input.
//! - `MessageStop` becomes `AssembledEvent::Done`.
//!
//! The agent loop typically drains this via [`collect_turn`] to get a complete
//! turn result, or consumes it live when the UI wants streaming text.

use crate::error::{Error, Result};
use crate::providers::{ProviderEvent, Usage};
use crate::types::ContentBlock;
use async_stream::try_stream;
use futures::{Stream, StreamExt};

#[derive(Debug, Clone, PartialEq)]
pub enum AssembledEvent {
    Text(String),
    /// Reasoning delta from a thinking model. Streamed live so the GUI can
    /// surface it (collapsed by default), and folded into the persisted
    /// assistant message so the next turn can echo it back to providers
    /// that require it (DeepSeek v4-*, OpenAI o-series).
    Thinking(String),
    /// Always `ContentBlock::ToolUse { id, name, input }`.
    ToolUse(ContentBlock),
    Done {
        stop_reason: Option<String>,
        usage: Option<Usage>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct TurnResult {
    pub text: String,
    /// Concatenated reasoning_content from the turn (empty for non-thinking
    /// models). Persisted as a `ContentBlock::Thinking` on the assistant
    /// message so subsequent requests carry it back to the provider.
    pub thinking: String,
    /// Each entry is a `ContentBlock::ToolUse`.
    pub tool_uses: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
}

enum BlockState {
    None,
    Text,
    ToolUse {
        id: String,
        name: String,
        buf: String,
    },
}

/// Returns true for model families known to emit *implicit* reasoning —
/// they start streaming chain-of-thought text with no opening `<think>`,
/// then close it with `</think>` before the actual answer. We use this to
/// gate the lookahead-buffer hack in `assemble`. Be conservative: only the
/// specific families that need it. Other families (qwen2.5-coder, qwen-vl,
/// generic *r1 finetunes) must NOT match or every turn pays a 1KB delay.
fn is_implicit_thinking_model(model: &str) -> bool {
    let m = model.to_lowercase();
    // Qwen3 thinking variants. Plain "qwen" / "qwen2" must not match.
    if m.contains("qwen3") || m.contains("qwq") {
        return true;
    }
    // DeepSeek R1 family (and OpenRouter prefix form).
    if m.contains("deepseek-r1") || m.contains("deepseek/deepseek-r1") {
        return true;
    }
    false
}

/// Streaming state for `split_think_text`. Persists across SSE chunks so
/// tags split on chunk boundaries — and the blank lines Qwen3-style models
/// emit between `</think>` and the answer — are handled correctly.
#[derive(Default)]
struct ThinkState {
    in_block: bool,
    tag_buf: String,
    /// Set right after a `</think>` is consumed. Strips leading newlines
    /// from the next text the model emits, in case `</think>\n\n` straddles
    /// two SSE chunks.
    trim_leading_newlines: bool,
}

/// Split a text chunk that may contain `<think>…</think>` blocks into the
/// appropriate `AssembledEvent`s. Any text inside the tags becomes
/// `Thinking`; text outside becomes `Text`.
fn split_think_text(chunk: &str, state: &mut ThinkState) -> Vec<AssembledEvent> {
    let mut out = Vec::new();
    let mut combined = if state.tag_buf.is_empty() {
        chunk.to_string()
    } else {
        format!("{}{}", std::mem::take(&mut state.tag_buf), chunk)
    };
    if state.trim_leading_newlines && !state.in_block {
        let trimmed = combined.trim_start_matches('\n');
        if trimmed.len() < combined.len() {
            combined = trimmed.to_string();
        }
        if !combined.is_empty() {
            state.trim_leading_newlines = false;
        }
    }
    let mut s = combined.as_str();
    loop {
        if s.is_empty() {
            break;
        }
        if state.in_block {
            const CLOSE: &str = "</think>";
            if let Some(pos) = s.find(CLOSE) {
                if pos > 0 {
                    out.push(AssembledEvent::Thinking(s[..pos].to_string()));
                }
                state.in_block = false;
                let after = &s[pos + CLOSE.len()..];
                let trimmed = after.trim_start_matches('\n');
                // If nothing remains after stripping newlines in this chunk,
                // remember to strip leading newlines from the next chunk too.
                state.trim_leading_newlines = trimmed.is_empty();
                s = trimmed;
            } else {
                let keep = longest_tag_prefix(s, CLOSE);
                if s.len() > keep {
                    out.push(AssembledEvent::Thinking(s[..s.len() - keep].to_string()));
                }
                if keep > 0 {
                    state.tag_buf.push_str(&s[s.len() - keep..]);
                }
                break;
            }
        } else {
            const OPEN: &str = "<think>";
            const CLOSE: &str = "</think>";
            if let Some(pos) = s.find(OPEN) {
                if pos > 0 {
                    out.push(AssembledEvent::Text(s[..pos].to_string()));
                }
                state.in_block = true;
                s = s[pos + OPEN.len()..].trim_start_matches('\n');
            } else if let Some(pos) = s.find(CLOSE) {
                // Found </think> but we weren't in a think block. This happens
                // with models (like Qwen3) where the opening <think> is pre-filled
                // in the prompt. Treat preceding text in this chunk as thinking.
                if pos > 0 {
                    out.push(AssembledEvent::Thinking(s[..pos].to_string()));
                }
                let after = &s[pos + CLOSE.len()..];
                let trimmed = after.trim_start_matches('\n');
                state.trim_leading_newlines = trimmed.is_empty();
                s = trimmed;
            } else {
                let keep = longest_tag_prefix(s, OPEN);
                if s.len() > keep {
                    out.push(AssembledEvent::Text(s[..s.len() - keep].to_string()));
                }
                if keep > 0 {
                    state.tag_buf.push_str(&s[s.len() - keep..]);
                }
                break;
            }
        }
    }
    out
}

fn longest_tag_prefix(haystack: &str, needle: &str) -> usize {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    let max = h.len().min(n.len() - 1);
    for len in (1..=max).rev() {
        if h[h.len() - len..] == n[..len] {
            return len;
        }
    }
    0
}

pub fn assemble<S>(inner: S) -> impl Stream<Item = Result<AssembledEvent>> + Send + 'static
where
    S: Stream<Item = Result<ProviderEvent>> + Send + 'static,
{
    try_stream! {
        let mut state = BlockState::None;
        // For implicit-thinking models (Qwen3, DeepSeek-R1) the stream begins
        // with reasoning that has no opening `<think>` tag — only a closing
        // `</think>` before the answer. Pre-seed `in_block = true` so
        // `split_think_text` emits the prefix as Thinking until it finds
        // `</think>`. If the closing tag never arrives, the whole stream
        // stays in Thinking and no chain-of-thought leaks as Text.
        let mut think = ThinkState::default();

        let mut inner = Box::pin(inner);
        while let Some(ev) = inner.next().await {
            let ev = ev?;
            match ev {
                ProviderEvent::MessageStart { model } => {
                    if is_implicit_thinking_model(&model) {
                        think.in_block = true;
                    }
                }
                ProviderEvent::TextDelta(s) => {
                    state = BlockState::Text;
                    for ev in split_think_text(&s, &mut think) {
                        yield ev;
                    }
                }
                ProviderEvent::ThinkingDelta(s) => {
                    // A structured ThinkingDelta means the provider
                    // already separates reasoning (DashScope/OpenRouter
                    // `reasoning_content`, Ollama `message.thinking`,
                    // OpenAI o-series). The implicit-thinking
                    // pre-seed was guarding against models that stream
                    // raw chain-of-thought as text up to a closing
                    // `</think>` tag — that mode is mutually exclusive
                    // with structured reasoning, so flip the buffer
                    // off the first time we see one. Without this,
                    // qwen3.6 on DashScope would never emit any text
                    // (the `</think>` close never arrives in the
                    // content stream because reasoning lives elsewhere).
                    think.in_block = false;
                    yield AssembledEvent::Thinking(s);
                }
                ProviderEvent::ToolUseStart { id, name } => {
                    state = BlockState::ToolUse {
                        id,
                        name,
                        buf: String::new(),
                    };
                }
                ProviderEvent::ToolUseDelta { partial_json } => {
                    if let BlockState::ToolUse { buf, .. } = &mut state {
                        buf.push_str(&partial_json);
                    }
                }
                ProviderEvent::ContentBlockStop => {
                    let prev = std::mem::replace(&mut state, BlockState::None);
                    if let BlockState::ToolUse { id, name, buf } = prev {
                        let input: serde_json::Value = if buf.trim().is_empty() {
                            serde_json::json!({})
                        } else {
                            serde_json::from_str(&buf).map_err(|e| {
                                Error::Provider(format!(
                                    "tool_use json parse: {e} (buf={buf})"
                                ))
                            })?
                        };
                        yield AssembledEvent::ToolUse(ContentBlock::ToolUse { id, name, input });
                    }
                }
                ProviderEvent::MessageStop { stop_reason, usage } => {
                    yield AssembledEvent::Done { stop_reason, usage };
                }
            }
        }
    }
}

pub async fn collect_turn<S>(stream: S) -> Result<TurnResult>
where
    S: Stream<Item = Result<AssembledEvent>> + Send,
{
    let mut out = TurnResult::default();
    let mut stream = Box::pin(stream);
    while let Some(ev) = stream.next().await {
        match ev? {
            AssembledEvent::Text(s) => out.text.push_str(&s),
            AssembledEvent::Thinking(s) => out.thinking.push_str(&s),
            AssembledEvent::ToolUse(block) => out.tool_uses.push(block),
            AssembledEvent::Done { stop_reason, usage } => {
                out.stop_reason = stop_reason;
                out.usage = usage;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    fn src(events: Vec<ProviderEvent>) -> impl Stream<Item = Result<ProviderEvent>> + Send {
        stream::iter(events.into_iter().map(Ok))
    }

    async fn collected(events: Vec<ProviderEvent>) -> TurnResult {
        collect_turn(assemble(src(events))).await.unwrap()
    }

    #[tokio::test]
    async fn text_only_turn() {
        let r = collected(vec![
            ProviderEvent::MessageStart { model: "m".into() },
            ProviderEvent::TextDelta("Hello".into()),
            ProviderEvent::TextDelta(", world".into()),
            ProviderEvent::ContentBlockStop,
            ProviderEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: Some(Usage {
                    input_tokens: 3,
                    output_tokens: 2,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                }),
            },
        ])
        .await;

        assert_eq!(r.text, "Hello, world");
        assert_eq!(r.tool_uses.len(), 0);
        assert_eq!(r.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(r.usage.unwrap().output_tokens, 2);
    }

    #[tokio::test]
    async fn structured_thinking_disables_implicit_thinking_buffer() {
        // qwen3.6-flash on DashScope: the model name contains "qwen3" so
        // is_implicit_thinking_model returns true and the assembler used
        // to pre-seed in_block=true, waiting for a `</think>` tag in the
        // text stream that never arrives because reasoning lives in a
        // separate `reasoning_content` field. Result: every TextDelta
        // got swallowed as Thinking and the user saw an empty bubble.
        // Once a structured ThinkingDelta lands we know reasoning is
        // out-of-band, so subsequent TextDeltas should render as the
        // answer.
        let r = collected(vec![
            ProviderEvent::MessageStart {
                model: "qwen3.6-flash".into(),
            },
            ProviderEvent::ThinkingDelta("Let me consider…".into()),
            ProviderEvent::TextDelta("Hello".into()),
            ProviderEvent::TextDelta("!".into()),
            ProviderEvent::ContentBlockStop,
            ProviderEvent::MessageStop {
                stop_reason: Some("stop".into()),
                usage: None,
            },
        ])
        .await;

        assert_eq!(r.text, "Hello!");
        assert!(
            r.thinking.contains("consider"),
            "reasoning should still land in a Thinking block, got: {:?}",
            r.thinking
        );
    }

    #[tokio::test]
    async fn tool_use_accumulates_partial_json() {
        let r = collected(vec![
            ProviderEvent::ToolUseStart {
                id: "toolu_1".into(),
                name: "read_file".into(),
            },
            ProviderEvent::ToolUseDelta {
                partial_json: "{\"pa".into(),
            },
            ProviderEvent::ToolUseDelta {
                partial_json: "th\":\"".into(),
            },
            ProviderEvent::ToolUseDelta {
                partial_json: "/tmp/x\"}".into(),
            },
            ProviderEvent::ContentBlockStop,
            ProviderEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ])
        .await;

        assert_eq!(r.text, "");
        assert_eq!(r.tool_uses.len(), 1);
        match &r.tool_uses[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "read_file");
                assert_eq!(input, &serde_json::json!({"path": "/tmp/x"}));
            }
            _ => panic!("expected ToolUse"),
        }
        assert_eq!(r.stop_reason.as_deref(), Some("tool_use"));
    }

    #[tokio::test]
    async fn text_then_tool_use_in_one_turn() {
        let r = collected(vec![
            ProviderEvent::TextDelta("Let me check ".into()),
            ProviderEvent::TextDelta("that file.".into()),
            ProviderEvent::ContentBlockStop,
            ProviderEvent::ToolUseStart {
                id: "toolu_2".into(),
                name: "glob".into(),
            },
            ProviderEvent::ToolUseDelta {
                partial_json: "{\"pattern\":\"*.rs\"}".into(),
            },
            ProviderEvent::ContentBlockStop,
            ProviderEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ])
        .await;

        assert_eq!(r.text, "Let me check that file.");
        assert_eq!(r.tool_uses.len(), 1);
        if let ContentBlock::ToolUse { name, input, .. } = &r.tool_uses[0] {
            assert_eq!(name, "glob");
            assert_eq!(input["pattern"], "*.rs");
        } else {
            panic!("expected ToolUse");
        }
    }

    #[tokio::test]
    async fn two_tool_uses_in_one_turn() {
        let r = collected(vec![
            ProviderEvent::ToolUseStart {
                id: "a".into(),
                name: "read".into(),
            },
            ProviderEvent::ToolUseDelta {
                partial_json: "{\"p\":1}".into(),
            },
            ProviderEvent::ContentBlockStop,
            ProviderEvent::ToolUseStart {
                id: "b".into(),
                name: "write".into(),
            },
            ProviderEvent::ToolUseDelta {
                partial_json: "{\"p\":2}".into(),
            },
            ProviderEvent::ContentBlockStop,
            ProviderEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ])
        .await;

        assert_eq!(r.tool_uses.len(), 2);
        let ids: Vec<_> = r
            .tool_uses
            .iter()
            .map(|b| match b {
                ContentBlock::ToolUse { id, .. } => id.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn empty_tool_input_becomes_empty_object() {
        let r = collected(vec![
            ProviderEvent::ToolUseStart {
                id: "x".into(),
                name: "list_projects".into(),
            },
            ProviderEvent::ContentBlockStop,
            ProviderEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ])
        .await;
        assert_eq!(r.tool_uses.len(), 1);
        if let ContentBlock::ToolUse { input, .. } = &r.tool_uses[0] {
            assert_eq!(input, &serde_json::json!({}));
        } else {
            panic!("expected ToolUse");
        }
    }

    #[test]
    fn implicit_thinking_detection_is_specific() {
        // Should match: qwen3 family, qwq, deepseek-r1 family.
        assert!(is_implicit_thinking_model("qwen3-30b-a3b-thinking"));
        assert!(is_implicit_thinking_model("qwen/qwen3-235b"));
        assert!(is_implicit_thinking_model("qwq-32b-preview"));
        assert!(is_implicit_thinking_model("deepseek-r1"));
        assert!(is_implicit_thinking_model("deepseek/deepseek-r1-distill"));

        // Must NOT match: non-thinking qwen variants and unrelated -r1 ids.
        assert!(!is_implicit_thinking_model("qwen2.5-coder-32b"));
        assert!(!is_implicit_thinking_model("qwen-vl-plus"));
        assert!(!is_implicit_thinking_model("qwen-turbo"));
        assert!(!is_implicit_thinking_model("gpt-4o"));
        assert!(!is_implicit_thinking_model("anthropic/claude-sonnet-4-6"));
    }

    #[test]
    fn split_think_text_handles_lone_closing_tag() {
        let mut state = ThinkState::default();

        // Case: starts with reasoning (no <think>) and ends with </think>
        let ev = split_think_text("reasoning here </think>actual answer", &mut state);
        assert!(!state.in_block);
        let thinking: Vec<_> = ev
            .iter()
            .filter_map(|e| {
                if let AssembledEvent::Thinking(s) = e {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .collect();
        let text: Vec<_> = ev
            .iter()
            .filter_map(|e| {
                if let AssembledEvent::Text(s) = e {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(thinking.join(""), "reasoning here ");
        assert_eq!(text.join(""), "actual answer");
    }

    #[test]
    fn split_think_text_strips_newlines_across_chunk_boundary() {
        // Real Qwen3 / vLLM behavior: `</think>` ends one SSE chunk and the
        // next chunk starts with `\n\n`. The blank lines must not leak into
        // the rendered Text.
        let mut state = ThinkState {
            in_block: true,
            ..Default::default()
        };
        let ev1 = split_think_text("</think>", &mut state);
        let ev2 = split_think_text("\n\nHi.", &mut state);

        let text: String = ev1
            .iter()
            .chain(ev2.iter())
            .filter_map(|e| {
                if let AssembledEvent::Text(s) = e {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(text, "Hi.");
    }

    #[tokio::test]
    async fn implicit_thinking_routes_prefix_to_thinking() {
        // Qwen3 streams reasoning then `</think>` then answer, with no
        // opening tag. Reasoning must land in `thinking`, not `text`.
        let r = collected(vec![
            ProviderEvent::MessageStart {
                model: "qwen3.6-35b-a3b-fp8".into(),
            },
            ProviderEvent::TextDelta("let me think about this".into()),
            ProviderEvent::TextDelta(" carefully</think>".into()),
            ProviderEvent::TextDelta("Final answer.".into()),
            ProviderEvent::ContentBlockStop,
            ProviderEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ])
        .await;
        assert_eq!(r.thinking, "let me think about this carefully");
        assert_eq!(r.text, "Final answer.");
    }

    #[tokio::test]
    async fn implicit_thinking_without_close_stays_in_thinking() {
        // Degenerate case: stream ends mid-reasoning. The chain-of-thought
        // must NOT leak as `text` — keep it in `thinking`.
        let r = collected(vec![
            ProviderEvent::MessageStart {
                model: "qwen3-30b".into(),
            },
            ProviderEvent::TextDelta("step 1, step 2, step 3".into()),
            ProviderEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ])
        .await;
        assert_eq!(r.text, "");
        assert_eq!(r.thinking, "step 1, step 2, step 3");
    }

    #[tokio::test]
    async fn non_thinking_model_passes_text_through() {
        // qwen2.5-coder must not be detected as implicit-thinking — its
        // output is plain text and should not be re-routed.
        let r = collected(vec![
            ProviderEvent::MessageStart {
                model: "qwen2.5-coder-32b".into(),
            },
            ProviderEvent::TextDelta("hello world".into()),
            ProviderEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ])
        .await;
        assert_eq!(r.text, "hello world");
        assert_eq!(r.thinking, "");
    }

    #[tokio::test]
    async fn malformed_tool_json_yields_error() {
        let result = collect_turn(assemble(src(vec![
            ProviderEvent::ToolUseStart {
                id: "x".into(),
                name: "t".into(),
            },
            ProviderEvent::ToolUseDelta {
                partial_json: "{not-json".into(),
            },
            ProviderEvent::ContentBlockStop,
        ])))
        .await;
        assert!(result.is_err(), "expected parse error");
        assert!(format!("{:?}", result.unwrap_err()).contains("tool_use json parse"));
    }
}
