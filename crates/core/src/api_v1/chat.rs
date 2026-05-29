//! `POST /v1/chat/completions` — OpenAI-compatible chat endpoint.
//!
//! Maps OpenAI request shape → thClaws agent run → OpenAI response.
//! See `dev-plan/19-thclaws-openai-compat.md` §"POST /v1/chat/completions".
//!
//! Each request is independent: we build a fresh `Agent` per call,
//! feed the `messages` array as history, run the last user message
//! through one turn, and serialize the outcome. The pod's filesystem
//! (cwd / `/workspace`) is the long-lived state across calls — chat
//! history is stateless per OpenAI convention.

use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;

use super::errors::OpenAiError;
use super::AuthOk;
use crate::agent::{collect_agent_turn, Agent, AgentEvent, AgentTurnOutcome};
use crate::config::AppConfig;
use crate::providers::Usage;
use crate::tools::ToolRegistry;
use crate::types::{ContentBlock, Message, Role};

// ── request shape ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// thClaws extension: opt the request into fire-and-forget + final
    /// webhook callback. If present (and valid), the handler returns
    /// 202 Accepted immediately and the run continues in the background;
    /// when done, thClaws POSTs the final result to `x_callback.url`.
    /// Absent ⇒ behaviour unchanged (sync or SSE per `stream`).
    /// See `dev-plan/23-thclaws-async-callback.md`.
    #[serde(default)]
    pub x_callback: Option<XCallback>,
    // Unknown fields silently ignored (matches OpenAI tolerance).
}

