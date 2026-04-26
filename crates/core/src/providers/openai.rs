//! OpenAI chat/completions streaming provider.
//!
//! Wire format differs meaningfully from Anthropic:
//! - SSE is `data: {chunk_json}\n\n`; no `event:` lines. Terminator is `data: [DONE]`.
//! - Tool calls stream via `choices[0].delta.tool_calls[i].function.arguments`;
//!   a new tool call is marked by a new `index` value (and the first chunk for
//!   that index includes `id` + `function.name`).
//! - `finish_reason` appears on the last content chunk, not as a separate event.
//!
//! Adaptation to the common [`ProviderEvent`] stream uses a small stateful
//! parser ([`ParseState`]) that:
//! - emits a single `MessageStart` on the first parsed chunk,
//! - emits synthetic `ContentBlockStop` events when the tool-call index switches
//!   or when `finish_reason` arrives,
//! - emits `MessageStop` with the OpenAI stop reason and (for now) `None` usage.
//!
//! Downstream [`crate::providers::assemble`] folds this identically to Anthropic.

use super::{EventStream, ModelInfo, Provider, ProviderEvent, StreamRequest, Usage};
use crate::error::{Error, Result};
use crate::types::{ContentBlock, ImageSource, Role, ToolResultBlock, ToolResultContent};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};

pub const DEFAULT_API_URL: &str = "https://api.openai.com/v1/chat/completions";

pub struct OpenAIProvider {
    client: Client,
    api_key: String,
    base_url: String,
    /// Optional prefix stripped from `req.model` before sending to the
    /// remote. Used by aggregator-style providers (e.g. `ap/gemma4-12b` →
    /// `gemma4-12b`) where the prefix exists only to route `detect()` on
    /// our side.
    strip_model_prefix: Option<String>,
    /// Override the auth header name. `None` → `Authorization: Bearer {key}`.
    /// Azure AI Foundry uses `api-key: {key}` instead.
    api_key_header: Option<String>,
    /// Explicit URL for GET /models. When `None` the URL is derived from
    /// `base_url` by replacing `/chat/completions` with `/models`.
    /// Azure's models path differs from the completions path, so it needs
    /// an explicit override.
    list_models_url: Option<String>,
}

