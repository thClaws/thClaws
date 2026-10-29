use crate::providers::{assemble, collect_turn, Provider, StreamRequest};
use crate::types::Message;

/// Ask the active provider to author a JavaScript workflow script for
/// `user_prompt`. On re-author, `revision_note` carries the user's edit
/// request from the review panel. The model sees the API spec
/// ([`crate::prompts::defaults::WORKFLOW_AUTHOR`]) as the system prompt
/// and the goal as the only user message — no conversation history
/// from the calling session leaks in.
pub(crate) async fn author(
    provider: &dyn Provider,
    model: &str,
    user_prompt: &str,
    revision_note: Option<&str>,
) -> Result<String, String> {
    let system = crate::prompts::load("workflow_author", crate::prompts::defaults::WORKFLOW_AUTHOR);

    let user_msg = match revision_note {
        Some(note) if !note.trim().is_empty() => format!(
            "Goal:\n{user_prompt}\n\nThe previous script was rejected. Reviewer note:\n{note}"
        ),
        _ => format!("Goal:\n{user_prompt}"),
    };

    // The author call is a single streaming turn with no retry of its
    // own — a transient provider error (e.g. a dropped SSE stream
    // during a parallel WorkflowRun burst) used to fail the whole run
    // before any stage executed. Retry transient failures a few times
    // with exponential backoff; deterministic ones (auth, empty script)
    // still fail fast.
    const MAX_ATTEMPTS: u32 = 3;
    let mut attempt = 0u32;
    let turn = loop {
        attempt += 1;
        let req = StreamRequest {
            model: model.to_string(),
            system: Some(system.clone()),
            messages: vec![Message::user(user_msg.clone())],
            tools: vec![],
            max_tokens: 4096,
            thinking_budget: None,
            stream_chunk_timeout_override: None,
        };
        let attempt_res: Result<_, String> = async {
            let stream = provider.stream(req).await.map_err(|e| e.to_string())?;
            collect_turn(assemble(stream))
                .await
                .map_err(|e| e.to_string())
        }
        .await;
        match attempt_res {
            Ok(turn) => {
                if let Some(u) = &turn.usage {
                    crate::usage::note_side_usage(model, u);
                }
                break turn;
            }
            Err(e) => {
                if attempt < MAX_ATTEMPTS && super::is_transient_provider_error(&e) {
                    let delay = std::time::Duration::from_secs(1u64 << (attempt - 1));
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(e);
            }
        }
    };

    let script = strip_markdown_fence(&turn.text);
    if script.trim().is_empty() {
        return Err("model returned empty script".to_string());
    }
    Ok(script)
}

/// Strip a single leading ```js (or ```javascript, or bare ```) fence
/// and its matching trailing ```. Models occasionally wrap output in
/// markdown despite the system prompt telling them not to; better to
/// quietly unwrap than to fail.
fn strip_markdown_fence(text: &str) -> String {
    let trimmed = text.trim();
    for prefix in ["```javascript", "```js", "```"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let inner = rest.trim_start_matches('\n');
            if let Some(body) = inner.strip_suffix("```") {
                return body.trim_end().to_string();
            }
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_with_js_lang_tag() {
        let input = "```js\nlet x = 1;\nx\n```";
        assert_eq!(strip_markdown_fence(input), "let x = 1;\nx");
    }

    #[test]
    fn fence_with_javascript_lang_tag() {
        let input = "```javascript\nlet x = 1;\n```";
        assert_eq!(strip_markdown_fence(input), "let x = 1;");
    }

    #[test]
    fn bare_fence() {
        let input = "```\nlet x = 1;\n```";
        assert_eq!(strip_markdown_fence(input), "let x = 1;");
    }

    #[test]
    fn no_fence_passes_through() {
        let input = "// Workflow: hi\nlet x = 1;\nx";
        assert_eq!(strip_markdown_fence(input), input);
    }

    #[test]
    fn trims_outer_whitespace() {
        let input = "\n\n```js\nlet x = 1;\n```\n\n";
        assert_eq!(strip_markdown_fence(input), "let x = 1;");
    }

    #[tokio::test]
    async fn author_retries_transient_stream_error() {
        use crate::providers::{EventStream, ProviderEvent, Usage};
        use futures::stream;
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        // Fails the first stream() with the exact transient error that
        // killed a real WorkflowRun, then succeeds — author() must retry
        // and return the script rather than propagating the failure.
        struct FlakyAuthor {
            calls: Arc<AtomicU32>,
        }
        #[async_trait::async_trait]
        impl Provider for FlakyAuthor {
            async fn stream(
                &self,
                _req: StreamRequest,
            ) -> std::result::Result<EventStream, crate::error::Error> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 2 {
                    return Err(crate::error::Error::Provider(
                        "stream: error decoding response body".into(),
                    ));
                }
                let events = vec![
                    Ok(ProviderEvent::TextDelta("'hi'".into())),
                    Ok(ProviderEvent::MessageStop {
                        stop_reason: None,
                        usage: Some(Usage {
                            input_tokens: 1,
                            output_tokens: 1,
                            ..Default::default()
                        }),
                    }),
                ];
                Ok(Box::pin(stream::iter(events)))
            }
        }

        let calls = Arc::new(AtomicU32::new(0));
        let provider = FlakyAuthor {
            calls: calls.clone(),
        };
        let script = author(&provider, "test-model", "say hi", None)
            .await
            .expect("author should retry the transient error and succeed");
        assert_eq!(script, "'hi'");
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "author should have retried the transient stream error"
        );
    }
}
