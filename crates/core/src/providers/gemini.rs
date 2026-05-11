//! Google Gemini provider — `generativelanguage.googleapis.com/v1beta` with
//! SSE streaming.
//!
//! Wire shape (different enough from Anthropic/OpenAI to warrant its own adapter):
//! - Endpoint: `{base}/v1beta/models/{model}:streamGenerateContent?alt=sse`.
//!   Auth via `x-goog-api-key` header.
//! - Body uses `contents` instead of `messages`, with `user`/`model` roles.
//!   System prompt goes in a top-level `systemInstruction` field.
//! - Each content message has `parts: [{text}|{functionCall}|{functionResponse}]`.
//! - Tool declarations live under `tools: [{functionDeclarations: [...]}]`.
//! - Tool results come back as `user` messages containing a `functionResponse`
//!   part. There's no explicit tool_use_id — we track id→name locally for
//!   round-tripping.
//! - SSE format: `data: {json}\n\n`. No event: lines. No [DONE] terminator.
//!   Tool calls are **not streamed**: a functionCall part arrives in a single
//!   chunk with the full `args` object.
//!
//! Model name handling: the `gemini-*` prefix is passed through directly.

use super::{EventStream, ModelInfo, Provider, ProviderEvent, StreamRequest, Usage};
use crate::error::{Error, Result};
use crate::types::{ContentBlock, ImageSource, Role, ToolResultBlock, ToolResultContent};
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// If `THCLAWS_DEBUG_GEMINI` is set, open the log file (creating dirs as
/// needed) and write a separator + the request body. Returns the file handle
/// so the streaming loop can append raw chunks to it.
fn open_debug_log(body: &Value, model: &str) -> Option<std::fs::File> {
    let setting = std::env::var("THCLAWS_DEBUG_GEMINI").ok()?;
    if setting.is_empty() || setting == "0" {
        return None;
    }
    let path = if setting == "1" || setting.eq_ignore_ascii_case("true") {
        std::env::current_dir()
            .ok()?
            .join(".thclaws/logs/gemini-raw.log")
    } else {
        std::path::PathBuf::from(setting)
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    use std::io::Write;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(f, "\n===== {now} model={model} =====");
    let _ = writeln!(
        f,
        "REQUEST: {}",
        serde_json::to_string(body).unwrap_or_default()
    );
    let _ = writeln!(f, "RAW STREAM:");
    let _ = f.flush();
    eprintln!(
        "\x1b[35m[gemini debug] logging raw response → {}\x1b[0m",
        path.display()
    );
    Some(f)
}

pub struct GeminiProvider {
    client: Client,
    api_key: String,
    base_url: String,
    /// M6.21 BUG H2: monotonic counter for synthesized tool-call ids,
    /// SHARED across all streams from this provider instance. Pre-fix
    /// the counter lived per-`ParseState` and reset to 0 every stream,
    /// so id `gemini-call-0` from turn 1 collided with id
    /// `gemini-call-0` from turn 2. The `id_to_name` HashMap built in
    /// `messages_to_gemini` had last-write-wins semantics, so turn 1's
    /// ToolResult got mislabeled with turn 2's tool name in the wire
    /// `functionResponse.name` field — breaking multi-turn tool sessions.
    next_tool_id: Arc<AtomicU64>,
}

impl GeminiProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            next_tool_id: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Convert canonical messages → Gemini `contents` array.
    /// Gemini uses `user`/`model` roles (not `assistant`), inlines tool_use
    /// as `functionCall` parts, and inlines tool_result as `functionResponse`
    /// parts in a `user` message. System messages are skipped (they go in
    /// the top-level `systemInstruction` via `build_body`).
    fn messages_to_gemini(req: &StreamRequest) -> Vec<Value> {
        // Build id→tool name map so ToolResult blocks can resolve the right
        // function name (Gemini's functionResponse needs `name`, not an id).
        let mut id_to_name: HashMap<String, String> = HashMap::new();
        for m in &req.messages {
            if matches!(m.role, Role::Assistant) {
                for block in &m.content {
                    if let ContentBlock::ToolUse { id, name, .. } = block {
                        id_to_name.insert(id.clone(), name.clone());
                    }
                }
            }
        }

        let mut out: Vec<Value> = Vec::new();
        for m in &req.messages {
            if matches!(m.role, Role::System) {
                continue;
            }
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "model",
                Role::System => unreachable!(),
            };
            let mut parts: Vec<Value> = Vec::new();
            for block in &m.content {
                match block {
                    ContentBlock::Text { text } => {
                        if !text.is_empty() {
                            parts.push(json!({"text": text}));
                        }
                    }
                    // Gemini doesn't accept reasoning_content; drop it.
                    // The block stays in our local history for any future
                    // turns against a thinking model — only the wire body
                    // strips it.
                    ContentBlock::Thinking { .. } => {}
                    // User-attached image (Phase 4 paste/drag-drop).
                    // Gemini's `inlineData` part can sit alongside text
                    // parts inside the same user content, so we just
                    // push it into the parts vec directly.
                    ContentBlock::Image {
                        source: ImageSource::Base64 { media_type, data },
                    } => {
                        parts.push(json!({
                            "inlineData": {
                                "mimeType": media_type,
                                "data": data,
                            }
                        }));
                    }
                    ContentBlock::ToolUse {
                        name,
                        input,
                        thought_signature,
                        ..
                    } => {
                        let mut fc = json!({
                            "functionCall": { "name": name, "args": input }
                        });
                        if let Some(sig) = thought_signature {
                            fc["thoughtSignature"] = json!(sig);
                        }
                        parts.push(fc);
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        let name = id_to_name
                            .get(tool_use_id)
                            .cloned()
                            .unwrap_or_else(|| "unknown".to_string());
                        // The functionResponse part itself is text-only —
                        // Gemini parses `response.content` as a scalar
                        // string. Multimodal payloads (Read on a PNG)
                        // ride alongside as inlineData parts in the
                        // same user content (see below).
                        parts.push(json!({
                            "functionResponse": {
                                "name": name,
                                "response": { "content": content.to_text() }
                            }
                        }));
                        // Image-bearing tool result → push inlineData
                        // parts into the same user content. Gemini
                        // accepts mixed part types within one content,
                        // and a vision-capable model (gemini-1.5-*,
                        // gemini-2.x family) decodes inlineData as an
                        // image. Non-vision models will reject — the
                        // user must select an appropriate model.
                        if let ToolResultContent::Blocks(blocks) = content {
                            for b in blocks {
                                if let ToolResultBlock::Image {
                                    source: ImageSource::Base64 { media_type, data },
                                } = b
                                {
                                    parts.push(json!({
                                        "inlineData": {
                                            "mimeType": media_type,
                                            "data": data,
                                        }
                                    }));
                                }
                            }
                        }
                    }
                }
            }
            if !parts.is_empty() {
                out.push(json!({"role": role, "parts": parts}));
            }
        }
        out
    }

    fn build_body(req: &StreamRequest) -> Value {
        let contents = Self::messages_to_gemini(req);
        let mut body = json!({
            "contents": contents,
            "generationConfig": {
                "maxOutputTokens": req.max_tokens,
            },
        });
        // Gemma open-weights models are served via the same API but don't
        // support `systemInstruction` ("Developer instruction is not enabled")
        // or function calling ("Function calling is not enabled"). For Gemma
        // we inline the system prompt as the first user turn and skip tools.
        //
        // Gemma also does chain-of-thought in plain prose by default. Ask it
        // to wrap reasoning in `<thinking>...</thinking>` so we can visually
        // demote it downstream.
        let is_gemma = req.model.starts_with("gemma-");
        if is_gemma {
            let thinking_rule = "Format rule (mandatory): wrap any internal \
                reasoning, planning, or self-talk in <thinking>...</thinking> \
                tags. Put ONLY the final user-facing answer outside those \
                tags. Do not reveal raw chain-of-thought as plain text.";
            let sys = req.system.as_deref().unwrap_or("").to_string();
            let combined = if sys.is_empty() {
                thinking_rule.to_string()
            } else {
                format!("{sys}\n\n{thinking_rule}")
            };
            let prefixed = json!({
                "role": "user",
                "parts": [{"text": combined}]
            });
            if let Some(arr) = body["contents"].as_array_mut() {
                arr.insert(0, prefixed);
            }
        } else if let Some(sys) = &req.system {
            if !sys.is_empty() {
                body["systemInstruction"] = json!({
                    "parts": [{"text": sys}]
                });
            }
        }
        let supports_tools = !is_gemma;
        if supports_tools && !req.tools.is_empty() {
            let decls: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    })
                })
                .collect();
            body["tools"] = json!([{ "functionDeclarations": decls }]);
        }
        body
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/v1beta/models", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .get(&url)
            .header("x-goog-api-key", &self.api_key)
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
            .get("models")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        // `name` comes back as `models/gemini-2.0-flash` — strip the prefix
                        // so users can paste it straight into /model.
                        let raw = m.get("name").and_then(Value::as_str)?;
                        let id = raw.strip_prefix("models/").unwrap_or(raw).to_string();
                        let display_name = m
                            .get("displayName")
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
        let body = Self::build_body(&req);
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.base_url.trim_end_matches('/'),
            req.model
        );

        let resp = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
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

        let is_gemma = req.model.starts_with("gemma-");
        // Optional raw-response logging / inline dump:
        //   THCLAWS_DEBUG_GEMINI=1              → log to ./.thclaws/logs/gemini-raw.log
        //   THCLAWS_DEBUG_GEMINI=/path/to/file  → log to that exact path
        //   THCLAWS_SHOW_RAW=1                  → dump the assistant's raw text
        //                                          to stderr after each turn
        // The two are independent — set both for file + inline. The inline
        // dump is dim-formatted and fenced so it's easy to scan for
        // protocol/formatting issues without leaving the terminal.
        let debug_log = open_debug_log(&body, &req.model);
        let raw_dump = super::RawDump::new(format!("gemini {}", req.model));
        // M6.21 BUG H2: pass the provider's shared tool-id counter into
        // the parser so synthesized ids stay unique across streams.
        let counter = self.next_tool_id.clone();
        let event_stream = try_stream! {
            // M6.21 BUG H1: byte buffer to avoid UTF-8 corruption at
            // chunk boundaries. Critical for Gemini because Gemini-served
            // models often emit non-ASCII text (Thai, CJK) and the
            // streamGenerateContent endpoint returns CRLF-framed events
            // that frequently span TCP packets.
            let mut buffer: Vec<u8> = Vec::new();
            let mut byte_stream = Box::pin(byte_stream);
            let mut state = ParseState::with_counter(counter);
            // For Gemma: track whether we're currently inside a
            // `<thinking>...</thinking>` block across chunk boundaries so we
            // can wrap inner text with ANSI dim codes.
            let mut think = ThinkFilter::new();
            let mut log = debug_log;
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
                if let Some(f) = log.as_mut() {
                    use std::io::Write;
                    let _ = f.write_all(&chunk);
                    let _ = f.flush();
                }
                buffer.extend_from_slice(&chunk);

                // SSE event boundaries can be either `\n\n` (unix) or
                // `\r\n\r\n` (HTTP-spec). Google Gen Lang returns CRLF on
                // streamGenerateContent, so a plain `\n\n` search silently
                // buffers forever and yields zero events.
                while let Some((boundary, sep_len)) = super::find_bytes(&buffer, b"\r\n\r\n").map(|p| (p, 4))
                    .or_else(|| super::find_bytes(&buffer, b"\n\n").map(|p| (p, 2)))
                {
                    let event_bytes: Vec<u8> = buffer.drain(..boundary + sep_len).collect();
                    let event_text = String::from_utf8_lossy(&event_bytes);
                    let trimmed = event_text
                        .trim_end_matches(|c: char| c == '\n' || c == '\r');
                    for event in parse_sse_event(trimmed, &mut state)? {
                        if let ProviderEvent::TextDelta(ref s) = event {
                            raw.push(s);
                        }
                        if is_gemma {
                            if let ProviderEvent::TextDelta(s) = event {
                                let transformed = think.push(&s);
                                if !transformed.is_empty() {
                                    yield ProviderEvent::TextDelta(transformed);
                                }
                                continue;
                            }
                        }
                        yield event;
                    }
                }
            }
            if is_gemma {
                let tail = think.flush();
                if !tail.is_empty() {
                    yield ProviderEvent::TextDelta(tail);
                }
            }
            raw.flush();
        };

        Ok(Box::pin(event_stream))
    }
}

