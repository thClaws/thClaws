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

    let req = StreamRequest {
        model: model.to_string(),
        system: Some(system),
        messages: vec![Message::user(user_msg)],
        tools: vec![],
        max_tokens: 4096,
        thinking_budget: None,
        stream_chunk_timeout_override: None,
    };

    let stream = provider.stream(req).await.map_err(|e| e.to_string())?;
    let turn = collect_turn(assemble(stream))
        .await
        .map_err(|e| e.to_string())?;

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
}