impl OpenAIProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            base_url: DEFAULT_API_URL.to_string(),
            strip_model_prefix: None,
            api_key_header: None,
            list_models_url: None,
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_strip_model_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.strip_model_prefix = Some(prefix.into());
        self
    }

    pub fn with_api_key_header(mut self, name: impl Into<String>) -> Self {
        self.api_key_header = Some(name.into());
        self
    }

    pub fn with_list_models_url(mut self, url: impl Into<String>) -> Self {
        self.list_models_url = Some(url.into());
        self
    }

    fn auth_header_name(&self) -> &str {
        self.api_key_header.as_deref().unwrap_or("authorization")
    }

    fn auth_header_value(&self) -> String {
        match &self.api_key_header {
            Some(_) => self.api_key.clone(),
            None => format!("Bearer {}", self.api_key),
        }
    }

    /// Convert canonical `Message`s → OpenAI chat/completions messages array.
    /// Splits ToolResult blocks out as separate `role: "tool"` messages.
    /// When a ToolResult carries inline images (Read on a PNG, etc.), an
    /// extra `role: "user"` message with image_url blocks is appended
    /// after the tool message — OpenAI's tool-role messages are
    /// text-only, so this is the documented pattern for getting
    /// tool-returned imagery in front of a vision-capable model.
    fn messages_to_openai(req: &StreamRequest) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        let echo_reasoning = model_uses_reasoning_content(&req.model);

        if let Some(sys) = &req.system {
            if !sys.is_empty() {
                out.push(json!({"role": "system", "content": sys}));
            }
        }

        for m in &req.messages {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
            };

            let mut text_parts: Vec<String> = Vec::new();
            let mut thinking_parts: Vec<String> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            // (tool_call_id, text_content, images-from-this-result).
            // Each image is (media_type, base64_data) and gets emitted
            // as a follow-up synthetic user message with image_url
            // blocks — OpenAI's tool-role messages are text-only, so a
            // separate user message is the documented pattern for
            // getting tool-returned imagery in front of a vision model.
            let mut trailing_tool_results: Vec<(String, String, Vec<(String, String)>)> =
                Vec::new();
            // Inline images attached directly to a user message
            // (Phase 4 paste/drag-drop). Held separately so the
            // emit-step below can switch to OpenAI's array-form
            // content shape only when there's actually an image.
            let mut inline_user_images: Vec<(String, String)> = Vec::new();

            for block in &m.content {
                match block {
                    ContentBlock::Text { text } => text_parts.push(text.clone()),
                    ContentBlock::Thinking { content, .. } => {
                        // Only carry reasoning_content into the wire body
                        // for models that explicitly require it. For all
                        // other OpenAI-compat targets (gpt-4o, deepseek-v3,
                        // qwen non-thinking, etc.), drop the block — saves
                        // tokens and avoids surprising the server.
                        if echo_reasoning {
                            thinking_parts.push(content.clone());
                        }
                    }
                    ContentBlock::Image {
                        source: ImageSource::Base64 { media_type, data },
                    } => {
                        inline_user_images.push((media_type.clone(), data.clone()));
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        let args = serde_json::to_string(input).unwrap_or_else(|_| "{}".into());
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": args },
                        }));
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        // Tool message itself is text-only — extract the
                        // text portions via to_text(). Any images get
                        // queued for the synthetic user message that
                        // follows the tool message (see the emission
                        // loop below).
                        let text = content.to_text();
                        let images = extract_images(content);
                        trailing_tool_results.push((tool_use_id.clone(), text, images));
                    }
                }
            }

            let content_text = text_parts.join("");
            let reasoning_text = thinking_parts.join("");
            let has_text = !content_text.is_empty();
            let has_reasoning = !reasoning_text.is_empty();
            let has_tools = !tool_calls.is_empty();
            let has_inline_images = !inline_user_images.is_empty();

            if has_text || has_tools || has_reasoning || has_inline_images {
                let mut msg = json!({"role": role});
                if has_inline_images {
                    // Mixed text + image_url content array. OpenAI
                    // requires this shape any time an image_url
                    // block appears, even if a string would otherwise
                    // suffice for the same role + text.
                    let mut content_arr: Vec<Value> = Vec::new();
                    if has_text {
                        content_arr.push(json!({"type": "text", "text": content_text}));
                    }
                    for (media_type, data) in &inline_user_images {
                        content_arr.push(json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{media_type};base64,{data}")
                            }
                        }));
                    }
                    msg["content"] = json!(content_arr);
                } else if has_text {
                    msg["content"] = json!(content_text);
                } else if has_tools {
                    msg["content"] = Value::Null;
                }
                if has_tools {
                    msg["tool_calls"] = json!(tool_calls);
                }
                if has_reasoning {
                    msg["reasoning_content"] = json!(reasoning_text);
                }
                out.push(msg);
            }

            // Emit ALL tool messages back-to-back first. OpenAI's
            // contract: an assistant message with `tool_calls` must
            // be followed by tool-role messages responding to every
            // tool_call_id, with no other roles interleaved. An
            // earlier (broken) version of this code emitted a
            // synthetic user message after each individual tool
            // message — fine for one tool call but a 400 from the
            // server when the model batched N parallel calls
            // ("tool_call_ids did not have response messages").
            for (tool_call_id, content, _images) in &trailing_tool_results {
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content,
                }));
            }
            // Then ONE combined synthetic user message carrying every
            // image returned by any of those tool calls — text labels
            // tag each image_url with its originating tool_call_id so
            // the model can correlate. This is the documented OpenAI
            // pattern for getting tool-returned imagery in front of a
            // vision-capable model. The user must select a vision-
            // capable model (gpt-4o, gpt-4o-mini, …); non-vision
            // models will 400 with a clear server error.
            let total_images: usize = trailing_tool_results.iter().map(|(_, _, i)| i.len()).sum();
            if total_images > 0 {
                let mut user_content: Vec<Value> = Vec::with_capacity(total_images * 2 + 1);
                let call_ids: Vec<&str> = trailing_tool_results
                    .iter()
                    .filter(|(_, _, i)| !i.is_empty())
                    .map(|(id, _, _)| id.as_str())
                    .collect();
                user_content.push(json!({
                    "type": "text",
                    "text": format!(
                        "(image{} attached from preceding tool_result{}: {})",
                        if total_images == 1 { "" } else { "s" },
                        if call_ids.len() == 1 { "" } else { "s" },
                        call_ids.join(", ")
                    ),
                }));
                for (tool_call_id, _content, images) in &trailing_tool_results {
                    for (media_type, data) in images {
                        user_content.push(json!({
                            "type": "text",
                            "text": format!("from {tool_call_id}:"),
                        }));
                        user_content.push(json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{media_type};base64,{data}")
                            }
                        }));
                    }
                }
                out.push(json!({
                    "role": "user",
                    "content": user_content,
                }));
            }
        }

        out
    }

    fn build_body(req: &StreamRequest) -> Value {
        let messages = Self::messages_to_openai(req);
        let mut body = json!({
            "model": req.model,
            "max_completion_tokens": req.max_tokens,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }
        body
    }
}