/// Webhook delivery target carried in a chat-completions request body.
/// All three fields are required when the extension is used; missing or
/// empty values fail validation with 400.
///
/// thClaws is OPAQUE to `api_key` and `run_id` — it forwards them
/// verbatim to the receiver (Authorization Bearer header + JSON body)
/// and never inspects them. The receiver mints+verifies these on its
/// side (the recommended pattern is a short-lived JWT with `run_id` in
/// claims so spoofing one field invalidates the rest).
#[derive(Deserialize, Clone, Debug)]
pub struct XCallback {
    pub url: String,
    pub api_key: String,
    pub run_id: String,
    /// Optional explicit idempotency key. Defaults to `run_id` if absent.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Deserialize)]
pub struct ChatMessage {
    pub role: String,
    /// OpenAI allows content to be either a string or an array of
    /// content parts (for vision). For v1 we accept only the string
    /// form; array form is logged + degraded to "".
    #[serde(default)]
    pub content: Option<ChatContent>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum ChatContent {
    Text(String),
    Parts(Vec<serde_json::Value>),
}

impl ChatContent {
    fn as_text(&self) -> String {
        match self {
            ChatContent::Text(s) => s.clone(),
            ChatContent::Parts(parts) => {
                // Best-effort flatten of OpenAI content-parts. Pull every
                // `{ "type": "text", "text": "..." }`; ignore image / audio
                // parts entirely (no vision in v1).
                parts
                    .iter()
                    .filter_map(|p| {
                        let obj = p.as_object()?;
                        match obj.get("type").and_then(|t| t.as_str())? {
                            "text" => obj.get("text").and_then(|t| t.as_str()).map(String::from),
                            _ => None,
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    }
}

// ── response shape ────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ChatResponse {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: UsageRow,
}

#[derive(Serialize)]
pub struct Choice {
    pub index: u32,
    pub message: AssistantMessage,
    pub finish_reason: String,
}

#[derive(Serialize)]
pub struct AssistantMessage {
    pub role: &'static str,
    pub content: String,
}

#[derive(Serialize)]
pub struct UsageRow {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    // dev-plan/24: extra token-type counts for cost computation by
    // downstream consumers. Each is Optional + skipped when None so
    // strict-OpenAI clients see a usage block that's a SUPERSET of
    // the standard shape (extra fields, never missing standard ones).
    // Consumers (paperclip-adapter, thcompany) compute USD locally
    // using the rate table from /v1/models. thClaws never emits cost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
    /// o1/o3 hidden reasoning tokens. INCLUDED in `completion_tokens`
    /// already per OpenAI's wire convention; surfaced separately so
    /// consumers that price reasoning at a distinct rate can.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u32>,
}

// ── handler ───────────────────────────────────────────────────────────

pub async fn chat_completions(
    _auth: AuthOk,
    Json(req): Json<ChatRequest>,
) -> Result<Response, Response> {
    // Async + webhook callback path: fire-and-forget, deliver final
    // result via POST to x_callback.url. Validation failures are the
    // only sync error mode — anything past 202 lands on the callback.
    if let Some(callback) = req.x_callback.clone() {
        return chat_completions_async(req, callback).await;
    }

    if req.stream {
        return chat_completions_stream(req).await;
    }

    let model = req.model.clone();
    let outcome = run_turn_from_messages(&req).await.map_err(|e| {
        let msg = format!("{e}");
        eprintln!("[api_v1] chat_completions failure: {msg}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(OpenAiError::server_error(msg)),
        )
            .into_response()
    })?;

    let resp = build_chat_response(model, outcome);
    Ok(Json(resp).into_response())
}

/// Validate the `x_callback` envelope, spawn the agent run on a detached
/// tokio task, return 202 immediately. The task delivers a single final
/// POST to `callback.url` with the run outcome (see `super::callback`).
async fn chat_completions_async(
    req: ChatRequest,
    callback: XCallback,
) -> Result<Response, Response> {
    let target = super::callback::CallbackTarget::from_request(&callback).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(OpenAiError::invalid_request(e, "invalid_x_callback")),
        )
            .into_response()
    })?;

    let model = req.model.clone();
    let run_id = target.run_id.clone();
    let started_at = chrono::Utc::now();

    let target_for_task = target.clone();
    let model_for_task = model.clone();
    let run_id_for_task = run_id.clone();
    // Spawn the agent loop on a detached task. If the task panics
    // (which would be a thClaws bug, not user input — the agent path
    // doesn't take untrusted closures), tokio's JoinError is the only
    // signal we'd get; await it on a separate watcher task so we at
    // least surface the failure to operators rather than silently
    // dropping the callback.
    let handle = tokio::spawn(async move {
        let outcome = run_turn_from_messages(&req).await;
        let payload = super::callback::CallbackPayload::from_outcome(
            &run_id_for_task,
            &model_for_task,
            started_at,
            outcome,
        );
        super::callback::deliver(&target_for_task, &payload).await;
    });
    let watch_run_id = run_id.clone();
    let watch_target = target.clone();
    tokio::spawn(async move {
        match handle.await {
            Ok(()) => {}
            Err(join_err) if join_err.is_panic() => {
                eprintln!(
                    "[api_v1] callback_failed run_id={} reason=task_panicked",
                    watch_run_id
                );
                // Best-effort: tell the receiver the run died so it
                // doesn't hang forever waiting for a callback.
                let payload =
                    super::callback::CallbackPayload::panic_payload(&watch_run_id, started_at);
                super::callback::deliver(&watch_target, &payload).await;
            }
            Err(join_err) => {
                eprintln!(
                    "[api_v1] callback_failed run_id={} reason=task_cancelled error=\"{join_err}\"",
                    watch_run_id
                );
            }
        }
    });

    let body = serde_json::json!({
        "run_id": run_id,
        "status": "accepted",
        "model": model,
    });
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

/// SSE response: emit OpenAI chat.completion.chunk events as the agent
/// produces text + a terminal chunk carrying finish_reason + usage,
/// then `data: [DONE]\n\n`. Format matches what the openai-python SDK
/// + LiteLLM parse on `stream=true`.
async fn chat_completions_stream(req: ChatRequest) -> Result<Response, Response> {
    // Build the agent + history up-front (any setup error becomes a
    // plain 5xx — we haven't started the SSE response yet, so we can
    // still return an error envelope cleanly).
    let model = req.model.clone();
    let (history, prompt, extra_system) = split_messages(&req.messages).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(OpenAiError::invalid_request(
                e.to_string(),
                "invalid_messages",
            )),
        )
            .into_response()
    })?;