#[derive(Debug)]
pub struct ParseState {
    pub seen_message_start: bool,
    /// M6.21 BUG H2: now an `Arc<AtomicU64>` shared across streams via
    /// the parent provider so synthesized tool ids stay unique.
    /// `Default::default()` mints a fresh local counter for tests.
    pub next_tool_id: Arc<AtomicU64>,
}

impl Default for ParseState {
    fn default() -> Self {
        Self {
            seen_message_start: false,
            next_tool_id: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl ParseState {
    /// Construct with a shared counter from the parent provider.
    /// Production code uses this so synthesized ids are unique across
    /// all streams from the same `GeminiProvider`.
    pub fn with_counter(counter: Arc<AtomicU64>) -> Self {
        Self {
            seen_message_start: false,
            next_tool_id: counter,
        }
    }
}

/// Parse one SSE event from the Gemini stream. Stateful across events.
pub fn parse_sse_event(raw: &str, state: &mut ParseState) -> Result<Vec<ProviderEvent>> {
    let mut out = Vec::new();

    // Find the `data:` line.
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
    let v: Value = serde_json::from_str(data)?;

    if !state.seen_message_start {
        let model = v
            .get("modelVersion")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        out.push(ProviderEvent::MessageStart { model });
        state.seen_message_start = true;
    }

    let Some(candidates) = v.get("candidates").and_then(Value::as_array) else {
        return Ok(out);
    };
    let Some(candidate) = candidates.first() else {
        return Ok(out);
    };

    // Emit text deltas and tool calls from parts.
    if let Some(parts) = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
    {
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    out.push(ProviderEvent::TextDelta(text.to_string()));
                }
            } else if let Some(fc) = part.get("functionCall") {
                let name = fc
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                // M6.21 BUG H2: fetch_add on the shared counter so ids
                // stay unique across streams. Pre-fix the per-stream
                // counter reset to 0 every turn, colliding ids across
                // turns.
                let counter_value = state.next_tool_id.fetch_add(1, Ordering::Relaxed);
                let id = format!("gemini-call-{counter_value}");
                let thought_signature = part
                    .get("thoughtSignature")
                    .and_then(Value::as_str)
                    .map(String::from);
                out.push(ProviderEvent::ToolUseStart {
                    id,
                    name,
                    thought_signature,
                });
                out.push(ProviderEvent::ToolUseDelta {
                    partial_json: args.to_string(),
                });
                out.push(ProviderEvent::ContentBlockStop);
            }
        }
    }

