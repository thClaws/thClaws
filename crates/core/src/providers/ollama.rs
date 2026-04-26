//! Ollama provider — `/api/chat` with NDJSON streaming.
//!
//! Wire format notes (vs OpenAI):
//! - Endpoint: `{base}/api/chat`. Default base: `http://localhost:11434`.
//! - No auth. Anything listening on the URL is trusted.
//! - Stream is **NDJSON**, not SSE: one complete JSON object per line, no
//!   `data: ` prefix, no `[DONE]` terminator. Final line has `"done": true`.
//! - Messages shape is similar to OpenAI's chat/completions but simpler:
//!   `{role, content}` for text; `{role: "assistant", content, tool_calls}`
//!   for tool uses; `{role: "tool", content}` for tool results (no
//!   tool_call_id — Ollama relies on order).
//! - Tool calls are **not streamed**: the entire `function.arguments` object
//!   arrives in one message payload with `done: false`, then a final `done:
//!   true` line with usage metadata.
//!
//! Model string handling: we strip an `ollama/` prefix before sending, so a
//! config like `model = "ollama/llama3.2"` hits the native llama3.2 model.

use super::{EventStream, ModelInfo, Provider, ProviderEvent, StreamRequest, Usage};
use crate::error::{Error, Result};
use crate::types::{ContentBlock, Role};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};

pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";

pub struct OllamaProvider {
    client: Client,
    base_url: String,
}

impl OllamaProvider {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    fn model_name(req_model: &str) -> &str {
        req_model.strip_prefix("ollama/").unwrap_or(req_model)
    }

    /// `POST /api/show` — returns the chosen context window for `model` so
    /// callers can cache it in the catalogue. Prefers `parameters.num_ctx`
    /// (what this Ollama instance will actually accept per turn) and falls
    /// back to `model_info.<arch>.context_length` (the model's native
    /// ceiling). Returns `(context, source_note)` where `source_note` is
    /// `"num_ctx"` or `"native"` — useful for the catalogue's `source` field.
    pub async fn show(&self, model: &str) -> Result<(u32, &'static str)> {
        let name = Self::model_name(model);
        let url = format!("{}/api/show", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .json(&json!({ "model": name }))
            .send()
            .await
            .map_err(|e| Error::Provider(format!("ollama /api/show http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!(
                "ollama /api/show {status}: {text}"
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("ollama /api/show json: {e}")))?;

        // `parameters` is a space-separated string like "num_ctx 8192\nstop ...".
        if let Some(params) = v.get("parameters").and_then(Value::as_str) {
            for line in params.lines() {
                let mut it = line.split_whitespace();
                if it.next() == Some("num_ctx") {
                    if let Some(n) = it.next().and_then(|s| s.parse::<u32>().ok()) {
                        return Ok((n, "num_ctx"));
                    }
                }
            }
        }

        // `model_info` is an object with keys like `llama.context_length`,
        // `qwen2.context_length`, `phi3.context_length` — one per model
        // architecture. Scan values for the first `<arch>.context_length`.
        if let Some(info) = v.get("model_info").and_then(Value::as_object) {
            for (k, val) in info {
                if k.ends_with(".context_length") {
                    if let Some(n) = val.as_u64() {
                        return Ok((n as u32, "native"));
                    }
                }
            }
        }

        Err(Error::Provider(
            "ollama /api/show did not report context_length".into(),
        ))
    }

    fn messages_to_ollama(req: &StreamRequest) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();

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
            let mut tool_calls: Vec<Value> = Vec::new();
            let mut trailing_tool_results: Vec<String> = Vec::new();

            for block in &m.content {
                match block {
                    ContentBlock::Text { text } => text_parts.push(text.clone()),
                    // Ollama's chat API has no reasoning_content field.
                    // Drop the block — it stays in our local history but
                    // doesn't go on the wire.
                    ContentBlock::Thinking { .. } => {}
                    // Ollama's stock chat API doesn't support
                    // image attachments either (vision models like
                    // llava use a separate `images: [...]` field that
                    // we don't yet plumb). Drop the block on the wire
                    // for now — it stays in local history so a future
                    // turn against an Anthropic / OpenAI / Gemini
                    // model still sees it.
                    ContentBlock::Image { .. } => {}
                    ContentBlock::ToolUse { name, input, .. } => {
                        tool_calls.push(json!({
                            "function": { "name": name, "arguments": input },
                        }));
                    }
                    ContentBlock::ToolResult { content, .. } => {
                        // Ollama is text-only; flatten any multimodal
                        // blocks via to_text() so the Read-an-image
                        // path doesn't 500 — the model gets the
                        // accompanying text summary instead of pixels.
                        trailing_tool_results.push(content.to_text());
                    }
                }
            }

            let content = text_parts.join("");
            let has_text = !content.is_empty();
            let has_tools = !tool_calls.is_empty();

            if has_text || has_tools {
                let mut msg = json!({"role": role, "content": content});
                if has_tools {
                    msg["tool_calls"] = json!(tool_calls);
                }
                out.push(msg);
            }

            // Ollama doesn't have tool_call_id — emit tool results as separate
            // `role: "tool"` messages in call order.
            for content in trailing_tool_results {
                out.push(json!({"role": "tool", "content": content}));
            }
        }

        out
    }