/// Extract `(media_type, base64_data)` pairs from a ToolResultContent.
/// Returns empty for the Text variant or for Blocks containing no
/// images. Used by `messages_to_openai` to decide whether to emit a
/// follow-up synthetic user message carrying image_url blocks.
fn extract_images(content: &ToolResultContent) -> Vec<(String, String)> {
    match content {
        ToolResultContent::Text(_) => Vec::new(),
        ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ToolResultBlock::Image {
                    source: ImageSource::Base64 { media_type, data },
                } => Some((media_type.clone(), data.clone())),
                ToolResultBlock::Text { .. } => None,
            })
            .collect(),
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let models_url = self.list_models_url.clone().unwrap_or_else(|| {
            // Derive from base_url: /v1/chat/completions → /v1/models
            self.base_url
                .rsplit_once("/chat/completions")
                .map(|(base, _)| format!("{base}/models"))
                .unwrap_or_else(|| format!("{}/models", self.base_url.trim_end_matches('/')))
        });

        let resp = self
            .client
            .get(&models_url)
            .header(self.auth_header_name(), self.auth_header_value())
            .send()
            .await
            .map_err(|e| Error::Provider(format!("http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!(
                "http {status}: {}",
                super::redact_key(&text, &self.api_key)
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("json: {e}")))?;
        let prefix = self.strip_model_prefix.as_deref().unwrap_or("");
        let mut out: Vec<ModelInfo> = v
            .get("data")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let raw = m.get("id").and_then(Value::as_str)?;
                        // Prefix the listing so users can paste IDs straight
                        // into `/model` (e.g. `ap/gemma4-12b`). `detect()`
                        // routes on this prefix; the stream call strips it
                        // before hitting the remote.
                        let id = if prefix.is_empty() || raw.starts_with(prefix) {
                            raw.to_string()
                        } else {
                            format!("{prefix}{raw}")
                        };
                        Some(ModelInfo {
                            id,
                            display_name: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    async fn stream(&self, mut req: StreamRequest) -> Result<EventStream> {
        if let Some(prefix) = &self.strip_model_prefix {
            if let Some(rest) = req.model.strip_prefix(prefix.as_str()) {
                req.model = rest.to_string();
            }
        }
        let body = Self::build_body(&req);
        let resp = self
            .client
            .post(&self.base_url)
            .header(self.auth_header_name(), self.auth_header_value())
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("http: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!(
                "http {status}: {}",
                super::redact_key(&text, &self.api_key)
            )));
        }

        let byte_stream = resp.bytes_stream();
        let raw_dump = super::RawDump::new(format!("openai {}", req.model));

        let event_stream = try_stream! {
            let mut buffer = String::new();
            let mut byte_stream = Box::pin(byte_stream);
            let mut state = ParseState::default();
            let mut raw = raw_dump;

            while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk.map_err(|e| Error::Provider(format!("stream: {e}")))?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(boundary) = buffer.find("\n\n") {
                    let event_text: String = buffer.drain(..boundary + 2).collect();
                    let trimmed = event_text.trim_end_matches('\n');
                    for event in parse_chunk(trimmed, &mut state)? {
                        if let ProviderEvent::TextDelta(ref s) = event { raw.push(s); }
                        yield event;
                    }
                }
            }

            for event in state.flush_eof() {
                yield event;
            }
            raw.flush();
        };

        Ok(Box::pin(event_stream))
    }
}

#[derive(Default, Debug)]
pub struct ParseState {
    pub seen_message_start: bool,
    pub active_tool_index: Option<i64>,
    pub emitted_message_stop: bool,
}

impl ParseState {
    fn flush_eof(&mut self) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        if self.active_tool_index.is_some() {
            out.push(ProviderEvent::ContentBlockStop);
            self.active_tool_index = None;
        }
        out
    }
}

fn parse_openai_usage(v: &Value) -> Option<Usage> {
    let u = v.get("usage")?;
    let input = u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output = u
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if input == 0 && output == 0 {
        return None;
    }
    Some(Usage {
        input_tokens: input as u32,
        output_tokens: output as u32,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    })
}

