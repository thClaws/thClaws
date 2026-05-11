//! Anthropic messages streaming provider.
//!
//! SSE event flow (text turn):
//!   message_start → content_block_start(text) → content_block_delta(text_delta)* →
//!   content_block_stop → message_delta(stop_reason, usage) → message_stop
//!
//! Tool-use turn adds: content_block_start(tool_use) →
//!   content_block_delta(input_json_delta)* → content_block_stop

use super::{EventStream, ModelInfo, Provider, ProviderEvent, StreamRequest, Usage};
use crate::error::{Error, Result};
use crate::types::Role;
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};

pub const DEFAULT_API_URL: &str = "https://api.anthropic.com/v1/messages";
pub const API_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    base_url: String,
    /// Optional override for the auth header name. `None` → `x-api-key`
    /// (the standard Anthropic header, also accepted by Azure AI Foundry).
    api_key_header: Option<String>,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            base_url: DEFAULT_API_URL.to_string(),
            api_key_header: None,
        }
    }

    /// Override the endpoint (for tests using a mock server).
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_api_key_header(mut self, name: impl Into<String>) -> Self {
        self.api_key_header = Some(name.into());
        self
    }

    fn auth_header_name(&self) -> &str {
        self.api_key_header.as_deref().unwrap_or("x-api-key")
    }

    fn build_body(req: &StreamRequest) -> Value {
        Self::build_body_inner(req, true)
    }

    /// M6.22 BUG G5: cache-disabled body builder for the defensive
    /// retry path. When the server rejects a request with a 400
    /// mentioning `cache_control` (e.g. an Anthropic API version
    /// regression, a model that disallows cache_control on tool_result,
    /// a future API breaking change), `stream` re-issues the request
    /// without ANY cache_control markers. The conversation works
    /// (without prompt caching) instead of the user being unable to
    /// chat at all.
    fn build_body_no_cache(req: &StreamRequest) -> Value {
        Self::build_body_inner(req, false)
    }

    /// Common body construction. `with_cache` controls whether the 3
    /// cache_control breakpoints (system + last tool + second-to-last
    /// message) are included.
    ///
    /// Anthropic supports `cache_control` on every block type per
    /// current API docs (text, image, tool_use, tool_result, thinking,
    /// document) — verified by `tests::cache_control_lands_on_*`. The
    /// `with_cache=false` path exists as a safety net, NOT because
    /// the marker placement is currently wrong.
    fn build_body_inner(req: &StreamRequest, with_cache: bool) -> Value {
        let mut msgs: Vec<Value> = req
            .messages
            .iter()
            .filter(|m| !matches!(m.role, Role::System))
            .map(|m| {
                json!({
                    "role": match m.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::System => unreachable!(),
                    },
                    "content": m.content,
                })
            })
            .collect();

        // Third cache breakpoint: tag the last content block of the
        // second-to-last message so the rolling conversation history
        // becomes a cached prefix on subsequent turns. The newest
        // message is the live user turn (uncached by definition); the
        // one before it is byte-stable across the next call. Skip
        // when the history is too short — Anthropic's minimum
        // cacheable prefix is 1024 tokens, so 1–2 messages rarely
        // qualify and the breakpoint slot is better preserved for
        // later turns. Anthropic allows up to 4 cache_control markers
        // per request; this is the third (system + last tool are the
        // other two).
        if with_cache && msgs.len() >= 3 {
            let target_idx = msgs.len() - 2;
            if let Some(content) = msgs[target_idx]
                .get_mut("content")
                .and_then(Value::as_array_mut)
            {
                if let Some(last_block) = content.last_mut() {
                    if let Some(obj) = last_block.as_object_mut() {
                        obj.insert("cache_control".into(), json!({"type": "ephemeral"}));
                    }
                }
            }
        }

        // Strip provider prefixes so the model name matches what the backend expects.
        let model = req
            .model
            .strip_prefix("oa/")
            .or_else(|| req.model.strip_prefix("azure/"))
            .unwrap_or(&req.model);

        let mut body = json!({
            "model": model,
            "max_tokens": req.max_tokens,
            "messages": msgs,
            "stream": true,
        });

        if let Some(sys) = &req.system {
            if !sys.is_empty() {
                if with_cache {
                    // Wrap system in a content block with cache_control for prompt caching.
                    body["system"] = json!([{
                        "type": "text",
                        "text": sys,
                        "cache_control": {"type": "ephemeral"}
                    }]);
                } else {
                    body["system"] = json!([{
                        "type": "text",
                        "text": sys,
                    }]);
                }
            }
        }

        if let Some(budget) = req.thinking_budget {
            if budget > 0 {
                body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
            }
        }

        if !req.tools.is_empty() {
            let mut tools_json = json!(req.tools);
            if with_cache {
                // Add cache_control to the last tool definition so Anthropic
                // caches the entire tool schema block (doesn't change per turn).
                if let Some(arr) = tools_json.as_array_mut() {
                    if let Some(last) = arr.last_mut() {
                        last["cache_control"] = json!({"type": "ephemeral"});
                    }
                }
            }
            body["tools"] = tools_json;
        }

        body
    }

    /// POST `body` to `base_url` with the standard Anthropic headers.
    /// Used by `stream` for both the cache-enabled first attempt and
    /// the cache-disabled retry. Returns the raw `reqwest::Response`
    /// — caller is responsible for status checking and body draining.
    async fn send_request(&self, body: &Value) -> Result<reqwest::Response> {
        self.client
            .post(&self.base_url)
            .header(self.auth_header_name(), &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("http: {e}")))
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        // /v1/messages → strip suffix → /v1/models. For the default URL this
        // yields https://api.anthropic.com/v1/models.
        let models_url = self
            .base_url
            .rsplit_once("/messages")
            .map(|(base, _)| format!("{base}/models"))
            .unwrap_or_else(|| format!("{}/models", self.base_url.trim_end_matches('/')));

        let resp = self
            .client
            .get(&models_url)
            .header(self.auth_header_name(), &self.api_key)
            .header("anthropic-version", API_VERSION)
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
                        let display_name = m
                            .get("display_name")
                            .and_then(Value::as_str)
                            .map(String::from);
                        Some(ModelInfo { id, display_name })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    async fn stream(&self, req: StreamRequest) -> Result<EventStream> {
        // M6.22 BUG G5: defensive cache-disabled retry. Send the
        // request with `cache_control` markers first; if the server
        // rejects with a 400 mentioning `cache_control`, re-issue
        // ONCE without them. This protects against future Anthropic
        // API regressions, model-specific cache_control restrictions,
        // and historically-inconsistent behavior on tool_result blocks
        // — without breaking caching for the common case where the
        // server accepts the markers.
        let body = Self::build_body(&req);
        let resp = self.send_request(&body).await?;

        let resp = if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            // Detect cache_control rejections specifically. The
            // Anthropic error body contains the field name in its
            // `error.message` when the rejection is cache-related.
            // Other 400s (content_too_long, invalid model, etc.) get
            // surfaced unchanged.
            if status.as_u16() == 400 && text.contains("cache_control") {
                eprintln!(
                    "\x1b[33m[anthropic] cache_control rejected by server; retrying without cache markers\n  body: {}\x1b[0m",
                    super::redact_key(&text, &self.api_key)
                        .chars()
                        .take(300)
                        .collect::<String>()
                );
                let body_no_cache = Self::build_body_no_cache(&req);
                let retry = self.send_request(&body_no_cache).await?;
                if !retry.status().is_success() {
                    let status = retry.status();
                    let text = retry.text().await.unwrap_or_default();
                    return Err(Error::Provider(format!(
                        "http {status} (retry without cache also failed): {}",
                        super::redact_key(&text, &self.api_key)
                    )));
                }
                retry
            } else {
                return Err(Error::Provider(format!(
                    "http {status}: {}",
                    super::redact_key(&text, &self.api_key)
                )));
            }
        } else {
            resp
        };

        let byte_stream = resp.bytes_stream();
        let raw_dump = super::RawDump::new(format!("anthropic {}", req.model));

        let event_stream = try_stream! {
            // M6.21 BUG H1: buffer raw bytes (NOT String::from_utf8_lossy
            // per chunk) so multi-byte UTF-8 chars split across TCP packet
            // boundaries don't get corrupted into U+FFFD pairs.
            let mut buffer: Vec<u8> = Vec::new();
            let mut byte_stream = Box::pin(byte_stream);
            let mut raw = raw_dump;
            loop {
                let maybe_chunk = tokio::time::timeout(
                    super::STREAM_CHUNK_TIMEOUT,
                    byte_stream.next(),
                )
                .await
                .map_err(|_| Error::Provider(format!(
                    "stream idle for {}s — provider stopped sending; try again",
                    super::STREAM_CHUNK_TIMEOUT.as_secs()
                )))?;
                let Some(chunk) = maybe_chunk else { break };
                let chunk = chunk.map_err(|e| Error::Provider(format!("stream: {e}")))?;
                buffer.extend_from_slice(&chunk);

                while let Some(boundary) = super::find_bytes(&buffer, b"\n\n") {
                    let event_bytes: Vec<u8> = buffer.drain(..boundary + 2).collect();
                    // Complete events are valid UTF-8 by construction
                    // (well-formed JSON), so decoding here is safe.
                    let event_text = String::from_utf8_lossy(&event_bytes);
                    let trimmed = event_text.trim_end_matches('\n');
                    if let Some(ev) = parse_sse_event(trimmed)? {
                        if let ProviderEvent::TextDelta(ref s) = ev { raw.push(s); }
                        yield ev;
                    }
                }
            }
            raw.flush();
        };

        Ok(Box::pin(event_stream))
    }
}