    fn build_body(req: &StreamRequest) -> Value {
        let model = Self::model_name(&req.model);
        let messages = Self::messages_to_ollama(req);
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
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

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/api/tags", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!("http {status}: {text}")));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("json: {e}")))?;
        let mut out: Vec<ModelInfo> = v
            .get("models")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let name = m.get("name").and_then(Value::as_str)?.to_string();
                        // Prefix with `ollama/` so users can paste it straight into `/model`.
                        Some(ModelInfo {
                            id: format!("ollama/{name}"),
                            display_name: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    async fn stream(&self, req: StreamRequest) -> Result<EventStream> {
        let body = Self::build_body(&req);
        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));

        let resp = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("http: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!("http {status}: {text}")));
        }

        let byte_stream = resp.bytes_stream();
        let raw_dump = super::RawDump::new(format!("ollama {}", req.model));

        let event_stream = try_stream! {
            let mut buffer = String::new();
            let mut byte_stream = Box::pin(byte_stream);
            let mut state = ParseState::default();
            let mut raw = raw_dump;

            while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk.map_err(|e| Error::Provider(format!("stream: {e}")))?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(newline) = buffer.find('\n') {
                    let line: String = buffer.drain(..newline + 1).collect();
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    for event in parse_line(line, &mut state)? {
                        if let ProviderEvent::TextDelta(ref s) = event { raw.push(s); }
                        yield event;
                    }
                }
            }

            // Flush any trailing buffered line (Ollama normally terminates with \n but be safe).
            if !buffer.trim().is_empty() {
                for event in parse_line(buffer.trim(), &mut state)? {
                    if let ProviderEvent::TextDelta(ref s) = event { raw.push(s); }
                    yield event;
                }
            }
            raw.flush();
        };

        Ok(Box::pin(event_stream))
    }
}

#[derive(Default, Debug)]
pub struct ParseState {
    pub seen_message_start: bool,
}