    let agent = build_agent(&req, extra_system).map_err(|e| {
        let msg = format!("{e}");
        eprintln!("[api_v1] chat_completions_stream setup failure: {msg}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(OpenAiError::server_error(msg)),
        )
            .into_response()
    })?;
    agent.set_history(history);

    let chunk_id = format!("chatcmpl-thc-{}", short_id());
    let created = chrono::Utc::now().timestamp();

    // async-stream lets us mix initial chunks, the AgentEvent stream,
    // and a terminal chunk in one declarative generator. Each yield
    // is wrapped Result<Event, Infallible> — we never abort the SSE
    // mid-flight; errors get encoded as a delta + finish chunk.
    let model_for_stream = model.clone();
    let id_for_stream = chunk_id.clone();
    let stream = async_stream::stream! {
        // Initial chunk: signal role=assistant. The openai-python SDK
        // expects this as the first delta.
        yield Ok::<_, Infallible>(role_chunk(&id_for_stream, created, &model_for_stream));

        let mut turn = Box::pin(agent.run_turn(prompt));
        let mut final_usage: Option<Usage> = None;
        let mut final_stop: Option<String> = None;

        while let Some(ev) = turn.next().await {
            match ev {
                Ok(AgentEvent::Text(s)) => {
                    if !s.is_empty() {
                        yield Ok(content_chunk(&id_for_stream, created, &model_for_stream, &s));
                    }
                }
                Ok(AgentEvent::Done { stop_reason, usage }) => {
                    final_stop = stop_reason;
                    final_usage = Some(usage);
                }
                // Tool-use events ride alongside the normal stream as an
                // x_thclaws_tool_use extension field on otherwise-empty
                // chat.completion.chunk events. Strict-OpenAI clients
                // ignore the extra field; custom clients render the
                // tool-call lifecycle as it happens.
                Ok(AgentEvent::ToolCallStart { id, name, input }) => {
                    yield Ok(tool_use_started_chunk(
                        &id_for_stream, created, &model_for_stream,
                        &id, &name, &input,
                    ));
                }
                Ok(AgentEvent::ToolCallResult { id, name, output, .. }) => {
                    yield Ok(tool_use_completed_chunk(
                        &id_for_stream, created, &model_for_stream,
                        &id, &name, &output,
                    ));
                }
                Ok(AgentEvent::ToolCallDenied { id, name }) => {
                    yield Ok(tool_use_denied_chunk(
                        &id_for_stream, created, &model_for_stream,
                        &id, &name,
                    ));
                }
                Ok(AgentEvent::IterationStart { .. })
                | Ok(AgentEvent::Thinking(_))
                | Ok(AgentEvent::Progress(_))
                | Ok(AgentEvent::UserMessageInjected { .. }) => {}
                Err(e) => {
                    // Surface as a content delta so the consumer sees the
                    // failure inline + still gets a terminal chunk to
                    // close the stream cleanly. Don't 5xx mid-SSE — the
                    // response headers are already flushed.
                    let msg = format!("\n\n[thclaws error] {e}");
                    yield Ok(content_chunk(&id_for_stream, created, &model_for_stream, &msg));
                    final_stop = Some("error".into());
                    break;
                }
            }
        }

        let finish = map_finish_reason(final_stop.as_deref());
        let usage_row = final_usage.as_ref().map(usage_row);
        yield Ok(final_chunk(&id_for_stream, created, &model_for_stream, &finish, usage_row));

        // Standard OpenAI SSE terminator.
        yield Ok(Event::default().data("[DONE]"));
    };

