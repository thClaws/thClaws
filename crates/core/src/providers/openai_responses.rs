//! OpenAI Responses API streaming provider.
//!
//! The Responses API (`/v1/responses`) is OpenAI's newer API that supports
//! server-side conversation state, built-in tools, and models like Codex
//! that don't work with `/v1/chat/completions`.
//!
//! Key differences from chat/completions:
//! - `instructions` instead of system message in messages array
//! - `input` array instead of `messages` (or a `previous_response_id` for
//!   server-side history continuations)
//! - SSE events use typed `event:` lines (`response.output_text.delta`, etc.)
//! - Tool definitions have `name`/`description`/`parameters` at top level
//!   (not nested under `function`)
//!
//! Model prefix: `codex/` (stripped before sending to API).

use super::{EventStream, ModelInfo, Provider, ProviderEvent, Usage};
use crate::error::{Error, Result};
use crate::types::{ContentBlock, Role};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub const DEFAULT_API_URL: &str = "https://api.openai.com/v1/responses";

pub struct OpenAIResponsesProvider {
    client: Client,
    api_key: String,
    base_url: String,
    /// Last response ID for server-side history continuation.
    last_response_id: Arc<Mutex<Option<String>>>,
}

impl OpenAIResponsesProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            base_url: DEFAULT_API_URL.to_string(),
            last_response_id: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Strip the `codex/` prefix if present.
    fn model_id(model: &str) -> &str {
        model.strip_prefix("codex/").unwrap_or(model)
    }

    /// Convert our Message types → Responses API input array.
    fn messages_to_input(req: &super::StreamRequest) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();

        for m in &req.messages {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
            };

            for block in &m.content {
                match block {
                    ContentBlock::Text { text } => {
                        out.push(json!({
                            "role": role,
                            "content": text,
                        }));
                    }
                    // Responses API has its own `reasoning` block format
                    // (different from chat-completions' `reasoning_content`).
                    // For now, drop — when Responses-API thinking models
                    // are wired up, map this to `{"type":"reasoning",...}`.
                    ContentBlock::Thinking { .. } => {}
                    // Image input on the Responses API is technically
                    // supported via `input_image` content blocks but
                    // that path isn't wired here yet — drop on the
                    // wire (the block stays in local history so a
                    // future turn against a vision-capable provider
                    // still sees it).
                    ContentBlock::Image { .. } => {}
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        out.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                        }));
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        out.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": content,
                        }));
                    }
                }
            }
        }

        out
    }

    fn build_body(&self, req: &super::StreamRequest) -> Value {
        let model = Self::model_id(&req.model);
        let input = Self::messages_to_input(req);

        let mut body = json!({
            "model": model,
            "input": input,
            "stream": true,
        });

        // System prompt → instructions field.
        if let Some(sys) = &req.system {
            if !sys.is_empty() {
                body["instructions"] = json!(sys);
            }
        }

        if req.max_tokens > 0 {
            body["max_output_tokens"] = json!(req.max_tokens);
        }

        // Server-side history: continue from last response.
        if let Ok(guard) = self.last_response_id.lock() {
            if let Some(ref id) = *guard {
                body["previous_response_id"] = json!(id);
            }
        }

        // Tools use the Responses API format.
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }

        body
    }
}

#[async_trait]
impl Provider for OpenAIResponsesProvider {
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let models_url = self.base_url.replace("/responses", "/models");

        let resp = self
            .client
            .get(&models_url)
            .header("authorization", format!("Bearer {}", self.api_key))
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
        let mut out: Vec<ModelInfo> = v
            .get("data")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m.get("id").and_then(Value::as_str)?.to_string();
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

    async fn stream(&self, req: super::StreamRequest) -> Result<EventStream> {
        let body = self.build_body(&req);
        let resp = self
            .client
            .post(&self.base_url)
            .header("authorization", format!("Bearer {}", self.api_key))
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
        let id_store = self.last_response_id.clone();

        let id_slot = Arc::new(Mutex::new(None::<String>));
        let id_slot_stream = id_slot.clone();
        let id_slot_final = id_slot.clone();

        let raw_dump = super::RawDump::new(format!("openai-responses {}", req.model));
        let event_stream = try_stream! {
            // M6.21 BUG H1: byte buffer to avoid UTF-8 corruption at
            // chunk boundaries. See providers::find_bytes doc.
            let mut buffer: Vec<u8> = Vec::new();
            let mut byte_stream = Box::pin(byte_stream);
            let mut seen_start = false;
            let mut raw = raw_dump;

            while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk.map_err(|e| Error::Provider(format!("stream: {e}")))?;
                buffer.extend_from_slice(&chunk);

                while let Some(boundary) = super::find_bytes(&buffer, b"\n\n") {
                    let event_bytes: Vec<u8> = buffer.drain(..boundary + 2).collect();
                    let event_text = String::from_utf8_lossy(&event_bytes);
                    let events = parse_response_event(&event_text, &mut seen_start, &id_slot_stream)?;
                    for ev in events {
                        if let ProviderEvent::TextDelta(ref s) = ev { raw.push(s); }
                        yield ev;
                    }
                }
            }

            // Copy the captured response ID for next-turn server-side history.
            if let Ok(captured) = id_slot_final.lock() {
                if let Some(ref id) = *captured {
                    if let Ok(mut store) = id_store.lock() {
                        *store = Some(id.clone());
                    }
                }
            }
            raw.flush();
        };

