//! `POST /agent/run` — thClaws-native agent endpoint.
//!
//! Where `/v1/chat/completions` is the OpenAI-compatible surface for
//! external clients (Cursor, Aider, n8n, …), `/agent/run` is the
//! agent-shaped surface for orchestrators that treat thClaws as a
//! sovereign agent peer (paperclip-adapter / thcompany). It takes an
//! explicit `workspace_dir` and runs the full skill / MCP / plugin /
//! policy bootstrap scoped to that directory — see
//! `dev-plan/25-thclaws-as-agent.md`.
//!
//! Wire shape mirrors `/v1/chat/completions` for the parts that map
//! cleanly (sync JSON, SSE stream, `x_callback` async) but emits
//! native thClaws SSE events instead of OpenAI chunks. That lets
//! orchestrators consume tool calls + skill invocations without
//! pretending they're OpenAI tokens.

use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;

use super::callback::{deliver, CallbackPayload, CallbackTarget};
use super::chat::XCallback;
use super::errors::OpenAiError;
use super::AuthOk;
use crate::agent::{collect_agent_turn, AgentEvent, AgentTurnOutcome};
use crate::agent_runtime::{build_runtime_for_workspace, validate_workspace_dir};
use crate::config::AppConfig;

// ── request shape ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AgentRunRequest {
    pub prompt: String,
    /// Absolute path to the per-agent workspace. Read-from for skill /
    /// MCP / policy discovery; read+write for any file tool the agent
    /// invokes. Validated against `THCLAWS_AGENT_WORKSPACE_ROOT` when
    /// set (see [`crate::agent_runtime::validate_workspace_dir`]).
    ///
    /// dev-plan/26 Phase B: optional. When absent or empty, the
    /// daemon falls through to its own current working directory.
    /// Freelancer-mode pods (dev-plan/26) omit the field so the pod's
    /// own `/workspace` is used. Employee-mode adapters
    /// (paperclip-adapter for `thclaws_local`, per dev-plan/25)
    /// always supply it.
    #[serde(default)]
    pub workspace_dir: Option<String>,
    /// Optional extra system prompt. Appended to the thClaws default
    /// + skill catalog — does NOT replace them.
    #[serde(default)]
    pub system: Option<String>,
    /// Optional model id override. Defaults to whatever the daemon
    /// config carries.
    #[serde(default)]
    pub model: Option<String>,
    /// Session id for multi-turn continuation. When set, the handler
    /// loads the session JSONL from
    /// `<workspace_dir>/.thclaws/sessions/<session_id>.jsonl`,
    /// hydrates the agent's history from it, runs the new turn, and
    /// persists the updated history back to the same file. When unset,
    /// a fresh session is created and the new id is returned in the
    /// response (`session_id` JSON field on sync, `session` SSE event
    /// on stream, `session_id` in the 202 ACK on async). Pass the
    /// returned id on the next call to continue the conversation.
    /// Returns 404 (`session_not_found`) if the id is supplied but no
    /// JSONL exists at that path.
    #[serde(default)]
    pub session_id: Option<String>,
    /// `true` (default) → SSE stream of native agent events.
    /// `false` → wait for completion, return one JSON result.
    /// Ignored when `x_callback` is present (async always 202s).
    #[serde(default = "default_stream")]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Fire-and-forget mode. Same envelope as
    /// `chat_completions::XCallback` — handler returns 202 ACK
    /// immediately and POSTs the terminal payload to the callback URL
    /// when the run finishes.
    #[serde(default)]
    pub x_callback: Option<XCallback>,
}

fn default_stream() -> bool {
    true
}

// ── handler ───────────────────────────────────────────────────────────