    // finishReason → MessageStop
    if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
        let usage = v.get("usageMetadata").map(|u| {
            let total_input = u
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            // M6.22 BUG G3: surface Gemini's implicit prompt cache
            // (auto-caches prefixes ≥4096 tokens on Pro/Flash, 25%
            // discount on the cached portion). Pre-fix this was
            // hardcoded None, hiding the savings from the per-turn
            // pill and daily totals.
            //
            // Subtract cached from promptTokenCount so the canonical
            // `Usage.input_tokens` is the UNCACHED new portion —
            // matching Anthropic semantics.
            let cached = u.get("cachedContentTokenCount").and_then(Value::as_u64);
            let cached_count = cached.unwrap_or(0);
            let uncached_input = total_input.saturating_sub(cached_count);
            Usage {
                input_tokens: uncached_input as u32,
                output_tokens: u
                    .get("candidatesTokenCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: cached.map(|v| v as u32),
            }
        });
        out.push(ProviderEvent::MessageStop {
            stop_reason: Some(reason.to_string()),
            usage,
        });
    }

    Ok(out)
}

/// Streaming filter for Gemma's `<thinking>...</thinking>` blocks.
///
/// We ask Gemma in its system prompt to wrap chain-of-thought in those tags,
/// then this filter translates them to ANSI dim sequences inline. Tags can
/// straddle chunk boundaries, so we hold back partial-tag tail bytes between
/// pushes.
struct ThinkFilter {
    inside: bool,
    /// Bytes we've buffered because they *might* be the start of a tag.
    pending: String,
}