        Ok(Box::pin(event_stream))
    }
}

/// Parse a single Responses API SSE event.
///
/// The Responses API uses typed `event:` lines:
/// - `response.created` — contains response ID
/// - `response.output_text.delta` — text chunk
/// - `response.function_call_arguments.delta` — tool call argument chunk
/// - `response.function_call_arguments.done` — tool call complete
/// - `response.output_item.added` — new output item (could be function_call)
/// - `response.completed` — final response with usage
fn parse_response_event(
    raw: &str,
    seen_start: &mut bool,
    response_id_slot: &Arc<Mutex<Option<String>>>,
) -> Result<Vec<ProviderEvent>> {
    let mut out = Vec::new();

    // Parse event type and data.
    let mut event_type: Option<&str> = None;
    let mut data_line: Option<&str> = None;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("event: ") {
            event_type = Some(rest.trim());
        } else if let Some(rest) = line.strip_prefix("data: ") {
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
    let etype = event_type.unwrap_or("");

    // Emit MessageStart on first event.
    if !*seen_start {
        let model = v
            .get("model")
            .or_else(|| v.get("response").and_then(|r| r.get("model")))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        out.push(ProviderEvent::MessageStart { model });
        *seen_start = true;
    }

    // Capture response ID for server-side history.
    if let Some(id) = v
        .get("response")
        .and_then(|r| r.get("id"))
        .and_then(Value::as_str)
    {
        if let Ok(mut slot) = response_id_slot.lock() {
            *slot = Some(id.to_string());
        }
    }
    // Also check top-level id for response.created events.
    if etype == "response.created" || etype == "response.in_progress" {
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            if let Ok(mut slot) = response_id_slot.lock() {
                *slot = Some(id.to_string());
            }
        }
    }

    match etype {
        "response.output_text.delta" => {
            if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    out.push(ProviderEvent::TextDelta(delta.to_string()));
                }
            }
        }
        "response.output_item.added" => {
            // A new output item — could be a function_call.
            if let Some(item) = v.get("item") {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    out.push(ProviderEvent::ToolUseStart {
                        id,
                        name,
                        thought_signature: None,
                    });
                }
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    out.push(ProviderEvent::ToolUseDelta {
                        partial_json: delta.to_string(),
                    });
                }
            }
        }
        "response.function_call_arguments.done" => {
            out.push(ProviderEvent::ContentBlockStop);
        }
        "response.output_text.done" => {
            // Text output complete — could emit ContentBlockStop if needed.
        }
        "response.completed" => {
            let usage = v.get("response").and_then(|r| r.get("usage")).map(|u| {
                let total_input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                let output = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                // M6.22 BUG G2: surface Responses API's auto-cache.
                // Responses puts cached counts under
                // `response.usage.input_tokens_details.cached_tokens`
                // (parallel to Chat Completions' `prompt_tokens_details`,
                // but renamed since Responses uses `input_tokens` not
                // `prompt_tokens`). Pre-fix this was hardcoded None,
                // hiding the auto-cache savings (and the implicit
                // server-side history continuation via previous_response_id)
                // from the per-turn pill and daily totals.
                //
                // Subtract cached from total_input so the canonical
                // `Usage.input_tokens` is the UNCACHED new portion —
                // matching Anthropic semantics. Without this, daily
                // totals would double-count (input + cache_read).
                let cached = u
                    .pointer("/input_tokens_details/cached_tokens")
                    .and_then(Value::as_u64);
                let cached_count = cached.unwrap_or(0);
                let uncached_input = total_input.saturating_sub(cached_count);
                Usage {
                    input_tokens: uncached_input as u32,
                    output_tokens: output as u32,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: cached.map(|v| v as u32),
                }
            });
            let stop_reason = v
                .get("response")
                .and_then(|r| r.get("status"))
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            out.push(ProviderEvent::MessageStop { stop_reason, usage });
        }
        _ => {
            // Ignore unknown event types (response.created, response.in_progress, etc.)
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_events(chunks: &[(&str, &str)]) -> Vec<ProviderEvent> {
        let mut seen_start = false;
        let id_slot = Arc::new(Mutex::new(None));
        let mut out = Vec::new();
        for (etype, data) in chunks {
            let raw = format!("event: {etype}\ndata: {data}\n\n");
            out.extend(parse_response_event(&raw, &mut seen_start, &id_slot).unwrap());
        }
        out
    }

    #[test]
    fn text_delta_emits_message_start_and_text() {
        let events = parse_events(&[
            (
                "response.created",
                r#"{"id":"resp_abc","model":"gpt-5.2-codex","status":"in_progress"}"#,
            ),
            (
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","delta":"Hello"}"#,
            ),
            (
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","delta":" world"}"#,
            ),
            (
                "response.completed",
                r#"{"type":"response.completed","response":{"id":"resp_abc","model":"gpt-5.2-codex","status":"completed","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            ),
        ]);

        assert!(matches!(events[0], ProviderEvent::MessageStart { .. }));
        assert_eq!(events[1], ProviderEvent::TextDelta("Hello".into()));
        assert_eq!(events[2], ProviderEvent::TextDelta(" world".into()));
        assert!(matches!(events[3], ProviderEvent::MessageStop { .. }));
    }

    #[test]
    fn function_call_emits_tool_use_events() {
        let events = parse_events(&[
            (
                "response.created",
                r#"{"id":"resp_abc","model":"gpt-5.2-codex"}"#,
            ),
            (
                "response.output_item.added",
                r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"Read"}}"#,
            ),
            (
                "response.function_call_arguments.delta",
                r#"{"type":"response.function_call_arguments.delta","delta":"{\"path\":"}"#,
            ),
            (
                "response.function_call_arguments.delta",
                r#"{"type":"response.function_call_arguments.delta","delta":"\"/tmp\"}"}"#,
            ),
            (
                "response.function_call_arguments.done",
                r#"{"type":"response.function_call_arguments.done","arguments":"{\"path\":\"/tmp\"}"}"#,
            ),
            (
                "response.completed",
                r#"{"type":"response.completed","response":{"id":"resp_abc","status":"completed","usage":{"input_tokens":20,"output_tokens":10}}}"#,
            ),
        ]);

        assert!(matches!(events[0], ProviderEvent::MessageStart { .. }));
        assert_eq!(
            events[1],
            ProviderEvent::ToolUseStart {
                id: "call_1".into(),
                name: "Read".into(),
                thought_signature: None,
            }
        );
        assert!(matches!(events[2], ProviderEvent::ToolUseDelta { .. }));
        assert!(matches!(events[3], ProviderEvent::ToolUseDelta { .. }));
        assert_eq!(events[4], ProviderEvent::ContentBlockStop);
        assert!(matches!(events[5], ProviderEvent::MessageStop { .. }));
    }

    #[test]
    fn response_id_captured() {
        let id_slot = Arc::new(Mutex::new(None));
        let mut seen = false;
        parse_response_event(
            "event: response.created\ndata: {\"id\":\"resp_xyz\",\"model\":\"gpt-5.2-codex\"}\n\n",
            &mut seen,
            &id_slot,
        )
        .unwrap();
        assert_eq!(id_slot.lock().unwrap().as_deref(), Some("resp_xyz"));
    }

    /// M6.22 BUG G2: Responses API auto-cache stats must surface from
    /// `response.usage.input_tokens_details.cached_tokens`. Pre-fix this
    /// was hardcoded None, hiding the cache savings from the user even
    /// though the server-side cache + previous_response_id continuation
    /// was applying the discount.
    #[test]
    fn response_completed_extracts_cached_tokens_from_input_tokens_details() {
        let events = parse_events(&[(
            "response.completed",
            r#"{"type":"response.completed","response":{"id":"resp_xyz","status":"completed","usage":{"input_tokens":5000,"output_tokens":200,"input_tokens_details":{"cached_tokens":4500}}}}"#,
        )]);
        let stop = events
            .iter()
            .find_map(|e| match e {
                ProviderEvent::MessageStop { usage: Some(u), .. } => Some(u),
                _ => None,
            })
            .expect("MessageStop with usage");
        // input_tokens reports the UNCACHED portion (5000 total - 4500 cached = 500 new).
        // Daily-totals math: input + cache_read = 500 + 4500 = 5000 (correct billable).
        assert_eq!(stop.input_tokens, 500);
        assert_eq!(stop.output_tokens, 200);
        assert_eq!(stop.cache_read_input_tokens, Some(4500));
        assert_eq!(stop.cache_creation_input_tokens, None);
    }

    #[test]
    fn model_id_strips_prefix() {
        assert_eq!(
            OpenAIResponsesProvider::model_id("codex/gpt-5.2-codex"),
            "gpt-5.2-codex"
        );
        assert_eq!(
            OpenAIResponsesProvider::model_id("gpt-5.2-codex"),
            "gpt-5.2-codex"
        );
    }
}