/// Parse a single SSE event (one or more lines, already split on blank line).
/// Returns `Ok(None)` for events we deliberately ignore (ping, message_stop marker).
pub fn parse_sse_event(raw: &str) -> Result<Option<ProviderEvent>> {
    let mut data_line: Option<&str> = None;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("data: ") {
            data_line = Some(rest);
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_line = Some(rest);
        }
    }
    let Some(data) = data_line else {
        return Ok(None);
    };
    let v: Value = serde_json::from_str(data)?;
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");

    let event = match ty {
        "message_start" => {
            let model = v
                .pointer("/message/model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(ProviderEvent::MessageStart { model })
        }
        "content_block_start" => {
            let cb = v.get("content_block");
            let cb_type = cb
                .and_then(|c| c.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            match cb_type {
                "tool_use" => {
                    let id = cb
                        .and_then(|c| c.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let name = cb
                        .and_then(|c| c.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Some(ProviderEvent::ToolUseStart {
                        id,
                        name,
                        thought_signature: None,
                    })
                }
                _ => None,
            }
        }
        "content_block_delta" => {
            let delta = v.get("delta");
            let dt = delta
                .and_then(|d| d.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            match dt {
                "text_delta" => {
                    let text = delta
                        .and_then(|d| d.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Some(ProviderEvent::TextDelta(text))
                }
                "input_json_delta" => {
                    let pj = delta
                        .and_then(|d| d.get("partial_json"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Some(ProviderEvent::ToolUseDelta { partial_json: pj })
                }
                _ => None,
            }
        }
        "content_block_stop" => Some(ProviderEvent::ContentBlockStop),
        "message_delta" => {
            let stop_reason = v
                .pointer("/delta/stop_reason")
                .and_then(Value::as_str)
                .map(String::from);
            let usage = v.get("usage").map(|u| Usage {
                input_tokens: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
                output_tokens: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
                cache_creation_input_tokens: u
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32),
                cache_read_input_tokens: u
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32),
            });
            Some(ProviderEvent::MessageStop { stop_reason, usage })
        }
        "message_stop" | "ping" => None,
        _ => None,
    };

    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, Message};

    #[test]
    fn parse_message_start() {
        let raw = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-5\"}}";
        let ev = parse_sse_event(raw).unwrap().unwrap();
        assert_eq!(
            ev,
            ProviderEvent::MessageStart {
                model: "claude-sonnet-4-5".into()
            }
        );
    }

    #[test]
    fn parse_text_delta() {
        let raw = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}";
        let ev = parse_sse_event(raw).unwrap().unwrap();
        assert_eq!(ev, ProviderEvent::TextDelta("Hello".into()));
    }

    #[test]
    fn parse_tool_use_start() {
        let raw = "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_abc\",\"name\":\"read_file\"}}";
        let ev = parse_sse_event(raw).unwrap().unwrap();
        assert_eq!(
            ev,
            ProviderEvent::ToolUseStart {
                id: "toolu_abc".into(),
                name: "read_file".into(),
                thought_signature: None,
            }
        );
    }

    #[test]
    fn parse_input_json_delta() {
        let raw = "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}";
        let ev = parse_sse_event(raw).unwrap().unwrap();
        assert_eq!(
            ev,
            ProviderEvent::ToolUseDelta {
                partial_json: "{\"path\":".into()
            }
        );
    }

    #[test]
    fn parse_content_block_stop() {
        let raw = "data: {\"type\":\"content_block_stop\",\"index\":0}";
        let ev = parse_sse_event(raw).unwrap().unwrap();
        assert_eq!(ev, ProviderEvent::ContentBlockStop);
    }

    #[test]
    fn parse_message_delta_with_usage() {
        let raw = "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":12,\"output_tokens\":34}}";
        let ev = parse_sse_event(raw).unwrap().unwrap();
        assert_eq!(
            ev,
            ProviderEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: Some(Usage {
                    input_tokens: 12,
                    output_tokens: 34,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                }),
            }
        );
    }

    #[test]
    fn parse_ignores_ping_and_message_stop_marker() {
        assert!(parse_sse_event("data: {\"type\":\"ping\"}")
            .unwrap()
            .is_none());
        assert!(parse_sse_event("data: {\"type\":\"message_stop\"}")
            .unwrap()
            .is_none());
    }

    #[test]
    fn parse_ignores_event_with_no_data_line() {
        assert!(parse_sse_event("event: ping").unwrap().is_none());
    }

    #[test]
    fn build_body_puts_system_at_top_level_and_excludes_from_messages() {
        let req = StreamRequest {
            model: "claude-sonnet-4-5".into(),
            system: Some("you are helpful".into()),
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 1024,
            thinking_budget: None,
        };
        let body = AnthropicProvider::build_body(&req);
        // System is now wrapped in a content block with cache_control.
        assert_eq!(body["system"][0]["text"], "you are helpful");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["stream"], true);
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["max_tokens"], 1024);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn build_body_caches_second_to_last_message_when_history_long_enough() {
        // Three-message rolling history: user/assistant/user. The
        // assistant turn (index 1) is second-to-last and should carry
        // the cache_control marker so its tail becomes a cached prefix
        // on the next call. The newest user turn (index 2) must stay
        // uncached.
        let req = StreamRequest {
            model: "claude-sonnet-4-5".into(),
            system: Some("you are helpful".into()),
            messages: vec![
                Message::user("first"),
                Message::assistant("reply"),
                Message::user("second"),
            ],
            tools: vec![],
            max_tokens: 1024,
            thinking_budget: None,
        };
        let body = AnthropicProvider::build_body(&req);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        let second_to_last_last_block = msgs[1]["content"].as_array().unwrap().last().unwrap();
        assert_eq!(
            second_to_last_last_block["cache_control"]["type"],
            "ephemeral"
        );
        for block in msgs[2]["content"].as_array().unwrap() {
            assert!(
                block.get("cache_control").is_none(),
                "newest message must not carry cache_control"
            );
        }
    }

    #[test]
    fn build_body_skips_message_cache_when_history_too_short() {
        // Two-message history: the breakpoint slot is preserved for
        // later turns. System + tools breakpoints still apply.
        let req = StreamRequest {
            model: "claude-sonnet-4-5".into(),
            system: Some("sys".into()),
            messages: vec![Message::user("first"), Message::assistant("reply")],
            tools: vec![],
            max_tokens: 1024,
            thinking_budget: None,
        };
        let body = AnthropicProvider::build_body(&req);
        for msg in body["messages"].as_array().unwrap() {
            for block in msg["content"].as_array().unwrap() {
                assert!(
                    block.get("cache_control").is_none(),
                    "no message cache_control when history < 3"
                );
            }
        }
    }

    #[test]
    fn build_body_message_cache_is_byte_stable_across_calls() {
        // Same input twice → identical body. Guards against silent
        // cache busts from non-deterministic field ordering.
        let req = StreamRequest {
            model: "claude-sonnet-4-5".into(),
            system: Some("sys".into()),
            messages: vec![
                Message::user("a"),
                Message::assistant("b"),
                Message::user("c"),
            ],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let a = AnthropicProvider::build_body(&req);
        let b = AnthropicProvider::build_body(&req);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    #[test]
    fn build_body_omits_empty_system_and_tools() {
        let req = StreamRequest {
            model: "claude-sonnet-4-5".into(),
            system: None,
            messages: vec![Message::user("x")],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let body = AnthropicProvider::build_body(&req);
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());
    }

    #[tokio::test]
    async fn stream_end_to_end_with_mock_server() {
        use futures::StreamExt;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let sse_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-5\"}}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", API_VERSION))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new("test-key")
            .with_base_url(format!("{}/v1/messages", server.uri()));

        let req = StreamRequest {
            model: "claude-sonnet-4-5".into(),
            system: Some("sys".into()),
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };

        let stream = provider.stream(req).await.expect("stream");
        let collected: Vec<Result<ProviderEvent>> = stream.collect().await;
        let events: Vec<ProviderEvent> = collected
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .expect("all events ok");

        assert!(
            matches!(&events[0], ProviderEvent::MessageStart { model } if model == "claude-sonnet-4-5"),
            "first event: {:?}",
            events[0]
        );
        assert_eq!(events[1], ProviderEvent::TextDelta("Hello".into()));
        assert_eq!(events[2], ProviderEvent::TextDelta(" world".into()));
        assert_eq!(events[3], ProviderEvent::ContentBlockStop);
        match &events[4] {
            ProviderEvent::MessageStop { stop_reason, usage } => {
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                let u = usage.as_ref().expect("usage");
                assert_eq!(u.input_tokens, 5);
                assert_eq!(u.output_tokens, 2);
            }
            e => panic!("expected MessageStop, got {:?}", e),
        }
        assert_eq!(events.len(), 5, "unexpected events: {:?}", events);
    }

    #[tokio::test]
    async fn stream_with_tool_use_assembles_to_turn_result() {
        use crate::providers::{assemble, collect_turn};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // Text block "I'll read " + "that file." then a tool_use "read_file"
        // whose input arrives in 3 partial_json chunks that must combine
        // to {"path":"/tmp/x"}.
        let sse_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-5\"}}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"I'll read \"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"that file.\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01A\",\"name\":\"read_file\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"pa\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"th\\\":\\\"\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"/tmp/x\\\"}\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":20,\"output_tokens\":15}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new("test-key")
            .with_base_url(format!("{}/v1/messages", server.uri()));
        let req = StreamRequest {
            model: "claude-sonnet-4-5".into(),
            system: None,
            messages: vec![Message::user("read /tmp/x")],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };

        let raw = provider.stream(req).await.expect("stream");
        let result = collect_turn(assemble(raw)).await.expect("collect");

        assert_eq!(result.text, "I'll read that file.");
        assert_eq!(result.tool_uses.len(), 1);
        if let ContentBlock::ToolUse {
            id, name, input, ..
        } = &result.tool_uses[0]
        {
            assert_eq!(id, "toolu_01A");
            assert_eq!(name, "read_file");
            assert_eq!(input, &serde_json::json!({"path": "/tmp/x"}));
        } else {
            panic!("expected ToolUse");
        }
        assert_eq!(result.stop_reason.as_deref(), Some("tool_use"));
        let u = result.usage.expect("usage");
        assert_eq!(u.input_tokens, 20);
        assert_eq!(u.output_tokens, 15);
    }

    #[tokio::test]
    async fn list_models_parses_data_array() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = r#"{"data":[
            {"id":"claude-opus-4-5","display_name":"Claude Opus 4.5","type":"model"},
            {"id":"claude-sonnet-4-5","display_name":"Claude Sonnet 4.5","type":"model"}
        ]}"#;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new("test-key")
            .with_base_url(format!("{}/v1/messages", server.uri()));
        let models = provider.list_models().await.expect("list");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "claude-opus-4-5");
        assert_eq!(models[0].display_name.as_deref(), Some("Claude Opus 4.5"));
    }

    #[tokio::test]
    async fn stream_surfaces_http_errors() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new("bad-key")
            .with_base_url(format!("{}/v1/messages", server.uri()));
        let req = StreamRequest {
            model: "claude-sonnet-4-5".into(),
            system: None,
            messages: vec![Message::user("x")],
            tools: vec![],
            max_tokens: 10,
            thinking_budget: None,
        };
        match provider.stream(req).await {
            Err(e) => {
                let s = format!("{e}");
                assert!(s.contains("401"), "expected 401 in error, got: {s}");
            }
            Ok(_) => panic!("expected error for 401 response"),
        }
    }

    #[test]
    fn build_body_preserves_tool_result_blocks() {
        let req = StreamRequest {
            model: "claude-sonnet-4-5".into(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
            }],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let body = AnthropicProvider::build_body(&req);
        let first = &body["messages"][0]["content"][0];
        assert_eq!(first["type"], "tool_result");
        assert_eq!(first["tool_use_id"], "toolu_1");
    }

    /// M6.22 BUG G5: pin the cache_control placement on each block
    /// type Anthropic might encounter at the second-to-last position.
    /// Anthropic supports cache_control on all of these per current
    /// API docs; if the API behavior ever regresses on a specific
    /// block type, the defensive retry-without-cache in `stream` is
    /// the safety net (see `cache_control_400_triggers_retry_without_cache`).
    fn three_msg_history_with_last_block(last_block: ContentBlock) -> StreamRequest {
        StreamRequest {
            model: "claude-sonnet-4-5".into(),
            system: Some("sys".into()),
            messages: vec![
                Message::user("first"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Text {
                            text: "filler".into(),
                        },
                        last_block,
                    ],
                },
                Message::user("third"),
            ],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        }
    }

    #[test]
    fn cache_control_lands_on_text_block() {
        let req = three_msg_history_with_last_block(ContentBlock::Text {
            text: "assistant reply".into(),
        });
        let body = AnthropicProvider::build_body(&req);
        let last = body["messages"][1]["content"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert_eq!(last["type"], "text");
        assert_eq!(last["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_control_lands_on_tool_use_block() {
        let req = three_msg_history_with_last_block(ContentBlock::ToolUse {
            id: "toolu_x".into(),
            name: "Read".into(),
            input: serde_json::json!({"path": "/tmp"}),
            thought_signature: None,
        });
        let body = AnthropicProvider::build_body(&req);
        let last = body["messages"][1]["content"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert_eq!(last["type"], "tool_use");
        assert_eq!(last["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_control_lands_on_tool_result_block() {
        // User message ending in a tool_result is the trigger case for
        // the historical G5 concern. Verify cache_control IS placed
        // (per current Anthropic API support); the defensive retry
        // catches it at runtime if the API ever regresses.
        let req = StreamRequest {
            model: "claude-sonnet-4-5".into(),
            system: Some("sys".into()),
            messages: vec![
                Message::user("first"),
                Message::assistant("reply"),
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "toolu_y".into(),
                        content: "tool output".into(),
                        is_error: false,
                    }],
                },
                Message::user("fourth"),
            ],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let body = AnthropicProvider::build_body(&req);
        // Second-to-last is index 2 (the ToolResult message)
        let last = body["messages"][2]["content"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert_eq!(last["type"], "tool_result");
        assert_eq!(last["cache_control"]["type"], "ephemeral");
    }

    /// M6.22 BUG G5: cache-disabled body must strip ALL cache_control
    /// markers — system, tools, AND second-to-last message. Used by
    /// the defensive retry path in `stream`.
    #[test]
    fn build_body_no_cache_strips_all_cache_control_markers() {
        let req = StreamRequest {
            model: "claude-sonnet-4-5".into(),
            system: Some("sys with cache".into()),
            messages: vec![
                Message::user("first"),
                Message::assistant("reply"),
                Message::user("third"),
            ],
            tools: vec![crate::types::ToolDef {
                name: "Read".into(),
                description: "read a file".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            max_tokens: 100,
            thinking_budget: None,
        };

        let cached = AnthropicProvider::build_body(&req);
        let bare = AnthropicProvider::build_body_no_cache(&req);

        // Cached version has the markers.
        assert!(cached["system"][0].get("cache_control").is_some());
        assert!(cached["tools"][0].get("cache_control").is_some());
        let last_block = cached["messages"][1]["content"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert!(last_block.get("cache_control").is_some());

        // Bare version has NONE.
        assert!(bare["system"][0].get("cache_control").is_none());
        assert!(bare["tools"][0].get("cache_control").is_none());
        for msg in bare["messages"].as_array().unwrap() {
            for block in msg["content"].as_array().unwrap() {
                assert!(
                    block.get("cache_control").is_none(),
                    "bare body must have no cache_control on any message block, got: {block:?}"
                );
            }
        }
        // Same model, max_tokens, messages structure aside from cache markers.
        assert_eq!(cached["model"], bare["model"]);
        assert_eq!(cached["max_tokens"], bare["max_tokens"]);
        assert_eq!(
            cached["messages"].as_array().unwrap().len(),
            bare["messages"].as_array().unwrap().len()
        );
    }

    /// M6.22 BUG G5: when the server returns 400 with a body
    /// mentioning `cache_control`, the provider must retry ONCE
    /// without cache markers and surface the second response. Verifies
    /// the safety net that protects users if Anthropic regresses on
    /// cache_control acceptance.
    #[tokio::test]
    async fn cache_control_400_triggers_retry_without_cache() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // First call: body contains "cache_control" → 400
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_string_contains("cache_control"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"cache_control is not supported on this resource"}}"#,
            ))
            .mount(&server)
            .await;

        // Retry: body without "cache_control" → 200 with valid SSE
        let sse_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-5\"}}\n",
            "\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n",
            "\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}\n",
            "\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
            "\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new("test-key")
            .with_base_url(format!("{}/v1/messages", server.uri()));

        // Need ≥3 messages to actually emit cache_control on second-to-last
        let req = StreamRequest {
            model: "claude-sonnet-4-5".into(),
            system: Some("sys".into()),
            messages: vec![
                Message::user("first"),
                Message::assistant("reply"),
                Message::user("third"),
            ],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };

        let stream = provider
            .stream(req)
            .await
            .expect("stream succeeds via retry");
        use futures::StreamExt;
        let collected: Vec<Result<ProviderEvent>> = stream.collect().await;
        let events: Vec<ProviderEvent> = collected
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .expect("retry events parse");
        // Verify the retry succeeded by checking we got the expected text delta.
        let got_text = events
            .iter()
            .any(|e| matches!(e, ProviderEvent::TextDelta(s) if s == "ok"));
        assert!(got_text, "retry response not received: {events:?}");
    }

    /// M6.22 BUG G5: a 400 NOT mentioning cache_control must surface
    /// directly without triggering the retry. Ensures the safety net
    /// only fires for its intended trigger case.
    #[tokio::test]
    async fn non_cache_400_does_not_trigger_retry() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // Single mock: 400 with content_too_long error (no cache_control mention)
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 limit"}}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new("test-key")
            .with_base_url(format!("{}/v1/messages", server.uri()));
        let req = StreamRequest {
            model: "claude-sonnet-4-5".into(),
            system: Some("sys".into()),
            messages: vec![
                Message::user("first"),
                Message::assistant("reply"),
                Message::user("third"),
            ],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };

        match provider.stream(req).await {
            Err(e) => {
                let s = format!("{e}");
                assert!(s.contains("400"), "expected 400 in error, got: {s}");
                assert!(
                    s.contains("prompt is too long") || s.contains("invalid_request_error"),
                    "expected original error body, got: {s}"
                );
            }
            Ok(_) => panic!("expected error, not retry-success"),
        }
        // wiremock's .expect(1) verifies via Drop that exactly one
        // request was made — proving no retry was issued.
    }
}