/// Parse a single SSE chunk (one `data: {...}` event). Stateful: call with a
/// persistent `ParseState` across the lifetime of the stream.
pub fn parse_chunk(raw: &str, state: &mut ParseState) -> Result<Vec<ProviderEvent>> {
    let mut out = Vec::new();

    let mut data_line: Option<&str> = None;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("data: ") {
            data_line = Some(rest);
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_line = Some(rest);
        }
    }
    let Some(data) = data_line else {
        return Ok(out);
    };
    if data.trim() == "[DONE]" {
        return Ok(out);
    }

    let v: Value = serde_json::from_str(data)?;

    // Some OpenAI-compatible gateways return HTTP 200 but wrap an upstream
    // error inside a single SSE data frame (e.g. `data: {"error": {...}}`).
    // Surface it as a hard error instead of silently completing with no
    // output.
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        return Err(Error::Provider(format!("upstream error: {msg}")));
    }

    if !state.seen_message_start {
        let model = v
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        out.push(ProviderEvent::MessageStart { model });
        state.seen_message_start = true;
    }

    let Some(choices) = v.get("choices").and_then(Value::as_array) else {
        return Ok(out);
    };
    let Some(choice) = choices.first() else {
        // Final `stream_options.include_usage` frame: choices is an empty
        // array and the top-level chunk carries `usage`. DashScope + OpenAI
        // both do this. Emit a MessageStop carrying the usage so the agent's
        // cumulative_usage picks it up — otherwise we report 0in/0out.
        if state.emitted_message_stop {
            if let Some(usage) = parse_openai_usage(&v) {
                out.push(ProviderEvent::MessageStop {
                    stop_reason: Some("stop".into()),
                    usage: Some(usage),
                });
            }
        }
        return Ok(out);
    };

    if let Some(delta) = choice.get("delta") {
        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            if !content.is_empty() {
                out.push(ProviderEvent::TextDelta(content.to_string()));
            }
        }

        // Reasoning models (DeepSeek v4-*, OpenAI o-series via OpenRouter)
        // emit `delta.reasoning_content` alongside `delta.content`. Capture
        // it as a ThinkingDelta so it gets folded into a Thinking block and
        // can be echoed back on the next turn — the server requires the
        // prior reasoning_content in history or returns 400.
        if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
            if !reasoning.is_empty() {
                out.push(ProviderEvent::ThinkingDelta(reasoning.to_string()));
            }
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tool_calls {
                let index = tc.get("index").and_then(Value::as_i64).unwrap_or(0);
                let func = tc.get("function");

                if state.active_tool_index != Some(index) {
                    if state.active_tool_index.is_some() {
                        out.push(ProviderEvent::ContentBlockStop);
                    }
                    state.active_tool_index = Some(index);

                    let id = tc
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = func
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    out.push(ProviderEvent::ToolUseStart { id, name });
                }

                if let Some(args) = func
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                {
                    if !args.is_empty() {
                        out.push(ProviderEvent::ToolUseDelta {
                            partial_json: args.to_string(),
                        });
                    }
                }
            }
        }
    }

    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        if state.active_tool_index.is_some() {
            out.push(ProviderEvent::ContentBlockStop);
            state.active_tool_index = None;
        }
        out.push(ProviderEvent::MessageStop {
            stop_reason: Some(reason.to_string()),
            usage: parse_openai_usage(&v),
        });
        state.emitted_message_stop = true;
    }

    // With stream_options.include_usage, a final chunk has usage but empty choices.
    // Emit a MessageStop with usage if we already emitted one without.
    if state.emitted_message_stop {
        if let Some(usage) = parse_openai_usage(&v) {
            out.push(ProviderEvent::MessageStop {
                stop_reason: Some("stop".into()),
                usage: Some(usage),
            });
        }
    }

    Ok(out)
}