pub async fn agent_run(
    _auth: AuthOk,
    Json(req): Json<AgentRunRequest>,
) -> Result<Response, Response> {
    // Resolve workspace_dir up-front so all paths (sync/SSE/async)
    // share the same 400 surface.
    //
    // dev-plan/26 Phase B: when the caller omits workspace_dir (or
    // sends an empty string), fall through to the daemon's CWD. For
    // a freelancer-mode pod that's typically `/workspace`. The
    // existing `THCLAWS_AGENT_WORKSPACE_ROOT` gate doesn't apply to
    // the CWD fallback — operators control the daemon's CWD at
    // launch time, which IS the safety boundary.
    let workspace_dir = match req
        .workspace_dir
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(raw) => validate_workspace_dir(raw).map_err(|msg| {
            (
                StatusCode::BAD_REQUEST,
                Json(OpenAiError::invalid_request(msg, "invalid_workspace_dir")),
            )
                .into_response()
        })?,
        None => std::env::current_dir().map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(OpenAiError::server_error(format!(
                    "could not resolve daemon CWD as default workspace_dir: {e}"
                ))),
            )
                .into_response()
        })?,
    };

    if let Some(callback) = req.x_callback.clone() {
        return agent_run_async(req, workspace_dir, callback).await;
    }

    if req.stream {
        return agent_run_stream(req, workspace_dir).await;
    }

    agent_run_sync(req, workspace_dir).await
}

// ── sync (non-stream) path ────────────────────────────────────────────

async fn agent_run_sync(
    req: AgentRunRequest,
    workspace_dir: std::path::PathBuf,
) -> Result<Response, Response> {
    let model = effective_config(&req).model;
    let (session, store) = resolve_session(&workspace_dir, req.session_id.as_deref(), &model)?;
    let (outcome, session_id) = run_outcome_with_session(&req, &workspace_dir, session, &store)
        .await
        .map_err(|e| {
            let msg = format!("{e}");
            eprintln!("[api_v1] agent_run sync failure: {msg}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(OpenAiError::server_error(msg)),
            )
                .into_response()
        })?;
    let usage = outcome.usage.unwrap_or_default();
    Ok(Json(json!({
        "model": model,
        "workspace_dir": workspace_dir.display().to_string(),
        "session_id": session_id,
        "summary": outcome.text,
        "stop_reason": outcome.stop_reason,
        "iterations": outcome.iterations,
        "usage": {
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "cached_input_tokens": usage.cache_read_input_tokens,
            "cache_creation_input_tokens": usage.cache_creation_input_tokens,
            "reasoning_output_tokens": usage.reasoning_output_tokens,
        },
    }))
    .into_response())
}

// ── SSE stream path ───────────────────────────────────────────────────