/// Parse a single NDJSON line from the Ollama stream. Emits zero or more events.
pub fn parse_line(line: &str, state: &mut ParseState) -> Result<Vec<ProviderEvent>> {
    let mut out = Vec::new();
    let v: Value = serde_json::from_str(line)?;

    if !state.seen_message_start {
        let model = v
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        out.push(ProviderEvent::MessageStart { model });
        state.seen_message_start = true;
    }

    // Text content delta.
    if let Some(text) = v.pointer("/message/content").and_then(Value::as_str) {
        if !text.is_empty() {
            out.push(ProviderEvent::TextDelta(text.to_string()));
        }
    }

    // Ollama Cloud uses a sibling `thinking` field on the message.
    if let Some(thinking) = v.pointer("/message/thinking").and_then(Value::as_str) {
        if !thinking.is_empty() {
            out.push(ProviderEvent::ThinkingDelta(thinking.to_string()));
        }
    }
    // Tool calls (non-streamed — entire call arrives in one message payload).
    if let Some(tool_calls) = v.pointer("/message/tool_calls").and_then(Value::as_array) {
        for (i, tc) in tool_calls.iter().enumerate() {
            let id = tc
                .get("id")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| format!("ollama-call-{i}"));
            let name = tc
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args_json = tc
                .pointer("/function/arguments")
                .map(|a| {
                    if let Some(s) = a.as_str() {
                        s.to_string()
                    } else {
                        a.to_string()
                    }
                })
                .unwrap_or_else(|| "{}".to_string());

            out.push(ProviderEvent::ToolUseStart { id, name });
            if !args_json.is_empty() {
                out.push(ProviderEvent::ToolUseDelta {
                    partial_json: args_json,
                });
            }
            out.push(ProviderEvent::ContentBlockStop);
        }
    }

    // Done marker with usage.
    if v.get("done").and_then(Value::as_bool).unwrap_or(false) {
        let stop_reason = v
            .get("done_reason")
            .and_then(Value::as_str)
            .map(String::from);
        let usage = if v.get("prompt_eval_count").is_some() || v.get("eval_count").is_some() {
            Some(Usage {
                input_tokens: v
                    .get("prompt_eval_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32,
                output_tokens: v.get("eval_count").and_then(Value::as_u64).unwrap_or(0) as u32,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            })
        } else {
            None
        };
        out.push(ProviderEvent::MessageStop { stop_reason, usage });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{assemble, collect_turn};
    use crate::types::Message;

    fn parse_all(lines: &[&str]) -> Vec<ProviderEvent> {
        let mut state = ParseState::default();
        let mut out = Vec::new();
        for l in lines {
            out.extend(parse_line(l, &mut state).unwrap());
        }
        out
    }

    #[test]
    fn parse_text_stream_emits_message_start_deltas_and_stop() {
        let events = parse_all(&[
            r#"{"model":"llama3.2","message":{"role":"assistant","content":"Hello"},"done":false}"#,
            r#"{"model":"llama3.2","message":{"role":"assistant","content":" world"},"done":false}"#,
            r#"{"model":"llama3.2","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":5,"eval_count":2}"#,
        ]);
        assert!(matches!(events[0], ProviderEvent::MessageStart { .. }));
        assert_eq!(events[1], ProviderEvent::TextDelta("Hello".into()));
        assert_eq!(events[2], ProviderEvent::TextDelta(" world".into()));
        match &events[3] {
            ProviderEvent::MessageStop { stop_reason, usage } => {
                assert_eq!(stop_reason.as_deref(), Some("stop"));
                let u = usage.as_ref().unwrap();
                assert_eq!(u.input_tokens, 5);
                assert_eq!(u.output_tokens, 2);
            }
            e => panic!("expected MessageStop, got {:?}", e),
        }
    }

    #[test]
    fn parse_tool_call_emits_complete_tool_use() {
        let events = parse_all(&[
            r#"{"model":"llama3.2","message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"Read","arguments":{"path":"/tmp/x"}}}]},"done":false}"#,
            r#"{"model":"llama3.2","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop"}"#,
        ]);
        // MessageStart, ToolUseStart, ToolUseDelta, ContentBlockStop, MessageStop
        assert!(matches!(events[0], ProviderEvent::MessageStart { .. }));
        assert_eq!(
            events[1],
            ProviderEvent::ToolUseStart {
                id: "ollama-call-0".into(),
                name: "Read".into(),
            }
        );
        match &events[2] {
            ProviderEvent::ToolUseDelta { partial_json } => {
                assert!(partial_json.contains("path"));
                assert!(partial_json.contains("/tmp/x"));
            }
            e => panic!("expected ToolUseDelta, got {:?}", e),
        }
        assert_eq!(events[3], ProviderEvent::ContentBlockStop);
        assert!(matches!(events[4], ProviderEvent::MessageStop { .. }));
    }

    #[test]
    fn model_name_strips_ollama_prefix() {
        assert_eq!(OllamaProvider::model_name("ollama/llama3.2"), "llama3.2");
        assert_eq!(OllamaProvider::model_name("llama3.2"), "llama3.2");
    }

    #[test]
    fn messages_to_ollama_system_prepended_and_tool_results_emitted_as_tool_role() {
        let req = StreamRequest {
            model: "ollama/llama3.2".into(),
            system: Some("be brief".into()),
            messages: vec![
                Message::user("hi"),
                crate::types::Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "Read".into(),
                        input: json!({"path": "/x"}),
                    }],
                },
                crate::types::Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "file body".into(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let msgs = OllamaProvider::messages_to_ollama(&req);
        // system + user(hi) + assistant(tool_calls) + tool(result)
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be brief");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hi");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "Read");
        assert_eq!(
            msgs[2]["tool_calls"][0]["function"]["arguments"]["path"],
            "/x"
        );
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["content"], "file body");
    }

    #[test]
    fn build_body_strips_prefix_and_sets_stream_true() {
        let req = StreamRequest {
            model: "ollama/llama3.2".into(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 0,
            thinking_budget: None,
        };
        let body = OllamaProvider::build_body(&req);
        assert_eq!(body["model"], "llama3.2");
        assert_eq!(body["stream"], true);
    }

    #[tokio::test]
    async fn show_prefers_num_ctx_over_native() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // `num_ctx` in parameters wins — that's what the instance will
        // actually accept per turn, regardless of the model's native ceiling.
        let body = r#"{
            "parameters": "num_ctx 8192\nstop \"</s>\"\ntemperature 0.7",
            "model_info": {
                "llama.context_length": 131072
            }
        }"#;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new().with_base_url(server.uri());
        let (ctx, which) = provider.show("llama3.2").await.expect("show");
        assert_eq!(ctx, 8192);
        assert_eq!(which, "num_ctx");
    }

    #[tokio::test]
    async fn show_falls_back_to_native_context_length() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // No `num_ctx` in parameters → fall through to model_info native.
        let body = r#"{
            "parameters": "stop \"</s>\"\ntemperature 0.7",
            "model_info": {
                "qwen2.context_length": 32768
            }
        }"#;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new().with_base_url(server.uri());
        let (ctx, which) = provider.show("qwen2.5:7b").await.expect("show");
        assert_eq!(ctx, 32768);
        assert_eq!(which, "native");
    }

    #[tokio::test]
    async fn show_strips_ollama_prefix_from_model_id() {
        use serde_json::json;
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = r#"{"parameters":"num_ctx 4096"}"#;
        // Verify the request body contains the BARE model name, not the
        // `ollama/` prefix the user typed.
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .and(body_partial_json(json!({"model": "llama3.2"})))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new().with_base_url(server.uri());
        let (ctx, _) = provider.show("ollama/llama3.2").await.expect("show");
        assert_eq!(ctx, 4096);
    }

    #[tokio::test]
    async fn list_models_from_api_tags() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = r#"{"models":[
            {"name":"llama3.2:latest","size":123},
            {"name":"qwen2.5-coder:7b","size":456}
        ]}"#;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new().with_base_url(server.uri());
        let models = provider.list_models().await.expect("list");
        // Names are prefixed with `ollama/` so users can paste them into /model.
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["ollama/llama3.2:latest", "ollama/qwen2.5-coder:7b"]
        );
    }

    #[tokio::test]
    async fn stream_end_to_end_text_via_wiremock() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let ndjson = concat!(
            "{\"model\":\"llama3.2\",\"message\":{\"role\":\"assistant\",\"content\":\"Hi\"},\"done\":false}\n",
            "{\"model\":\"llama3.2\",\"message\":{\"role\":\"assistant\",\"content\":\" there\"},\"done\":false}\n",
            "{\"model\":\"llama3.2\",\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\",\"eval_count\":4}\n",
        );
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/x-ndjson")
                    .set_body_raw(ndjson.as_bytes().to_vec(), "application/x-ndjson"),
            )
            .mount(&server)
            .await;

        let provider = OllamaProvider::new().with_base_url(server.uri());
        let req = StreamRequest {
            model: "ollama/llama3.2".into(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 0,
            thinking_budget: None,
        };
        let raw = provider.stream(req).await.expect("stream");
        let result = collect_turn(assemble(raw)).await.expect("collect");
        assert_eq!(result.text, "Hi there");
        assert_eq!(result.tool_uses.len(), 0);
        assert_eq!(result.stop_reason.as_deref(), Some("stop"));
        assert_eq!(result.usage.as_ref().unwrap().output_tokens, 4);
    }

    #[tokio::test]
    async fn stream_end_to_end_tool_use_via_wiremock() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let ndjson = concat!(
            "{\"model\":\"llama3.2\",\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"Read\",\"arguments\":{\"path\":\"/tmp/x\"}}}]},\"done\":false}\n",
            "{\"model\":\"llama3.2\",\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\"}\n",
        );
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/x-ndjson")
                    .set_body_raw(ndjson.as_bytes().to_vec(), "application/x-ndjson"),
            )
            .mount(&server)
            .await;

        let provider = OllamaProvider::new().with_base_url(server.uri());
        let req = StreamRequest {
            model: "ollama/llama3.2".into(),
            system: None,
            messages: vec![Message::user("read it")],
            tools: vec![],
            max_tokens: 0,
            thinking_budget: None,
        };
        let raw = provider.stream(req).await.expect("stream");
        let result = collect_turn(assemble(raw)).await.expect("collect");

        assert_eq!(result.tool_uses.len(), 1);
        if let ContentBlock::ToolUse { name, input, .. } = &result.tool_uses[0] {
            assert_eq!(name, "Read");
            assert_eq!(input, &json!({"path": "/tmp/x"}));
        } else {
            panic!("expected ToolUse");
        }
    }
}