    // Keep-alive sends a `:keepalive\n\n` comment every 15s so proxies
    // don't drop the connection during long agent thinks. Comments are
    // ignored by spec-compliant SSE parsers (openai-python, LiteLLM).
    let sse = Sse::new(stream).keep_alive(KeepAlive::new());
    Ok(sse.into_response())
}

// ── translation: OpenAI request → thClaws agent run ───────────────────

async fn run_turn_from_messages(req: &ChatRequest) -> crate::error::Result<AgentTurnOutcome> {
    let (history, prompt, extra_system) = split_messages(&req.messages)?;
    let agent = build_agent(req, extra_system)?;
    agent.set_history(history);
    collect_agent_turn(agent.run_turn(prompt)).await
}

/// Construct a fresh Agent with thClaws's default toolset + system
/// prompt, parameterized by the request's model + max_tokens. Shared
/// between the non-stream and SSE paths.
fn build_agent(req: &ChatRequest, extra_system: Option<String>) -> crate::error::Result<Agent> {
    // Load the user's full config (~/.config/thclaws + project) so the
    // configured default model is used when a caller (e.g. a widget's
    // server-side tool call going through our loopback) omits `model`
    // or sends `""`. Falls back to bare defaults if load fails.
    let mut config = AppConfig::load().unwrap_or_default();
    if !req.model.trim().is_empty() {
        config.model = req.model.clone();
    }
    if let Some(max) = req.max_tokens {
        config.max_tokens = max;
    }

    let provider = crate::repl::build_provider(&config)?;

    let mut tools = ToolRegistry::with_builtins();
    // Match the print-mode toolset (KMS + Memory) — these are always-on
    // there too, and keep the agent's tool surface predictable for
    // clients that don't know about MCP plugins.
    tools.register(Arc::new(crate::tools::KmsReadTool));
    tools.register(Arc::new(crate::tools::KmsSearchTool));
    tools.register(Arc::new(crate::tools::KmsWriteTool));
    tools.register(Arc::new(crate::tools::KmsAppendTool));
    tools.register(Arc::new(crate::tools::KmsDeleteTool));
    tools.register(Arc::new(crate::tools::KmsCreateTool));
    tools.register(Arc::new(crate::tools::MemoryReadTool));
    tools.register(Arc::new(crate::tools::MemoryWriteTool));
    tools.register(Arc::new(crate::tools::MemoryAppendTool));

    // System prompt: thClaws default with any client-provided system
    // message appended. Append-not-replace because the default carries
    // the tool-aware scaffolding the agent needs.
    let mut system = crate::prompts::load("system", crate::prompts::defaults::SYSTEM);
    if let Some(s) = extra_system {
        if !s.trim().is_empty() {
            system.push_str("\n\n# Client-provided context\n");
            system.push_str(s.trim());
        }
    }

    Ok(Agent::new(provider, tools, config.model.clone(), system)
        .with_max_iterations(config.max_iterations)
        .with_max_tokens(config.max_tokens))
}

// ── SSE chunk builders ────────────────────────────────────────────────

fn role_chunk(id: &str, created: i64, model: &str) -> Event {
    let body = serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant" },
            "finish_reason": null,
        }],
    });
    Event::default().data(body.to_string())
}

fn content_chunk(id: &str, created: i64, model: &str, content: &str) -> Event {
    let body = serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": { "content": content },
            "finish_reason": null,
        }],
    });
    Event::default().data(body.to_string())
}

/// Truncate large tool outputs so a single tool call can't blow up
/// the SSE stream — clients that need full output can re-run the tool
/// or call non-stream mode. Returns a {preview, truncated, total_chars}
/// object that round-trips cleanly through JSON.
const TOOL_OUTPUT_PREVIEW_LIMIT: usize = 400;

fn output_summary(out: &str) -> serde_json::Value {
    if out.len() <= TOOL_OUTPUT_PREVIEW_LIMIT {
        serde_json::json!({
            "preview": out,
            "truncated": false,
            "total_chars": out.len(),
        })
    } else {
        // Boundary-safe slice — UTF-8 can't be split mid-codepoint
        // without producing invalid JSON. Walk back to the nearest
        // char boundary at-or-before the limit.
        let mut cut = TOOL_OUTPUT_PREVIEW_LIMIT;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        serde_json::json!({
            "preview": &out[..cut],
            "truncated": true,
            "total_chars": out.len(),
        })
    }
}