async fn agent_run_stream(
    req: AgentRunRequest,
    workspace_dir: std::path::PathBuf,
) -> Result<Response, Response> {
    let config = effective_config(&req);
    let (session, store) =
        resolve_session(&workspace_dir, req.session_id.as_deref(), &config.model)?;
    let session_id_for_event = session.id.clone();
    let runtime = build_runtime_for_workspace(&config, &workspace_dir, req.system.as_deref())
        .await
        .map_err(|e| {
            let msg = format!("{e}");
            eprintln!("[api_v1] agent_run stream setup failure: {msg}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(OpenAiError::server_error(msg)),
            )
                .into_response()
        })?;
    runtime.agent.set_history(session.messages.clone());

    let model_for_stream = config.model.clone();
    let prompt = req.prompt;
    let stream = async_stream::stream! {
        // Keep the runtime alive (MCP subprocesses, skill store handle)
        // until the stream finishes. Move it into the generator body
        // so its Drop happens after the last yield.
        let runtime = runtime;
        let mut session = session;
        let store = store;

        // Surface the session id up-front so the client knows which id
        // to pass on the next call (continuation or fresh-id echo).
        yield Ok::<_, Infallible>(named_event(
            "session",
            json!({ "id": session_id_for_event }),
        ));

        let mut turn = Box::pin(runtime.agent.run_turn(prompt));
        let mut emitted_error = false;
        while let Some(ev) = turn.next().await {
            match ev {
                Ok(AgentEvent::Text(s)) => {
                    if !s.is_empty() {
                        yield Ok::<_, Infallible>(named_event("text", json!({ "delta": s })));
                    }
                }
                Ok(AgentEvent::Thinking(s)) => {
                    if !s.is_empty() {
                        yield Ok(named_event("thinking", json!({ "delta": s })));
                    }
                }
                Ok(AgentEvent::ToolCallStart { id, name, input }) => {
                    // Skill invocations are tool calls under the hood
                    // (the `Skill` tool); surface them as a distinct
                    // event so consumers don't have to special-case
                    // the tool name on every parse.
                    let event_name = if name == "Skill" { "skill_invoked" } else { "tool_use_start" };
                    yield Ok(named_event(event_name, json!({
                        "id": id,
                        "name": name,
                        "input": input,
                    })));
                }
                Ok(AgentEvent::ToolCallResult { id, name, output, .. }) => {
                    let (status, payload) = match output {
                        Ok(s) => ("ok", s),
                        Err(s) => ("error", s),
                    };
                    let event_name = if name == "Skill" { "skill_invoked_result" } else { "tool_use_result" };
                    yield Ok(named_event(event_name, json!({
                        "id": id,
                        "name": name,
                        "status": status,
                        "output": payload,
                    })));
                }
                Ok(AgentEvent::ToolCallDenied { id, name }) => {
                    yield Ok(named_event("tool_use_denied", json!({
                        "id": id,
                        "name": name,
                    })));
                }
                Ok(AgentEvent::IterationStart { .. }) => {}
                Ok(AgentEvent::Progress(_)) => {}
                Ok(AgentEvent::UserMessageInjected { text }) => {
                    yield Ok(named_event("user_message_injected", json!({ "text": text })));
                }
                Ok(AgentEvent::Done { stop_reason, usage }) => {
                    yield Ok(named_event("usage", json!({
                        "prompt_tokens": usage.input_tokens,
                        "completion_tokens": usage.output_tokens,
                        "cached_input_tokens": usage.cache_read_input_tokens,
                        "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                        "reasoning_output_tokens": usage.reasoning_output_tokens,
                    })));
                    yield Ok(named_event("result", json!({
                        "model": model_for_stream,
                        "stop_reason": stop_reason,
                    })));
                }
                Err(e) => {
                    emitted_error = true;
                    yield Ok(named_event("error", json!({
                        "message": format!("{e}"),
                    })));
                    break;
                }
            }
        }

        // Persist the turn's updated history before the terminal
        // sentinel so a client that disconnects on `[DONE]` and then
        // reconnects with the same session_id sees the just-finished
        // turn. Save errors are logged but never abort the stream —
        // the response already reached the client.
        session.sync(runtime.agent.history_snapshot());
        if let Err(e) = store.save(&mut session) {
            eprintln!("[api_v1] session save failed for {}: {e}", session.id);
        }

        if !emitted_error {
            // Terminal sentinel — clients can use this as an unambiguous
            // end-of-stream marker instead of waiting for connection close.
            yield Ok(Event::default().data("[DONE]"));
        }
    };

    let sse = Sse::new(stream).keep_alive(KeepAlive::new());
    Ok(sse.into_response())
}

// ── async (x_callback) path ───────────────────────────────────────────

async fn agent_run_async(
    req: AgentRunRequest,
    workspace_dir: std::path::PathBuf,
    callback: XCallback,
) -> Result<Response, Response> {
    let target = CallbackTarget::from_request(&callback).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(OpenAiError::invalid_request(e, "invalid_x_callback")),
        )
            .into_response()
    })?;

    // Resolve the session BEFORE the 202 so the ACK can include the
    // id. The task below takes ownership; we keep a clone of the id
    // for the response. 404 surfaces here on a stale session id rather
    // than disappearing into the callback path.
    let effective_model = effective_config(&req).model;
    let (session, store) =
        resolve_session(&workspace_dir, req.session_id.as_deref(), &effective_model)?;
    let session_id_for_ack = session.id.clone();

    let run_id = target.run_id.clone();
    let model = req.model.clone().unwrap_or_default();
    let started_at = chrono::Utc::now();

    let target_for_task = target.clone();
    let model_for_task = model.clone();
    let run_id_for_task = run_id.clone();
    let workspace_for_task = workspace_dir.clone();
    let req_for_task = req;
    let handle = tokio::spawn(async move {
        let outcome = run_outcome_with_session(&req_for_task, &workspace_for_task, session, &store)
            .await
            .map(|(o, _id)| o);
        let payload =
            CallbackPayload::from_outcome(&run_id_for_task, &model_for_task, started_at, outcome);
        deliver(&target_for_task, &payload).await;
    });

    let watch_run_id = run_id.clone();
    let watch_target = target.clone();
    tokio::spawn(async move {
        match handle.await {
            Ok(()) => {}
            Err(join_err) if join_err.is_panic() => {
                eprintln!(
                    "[api_v1] agent_run async callback_failed run_id={} reason=task_panicked",
                    watch_run_id
                );
                let payload = CallbackPayload::panic_payload(&watch_run_id, started_at);
                deliver(&watch_target, &payload).await;
            }
            Err(join_err) => {
                eprintln!(
                    "[api_v1] agent_run async callback_failed run_id={} reason=task_cancelled error=\"{join_err}\"",
                    watch_run_id
                );
            }
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "run_id": run_id,
            "session_id": session_id_for_ack,
            "status": "accepted",
            "model": model,
            "workspace_dir": workspace_dir.display().to_string(),
        })),
    )
        .into_response())
}

