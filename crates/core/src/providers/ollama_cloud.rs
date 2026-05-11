//! Ollama Cloud provider — `/api/chat` via `https://ollama.com` with API key auth.
//!
//! Wire format notes (vs local Ollama):
//! - Endpoint: `https://ollama.com/api/chat` (fixed, not configurable)
//! - Auth: Bearer token via `Authorization: Bearer <OLLAMA_CLOUD_API_KEY>` env var (REQUIRED)
//! - Stream is **NDJSON**, identical to local Ollama: one complete JSON object per line
//! - Messages shape identical to local Ollama provider
//! - **Thinking/reasoning**: Cloud models emit `thinking` in stream responses
//! - **Images**: Supported via `images: [base64...]` field in message objects
//!
//! Model string handling: we strip an `ollama-cloud/` prefix before sending, so a
//! config like `model = "ollama-cloud/gpt-oss-120b-cloud"` hits the cloud model.

use super::{EventStream, ModelInfo, Provider, ProviderEvent, StreamRequest};
use crate::error::{Error, Result};
use crate::types::{ContentBlock, ImageSource, Role};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};

pub struct OllamaCloudProvider {
    client: Client,
    api_key: String,
}

impl OllamaCloudProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
        }
    }

    fn model_name(req_model: &str) -> &str {
        req_model.strip_prefix("ollama-cloud/").unwrap_or(req_model)
    }

    fn messages_to_ollama_cloud(req: &StreamRequest) -> Vec<Value> {
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
            let mut thinking_parts: Vec<String> = Vec::new();
            let mut images: Vec<String> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            let mut trailing_tool_results: Vec<String> = Vec::new();

            for block in &m.content {
                match block {
                    ContentBlock::Text { text } => text_parts.push(text.clone()),
                    // Ollama Cloud supports thinking/reasoning models —
                    // round-trip as a sibling `thinking` field on the
                    // assistant message so the model sees its prior
                    // thinking and the server doesn't 400.
                    ContentBlock::Thinking { content, .. } => {
                        thinking_parts.push(content.clone());
                    }
                    // Ollama Cloud supports vision models via base64 images
                    ContentBlock::Image { source } => {
                        let ImageSource::Base64 { data, .. } = source;
                        // Strip data:image/...;base64, prefix if present
                        let clean = data.split(',').next_back().unwrap_or(data).to_string();
                        images.push(clean);
                    }
                    ContentBlock::ToolUse { name, input, .. } => {
                        tool_calls.push(json!({
                            "function": { "name": name, "arguments": input },
                        }));
                    }
                    ContentBlock::ToolResult { content, .. } => {
                        trailing_tool_results.push(content.to_text());
                    }
                }
            }

            let content = text_parts.join("");
            let thinking_text = thinking_parts.join("");
            let has_text = !content.is_empty();
            let has_thinking = !thinking_text.is_empty();
            let has_tools = !tool_calls.is_empty();
            let has_images = !images.is_empty();

            // Build message with optional images array
            let mut msg = json!({"role": role});
            if has_images {
                // Vision models: include both text content and images array
                msg["content"] = json!(content);
                msg["images"] = json!(images);
            } else if has_text || has_tools {
                msg["content"] = json!(content);
            }
            if has_thinking {
                msg["thinking"] = json!(thinking_text);
            }
            if has_tools {
                msg["tool_calls"] = json!(tool_calls);
            }
            if has_text || has_tools || has_images || has_thinking {
                out.push(msg);
            }

            for content in trailing_tool_results {
                out.push(json!({"role": "tool", "content": content}));
            }
        }

        out
    }

    /// Returns the appropriate `think` value for the model.
    /// - GPT-OSS family requires `"low" | "medium" | "high"`.
    /// - All other thinking models accept a boolean.
    fn think_value(model: &str) -> Value {
        if model.starts_with("gpt-oss") {
            json!("high")
        } else {
            json!(true)
        }
    }

    fn build_body(req: &StreamRequest) -> Value {
        let model = Self::model_name(&req.model);
        let messages = Self::messages_to_ollama_cloud(req);
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "think": Self::think_value(model)
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

impl Default for OllamaCloudProvider {
    fn default() -> Self {
        Self::new(String::new())
    }
}

#[async_trait]
impl Provider for OllamaCloudProvider {
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let url = "https://ollama.com/v1/models";
        let resp = self
            .client
            .get(url)
            .header("Authorization", format!("Bearer {}", self.api_key))
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
                        let id = m.get("id").and_then(Value::as_str)?;
                        // Add prefix so users can paste straight into /model
                        // and so it matches the config format. stream() strips
                        // this prefix before hitting the remote.
                        Some(ModelInfo {
                            id: format!("ollama-cloud/{id}"),
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
        let resp = self
            .client
            .post("https://ollama.com/api/chat")
            .header("content-type", "application/json")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("http: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let redacted = super::redact_key(&text, &self.api_key);
            return Err(Error::Provider(format!("http {status}: {redacted}")));
        }

        let byte_stream = resp.bytes_stream();
        let raw_dump = super::RawDump::new(format!("ollama-cloud {}", req.model));

        let event_stream = try_stream! {
            // M6.21 BUG H1: byte buffer to avoid UTF-8 corruption at
            // chunk boundaries.
            let mut buffer: Vec<u8> = Vec::new();
            let mut byte_stream = Box::pin(byte_stream);
            let mut state = super::ollama::ParseState::default();
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

                while let Some(newline) = super::find_bytes(&buffer, b"\n") {
                    let line_bytes: Vec<u8> = buffer.drain(..newline + 1).collect();
                    let line_text = String::from_utf8_lossy(&line_bytes);
                    let line = line_text.trim();
                    if line.is_empty() {
                        continue;
                    }
                    for event in super::ollama::parse_line(line, &mut state)? {
                        if let ProviderEvent::TextDelta(ref s) = event { raw.push(s); }
                        yield event;
                    }
                }
            }

            if !buffer.is_empty() {
                let line_text = String::from_utf8_lossy(&buffer);
                let trimmed = line_text.trim();
                if !trimmed.is_empty() {
                    for event in super::ollama::parse_line(trimmed, &mut state)? {
                        if let ProviderEvent::TextDelta(ref s) = event { raw.push(s); }
                        yield event;
                    }
                }
            }
            raw.flush();
        };

        Ok(Box::pin(event_stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{assemble, collect_turn};
    use crate::types::Message;

    #[test]
    fn model_name_strips_prefix() {
        assert_eq!(
            OllamaCloudProvider::model_name("ollama-cloud/gpt-oss-120b-cloud"),
            "gpt-oss-120b-cloud"
        );
        assert_eq!(
            OllamaCloudProvider::model_name("gpt-oss-120b-cloud"),
            "gpt-oss-120b-cloud"
        );
    }

    #[test]
    fn build_body_strips_prefix_and_sets_stream_true() {
        let req = StreamRequest {
            model: "ollama-cloud/gpt-oss-120b-cloud".into(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 0,
            thinking_budget: None,
        };
        let body = OllamaCloudProvider::build_body(&req);
        assert_eq!(body["model"], "gpt-oss-120b-cloud");
        assert_eq!(body["stream"], true);
        assert_eq!(body["think"], "high"); // GPT-OSS requires string value
    }

    #[test]
    fn build_body_sets_think_true_for_non_gpt_oss() {
        let req = StreamRequest {
            model: "ollama-cloud/deepseek-v4-flash".into(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 0,
            thinking_budget: None,
        };
        let body = OllamaCloudProvider::build_body(&req);
        assert_eq!(body["model"], "deepseek-v4-flash");
        assert_eq!(body["think"], true); // boolean for non-GPT-OSS
    }

    #[test]
    fn messages_with_image_attachment() {
        use crate::types::ImageSource;
        let req = StreamRequest {
            model: "ollama-cloud/deepseek-v4-flash".into(),
            system: None,
            messages: vec![crate::types::Message {
                role: Role::User,
                content: vec![
                    ContentBlock::text("What's in this image?"),
                    ContentBlock::Image {
                        source: ImageSource::Base64 {
                            media_type: "image/jpeg".to_string(),
                            data: "data:image/jpeg;base64,ABC123".to_string(),
                        },
                    },
                ],
            }],
            tools: vec![],
            max_tokens: 0,
            thinking_budget: None,
        };
        let msgs = OllamaCloudProvider::messages_to_ollama_cloud(&req);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "What's in this image?");
        assert!(msgs[0]["images"].is_array());
        assert_eq!(msgs[0]["images"][0], "ABC123");
    }

    #[test]
    fn messages_with_thinking_block() {
        let req = StreamRequest {
            model: "ollama-cloud/deepseek-v4-flash".into(),
            system: None,
            messages: vec![crate::types::Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::text("The answer is 42."),
                    ContentBlock::Thinking {
                        content: "Let me think about this...".to_string(),
                        signature: None,
                    },
                ],
            }],
            tools: vec![],
            max_tokens: 0,
            thinking_budget: None,
        };
        let msgs = OllamaCloudProvider::messages_to_ollama_cloud(&req);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"].as_str().unwrap(), "The answer is 42.");
        assert_eq!(
            msgs[0]["thinking"].as_str().unwrap(),
            "Let me think about this..."
        );
    }

    #[test]
    fn messages_with_only_thinking_block() {
        let req = StreamRequest {
            model: "ollama-cloud/deepseek-v4-flash".into(),
            system: None,
            messages: vec![crate::types::Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Thinking {
                    content: "planning...".to_string(),
                    signature: None,
                }],
            }],
            tools: vec![],
            max_tokens: 0,
            thinking_budget: None,
        };
        let msgs = OllamaCloudProvider::messages_to_ollama_cloud(&req);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["thinking"].as_str().unwrap(), "planning...");
        assert!(msgs[0].get("content").is_none());
    }

    #[test]
    fn stream_emits_thinking_delta_for_thinking() {
        use super::super::ollama::ParseState;
        let line = r#"{"model":"deepseek-v4-flash","message":{"role":"assistant","content":"","thinking":"step 1"},"done":false}"#;
        let mut state = ParseState::default();
        let events = super::super::ollama::parse_line(line, &mut state).unwrap();
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                super::super::ProviderEvent::MessageStart { .. } => "MessageStart",
                super::super::ProviderEvent::ThinkingDelta(_) => "ThinkingDelta",
                super::super::ProviderEvent::TextDelta(_) => "TextDelta",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["MessageStart", "ThinkingDelta"]);
    }
}