/// Build the JSON body for a tool-use-started chunk. Wrapped by
/// [`tool_use_started_chunk`] into an Event; broken out so unit tests
/// can assert on the structured value directly without going through
/// the SSE body-extraction dance.
fn tool_use_started_body(
    chunk_id: &str,
    created: i64,
    model: &str,
    tool_id: &str,
    name: &str,
    input: &serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": null,
        }],
        "x_thclaws_tool_use": {
            "id": tool_id,
            "name": name,
            "status": "started",
            "input": input,
        },
    })
}

fn tool_use_started_chunk(
    chunk_id: &str,
    created: i64,
    model: &str,
    tool_id: &str,
    name: &str,
    input: &serde_json::Value,
) -> Event {
    Event::default()
        .data(tool_use_started_body(chunk_id, created, model, tool_id, name, input).to_string())
}

fn tool_use_completed_body(
    chunk_id: &str,
    created: i64,
    model: &str,
    tool_id: &str,
    name: &str,
    output: &std::result::Result<String, String>,
) -> serde_json::Value {
    let (status, output_field) = match output {
        Ok(s) => ("completed", output_summary(s)),
        Err(e) => ("error", output_summary(e)),
    };
    serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": null,
        }],
        "x_thclaws_tool_use": {
            "id": tool_id,
            "name": name,
            "status": status,
            "output": output_field,
        },
    })
}

fn tool_use_completed_chunk(
    chunk_id: &str,
    created: i64,
    model: &str,
    tool_id: &str,
    name: &str,
    output: &std::result::Result<String, String>,
) -> Event {
    Event::default()
        .data(tool_use_completed_body(chunk_id, created, model, tool_id, name, output).to_string())
}

fn tool_use_denied_body(
    chunk_id: &str,
    created: i64,
    model: &str,
    tool_id: &str,
    name: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": null,
        }],
        "x_thclaws_tool_use": {
            "id": tool_id,
            "name": name,
            "status": "denied",
        },
    })
}

fn tool_use_denied_chunk(
    chunk_id: &str,
    created: i64,
    model: &str,
    tool_id: &str,
    name: &str,
) -> Event {
    Event::default().data(tool_use_denied_body(chunk_id, created, model, tool_id, name).to_string())
}

fn final_chunk(
    id: &str,
    created: i64,
    model: &str,
    finish_reason: &str,
    usage: Option<UsageRow>,
) -> Event {
    let mut body = serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": finish_reason,
        }],
    });
    if let Some(u) = usage {
        // serde-driven so the optional cache/reasoning fields obey the
        // same skip_serializing_if rules as the struct itself — keeps
        // the JSON exactly the same shape as the non-stream response's
        // usage block, no manual divergence to maintain.
        body["usage"] = serde_json::to_value(&u).unwrap_or_else(|_| serde_json::Value::Null);
    }
    Event::default().data(body.to_string())
}

/// Split the incoming OpenAI `messages` array into:
///   (history before the last user msg, last user msg text, optional
///    extra system text accumulated from any system messages)
///
/// Returns Err if the array is empty or contains no user message.
fn split_messages(
    messages: &[ChatMessage],
) -> crate::error::Result<(Vec<Message>, String, Option<String>)> {
    if messages.is_empty() {
        return Err(crate::error::Error::Tool("messages array is empty".into()));
    }

    // Find the last user message — that's the prompt. Everything before
    // it becomes history. Anything after the last user (assistant
    // turns the client thought it received) is ignored — OpenAI requests
    // never end on assistant, so this would be a client bug we silently
    // accommodate.
    let last_user_idx = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, m)| (m.role == "user").then_some(i))
        .ok_or_else(|| crate::error::Error::Tool("no user message in request".into()))?;

    let mut extra_system: Vec<String> = Vec::new();
    let mut history: Vec<Message> = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if i == last_user_idx {
            continue;
        }
        match m.role.as_str() {
            "system" => {
                if let Some(c) = &m.content {
                    extra_system.push(c.as_text());
                }
            }
            "user" => history.push(Message {
                role: Role::User,
                content: vec![ContentBlock::text(
                    m.content.as_ref().map(|c| c.as_text()).unwrap_or_default(),
                )],
            }),
            "assistant" => history.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::text(
                    m.content.as_ref().map(|c| c.as_text()).unwrap_or_default(),
                )],
            }),
            // Unknown roles (tool, function, developer, …) are dropped
            // silently. We don't model OpenAI's tool-call exchange in
            // v1 — thClaws's tools are internal.
            _ => {}
        }
    }

    let prompt = messages[last_user_idx]
        .content
        .as_ref()
        .map(|c| c.as_text())
        .unwrap_or_default();

    let combined_system = if extra_system.is_empty() {
        None
    } else {
        Some(extra_system.join("\n\n"))
    };
    Ok((history, prompt, combined_system))
}

