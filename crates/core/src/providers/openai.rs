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

use super::{EventStream, ModelInfo, Provider, ProviderEvent, ProviderKind, StreamRequest, Usage};
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
    /// remote. Used by aggregator-style providers (e.g. `zai/glm-5.2` →
    /// `glm-5.2`) where the prefix exists only to route `detect()` on
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
    /// When set, the request's model is replaced with this id before the
    /// wire call (after which `strip_model_prefix` still applies). Used by
    /// the `openrouter/fusion+` pseudo-model to call a configured outer
    /// model while the user-facing model id stays `openrouter/fusion+`.
    model_override: Option<String>,
    /// Extra tool objects appended verbatim to the request `tools` array
    /// (e.g. `{"type":"openrouter:fusion","parameters":{…}}`). Carried
    /// alongside the agent's normal function tools.
    injected_tools: Vec<Value>,
    /// Optional `tool_choice` body value (e.g. `"required"`). `None` omits
    /// it, leaving the provider's default (auto).
    tool_choice: Option<Value>,
    /// Attach `x-thclaws-session` / `x-thclaws-turn` (audit RFC 0001).
    /// Set only on the org-gateway provider built by `build_provider`.
    gateway_correlation: bool,
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
            model_override: None,
            injected_tools: Vec::new(),
            tool_choice: None,
            gateway_correlation: false,
        }
    }

    /// Send audit correlation headers with every request (org gateway
    /// only; the audit layer decides whether they are actually on).
    pub fn with_gateway_correlation(mut self) -> Self {
        self.gateway_correlation = true;
        self
    }

    /// Replace the request model with `model` before the wire call (the
    /// `strip_model_prefix`, if any, still runs afterward). See
    /// [`Self::model_override`].
    pub fn with_model_override(mut self, model: impl Into<String>) -> Self {
        self.model_override = Some(model.into());
        self
    }

    /// Append a raw tool object to every request's `tools` array.
    pub fn with_injected_tool(mut self, tool: Value) -> Self {
        self.injected_tools.push(tool);
        self
    }

    /// Set the `tool_choice` body value (e.g. `json!("required")`).
    pub fn with_tool_choice(mut self, choice: Value) -> Self {
        self.tool_choice = Some(choice);
        self
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
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        // Dedup by id. Some OpenAI-compat models (DeepSeek)
                        // occasionally emit two parallel tool_calls sharing
                        // one id; the strict endpoint then rejects the
                        // follow-up ("insufficient tool messages following
                        // tool_calls"). Keep the first, drop the collision.
                        if tool_calls
                            .iter()
                            .any(|tc| tc["id"].as_str() == Some(id.as_str()))
                        {
                            continue;
                        }
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
                        // loop below). Dedup by id to mirror the tool_call
                        // dedup above — a duplicated result id would
                        // re-introduce the count mismatch.
                        if trailing_tool_results
                            .iter()
                            .any(|(rid, _, _)| rid == tool_use_id)
                        {
                            continue;
                        }
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

            // Tool results FIRST. OpenAI's contract: an assistant message
            // with `tool_calls` must be immediately followed by tool-role
            // messages answering every tool_call_id, with no other role
            // interleaved. Emitting these before THIS message's own
            // text/image content guarantees that even a results-bearing
            // user message that also carries text (or an interleaved user
            // turn) can't wedge a `user` role between the assistant's
            // tool_calls and their results — which strict endpoints
            // (DeepSeek, …) 400 on ("insufficient tool messages following
            // tool_calls"). A results-only user message (the common case)
            // emits just these and no main message at all.
            for (tool_call_id, content, _images) in &trailing_tool_results {
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content,
                }));
            }

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
                } else {
                    // Reasoning-only turn (a thinking block, no text / tools /
                    // images). Without a `content` field some OpenAI-compatible
                    // providers (DeepSeek, etc.) reject the message with HTTP
                    // 400 — fall back to an empty string (issue #163 Bug 3).
                    msg["content"] = json!("");
                }
                if has_tools {
                    msg["tool_calls"] = json!(tool_calls);
                }
                if has_reasoning {
                    msg["reasoning_content"] = json!(reasoning_text);
                }
                out.push(msg);
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

    fn build_body(&self, req: &StreamRequest) -> Value {
        let messages = Self::messages_to_openai(req);
        let mut body = json!({
            "model": req.model,
            "max_completion_tokens": req.max_tokens,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        let mut tools: Vec<Value> = req
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
        tools.extend(self.injected_tools.iter().cloned());
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if let Some(tc) = &self.tool_choice {
            body["tool_choice"] = tc.clone();
        }
        body
    }

    /// True when the endpoint is a local OpenAI-compatible server (vLLM, ds4,
    /// llama.cpp, …). These enforce `max_completion_tokens <= max_model_len`,
    /// so our default (32000) 400s against a small served context.
    fn is_local_base(&self) -> bool {
        let b = self.base_url.as_str();
        b.contains("localhost") || b.contains("127.0.0.1") || b.contains("0.0.0.0")
    }

    /// Best-effort fetch of the served model's `max_model_len` from `GET
    /// /models` so `stream` can clamp `max_tokens`. `None` when the server
    /// doesn't expose it (ds4/llama.cpp may omit it) — caller leaves the
    /// request unchanged in that case.
    async fn served_context(&self, model: &str) -> Option<u32> {
        let url = self.list_models_url.clone().unwrap_or_else(|| {
            self.base_url
                .rsplit_once("/chat/completions")
                .map(|(base, _)| format!("{base}/models"))
                .unwrap_or_else(|| format!("{}/models", self.base_url.trim_end_matches('/')))
        });
        let resp = self
            .client
            .get(&url)
            .header(self.auth_header_name(), self.auth_header_value())
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        let data = v.get("data")?.as_array()?;
        let m = data
            .iter()
            .find(|m| m.get("id").and_then(Value::as_str) == Some(model))
            .or_else(|| data.first())?;
        // vLLM exposes `max_model_len`; ds4/OpenRouter-shaped servers use
        // `context_length` (top-level or nested under `top_provider`).
        m.get("max_model_len")
            .or_else(|| m.get("context_length"))
            // LiteLLM's proxy shape, when the operator declared it.
            .or_else(|| m.get("max_input_tokens"))
            .or_else(|| m.get("top_provider").and_then(|p| p.get("context_length")))
            .and_then(Value::as_u64)
            .map(|n| n as u32)
    }

    /// Ask the server exactly how large the prompt is, via vLLM-style
    /// `POST /tokenize`, which applies the real chat template to the
    /// messages *and* the tool definitions. Beats guessing: the tool block
    /// alone is ~17k tokens on a full agentic turn, and `estimate_tokens`
    /// (chars/2.8, tuned for Thai prose) reads JSON schemas ~1.4x heavy.
    /// `None` when the server has no such endpoint — caller falls back to
    /// the estimate.
    async fn served_prompt_tokens(&self, req: &StreamRequest) -> Option<u32> {
        // vLLM serves `/tokenize` off the server root, NOT under `/v1`
        // (`/v1/tokenize` 404s), so strip the whole OpenAI path segment.
        let api_base = self
            .base_url
            .rsplit_once("/chat/completions")
            .map(|(base, _)| base)
            .unwrap_or_else(|| self.base_url.trim_end_matches('/'));
        let root = api_base.trim_end_matches('/').trim_end_matches("/v1");
        let url = format!("{root}/tokenize");
        let mut probe = self.build_body(req);
        // `/tokenize` only reads the prompt-shaped fields; the sampling ones
        // make some builds reject the request outright.
        for k in [
            "stream",
            "stream_options",
            "max_completion_tokens",
            "tool_choice",
        ] {
            probe.as_object_mut()?.remove(k);
        }
        let resp = self
            .client
            .post(&url)
            .header(self.auth_header_name(), self.auth_header_value())
            .header("content-type", "application/json")
            .json(&probe)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        v.get("count").and_then(Value::as_u64).map(|n| n as u32)
    }

    /// POST a prepared body to the chat/completions endpoint. Factored
    /// out so `stream` can issue a second attempt (image-stripped retry)
    /// without duplicating the header/auth wiring.
    async fn send_body(&self, body: &Value) -> Result<reqwest::Response> {
        let mut rb = crate::multi_tenant::attach_member(self.client.post(&self.base_url))
            .header(self.auth_header_name(), self.auth_header_value())
            .header("content-type", "application/json");
        if self.gateway_correlation {
            for (k, v) in crate::audit::correlation_headers() {
                rb = rb.header(k, v);
            }
        }
        rb.json(body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("http: {e}")))
    }
}