impl ThinkFilter {
    fn new() -> Self {
        Self {
            inside: false,
            pending: String::new(),
        }
    }

    /// Feed a chunk of raw text from the model. Returns the transformed text
    /// (may include ANSI escapes; may be empty if everything is buffered).
    fn push(&mut self, chunk: &str) -> String {
        let mut input = std::mem::take(&mut self.pending);
        input.push_str(chunk);
        let mut out = String::new();
        let mut idx = 0;
        let bytes = input.as_bytes();
        const OPEN: &str = "<thinking>";
        const CLOSE: &str = "</thinking>";

        // Styling:
        //   inside  <thinking>...</thinking>   → dim white   (\x1b[2;37m)
        //   outside (the user-facing answer)   → bright green (\x1b[1;32m)
        // Each emitted span is wrapped with its open code + reset, so the
        // terminal returns to default after the stream ends.
        const STYLE_THINK: &str = "\x1b[2;37m";
        const STYLE_ANSWER: &str = "\x1b[1;32m";
        const RESET: &str = "\x1b[0m";

        while idx < bytes.len() {
            let needle = if self.inside { CLOSE } else { OPEN };
            if let Some(rel) = input[idx..].find(needle) {
                // Emit the text before the tag with the appropriate styling.
                let before = &input[idx..idx + rel];
                if !before.is_empty() {
                    let style = if self.inside {
                        STYLE_THINK
                    } else {
                        STYLE_ANSWER
                    };
                    out.push_str(style);
                    out.push_str(before);
                    out.push_str(RESET);
                }
                idx += rel + needle.len();
                let was_inside = self.inside;
                self.inside = !self.inside;
                // After closing a thinking block, drop the model's first
                // newline (if any) and emit our own so the user-facing answer
                // always starts on a clean fresh line under the reasoning.
                if was_inside && !self.inside {
                    let after = &input[idx..];
                    if let Some(s) = after.strip_prefix('\n') {
                        idx += 1;
                        if s.starts_with('\n') {
                            idx += 1;
                        }
                    }
                    out.push('\n');
                }
            } else {
                // No complete tag in remaining input. Hold back the LONGEST
                // suffix of the tail that's a prefix of `needle` so a tag
                // straddling the chunk boundary still resolves correctly.
                //
                // CAREFUL: tag bytes are ASCII, but the surrounding text may
                // include multi-byte chars (Thai, CJK, emoji). Only consider
                // suffix lengths that land on a char boundary, otherwise
                // string slicing panics and takes the whole agent thread —
                // and the GUI window — down with it.
                let tail = &input[idx..];
                let max_partial = needle.len().saturating_sub(1);
                let limit = tail.len().min(max_partial);
                let mut keepback = 0;
                for n in 1..=limit {
                    let start = tail.len() - n;
                    if !tail.is_char_boundary(start) {
                        continue;
                    }
                    if needle.starts_with(&tail[start..]) {
                        keepback = n;
                    }
                }
                let split_at = tail.len() - keepback;
                debug_assert!(tail.is_char_boundary(split_at));
                let emit = &tail[..split_at];
                if !emit.is_empty() {
                    let style = if self.inside {
                        STYLE_THINK
                    } else {
                        STYLE_ANSWER
                    };
                    out.push_str(style);
                    out.push_str(emit);
                    out.push_str(RESET);
                }
                self.pending.push_str(&tail[split_at..]);
                break;
            }
        }
        out
    }