// ── translation: outcome → OpenAI response ────────────────────────────

fn build_chat_response(model: String, outcome: AgentTurnOutcome) -> ChatResponse {
    let usage = outcome.usage.unwrap_or_default();
    let finish_reason = map_finish_reason(outcome.stop_reason.as_deref());
    ChatResponse {
        id: format!("chatcmpl-thc-{}", short_id()),
        object: "chat.completion",
        created: chrono::Utc::now().timestamp(),
        model,
        choices: vec![Choice {
            index: 0,
            message: AssistantMessage {
                role: "assistant",
                content: outcome.text,
            },
            finish_reason,
        }],
        usage: usage_row(&usage),
    }
}

fn map_finish_reason(stop: Option<&str>) -> String {
    // OpenAI canonical values: stop / length / tool_calls / content_filter.
    // thClaws's stop_reason comes from the underlying provider (Anthropic's
    // "end_turn" / "max_tokens" / "tool_use" / OpenAI's "stop" / "length").
    // Normalize down to OpenAI's set; unknown values pass through unchanged.
    match stop {
        Some("end_turn") | Some("stop") | Some("stop_sequence") | None => "stop".into(),
        Some("max_tokens") | Some("length") => "length".into(),
        Some("tool_use") | Some("tool_calls") => "tool_calls".into(),
        Some(other) => other.into(),
    }
}

fn usage_row(u: &Usage) -> UsageRow {
    UsageRow {
        prompt_tokens: u.input_tokens,
        completion_tokens: u.output_tokens,
        total_tokens: u.input_tokens + u.output_tokens,
        cached_input_tokens: u.cache_read_input_tokens,
        cache_creation_input_tokens: u.cache_creation_input_tokens,
        reasoning_output_tokens: u.reasoning_output_tokens,
    }
}