/// Allowlist of OpenAI-compat model id patterns whose chat-completions API
/// emits and requires `reasoning_content` to be echoed back in subsequent
/// turns. Conservative by default — anything not on this list will have
/// `Thinking` blocks dropped during serialization, so non-thinking models
/// get exactly the same wire bytes as before this change. Add new
/// thinking-model families here as they appear.
///
/// Matches by substring against the model id (after `strip_model_prefix`
/// has run, so the `openrouter/` prefix is already removed). The bare id
/// is what the upstream provider sees, so e.g. `deepseek/deepseek-v4-flash`
/// is what we test against.
pub fn model_uses_reasoning_content(model: &str) -> bool {
    const PATTERNS: &[&str] = &[
        // DeepSeek's v4 line — the symptom that drove this fix.
        "deepseek/deepseek-v4",
        "deepseek-v4",
        // OpenAI o-series via OpenRouter (`openai/o1-mini`, `openai/o3`,
        // etc). Direct OpenAI calls go through Responses API, not this
        // chat-completions client, so this only catches the OpenRouter
        // proxy form.
        "openai/o1",
        "openai/o3",
        "openai/o4",
        // DeepSeek r1 family also returns reasoning_content.
        "deepseek/deepseek-r1",
        "deepseek-r1",
    ];
    let m = model.to_lowercase();
    PATTERNS.iter().any(|p| m.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{assemble, collect_turn};
    use crate::types::Message;

    fn parse_all(chunks: &[&str]) -> Vec<ProviderEvent> {
        let mut state = ParseState::default();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(parse_chunk(c, &mut state).unwrap());
        }
        out.extend(state.flush_eof());
        out
    }

    #[test]
    fn parse_text_chunk_emits_message_start_and_text_delta() {
        let events = parse_all(&[
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}",
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}",
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"}}]}",
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}",
            "data: [DONE]",
        ]);

        assert_eq!(
            events[0],
            ProviderEvent::MessageStart {
                model: "gpt-4o".into()
            }
        );
        assert_eq!(events[1], ProviderEvent::TextDelta("Hello".into()));
        assert_eq!(events[2], ProviderEvent::TextDelta(" world".into()));
        match &events[3] {
            ProviderEvent::MessageStop { stop_reason, .. } => {
                assert_eq!(stop_reason.as_deref(), Some("stop"));
            }
            e => panic!("expected MessageStop, got {:?}", e),
        }
        assert_eq!(events.len(), 4);
    }

    #[test]
    fn final_empty_choices_chunk_emits_usage_stop() {
        // DashScope (and OpenAI with stream_options.include_usage) send a
        // trailing frame with `choices: []` and the real token counts. We
        // must not drop it — otherwise the turn reports 0in/0out.
        let events = parse_all(&[
            "data: {\"id\":\"1\",\"model\":\"qwen-max\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}",
            "data: {\"id\":\"1\",\"model\":\"qwen-max\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}",
            "data: {\"id\":\"1\",\"model\":\"qwen-max\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3,\"total_tokens\":14}}",
            "data: [DONE]",
        ]);

        let usage_stops: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::MessageStop { usage: Some(u), .. } => Some(u),
                _ => None,
            })
            .collect();
        assert_eq!(
            usage_stops.len(),
            1,
            "expected a MessageStop carrying usage"
        );
        assert_eq!(usage_stops[0].input_tokens, 11);
        assert_eq!(usage_stops[0].output_tokens, 3);
    }

    #[test]
    fn parse_tool_call_streams_and_flushes_stop_on_finish() {
        let events = parse_all(&[
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}",
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}",
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"pa\"}}]}}]}",
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"/tmp/x\\\"}\"}}]}}]}",
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}",
            "data: [DONE]",
        ]);

        // Expected sequence:
        // MessageStart, ToolUseStart(call_abc, read_file),
        // ToolUseDelta('{\"pa'), ToolUseDelta('th\":\"/tmp/x\"}'),
        // ContentBlockStop, MessageStop("tool_calls")
        assert!(matches!(events[0], ProviderEvent::MessageStart { .. }));
        assert_eq!(
            events[1],
            ProviderEvent::ToolUseStart {
                id: "call_abc".into(),
                name: "read_file".into()
            }
        );
        assert_eq!(
            events[2],
            ProviderEvent::ToolUseDelta {
                partial_json: "{\"pa".into()
            }
        );
        assert_eq!(
            events[3],
            ProviderEvent::ToolUseDelta {
                partial_json: "th\":\"/tmp/x\"}".into()
            }
        );
        assert_eq!(events[4], ProviderEvent::ContentBlockStop);
        assert!(matches!(events[5], ProviderEvent::MessageStop { .. }));
        assert_eq!(events.len(), 6);
    }

    #[test]
    fn parse_two_tool_calls_emits_stop_between_indexes() {
        let events = parse_all(&[
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"type\":\"function\",\"function\":{\"name\":\"r\",\"arguments\":\"{}\"}}]}}]}",
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"b\",\"type\":\"function\",\"function\":{\"name\":\"w\",\"arguments\":\"{}\"}}]}}]}",
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}",
        ]);

        // MessageStart,
        // ToolUseStart(a), ToolUseDelta({}),
        // ContentBlockStop (index switch 0→1),
        // ToolUseStart(b), ToolUseDelta({}),
        // ContentBlockStop (finish_reason),
        // MessageStop
        assert!(matches!(events[0], ProviderEvent::MessageStart { .. }));
        assert_eq!(
            events[1],
            ProviderEvent::ToolUseStart {
                id: "a".into(),
                name: "r".into()
            }
        );
        assert_eq!(
            events[2],
            ProviderEvent::ToolUseDelta {
                partial_json: "{}".into()
            }
        );
        assert_eq!(events[3], ProviderEvent::ContentBlockStop);
        assert_eq!(
            events[4],
            ProviderEvent::ToolUseStart {
                id: "b".into(),
                name: "w".into()
            }
        );
        assert_eq!(
            events[5],
            ProviderEvent::ToolUseDelta {
                partial_json: "{}".into()
            }
        );
        assert_eq!(events[6], ProviderEvent::ContentBlockStop);
        assert!(matches!(events[7], ProviderEvent::MessageStop { .. }));
    }

    #[test]
    fn parse_done_marker_is_noop() {
        let mut state = ParseState::default();
        let events = parse_chunk("data: [DONE]", &mut state).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn messages_to_openai_splits_tool_results_into_tool_role() {
        let req = StreamRequest {
            model: "gpt-4o".into(),
            system: Some("be helpful".into()),
            messages: vec![
                Message::user("hi"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "read".into(),
                        input: json!({"path": "/a"}),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".into(),
                        content: "hello file".into(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let msgs = OpenAIProvider::messages_to_openai(&req);
        // system, user(hi), assistant(tool_calls), tool(result)
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be helpful");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hi");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], Value::Null);
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(
            msgs[2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"/a\"}"
        );
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[3]["content"], "hello file");
    }

    #[test]
    fn messages_to_openai_image_tool_result_emits_synthetic_user_message() {
        // ToolResult with Blocks (Image + Text) — OpenAI's tool-role
        // message must stay text-only (the summary), and a synthetic
        // user message with an image_url block must follow so a
        // vision-capable model can actually see the pixels.
        use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};
        let req = StreamRequest {
            model: "gpt-4o".into(),
            system: None,
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "call_2".into(),
                        name: "Read".into(),
                        input: json!({"path": "/tmp/x.png"}),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_2".into(),
                        content: ToolResultContent::Blocks(vec![
                            ToolResultBlock::Image {
                                source: ImageSource::Base64 {
                                    media_type: "image/png".into(),
                                    data: "AAAA".into(),
                                },
                            },
                            ToolResultBlock::Text {
                                text: "image: x.png · 1 KB · image/png".into(),
                            },
                        ]),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let msgs = OpenAIProvider::messages_to_openai(&req);
        // assistant(tool_use), tool(text-only summary), user(image_url)
        assert_eq!(msgs.len(), 3, "expected 3 wire messages, got {msgs:#?}");

        // Tool message: text-only summary, NOT the image bytes.
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "call_2");
        assert_eq!(msgs[1]["content"], "image: x.png · 1 KB · image/png");

        // Synthetic user message: intro text + per-image label +
        // image_url block. The intro names the originating call_id;
        // the per-image label "from <call_id>:" repeats it inline so
        // the model can correlate when there are multiple images.
        assert_eq!(msgs[2]["role"], "user");
        let user_content = msgs[2]["content"].as_array().expect("user content array");
        assert_eq!(
            user_content.len(),
            3,
            "expected intro text + image label + image_url block"
        );
        assert_eq!(user_content[0]["type"], "text");
        assert!(
            user_content[0]["text"].as_str().unwrap().contains("call_2"),
            "user-message intro should reference originating tool_call_id"
        );
        assert_eq!(user_content[1]["type"], "text");
        assert!(user_content[1]["text"].as_str().unwrap().contains("call_2"));
        assert_eq!(user_content[2]["type"], "image_url");
        assert_eq!(
            user_content[2]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
    }

    #[test]
    fn messages_to_openai_batched_image_tool_results_emit_tool_messages_back_to_back() {
        // Regression for the v0.3.2-dev image attachment bug: when the
        // model batches N parallel Read calls and each result carries
        // an image, OpenAI's contract requires ALL tool messages to
        // immediately follow the assistant's tool_calls, with no other
        // roles interleaved. The previous (broken) emission inserted
        // a synthetic user message after each individual tool message,
        // producing an assistant→tool→user→tool→user→... shape that
        // OpenAI rejects with `tool_call_ids did not have response
        // messages: ...`.
        //
        // Correct shape: assistant → tool × N → user (combined images).
        use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};
        let req = StreamRequest {
            model: "gpt-4o".into(),
            system: None,
            messages: vec![
                // Assistant batches 3 Read calls in one turn.
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::ToolUse {
                            id: "call_a".into(),
                            name: "Read".into(),
                            input: json!({"path": "/tmp/a.png"}),
                        },
                        ContentBlock::ToolUse {
                            id: "call_b".into(),
                            name: "Read".into(),
                            input: json!({"path": "/tmp/b.png"}),
                        },
                        ContentBlock::ToolUse {
                            id: "call_c".into(),
                            name: "Read".into(),
                            input: json!({"path": "/tmp/c.png"}),
                        },
                    ],
                },
                // User message carries 3 ToolResults (one per call).
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::ToolResult {
                            tool_use_id: "call_a".into(),
                            content: ToolResultContent::Blocks(vec![
                                ToolResultBlock::Image {
                                    source: ImageSource::Base64 {
                                        media_type: "image/png".into(),
                                        data: "AAA".into(),
                                    },
                                },
                                ToolResultBlock::Text {
                                    text: "image: a.png".into(),
                                },
                            ]),
                            is_error: false,
                        },
                        ContentBlock::ToolResult {
                            tool_use_id: "call_b".into(),
                            content: ToolResultContent::Blocks(vec![
                                ToolResultBlock::Image {
                                    source: ImageSource::Base64 {
                                        media_type: "image/png".into(),
                                        data: "BBB".into(),
                                    },
                                },
                                ToolResultBlock::Text {
                                    text: "image: b.png".into(),
                                },
                            ]),
                            is_error: false,
                        },
                        ContentBlock::ToolResult {
                            tool_use_id: "call_c".into(),
                            content: ToolResultContent::Blocks(vec![
                                ToolResultBlock::Image {
                                    source: ImageSource::Base64 {
                                        media_type: "image/png".into(),
                                        data: "CCC".into(),
                                    },
                                },
                                ToolResultBlock::Text {
                                    text: "image: c.png".into(),
                                },
                            ]),
                            is_error: false,
                        },
                    ],
                },
            ],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let msgs = OpenAIProvider::messages_to_openai(&req);

        // Expected sequence:
        //   [0] assistant {tool_calls: [a, b, c]}
        //   [1] tool      tool_call_id=call_a
        //   [2] tool      tool_call_id=call_b
        //   [3] tool      tool_call_id=call_c
        //   [4] user      [text intro, label_a, img_a, label_b, img_b, label_c, img_c]
        assert_eq!(msgs.len(), 5, "expected 5 wire messages, got {msgs:#?}");

        // Three tool messages back-to-back, in input order.
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "call_a");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "call_b");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_c");

        // ONE combined synthetic user message after the tool batch.
        assert_eq!(msgs[4]["role"], "user");
        let user_content = msgs[4]["content"].as_array().expect("user content array");
        // 1 intro + (label + image_url) × 3 = 7 blocks
        assert_eq!(user_content.len(), 7);
        assert_eq!(user_content[0]["type"], "text");
        let intro = user_content[0]["text"].as_str().unwrap();
        assert!(
            intro.contains("call_a") && intro.contains("call_b") && intro.contains("call_c"),
            "intro should list every originating tool_call_id, got: {intro}"
        );

        // Each image is preceded by a "from <call_id>:" label so the
        // model can correlate without relying on positional ordering.
        assert_eq!(user_content[1]["type"], "text");
        assert!(user_content[1]["text"].as_str().unwrap().contains("call_a"));
        assert_eq!(user_content[2]["type"], "image_url");
        assert_eq!(
            user_content[2]["image_url"]["url"],
            "data:image/png;base64,AAA"
        );
        assert!(user_content[3]["text"].as_str().unwrap().contains("call_b"));
        assert_eq!(
            user_content[4]["image_url"]["url"],
            "data:image/png;base64,BBB"
        );
        assert!(user_content[5]["text"].as_str().unwrap().contains("call_c"));
        assert_eq!(
            user_content[6]["image_url"]["url"],
            "data:image/png;base64,CCC"
        );
    }

    #[test]
    fn messages_to_openai_user_message_with_image_uses_array_content() {
        // User attaches an image to a chat message (Phase 4 paste /
        // drag-drop). OpenAI requires array-form `content` whenever
        // an image_url block appears, even if the only sibling block
        // is a text part. Verify the wire shape.
        use crate::types::{ContentBlock, ImageSource};
        let req = StreamRequest {
            model: "gpt-4o".into(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: "what's in this?".into(),
                    },
                    ContentBlock::Image {
                        source: ImageSource::Base64 {
                            media_type: "image/jpeg".into(),
                            data: "ZZZ".into(),
                        },
                    },
                ],
            }],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let msgs = OpenAIProvider::messages_to_openai(&req);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        let content = msgs[0]["content"].as_array().expect("array content");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what's in this?");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/jpeg;base64,ZZZ");
    }

    #[test]
    fn messages_to_openai_text_only_tool_result_skips_synthetic_user() {
        // No images in the tool_result → no synthetic user message
        // (regression guard against accidentally appending an empty
        // user message after every text-only tool call).
        let req = StreamRequest {
            model: "gpt-4o".into(),
            system: None,
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "call_3".into(),
                        name: "Bash".into(),
                        input: json!({"cmd": "ls"}),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_3".into(),
                        content: "file1\nfile2\n".into(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let msgs = OpenAIProvider::messages_to_openai(&req);
        // assistant(tool_use), tool(text) — no synthetic user.
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "tool");
    }

    #[test]
    fn build_body_maps_tools_to_openai_function_shape() {
        use crate::types::ToolDef;
        let req = StreamRequest {
            model: "gpt-4o".into(),
            system: None,
            messages: vec![Message::user("x")],
            tools: vec![ToolDef {
                name: "read_file".into(),
                description: "read a file".into(),
                input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            }],
            max_tokens: 100,
            thinking_budget: None,
        };
        let body = OpenAIProvider::build_body(&req);
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(body["tools"][0]["function"]["description"], "read a file");
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["path"]["type"],
            "string"
        );
    }

    #[tokio::test]
    async fn list_models_parses_data_array() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = r#"{"data":[
            {"id":"gpt-4o","object":"model","owned_by":"openai"},
            {"id":"gpt-4o-mini","object":"model","owned_by":"openai"}
        ]}"#;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let provider = OpenAIProvider::new("test-key")
            .with_base_url(format!("{}/v1/chat/completions", server.uri()));
        let models = provider.list_models().await.expect("list");
        // Sorted
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["gpt-4o", "gpt-4o-mini"]);
    }

    #[tokio::test]
    async fn stream_end_to_end_text_via_wiremock() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let sse_body = concat!(
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there\"}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = OpenAIProvider::new("test-key")
            .with_base_url(format!("{}/v1/chat/completions", server.uri()));
        let req = StreamRequest {
            model: "gpt-4o".into(),
            system: None,
            messages: vec![Message::user("hey")],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let raw = provider.stream(req).await.expect("stream");
        let result = collect_turn(assemble(raw)).await.expect("collect");
        assert_eq!(result.text, "Hi there");
        assert_eq!(result.tool_uses.len(), 0);
        assert_eq!(result.stop_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn stream_end_to_end_tool_use_via_wiremock() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let sse_body = concat!(
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"/tmp/x\\\"}\"}}]}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = OpenAIProvider::new("test-key")
            .with_base_url(format!("{}/v1/chat/completions", server.uri()));
        let req = StreamRequest {
            model: "gpt-4o".into(),
            system: None,
            messages: vec![Message::user("read /tmp/x")],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let raw = provider.stream(req).await.expect("stream");
        let result = collect_turn(assemble(raw)).await.expect("collect");

        assert_eq!(result.text, "");
        assert_eq!(result.tool_uses.len(), 1);
        if let ContentBlock::ToolUse { id, name, input } = &result.tool_uses[0] {
            assert_eq!(id, "call_abc");
            assert_eq!(name, "read_file");
            assert_eq!(input, &json!({"path": "/tmp/x"}));
        } else {
            panic!("expected ToolUse");
        }
        assert_eq!(result.stop_reason.as_deref(), Some("tool_calls"));
    }

    /// DeepSeek v4 (and OpenAI o-series via OpenRouter) emit
    /// `delta.reasoning_content` alongside (or before) `delta.content`.
    /// Verify the parser captures it as a `ThinkingDelta` so the assembly
    /// pipeline can build a `ContentBlock::Thinking` for echo on later turns.
    #[test]
    fn parse_chunk_emits_thinking_delta_for_reasoning_content() {
        let events = parse_all(&[
            "data: {\"id\":\"1\",\"model\":\"deepseek/deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}",
            "data: {\"id\":\"1\",\"model\":\"deepseek/deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"let me think\"}}]}",
            "data: {\"id\":\"1\",\"model\":\"deepseek/deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"}}]}",
            "data: {\"id\":\"1\",\"model\":\"deepseek/deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}",
        ]);
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                ProviderEvent::MessageStart { .. } => "MessageStart",
                ProviderEvent::ThinkingDelta(_) => "ThinkingDelta",
                ProviderEvent::TextDelta(_) => "TextDelta",
                ProviderEvent::ToolUseStart { .. } => "ToolUseStart",
                ProviderEvent::ToolUseDelta { .. } => "ToolUseDelta",
                ProviderEvent::ContentBlockStop => "ContentBlockStop",
                ProviderEvent::MessageStop { .. } => "MessageStop",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["MessageStart", "ThinkingDelta", "TextDelta", "MessageStop"]
        );
        match &events[1] {
            ProviderEvent::ThinkingDelta(s) => assert_eq!(s, "let me think"),
            other => panic!("expected ThinkingDelta, got {other:?}"),
        }
    }

    #[test]
    fn model_uses_reasoning_content_allowlist() {
        // Thinking models (substring match, lowercase-insensitive).
        assert!(model_uses_reasoning_content("deepseek/deepseek-v4-flash"));
        assert!(model_uses_reasoning_content("deepseek/deepseek-v4-pro"));
        assert!(model_uses_reasoning_content("deepseek-v4-flash"));
        assert!(model_uses_reasoning_content("deepseek/deepseek-r1"));
        assert!(model_uses_reasoning_content("openai/o1-mini"));
        assert!(model_uses_reasoning_content("openai/o3"));
        // Non-thinking models — every other workflow's tokens stay
        // unaffected by this change.
        assert!(!model_uses_reasoning_content("gpt-4o"));
        assert!(!model_uses_reasoning_content("openai/gpt-4o"));
        assert!(!model_uses_reasoning_content("deepseek/deepseek-v3.2"));
        assert!(!model_uses_reasoning_content("deepseek/deepseek-chat"));
        assert!(!model_uses_reasoning_content("anthropic/claude-sonnet-4-6"));
        assert!(!model_uses_reasoning_content("qwen/qwen3.6-plus"));
    }

    /// For thinking models, a Thinking block in history must be echoed back
    /// as `reasoning_content` on the assistant message. For non-thinking
    /// models, the same block must be silently dropped (no extra tokens).
    #[test]
    fn messages_to_openai_echoes_reasoning_only_for_thinking_models() {
        let history = vec![
            Message::user("solve x"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        content: "think think".into(),
                        signature: None,
                    },
                    ContentBlock::Text {
                        text: "x = 42".into(),
                    },
                ],
            },
            Message::user("now y"),
        ];

        // Thinking-model target: reasoning_content present.
        let req = StreamRequest {
            model: "deepseek/deepseek-v4-flash".into(),
            system: None,
            messages: history.clone(),
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let msgs = OpenAIProvider::messages_to_openai(&req);
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(assistant["content"], "x = 42");
        assert_eq!(assistant["reasoning_content"], "think think");

        // Non-thinking target: reasoning_content stripped, identical wire
        // bytes to pre-patch behavior.
        let req_plain = StreamRequest {
            model: "gpt-4o".into(),
            system: None,
            messages: history,
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let msgs_plain = OpenAIProvider::messages_to_openai(&req_plain);
        let assistant_plain = msgs_plain
            .iter()
            .find(|m| m["role"] == "assistant")
            .unwrap();
        assert_eq!(assistant_plain["content"], "x = 42");
        assert!(
            assistant_plain.get("reasoning_content").is_none(),
            "non-thinking model must not see reasoning_content; got {assistant_plain:?}"
        );
    }
}