// ── shared internals ──────────────────────────────────────────────────

/// Resolve the session for this turn — either load an existing one
/// (when `session_id` is supplied) or mint a new one. Sessions live at
/// `<workspace_dir>/.thclaws/sessions/<id>.jsonl` (project-scoped so a
/// pod / employee instance keeps its conversation history alongside
/// the workspace it serves). 404 when an id is supplied but no file
/// exists — never silently creates a new session under the caller's
/// id, since that would mask a typo as "agent forgot everything."
fn resolve_session(
    workspace_dir: &std::path::Path,
    session_id: Option<&str>,
    model: &str,
) -> Result<(crate::session::Session, crate::session::SessionStore), Response> {
    let store_root = workspace_dir.join(".thclaws").join("sessions");
    let store = crate::session::SessionStore::new(store_root);
    match session_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => match store.load(id) {
            Ok(session) => Ok((session, store)),
            Err(e) => Err((
                StatusCode::NOT_FOUND,
                Json(OpenAiError::invalid_request(
                    format!("session '{id}' not found in workspace: {e}"),
                    "session_not_found",
                )),
            )
                .into_response()),
        },
        None => Ok((
            crate::session::Session::new(model.to_string(), workspace_dir.display().to_string()),
            store,
        )),
    }
}

/// One turn against an already-resolved session. Sets the session's
/// history onto the agent, runs the turn, then syncs new messages back
/// onto the session and persists. Save failures are logged but don't
/// fail the request — the turn already produced a response and the
/// caller has it in hand. Returns the session id the caller should
/// pass on the next call (same id either way; surfaced here so each
/// handler can echo it without re-resolving).
async fn run_outcome_with_session(
    req: &AgentRunRequest,
    workspace_dir: &std::path::Path,
    mut session: crate::session::Session,
    store: &crate::session::SessionStore,
) -> crate::error::Result<(AgentTurnOutcome, String)> {
    let config = effective_config(req);
    let runtime =
        build_runtime_for_workspace(&config, workspace_dir, req.system.as_deref()).await?;
    runtime.agent.set_history(session.messages.clone());
    let turn = runtime.agent.run_turn(req.prompt.clone());
    let outcome = collect_agent_turn(turn).await?;
    session.sync(runtime.agent.history_snapshot());
    if let Err(e) = store.save(&mut session) {
        eprintln!("[api_v1] session save failed for {}: {e}", session.id);
    }
    Ok((outcome, session.id))
}

fn effective_config(req: &AgentRunRequest) -> AppConfig {
    let mut config = AppConfig::load().unwrap_or_default();
    if let Some(m) = req.model.as_ref().filter(|s| !s.trim().is_empty()) {
        config.model = m.clone();
    }
    if let Some(max) = req.max_tokens {
        config.max_tokens = max;
    }
    let _ = req.temperature; // reserved; not all providers honor it
    config
}