/// Strip the routing prefix from a stored model id before sending it
/// upstream. Normally just removes `prefix` (e.g. `openrouter/` →
/// `anthropic/claude-…`, `lmstudio/llama` → `llama`).
///
/// Special case for OpenRouter: its own router models (`openrouter/fusion`,
/// `openrouter/auto`) have vendor == "openrouter", colliding with the
/// `openrouter/` routing prefix. A real OpenRouter id is always
/// `vendor/model` (contains a slash); if stripping `openrouter/` leaves a
/// bare single segment, the original `openrouter/<x>` already WAS the
/// correct upstream id — keep it. Sending the vendor-less `<x>` 404s with
/// "No endpoints found that support tool use".
/// Whether an error body is OpenAI refusing function tools because the
/// model's default `reasoning_effort` is not 'none'.
///
/// Matched on the two stable halves of the sentence rather than the whole
/// string: the message names the model inline, so an exact match would break
/// on the next model, and both halves together are specific enough that no
/// other 400 collides with them.
fn needs_reasoning_effort_none(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("reasoning_effort") && b.contains("function tools")
}

fn strip_wire_prefix(model: &str, strip_prefix: Option<&str>) -> String {
    let Some(prefix) = strip_prefix else {
        return model.to_string();
    };
    match model.strip_prefix(prefix) {
        Some(rest) if prefix == "openrouter/" && !rest.contains('/') => model.to_string(),
        Some(rest) => rest.to_string(),
        None => model.to_string(),
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

/// True if any message in the request carries image pixels — either an
/// inline `ContentBlock::Image` (pasted/attached) or a `ToolResult` whose
/// content includes an image block (Read on an image, PdfRead rendering a
/// scanned/image PDF to pages). Used to turn an otherwise-opaque provider
/// 4xx into an actionable "this model can't see images" hint: text-only
/// models (DeepSeek v4, most non-`-vl` Qwen, etc.) reject image_url
/// content with a bare HTTP 400. See issue #164.
fn request_carries_image(req: &StreamRequest) -> bool {
    req.messages.iter().any(|m| {
        m.content.iter().any(|b| match b {
            ContentBlock::Image { .. } => true,
            ContentBlock::ToolResult { content, .. } => {
                matches!(content, ToolResultContent::Blocks(blocks)
                    if blocks.iter().any(|tb| matches!(tb, ToolResultBlock::Image { .. })))
            }
            _ => false,
        })
    })
}

/// A copy of `req` with every image pixel replaced by a short text note.
/// Used to retry once after a text-only model rejects `image_url` with a
/// 4xx (issue #164 follow-up): the turn then completes with the model
/// merely *told* an image existed, instead of dead-ending. The real
/// session history keeps the image — only this one wire request drops the
/// bytes — so a later switch to a vision model still sees it.
fn strip_request_images(req: &StreamRequest) -> StreamRequest {
    const NOTE: &str =
        "[image omitted — the current model is not vision-capable; the image file was still written to disk]";
    let mut out = req.clone();
    for m in &mut out.messages {
        for block in &mut m.content {
            match block {
                ContentBlock::Image { .. } => {
                    *block = ContentBlock::Text {
                        text: NOTE.to_string(),
                    };
                }
                ContentBlock::ToolResult { content, .. } => {
                    if let ToolResultContent::Blocks(blocks) = content {
                        for tb in blocks.iter_mut() {
                            if matches!(tb, ToolResultBlock::Image { .. }) {
                                *tb = ToolResultBlock::Text {
                                    text: NOTE.to_string(),
                                };
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// True when a 4xx response is a request-size / body-cap rejection rather than
/// a modality (image) rejection. The image-strip retry below must NOT fire on
/// these: stripping the images would wrongly stamp a vision-capable model
/// (e.g. gpt-4.1-nano) as "not vision-capable" and hide the real cause (too
/// many / too-large images in one request — the gateway's 5 MB body cap).
/// Fallback prompt sizing when the server won't tokenize for us. Deliberately
/// generous: `estimate_tokens` (chars/2.8, tuned for Thai prose) plus 25% and
/// a fixed slack for the chat template, so prompt + output stays inside the
/// served window even when the guess runs low.
fn estimated_prompt_reserve(req: &StreamRequest) -> usize {
    let mut est = crate::compaction::estimate_messages_tokens(&req.messages);
    if let Some(sys) = &req.system {
        est += crate::tokens::estimate_tokens(sys);
    }
    if !req.tools.is_empty() {
        if let Ok(s) = serde_json::to_string(&req.tools) {
            est += crate::tokens::estimate_tokens(&s);
        }
    }
    est + est / 4 + 1024
}

/// Pull `(context_window, prompt_tokens)` out of a "context length exceeded"
/// refusal. Both vLLM and OpenAI state the two numbers we need, so the retry
/// can size the next attempt exactly instead of halving blindly:
///
///   This model's maximum context length is 32768 tokens. However, you
///   requested 32000 output tokens and your prompt contains at least 769
///   input tokens, for a total of at least 32769 tokens.
///
///   This model's maximum context length is 8192 tokens. However, your
///   messages resulted in 8500 tokens.
///
/// `None` when the body isn't that error, or states no usable numbers.
fn context_window_overflow(body: &str) -> Option<(u32, u32)> {
    let ctx = regex::Regex::new(r"maximum context length is (\d+) tokens")
        .ok()?
        .captures(body)?
        .get(1)?
        .as_str()
        .parse::<u32>()
        .ok()?;
    // vLLM phrasing first, then OpenAI's.
    let prompt = regex::Regex::new(
        r"(?:prompt contains at least (\d+) input tokens|messages resulted in (\d+) tokens)",
    )
    .ok()
    .and_then(|re| re.captures(body))
    .and_then(|c| {
        c.get(1)
            .or_else(|| c.get(2))
            .map(|m| m.as_str().to_string())
    })
    .and_then(|v| v.parse::<u32>().ok())?;
    Some((ctx, prompt))
}

fn is_request_too_large(status: reqwest::StatusCode, body: &str) -> bool {
    if status.as_u16() == 413 {
        return true;
    }
    let t = body.to_ascii_lowercase();
    [
        "byte cap",
        "request body",
        "too large",
        "payload too large",
        "request_too_large",
        "entity too large",
    ]
    .iter()
    .any(|needle| t.contains(needle))
}

/// Probe a user-pointed OpenAI-compatible endpoint for a model's limits.
/// Builds its own client from the same env the provider uses, so callers
/// don't need a `Provider` handle (the trait isn't downcastable). `kind`
/// selects which env pair / model prefix to read — `openai-compat` and
/// `litellm` are the two kinds whose models can't come from the catalogue.
pub async fn probe_compat_limits(
    kind: ProviderKind,
    model: &str,
) -> Option<(u32, Option<u32>, String)> {
    let base = kind
        .endpoint_env()
        .and_then(|v| std::env::var(v).ok())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| kind.default_endpoint().map(String::from))?;
    // Local runtimes usually have no auth; the header is harmless there.
    let key = crate::secrets::get(kind.name())
        .or_else(|| kind.api_key_env().and_then(|v| std::env::var(v).ok()))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "local-no-auth".to_string());
    let url = if base.ends_with("/chat/completions") {
        base
    } else {
        format!("{}/chat/completions", base.trim_end_matches('/'))
    };
    let prefix = match kind {
        ProviderKind::LiteLlm => "litellm/",
        _ => "oai/",
    };
    OpenAIProvider::new(key)
        .with_base_url(url)
        .with_strip_model_prefix(prefix)
        .probe_model_limits(model)
        .await
}

impl OpenAIProvider {
    /// Ask an OpenAI-compatible endpoint what a model's limits are, so a
    /// `/model` switch onto a backend the shipped catalogue can't know
    /// (vLLM, LiteLLM, an internal proxy) still gets a real context window
    /// instead of the fallback — and a `max_tokens` the upstream accepts.
    ///
    /// Two shapes, in order:
    /// 1. `GET {base}/model/info` — LiteLLM: `data[].model_info` carries
    ///    `max_input_tokens` / `max_output_tokens`.
    /// 2. `GET {base}/models` — vLLM puts `max_model_len` on the model row;
    ///    some servers use `context_length`.
    ///
    /// Returns `(context, max_output, source)`. `None` when neither shape
    /// answers — the caller keeps whatever it had.
    pub async fn probe_model_limits(&self, model: &str) -> Option<(u32, Option<u32>, String)> {
        let bare = match &self.strip_model_prefix {
            Some(p) => model.strip_prefix(p.as_str()).unwrap_or(model),
            None => model,
        };
        let root = self
            .base_url
            .rsplit_once("/chat/completions")
            .map(|(b, _)| b.to_string())
            .unwrap_or_else(|| self.base_url.trim_end_matches('/').to_string());

        if let Some(hit) = self.probe_model_info(&root, bare).await {
            return Some(hit);
        }
        self.probe_models_list(&root, bare).await
    }

    async fn get_json(&self, url: &str) -> Option<Value> {
        let resp = self
            .client
            .get(url)
            .header(self.auth_header_name(), self.auth_header_value())
            .timeout(std::time::Duration::from_secs(8))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json().await.ok()
    }

    /// LiteLLM's `/model/info`. The proxy serves chat completions under
    /// `/v1` but mounts the admin routes at the root, so a base URL ending
    /// in `/v1` gets a second attempt one level up before giving up.
    async fn probe_model_info(&self, root: &str, bare: &str) -> Option<(u32, Option<u32>, String)> {
        let mut url = format!("{root}/model/info");
        let v = match self.get_json(&url).await {
            Some(v) => v,
            None => {
                let parent = root.strip_suffix("/v1")?;
                url = format!("{parent}/model/info");
                self.get_json(&url).await?
            }
        };
        let rows = v.get("data").and_then(Value::as_array)?;
        let row = rows.iter().find(|r| {
            r.get("model_name").and_then(Value::as_str) == Some(bare)
                || r.pointer("/model_info/id").and_then(Value::as_str) == Some(bare)
        })?;
        let info = row.get("model_info")?;
        let ctx = info
            .get("max_input_tokens")
            .or_else(|| info.get("max_tokens"))
            .and_then(Value::as_u64)? as u32;
        let out = info
            .get("max_output_tokens")
            .and_then(Value::as_u64)
            .map(|n| n as u32);
        Some((ctx, out, format!("{url} (model_info)")))
    }

    /// `/models`, where vLLM & friends publish the served length.
    async fn probe_models_list(
        &self,
        root: &str,
        bare: &str,
    ) -> Option<(u32, Option<u32>, String)> {
        let url = format!("{root}/models");
        let v = self.get_json(&url).await?;
        let rows = v.get("data").and_then(Value::as_array)?;
        let row = rows
            .iter()
            .find(|r| r.get("id").and_then(Value::as_str) == Some(bare))?;
        let ctx = row
            .get("max_model_len")
            .or_else(|| row.get("context_length"))
            .or_else(|| row.pointer("/model_info/max_input_tokens"))
            .and_then(Value::as_u64)? as u32;
        let out = row
            .get("max_output_tokens")
            .and_then(Value::as_u64)
            .map(|n| n as u32);
        Some((ctx, out, format!("{url} (max_model_len)")))
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
                        // into `/model` (e.g. `zai/glm-5.2`). `detect()`
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
        if let Some(m) = &self.model_override {
            req.model = m.clone();
        }
        req.model = strip_wire_prefix(&req.model, self.strip_model_prefix.as_deref());
        // Local OpenAI-compat backends (vLLM/ds4/…) reject
        // `max_completion_tokens > max_model_len`; clamp to the served context
        // so the default (32000) works against a small window. 2048 tokens of
        // headroom left for the prompt.
        if self.is_local_base() {
            if let Some(ctx) = self.served_context(&req.model).await {
                // Exact when the server will tell us, estimated otherwise.
                // Getting this right matters more than it looks: a full tool
                // block is ~17k of a 32k window, so an inflated guess used to
                // drive the cap straight into its own floor and leave 512
                // tokens of output — not enough for a reasoning model to
                // finish thinking, let alone emit the tool call afterwards.
                let reserve = match self.served_prompt_tokens(&req).await {
                    Some(exact) => exact as usize + 512,
                    None => estimated_prompt_reserve(&req),
                };
                let cap = (ctx as usize).saturating_sub(reserve).max(512) as u32;
                if req.max_tokens > cap {
                    req.max_tokens = cap;
                }
            }
        }
        let body = self.build_body(&req);
        let mut resp = self.send_body(&body).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            // Read the error body once, up front: we both classify it and
            // surface it. (Consumes `resp`; the success/strip-retry paths
            // below reassign it.)
            let text = resp.text().await.unwrap_or_default();
            let carries_image = request_carries_image(&req);
            let too_large = is_request_too_large(status, &text);

            // Issue #164 follow-up: a 4xx on a request shipping image pixels
            // is *usually* a text-only model rejecting `image_url`. Retry ONCE
            // with the pixels swapped for a short text note so the turn
            // completes. BUT a size/body-cap 4xx (the gateway's 5 MB cap, or a
            // 413) is NOT a vision problem — stripping there would mislabel a
            // vision-capable model as "not vision-capable" and mask the real
            // cause, so we surface a clear size error instead.
            // The server knows its own window; when it refuses because
            // prompt + max_tokens overruns it, resize and go again. This is
            // the backstop for every self-hosted setup that does not publish a
            // context window at all: a LiteLLM proxy in front of vLLM reports
            // `max_input_tokens: null`, so `served_context` finds nothing to
            // clamp against and the default 32000 sails past a 32k window.
            if let Some((ctx, prompt)) =
                context_window_overflow(&text).filter(|_| status.is_client_error())
            {
                // vLLM says "at least N input tokens" — a LOWER bound that
                // leaves out the tool block, which is the bulk of an agentic
                // prompt (79 tools ≈ 17k of a 32k window against a reported
                // 769). Sizing off that number alone overruns the window a
                // second time, so take whichever reserve is larger.
                let reserve = (prompt as usize).max(estimated_prompt_reserve(&req));
                let cap = (ctx as usize)
                    .saturating_sub(reserve)
                    .saturating_sub(256)
                    .max(256) as u32;
                if cap < req.max_tokens {
                    req.max_tokens = cap;
                    let retry_body = self.build_body(&req);
                    match self.send_body(&retry_body).await {
                        Ok(r) if r.status().is_success() => resp = r,
                        _ => {
                            return Err(Error::Provider(format!(
                                "http {status}: {}\n\n⚠️ `{}` has a {ctx}-token window and the prompt already uses {prompt}.                                  Retrying with max_tokens={cap} did not help — shorten the conversation (/compact) or use a model with a larger window.",
                                super::redact_key(&text, &self.api_key),
                                req.model
                            )));
                        }
                    }
                } else {
                    return Err(Error::Provider(format!(
                        "http {status}: {}\n\n⚠️ `{}` has a {ctx}-token window and the prompt alone uses {prompt}.                          There is no room left for a reply — shorten the conversation (/compact) or use a model with a larger window.",
                        super::redact_key(&text, &self.api_key),
                        req.model
                    )));
                }
            } else if status.is_client_error() && needs_reasoning_effort_none(&text) {
                // gpt-5.6-* defaults `reasoning_effort` to something other than
                // 'none' and then refuses function tools on chat/completions:
                //
                //   Function tools with reasoning_effort are not supported for
                //   gpt-5.6-terra in /v1/chat/completions. To use function
                //   tools, use /v1/responses or set reasoning_effort to 'none'.
                //
                // Nothing in this stack sends that field — the default is the
                // provider's — so the only way through chat/completions is to
                // say 'none' explicitly. Retrying on the error rather than
                // matching a model-name prefix is deliberate: the affected set
                // is whatever OpenAI decides next, and a hardcoded `gpt-5.6`
                // list would be wrong the week gpt-5.7 ships. It also leaves
                // reasoning untouched for every model that does not complain.
                //
                // Only this endpoint is affected; the same models reach us
                // fine through OpenRouter, which handles the field upstream.
                let mut retry_body = body.clone();
                if let Some(obj) = retry_body.as_object_mut() {
                    obj.insert("reasoning_effort".into(), json!("none"));
                }
                match self.send_body(&retry_body).await {
                    Ok(r) if r.status().is_success() => resp = r,
                    _ => {
                        return Err(Error::Provider(format!(
                            "http {status}: {}\n\n⚠️ `{}` rejects function tools unless reasoning is off. \
                             Retrying with `reasoning_effort: none` did not help — use this model without tools, \
                             or reach it through OpenRouter, which handles the combination.",
                            super::redact_key(&text, &self.api_key),
                            req.model
                        )));
                    }
                }
            } else if status.is_client_error() && carries_image && !too_large {
                let retry_body = self.build_body(&strip_request_images(&req));
                match self.send_body(&retry_body).await {
                    Ok(r) if r.status().is_success() => resp = r,
                    _ => {
                        return Err(Error::Provider(format!(
                            "http {status}: {}\n\n⚠️ This request included an image, but model `{}` may not support image input. \
                             Switch to a vision-capable model (e.g. dashscope/qwen3-vl-plus, gpt-4o, gemini-2.x, a Claude model), \
                             or extract the PDF/image to text first (e.g. read it once with a vision model and save to KMS, then query the text).",
                            super::redact_key(&text, &self.api_key),
                            req.model
                        )));
                    }
                }
            } else if too_large && carries_image {
                return Err(Error::Provider(format!(
                    "http {status}: {}\n\n⚠️ The request body is too large because of image data — not a vision-capability problem. \
                     Read fewer images per turn (the engine also auto-downscales images to fit the body cap).",
                    super::redact_key(&text, &self.api_key)
                )));
            } else {
                return Err(Error::Provider(format!(
                    "http {status}: {}",
                    super::redact_key(&text, &self.api_key)
                )));
            }
        }

        let byte_stream = resp.bytes_stream();
        let raw_dump = super::RawDump::new(format!("openai {}", req.model));
        let chunk_timeout = req
            .stream_chunk_timeout_override
            .unwrap_or_else(super::stream_chunk_timeout);

        let event_stream = try_stream! {
            // M6.21 BUG H1: buffer raw bytes so UTF-8 chars don't get
            // corrupted at chunk boundaries. See providers::find_bytes
            // doc comment for full bug description.
            let mut buffer: Vec<u8> = Vec::new();
            let mut byte_stream = Box::pin(byte_stream);
            let mut state = ParseState::default();
            let mut raw = raw_dump;
            let mut last_activity = std::time::Instant::now();
            let mut idle_total = std::time::Duration::ZERO;

            loop {
                let since = last_activity.elapsed();
                let threshold = crate::tool_display::THINKING_HEARTBEAT_AFTER;
                let wait = if since >= threshold {
                    crate::tool_display::HEARTBEAT_EVERY
                } else {
                    threshold - since
                }
                .min(chunk_timeout.saturating_sub(idle_total));

                let maybe_chunk = tokio::time::timeout(
                    wait,
                    byte_stream.next(),
                )
                .await;

                match maybe_chunk {
                    Err(_) => {
                        idle_total += wait;
                        if idle_total >= chunk_timeout {
                            Err(Error::Provider(format!(
                                "stream idle for {}s — provider stopped sending; try again",
                                chunk_timeout.as_secs()
                            )))?;
                        }
                        yield ProviderEvent::Progress(super::ProgressKind::Thinking);
                        continue;
                    }
                    Ok(maybe) => {
                        let Some(chunk) = maybe else { break };
                        let chunk = chunk.map_err(|e| Error::Provider(format!("stream: {e}")))?;
                        buffer.extend_from_slice(&chunk);
                        last_activity = std::time::Instant::now();
                        idle_total = std::time::Duration::ZERO;
                    }
                }

                while let Some(boundary) = super::find_bytes(&buffer, b"\n\n") {
                    let event_bytes: Vec<u8> = buffer.drain(..boundary + 2).collect();
                    let event_text = String::from_utf8_lossy(&event_bytes);
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
    /// Usage seen on the wire but not yet handed to the agent. Compat
    /// servers disagree on where the `stream_options.include_usage`
    /// counts ride: OpenAI/DashScope put them on a trailing frame with
    /// `choices: []`, but vLLM/SGLang/LiteLLM-proxied upstreams keep a
    /// `choices` entry with `finish_reason: null`, and some send the
    /// usage frame *before* the finish_reason one. Reading usage off
    /// only the finish_reason and empty-choices frames dropped both of
    /// those shapes, so the turn reported 0in/0out.
    pending_usage: Option<Usage>,
    /// `Agent::run_turn` *accumulates* every `MessageStop` usage, so a
    /// second delivery double-counts. Latch it.
    usage_delivered: bool,
    /// The real `finish_reason`, echoed on any later usage-only stop.
    /// `collect_turn` overwrites `stop_reason` on every `Done`, so a
    /// trailing frame that reports a different one relabels the turn —
    /// which is how a `length` truncation used to come out as `stop`.
    last_stop_reason: Option<String>,
}

impl ParseState {
    /// Hand over the captured usage at most once per stream.
    fn take_usage(&mut self) -> Option<Usage> {
        if self.usage_delivered {
            return None;
        }
        let u = self.pending_usage.take()?;
        self.usage_delivered = true;
        Some(u)
    }

    fn flush_eof(&mut self) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        if self.active_tool_index.is_some() {
            out.push(ProviderEvent::ContentBlockStop);
            self.active_tool_index = None;
        }
        // Usage that arrived on a frame we couldn't attach it to (or a
        // stream that ended without a finish_reason at all). Reported as
        // its own stop so the counts still reach the turn.
        if let Some(usage) = self.take_usage() {
            out.push(ProviderEvent::MessageStop {
                stop_reason: Some(
                    self.last_stop_reason
                        .clone()
                        .unwrap_or_else(|| "stop".to_string()),
                ),
                usage: Some(usage),
            });
            self.emitted_message_stop = true;
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
    // M6.22 BUG G1+G4: surface server-side prompt cache stats. OpenAI's
    // auto-cache reports the cached portion under
    // `usage.prompt_tokens_details.cached_tokens` (subset of
    // `prompt_tokens`). DeepSeek went their own way with
    // `prompt_cache_hit_tokens` (also subset of `prompt_tokens`).
    // Defensive dual-check: try the OpenAI shape first, fall back to
    // DeepSeek's. No provider should expose both with conflicting
    // meanings, so the precedence is safe.
    let cached = u
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .or_else(|| u.get("prompt_cache_hit_tokens").and_then(Value::as_u64));
    // Early-return guards against the trailing-usage-frame where both
    // counts are 0 — but if a fully-cached prompt arrives (cached > 0
    // even when uncached input == 0), keep the frame so the cache hit
    // is surfaced.
    if input == 0 && output == 0 && cached.unwrap_or(0) == 0 {
        return None;
    }
    // M6.22: subtract cached portion from input_tokens so the per-turn
    // pill and daily-totals math match Anthropic's semantics:
    //   total billable input = input_tokens (uncached new) + cache_read_input_tokens (cached)
    // OpenAI's wire format reports `prompt_tokens` as the FULL prompt
    // (cached + uncached), so we subtract `cached_tokens` here to get
    // the uncached portion. Without this, `usage.rs::record` would
    // double-count: input += 5000 AND cache_read += 4500 = 9500
    // contribution from a turn that actually consumed 5000 tokens.
    let cached_count = cached.unwrap_or(0);
    let uncached_input = input.saturating_sub(cached_count);
    // dev-plan/24: o1/o3 hidden reasoning tokens via Chat Completions
    // wire format. Lives at completion_tokens_details.reasoning_tokens
    // (folded into completion_tokens total already).
    let reasoning = u
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64);
    Some(Usage {
        input_tokens: uncached_input as u32,
        output_tokens: output as u32,
        // OpenAI doesn't separate writes from reads (auto-managed; the
        // user pays the write premium silently the first time). Map
        // cached → cache_read; leave cache_creation as None.
        cache_creation_input_tokens: None,
        cache_read_input_tokens: cached.map(|v| v as u32),
        reasoning_output_tokens: reasoning.map(|v| v as u32),
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

    // Capture usage from whatever frame carries it — the shape varies per
    // server (see `ParseState::pending_usage`). Delivery happens once, at
    // the finish_reason stop, the trailing-frame stop, or EOF.
    if let Some(usage) = parse_openai_usage(&v) {
        state.pending_usage = Some(usage);
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
            if let Some(usage) = state.take_usage() {
                out.push(ProviderEvent::MessageStop {
                    stop_reason: Some(
                        state
                            .last_stop_reason
                            .clone()
                            .unwrap_or_else(|| "stop".to_string()),
                    ),
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
                    out.push(ProviderEvent::ToolUseStart {
                        id,
                        name,
                        thought_signature: None,
                    });
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
            usage: state.take_usage(),
        });
        state.emitted_message_stop = true;
        state.last_stop_reason = Some(reason.to_string());
    }

    // M6.21 BUG M2: the trailing-usage-frame guard at the top of the
    // function (when `choices` is missing/empty) handles the standard
    // OpenAI shape where usage arrives in a separate frame. The
    // duplicate guard that USED to live here also fired when
    // `finish_reason` and `usage` arrived in the SAME chunk (some
    // OpenAI-compat aggregators consolidate them) — emitting a second
    // MessageStop with the same usage values, which the agent loop's
    // `cumulative_usage.accumulate` then double-counted. Removed.
    // The trailing-usage-frame case is exclusively the empty-choices
    // path above.

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
        // DeepSeek's hosted API names the R1 model `deepseek-reasoner`
        // (not `deepseek-r1`). Same reasoning_content shape on the wire.
        "deepseek-reasoner",
    ];
    let m = model.to_lowercase();
    PATTERNS.iter().any(|p| m.contains(p))
}

#[cfg(test)]
mod tests {
    /// `/model` onto a user-hosted backend has to learn the real context
    /// somewhere — the shipped catalogue can't know a private vLLM box.
    /// Both published shapes are parsed off the same JSON the servers send.
    #[test]
    fn model_limit_shapes_parse() {
        let litellm = serde_json::json!({
            "data": [
                {"model_name": "other", "model_info": {"max_input_tokens": 1}},
                {"model_name": "qwen3-32b-awq",
                 "model_info": {"max_input_tokens": 131072, "max_output_tokens": 8192}}
            ]
        });
        let row = litellm["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["model_name"] == "qwen3-32b-awq")
            .unwrap();
        assert_eq!(row["model_info"]["max_input_tokens"].as_u64(), Some(131072));
        assert_eq!(row["model_info"]["max_output_tokens"].as_u64(), Some(8192));

        let vllm = serde_json::json!({
            "data": [{"id": "qwen3-32b-awq", "max_model_len": 40960}]
        });
        let row = vllm["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "qwen3-32b-awq")
            .unwrap();
        assert_eq!(row["max_model_len"].as_u64(), Some(40960));
    }

    use super::*;
    use crate::providers::{assemble, collect_turn};
    use crate::types::Message;

    #[test]
    fn size_cap_4xx_not_classified_as_vision_error() {
        use reqwest::StatusCode;
        // The gateway's body-cap 400 must read as a size error, so the
        // image-strip ("not vision-capable") retry is skipped.
        assert!(is_request_too_large(
            StatusCode::BAD_REQUEST,
            r#"{"error":"request body exceeds 5242880 byte cap"}"#
        ));
        assert!(is_request_too_large(StatusCode::PAYLOAD_TOO_LARGE, ""));
        // A genuine modality rejection is NOT a size error → strip path stays.
        assert!(!is_request_too_large(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"This model does not support image_url","type":"invalid_request_error"}}"#
        ));
    }

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

    /// Collect every `MessageStop` that carries usage. Exactly one per
    /// stream — `Agent::run_turn` accumulates them, so a second delivery
    /// double-bills the turn.
    fn usage_stops(events: &[ProviderEvent]) -> Vec<&Usage> {
        events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::MessageStop { usage: Some(u), .. } => Some(u),
                _ => None,
            })
            .collect()
    }

    /// vLLM / SGLang / several LiteLLM-proxied upstreams keep a `choices`
    /// entry (empty delta, `finish_reason: null`) on the trailing
    /// `include_usage` frame instead of sending `choices: []`. Pre-fix the
    /// parser only read usage off the finish_reason frame and the
    /// empty-choices frame, so this shape reported 0in/0out — the
    /// `openai-compat` symptom.
    #[test]
    fn usage_frame_with_a_null_finish_reason_choice_is_not_dropped() {
        let events = parse_all(&[
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}",
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}",
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":null}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3}}",
            "data: [DONE]",
        ]);
        let stops = usage_stops(&events);
        assert_eq!(stops.len(), 1, "got: {events:?}");
        assert_eq!(stops[0].input_tokens, 11);
        assert_eq!(stops[0].output_tokens, 3);
    }

    /// Some servers put the usage frame *ahead* of the finish_reason one.
    /// The counts ride the stop that follows rather than being dropped.
    #[test]
    fn usage_frame_before_the_finish_reason_rides_the_stop() {
        let events = parse_all(&[
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}",
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3}}",
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}",
            "data: [DONE]",
        ]);
        let stops = usage_stops(&events);
        assert_eq!(stops.len(), 1, "got: {events:?}");
        assert_eq!(stops[0].input_tokens, 11);
        assert_eq!(stops[0].output_tokens, 3);
    }

    /// A stream that ends without any finish_reason still has to report
    /// what it spent — the counts come out at EOF.
    #[test]
    fn usage_without_a_finish_reason_is_flushed_at_eof() {
        let events = parse_all(&[
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}",
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3}}",
            "data: [DONE]",
        ]);
        let stops = usage_stops(&events);
        assert_eq!(stops.len(), 1, "got: {events:?}");
        assert_eq!(stops[0].input_tokens, 11);
    }

    /// `collect_turn` overwrites `stop_reason` on every `Done`, so the
    /// trailing usage frame must echo the real one. It used to hardcode
    /// "stop", relabelling a truncated turn as a clean finish.
    #[test]
    fn trailing_usage_frame_keeps_the_real_stop_reason() {
        let events = parse_all(&[
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}",
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}",
            "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3}}",
            "data: [DONE]",
        ]);
        let reasons: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::MessageStop { stop_reason, .. } => stop_reason.as_deref(),
                _ => None,
            })
            .collect();
        assert_eq!(reasons, vec!["length", "length"], "got: {events:?}");
    }

    /// M6.22 BUG G1: surface OpenAI's auto-prompt-cache stats. Pre-fix
    /// `parse_openai_usage` hardcoded `cache_read_input_tokens: None`,
    /// hiding the cached-portion of `prompt_tokens` from the per-turn
    /// pill and daily totals even though OpenAI was applying the 50%
    /// discount server-side. Verify `usage.prompt_tokens_details.cached_tokens`
    /// is parsed.
    #[test]
    fn parse_openai_usage_reads_cached_tokens_from_prompt_tokens_details() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"usage": {
                "prompt_tokens": 5000,
                "completion_tokens": 200,
                "prompt_tokens_details": {"cached_tokens": 4500}
            }}"#,
        )
        .unwrap();
        let u = parse_openai_usage(&v).expect("usage parsed");
        assert_eq!(u.input_tokens, 500, "uncached portion (5000 - 4500)");
        assert_eq!(u.output_tokens, 200);
        assert_eq!(u.cache_read_input_tokens, Some(4500));
        assert_eq!(u.cache_creation_input_tokens, None);
    }

    /// M6.22 BUG G4: DeepSeek uses `prompt_cache_hit_tokens` instead of
    /// OpenAI's `prompt_tokens_details.cached_tokens`. Defensive
    /// dual-check in `parse_openai_usage` should catch both since
    /// DeepSeek routes through `OpenAIProvider`.
    #[test]
    fn parse_openai_usage_reads_deepseek_prompt_cache_hit_tokens() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"usage": {
                "prompt_tokens": 5000,
                "prompt_cache_hit_tokens": 4500,
                "prompt_cache_miss_tokens": 500,
                "completion_tokens": 200
            }}"#,
        )
        .unwrap();
        let u = parse_openai_usage(&v).expect("usage parsed");
        assert_eq!(u.input_tokens, 500);
        assert_eq!(u.output_tokens, 200);
        assert_eq!(u.cache_read_input_tokens, Some(4500));
    }

    /// Edge case from M6.22 audit: a fully-cached prompt where uncached
    /// input is 0 but cached is non-zero must still surface (don't
    /// trigger the trailing-usage-frame None guard).
    #[test]
    fn parse_openai_usage_surfaces_fully_cached_prompts() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"usage": {
                "prompt_tokens": 4500,
                "completion_tokens": 0,
                "prompt_tokens_details": {"cached_tokens": 4500}
            }}"#,
        )
        .unwrap();
        let u = parse_openai_usage(&v).expect("usage with all-cached input must surface");
        assert_eq!(u.cache_read_input_tokens, Some(4500));
    }

    /// Old behavior preserved: a usage frame with no token counts at
    /// all returns None (the trailing-empty-frame case stays guarded).
    #[test]
    fn parse_openai_usage_returns_none_on_truly_empty_frame() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"usage": {"prompt_tokens": 0, "completion_tokens": 0}}"#)
                .unwrap();
        assert!(parse_openai_usage(&v).is_none());
    }

    /// M6.21 BUG M2: when a chunk contains BOTH `finish_reason` (on a
    /// non-empty `choices` entry) AND a top-level `usage` object — as
    /// some OpenAI-compat aggregators (LiteLLM, OpenRouter forks) emit
    /// — pre-fix the parser fired TWO `MessageStop` events with the
    /// same usage values, which the agent's `cumulative_usage.accumulate`
    /// then double-counted. Verify only ONE MessageStop comes out.
    #[test]
    fn finish_reason_with_inline_usage_does_not_double_emit_message_stop() {
        let events = parse_all(&[
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}",
            // Single chunk consolidating finish_reason AND usage:
            "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}",
            "data: [DONE]",
        ]);

        let stops: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ProviderEvent::MessageStop { .. }))
            .collect();
        assert_eq!(
            stops.len(),
            1,
            "expected exactly one MessageStop, got {}: {:?}",
            stops.len(),
            stops
        );
        match stops[0] {
            ProviderEvent::MessageStop { stop_reason, usage } => {
                assert_eq!(stop_reason.as_deref(), Some("stop"));
                let u = usage.as_ref().expect("usage should be present");
                assert_eq!(u.input_tokens, 10);
                assert_eq!(u.output_tokens, 5);
            }
            _ => unreachable!(),
        }
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
                name: "read_file".into(),
                thought_signature: None,
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
                name: "r".into(),
                thought_signature: None,
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
                name: "w".into(),
                thought_signature: None,
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
                        thought_signature: None,
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
            stream_chunk_timeout_override: None,
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
    fn messages_to_openai_dedups_duplicate_tool_call_ids() {
        // DeepSeek-style glitch: two parallel tool_calls share one id, and
        // the results come back with that same duplicated id. Both sides
        // must collapse to a single call + single tool message so the
        // endpoint sees one matched pair (not 2 calls vs 1 result, which
        // 400s as "insufficient tool messages following tool_calls").
        let req = StreamRequest {
            model: "deepseek-v4-pro".into(),
            system: None,
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::ToolUse {
                            id: "call_0".into(),
                            name: "WebSearch".into(),
                            input: json!({"query": "a"}),
                            thought_signature: None,
                        },
                        ContentBlock::ToolUse {
                            id: "call_0".into(),
                            name: "WebFetch".into(),
                            input: json!({"url": "b"}),
                            thought_signature: None,
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::ToolResult {
                            tool_use_id: "call_0".into(),
                            content: "r1".into(),
                            is_error: false,
                        },
                        ContentBlock::ToolResult {
                            tool_use_id: "call_0".into(),
                            content: "r2".into(),
                            is_error: false,
                        },
                    ],
                },
            ],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
            stream_chunk_timeout_override: None,
        };
        let msgs = OpenAIProvider::messages_to_openai(&req);
        // assistant(1 deduped tool_call), tool(1 deduped result)
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(msgs[0]["tool_calls"][0]["id"], "call_0");
        assert_eq!(msgs.iter().filter(|m| m["role"] == "tool").count(), 1);
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "call_0");
    }

    #[test]
    fn messages_to_openai_tool_results_precede_interleaved_user_text() {
        // A user message carrying tool_results AND text must emit the tool
        // messages FIRST (immediately after the assistant tool_calls), with
        // the user text after — never wedged between the calls and their
        // results.
        let req = StreamRequest {
            model: "deepseek-v4-pro".into(),
            system: None,
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "call_0".into(),
                        name: "WebSearch".into(),
                        input: json!({"query": "a"}),
                        thought_signature: None,
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::ToolResult {
                            tool_use_id: "call_0".into(),
                            content: "result".into(),
                            is_error: false,
                        },
                        ContentBlock::Text {
                            text: "now summarize".into(),
                        },
                    ],
                },
            ],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
            stream_chunk_timeout_override: None,
        };
        let msgs = OpenAIProvider::messages_to_openai(&req);
        // assistant(tool_calls), tool(result), user(text) — tool BEFORE user.
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "assistant");
        assert!(msgs[0]["tool_calls"].is_array());
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "call_0");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"], "now summarize");
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
                        thought_signature: None,
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
            stream_chunk_timeout_override: None,
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
    fn request_carries_image_detects_inline_and_tool_result_images() {
        use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};
        let img = ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        };
        let base = |content: Vec<ContentBlock>| StreamRequest {
            model: "deepseek-v4-flash".into(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content,
            }],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
            stream_chunk_timeout_override: None,
        };

        // Text-only request → no image.
        assert!(!request_carries_image(&base(vec![ContentBlock::text(
            "hi"
        )])));

        // Inline (pasted) image → detected.
        assert!(request_carries_image(&base(vec![ContentBlock::Image {
            source: img.clone(),
        }])));

        // Image riding inside a ToolResult (Read / PdfRead fallback) → detected.
        assert!(request_carries_image(&base(vec![
            ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: ToolResultContent::Blocks(vec![ToolResultBlock::Image { source: img }]),
                is_error: false,
            }
        ])));

        // Text-only ToolResult → no image.
        assert!(!request_carries_image(&base(vec![
            ContentBlock::ToolResult {
                tool_use_id: "c2".into(),
                content: ToolResultContent::Text("just text".into()),
                is_error: false,
            }
        ])));
    }

    #[test]
    fn strip_request_images_drops_pixels_keeps_text() {
        // Issue #164 follow-up: the retry path must leave NO image pixels
        // (so a text-only model stops 400'ing on image_url) while keeping
        // the tool result's text summary so the model still knows an image
        // was produced.
        use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};
        let img = ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        };
        let req = StreamRequest {
            model: "deepseek-v4-pro".into(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Image {
                        source: img.clone(),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "c1".into(),
                        content: ToolResultContent::Blocks(vec![
                            ToolResultBlock::Text {
                                text: "Wrote output/img.png".into(),
                            },
                            ToolResultBlock::Image { source: img },
                        ]),
                        is_error: false,
                    },
                ],
            }],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
            stream_chunk_timeout_override: None,
        };
        assert!(request_carries_image(&req));

        let stripped = strip_request_images(&req);
        assert!(
            !request_carries_image(&stripped),
            "no image pixels should remain after stripping"
        );
        // The OpenAI wire form must carry no image_url, but keep the summary.
        let wire = OpenAIProvider::messages_to_openai(&stripped);
        let json = serde_json::to_string(&wire).unwrap();
        assert!(
            !json.contains("image_url"),
            "stripped request must not emit image_url: {json}"
        );
        assert!(json.contains("Wrote output/img.png"));
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
                            thought_signature: None,
                        },
                        ContentBlock::ToolUse {
                            id: "call_b".into(),
                            name: "Read".into(),
                            input: json!({"path": "/tmp/b.png"}),
                            thought_signature: None,
                        },
                        ContentBlock::ToolUse {
                            id: "call_c".into(),
                            name: "Read".into(),
                            input: json!({"path": "/tmp/c.png"}),
                            thought_signature: None,
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
            stream_chunk_timeout_override: None,
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
            stream_chunk_timeout_override: None,
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
                        thought_signature: None,
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
            stream_chunk_timeout_override: None,
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
            stream_chunk_timeout_override: None,
        };
        let body = OpenAIProvider::new("k").build_body(&req);
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(body["tools"][0]["function"]["description"], "read a file");
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["path"]["type"],
            "string"
        );
    }

    #[test]
    fn build_body_appends_injected_tool_and_tool_choice() {
        use crate::types::ToolDef;
        let req = StreamRequest {
            model: "openai/gpt-4.1".into(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![ToolDef {
                name: "read_file".into(),
                description: "read a file".into(),
                input_schema: json!({"type":"object"}),
            }],
            max_tokens: 100,
            thinking_budget: None,
            stream_chunk_timeout_override: None,
        };
        let provider = OpenAIProvider::new("k")
            .with_injected_tool(json!({
                "type": "openrouter:fusion",
                "parameters": {"analysis_models": ["anthropic/claude-opus-4.8"]}
            }))
            .with_tool_choice(json!("required"));
        let body = provider.build_body(&req);
        // Agent function tool stays first; fusion tool is appended after.
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(body["tools"][1]["type"], "openrouter:fusion");
        assert_eq!(
            body["tools"][1]["parameters"]["analysis_models"][0],
            "anthropic/claude-opus-4.8"
        );
        assert_eq!(body["tool_choice"], "required");
    }

    #[test]
    fn model_override_then_strip_yields_outer_wire_model() {
        // fusion+ scenario: stream() replaces the model with the configured
        // outer model, then strip_model_prefix removes the routing prefix.
        let provider = OpenAIProvider::new("k")
            .with_strip_model_prefix("openrouter/")
            .with_model_override("openrouter/openai/gpt-4.1");
        let override_model = provider.model_override.clone().unwrap();
        let wire = strip_wire_prefix(&override_model, provider.strip_model_prefix.as_deref());
        assert_eq!(wire, "openai/gpt-4.1");
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
    async fn probe_model_info_falls_back_to_the_root_mount() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // LiteLLM serves chat completions under /v1 but mounts the admin
        // routes at the root, so probing only {base}/model/info misses.
        let server = MockServer::start().await;
        let body = r#"{"data":[{"model_name":"gpt-4o-mini","model_info":
            {"max_input_tokens":128000,"max_output_tokens":16384}}]}"#;
        Mock::given(method("GET"))
            .and(path("/v1/model/info"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/model/info"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let provider = OpenAIProvider::new("local-no-auth")
            .with_base_url(format!("{}/v1/chat/completions", server.uri()))
            .with_strip_model_prefix("litellm/");
        let (ctx, max_out, source) = provider
            .probe_model_limits("litellm/gpt-4o-mini")
            .await
            .expect("limits");
        assert_eq!(ctx, 128_000);
        assert_eq!(max_out, Some(16_384));
        assert!(
            source.ends_with("/model/info (model_info)"),
            "got: {source}"
        );
        assert!(!source.contains("/v1/model/info"), "got: {source}");
    }

    /// A local backend's real constraint is `prompt + output <= max_model_len`,
    /// and the tool block dominates the prompt. Estimating it drove the cap
    /// into its own 512 floor, which starved reasoning models mid-thought —
    /// the symptom once misread as "thinking breaks tool-calling". Ask the
    /// server instead: vLLM's `/tokenize` applies the real chat template to
    /// messages *and* tools.
    /// A LiteLLM proxy in front of vLLM publishes no window at all
    /// (`max_input_tokens: null`), so nothing can be clamped up front and the
    /// server refuses the default 32000. Its refusal states both numbers —
    /// parse them so the retry can be sized exactly.
    #[test]
    fn context_overflow_is_parsed_from_both_server_phrasings() {
        let vllm = "litellm.ContextWindowExceededError: OpenAIException - This model's \
                    maximum context length is 32768 tokens. However, you requested 32000 \
                    output tokens and your prompt contains at least 769 input tokens, for \
                    a total of at least 32769 tokens.";
        assert_eq!(context_window_overflow(vllm), Some((32768, 769)));

        let openai = "This model's maximum context length is 8192 tokens. However, your \
                      messages resulted in 8500 tokens.";
        assert_eq!(context_window_overflow(openai), Some((8192, 8500)));

        // Unrelated 4xx bodies must not be mistaken for it.
        assert_eq!(
            context_window_overflow("The model `auto` does not exist."),
            None
        );
        assert_eq!(
            context_window_overflow("\"auto\" tool choice requires --enable-auto-tool-choice"),
            None
        );
    }

    /// End to end: the first attempt overruns the window, the retry is resized
    /// from the server's own numbers and succeeds.
    #[tokio::test]
    async fn context_overflow_retries_with_a_fitted_max_tokens() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"c\",\"model\":\"auto\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut req = local_req(32000);
        req.model = "auto".into();
        // The server's "at least 769" leaves out the tool block, so the retry
        // sizes off whichever reserve is larger.
        let reserve = 769usize.max(estimated_prompt_reserve(&req));
        let expected = 32768usize - reserve - 256;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_partial_json(
                serde_json::json!({"max_completion_tokens": expected}),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;
        // Anything else (i.e. the un-resized first attempt) gets the refusal.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "This model's maximum context length is 32768 tokens. However, you \
                 requested 32000 output tokens and your prompt contains at least 769 \
                 input tokens, for a total of at least 32769 tokens.",
            ))
            .mount(&server)
            .await;

        let provider =
            OpenAIProvider::new("k").with_base_url(format!("{}/v1/chat/completions", server.uri()));
        let raw = provider.stream(req).await.expect("retry succeeds");
        let out = collect_turn(assemble(raw)).await.expect("collect");
        assert_eq!(out.text, "ok");
        assert!(
            expected < 32000,
            "the retry must shrink the ask: {expected}"
        );
    }

    #[tokio::test]
    async fn local_clamp_sizes_the_prompt_with_served_tokenize() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        mount_models(&server, 32768).await;
        Mock::given(method("POST"))
            .and(path("/tokenize"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"count": 21555, "max_model_len": 32768})),
            )
            .mount(&server)
            .await;
        mount_empty_stream(&server).await;

        let body = sent_body(&server, local_req(32000)).await;
        // 32768 - 21555 - 512 slack
        assert_eq!(body["max_completion_tokens"], 10701);
        // The template's own default decides thinking; we no longer force it
        // off behind the user's back.
        assert!(
            body.get("chat_template_kwargs").is_none(),
            "must not override the chat template: {body}"
        );
    }

    /// Servers without `/tokenize` (llama.cpp builds, ds4) must still get a
    /// clamp — just from the estimate.
    #[tokio::test]
    async fn local_clamp_falls_back_to_the_estimate_without_tokenize() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        mount_models(&server, 8192).await;
        Mock::given(method("POST"))
            .and(path("/tokenize"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        mount_empty_stream(&server).await;

        let body = sent_body(&server, local_req(32000)).await;
        let cap = body["max_completion_tokens"].as_u64().unwrap();
        assert!(cap < 8192, "clamped below the served window: {cap}");
        assert!(cap >= 512, "never below the floor: {cap}");
    }

    async fn mount_models(server: &wiremock::MockServer, ctx: u32) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "local-model", "max_model_len": ctx}]
            })))
            .mount(server)
            .await;
    }

    async fn mount_empty_stream(server: &wiremock::MockServer) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let sse = concat!(
            "data: {\"id\":\"c\",\"model\":\"local-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(server)
            .await;
    }

    fn local_req(max_tokens: u32) -> StreamRequest {
        StreamRequest {
            model: "local-model".into(),
            system: Some("you are an agent".into()),
            messages: vec![Message::user("hey")],
            tools: vec![crate::types::ToolDef {
                name: "Bash".into(),
                description: "Run a shell command.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"]
                }),
            }],
            max_tokens,
            thinking_budget: None,
            stream_chunk_timeout_override: None,
        }
    }

    /// Drive one stream against the mock and hand back the chat body it saw.
    async fn sent_body(server: &wiremock::MockServer, req: StreamRequest) -> serde_json::Value {
        let provider =
            OpenAIProvider::new("k").with_base_url(format!("{}/v1/chat/completions", server.uri()));
        let raw = provider.stream(req).await.expect("stream");
        let _ = collect_turn(assemble(raw)).await;
        let reqs = server.received_requests().await.expect("recorded requests");
        let chat = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/chat/completions"))
            .expect("a chat request was sent");
        serde_json::from_slice(&chat.body).expect("json body")
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
            stream_chunk_timeout_override: None,
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
            stream_chunk_timeout_override: None,
        };
        let raw = provider.stream(req).await.expect("stream");
        let result = collect_turn(assemble(raw)).await.expect("collect");

        assert_eq!(result.text, "");
        assert_eq!(result.tool_uses.len(), 1);
        if let ContentBlock::ToolUse {
            id, name, input, ..
        } = &result.tool_uses[0]
        {
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
                ProviderEvent::Progress(_) => "Progress",
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
            stream_chunk_timeout_override: None,
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
            stream_chunk_timeout_override: None,
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

    /// The retry that unblocks gpt-5.6-* must fire on OpenAI's wording and
    /// nothing else. Matching too loosely would send `reasoning_effort: none`
    /// after unrelated 400s — silently disabling reasoning on models that
    /// never asked for it.
    #[test]
    fn reasoning_effort_retry_matches_only_the_tools_refusal() {
        assert!(needs_reasoning_effort_none(
            "Function tools with reasoning_effort are not supported for \
             gpt-5.6-terra in /v1/chat/completions. To use function tools, \
             use /v1/responses or set reasoning_effort to 'none'."
        ));
        // Same sentence, a model OpenAI has not shipped yet.
        assert!(needs_reasoning_effort_none(
            "Function tools with reasoning_effort are not supported for gpt-9-x"
        ));
        for other in [
            "Unsupported parameter: 'max_tokens' is not supported with this model.",
            "This model is only supported in v1/responses and not in v1/chat/completions.",
            "The model `gpt-5.1-codex` has been deprecated",
            "rate limit exceeded",
            // Mentions one half only — not the refusal we handle.
            "Invalid value for reasoning_effort: expected one of low, medium, high",
            "Function tools must have a name",
        ] {
            assert!(
                !needs_reasoning_effort_none(other),
                "should not match: {other}"
            );
        }
    }

    /// `openrouter/fusion` and `openrouter/auto` are OpenRouter's own
    /// vendor-less ids: stripping the routing prefix must not eat the
    /// vendor segment of a normal `openrouter/<vendor>/<model>`.
    #[test]
    fn strip_wire_prefix_handles_openrouter_vendor_collision() {
        // Normal OpenRouter models: strip the routing prefix → vendor/model.
        assert_eq!(
            strip_wire_prefix(
                "openrouter/anthropic/claude-sonnet-4-6",
                Some("openrouter/")
            ),
            "anthropic/claude-sonnet-4-6"
        );
        assert_eq!(
            strip_wire_prefix("openrouter/openai/gpt-4.1", Some("openrouter/")),
            "openai/gpt-4.1"
        );
        // OpenRouter-vendor router models: keep the full id (vendor is
        // "openrouter") — stripping would send a vendor-less id that 404s.
        assert_eq!(
            strip_wire_prefix("openrouter/fusion", Some("openrouter/")),
            "openrouter/fusion"
        );
        assert_eq!(
            strip_wire_prefix("openrouter/auto", Some("openrouter/")),
            "openrouter/auto"
        );
        // Other providers still strip to a bare id (no slash by design).
        assert_eq!(
            strip_wire_prefix("lmstudio/llama-3.2", Some("lmstudio/")),
            "llama-3.2"
        );
        assert_eq!(
            strip_wire_prefix("dashscope/qwen-max", Some("dashscope/")),
            "qwen-max"
        );
        // No prefix configured / no match → unchanged.
        assert_eq!(strip_wire_prefix("gpt-4o", None), "gpt-4o");
        assert_eq!(strip_wire_prefix("gpt-4o", Some("openrouter/")), "gpt-4o");
    }

    /// public issue #195: DeepSeek (and any endpoint that enforces the same
    /// rule) 400s with "insufficient tool messages following tool_calls" when
    /// anything of another role lands between an assistant's `tool_calls` and
    /// the tool messages answering them. The agent loop appends drained
    /// injections — teammate reports, reminders — as Text blocks onto the very
    /// user message that carries the tool results, so the serializer has to
    /// emit the tool messages first even though the text sits in the same
    /// turn. That has held since v0.78.0 on a comment alone; this pins it.
    #[test]
    fn injected_text_never_wedges_between_tool_calls_and_their_results() {
        let history = vec![
            Message::user("run it"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                    thought_signature: None,
                }],
            },
            // Tool result AND an injection, in one user turn — the shape the
            // agent loop actually produces.
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "call_1".into(),
                        content: crate::types::ToolResultContent::Text("a.txt".into()),
                        is_error: false,
                    },
                    ContentBlock::Text {
                        text: "[teammate] backend finished".into(),
                    },
                ],
            },
        ];
        let req = StreamRequest {
            model: "deepseek-v4-flash".into(),
            system: None,
            messages: history,
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
            stream_chunk_timeout_override: None,
        };
        let msgs = OpenAIProvider::messages_to_openai(&req);
        let roles: Vec<&str> = msgs.iter().filter_map(|m| m["role"].as_str()).collect();

        let assistant_at = roles
            .iter()
            .position(|r| *r == "assistant")
            .expect("assistant");
        assert_eq!(
            roles.get(assistant_at + 1),
            Some(&"tool"),
            "a tool message must immediately follow the tool_calls; got {roles:?}"
        );
        // And the injected text still reaches the model, just after.
        let injected = msgs.iter().skip(assistant_at + 1).any(|m| {
            m["role"] == "user" && m["content"].as_str().unwrap_or("").contains("teammate")
        });
        assert!(injected, "injected text was dropped: {msgs:?}");
    }

    /// Issue #163 Bug 3: a reasoning-ONLY assistant turn (a Thinking
    /// block, no text / tools) must still serialize a `content` field —
    /// some OpenAI-compatible providers 400 on an assistant message with
    /// no `content`. We fall back to an empty string.
    #[test]
    fn messages_to_openai_reasoning_only_turn_has_empty_content() {
        let history = vec![
            Message::user("solve x"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Thinking {
                    content: "thinking but produced no text".into(),
                    signature: None,
                }],
            },
            Message::user("continue"),
        ];
        let req = StreamRequest {
            model: "deepseek/deepseek-v4-flash".into(),
            system: None,
            messages: history,
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
            stream_chunk_timeout_override: None,
        };
        let msgs = OpenAIProvider::messages_to_openai(&req);
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        // `content` must be present (not missing) and an empty string.
        assert_eq!(
            assistant.get("content"),
            Some(&serde_json::json!("")),
            "reasoning-only assistant must carry content:\"\"; got {assistant:?}"
        );
    }
}