fn short_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{ts:x}{ns:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: Some(ChatContent::Text(text.into())),
        }
    }

    #[test]
    fn split_messages_extracts_last_user_as_prompt() {
        let messages = vec![
            msg("system", "You are helpful."),
            msg("user", "first question"),
            msg("assistant", "first answer"),
            msg("user", "second question"),
        ];
        let (history, prompt, extra_system) = split_messages(&messages).unwrap();
        assert_eq!(prompt, "second question");
        assert_eq!(history.len(), 2);
        assert!(matches!(history[0].role, Role::User));
        assert!(matches!(history[1].role, Role::Assistant));
        assert_eq!(extra_system.as_deref(), Some("You are helpful."));
    }

    #[test]
    fn split_messages_rejects_empty_array() {
        assert!(split_messages(&[]).is_err());
    }

    #[test]
    fn split_messages_rejects_no_user_message() {
        let messages = vec![msg("system", "only system here")];
        assert!(split_messages(&messages).is_err());
    }

    #[test]
    fn chat_content_parts_flatten_to_text_only() {
        let parts = ChatContent::Parts(vec![
            serde_json::json!({"type": "text", "text": "first"}),
            serde_json::json!({"type": "image_url", "image_url": {"url": "..."}}),
            serde_json::json!({"type": "text", "text": "second"}),
        ]);
        assert_eq!(parts.as_text(), "first\nsecond");
    }

    #[test]
    fn map_finish_reason_normalizes_to_openai_set() {
        assert_eq!(map_finish_reason(Some("end_turn")), "stop");
        assert_eq!(map_finish_reason(Some("max_tokens")), "length");
        assert_eq!(map_finish_reason(Some("tool_use")), "tool_calls");
        assert_eq!(map_finish_reason(None), "stop");
        assert_eq!(map_finish_reason(Some("custom")), "custom");
    }

    #[test]
    fn tool_use_started_body_carries_extension_field() {
        let body = tool_use_started_body(
            "chatcmpl-thc-test",
            1000,
            "claude-haiku-4-5",
            "tu_abc",
            "Read",
            &serde_json::json!({"file_path": "/tmp/x"}),
        );
        // Standard OpenAI chunk shape preserved so strict clients can
        // still parse the chunk + ignore the extension field.
        assert_eq!(body["object"], "chat.completion.chunk");
        assert_eq!(body["model"], "claude-haiku-4-5");
        assert_eq!(body["choices"][0]["finish_reason"], serde_json::Value::Null);
        assert_eq!(body["choices"][0]["delta"], serde_json::json!({}));
        // Extension field carries the tool-use info.
        assert_eq!(body["x_thclaws_tool_use"]["id"], "tu_abc");
        assert_eq!(body["x_thclaws_tool_use"]["name"], "Read");
        assert_eq!(body["x_thclaws_tool_use"]["status"], "started");
        assert_eq!(body["x_thclaws_tool_use"]["input"]["file_path"], "/tmp/x");
    }

    #[test]
    fn tool_use_completed_body_distinguishes_ok_and_err() {
        let ok = tool_use_completed_body("id", 1, "m", "tu", "Read", &Ok("file contents".into()));
        assert_eq!(ok["x_thclaws_tool_use"]["status"], "completed");
        assert_eq!(
            ok["x_thclaws_tool_use"]["output"]["preview"],
            "file contents"
        );
        assert_eq!(ok["x_thclaws_tool_use"]["output"]["truncated"], false);

        let err =
            tool_use_completed_body("id", 1, "m", "tu", "Read", &Err("permission denied".into()));
        assert_eq!(err["x_thclaws_tool_use"]["status"], "error");
        assert_eq!(
            err["x_thclaws_tool_use"]["output"]["preview"],
            "permission denied"
        );
    }

    #[test]
    fn output_summary_truncates_large_strings_at_char_boundary() {
        let small = "short".to_string();
        let s = output_summary(&small);
        assert_eq!(s["preview"], "short");
        assert_eq!(s["truncated"], false);
        assert_eq!(s["total_chars"], 5);

        let big = "a".repeat(1000);
        let b = output_summary(&big);
        assert_eq!(
            b["preview"].as_str().unwrap().len(),
            TOOL_OUTPUT_PREVIEW_LIMIT
        );
        assert_eq!(b["truncated"], true);
        assert_eq!(b["total_chars"], 1000);

        // Multi-byte char straddling the boundary — must not panic +
        // preview must be valid UTF-8 (assertion: as_str() returns Some).
        let prefix_len = TOOL_OUTPUT_PREVIEW_LIMIT - 1;
        let mixed = format!("{}é{}", "a".repeat(prefix_len), "x".repeat(100));
        let m = output_summary(&mixed);
        let preview = m["preview"].as_str().expect("preview must be utf-8");
        assert!(preview.len() <= TOOL_OUTPUT_PREVIEW_LIMIT);
        assert_eq!(m["truncated"], true);
    }

    #[test]
    fn tool_use_denied_body_emits_status_denied() {
        let body = tool_use_denied_body("id", 1, "m", "tu", "Bash");
        assert_eq!(body["x_thclaws_tool_use"]["status"], "denied");
        assert_eq!(body["x_thclaws_tool_use"]["name"], "Bash");
        // No `input` or `output` field — those are status-specific.
        assert!(body["x_thclaws_tool_use"]["input"].is_null());
        assert!(body["x_thclaws_tool_use"]["output"].is_null());
    }
}