fn named_event(name: &str, payload: serde_json::Value) -> Event {
    Event::default().event(name).data(payload.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_request() {
        let raw = serde_json::json!({
            "prompt": "hello",
            "workspace_dir": "/tmp/agent-1",
        });
        let req: AgentRunRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.prompt, "hello");
        assert_eq!(req.workspace_dir.as_deref(), Some("/tmp/agent-1"));
        assert!(req.stream); // default
        assert!(req.system.is_none());
        assert!(req.x_callback.is_none());
    }

    #[test]
    fn deserialize_freelancer_request_without_workspace_dir() {
        // dev-plan/26 Phase B: pods omit workspace_dir entirely and
        // the handler falls back to the daemon's CWD.
        let raw = serde_json::json!({
            "prompt": "hello",
        });
        let req: AgentRunRequest = serde_json::from_value(raw).unwrap();
        assert!(
            req.workspace_dir.is_none(),
            "missing workspace_dir should deserialize as None, not Some(\"\")"
        );
    }

    #[test]
    fn deserialize_workspace_dir_empty_string_accepted() {
        // Empty-string is treated identically to absent by the handler
        // (both fall through to CWD). Make sure deserialization itself
        // doesn't reject the empty value.
        let raw = serde_json::json!({
            "prompt": "hello",
            "workspace_dir": "",
        });
        let req: AgentRunRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.workspace_dir.as_deref(), Some(""));
    }

    #[test]
    fn deserialize_full_request_with_xcallback() {
        let raw = serde_json::json!({
            "prompt": "do work",
            "workspace_dir": "/tmp/agent-1",
            "system": "You are helpful.",
            "model": "claude-sonnet-4-6",
            "session_id": "sess-1",
            "stream": false,
            "temperature": 0.3,
            "max_tokens": 8192,
            "x_callback": {
                "url": "https://receiver.example/cb",
                "api_key": "secret",
                "run_id": "run-1"
            }
        });
        let req: AgentRunRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.model.as_deref(), Some("claude-sonnet-4-6"));
        assert!(!req.stream);
        assert_eq!(req.temperature, Some(0.3));
        let cb = req.x_callback.expect("x_callback");
        assert_eq!(cb.run_id, "run-1");
    }

    #[test]
    fn unknown_fields_silently_ignored() {
        // Matches OpenAI tolerance — forward-compat space for
        // mcp_overrides, policy_overrides, etc.
        let raw = serde_json::json!({
            "prompt": "x",
            "workspace_dir": "/tmp/a",
            "mcp_overrides": [{"name": "fs"}],
            "future_field": 42,
        });
        let req: AgentRunRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.prompt, "x");
    }

    #[tokio::test]
    async fn rejects_relative_workspace_dir_with_400() {
        // When workspace_dir is explicitly supplied, it must be absolute.
        // (The freelancer fallback to daemon CWD only kicks in when the
        // field is absent or empty — not when it's set to an invalid value.)
        let supplied = "relative/path";
        let err = crate::agent_runtime::validate_workspace_dir(supplied).unwrap_err();
        assert!(err.contains("must be absolute"), "got: {err}");
    }

    #[test]
    fn resolve_session_mints_new_when_id_absent() {
        let dir = tempfile::tempdir().unwrap();
        let (session, _store) = resolve_session(dir.path(), None, "claude-sonnet-4-6")
            .expect("fresh session creation should succeed");
        assert!(session.id.starts_with("sess-"));
        assert_eq!(session.model, "claude-sonnet-4-6");
        assert!(session.messages.is_empty());
    }

    #[test]
    fn resolve_session_loads_existing_jsonl() {
        // Seed a session JSONL on disk under <workspace>/.thclaws/sessions/
        // then resolve with that id — should return the persisted session,
        // not mint a fresh one.
        let dir = tempfile::tempdir().unwrap();
        let store_root = dir.path().join(".thclaws").join("sessions");
        std::fs::create_dir_all(&store_root).unwrap();
        let store = crate::session::SessionStore::new(store_root);
        let mut seed =
            crate::session::Session::new("claude-sonnet-4-6", dir.path().display().to_string());
        let seed_id = seed.id.clone();
        store.save(&mut seed).unwrap();

        let (session, _store) = resolve_session(dir.path(), Some(&seed_id), "claude-sonnet-4-6")
            .expect("existing session should load");
        assert_eq!(session.id, seed_id);
    }

    #[tokio::test]
    async fn resolve_session_returns_404_for_unknown_id() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_session(dir.path(), Some("sess-does-not-exist"), "gpt-4o")
            .expect_err("unknown id should not silently create a fresh session");
        // Map back through axum's Response — we only need to verify the
        // status; OpenAiError shape is covered by the chat tests.
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }
}