    /// Stream ended — emit any held-back bytes with the active style.
    fn flush(&mut self) -> String {
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() {
            return String::new();
        }
        let style = if self.inside {
            "\x1b[2;37m"
        } else {
            "\x1b[1;32m"
        };
        format!("{style}{pending}\x1b[0m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{assemble, collect_turn};
    use crate::types::Message;

    fn parse_all(events: &[&str]) -> Vec<ProviderEvent> {
        let mut state = ParseState::default();
        let mut out = Vec::new();
        for e in events {
            out.extend(parse_sse_event(e, &mut state).unwrap());
        }
        out
    }

    #[test]
    fn parse_text_stream() {
        let events = parse_all(&[
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hello\"}]}}],\"modelVersion\":\"gemini-2.0-flash\"}",
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\" world\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":4,\"candidatesTokenCount\":2}}",
        ]);
        assert!(
            matches!(&events[0], ProviderEvent::MessageStart { model } if model == "gemini-2.0-flash")
        );
        assert_eq!(events[1], ProviderEvent::TextDelta("Hello".into()));
        assert_eq!(events[2], ProviderEvent::TextDelta(" world".into()));
        match &events[3] {
            ProviderEvent::MessageStop { stop_reason, usage } => {
                assert_eq!(stop_reason.as_deref(), Some("STOP"));
                let u = usage.as_ref().unwrap();
                assert_eq!(u.input_tokens, 4);
                assert_eq!(u.output_tokens, 2);
            }
            e => panic!("expected MessageStop, got {:?}", e),
        }
    }

    /// M6.22 BUG G3: surface Gemini's implicit prompt cache. Pre-fix
    /// `usageMetadata.cachedContentTokenCount` was hardcoded None,
    /// hiding the auto-cache savings (25% discount on cached portion
    /// for prefixes ≥4096 tokens on Pro/Flash) from the user.
    #[test]
    fn parse_extracts_cached_content_token_count_from_usage_metadata() {
        let events = parse_all(&[
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hi\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5000,\"candidatesTokenCount\":200,\"cachedContentTokenCount\":4500,\"totalTokenCount\":5200},\"modelVersion\":\"gemini-2.0-flash\"}",
        ]);
        let stop = events
            .iter()
            .find_map(|e| match e {
                ProviderEvent::MessageStop { usage: Some(u), .. } => Some(u),
                _ => None,
            })
            .expect("MessageStop with usage");
        // input_tokens reports the UNCACHED portion (5000 - 4500 = 500).
        // Total billable = input + cache_read = 500 + 4500 = 5000 (matches promptTokenCount).
        assert_eq!(stop.input_tokens, 500);
        assert_eq!(stop.output_tokens, 200);
        assert_eq!(stop.cache_read_input_tokens, Some(4500));
        assert_eq!(stop.cache_creation_input_tokens, None);
    }

    #[test]
    fn parse_handles_usage_without_cached_content_token_count() {
        // Pre-cache turns / models without implicit caching: the field
        // is absent. Must still produce Usage with cache_read None.
        let events = parse_all(&[
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hi\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":50,\"candidatesTokenCount\":3},\"modelVersion\":\"gemini-2.0-flash\"}",
        ]);
        let stop = events
            .iter()
            .find_map(|e| match e {
                ProviderEvent::MessageStop { usage: Some(u), .. } => Some(u),
                _ => None,
            })
            .expect("MessageStop with usage");
        assert_eq!(stop.input_tokens, 50);
        assert_eq!(stop.output_tokens, 3);
        assert_eq!(stop.cache_read_input_tokens, None);
    }

    /// M6.21 BUG H2: synthesized tool ids must NOT collide across
    /// streams. Pre-fix the per-`ParseState` counter reset to 0 every
    /// stream, so `gemini-call-0` from turn 1 collided with
    /// `gemini-call-0` from turn 2 — `messages_to_gemini`'s id_to_name
    /// HashMap then last-wrote turn 2's tool name onto turn 1's
    /// ToolResult, producing a wire `functionResponse: {name: <wrong>}`.
    /// Verify a shared `Arc<AtomicU64>` counter keeps ids unique
    /// across streams.
    #[test]
    fn synthesized_tool_ids_stay_unique_across_streams() {
        let shared = Arc::new(AtomicU64::new(0));

        // Turn 1
        let mut state = ParseState::with_counter(shared.clone());
        let events_t1 = parse_sse_event(
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"Read\",\"args\":{}}}]},\"finishReason\":\"STOP\"}],\"modelVersion\":\"gemini-2.0-flash\"}",
            &mut state,
        ).unwrap();

        // Turn 2 — fresh ParseState (simulates new stream) but same shared counter
        let mut state = ParseState::with_counter(shared.clone());
        let events_t2 = parse_sse_event(
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"Bash\",\"args\":{}}}]},\"finishReason\":\"STOP\"}],\"modelVersion\":\"gemini-2.0-flash\"}",
            &mut state,
        ).unwrap();

        // Extract the synthesized ids from each turn
        let t1_id = events_t1
            .iter()
            .find_map(|e| match e {
                ProviderEvent::ToolUseStart { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("turn 1 ToolUseStart");
        let t2_id = events_t2
            .iter()
            .find_map(|e| match e {
                ProviderEvent::ToolUseStart { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("turn 2 ToolUseStart");

        assert_ne!(
            t1_id, t2_id,
            "ids must be unique across streams to prevent id_to_name HashMap collision"
        );
        assert_eq!(t1_id, "gemini-call-0");
        assert_eq!(t2_id, "gemini-call-1");
    }

    #[test]
    fn parse_function_call_emits_complete_tool_use() {
        let events = parse_all(&[
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"Read\",\"args\":{\"path\":\"/tmp/x\"}}}]},\"finishReason\":\"STOP\"}],\"modelVersion\":\"gemini-2.0-flash\"}",
        ]);
        assert!(matches!(events[0], ProviderEvent::MessageStart { .. }));
        assert_eq!(
            events[1],
            ProviderEvent::ToolUseStart {
                id: "gemini-call-0".into(),
                name: "Read".into(),
                thought_signature: None,
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
    fn parse_ignores_events_with_no_data_line() {
        let mut state = ParseState::default();
        let events = parse_sse_event("event: ping", &mut state).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn messages_to_gemini_maps_tool_result_name_via_id_lookup() {
        let req = StreamRequest {
            model: "gemini-2.0-flash".into(),
            system: Some("be brief".into()),
            messages: vec![
                Message::user("hi"),
                crate::types::Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "Read".into(),
                        input: json!({"path": "/x"}),
                        thought_signature: None,
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
        let contents = GeminiProvider::messages_to_gemini(&req);
        // user(hi), model(functionCall), user(functionResponse)
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "hi");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["functionCall"]["name"], "Read");
        assert_eq!(contents[2]["role"], "user");
        // The crucial bit: name is filled from the id→name map, not left as "unknown".
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "Read");
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["response"]["content"],
            "file body"
        );
    }

    #[test]
    fn messages_to_gemini_image_tool_result_emits_inline_data_part() {
        // ToolResult with Blocks (Image + Text) — Gemini wants the
        // functionResponse part to carry only the text summary, with
        // each image emitted as a sibling inlineData part inside the
        // *same* user content. Mixing part types within one content
        // is the documented Gemini pattern for getting tool-returned
        // imagery in front of a vision-capable model.
        use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};
        let req = StreamRequest {
            model: "gemini-2.0-flash".into(),
            system: None,
            messages: vec![
                crate::types::Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "t2".into(),
                        name: "Read".into(),
                        input: json!({"path": "/tmp/x.png"}),
                        thought_signature: None,
                    }],
                },
                crate::types::Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "t2".into(),
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
        let contents = GeminiProvider::messages_to_gemini(&req);
        // model(functionCall), user(functionResponse + inlineData)
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[1]["role"], "user");
        let parts = contents[1]["parts"].as_array().expect("parts array");
        assert_eq!(
            parts.len(),
            2,
            "expected functionResponse + inlineData parts, got {parts:#?}"
        );

        // First part: text-only functionResponse summarizing the image.
        assert_eq!(parts[0]["functionResponse"]["name"], "Read");
        assert_eq!(
            parts[0]["functionResponse"]["response"]["content"],
            "image: x.png · 1 KB · image/png"
        );

        // Second part: inlineData carrying the actual pixels.
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[1]["inlineData"]["data"], "AAAA");
    }

    #[test]
    fn messages_to_gemini_user_message_with_image_emits_inline_data() {
        // User attaches an image to a chat message (Phase 4 paste /
        // drag-drop). Gemini accepts inlineData parts directly inside
        // a user content's parts array, alongside text parts.
        use crate::types::{ContentBlock, ImageSource};
        let req = StreamRequest {
            model: "gemini-2.0-flash".into(),
            system: None,
            messages: vec![crate::types::Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: "describe this image".into(),
                    },
                    ContentBlock::Image {
                        source: ImageSource::Base64 {
                            media_type: "image/png".into(),
                            data: "QQQQ".into(),
                        },
                    },
                ],
            }],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let contents = GeminiProvider::messages_to_gemini(&req);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        let parts = contents[0]["parts"].as_array().expect("parts array");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "describe this image");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[1]["inlineData"]["data"], "QQQQ");
    }

    #[test]
    fn messages_to_gemini_text_only_tool_result_skips_inline_data() {
        // No images → no extra inlineData part. Regression guard
        // against accidentally appending empty parts every turn.
        let req = StreamRequest {
            model: "gemini-2.0-flash".into(),
            system: None,
            messages: vec![
                crate::types::Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "t3".into(),
                        name: "Bash".into(),
                        input: json!({"cmd": "ls"}),
                        thought_signature: None,
                    }],
                },
                crate::types::Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "t3".into(),
                        content: "file1\nfile2\n".into(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let contents = GeminiProvider::messages_to_gemini(&req);
        let parts = contents[1]["parts"].as_array().expect("parts array");
        assert_eq!(parts.len(), 1, "expected only the functionResponse part");
        assert!(parts[0].get("functionResponse").is_some());
    }

    #[test]
    fn build_body_places_system_in_systemInstruction() {
        let req = StreamRequest {
            model: "gemini-2.0-flash".into(),
            system: Some("you are helpful".into()),
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 1024,
            thinking_budget: None,
        };
        let body = GeminiProvider::build_body(&req);
        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"],
            "you are helpful"
        );
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 1024);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn build_body_tool_declarations_shape() {
        use crate::types::ToolDef;
        let req = StreamRequest {
            model: "gemini-2.0-flash".into(),
            system: None,
            messages: vec![Message::user("x")],
            tools: vec![ToolDef {
                name: "Read".into(),
                description: "read a file".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}}
                }),
            }],
            max_tokens: 100,
            thinking_budget: None,
        };
        let body = GeminiProvider::build_body(&req);
        assert_eq!(body["tools"][0]["functionDeclarations"][0]["name"], "Read");
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["description"],
            "read a file"
        );
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parameters"]["type"],
            "object"
        );
    }

    #[tokio::test]
    async fn list_models_strips_models_prefix() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = r#"{"models":[
            {"name":"models/gemini-2.0-flash","displayName":"Gemini 2.0 Flash"},
            {"name":"models/gemini-1.5-pro","displayName":"Gemini 1.5 Pro"}
        ]}"#;
        Mock::given(method("GET"))
            .and(path("/v1beta/models"))
            .and(header("x-goog-api-key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let provider = GeminiProvider::new("test-key").with_base_url(server.uri());
        let models = provider.list_models().await.expect("list");
        // Sorted + prefix stripped.
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["gemini-1.5-pro", "gemini-2.0-flash"]);
        assert_eq!(models[1].display_name.as_deref(), Some("Gemini 2.0 Flash"));
    }

    #[tokio::test]
    async fn stream_end_to_end_text_via_wiremock() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sse_body = concat!(
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hi \"}]}}],\"modelVersion\":\"gemini-2.0-flash\"}\n\n",
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"there\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
            ))
            .and(header("x-goog-api-key", "test-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = GeminiProvider::new("test-key").with_base_url(server.uri());
        let req = StreamRequest {
            model: "gemini-2.0-flash".into(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 100,
            thinking_budget: None,
        };
        let raw = provider.stream(req).await.expect("stream");
        let result = collect_turn(assemble(raw)).await.expect("collect");
        assert_eq!(result.text, "Hi there");
        assert_eq!(result.stop_reason.as_deref(), Some("STOP"));
        assert_eq!(result.usage.as_ref().unwrap().output_tokens, 2);
    }

    #[tokio::test]
    async fn stream_end_to_end_function_call_via_wiremock() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sse_body = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"Read\",\"args\":{\"path\":\"/tmp/x\"}}}]},\"finishReason\":\"STOP\"}],\"modelVersion\":\"gemini-2.0-flash\"}\n\n";
        Mock::given(method("POST"))
            .and(path(
                "/v1beta/models/gemini-2.0-flash:streamGenerateContent",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let provider = GeminiProvider::new("test-key").with_base_url(server.uri());
        let req = StreamRequest {
            model: "gemini-2.0-flash".into(),
            system: None,
            messages: vec![Message::user("read")],
            tools: vec![],
            max_tokens: 100,
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

    #[test]
    fn think_filter_dims_inside_tags_in_one_chunk() {
        let mut f = ThinkFilter::new();
        let out = f.push("hello <thinking>plan stuff</thinking>world");
        // Newline after </thinking> so the user-facing answer starts on its
        // own line under the reasoning.
        assert_eq!(
            out,
            "\x1b[1;32mhello \x1b[0m\x1b[2;37mplan stuff\x1b[0m\n\x1b[1;32mworld\x1b[0m"
        );
        assert_eq!(f.flush(), "");
        assert!(!f.inside);
    }

    #[test]
    fn think_filter_collapses_models_own_trailing_newlines_after_close() {
        let mut f = ThinkFilter::new();
        // Model already emits "</thinking>\n\nanswer" — we should NOT end up
        // with three newlines before "answer".
        let out = f.push("<thinking>plan</thinking>\n\nanswer");
        assert_eq!(out, "\x1b[2;37mplan\x1b[0m\n\x1b[1;32manswer\x1b[0m");
    }

    #[test]
    fn think_filter_handles_split_open_tag_across_chunks() {
        let mut f = ThinkFilter::new();
        // Tag starts at very end of chunk 1, finishes in chunk 2.
        let a = f.push("hello <thi");
        // "hello " emitted as bright-green (the answer style) since we're
        // not yet inside a thinking block; the partial "<thi" is held back.
        assert_eq!(a, "\x1b[1;32mhello \x1b[0m");
        let b = f.push("nking>plan</thinking>done");
        assert_eq!(b, "\x1b[2;37mplan\x1b[0m\n\x1b[1;32mdone\x1b[0m");
        assert_eq!(f.flush(), "");
    }

    #[test]
    fn think_filter_does_not_panic_on_multibyte_chars_at_chunk_end() {
        // "สวัสดี" → mostly 3-byte UTF-8 chars; if the keepback search splits
        // mid-byte the slice indexing panics, taking the agent thread down.
        let mut f = ThinkFilter::new();
        let _ = f.push("สวัสดี");
        let _ = f.push("จาก");
        let _ = f.push(" AI <thi");
        let _ = f.push("nking>plan</thinking>OK");
        let _ = f.flush();
        // No panic = pass.
    }

    #[test]
    fn think_filter_handles_unclosed_thinking_at_eof() {
        // The dim styling is emitted on push; flush only handles the held-back
        // partial-tag tail (none here).
        let mut f = ThinkFilter::new();
        let pushed = f.push("answer <thinking>still planning");
        assert_eq!(
            pushed,
            "\x1b[1;32manswer \x1b[0m\x1b[2;37mstill planning\x1b[0m"
        );
        assert_eq!(f.flush(), "");
        assert!(f.inside);
    }
}
