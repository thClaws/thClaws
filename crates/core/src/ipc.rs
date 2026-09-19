//! Transport-agnostic IPC dispatch — handles the JSON message protocol
//! the React frontend uses to talk to the Rust engine.
//!
//! Pre-M6.36 the dispatch lived as a 1600-LOC `match` block inside
//! `gui.rs::run`'s `with_ipc_handler` closure, capturing wry-specific
//! handles (`EventLoopProxy<UserEvent>`, the wry webview, etc.). That
//! prevented sharing the dispatch with the new `--serve` (Axum + WS)
//! transport.
//!
//! M6.36 SERVE1 promotes the dispatch into [`handle_ipc`] which takes
//! an [`IpcContext`] carrying the transport-agnostic primitives:
//!
//! - [`IpcContext::shared`] — `SharedSessionHandle` (input_tx / events_tx)
//! - [`IpcContext::approver`] — `GuiApprover` so `approval_response`
//!   can resolve pending oneshots regardless of transport
//! - [`IpcContext::pending_asks`] — same for `ask_user_response`
//! - [`IpcContext::dispatch`] — closure that pushes a JSON payload to
//!   the frontend (wry: `webview.evaluate_script("__thclaws_dispatch(...)")`;
//!   web: `ws.send(Message::Text(payload))`)
//! - [`IpcContext::on_quit`] / `on_send_initial_state` / `on_zoom` —
//!   transport-specific bridges for the few non-payload events.
//!
//! Both `gui.rs` (wry) and `server.rs` (Axum/WS — to be added in SERVE2)
//! build their own `IpcContext` flavor and call [`handle_ipc`] uniformly.
//! The body of [`handle_ipc`] is identical regardless of transport.

use crate::bridge::BridgeConfig;
use crate::permissions::{
    AgentOrigin, ApprovalDecision, ApprovalRequest, ApprovalSink, GuiApprover,
};
use crate::shared_session::{SharedSessionHandle, ShellInput};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Pending `AskUserQuestion` responders, keyed by request id. The IPC
/// handler's `ask_user_response` arm pulls the matching oneshot and
/// completes it with the user's text. Same shape as the Mutex<HashMap>
/// `gui.rs::run` constructs around the `set_gui_ask_sender` plumbing.
pub type PendingAsks = Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<String>>>>;

/// Closure that pushes a JSON payload to the frontend. Wry calls
/// `webview.evaluate_script("window.__thclaws_dispatch('<payload>')")`;
/// the future WS layer calls `ws.send(Message::Text(payload))`. The
/// payload is already a complete JSON message — the dispatch is just
/// the transport.
pub type DispatchFn = Arc<dyn Fn(String) + Send + Sync>;

/// Transport-specific bridge fired when the frontend requests a quit
/// (`{"type": "app_close"}`). Wry sets `ControlFlow::Exit`; the WS
/// layer drops the connection / shuts down the server.
pub type QuitFn = Arc<dyn Fn() + Send + Sync>;

/// Transport-specific bridge fired when the frontend signals it's
/// ready (`{"type": "frontend_ready"}`). Triggers the heavyweight
/// initial-state build (provider + model + KMS list + recent sessions
/// + …) and pushes it to the frontend. Wry's impl synthesizes the
/// JSON inline in the event-loop arm; the WS layer's impl will send a
/// snapshot frame.
pub type SendInitialStateFn = Arc<dyn Fn() + Send + Sync>;

/// Transport-specific bridge fired when the frontend persists a new
/// `guiScale` value (`{"type": "gui_set_zoom"}`). Wry calls
/// `webview.zoom(scale)`; the WS layer forwards the scale to the
/// client (the browser's CSS zoom handles the rest).
pub type ZoomFn = Arc<dyn Fn(f64) + Send + Sync>;

/// dev-plan/42: resolve the session store for IPC session ops. In
/// multiuser `--serve` the handle carries per-user `session_roots`, so
/// session list/rename/delete must hit THAT user's `sessions_dir` — not
/// `SessionStore::default_path()` (process-cwd-relative = the owner's
/// shared `/workspace/.thclaws/sessions/`, which leaked every user the
/// owner's sessions and made the listed ids unloadable by the per-user
/// worker). Falls back to the default path for single-tenant.
fn ipc_session_store(ctx: &IpcContext) -> Option<crate::session::SessionStore> {
    ctx.shared
        .session_roots
        .as_ref()
        .map(|r| crate::session::SessionStore::new(r.sessions_dir.clone()))
        .or_else(|| {
            crate::session::SessionStore::default_path().map(crate::session::SessionStore::new)
        })
}

/// Heartbeat schedule id, scoped per workspace: the ScheduleStore is a
/// GLOBAL file, so a bare "heartbeat" id would collide across
/// workspaces (and one workspace's shell could clobber another's).
fn heartbeat_id_for_workspace() -> String {
    use std::hash::{Hash, Hasher};
    let cwd = crate::workdir::current_workdir();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    cwd.hash(&mut h);
    format!("heartbeat-{:08x}", (h.finish() & 0xffff_ffff) as u32)
}

/// Per-user-aware memory store for the IPC thread. Runs OFF the worker's
/// task-local scope, so it can't rely on `current_workdir()` — it takes
/// the per-user `workspace_root` from `session_roots` (multi-tenant
/// "workspace per user") and roots memory at `<workspace_root>/.thclaws/
/// memory`, matching where the agent's turn resolves it. Falls back to
/// `MemoryStore::default_path()` for single-tenant / desktop.
fn ipc_memory_store(ctx: &IpcContext) -> Option<crate::memory::MemoryStore> {
    if let Some(ws) = ctx
        .shared
        .session_roots
        .as_ref()
        .and_then(|r| r.workspace_root.as_ref())
    {
        return Some(crate::memory::MemoryStore::new(
            ws.join(".thclaws").join("memory"),
        ));
    }
    crate::memory::MemoryStore::default_path().map(crate::memory::MemoryStore::new)
}

/// Everything the IPC dispatch needs from its surrounding transport.
/// Construct one per session in the transport's setup; pass `&` to
/// [`handle_ipc`] for each inbound message.
#[derive(Clone)]
pub struct IpcContext {
    /// `true` for cloud `--serve` mode (no desktop wry window). Used
    /// by `get_cwd` to skip the workspace-folder modal — the cloud
    /// engine's cwd is fixed at `/workspace` by the runner template;
    /// the desktop GUI lets the user pick at startup.
    pub is_serve_mode: bool,
    pub shared: Arc<SharedSessionHandle>,
    pub approver: Arc<GuiApprover>,
    pub pending_asks: PendingAsks,
    pub dispatch: DispatchFn,
    pub on_quit: QuitFn,
    pub on_send_initial_state: SendInitialStateFn,
    pub on_zoom: ZoomFn,
    /// dev-plan/32 Tier 3 workflow review approver. The
    /// `workflow_decision` IPC message looks up pending requests by
    /// `id` and resolves the matching oneshot, the same way the
    /// tool-call approver resolves `approval_response`.
    pub workflow_approver: Arc<crate::workflow::WorkflowApprover>,
}

/// Strip a single pair of wrapping `"…"` or `'…'` quotes from `s` if
/// present. Used to normalise pasted API keys at the `api_key_set`
/// boundary — copy-paste from a `.env` file / shell `export` line
/// often includes the surrounding quotes verbatim, and a key like
/// `"sk-or-v1-…"` becomes `Authorization: Bearer "sk-or-v1-…"` on
/// the wire, which OpenRouter rejects as `Missing Authentication
/// header` (issue #145).
fn strip_wrapping_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Whether the installed shell `shell_id` declares `perm` in its
/// manifest. Used to enforce the `model.read` / `model.write` flags on
/// the `gui_shell_model_*` arms (a shell can't switch the app model
/// without explicitly asking for it).
fn shell_has_permission(shell_id: &str, perm: &str) -> bool {
    if shell_id.is_empty() {
        return false;
    }
    crate::gui_shell::ShellRegistry::new()
        .resolve(shell_id)
        .map(|s| s.manifest().permissions.iter().any(|p| p == perm))
        .unwrap_or(false)
}

/// Serialize a session's messages into the flat `{role, text}` shape a
/// chat-style GUI Shell renders (dev-plan/33 sessions bridge). User →
/// "user", Assistant → "bot"; System and empty/tool-only turns are
/// dropped (they don't render as chat bubbles).
/// Shell-shaped history with each turn's stored usage footer dropped
/// back in as a `usage` row. A shell shows those footers live, so a
/// reopened chat that omits them is missing information the user had a
/// moment ago.
fn serialize_shell_history_with_usage(session: &crate::session::Session) -> Vec<serde_json::Value> {
    if session.turn_usage.is_empty() {
        return serialize_shell_history(&session.messages);
    }
    let mut out = Vec::new();
    let mut next = 0usize;
    for (i, _) in session.messages.iter().enumerate() {
        out.extend(serialize_shell_history(&session.messages[i..=i]));
        while next < session.turn_usage.len() && session.turn_usage[next].after == i + 1 {
            out.push(serde_json::json!({
                "role": "usage",
                "text": session.turn_usage[next].text,
            }));
            next += 1;
        }
    }
    for u in &session.turn_usage[next..] {
        out.push(serde_json::json!({ "role": "usage", "text": u.text }));
    }
    out
}

fn serialize_shell_history(messages: &[crate::types::Message]) -> Vec<serde_json::Value> {
    use crate::types::Role;
    messages
        .iter()
        .filter_map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "bot",
                Role::System => return None,
            };
            let text = m
                .content
                .iter()
                .filter_map(|c| match c {
                    crate::types::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            Some(serde_json::json!({ "role": role, "text": trimmed }))
        })
        .collect()
}

/// Connect or detach every MCP server a plugin contributes, so
/// enabling / disabling / removing one takes effect in the running
/// session instead of at the next restart. Skills, commands and agents
/// are read when the session builds its registry and are NOT covered —
/// callers should say so.
fn apply_plugin_mcp_state(plugin: &crate::plugins::Plugin, enabled: bool, ctx: &IpcContext) {
    let Ok(manifest) = plugin.manifest() else {
        return;
    };
    for (name, entry) in &manifest.mcp_servers {
        let input = if enabled {
            crate::shared_session::ShellInput::McpConnect(Box::new(entry.to_config(name)))
        } else {
            crate::shared_session::ShellInput::McpDisconnect {
                server_name: name.clone(),
            }
        };
        let _ = ctx.shared.input_tx.send(input);
    }
}

/// Names of the MCP servers configured in one scope's `mcp.json`
/// (`~/.config/thclaws/mcp.json` for user, `.thclaws/mcp.json` for
/// project). `AppConfig` merges both and forgets which file each came
/// from, but a Connectors surface has to show it — a user-scope
/// connector is shared by every project on the machine.
fn mcp_server_names_in_scope(user: bool) -> std::collections::HashSet<String> {
    let Some(path) = crate::config::mcp_config_path_for(user) else {
        return Default::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("mcpServers").and_then(|m| m.as_object()).cloned())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// Write `key` for `provider` into whichever secret backend the user
/// chose. Returns `(ok, error, storage)` — the same triple the
/// `api_key_set` reply carries. Split out so the GUI-Shell BYOK path
/// (`gui_shell_key_set`) stores keys through the exact same rules
/// instead of a parallel implementation that could drift.
fn store_provider_key(provider: &str, key: &str) -> (bool, String, &'static str) {
    if provider.is_empty() || key.is_empty() {
        (false, "provider and key are required".to_string(), "")
    } else {
        let env_var = crate::providers::ProviderKind::from_name(provider)
            .and_then(|k| k.api_key_env())
            .or_else(|| crate::secrets::service_env_var(provider));
        let backend = crate::secrets::get_backend().unwrap_or(crate::secrets::Backend::Keychain);
        match backend {
            crate::secrets::Backend::Keychain => match crate::secrets::set(provider, key) {
                Ok(()) => {
                    if let Some(var) = env_var {
                        std::env::set_var(var, key);
                    }
                    (true, String::new(), "keychain")
                }
                Err(e) => (false, format!("keychain failed: {e}"), ""),
            },
            crate::secrets::Backend::Dotenv => match env_var {
                Some(var) => match crate::dotenv::upsert_user_env(var, key) {
                    Ok(_) => {
                        std::env::set_var(var, key);
                        (true, String::new(), "dotenv")
                    }
                    Err(e) => (false, format!(".env write failed: {e}"), ""),
                },
                None => (false, format!("provider '{provider}' has no env var"), ""),
            },
        }
    }
}

/// Everything that must happen once a key write has been attempted:
/// tell the app, and on success auto-select a model for the newly
/// usable provider, broadcast the provider change, offer the model
/// picker, and reload the running session's config. Shared by
/// `api_key_set` and the GUI-Shell BYOK path so a key added from a
/// shell leaves the app in the same state as one added from Settings.
fn announce_key_stored(provider: &str, ok: bool, error: &str, storage: &str, ctx: &IpcContext) {
    let error = error.to_string();
    let payload = serde_json::json!({
        "type": "api_key_result",
        "action": "set",
        "provider": provider,
        "ok": ok,
        "error": error,
        "storage": storage,
    });
    (ctx.dispatch)(payload.to_string());
    // Post-key switch + model picker. Switch to the provider whose key was
    // just stored — that's what the user is telling us they want to use.
    // (This previously called `auto_fallback_model`, which only ever
    // returns a local runtime, so pasting an OpenAI key moved the session
    // to Ollama.) Only fires when the active model can't be reached
    // anyway, so a working setup is never disturbed.
    if ok {
        let cfg = crate::config::AppConfig::load().unwrap_or_default();
        let stored_kind = crate::providers::ProviderKind::ALL
            .iter()
            .copied()
            .find(|k| k.name() == provider);
        let switch_to = stored_kind.filter(|_| !crate::providers::provider_has_credentials(&cfg));
        if let Some(new_model) = switch_to.map(|k| k.default_model().to_string()) {
            let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
            project.set_model(&new_model);
            let _ = project.save();
            let new_cfg = crate::config::AppConfig::load().unwrap_or_default();
            let provider_name = new_cfg.detect_provider().unwrap_or("unknown");
            let ready = crate::providers::provider_has_credentials(&new_cfg);
            let broadcast = serde_json::json!({
                "type": "provider_update",
                "provider": provider_name,
                "model": new_cfg.model,
                "thinking": crate::providers::ThinkingLevel::json(new_cfg.thinking_budget),
                "provider_ready": ready,
            });
            (ctx.dispatch)(broadcast.to_string());
            // Off-thread: the builder lists an endpoint-owned provider
            // (LiteLLM / Ollama / …) over the network, and `handle_ipc` is
            // sync. Same shape as the `request_all_models` arm.
            let dispatch = ctx.dispatch.clone();
            tokio::spawn(async move {
                if let Some(picker) =
                    crate::providers::build_model_picker_payload(&new_cfg, 3).await
                {
                    dispatch(picker);
                }
            });
        } else {
            let provider_name = cfg.detect_provider().unwrap_or("unknown");
            let ready = crate::providers::provider_has_credentials(&cfg);
            let broadcast = serde_json::json!({
                "type": "provider_update",
                "provider": provider_name,
                "model": cfg.model,
                "thinking": crate::providers::ThinkingLevel::json(cfg.thinking_budget),
                "provider_ready": ready,
            });
            (ctx.dispatch)(broadcast.to_string());
        }
        let _ = ctx
            .shared
            .input_tx
            .send(crate::shared_session::ShellInput::ReloadConfig);
    }
}

/// Build the correlated `gui_shell_event` reply envelope for a sessions
/// bridge call (either `result` or `error`).
fn shell_reply(request_id: u64, body: serde_json::Value) -> String {
    let mut ev = serde_json::json!({ "type": "gui_shell_event", "replyTo": request_id });
    if let Some(obj) = ev.as_object_mut() {
        if let Some(bobj) = body.as_object() {
            for (k, v) in bobj {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    ev.to_string()
}

/// dev-plan/39 Tier 3: is `tool_name` invokable by `shell_id` per its
/// manifest? A shell opts INTO tool gating by declaring any
/// `tools.invoke:<tool>` permission — then only the declared tools (or
/// the `tools.invoke:*` wildcard) are allowed. A shell that declares NO
/// `tools.invoke:*` permission runs in legacy/unfettered mode (built-in
/// + hand-installed dev shells that predate the scheme), and an
/// unresolvable shell id is left unchanged too — so this is additive and
/// never breaks existing shells. Marketplace shells are pushed to declare
/// at publish time, not here.
fn shell_tool_invoke_allowed(shell_id: &str, tool_name: &str) -> bool {
    let Some(shell) = crate::gui_shell::ShellRegistry::new().resolve(shell_id) else {
        return true; // unknown shell → legacy behaviour, unchanged
    };
    tool_allowed_by_perms(&shell.manifest().permissions, tool_name)
}

/// Decode a base64 blob, write it into `<base>/_uploads/<unique>`, and
/// return `{path, url}` (dev-plan/39 Tier 3 uploadFile). Pure over an
/// explicit base dir so it's testable without cwd/session state.
fn shell_upload_into(
    base: &std::path::Path,
    name: &str,
    data_b64: &str,
) -> std::result::Result<serde_json::Value, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64.as_bytes())
        .map_err(|e| format!("invalid base64: {e}"))?;
    if bytes.len() as u64 > crate::uploads::UPLOAD_MAX_BYTES {
        return Err(format!(
            "{} exceeds {}-byte upload cap",
            name,
            crate::uploads::UPLOAD_MAX_BYTES
        ));
    }
    let dir = crate::uploads::ensure_uploads_dir(base)
        .map_err(|e| format!("cannot use uploads dir: {e}"))?;
    let dest = crate::uploads::unique_path(&dir, name);
    std::fs::write(&dest, &bytes).map_err(|e| format!("write {}: {e}", dest.display()))?;
    let rel = dest
        .strip_prefix(base)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| format!("{}/{name}", crate::uploads::UPLOADS_DIRNAME));
    Ok(serde_json::json!({
        "path": rel,
        // Relative so it resolves under the shell's base URL in both
        // Mode A (custom protocol) and Mode B (/t/<token>/).
        "url": format!("file-asset/{rel}"),
    }))
}

/// Pure allowlist check (testable without a registry): a `tools.invoke:*`
/// wildcard or the exact `tools.invoke:<tool>` grants it; declaring no
/// `tools.invoke:` permission at all = legacy/unfettered (allow).
fn tool_allowed_by_perms(perms: &[String], tool_name: &str) -> bool {
    let declared: Vec<&str> = perms
        .iter()
        .filter_map(|p| p.strip_prefix("tools.invoke:"))
        .collect();
    if declared.is_empty() {
        return true;
    }
    declared.iter().any(|d| *d == "*" || *d == tool_name)
}

/// Serialise a research `JobView` for the `thclaws.research.*` bridge
/// API (JobStatus/SystemTime aren't Serialize, so map by hand).
fn job_view_json(v: &crate::research::JobView) -> serde_json::Value {
    serde_json::json!({
        "id": v.id,
        "query": v.query,
        "status": v.status.as_str(),
        "phase": v.phase,
        "iterations_done": v.iterations_done,
        "source_count": v.source_count,
        "score": v.last_score,
        "kms_target": v.kms_target,
        "result_page": v.result_page,
        "error": v.error,
    })
}

/// Dispatch a single inbound IPC message. Routes by `msg.type` to one
/// of ~70 message-type arms (see the body for the full inventory).
///
/// Returns `true` if the message was recognized and dispatched, `false`
/// if `ty` didn't match any migrated arm. This lets the wry GUI's
/// closure fall through to its own (still-unmigrated) match for any
/// `false` return — incremental SERVE9 migration moves arms from
/// gui.rs to here over time, with the bool signal serving as the
/// hand-off contract until the migration completes.
///
/// The WebSocket transport ignores the return value: anything not
/// handled here is silently dropped (the WS-side dispatch surface IS
/// `handle_ipc` — there's no fallback closure to delegate to).
#[must_use = "callers must consult the returned bool to decide whether to fall through to a transport-specific dispatch"]
/// Workspace-relative paths of the per-slide PNGs beside a rendered
/// `deck.pdf`, for the Files tab's one-slide-at-a-time deck viewer.
fn rel_slide_pngs(workspace: &std::path::Path, pdf: &std::path::Path) -> Vec<String> {
    crate::tools::slide_render::slide_pngs(pdf)
        .iter()
        .map(|p| {
            p.strip_prefix(workspace)
                .unwrap_or(p.as_path())
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

fn draft_session(id: &str, ctx: &IpcContext) -> Option<crate::session::Session> {
    uuid::Uuid::parse_str(id.strip_prefix("draft-")?).ok()?;
    let model = crate::config::AppConfig::load()
        .map(|c| c.model)
        .unwrap_or_default();
    let mut session = crate::session::Session::new_detached(
        model,
        ctx.shared
            .session_roots
            .as_ref()
            .and_then(|r| r.workspace_root.clone())
            .unwrap_or_else(crate::workdir::current_workdir)
            .to_string_lossy(),
    );
    session.id = id.to_string();
    Some(session)
}

pub(crate) fn parse_session_attachments(
    msg: &Value,
) -> Result<Vec<(String, String)>, &'static str> {
    let Some(items) = msg.get("attachments").and_then(Value::as_array) else {
        return Ok(vec![]);
    };
    if items.len() > 10 {
        return Err("At most 10 attachments are allowed. Remove some images and send again.");
    }
    let mut total = 0usize;
    let mut images = Vec::new();
    for item in items {
        let media = item
            .get("mediaType")
            .and_then(Value::as_str)
            .ok_or("Invalid attachment mediaType.")?;
        let data = item
            .get("data")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or("Invalid or empty attachment data.")?;
        total = total.saturating_add(data.len());
        if total > 67 * 1024 * 1024 {
            return Err("Attachments exceed the 67 MiB base64-encoded payload limit. Reduce their size and send again.");
        }
        images.push((media.to_owned(), data.to_owned()));
    }
    Ok(images)
}

pub fn handle_ipc(msg: Value, ctx: &IpcContext) -> bool {
    let ty = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let target = msg
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let input = matches!(ty, "shell_input" | "chat_prompt" | "pty_write");
    let control = ty.starts_with("plan_")
        || ty.starts_with("goal_")
        || matches!(
            ty,
            "shell_cancel"
                | "user_input_inject"
                | "approval_response"
                | "ask_user_response"
                | "workflow_decision"
        );
    if let Some(target) = target {
        let busy = crate::agent_activity::busy_meta();
        let execution = ctx
            .shared
            .execution_session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let active = busy
            .as_ref()
            .map(|m| m.session_id.as_str())
            .unwrap_or(&execution);
        if target != active && (control || (input && busy.is_some())) {
            (ctx.dispatch)(serde_json::json!({"type":"session_action_rejected", "session_id":target, "text":format!("Session {active} is the execution session. Return to it to control the task; wait for it to finish before sending here.")}).to_string());
            return true;
        }
    }
    match ty {
        "app_close" => {
            (ctx.on_quit)();
        }

        // M6.36 SERVE3: minimum-viable WS dispatch surface — just
        // enough that a browser can send a message and observe events
        // come back. Wry continues handling the rich path
        // (image attachments via `LineWithImages`) — when this arm
        // detects attachments, it returns false so wry falls through
        // to its own richer handler. Web doesn't paste images today.
        "shell_input" | "chat_prompt" | "pty_write" => {
            let has_attachments = msg
                .get("attachments")
                .and_then(|v| v.as_array())
                .map(|arr| !arr.is_empty())
                .unwrap_or(false);
            if has_attachments && target.is_none() {
                // Defer to wry's rich handler so attachments aren't
                // silently dropped. Web users hit only the plain-text
                // path (no image-paste in browser yet).
                let _ = (&ctx.pending_asks, &ctx.dispatch, &ctx.on_zoom);
                return false;
            }
            let line = msg
                .get("text")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
            if trimmed.is_empty() && !has_attachments {
                return true;
            }
            // A browser closes its connection; it cannot answer a native quit dialog.
            if ctx.is_serve_mode
                && matches!(
                    trimmed.trim().to_ascii_lowercase().as_str(),
                    "/quit" | "/exit" | "/q"
                )
            {
                (ctx.on_quit)();
                return true;
            }
            // dev-plan/32 Tier 3 Terminal-tab approval intercept. The
            // worker loop is blocked inside `dispatch_workflow_run`'s
            // `.await` on the WorkflowApprover's oneshot — any text
            // queued through `input_tx` waits forever until the
            // review resolves. Catch typed decisions here at the IPC
            // boundary so they reach the approver directly. The same
            // parser also runs at the top of `handle_line` as a
            // safety net for non-IPC input paths (e.g. /loop body
            // re-fires).
            let pending = ctx.workflow_approver.pending_ids();
            if !pending.is_empty() {
                match crate::workflow::parse_chat_decision(&trimmed) {
                    Some(decision) => {
                        if let Some(id) = pending.into_iter().next_back() {
                            ctx.workflow_approver.resolve(&id, decision);
                        }
                        return true;
                    }
                    None => {
                        let _ = ctx.shared.events_tx.send(
                            crate::shared_session::ViewEvent::SlashOutput(
                                "workflow review pending — type `approve`, `cancel`, or \
                                 `rework: <note>` (or click in the Chat tab)"
                                    .to_string(),
                            ),
                        );
                        return true;
                    }
                }
            }
            let input = if let Some(id) = target {
                let images = match parse_session_attachments(&msg) {
                    Ok(images) => images,
                    Err(error) => {
                        (ctx.dispatch)(serde_json::json!({"type":"session_action_rejected", "session_id":id, "text":error}).to_string());
                        return true;
                    }
                };
                if let Some(session) = draft_session(id, ctx) {
                    let result = ipc_session_store(ctx)
                        .ok_or_else(|| "No session store".to_string())
                        .and_then(|store| {
                            session
                                .write_header_if_missing(&store.path_for(id))
                                .map_err(|e| e.to_string())
                        });
                    if let Err(error) = result {
                        (ctx.dispatch)(serde_json::json!({"type":"session_action_rejected", "session_id":id, "text":error}).to_string());
                        return true;
                    }
                }
                ShellInput::SessionInput {
                    id: id.to_string(),
                    text: trimmed,
                    images,
                }
            } else {
                ShellInput::Line(trimmed)
            };
            let _ = ctx.shared.input_tx.send(input);
        }

        "frontend_ready" => {
            // Wry: just signal the ready_gate (idempotent).
            // WS: also fire on_send_initial_state so the frontend gets
            // its initial snapshot. The wry path's send_event arm
            // synthesises the same JSON via gui.rs's event-loop.
            ctx.shared.ready_gate.signal();
            // The worker may have activated before this client subscribed.
            // Replay its identity and busy state on every handshake.
            let id = ctx
                .shared
                .execution_session_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if !id.is_empty() {
                (ctx.dispatch)(
                    serde_json::json!({"type":"session_execution","session_id":id}).to_string(),
                );
            }
            for frame in crate::event_render::render_chat_dispatches(
                &crate::shared_session::ViewEvent::BusyChanged(crate::agent_activity::busy_meta()),
            ) {
                (ctx.dispatch)(frame);
            }
            (ctx.on_send_initial_state)();
        }

        "approval_response" => {
            let id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let decision_str = msg
                .get("decision")
                .and_then(|v| v.as_str())
                .unwrap_or("deny");
            let decision = match decision_str {
                "allow" => crate::permissions::ApprovalDecision::Allow,
                "allow_for_session" => crate::permissions::ApprovalDecision::AllowForSession,
                _ => crate::permissions::ApprovalDecision::Deny,
            };
            if let Some(target) = target {
                if ctx.approver.unresolved_requests().iter().any(|r| {
                    r.id == id && r.session_id.as_deref().is_some_and(|owner| owner != target)
                }) {
                    return true;
                }
            }
            ctx.approver.resolve(id, decision);
        }

        // dev-plan/32 Tier 3 workflow approval response. Frontend posts
        // `{type: "workflow_decision", id, decision: "approve" |
        // "cancel" | "rework", note?}` when the user clicks a button
        // on the review bubble; we route it to the matching pending
        // oneshot inside WorkflowApprover.
        "workflow_decision" => {
            let id = msg
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let decision_str = msg
                .get("decision")
                .and_then(|v| v.as_str())
                .unwrap_or("cancel");
            let decision = match decision_str {
                "approve" => crate::workflow::WorkflowDecision::Approve,
                "rework" => {
                    let note = msg
                        .get("note")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    crate::workflow::WorkflowDecision::Rework(note)
                }
                _ => crate::workflow::WorkflowDecision::Cancel,
            };
            ctx.workflow_approver.resolve(&id, decision);
        }

        "shell_cancel" => {
            // Worker observes ctrl-C / cancel via the cancel token.
            ctx.shared.request_cancel();
        }

        // GUI Shell (dev-plan/33 Tier 1) — same input/cancel plumbing as
        // shell_input / shell_cancel above, but framed as a separate IPC
        // type so the bridge runtime's request/response correlator can
        // round-trip a `runId` back to the shell's JS through the
        // gui_shell_event dispatch. Per-shell session isolation is Tier 2;
        // Tier 1 routes through the shared session, which means the Chat
        // tab will also see the shell's conversation. Documented limit.
        "gui_shell_run" => {
            let prompt = msg
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if !prompt.is_empty() {
                // `isolated: true` (from `streamTurn(prompt, {isolated:true})`)
                // runs the turn on a throwaway child agent with empty history
                // that's discarded afterwards — so a GUI-shell agent firing
                // many one-shot generations (e.g. one per slide) never grows
                // the shared session's history or its per-turn input tokens.
                let isolated = msg
                    .get("isolated")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let _ = ctx.shared.input_tx.send(if isolated {
                    ShellInput::IsolatedLine(prompt)
                } else {
                    ShellInput::Line(prompt)
                });
            }
            // Reply so the bridge's Promise resolves. Tier 1 echoes the
            // request id as a placeholder runId — multi-run correlation
            // (cancelling a specific in-flight run) lands in Tier 2.
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "gui_shell_event",
                    "sessionId": session_id,
                    "replyTo": request_id,
                    "result": { "runId": format!("run-{request_id}") },
                })
                .to_string(),
            );
        }

        "gui_shell_cancel" => {
            ctx.shared.request_cancel();
        }

        // GUI Shell memory bridge (dev-plan/33 — settings "Memory" panel).
        // Core memory = the `MEMORY.md` index the agent injects into every
        // turn. get reads it; set overwrites it (the user editing what the
        // agent remembers). Gated by memory.read / memory.write.
        "gui_shell_memory_get" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let body = if !shell_has_permission(shell_id, "memory.read")
                && !shell_has_permission(shell_id, "memory.write")
            {
                serde_json::json!({ "error": "permission 'memory.read' not declared" })
            } else {
                let core = ipc_memory_store(ctx)
                    .and_then(|s| s.index())
                    .unwrap_or_default();
                serde_json::json!({ "result": { "core": core } })
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        "gui_shell_memory_set" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let text = msg
                .get("core")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let body = if !shell_has_permission(shell_id, "memory.write") {
                serde_json::json!({ "error": "permission 'memory.write' not declared" })
            } else {
                match ipc_memory_store(ctx) {
                    Some(store) => {
                        let path = store.root.join("MEMORY.md");
                        match std::fs::create_dir_all(&store.root)
                            .and_then(|_| std::fs::write(&path, &text))
                        {
                            Ok(_) => serde_json::json!({ "result": { "ok": true } }),
                            Err(e) => serde_json::json!({ "error": e.to_string() }),
                        }
                    }
                    None => serde_json::json!({ "error": "no memory store" }),
                }
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        // GUI Shell schedule bridge (settings "Schedule" + "Heartbeat"
        // panels). Reuses the ScheduleStore (schedules.json) the /schedule
        // command + scheduler daemon already run. Heartbeat is a reserved
        // schedule id ("heartbeat") with a fixed review-memory prompt so it
        // needs no new engine machinery.
        "gui_shell_schedule_list" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let body = if !shell_has_permission(shell_id, "schedule.read")
                && !shell_has_permission(shell_id, "schedule.write")
            {
                serde_json::json!({ "error": "permission 'schedule.read' not declared" })
            } else {
                match crate::schedule::ScheduleStore::load() {
                    Ok(store) => {
                        let ws_cwd = crate::workdir::current_workdir();
                        let arr: Vec<serde_json::Value> = store
                            .schedules
                            .iter()
                            .filter(|s| s.cwd == ws_cwd && !s.id.starts_with("heartbeat"))
                            .map(|s| {
                                serde_json::json!({
                                    "id": s.id,
                                    "cron": s.cron,
                                    "runAt": s.run_at,
                                    "prompt": s.prompt,
                                    "enabled": s.enabled,
                                    "lastRun": s.last_run,
                                })
                            })
                            .collect();
                        serde_json::json!({ "result": { "schedules": arr } })
                    }
                    Err(e) => serde_json::json!({ "error": e.to_string() }),
                }
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        "gui_shell_schedule_create" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let prompt = msg
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let cron = msg
                .get("cron")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let body = if !shell_has_permission(shell_id, "schedule.write") {
                serde_json::json!({ "error": "permission 'schedule.write' not declared" })
            } else if prompt.is_empty() || cron.is_empty() {
                serde_json::json!({ "error": "prompt and cron are required" })
            } else if let Err(e) = crate::schedule::validate_cron(&cron) {
                serde_json::json!({ "error": format!("invalid cron: {e}") })
            } else {
                let sched = crate::schedule::Schedule {
                    id: format!(
                        "sch-{:x}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0)
                    ),
                    cron,
                    run_at: None,
                    cwd: crate::workdir::current_workdir(),
                    prompt,
                    model: None,
                    resume_session: None,
                    max_iterations: None,
                    timeout_secs: None,
                    enabled: true,
                    watch_workspace: false,
                    last_run: None,
                    last_exit: None,
                };
                match crate::schedule::ScheduleStore::load().and_then(|mut st| {
                    st.add(sched)?;
                    st.save()
                }) {
                    Ok(()) => serde_json::json!({ "result": { "ok": true } }),
                    Err(e) => serde_json::json!({ "error": e.to_string() }),
                }
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        "gui_shell_schedule_delete" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let sid = msg
                .get("scheduleId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let body = if !shell_has_permission(shell_id, "schedule.write") {
                serde_json::json!({ "error": "permission 'schedule.write' not declared" })
            } else if sid.is_empty() {
                serde_json::json!({ "error": "scheduleId required" })
            } else {
                match crate::schedule::ScheduleStore::load().and_then(|mut st| {
                    // Workspace-scoped: a shell may only delete schedules
                    // whose cwd is THIS workspace (a scratch shell must
                    // never reach another workspace's schedules).
                    let ws_cwd = crate::workdir::current_workdir();
                    let in_ws = st.get(&sid).map(|s| s.cwd == ws_cwd).unwrap_or(false);
                    if !in_ws {
                        return Err(crate::error::Error::Tool(
                            "schedule not found in this workspace".into(),
                        ));
                    }
                    let removed = st.remove(&sid);
                    st.save()?;
                    Ok(removed)
                }) {
                    Ok(removed) => serde_json::json!({ "result": { "ok": removed } }),
                    Err(e) => serde_json::json!({ "error": e.to_string() }),
                }
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        "gui_shell_schedule_toggle" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let sid = msg
                .get("scheduleId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let enabled = msg.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            let body = if !shell_has_permission(shell_id, "schedule.write") {
                serde_json::json!({ "error": "permission 'schedule.write' not declared" })
            } else {
                match crate::schedule::ScheduleStore::load().and_then(|mut st| {
                    let ws_cwd = crate::workdir::current_workdir();
                    match st.get_mut(&sid).filter(|s| s.cwd == ws_cwd) {
                        Some(s) => {
                            s.enabled = enabled;
                            st.save()?;
                            Ok(true)
                        }
                        None => Ok(false),
                    }
                }) {
                    Ok(found) => serde_json::json!({ "result": { "ok": found } }),
                    Err(e) => serde_json::json!({ "error": e.to_string() }),
                }
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        // Heartbeat: reserved schedule id with interval presets. "off"
        // removes the entry; any interval upserts it enabled.
        "gui_shell_heartbeat_get" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let body = if !shell_has_permission(shell_id, "schedule.read")
                && !shell_has_permission(shell_id, "schedule.write")
            {
                serde_json::json!({ "error": "permission 'schedule.read' not declared" })
            } else {
                let hb_id = heartbeat_id_for_workspace();
                let interval = crate::schedule::ScheduleStore::load()
                    .ok()
                    .and_then(|st| {
                        st.get(&hb_id).filter(|s| s.enabled).map(|s| {
                            match s.cron.as_str() {
                                "*/30 * * * *" => "30m",
                                "0 * * * *" => "1h",
                                "0 */4 * * *" => "4h",
                                "0 9 * * *" => "1d",
                                _ => "custom",
                            }
                            .to_string()
                        })
                    })
                    .unwrap_or_else(|| "off".into());
                serde_json::json!({ "result": { "interval": interval } })
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        "gui_shell_heartbeat_set" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let interval = msg
                .get("interval")
                .and_then(|v| v.as_str())
                .unwrap_or("off")
                .to_string();
            let body = if !shell_has_permission(shell_id, "schedule.write") {
                serde_json::json!({ "error": "permission 'schedule.write' not declared" })
            } else if !matches!(interval.as_str(), "off" | "30m" | "1h" | "4h" | "1d") {
                serde_json::json!({ "error": format!("unknown interval '{interval}'") })
            } else {
                let cron = match interval.as_str() {
                    "30m" => Some("*/30 * * * *"),
                    "1h" => Some("0 * * * *"),
                    "4h" => Some("0 */4 * * *"),
                    "1d" => Some("0 9 * * *"),
                    _ => None, // "off"
                };
                let hb_id = heartbeat_id_for_workspace();
                match crate::schedule::ScheduleStore::load().and_then(|mut st| {
                    st.remove(&hb_id);
                    if let Some(cron) = cron {
                        st.add(crate::schedule::Schedule {
                            id: hb_id.clone(),
                            cron: cron.into(),
                            run_at: None,
                            cwd: crate::workdir::current_workdir(),
                            prompt: "Heartbeat check-in: review your core memory (MEMORY.md) \
                                     and any recent notes. If — and only if — something genuinely \
                                     deserves the user's attention right now (a due follow-up, a \
                                     promised reminder, an anomaly), write a short friendly message \
                                     about it. Otherwise reply exactly: HEARTBEAT-OK"
                                .into(),
                            model: None,
                            resume_session: None,
                            max_iterations: Some(10),
                            timeout_secs: Some(300),
                            enabled: true,
                            watch_workspace: false,
                            last_run: None,
                            last_exit: None,
                        })?;
                    }
                    st.save()
                }) {
                    Ok(()) => serde_json::json!({ "result": { "interval": interval } }),
                    Err(e) => serde_json::json!({ "error": e.to_string() }),
                }
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        // GUI Shell skills bridge (settings "Skills" panel). list = the
        // full discovered registry (builtin/user/plugin/project); get/save/
        // delete operate on the PROJECT skills dir (./.thclaws/skills/) —
        // per-user under multi-tenant workspace_root, and the highest-
        // precedence layer so a saved skill immediately wins.
        "gui_shell_skills_list" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let body = if !shell_has_permission(shell_id, "skills.read")
                && !shell_has_permission(shell_id, "skills.write")
            {
                serde_json::json!({ "error": "permission 'skills.read' not declared" })
            } else {
                let store = crate::skills::SkillStore::discover();
                let project_dir = crate::workdir::current_workdir()
                    .join(".thclaws")
                    .join("skills");
                let mut arr: Vec<serde_json::Value> = store
                    .skills
                    .values()
                    .map(|s| {
                        let editable = project_dir.join(&s.name).join("SKILL.md").is_file();
                        serde_json::json!({
                            "name": s.name,
                            "description": s.description,
                            "whenToUse": s.when_to_use,
                            "editable": editable,
                        })
                    })
                    .collect();
                arr.sort_by(|a, b| {
                    a["name"]
                        .as_str()
                        .unwrap_or("")
                        .cmp(b["name"].as_str().unwrap_or(""))
                });
                serde_json::json!({ "result": { "skills": arr } })
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        "gui_shell_skills_get" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let name = msg
                .get("skillName")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let body = if !shell_has_permission(shell_id, "skills.read")
                && !shell_has_permission(shell_id, "skills.write")
            {
                serde_json::json!({ "error": "permission 'skills.read' not declared" })
            } else if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                serde_json::json!({ "error": "invalid skill name" })
            } else {
                let path = crate::workdir::current_workdir()
                    .join(".thclaws")
                    .join("skills")
                    .join(&name)
                    .join("SKILL.md");
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        serde_json::json!({ "result": { "content": content, "editable": true } })
                    }
                    Err(_) => {
                        // Not a project skill — report the registry entry read-only.
                        let store = crate::skills::SkillStore::discover();
                        match store.skills.get(&name) {
                            Some(s) => serde_json::json!({ "result": {
                                "content": format!("# {}\n\n{}\n\n{}", s.name, s.description, s.when_to_use),
                                "editable": false,
                            }}),
                            None => serde_json::json!({ "error": "skill not found" }),
                        }
                    }
                }
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        // Install a skill from a git or zip URL — the same
        // `skills::install_from_url` the `/skill install` slash command
        // uses, so scope rules, the org-policy gate and the
        // executable-scripts policy all apply unchanged.
        "gui_shell_skill_install" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            if !shell_has_permission(shell_id, "skills.write") {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": "permission 'skills.write' not declared" }),
                ));
                return true;
            }
            let url = msg
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            // Optional: a repo whose name can't be derived from the URL
            // needs one, and `install_from_url` says so in its error.
            let name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string);
            // Project scope by default — a skill installed from this
            // workspace's chat belongs to this workspace.
            let project = !msg.get("user").and_then(|v| v.as_bool()).unwrap_or(false);
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({
                        "error": "url must be an https:// git repo or .zip (SSH remotes and local paths install from the desktop)",
                    }),
                ));
                return true;
            }
            let dispatch = ctx.dispatch.clone();
            let shared = ctx.shared.clone();
            tokio::spawn(async move {
                let reply =
                    match crate::skills::install_from_url(&url, name.as_deref(), project).await {
                        Ok(report) => {
                            // Rebuild around the new skill so it's
                            // usable in this session, not the next one.
                            let _ = shared
                                .input_tx
                                .send(crate::shared_session::ShellInput::SkillsRefresh);
                            serde_json::json!({ "result": {
                                "ok": true,
                                "report": report,
                                "scope": if project { "project" } else { "user" },
                            }})
                        }
                        Err(e) => serde_json::json!({ "error": e.to_string() }),
                    };
                dispatch(shell_reply(request_id, reply));
            });
        }

        "gui_shell_skills_save" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let name = msg
                .get("skillName")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let content = msg
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let body = if !shell_has_permission(shell_id, "skills.write") {
                serde_json::json!({ "error": "permission 'skills.write' not declared" })
            } else if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                serde_json::json!({ "error": "skill name must be alphanumeric/-/_" })
            } else if content.trim().is_empty() {
                serde_json::json!({ "error": "content required" })
            } else {
                let dir = crate::workdir::current_workdir()
                    .join(".thclaws")
                    .join("skills")
                    .join(&name);
                match std::fs::create_dir_all(&dir)
                    .and_then(|_| std::fs::write(dir.join("SKILL.md"), &content))
                {
                    Ok(_) => serde_json::json!({ "result": { "ok": true } }),
                    Err(e) => serde_json::json!({ "error": e.to_string() }),
                }
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        "gui_shell_skills_delete" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let name = msg
                .get("skillName")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let body = if !shell_has_permission(shell_id, "skills.write") {
                serde_json::json!({ "error": "permission 'skills.write' not declared" })
            } else if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                serde_json::json!({ "error": "invalid skill name" })
            } else {
                let dir = crate::workdir::current_workdir()
                    .join(".thclaws")
                    .join("skills")
                    .join(&name);
                if dir.join("SKILL.md").is_file() {
                    match std::fs::remove_dir_all(&dir) {
                        Ok(_) => serde_json::json!({ "result": { "ok": true } }),
                        Err(e) => serde_json::json!({ "error": e.to_string() }),
                    }
                } else {
                    serde_json::json!({ "error": "not a project skill (built-in/user skills are read-only here)" })
                }
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        // GUI Shell knowledge-write bridge (settings "Knowledge" panel).
        // Read side already exists (kms.list/browse, `kms.read`). create =
        // new project-scoped KMS; ingest = add an uploaded document
        // (thclaws.uploadFile → _uploads/<name>) into a KMS.
        "gui_shell_kms_create" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let name = msg
                .get("kmsName")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let body = if !shell_has_permission(shell_id, "kms.write") {
                serde_json::json!({ "error": "permission 'kms.write' not declared" })
            } else if name.is_empty() {
                serde_json::json!({ "error": "name required" })
            } else {
                match crate::kms::create(&name, crate::kms::KmsScope::Project) {
                    Ok(r) => serde_json::json!({ "result": { "ok": true, "name": r.name } }),
                    Err(e) => serde_json::json!({ "error": e.to_string() }),
                }
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        "gui_shell_kms_ingest" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let kms_name = msg
                .get("kmsName")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let path = msg
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let body = if !shell_has_permission(shell_id, "kms.write") {
                serde_json::json!({ "error": "permission 'kms.write' not declared" })
            } else if kms_name.is_empty() || path.is_empty() {
                serde_json::json!({ "error": "kmsName and path required" })
            } else {
                match crate::sandbox::Sandbox::check(&path) {
                    Err(e) => serde_json::json!({ "error": format!("path: {e}") }),
                    Ok(abs) => match crate::kms::resolve(&kms_name) {
                        None => {
                            serde_json::json!({ "error": format!("no KMS named '{kms_name}'") })
                        }
                        Some(kref) => match crate::kms::ingest(&kref, &abs, None, false) {
                            Ok(_) => serde_json::json!({ "result": { "ok": true } }),
                            Err(e) => serde_json::json!({ "error": e.to_string() }),
                        },
                    },
                }
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        // Composer "Auto ⌄" mode selector — read/switch the permission mode.
        "gui_shell_mode_get" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let mode = match crate::permissions::current_mode() {
                crate::permissions::PermissionMode::Auto => "auto",
                crate::permissions::PermissionMode::Ask => "ask",
                _ => "plan",
            };
            (ctx.dispatch)(shell_reply(
                request_id,
                serde_json::json!({ "result": { "mode": mode } }),
            ));
        }

        "gui_shell_mode_set" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let mode = msg.get("mode").and_then(|v| v.as_str()).unwrap_or("");
            let body = if !shell_has_permission(shell_id, "mode.write") {
                serde_json::json!({ "error": "permission 'mode.write' not declared" })
            } else {
                let parsed = match mode {
                    "auto" => Some(crate::permissions::PermissionMode::Auto),
                    "ask" => Some(crate::permissions::PermissionMode::Ask),
                    _ => None,
                };
                match parsed {
                    Some(m) => {
                        crate::permissions::set_current_mode_and_broadcast(m);
                        serde_json::json!({ "result": { "mode": mode } })
                    }
                    None => serde_json::json!({ "error": "mode must be 'auto' or 'ask'" }),
                }
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        // Profile panel: engine-side identity facts. Password/email live in
        // the cloud control plane — the shell shows those as cloud-managed.
        // GUI Shell: store a provider API key (BYOK) from a shell's
        // settings surface. Reuses the SAME normalise + backend routing
        // as the main app's `api_key_set` by delegating to it, so a key
        // added from a shell lands wherever the user's chosen backend
        // says (keychain or .env) and never diverges from the app path.
        //
        // Gated on `keys.write`, which no shell has by default — a chat
        // shell asking for it is asking to hold provider credentials,
        // and that should be visible in its manifest.
        "gui_shell_key_set" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            if !shell_has_permission(shell_id, "keys.write") {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": "permission 'keys.write' not declared" }),
                ));
                return true;
            }
            let provider = msg.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            let key = msg.get("key").and_then(|v| v.as_str()).unwrap_or("");
            if provider.is_empty() || key.trim().is_empty() {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": "provider and key are required" }),
                ));
                return true;
            }
            // Same store + same follow-up as the Settings path — a key
            // added from a shell must not diverge from one added from
            // the app. Report the REAL outcome: a keychain denial has to
            // reach the shell as an error, not a green tick.
            let (ok, error, storage) =
                store_provider_key(provider, strip_wrapping_quotes(key.trim()));
            if ok {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "result": { "ok": true, "provider": provider, "storage": storage } }),
                ));
            } else {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": error }),
                ));
            }
            announce_key_stored(provider, ok, &error, storage, ctx);
        }

        // ── Connectors (MCP servers) ─────────────────────────────────
        // A "connector" IS an MCP server — same mcp.json, same merge of
        // user (`~/.config/thclaws/mcp.json`) and project scope, same
        // servers the sidebar lists. This surface just makes them
        // manageable from a chat shell.
        "gui_shell_connectors_list" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            if !shell_has_permission(shell_id, "connectors.read") {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": "permission 'connectors.read' not declared" }),
                ));
                return true;
            }
            let cfg = crate::config::AppConfig::load().unwrap_or_default();
            let counts = crate::gui::mcp_tool_count_snapshot();
            let failures = crate::gui::mcp_failure_snapshot();
            // Which file a server came from decides whether this surface
            // may remove it — and a user-scope server is shared by every
            // project, which the UI has to be able to say out loud.
            let user_names = mcp_server_names_in_scope(true);
            // Plugins contribute MCP servers too, and the worker spawns
            // them alongside the mcp.json ones — so their tools are live
            // in the chat. Listing only `cfg.mcp_servers` showed a
            // connector-less workspace that nonetheless had connector
            // tools. Config wins on a name clash, matching the merge the
            // worker itself does.
            let owners: std::collections::HashMap<String, String> =
                crate::plugins::plugin_mcp_server_owners()
                    .into_iter()
                    .collect();
            let mut merged = cfg.mcp_servers.clone();
            for p_mcp in crate::plugins::plugin_mcp_servers() {
                if !merged.iter().any(|s| s.name == p_mcp.name) {
                    merged.push(p_mcp);
                }
            }
            let connectors: Vec<serde_json::Value> = merged
                .iter()
                .map(|s| {
                    let error = failures.get(&s.name);
                    let tools = counts.get(&s.name).copied().unwrap_or(0);
                    let status = if error.is_some() {
                        "failed"
                    } else if tools > 0 {
                        "connected"
                    } else {
                        "connecting"
                    };
                    serde_json::json!({
                        "name": s.name,
                        "transport": s.transport,
                        "url": s.url,
                        "command": s.command,
                        "args": s.args,
                        "headerNames": s.headers.keys().collect::<Vec<_>>(),
                        "scope": if owners.contains_key(&s.name) {
                            "plugin"
                        } else if user_names.contains(&s.name) {
                            "user"
                        } else {
                            "project"
                        },
                        // Set only for a plugin-owned server: it lives in
                        // the plugin's manifest, so it can't be removed
                        // by editing mcp.json.
                        "plugin": owners.get(&s.name),
                        "tools": tools,
                        "status": status,
                        "error": error,
                        "engineManaged": s.engine_managed,
                    })
                })
                .collect();
            (ctx.dispatch)(shell_reply(
                request_id,
                serde_json::json!({ "result": { "connectors": connectors } }),
            ));
        }

        "gui_shell_connector_add" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            if !shell_has_permission(shell_id, "connectors.write") {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": "permission 'connectors.write' not declared" }),
                ));
                return true;
            }
            let name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let url = msg
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            // HTTP transport ONLY, deliberately. A stdio entry names a
            // command the engine spawns on the next connect — letting a
            // shell write one is arbitrary local code execution, and the
            // first-spawn allowlist prompt can't be relied on to stop it
            // (hosted/multiuser forces auto-approve). stdio connectors
            // stay a desktop `/mcp add` decision.
            let err = if name.is_empty() || url.is_empty() {
                Some("name and url are required".to_string())
            } else if !crate::mcp::is_valid_server_name(&name) {
                Some("name may only contain letters, digits, '-' and '_'".to_string())
            } else if !(url.starts_with("https://") || url.starts_with("http://")) {
                Some("url must start with https:// (or http:// for localhost)".to_string())
            } else {
                None
            };
            if let Some(e) = err {
                (ctx.dispatch)(shell_reply(request_id, serde_json::json!({ "error": e })));
                return true;
            }
            let mut headers = std::collections::HashMap::new();
            if let Some(obj) = msg.get("headers").and_then(|v| v.as_object()) {
                for (k, v) in obj {
                    if let Some(val) = v.as_str() {
                        if !k.trim().is_empty() && !val.trim().is_empty() {
                            headers.insert(k.trim().to_string(), val.trim().to_string());
                        }
                    }
                }
            }
            let server = crate::mcp::McpServerConfig {
                name: name.clone(),
                transport: "http".into(),
                command: String::new(),
                args: Vec::new(),
                env: Default::default(),
                url,
                headers,
                // Hand-added, exactly like `/mcp add`: untrusted, so no
                // inline widget rendering. Only the marketplace install
                // flow grants trust.
                trusted: false,
                engine_managed: false,
            };
            match crate::config::save_mcp_server(&server, false) {
                Ok(path) => {
                    // Connect it now — a connector you have to restart to
                    // use isn't connected, it's configured.
                    let _ =
                        ctx.shared
                            .input_tx
                            .send(crate::shared_session::ShellInput::McpConnect(Box::new(
                                server,
                            )));
                    (ctx.dispatch)(shell_reply(
                        request_id,
                        serde_json::json!({ "result": {
                            "ok": true,
                            "name": name,
                            "path": path.to_string_lossy(),
                        }}),
                    ));
                }
                Err(e) => (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": e.to_string() }),
                )),
            }
        }

        "gui_shell_connector_remove" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            if !shell_has_permission(shell_id, "connectors.write") {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": "permission 'connectors.write' not declared" }),
                ));
                return true;
            }
            let name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if name.is_empty() {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": "name is required" }),
                ));
                return true;
            }
            // Try project first, then user — the same order the config
            // merge resolves, so removing by name from a shell drops
            // whichever entry was actually in effect.
            let mut removed_from: Option<String> = None;
            let mut removed_url: Option<String> = None;
            let mut last_err: Option<String> = None;
            for (user, label) in [(false, "project"), (true, "user")] {
                match crate::config::remove_mcp_server(&name, user) {
                    Ok((true, _, url)) => {
                        removed_from = Some(label.to_string());
                        removed_url = url;
                        break;
                    }
                    Ok((false, _, _)) => {}
                    Err(e) => last_err = Some(e.to_string()),
                }
            }
            match removed_from {
                Some(scope) => {
                    // Cached OAuth tokens are keyed by URL; leaving one
                    // behind would silently re-authorise a server the
                    // user just removed if they add the same URL later.
                    if let Some(url) = removed_url {
                        let mut store = crate::oauth::TokenStore::load();
                        store.remove(&url);
                        store.save();
                    }
                    let _ = ctx.shared.input_tx.send(
                        crate::shared_session::ShellInput::McpDisconnect {
                            server_name: name.clone(),
                        },
                    );
                    (ctx.dispatch)(shell_reply(
                        request_id,
                        serde_json::json!({ "result": { "ok": true, "name": name, "scope": scope } }),
                    ));
                }
                None => {
                    // A plugin's server isn't in any mcp.json, so the
                    // removal genuinely can't happen here — say which
                    // plugin owns it rather than "no such connector".
                    let owner = crate::plugins::plugin_mcp_server_owners()
                        .into_iter()
                        .find(|(server, _)| server == &name)
                        .map(|(_, plugin)| plugin);
                    let err = match (owner, last_err) {
                        (Some(plugin), _) => format!(
                            "'{name}' is provided by the plugin '{plugin}' — uninstall the plugin to remove it"
                        ),
                        (None, Some(e)) => e,
                        (None, None) => format!("no connector named '{name}'"),
                    };
                    (ctx.dispatch)(shell_reply(request_id, serde_json::json!({ "error": err })));
                }
            }
        }

        // One-shot completion on the session's ACTIVE model, no tools and
        // no history. For shells that need a small language-model step in
        // service of their own UI — rewriting a prompt, naming a thing —
        // rather than a conversation. `agent.run` would put that in the
        // user's chat, which is not where a UI helper belongs.
        //
        // Costs the user credits, so it's gated on its own permission and
        // the output is capped.
        "gui_shell_llm_complete" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            if !shell_has_permission(shell_id, "llm.complete") {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": "permission 'llm.complete' not declared" }),
                ));
                return true;
            }
            let prompt = msg
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if prompt.is_empty() {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": "prompt is required" }),
                ));
                return true;
            }
            let system = msg
                .get("system")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty());
            let max_tokens = msg
                .get("maxTokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(1024)
                .clamp(16, 4096) as u32;

            let dispatch = ctx.dispatch.clone();
            tokio::spawn(async move {
                let cfg = crate::config::AppConfig::load().unwrap_or_default();
                let reply = match crate::repl::build_provider(&cfg) {
                    Ok(provider) => {
                        let req = crate::providers::StreamRequest {
                            model: cfg.model.clone(),
                            system,
                            messages: vec![crate::types::Message::user(prompt)],
                            tools: vec![],
                            max_tokens,
                            thinking_budget: None,
                            stream_chunk_timeout_override: None,
                        };
                        match provider.stream(req).await {
                            Ok(stream) => {
                                match crate::providers::collect_turn(crate::providers::assemble(
                                    stream,
                                ))
                                .await
                                {
                                    Ok(turn) if !turn.text.trim().is_empty() => {
                                        serde_json::json!({ "result": {
                                            "text": turn.text.trim(),
                                            "model": cfg.model,
                                        }})
                                    }
                                    Ok(_) => serde_json::json!({
                                        "error": "the model returned nothing",
                                    }),
                                    Err(e) => serde_json::json!({ "error": e.to_string() }),
                                }
                            }
                            Err(e) => serde_json::json!({ "error": e.to_string() }),
                        }
                    }
                    Err(e) => serde_json::json!({
                        "error": format!("no usable model right now: {e}"),
                    }),
                };
                dispatch(shell_reply(request_id, reply));
            });
        }

        // ── Plugins ──────────────────────────────────────────────────
        // A plugin is a bundle: skills + commands + agents + MCP
        // servers, installed as one unit. This surface manages the ones
        // already installed. Installing is NOT here — it runs a git
        // clone or a zip extract, which is code arriving on the machine,
        // and that stays a desktop `/plugin install` decision (same line
        // the connectors surface draws at stdio).
        "gui_shell_plugins_list" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            if !shell_has_permission(shell_id, "plugins.read") {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": "permission 'plugins.read' not declared" }),
                ));
                return true;
            }
            let counts = crate::gui::mcp_tool_count_snapshot();
            let failures = crate::gui::mcp_failure_snapshot();
            let user_scope: std::collections::HashSet<String> =
                crate::plugins::PluginRegistry::load(true)
                    .map(|r| r.plugins.into_iter().map(|p| p.name).collect())
                    .unwrap_or_default();
            // Disabled ones too: a disabled plugin is exactly what the
            // user came here to turn back on, and `/plugins` lists it.
            let plugins: Vec<serde_json::Value> = crate::plugins::all_plugins_all_scopes()
                .iter()
                .map(|p| {
                    let manifest = p.manifest().ok();
                    let c = crate::plugins::contributions(p);
                    let servers: Vec<serde_json::Value> = c
                        .mcp_servers
                        .iter()
                        .map(|name| {
                            serde_json::json!({
                                "name": name,
                                "tools": counts.get(name).copied().unwrap_or(0),
                                "error": failures.get(name),
                            })
                        })
                        .collect();
                    serde_json::json!({
                        "name": p.name,
                        "version": if p.version.is_empty() {
                            manifest.as_ref().map(|m| m.version.clone()).unwrap_or_default()
                        } else {
                            p.version.clone()
                        },
                        "description": manifest.as_ref().map(|m| m.description.clone()).unwrap_or_default(),
                        "author": manifest.as_ref().map(|m| m.author.clone()).unwrap_or_default(),
                        "source": p.source,
                        "path": p.path.to_string_lossy(),
                        "scope": if user_scope.contains(&p.name) { "user" } else { "project" },
                        "enabled": p.enabled,
                        "skills": c.skills,
                        "commands": c.commands,
                        "agents": c.agents,
                        "mcpServers": servers,
                        // A broken manifest is why a plugin contributes
                        // nothing; without this the panel would just show
                        // zeros and look like the plugin was empty.
                        "manifestError": p.manifest().err().map(|e| e.to_string()),
                    })
                })
                .collect();
            (ctx.dispatch)(shell_reply(
                request_id,
                serde_json::json!({ "result": { "plugins": plugins } }),
            ));
        }

        // Install a plugin from a git or zip URL. The fetch itself only
        // downloads and unpacks — nothing from the archive runs at
        // install time. What the plugin CONTRIBUTES runs later, and each
        // route keeps its own gate: an stdio MCP server still goes
        // through the spawn allowlist, skills still need the model to
        // choose them. So this is the user's call, made explicit in the
        // panel, not a line the shell surface has to refuse.
        "gui_shell_plugin_install" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            if !shell_has_permission(shell_id, "plugins.write") {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": "permission 'plugins.write' not declared" }),
                ));
                return true;
            }
            let url = msg
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            // Project scope by default — a plugin installed from this
            // workspace's chat belongs to this workspace.
            let user = msg.get("user").and_then(|v| v.as_bool()).unwrap_or(false);
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({
                        "error": "url must be an https:// git repo or .zip (SSH remotes and local paths install from the desktop)",
                    }),
                ));
                return true;
            }
            let dispatch = ctx.dispatch.clone();
            let shared = ctx.shared.clone();
            tokio::spawn(async move {
                // A clone can take a while; the panel shows "Installing…"
                // until this lands.
                let reply = match crate::plugins::install(&url, user, false).await {
                    Ok(plugin) => {
                        let c = crate::plugins::contributions(&plugin);
                        // Bring its connectors up now, the same as
                        // enabling one — install already implies "I want
                        // this active".
                        if let Ok(manifest) = plugin.manifest() {
                            for (name, entry) in &manifest.mcp_servers {
                                let _ = shared.input_tx.send(
                                    crate::shared_session::ShellInput::McpConnect(Box::new(
                                        entry.to_config(name),
                                    )),
                                );
                            }
                        }
                        serde_json::json!({ "result": {
                            "ok": true,
                            "name": plugin.name,
                            "version": plugin.version,
                            "scope": if user { "user" } else { "project" },
                            "skills": c.skills,
                            "commands": c.commands,
                            "agents": c.agents,
                            "mcpServers": c.mcp_servers,
                        }})
                    }
                    Err(e) => serde_json::json!({ "error": e.to_string() }),
                };
                dispatch(shell_reply(request_id, reply));
            });
        }

        "gui_shell_plugin_set_enabled" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            if !shell_has_permission(shell_id, "plugins.write") {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": "permission 'plugins.write' not declared" }),
                ));
                return true;
            }
            let name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let enabled = msg.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            let Some((plugin, user)) = crate::plugins::find_installed_with_scope(&name) else {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": format!("no plugin named '{name}'") }),
                ));
                return true;
            };
            match crate::plugins::set_enabled(&name, user, enabled) {
                Ok(true) => {
                    // Its MCP servers can follow immediately; skills,
                    // commands and agents are read when the session
                    // builds its registry, so those wait. Say which is
                    // which rather than implying a full live swap.
                    apply_plugin_mcp_state(&plugin, enabled, ctx);
                    (ctx.dispatch)(shell_reply(
                        request_id,
                        serde_json::json!({ "result": {
                            "ok": true,
                            "name": name,
                            "enabled": enabled,
                            "scope": if user { "user" } else { "project" },
                        }}),
                    ));
                }
                Ok(false) => (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": format!("no plugin named '{name}'") }),
                )),
                Err(e) => (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": e.to_string() }),
                )),
            }
        }

        "gui_shell_plugin_remove" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            if !shell_has_permission(shell_id, "plugins.write") {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": "permission 'plugins.write' not declared" }),
                ));
                return true;
            }
            let name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let Some((plugin, user)) = crate::plugins::find_installed_with_scope(&name) else {
                (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": format!("no plugin named '{name}'") }),
                ));
                return true;
            };
            // Detach its servers BEFORE the files go: the tools stay in
            // the live registry otherwise and the model can call into a
            // server whose plugin no longer exists.
            apply_plugin_mcp_state(&plugin, false, ctx);
            match crate::plugins::remove(&name, user) {
                Ok(true) => (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "result": { "ok": true, "name": name } }),
                )),
                Ok(false) => (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": format!("no plugin named '{name}'") }),
                )),
                Err(e) => (ctx.dispatch)(shell_reply(
                    request_id,
                    serde_json::json!({ "error": e.to_string() }),
                )),
            }
        }

        "gui_shell_profile_get" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let member = ctx
                .shared
                .session_roots
                .as_ref()
                .and_then(|r| r.member_id.clone());
            // Only multiuser has a logged-in identity to show. Desktop
            // has no login, so a shell gets no name and should render
            // no name — not a placeholder.
            let display_name = ctx
                .shared
                .session_roots
                .as_ref()
                .and_then(|r| r.member_name.clone());
            let workspace = crate::workdir::current_workdir()
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            (ctx.dispatch)(shell_reply(
                request_id,
                serde_json::json!({ "result": {
                    "memberId": member,
                    "displayName": display_name,
                    "workspace": workspace,
                    "multiuser": ctx.shared.session_roots.is_some(),
                }}),
            ));
        }

        // GUI Shell sessions bridge (dev-plan/33 — chat-history surface).
        // Exposes the SAME shared session Chat uses: list past sessions,
        // load one (replacing the agent's history so run() continues it),
        // start a new one, rename, delete. Lets a chat-style shell drive
        // real engine sessions instead of a single bound one. Each replies
        // via the `gui_shell_event` correlator (result/error).
        "gui_shell_session_list" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let body = if !shell_has_permission(shell_id, "session.list")
                && !shell_has_permission(shell_id, "session.read")
            {
                serde_json::json!({ "error": "permission 'session.list' not declared" })
            } else {
                let store = ipc_session_store(ctx);
                let items = store
                    .as_ref()
                    .and_then(|s| s.list().ok())
                    .unwrap_or_default();
                let arr: Vec<serde_json::Value> = items
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "id": m.id,
                            "title": m.title,
                            "updatedAt": m.updated_at,
                            "messageCount": m.message_count,
                            "model": m.model,
                        })
                    })
                    .collect();
                serde_json::json!({ "result": { "sessions": arr } })
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        "gui_shell_session_load" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let sid = msg
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let load_id = msg
                .get("loadId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let body = if !shell_has_permission(shell_id, "session.read") {
                serde_json::json!({ "error": "permission 'session.read' not declared" })
            } else if load_id.is_empty() {
                serde_json::json!({ "error": "id required" })
            } else {
                let store = ipc_session_store(ctx);
                let msgs = store
                    .as_ref()
                    .and_then(|s| s.load(&load_id).ok())
                    .map(|sess| serialize_shell_history_with_usage(&sess))
                    .unwrap_or_default();
                // Prime the shared agent to continue this session so a
                // subsequent gui_shell_run appends to it.
                let _ = ctx
                    .shared
                    .input_tx
                    .send(crate::shared_session::ShellInput::LoadSession(
                        load_id.clone(),
                    ));
                serde_json::json!({ "result": { "messages": msgs } })
            };
            let _ = sid;
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        "gui_shell_session_new" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let body = if !shell_has_permission(shell_id, "session.write") {
                serde_json::json!({ "error": "permission 'session.write' not declared" })
            } else {
                let _ = ctx
                    .shared
                    .input_tx
                    .send(crate::shared_session::ShellInput::NewSession);
                serde_json::json!({ "result": { "ok": true } })
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        "gui_shell_session_rename" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let sess_id = msg
                .get("renameId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let title = msg
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let body = if !shell_has_permission(shell_id, "session.write") {
                serde_json::json!({ "error": "permission 'session.write' not declared" })
            } else if sess_id.is_empty() {
                serde_json::json!({ "error": "id required" })
            } else {
                match ipc_session_store(ctx)
                    .as_ref()
                    .map(|s| s.rename(&sess_id, &title))
                {
                    Some(Ok(_)) => {
                        let _ = ctx.shared.input_tx.send(
                            crate::shared_session::ShellInput::SessionRenamedExternal {
                                id: sess_id.clone(),
                                title: title.clone(),
                            },
                        );
                        serde_json::json!({ "result": { "ok": true } })
                    }
                    Some(Err(e)) => serde_json::json!({ "error": e.to_string() }),
                    None => serde_json::json!({ "error": "no session store" }),
                }
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        "gui_shell_session_delete" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let sess_id = msg
                .get("deleteId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let body = if !shell_has_permission(shell_id, "session.write") {
                serde_json::json!({ "error": "permission 'session.write' not declared" })
            } else if sess_id.is_empty() {
                serde_json::json!({ "error": "id required" })
            } else {
                match ipc_session_store(ctx).as_ref().map(|s| s.delete(&sess_id)) {
                    Some(Ok(())) => {
                        let _ = ctx.shared.input_tx.send(
                            crate::shared_session::ShellInput::SessionDeletedExternal {
                                id: sess_id.clone(),
                            },
                        );
                        serde_json::json!({ "result": { "ok": true } })
                    }
                    Some(Err(e)) => serde_json::json!({ "error": e.to_string() }),
                    None => serde_json::json!({ "error": "no session store" }),
                }
            };
            (ctx.dispatch)(shell_reply(request_id, body));
        }

        // GUI Shell (dev-plan/33 Tier 2/3) — direct tool invocation
        // bypassing the agent loop. The shell's domain UI uses this
        // for deterministic actions (Media Studio's "Generate" button
        // calls TextToImage/TextToVideo directly; no model round-trip).
        //
        // Rules:
        //   - Read-only tools (ls/read/glob/grep/web_fetch/...) → run.
        //   - Tools whose `requires_approval(&input)` returns true →
        //     routed through the same `GuiApprover` the agent uses
        //     (dev-plan/33 Tier 3 + dev-plan/40 Tier 3). The user gets
        //     the normal approval modal; Deny surfaces as an error.
        //   - MCP-contributed tools are NOT visible here — the fresh
        //     ToolRegistry::with_builtins() doesn't include them. The
        //     media tools (dev-plan/40) are flagged by `imageToolsEnabled`
        //     (same as for the agent) and registered below only when that
        //     flag is on OR the calling shell is `media-studio` — the
        //     built-in Media Studio is the media on-ramp, so loading it
        //     auto-enables them without the user toggling settings.
        //
        // The IPC dispatch is sync but Tool::call is async + the wry
        // IPC thread has no tokio runtime context. Build a per-call
        // single-threaded runtime in a fresh OS thread. The approval
        // await resolves out-of-band: the GuiApprover sends a modal
        // request to the frontend and the later `approval_response`
        // IPC (on the main thread) calls `approver.resolve(id, ...)`,
        // completing the oneshot this block_on is parked on.
        "gui_shell_tool_invoke" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let tool_name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = msg.get("args").cloned().unwrap_or(serde_json::Value::Null);
            let shell_id = msg
                .get("shellId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Media tools (dev-plan/40) are flagged by `imageToolsEnabled`
            // like for the agent — but the built-in Media Studio shell is
            // the on-ramp for media generation, so loading it auto-enables
            // them without the user toggling settings. So: register the
            // media tools into the invoke registry only when the flag is on
            // OR the calling shell is media-studio. (Other shells stay
            // gated by the flag.)
            // Built-in studios are the on-ramp for their tool families:
            // opening them enables the tools without a settings toggle
            // or a prior skill turn (dev-plan/40 + dev-plan/52).
            if shell_id == "film-studio" {
                crate::tools::activate_gate(crate::tools::filmscript::GATE);
            }
            let media_enabled = shell_id == "media-studio"
                || shell_id == "film-studio"
                || crate::config::AppConfig::load()
                    .map(|c| c.image_tools_enabled)
                    .unwrap_or(false);
            let hal_enabled = crate::config::AppConfig::load()
                .map(|c| c.hal_enabled)
                .unwrap_or(false);
            // dev-plan/39 Tier 3: when the shell hosts its own approval UI
            // it sends `preferInline` — route the approve/deny to the shell
            // instead of popping the full-screen system modal over it.
            let prefer_inline = msg
                .get("preferInline")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let dispatch = ctx.dispatch.clone();
            let approver = ctx.approver.clone();
            let inline_session = session_id.clone();
            std::thread::spawn(move || {
                let outcome: std::result::Result<String, String> = (|| {
                    if tool_name.is_empty() {
                        return Err("gui_shell_tool_invoke: missing 'name' field".into());
                    }
                    // dev-plan/39 Tier 3: enforce the manifest tool allowlist
                    // (no-op for shells that don't declare tools.invoke:*).
                    if !shell_tool_invoke_allowed(&shell_id, &tool_name) {
                        return Err(format!(
                            "tool '{tool_name}' not in this shell's manifest permissions \
                             (declare \"tools.invoke:{tool_name}\" to allow it)"
                        ));
                    }
                    let mut registry = crate::tools::ToolRegistry::with_builtins();
                    if media_enabled {
                        registry.register(Arc::new(crate::tools::TextToImageTool));
                        registry.register(Arc::new(crate::tools::ImageToImageTool));
                        registry.register(Arc::new(crate::tools::TextToSpeechTool));
                        registry.register(Arc::new(crate::tools::RenderSlidesTool));
                        registry.register(Arc::new(crate::tools::TextToVideoTool));
                        registry.register(Arc::new(crate::tools::ImageToVideoTool));
                        registry.register(Arc::new(crate::tools::MediaJobStatusTool));
                    }

                    if hal_enabled {
                        registry.register(Arc::new(crate::tools::YouTubeTranscriptTool::new()));
                        registry.register(Arc::new(crate::tools::WebScrapeTool::new()));
                    }
                    let tool = registry
                        .get(&tool_name)
                        .ok_or_else(|| format!("unknown tool: {tool_name}"))?;
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| format!("tokio runtime build: {e}"))?;
                    rt.block_on(async {
                        if tool.requires_approval(&args) {
                            let decision = if prefer_inline {
                                // Inline path: dispatch an approval_request
                                // event to the shell + await its widget's
                                // decision (5-min cap → deny, so a shell
                                // that never answers can't wedge the call).
                                let (aid, rx) = crate::gui_shell::inline_approval::register();
                                let evt = serde_json::json!({
                                    "type": "gui_shell_event",
                                    "sessionId": inline_session,
                                    "event": "approval_request",
                                    "payload": {
                                        "approvalId": aid,
                                        "toolName": tool_name.clone(),
                                        "input": args.clone(),
                                        "summary": format!("{tool_name} (GUI shell)"),
                                    },
                                });
                                dispatch(evt.to_string());
                                match tokio::time::timeout(std::time::Duration::from_secs(300), rx)
                                    .await
                                {
                                    Ok(Ok(d)) => d,
                                    _ => {
                                        crate::gui_shell::inline_approval::forget(aid);
                                        ApprovalDecision::Deny
                                    }
                                }
                            } else {
                                approver
                                    .approve(&ApprovalRequest {
                                        tool_name: tool_name.clone(),
                                        input: args.clone(),
                                        summary: Some(format!("{tool_name} (GUI shell)")),
                                        originator: AgentOrigin::Main,
                                    })
                                    .await
                            };
                            if matches!(decision, ApprovalDecision::Deny) {
                                return Err(format!("tool '{tool_name}' denied by user"));
                            }
                        }
                        tool.call(args).await.map_err(|e| e.to_string())
                    })
                })();
                let reply = match outcome {
                    Ok(output) => serde_json::json!({
                        "type": "gui_shell_event",
                        "sessionId": session_id,
                        "replyTo": request_id,
                        "result": output,
                    }),
                    Err(err) => serde_json::json!({
                        "type": "gui_shell_event",
                        "sessionId": session_id,
                        "replyTo": request_id,
                        "error": err,
                    }),
                };
                dispatch(reply.to_string());
            });
        }

        // GUI Shell (dev-plan/39 Tier 3) — the shell's inline approval
        // widget answering an `approval_request` it received. Resolves
        // the pending decision the `gui_shell_tool_invoke` inline path is
        // awaiting. Fire-and-forget (no reply needed).
        "gui_shell_approval_respond" => {
            let approval_id = msg.get("approvalId").and_then(|v| v.as_u64());
            let decision = msg
                .get("decision")
                .and_then(|v| v.as_str())
                .map(crate::gui_shell::inline_approval::parse_decision)
                .unwrap_or(ApprovalDecision::Deny);
            if let Some(id) = approval_id {
                crate::gui_shell::inline_approval::resolve(id, decision);
            }
        }

        // GUI Shell (dev-plan/33 Tier 2) — per-shell, per-session
        // key-value storage. State lives at
        // ~/.config/thclaws/gui-shell/<shellId>/state/<sessionId>.json
        // — user-level regardless of how the shell was installed (state
        // is the user's, not the repo's, so uninstall doesn't lose it).
        "gui_shell_storage_get" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let key = msg.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let result = match ctx.shared.session_roots.as_ref() {
                Some(roots) => {
                    crate::gui_shell::storage::get_in(&roots.storage_dir, shell_id, session_id, key)
                }
                None => crate::gui_shell::storage::get(shell_id, session_id, key),
            };
            let reply = match result {
                Ok(v) => serde_json::json!({
                    "type": "gui_shell_event",
                    "sessionId": session_id,
                    "replyTo": request_id,
                    "result": { "value": v },
                }),
                Err(e) => serde_json::json!({
                    "type": "gui_shell_event",
                    "sessionId": session_id,
                    "replyTo": request_id,
                    "error": e.to_string(),
                }),
            };
            (ctx.dispatch)(reply.to_string());
        }

        // Running-jobs UI (dev-plan/36) — point-in-time query for the
        // current busy state. Frontend hits this on initial connect /
        // reconnect so the running chip + auto-reattach logic don't
        // depend on catching a transient `gui_busy_changed` event
        // that fired before the WS was open. The shape mirrors the
        // event payload so a single React hook handles both.
        "gui_busy_query" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let meta = crate::agent_activity::busy_meta();
            let started_at_ms = meta.as_ref().and_then(|m| {
                m.started_at
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_millis() as u64)
            });
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "gui_busy_result",
                    "id": request_id,
                    "busy": meta.is_some(),
                    "sessionId": meta.as_ref().map(|m| m.session_id.clone()),
                    "startedAtMs": started_at_ms,
                    "lastProgress": meta.as_ref().and_then(|m| m.last_progress.clone()),
                })
                .to_string(),
            );
        }

        // GUI Shell (dev-plan/33 Tier 2) — picker list. Returns the
        // merged registry (builtin + user + project) so the picker can
        // render its grid. Reply is fired through ctx.dispatch as a
        // gui_shell_list_result envelope — the frontend correlates by
        // the request id it sent. Includes the `tabDefault` resolved
        // from settings.json::guiShell so the picker can auto-open
        // the user's preferred shell without showing the grid.
        "gui_shell_list" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let registry = crate::gui_shell::ShellRegistry::new();
            let listed: Vec<serde_json::Value> = registry
                .list()
                .into_iter()
                .map(|(source, m)| {
                    serde_json::json!({
                        "id": m.id,
                        "name": m.name,
                        "version": m.version,
                        "description": m.description,
                        "icon": m.icon,
                        "source": source.as_str(),
                        "permissions": m.permissions,
                    })
                })
                .collect();
            // Resolve tabDefault from layered config. None when unset
            // (picker shows grid as usual).
            let tab_default = crate::config::AppConfig::load().ok().and_then(|c| {
                c.gui_shell
                    .and_then(|s| s.tab_default().map(str::to_string))
            });
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "gui_shell_list_result",
                    "id": request_id,
                    "shells": listed,
                    "tabDefault": tab_default,
                })
                .to_string(),
            );
        }

        "gui_shell_storage_set" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let key = msg.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let value = msg.get("value").cloned().unwrap_or(serde_json::Value::Null);
            let result = match ctx.shared.session_roots.as_ref() {
                Some(roots) => crate::gui_shell::storage::set_in(
                    &roots.storage_dir,
                    shell_id,
                    session_id,
                    key,
                    value,
                ),
                None => crate::gui_shell::storage::set(shell_id, session_id, key, value),
            };
            let reply = match result {
                Ok(()) => serde_json::json!({
                    "type": "gui_shell_event",
                    "sessionId": session_id,
                    "replyTo": request_id,
                    "result": null,
                }),
                Err(e) => serde_json::json!({
                    "type": "gui_shell_event",
                    "sessionId": session_id,
                    "replyTo": request_id,
                    "error": e.to_string(),
                }),
            };
            (ctx.dispatch)(reply.to_string());
        }

        // GUI Shell (dev-plan/39 Tier 3) — delete a storage key. Mirrors
        // storage_set; `set(key,null)` stores null, this removes the key.
        "gui_shell_storage_delete" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let key = msg.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let result = match ctx.shared.session_roots.as_ref() {
                Some(roots) => crate::gui_shell::storage::delete_in(
                    &roots.storage_dir,
                    shell_id,
                    session_id,
                    key,
                ),
                None => crate::gui_shell::storage::delete(shell_id, session_id, key),
            };
            let reply = match result {
                Ok(()) => serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id,
                    "replyTo": request_id, "result": null,
                }),
                Err(e) => serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id,
                    "replyTo": request_id, "error": e.to_string(),
                }),
            };
            (ctx.dispatch)(reply.to_string());
        }

        // GUI Shell (dev-plan/39 Tier 3) — upload a blob (base64) into the
        // workspace's `_uploads/` and return a servable file-asset URL the
        // shell can use as <img src>/<a href>. Rides the IPC channel so it
        // works in both Mode A (desktop) and Mode B (serve); per-user
        // isolated in multiuser because the base is the session's own
        // workspace root. Capped at UPLOAD_MAX_BYTES.
        "gui_shell_upload_file" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("upload.bin")
                .to_string();
            let data_b64 = msg
                .get("dataB64")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Per-user root in multiuser (dp42), else the process workspace.
            let base = ctx
                .shared
                .session_roots
                .as_ref()
                .and_then(|r| r.workspace_root.clone())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let reply = shell_upload_into(&base, &name, &data_b64);
            let envelope = match reply {
                Ok(result) => serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id,
                    "replyTo": request_id, "result": result,
                }),
                Err(e) => serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id,
                    "replyTo": request_id, "error": e,
                }),
            };
            (ctx.dispatch)(envelope.to_string());
        }

        // GUI Shell (dev-plan/39 Tier 3) — the shell's declared manifest
        // permissions, for `thclaws.permissions.list()/has()`. Read-only
        // so a shell can grey out UI for actions it wasn't granted.
        "gui_shell_permissions_list" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let perms = crate::gui_shell::ShellRegistry::new()
                .resolve(shell_id)
                .map(|s| s.manifest().permissions.clone())
                .unwrap_or_default();
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id,
                    "replyTo": request_id, "result": perms,
                })
                .to_string(),
            );
        }

        // GUI Shell (dev-plan/39 Tier 3) — shell-initiated approval: the
        // shell asks the user to sign off on its OWN action (distinct from
        // the tool-invoke inline approval). Routes through the session's
        // approver (system modal / auto per mode) and returns the verdict
        // so the shell decides what to do. Runs on a worker thread since
        // approver.approve() blocks on user input.
        "gui_shell_await_approval" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let summary = msg
                .get("summary")
                .or_else(|| msg.get("reason"))
                .or_else(|| msg.get("title"))
                .and_then(|v| v.as_str())
                .unwrap_or("shell action")
                .to_string();
            let dispatch = ctx.dispatch.clone();
            let approver = ctx.approver.clone();
            std::thread::spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(_) => return,
                };
                let decision = rt.block_on(async {
                    approver
                        .approve(&ApprovalRequest {
                            tool_name: "shell.action".to_string(),
                            input: serde_json::json!({ "summary": summary }),
                            summary: Some(summary.clone()),
                            originator: AgentOrigin::Main,
                        })
                        .await
                });
                let approved = !matches!(decision, ApprovalDecision::Deny);
                dispatch(
                    serde_json::json!({
                        "type": "gui_shell_event", "sessionId": session_id,
                        "replyTo": request_id,
                        "result": { "approved": approved },
                    })
                    .to_string(),
                );
            });
        }

        // GUI Shell model widget (thclaws.model.*). Gated by manifest
        // permissions: `model.read` for get/list, `model.write` for set.
        // A shell that doesn't declare them gets an error and <thc-model>
        // renders nothing.
        "gui_shell_model_get" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let reply = if !shell_has_permission(shell_id, "model.read") {
                serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                    "error": "permission 'model.read' not granted in manifest",
                })
            } else {
                let cfg = crate::config::AppConfig::load().unwrap_or_default();
                let provider = cfg.detect_provider().unwrap_or("unknown");
                serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                    "result": {
                        "provider": provider,
                        "model": cfg.model,
                        "writable": shell_has_permission(shell_id, "model.write"),
                    },
                })
            };
            (ctx.dispatch)(reply.to_string());
        }

        "gui_shell_model_list" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let shell_id = msg
                .get("shellId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !shell_has_permission(&shell_id, "model.read") {
                (ctx.dispatch)(
                    serde_json::json!({
                        "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                        "error": "permission 'model.read' not granted in manifest",
                    })
                    .to_string(),
                );
            } else {
                // Full cross-provider catalogue — the same grouped payload
                // the main-app sidebar picker uses, so a shell can switch
                // provider AND model. build_all_models_payload is async
                // (live-polls local runtimes), so reply from a task.
                let dispatch = ctx.dispatch.clone();
                tokio::spawn(async move {
                    let payload = crate::providers::build_all_models_payload().await;
                    let groups = serde_json::from_str::<serde_json::Value>(&payload)
                        .ok()
                        .and_then(|v| v.get("groups").cloned())
                        .unwrap_or_else(|| serde_json::Value::Array(vec![]));
                    let cfg = crate::config::AppConfig::load().unwrap_or_default();
                    let reply = serde_json::json!({
                        "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                        "result": { "current": cfg.model, "groups": groups },
                    });
                    dispatch(reply.to_string());
                });
            }
        }

        "gui_shell_model_set" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let model = msg
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let reply = if !shell_has_permission(shell_id, "model.write") {
                serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                    "error": "permission 'model.write' not granted in manifest",
                })
            } else if model.is_empty() {
                serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                    "error": "gui_shell_model_set: missing 'model'",
                })
            } else {
                // Same path as the `model_set` arm: persist, reload, and
                // broadcast provider_update so the sidebar + every shell's
                // thclaws.model.onChange see the switch.
                let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
                project.set_model(&model);
                let _ = project.save();
                let new_cfg = crate::config::AppConfig::load().unwrap_or_default();
                let provider_name = new_cfg.detect_provider().unwrap_or("unknown");
                let ready = crate::providers::provider_has_credentials(&new_cfg);
                (ctx.dispatch)(
                    serde_json::json!({
                        "type": "provider_update",
                        "provider": provider_name,
                        "model": new_cfg.model,
                        "thinking": crate::providers::ThinkingLevel::json(new_cfg.thinking_budget),
                        "provider_ready": ready,
                    })
                    .to_string(),
                );
                let _ = ctx
                    .shared
                    .input_tx
                    .send(crate::shared_session::ShellInput::ReloadConfig);
                serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                    "result": { "ok": true, "model": new_cfg.model },
                })
            };
            (ctx.dispatch)(reply.to_string());
        }

        // Deterministic KMS API (thclaws.kms.*) — no LLM. Gated by
        // `kms.read`. list = knowledge bases; browse = one base's pages.
        "gui_shell_kms_list" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let reply = if !shell_has_permission(shell_id, "kms.read") {
                serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                    "error": "permission 'kms.read' not granted in manifest",
                })
            } else {
                let payload = crate::kms::build_update_payload();
                let mut kmss = payload
                    .get("kmss")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                // Enrich each base with a deterministic page count so a
                // shell can render it without a browse round-trip per base.
                for k in kmss.iter_mut() {
                    if let Some(name) = k.get("name").and_then(|v| v.as_str()) {
                        let pages = crate::kms::browse(name).map(|l| l.pages.len()).unwrap_or(0);
                        k["pages"] = serde_json::json!(pages);
                    }
                }
                serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                    "result": { "kmss": kmss },
                })
            };
            (ctx.dispatch)(reply.to_string());
        }

        "gui_shell_kms_browse" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let name = msg.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let reply = if !shell_has_permission(shell_id, "kms.read") {
                serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                    "error": "permission 'kms.read' not granted in manifest",
                })
            } else {
                match crate::kms::browse(name) {
                    Some(l) => serde_json::json!({
                        "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                        "result": { "kms": l.kms, "pages": l.pages, "sources": l.sources },
                    }),
                    None => serde_json::json!({
                        "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                        "error": format!("no KMS named '{name}'"),
                    }),
                }
            };
            (ctx.dispatch)(reply.to_string());
        }

        // Deterministic research-job API (thclaws.research.*) — no LLM.
        // Gated by `research.read`. Reads the live job registry (running +
        // recently-completed), the real source of {status, score, …}.
        "gui_shell_research_list" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let reply = if !shell_has_permission(shell_id, "research.read") {
                serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                    "error": "permission 'research.read' not granted in manifest",
                })
            } else {
                let jobs: Vec<serde_json::Value> = crate::research::manager()
                    .list()
                    .iter()
                    .map(job_view_json)
                    .collect();
                serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                    "result": { "jobs": jobs },
                })
            };
            (ctx.dispatch)(reply.to_string());
        }

        "gui_shell_research_get" => {
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let session_id = msg.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
            let shell_id = msg.get("shellId").and_then(|v| v.as_str()).unwrap_or("");
            let job_id = msg.get("jobId").and_then(|v| v.as_str()).unwrap_or("");
            let reply = if !shell_has_permission(shell_id, "research.read") {
                serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                    "error": "permission 'research.read' not granted in manifest",
                })
            } else {
                let job = crate::research::manager()
                    .get(job_id)
                    .map(|v| job_view_json(&v))
                    .unwrap_or(serde_json::Value::Null);
                serde_json::json!({
                    "type": "gui_shell_event", "sessionId": session_id, "replyTo": request_id,
                    "result": { "job": job },
                })
            };
            (ctx.dispatch)(reply.to_string());
        }

        // Schedule-add modal cron preview. Frontend debounces field
        // changes and asks the backend to validate + project the
        // next N fires so users see exactly when their schedule will
        // trigger before saving. Cheap: pure parser call, no I/O.
        "schedule_cron_preview" => {
            let cron = msg
                .get("cron")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if cron.is_empty() {
                (ctx.dispatch)(
                    serde_json::json!({
                        "type": "schedule_cron_preview_result",
                        "cron": cron,
                        "ok": false,
                        "error": "cron is empty",
                    })
                    .to_string(),
                );
                return true;
            }
            match crate::schedule::validate_cron(&cron) {
                Ok(()) => {
                    let now = chrono::Utc::now();
                    let fires: Vec<String> = crate::schedule::compute_next_n_fires(&cron, now, 3)
                        .into_iter()
                        .map(|t| t.to_rfc3339())
                        .collect();
                    (ctx.dispatch)(
                        serde_json::json!({
                            "type": "schedule_cron_preview_result",
                            "cron": cron,
                            "ok": true,
                            "fires": fires,
                        })
                        .to_string(),
                    );
                }
                Err(e) => {
                    (ctx.dispatch)(
                        serde_json::json!({
                            "type": "schedule_cron_preview_result",
                            "cron": cron,
                            "ok": false,
                            "error": format!("{e}"),
                        })
                        .to_string(),
                    );
                }
            }
        }

        // Schedule-add modal submit. Frontend posts the form fields;
        // we validate, persist, and dispatch `schedule_add_result` so
        // the modal can show success or surface an error inline.
        "schedule_add_submit" => {
            let id = msg
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let cron = msg
                .get("cron")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let prompt = msg
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let cwd = msg
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();

            let mut errors: Vec<String> = Vec::new();
            if id.is_empty() {
                errors.push("id is required".into());
            }
            if cron.is_empty() {
                errors.push("cron is required".into());
            }
            if prompt.trim().is_empty() {
                errors.push("prompt is required".into());
            }
            if cwd.is_empty() {
                errors.push("cwd is required".into());
            }
            if errors.is_empty() {
                if let Err(e) = crate::schedule::validate_cron(&cron) {
                    errors.push(format!("{e}"));
                }
                let cwd_path = std::path::PathBuf::from(&cwd);
                if !cwd_path.exists() {
                    errors.push(format!("cwd does not exist: {cwd}"));
                }
            }

            if !errors.is_empty() {
                (ctx.dispatch)(
                    serde_json::json!({
                        "type": "schedule_add_result",
                        "ok": false,
                        "error": errors.join("; "),
                    })
                    .to_string(),
                );
                return true;
            }

            let model = msg
                .get("model")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(String::from);
            let max_iterations = msg
                .get("maxIterations")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            let timeout_secs = msg
                .get("timeoutSecs")
                .and_then(|v| v.as_u64())
                .filter(|n| *n > 0);
            let enabled = !msg
                .get("disabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let watch_workspace = msg
                .get("watchWorkspace")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let entry = crate::schedule::Schedule {
                id: id.clone(),
                cron,
                // GUI schedule-add is recurring-only for now; one-shot
                // (--at/--in) is CLI-only until the modal mirrors it.
                run_at: None,
                cwd: std::path::PathBuf::from(cwd),
                prompt,
                model,
                // GUI modal doesn't expose heartbeat/resume yet — CLI-only.
                resume_session: None,
                max_iterations,
                timeout_secs,
                enabled,
                watch_workspace,
                last_run: None,
                last_exit: None,
            };
            let result = (|| -> crate::error::Result<()> {
                let mut store = crate::schedule::ScheduleStore::load()?;
                store.add(entry)?;
                store.save()
            })();
            match result {
                Ok(()) => {
                    (ctx.dispatch)(
                        serde_json::json!({
                            "type": "schedule_add_result",
                            "ok": true,
                            "id": id,
                        })
                        .to_string(),
                    );
                }
                Err(e) => {
                    (ctx.dispatch)(
                        serde_json::json!({
                            "type": "schedule_add_result",
                            "ok": false,
                            "error": format!("{e}"),
                        })
                        .to_string(),
                    );
                }
            }
        }

        "new_session" => {
            if let Some(request_id) = msg.get("view_request").and_then(Value::as_str) {
                let id = format!("draft-{}", uuid::Uuid::new_v4());
                let session = draft_session(&id, ctx).expect("generated draft id");
                let _ = ctx.shared.events_tx.send(
                    crate::shared_session::ViewEvent::SessionViewRequest {
                        session: Box::new(session),
                        request_id: request_id.to_string(),
                    },
                );
            } else {
                let _ = ctx.shared.input_tx.send(ShellInput::NewSession);
                (ctx.dispatch)(serde_json::json!({"type":"new_session_ack"}).to_string());
                (ctx.dispatch)(serde_json::json!({"type":"terminal_clear"}).to_string());
            }
        }

        // ── Plan sidebar (M6.36 SERVE9b — migrated from gui.rs) ─────
        "plan_approve" => {
            // M6.9 BUG C2 guard preserved: only act if there's an
            // unfinished plan to approve. Stale clicks / malformed IPCs
            // / races otherwise flip mode to Auto with no plan in scope.
            use crate::tools::plan_state::StepStatus;
            let plan = crate::tools::plan_state::get();
            let has_unfinished_plan = plan
                .as_ref()
                .map(|p| p.steps.iter().any(|s| s.status != StepStatus::Done))
                .unwrap_or(false);
            if has_unfinished_plan {
                crate::permissions::set_current_mode_and_broadcast(
                    crate::permissions::PermissionMode::Auto,
                );
                let _ = ctx
                    .shared
                    .input_tx
                    .send(ShellInput::Line("Begin executing the plan.".to_string()));
            }
        }

        "plan_cancel" => {
            // Restore pre-plan mode + clear the plan slot.
            let restored = crate::permissions::take_pre_plan_mode()
                .unwrap_or(crate::permissions::PermissionMode::Ask);
            crate::permissions::set_current_mode_and_broadcast(restored);
            crate::tools::plan_state::clear();
        }

        "plan_retry_step" => {
            // M6.7 status guard preserved: only Failed → InProgress.
            let step_id = msg
                .get("step_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !step_id.is_empty() {
                use crate::tools::plan_state::StepStatus;
                let current = crate::tools::plan_state::get()
                    .and_then(|p| p.step_by_id(&step_id).map(|s| s.status));
                if current == Some(StepStatus::Failed) {
                    let _ = crate::tools::plan_state::update_step(
                        &step_id,
                        StepStatus::InProgress,
                        None,
                    );
                    crate::tools::plan_state::reset_step_attempts_external();
                    let _ = ctx.shared.input_tx.send(ShellInput::Line(format!(
                        "Retry the failed step (\"{step_id}\")."
                    )));
                }
            }
        }

        "plan_skip_step" => {
            // Force-Done bypasses the normal gate (Failed → Done is
            // illegal via update_step). User's deliberate override;
            // audit note records it.
            let step_id = msg
                .get("step_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !step_id.is_empty() {
                let _ = crate::tools::plan_state::force_step_done(&step_id, "skipped by user");
                let _ = ctx.shared.input_tx.send(ShellInput::Line(format!(
                    "Step (\"{step_id}\") was skipped by the user. \
                     Continue with the next step in the plan."
                )));
            }
        }

        "plan_stalled_continue" => {
            // Reset stall + per-step attempt counters; nudge a turn.
            crate::tools::plan_state::reset_stall_counter_external();
            crate::tools::plan_state::reset_step_attempts_external();
            let _ = ctx.shared.input_tx.send(ShellInput::Line(
                "Continue with the plan. If you're stuck, commit to a UpdatePlanStep \
                 transition — either advance the current step to done, or mark it \
                 failed with a brief note so the user can retry / skip / abort."
                    .to_string(),
            ));
        }

        // ── Settings / theme (M6.36 SERVE9c — migrated from gui.rs) ─
        "theme_get" => {
            let payload = serde_json::json!({
                "type": "theme",
                "mode": crate::theme::load_theme(),
            });
            (ctx.dispatch)(payload.to_string());
        }

        "theme_set" => {
            let requested = msg.get("mode").and_then(|v| v.as_str()).unwrap_or("system");
            let normalized = crate::theme::normalize_theme(requested).to_string();
            crate::theme::save_theme(&normalized);
            let payload = serde_json::json!({
                "type": "theme",
                "mode": normalized,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "kms_list" => {
            (ctx.dispatch)(crate::kms::build_update_payload().to_string());
        }

        // M6.39.9: KMS browser — clicking a KMS title in the sidebar
        // emits `kms_browse` with the name; backend returns
        // `kms_browse_result` listing every page + source file. The
        // frontend renders this in the right-edge KMS browser panel.
        "kms_browse" => {
            let name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let payload = match crate::kms::browse(&name) {
                Some(listing) => serde_json::json!({
                    "type": "kms_browse_result",
                    "kms": listing.kms,
                    "pages": listing.pages,
                    "sources": listing.sources,
                    "entry": listing.entry,
                    "ok": true,
                }),
                None => serde_json::json!({
                    "type": "kms_browse_result",
                    "kms": name,
                    "pages": [],
                    "sources": [],
                    "ok": false,
                    "error": format!("KMS '{name}' not found"),
                }),
            };
            (ctx.dispatch)(payload.to_string());
        }

        // M6.39.13: KMS graph data — Obsidian-style nodes + edges
        // for the right-pane graph view. Fronted by clicking the
        // "Graph" button in `KmsBrowserSidebar`.
        "kms_graph" => {
            let name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let include_sources = msg
                .get("include_sources")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let payload = match crate::kms::graph(&name, include_sources) {
                Some(g) => serde_json::json!({
                    "type": "kms_graph_result",
                    "kms": g.kms,
                    "nodes": g.nodes,
                    "edges": g.edges,
                    "include_sources": include_sources,
                    "ok": true,
                }),
                None => serde_json::json!({
                    "type": "kms_graph_result",
                    "kms": name,
                    "nodes": [],
                    "edges": [],
                    "include_sources": include_sources,
                    "ok": false,
                    "error": format!("KMS '{name}' not found"),
                }),
            };
            (ctx.dispatch)(payload.to_string());
        }

        // M6.39.9: KMS file reader for the viewer overlay. Returns
        // raw markdown content; the frontend renders to HTML via
        // `marked`. `kind` is "page" or "source"; `name` is the
        // filename stem (no `.md`). Path-safety enforced server-side.
        "kms_read_file" => {
            let kms_name = msg
                .get("kms")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let kind = msg
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let file = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let payload = match crate::kms::read_browse_file(&kms_name, &kind, &file) {
                Ok(read) => {
                    // Absolute dir of this file so the viewer can resolve
                    // relative markdown image links (`![](alias-assets/x.png)`)
                    // through the /file-asset endpoint. Project-scoped KMS live
                    // under the workspace and serve fine; user-scoped roots sit
                    // outside it and the asset endpoint's sandbox will refuse
                    // them (images stay unresolved there — links are harmless).
                    let asset_base = crate::kms::resolve(&kms_name)
                        .map(|k| {
                            let sub = if kind == "source" { "sources" } else { "pages" };
                            k.root.join(sub).to_string_lossy().to_string()
                        })
                        .unwrap_or_default();
                    // Who points here. Computed on read, never stored:
                    // a backlink is a property of the graph, and a page
                    // that carried its own list would go stale the
                    // moment another note linked or unlinked it.
                    let backlinks: Vec<serde_json::Value> = if kind == "source" {
                        Vec::new()
                    } else {
                        let stem = file.trim_end_matches(".md");
                        crate::kms::backlinks(&kms_name, stem)
                            .into_iter()
                            .map(|(slug, title)| serde_json::json!({"slug": slug, "title": title}))
                            .collect()
                    };
                    serde_json::json!({
                        "type": "kms_file_content",
                        "kms": kms_name,
                        "kind": kind,
                        "name": file,
                        "content": read.content,
                        "total_bytes": read.total_bytes,
                        "truncated": read.truncated,
                        "asset_base": asset_base,
                        "backlinks": backlinks,
                        "ok": true,
                    })
                }
                Err(e) => serde_json::json!({
                    "type": "kms_file_content",
                    "kms": kms_name,
                    "kind": kind,
                    "name": file,
                    "content": "",
                    "ok": false,
                    "error": format!("{e}"),
                }),
            };
            (ctx.dispatch)(payload.to_string());
        }

        // Delete `.thclaws/todos.md` from disk and broadcast an empty
        // TodoUpdate so the sidebar (and any future renders) reflect
        // the cleared state. Triggered by TodoSidebar when the user
        // closes a fully-completed list — the prior session's "all
        // done" checkboxes shouldn't bleed into the next session as
        // a stale checked list.
        "clear_todos" => {
            let path = std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .join(".thclaws")
                .join("state")
                .join("todos.md");
            let removed = std::fs::remove_file(&path).is_ok();
            // Broadcast through the proper channel so every subscriber
            // (chat tab, terminal-translator, etc.) gets the update.
            let _ = ctx
                .shared
                .events_tx
                .send(crate::shared_session::ViewEvent::TodoUpdate(Vec::new()));
            let payload = serde_json::json!({
                "type": "todos_cleared",
                "removed": removed,
                "path": path.to_string_lossy(),
            });
            (ctx.dispatch)(payload.to_string());
        }

        // Plan-07 Phase 1.3 — LINE-bridge wiring. The GUI
        // LineConnectModal hits these three; the bridge itself
        // (WS + reply) lives in the worker so cancellation
        // happens off a single tokio task.
        "line_status" => {
            // Read from disk — paired ↔ saved config exists. The
            // worker's `state.line_session` is the truth for
            // "is the WS task running RIGHT NOW", but for first-
            // paint we only need "is this install paired?", which
            // is a cheap file existence check.
            let (state_str, server_url, display_name, picture_url) =
                match crate::line::LineConfig::load() {
                    Ok(Some(cfg)) => (
                        "connected".to_string(),
                        cfg.resolved_server_url(),
                        cfg.display_name.clone(),
                        cfg.picture_url.clone(),
                    ),
                    _ => ("disconnected".to_string(), String::new(), None, None),
                };
            let payload = serde_json::json!({
                "type": "line_status",
                "state": state_str,
                "server_url": server_url,
                "pending_approvals": 0,
                "display_name": display_name,
                "picture_url": picture_url,
            });
            (ctx.dispatch)(payload.to_string());
        }
        "line_pair" => {
            let code = msg
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let cwd = msg
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| ".".into())
                });
            let machine_label = msg
                .get("machine_label")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    std::env::var("HOSTNAME")
                        .or_else(|_| std::env::var("COMPUTERNAME"))
                        .unwrap_or_else(|_| "this-machine".into())
                });
            let server_url = std::env::var("THCLAWS_LINE_SERVER")
                .ok()
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| {
                    crate::line::config::DEFAULT_SERVER_URL
                        .trim_end_matches('/')
                        .to_string()
                });
            let pair_url = format!("{server_url}/pair");
            let input_tx = ctx.shared.input_tx.clone();
            let dispatch = ctx.dispatch.clone();
            tokio::spawn(async move {
                let body = serde_json::json!({
                    "code": code,
                    "cwd": cwd,
                    "machine_label": machine_label,
                });
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(15))
                    .build()
                    .expect("reqwest client build");
                let resp = match client.post(&pair_url).json(&body).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        let payload = serde_json::json!({
                            "type": "line_pair_result",
                            "ok": false,
                            "error": format!("relay HTTP: {e}"),
                        });
                        (dispatch)(payload.to_string());
                        return;
                    }
                };
                let status = resp.status();
                let response_text = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    let payload = serde_json::json!({
                        "type": "line_pair_result",
                        "ok": false,
                        "error": format!("relay {status}: {response_text}"),
                    });
                    (dispatch)(payload.to_string());
                    return;
                }
                // Expected shape:
                //   {token, line_user_id, expires_at,
                //    display_name?, picture_url?, language?}
                // Profile fields are optional — older relays don't
                // send them; relay also omits when LINE API fetch
                // failed.
                let parsed: serde_json::Value =
                    serde_json::from_str(&response_text).unwrap_or(serde_json::Value::Null);
                let token = parsed
                    .get("token")
                    .and_then(|t| t.as_str())
                    .map(String::from);
                let token = match token {
                    Some(t) if !t.is_empty() => t,
                    _ => {
                        let payload = serde_json::json!({
                            "type": "line_pair_result",
                            "ok": false,
                            "error": "relay response missing 'token'",
                        });
                        (dispatch)(payload.to_string());
                        return;
                    }
                };
                let pick_str = |key: &str| -> Option<String> {
                    parsed.get(key).and_then(|v| v.as_str()).map(String::from)
                };
                let display_name = pick_str("display_name");
                let picture_url = pick_str("picture_url");
                let language = pick_str("language");
                let cfg = crate::line::LineConfig {
                    binding_token: token,
                    server_url: Some(server_url.clone()),
                    display_name: display_name.clone(),
                    picture_url: picture_url.clone(),
                    language,
                };
                if let Err(e) = cfg.save() {
                    let payload = serde_json::json!({
                        "type": "line_pair_result",
                        "ok": false,
                        "error": format!("save config: {e}"),
                    });
                    (dispatch)(payload.to_string());
                    return;
                }
                // Hand off to the worker so the WS task lifetime
                // is owned where the cancel token already lives.
                let _ = input_tx.send(crate::shared_session::ShellInput::LineConnect(cfg));
                let payload = serde_json::json!({
                    "type": "line_pair_result",
                    "ok": true,
                    "server_url": server_url,
                });
                (dispatch)(payload.to_string());
            });
        }
        "line_disconnect" => {
            let _ = ctx
                .shared
                .input_tx
                .send(crate::shared_session::ShellInput::LineDisconnect);
            let payload = serde_json::json!({
                "type": "line_disconnect_ack",
                "ok": true,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // ── Phone-home tunnel wiring (dev-plan/44 Tier 1) ──────────
        // The cloud-token pairing that writes `.thclaws/phone-home.json`
        // is a follow-up; `phone_home_connect` reconnects an existing
        // binding (the worker also auto-reconnects one on boot).
        "phone_home_connect" => {
            let payload = match crate::phone_home::PhoneHomeConfig::load() {
                Ok(Some(cfg)) => {
                    let _ = ctx
                        .shared
                        .input_tx
                        .send(crate::shared_session::ShellInput::PhoneHomeConnect(cfg));
                    serde_json::json!({ "type": "phone_home_connect_ack", "ok": true })
                }
                Ok(None) => serde_json::json!({
                    "type": "phone_home_connect_ack",
                    "ok": false,
                    "error": "no phone-home binding on disk — pair first",
                }),
                Err(e) => serde_json::json!({
                    "type": "phone_home_connect_ack",
                    "ok": false,
                    "error": e.to_string(),
                }),
            };
            (ctx.dispatch)(payload.to_string());
        }
        "phone_home_disconnect" => {
            let _ = ctx
                .shared
                .input_tx
                .send(crate::shared_session::ShellInput::PhoneHomeDisconnect);
            let payload = serde_json::json!({
                "type": "phone_home_disconnect_ack",
                "ok": true,
            });
            (ctx.dispatch)(payload.to_string());
        }
        "phone_home_pair" => {
            // Exchange the stored cloud CLI token for a phone-home binding,
            // then connect. The worker does the network round-trip; we
            // ack immediately (with a clear error if not logged in).
            let payload = if crate::cloud::token().is_some() {
                let _ = ctx
                    .shared
                    .input_tx
                    .send(crate::shared_session::ShellInput::PhoneHomePair);
                serde_json::json!({ "type": "phone_home_pair_ack", "ok": true, "pending": true })
            } else {
                serde_json::json!({
                    "type": "phone_home_pair_ack",
                    "ok": false,
                    "error": "log in to thClaws.cloud first (Settings → thClaws.cloud)",
                })
            };
            (ctx.dispatch)(payload.to_string());
        }

        // ── Telegram bridge wiring (dev-plan/29 Tier 1) ────────────
        // The GUI TelegramConnectModal hits these; the polling session
        // itself lives on the worker so its cancel token sits on one
        // tokio task (mirrors the LINE handlers above).
        "telegram_status" => {
            // Live status (pending pairings + counts) lives in the
            // worker's in-memory handle — ask it to broadcast a fresh
            // snapshot rather than reading disk. The worker answers with
            // a disconnected payload when no session is active.
            let _ = ctx.shared.input_tx.send(ShellInput::TelegramStatusRequest);
        }
        "telegram_connect" => {
            let token = msg
                .get("bot_token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            // A blank token is only valid when TELEGRAM_BOT_TOKEN is set
            // — let the worker's getMe be the final arbiter, but catch an
            // obviously-malformed pasted token here for a fast error.
            if !token.is_empty() {
                if let Err(e) = crate::telegram::config::validate_token(&token) {
                    let payload = serde_json::json!({
                        "type": "telegram_connect_ack",
                        "ok": false,
                        "error": e.to_string(),
                    });
                    (ctx.dispatch)(payload.to_string());
                    return true;
                }
            }
            // Merge onto any existing on-disk config so we don't clobber
            // allow_from / policy when the user re-pastes a token.
            let mut cfg = crate::telegram::TelegramConfig::load()
                .ok()
                .flatten()
                .unwrap_or_default();
            cfg.enabled = true;
            if !token.is_empty() {
                cfg.bot_token = Some(token);
            }
            if let Err(e) = cfg.save() {
                let payload = serde_json::json!({
                    "type": "telegram_connect_ack",
                    "ok": false,
                    "error": format!("save config: {e}"),
                });
                (ctx.dispatch)(payload.to_string());
                return true;
            }
            let _ = ctx.shared.input_tx.send(ShellInput::TelegramConnect(cfg));
            let payload = serde_json::json!({
                "type": "telegram_connect_ack",
                "ok": true,
            });
            (ctx.dispatch)(payload.to_string());
        }
        "telegram_disconnect" => {
            let _ = ctx.shared.input_tx.send(ShellInput::TelegramDisconnect);
            let payload = serde_json::json!({
                "type": "telegram_disconnect_ack",
                "ok": true,
            });
            (ctx.dispatch)(payload.to_string());
        }
        "telegram_pairing_approve" => {
            if let Some(code) = msg.get("code").and_then(|v| v.as_str()) {
                let _ = ctx
                    .shared
                    .input_tx
                    .send(ShellInput::TelegramPairingApprove {
                        code: code.to_string(),
                    });
            }
        }
        "telegram_pairing_reject" => {
            if let Some(code) = msg.get("code").and_then(|v| v.as_str()) {
                let _ = ctx.shared.input_tx.send(ShellInput::TelegramPairingReject {
                    code: code.to_string(),
                });
            }
        }

        // ── Messenger bridge wiring (dev-plan/31) ──────────────────
        // The GUI MessengerConnectModal hits these. Pairing redemption
        // mirrors `line_pair`: POST the relay's /pair with the code the
        // relay DMed the user, save the binding JWT, hand off to the
        // worker. Status / disconnect mirror the LINE arms.
        "messenger_status" => {
            let _ = ctx.shared.input_tx.send(ShellInput::MessengerStatusRequest);
        }
        "messenger_pair" => {
            let code = msg
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let cwd = msg
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| ".".into())
                });
            let machine_label = msg
                .get("machine_label")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    std::env::var("HOSTNAME")
                        .or_else(|_| std::env::var("COMPUTERNAME"))
                        .unwrap_or_else(|_| "this-machine".into())
                });
            let server_url = std::env::var("THCLAWS_MESSENGER_SERVER")
                .ok()
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| {
                    crate::messenger::config::DEFAULT_SERVER_URL
                        .trim_end_matches('/')
                        .to_string()
                });
            let pair_url = format!("{server_url}/pair");
            let input_tx = ctx.shared.input_tx.clone();
            let dispatch = ctx.dispatch.clone();
            tokio::spawn(async move {
                let body = serde_json::json!({
                    "code": code,
                    "cwd": cwd,
                    "machine_label": machine_label,
                });
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(15))
                    .build()
                    .expect("reqwest client build");
                let resp = match client.post(&pair_url).json(&body).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        let payload = serde_json::json!({
                            "type": "messenger_pair_result",
                            "ok": false,
                            "error": format!("relay HTTP: {e}"),
                        });
                        (dispatch)(payload.to_string());
                        return;
                    }
                };
                let status = resp.status();
                let response_text = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    let payload = serde_json::json!({
                        "type": "messenger_pair_result",
                        "ok": false,
                        "error": format!("relay {status}: {response_text}"),
                    });
                    (dispatch)(payload.to_string());
                    return;
                }
                let parsed: serde_json::Value =
                    serde_json::from_str(&response_text).unwrap_or(serde_json::Value::Null);
                let token = parsed
                    .get("token")
                    .and_then(|t| t.as_str())
                    .filter(|t| !t.is_empty())
                    .map(String::from);
                let Some(token) = token else {
                    let payload = serde_json::json!({
                        "type": "messenger_pair_result",
                        "ok": false,
                        "error": "relay response missing 'token'",
                    });
                    (dispatch)(payload.to_string());
                    return;
                };
                let cfg = crate::messenger::MessengerConfig {
                    binding_token: token,
                    server_url: Some(server_url.clone()),
                    page_name: None,
                    page_id: None,
                };
                if let Err(e) = cfg.save() {
                    let payload = serde_json::json!({
                        "type": "messenger_pair_result",
                        "ok": false,
                        "error": format!("save config: {e}"),
                    });
                    (dispatch)(payload.to_string());
                    return;
                }
                let _ = input_tx.send(crate::shared_session::ShellInput::MessengerConnect(cfg));
                let payload = serde_json::json!({
                    "type": "messenger_pair_result",
                    "ok": true,
                    "server_url": server_url,
                });
                (dispatch)(payload.to_string());
            });
        }
        "messenger_disconnect" => {
            let _ = ctx
                .shared
                .input_tx
                .send(crate::shared_session::ShellInput::MessengerDisconnect);
            let payload = serde_json::json!({
                "type": "messenger_disconnect_ack",
                "ok": true,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // ── Working directory (M6.36 SERVE9d — migrated from gui.rs) ─
        "get_cwd" => {
            // dev-plan/61: the folder the user's files are in, which for an
            // agent under a workspace host is the workspace, not its own folder.
            let cwd = crate::workdir::workspace_root()
                .to_string_lossy()
                .to_string();
            // Serve mode: cwd is fixed (cloud runner template mounts
            // `/workspace`), so skip the picker modal. Also resolve
            // `guiShell.tabDefault` and pass it through as `initial_tab`
            // — the frontend uses this to land on the UI tab when a
            // shell is pinned, instead of always defaulting to terminal.
            let tab_default = crate::config::AppConfig::load().ok().and_then(|c| {
                c.gui_shell
                    .and_then(|s| s.tab_default().map(str::to_string))
            });
            let initial_tab = if tab_default.is_some() {
                Some("ui")
            } else {
                None
            };
            let payload = serde_json::json!({
                "type": "current_cwd",
                "path": cwd,
                // #211: and not again after a pick re-execed the app.
                "needs_modal": !ctx.is_serve_mode && !crate::util::picker_answered(),
                "recent_dirs": crate::recent_dirs::load_recent_dirs(),
                "initial_tab": initial_tab,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "set_cwd" => {
            // dev-plan/42: in a multiuser serve pod, switching the
            // *process* cwd would relocate every tenant's working dir.
            // Refuse — each user's root is fixed to their workspace-<id>/
            // via the task-local scope. (Desktop / single-tenant serve is
            // unaffected.)
            if crate::workdir::is_multiuser() {
                return true;
            }
            if let Some(path) = msg.get("path").and_then(|v| v.as_str()) {
                let p = std::path::Path::new(path);
                if p.is_dir() {
                    let _ = std::env::set_current_dir(p);
                    // A folder is chosen now, so the writes start-up held back
                    // belong here — in this folder, not whichever one the OS
                    // started the app in. The marker outlives a re-exec, so a
                    // pick that restarts instead bootstraps at start-up.
                    crate::util::mark_picker_answered();
                    crate::config::ProjectConfig::migrate_workspace_if_needed();
                    crate::config::ProjectConfig::ensure_default_exists();
                    let _ = crate::sandbox::Sandbox::init();
                    crate::recent_dirs::save_recent_dir(path);
                    // Tell the worker to reload project settings + swap
                    // model from the new project's settings.json.
                    let _ = ctx
                        .shared
                        .input_tx
                        .send(ShellInput::ChangeCwd(p.to_path_buf()));
                    let payload = serde_json::json!({
                        "type": "cwd_changed",
                        "path": path,
                        "ok": true,
                    });
                    (ctx.dispatch)(payload.to_string());
                } else {
                    let payload = serde_json::json!({
                        "type": "cwd_changed",
                        "path": path,
                        "ok": false,
                        "error": format!("'{}' is not a valid directory", path),
                    });
                    (ctx.dispatch)(payload.to_string());
                }
            }
        }

        // ── AGENTS.md instructions editor (M6.36 SERVE9d) ──────────
        "instructions_get" => {
            let scope = msg
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("folder");
            let path = crate::instructions::instructions_path(scope);
            let content = path
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_default();
            let payload = serde_json::json!({
                "type": "instructions_content",
                "scope": scope,
                "path": path.as_ref().map(|p| p.display().to_string()),
                "content": content,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "instructions_save" => {
            let scope = msg
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("folder");
            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, error, path) = match crate::instructions::instructions_path(scope) {
                Some(path) => {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match std::fs::write(&path, content) {
                        Ok(()) => (true, String::new(), Some(path.display().to_string())),
                        Err(e) => (false, e.to_string(), Some(path.display().to_string())),
                    }
                }
                None => (
                    false,
                    "path not resolvable (home directory unavailable)".into(),
                    None,
                ),
            };
            // Trigger an in-place system-prompt rebuild on the running
            // worker — without this, an edit-and-save cycle in the
            // Settings menu only takes effect on the next session.
            if ok {
                let _ = ctx.shared.input_tx.send(ShellInput::InstructionsChanged);
            }
            let payload = serde_json::json!({
                "type": "instructions_save_result",
                "scope": scope,
                "path": path,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // ── Agent editor (/agent new · /agent edit) ────────────────
        "agent_save" => {
            // Frontend AgentEditorModal submits the full `.md` body
            // (YAML frontmatter + system prompt). Always write the
            // project-scoped path `.thclaws/agents/<name>.md` — edits to
            // a user-scoped or built-in agent land here as an override.
            let raw_name = msg.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let body = msg.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, error, path) = match crate::agent_defs::sanitize_agent_name(raw_name) {
                None => (
                    false,
                    "invalid agent name (letters, digits, '-' or '_' only)".to_string(),
                    None,
                ),
                Some(name) => {
                    let path = crate::agent_defs::AgentDefsConfig::project_agent_path(&name);
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match std::fs::write(&path, body) {
                        Ok(()) => (true, String::new(), Some(path.display().to_string())),
                        Err(e) => (false, e.to_string(), Some(path.display().to_string())),
                    }
                }
            };
            // Reload the worker's def snapshot so the new/edited agent is
            // usable in-session (side-channel spawns + existence checks).
            if ok {
                let _ = ctx.shared.input_tx.send(ShellInput::AgentDefsChanged);
            }
            let payload = serde_json::json!({
                "type": "agent_save_result",
                "name": raw_name,
                "path": path,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // ── Deploy target (dev-plan/28: /deploy command config) ────
        "remote_agent_get" => {
            let url = crate::remote_agent::url();
            // Resolve the token to learn whether one is stored AND
            // how long it is — the length is what powers the
            // ••••• sentinel sizing in the Settings modal (matches
            // the api_key_status row shape). The value itself is
            // NEVER returned to the frontend.
            let token_resolved = crate::remote_agent::token();
            let has_token = token_resolved.is_some();
            let token_length = token_resolved.as_deref().map(|t| t.len()).unwrap_or(0);
            let env_var_set = std::env::var("THCLAWS_REMOTE_AGENT_TOKEN")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false);
            let payload = serde_json::json!({
                "type": "remote_agent_config",
                "url": url,
                "has_token": has_token,
                "token_length": token_length,
                "env_var_set": env_var_set,
                "keychain_writable": crate::remote_agent::keychain_writable(),
            });
            (ctx.dispatch)(payload.to_string());
        }

        "remote_agent_set" => {
            // url and token are independent — either can be omitted to
            // update only one. Empty string explicitly clears.
            let url_arg = msg.get("url").and_then(|v| v.as_str());
            let token_arg = msg.get("token").and_then(|v| v.as_str());
            let mut url_ok = true;
            let mut url_err = String::new();
            let mut token_ok = true;
            let mut token_err = String::new();

            if let Some(url) = url_arg {
                let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
                let normalized = if url.trim().is_empty() {
                    None
                } else {
                    Some(url)
                };
                project.set_remote_agent_url(normalized);
                if let Err(e) = project.save() {
                    url_ok = false;
                    url_err = format!("settings.json write failed: {e}");
                }
            }

            if let Some(token) = token_arg {
                let trimmed = token.trim();
                let result = if trimmed.is_empty() {
                    crate::remote_agent::clear_token()
                } else {
                    crate::remote_agent::set_token(trimmed)
                };
                if let Err(e) = result {
                    token_ok = false;
                    token_err = format!("{e}");
                }
            }

            let payload = serde_json::json!({
                "type": "remote_agent_result",
                "url_ok": url_ok,
                "url_error": url_err,
                "token_ok": token_ok,
                "token_error": token_err,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // ── thClaws.cloud catalog (dev-plan/34) ────────────────────
        // Same shape as remote_agent_get/set above. URL persists to
        // settings.json::cloud.url; token persists to the active
        // secrets backend (keychain or ~/.config/thclaws/.env), same
        // bundle as provider API keys.
        "cloud_config_get" => {
            let url = crate::cloud::persisted_url();
            let token_resolved = crate::cloud::token();
            let has_token = token_resolved.is_some();
            let token_length = token_resolved.as_deref().map(|t| t.len()).unwrap_or(0);
            let env_var_set = std::env::var(crate::cloud::ENV_TOKEN)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false);
            let payload = serde_json::json!({
                "type": "cloud_config",
                "url": url,
                "default_url": crate::cloud::DEFAULT_CLOUD_URL,
                "has_token": has_token,
                "token_length": token_length,
                "env_var_set": env_var_set,
                "token_writable": crate::cloud::token_writable(),
            });
            (ctx.dispatch)(payload.to_string());
        }

        "cloud_config_set" => {
            let url_arg = msg.get("url").and_then(|v| v.as_str());
            let token_arg = msg.get("token").and_then(|v| v.as_str());
            let mut url_ok = true;
            let mut url_err = String::new();
            let mut token_ok = true;
            let mut token_err = String::new();

            if let Some(url) = url_arg {
                let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
                let normalized = if url.trim().is_empty() {
                    None
                } else {
                    Some(url)
                };
                project.set_cloud_url(normalized);
                if let Err(e) = project.save() {
                    url_ok = false;
                    url_err = format!("settings.json write failed: {e}");
                }
            }

            if let Some(token) = token_arg {
                let trimmed = token.trim();
                let result = if trimmed.is_empty() {
                    crate::cloud::clear_token()
                } else if !crate::cloud::looks_like_cli_token(trimmed) {
                    Err(crate::error::Error::Config(
                        "that is not a thClaws.cloud CLI token — they start with thc_. Copy it again from thclaws.cloud → Settings → CLI tokens.".into(),
                    ))
                } else {
                    crate::cloud::set_token(trimmed)
                };
                if let Err(e) = result {
                    token_ok = false;
                    token_err = format!("{e}");
                }
            }

            let payload = serde_json::json!({
                "type": "cloud_config_result",
                "url_ok": url_ok,
                "url_error": url_err,
                "token_ok": token_ok,
                "token_error": token_err,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // ── Agent identity (dev-plan/34 Option A) ──────────────────
        // settings.json::agent block — the folder's authoritative
        // {id, name, description, uuid}. UUID is server-managed (set
        // by `cloud publish`, cleared by `cloud unbind`); the GUI lets
        // the user edit the other three + read the UUID.
        "agent_config_get" => {
            let agent = crate::config::ProjectConfig::load().and_then(|c| c.agent.clone());
            let payload = match agent {
                Some(a) => serde_json::json!({
                    "type": "agent_config",
                    "exists": true,
                    "id": a.id,
                    "name": a.name,
                    "description": a.description,
                    "uuid": a.uuid,
                }),
                None => serde_json::json!({
                    "type": "agent_config",
                    "exists": false,
                    "id": null,
                    "name": null,
                    "description": null,
                    "uuid": null,
                }),
            };
            (ctx.dispatch)(payload.to_string());
        }

        "agent_config_set" => {
            // UUID is deliberately NOT writable from the UI — it's
            // server-assigned. Empty-string for id/name/description
            // means "clear this field"; absent fields mean "no change".
            let id = msg.get("id").and_then(|v| v.as_str());
            let name = msg.get("name").and_then(|v| v.as_str());
            let description = msg.get("description").and_then(|v| v.as_str());

            let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
            // Convert "" → None so a cleared input drops the field;
            // a non-empty value updates it; an absent field is ignored
            // (preserves existing value — partial update). merge_agent's
            // None-as-no-change semantics fit publish-side writeback;
            // here we need explicit-clear, so we mutate `current`
            // directly so that field-present-but-empty becomes None.
            let normalize = |s: &str| -> Option<String> {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            };
            let mut current = project.agent.clone().unwrap_or_default();
            if let Some(v) = id {
                current.id = normalize(v);
            }
            if let Some(v) = name {
                current.name = normalize(v);
            }
            if let Some(v) = description {
                current.description = normalize(v);
            }
            let all_empty = current.id.is_none()
                && current.name.is_none()
                && current.description.is_none()
                && current.uuid.is_none();
            project.agent = if all_empty { None } else { Some(current) };

            let (ok, error) = match project.save() {
                Ok(()) => (true, String::new()),
                Err(e) => (false, format!("settings.json write failed: {e}")),
            };
            let payload = serde_json::json!({
                "type": "agent_config_result",
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "agent_unbind" => {
            let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
            let had_uuid = project
                .agent
                .as_ref()
                .and_then(|a| a.uuid.as_ref())
                .is_some();
            project.clear_agent_uuid();
            let (ok, error) = match project.save() {
                Ok(()) => (true, String::new()),
                Err(e) => (false, format!("settings.json write failed: {e}")),
            };
            let payload = serde_json::json!({
                "type": "agent_unbind_result",
                "ok": ok,
                "error": error,
                "had_uuid": had_uuid,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // ── Settings panel (M6.36 SERVE9e — migrated from gui.rs) ──
        "secrets_backend_get" => {
            // Hosted-workspace short-circuit. Two cloud variants both
            // pre-inject everything the engine needs at pod-start, so
            // the first-launch backend picker has nothing to decide:
            //   - Gateway-routed (THCLAWS_GATEWAY_API_KEY set) — all
            //     provider calls go through the gateway.
            //   - BYOK on cloud (just THCLAWS_WORKSPACE_ID set) —
            //     per-provider keys are decrypted and injected as env
            //     vars by the K8sProvisioner, never touching keychain
            //     or .env in the pod.
            // Both cases return the synthetic "hosted" sentinel which
            // also drives frontend chrome that's irrelevant in a
            // cloud workspace (e.g. the SSO Sign-in button — the
            // visitor is already authenticated at the routing layer).
            let in_hosted_workspace = std::env::var("THCLAWS_WORKSPACE_ID")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
                || std::env::var("THCLAWS_GATEWAY_API_KEY")
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false);
            let backend = if in_hosted_workspace {
                Some("hosted".to_string())
            } else {
                crate::secrets::get_backend().map(|b| b.as_str().to_string())
            };
            let payload = serde_json::json!({
                "type": "secrets_backend",
                "backend": backend,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "secrets_backend_set" => {
            let choice = msg.get("backend").and_then(|v| v.as_str()).unwrap_or("");
            let backend = match choice {
                "keychain" => Some(crate::secrets::Backend::Keychain),
                "dotenv" => Some(crate::secrets::Backend::Dotenv),
                _ => None,
            };
            let (ok, error) = match backend {
                Some(b) => match crate::secrets::set_backend(b) {
                    Ok(()) => (true, String::new()),
                    Err(e) => (false, e.to_string()),
                },
                None => (false, format!("unknown backend '{choice}'")),
            };
            let payload = serde_json::json!({
                "type": "secrets_backend_result",
                "backend": choice,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "api_key_status" => {
            let statuses: Vec<serde_json::Value> = crate::secrets::status()
                .into_iter()
                .map(|s| {
                    serde_json::json!({
                        "provider": s.provider,
                        "env_var": s.env_var,
                        "configured_in_keychain": s.configured_in_keychain,
                        "env_set": matches!(s.env_source, crate::secrets::KeySource::Environment),
                        "key_length": s.key_length,
                        "kind": s.kind,
                        "featured": s.featured,
                        "default_model": s.default_model,
                    })
                })
                .collect();
            let payload = serde_json::json!({
                "type": "api_key_status",
                "keys": statuses,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "api_key_clear" => {
            let provider = msg.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            let keychain = crate::secrets::clear(provider);
            let env_var = crate::providers::ProviderKind::from_name(provider)
                .and_then(|k| k.api_key_env())
                .or_else(|| crate::secrets::service_env_var(provider));
            if let Some(var) = env_var {
                std::env::remove_var(var);
                let _ = crate::dotenv::remove_from_user_env(var);
            }
            let (ok, error) = match keychain {
                Ok(()) => (true, String::new()),
                Err(e) => (true, format!("keychain remove warning: {e}")),
            };
            let payload = serde_json::json!({
                "type": "api_key_result",
                "action": "clear",
                "provider": provider,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
            let _ = ctx
                .shared
                .input_tx
                .send(crate::shared_session::ShellInput::ReloadConfig);
        }

        "endpoint_status" => {
            let statuses: Vec<serde_json::Value> = crate::endpoints::status()
                .into_iter()
                .map(|e| {
                    serde_json::json!({
                        "provider": e.provider,
                        "env_var": e.env_var,
                        "configured_url": e.configured_url,
                        "default_url": e.default_url,
                    })
                })
                .collect();
            let payload = serde_json::json!({
                "type": "endpoint_status",
                "endpoints": statuses,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "endpoint_set" => {
            let provider = msg.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            let url = msg.get("url").and_then(|v| v.as_str()).unwrap_or("").trim();
            let (ok, error) = if provider.is_empty() || url.is_empty() {
                (false, "provider and url are required".to_string())
            } else {
                match crate::endpoints::set(provider, url) {
                    Ok(()) => {
                        if let Some(kind) = crate::providers::ProviderKind::from_name(provider) {
                            if let Some(var) = kind.endpoint_env() {
                                std::env::set_var(var, url.trim_end_matches('/'));
                            }
                        }
                        (true, String::new())
                    }
                    Err(e) => (false, e.to_string()),
                }
            };
            let payload = serde_json::json!({
                "type": "endpoint_result",
                "action": "set",
                "provider": provider,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "endpoint_clear" => {
            let provider = msg.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, error) = match crate::endpoints::clear(provider) {
                Ok(()) => {
                    if let Some(kind) = crate::providers::ProviderKind::from_name(provider) {
                        if let Some(var) = kind.endpoint_env() {
                            std::env::remove_var(var);
                        }
                    }
                    (true, String::new())
                }
                Err(e) => (false, e.to_string()),
            };
            let payload = serde_json::json!({
                "type": "endpoint_result",
                "action": "clear",
                "provider": provider,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "model_set" => {
            let model = msg
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if !model.is_empty() {
                let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
                project.set_model(&model);
                let _ = project.save();
                let new_cfg = crate::config::AppConfig::load().unwrap_or_default();
                let provider_name = new_cfg.detect_provider().unwrap_or("unknown");
                let ready = crate::providers::provider_has_credentials(&new_cfg);
                let broadcast = serde_json::json!({
                    "type": "provider_update",
                    "provider": provider_name,
                    "model": new_cfg.model,
                    "thinking": crate::providers::ThinkingLevel::json(new_cfg.thinking_budget),
                    "provider_ready": ready,
                });
                (ctx.dispatch)(broadcast.to_string());
                let _ = ctx
                    .shared
                    .input_tx
                    .send(crate::shared_session::ShellInput::ReloadConfig);
            }
        }

        // Sidebar thinking selector. Persist first, then let the
        // worker's ReloadConfig rebuild the agent from the saved value
        // (rebuild_agent reads `config.thinking_budget`).
        "thinking_set" => {
            let raw = msg
                .get("level")
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::Null => "auto".to_string(),
                    other => other.to_string(),
                })
                .unwrap_or_else(|| "auto".to_string());
            match crate::providers::ThinkingLevel::parse(&raw) {
                Some(b) => {
                    let _ = crate::config::ProjectConfig::persist_thinking_budget(b);
                    (ctx.dispatch)(crate::providers::ThinkingLevel::update_payload(b).to_string());
                    let _ = ctx
                        .shared
                        .input_tx
                        .send(crate::shared_session::ShellInput::ReloadConfig);
                }
                None => eprintln!("[thinking_set] unknown level {raw:?}"),
            }
        }

        "config_poll" => {
            let cfg = crate::config::AppConfig::load().unwrap_or_default();
            let provider = cfg.detect_provider().unwrap_or("unknown");
            let has_key = crate::providers::provider_has_credentials(&cfg);
            let payload = serde_json::json!({
                "type": "provider_update",
                "provider": provider,
                "model": cfg.model,
                "thinking": crate::providers::ThinkingLevel::json(cfg.thinking_budget),
                "provider_ready": has_key,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "clipboard_read" => {
            let (ok, text) = match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                Ok(t) => (true, t),
                Err(_) => (false, String::new()),
            };
            use base64::Engine;
            let text_b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
            let payload = serde_json::json!({
                "type": "clipboard_text",
                "ok": ok,
                "text": text,
                "text_b64": text_b64,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "clipboard_write" => {
            let text = msg.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let _ = arboard::Clipboard::new().and_then(|mut c| c.set_text(text.to_string()));
        }

        // ── PTY-backed Shell tab ───────────────────────────────────
        // Distinct from `shell_input` (agent prompt) and from
        // `gui_shell_*` (iframe-loaded UI tab). One global session at
        // a time; `pty_open` replaces any existing session. Output
        // flows back as `pty_data` (base64 bytes) / `pty_exit` events
        // emitted by the reader thread inside `shell_pty::open`.
        #[cfg(feature = "gui")]
        "pty_open" => {
            // Opt-in gate. Without `shellTabEnabled: true` in
            // .thclaws/settings.json we refuse to spawn — protects
            // against a stale frontend that still has the tab cached
            // or an external caller poking at the IPC. The frontend
            // also filters the tab visibility based on this flag.
            let enabled = crate::config::ProjectConfig::load()
                .and_then(|c| c.shell_tab_enabled)
                .unwrap_or(false);
            if !enabled {
                let payload = serde_json::json!({
                    "type": "pty_open_result",
                    "ok": false,
                    "error": "shell tab is opt-in — set `shellTabEnabled: true` in .thclaws/settings.json to enable",
                });
                (ctx.dispatch)(payload.to_string());
                return true;
            }
            let cmd = msg
                .get("cmd")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(crate::shell_pty::default_shell);
            let args: Vec<String> = msg
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            // Resolve cwd: explicit `cwd` in the payload wins, else
            // fall back to the worker process's current_dir() — that's
            // the workspace folder set by the StartupModal / ChangeCwd
            // flow (`std::env::set_current_dir`). Without this fallback,
            // portable-pty just inherits whatever cwd the binary
            // happened to launch from, which can be the user's home or
            // an arbitrary path. Explicit beats implicit.
            let cwd = msg
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| {
                    Some(crate::workdir::workspace_root()).map(|p| p.to_string_lossy().to_string())
                });
            let cols = msg.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
            let rows = msg.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
            let result = crate::shell_pty::open(
                &cmd,
                &args,
                cwd.as_deref(),
                cols,
                rows,
                ctx.dispatch.clone(),
            );
            let payload = match result {
                Ok(()) => serde_json::json!({
                    "type": "pty_open_result",
                    "ok": true,
                    "cmd": cmd,
                    "cwd": cwd,
                }),
                Err(e) => serde_json::json!({
                    "type": "pty_open_result",
                    "ok": false,
                    "error": e,
                }),
            };
            (ctx.dispatch)(payload.to_string());
        }

        #[cfg(feature = "gui")]
        "pty_input" => {
            // Frontend ships keystrokes as base64 (xterm.js may surface
            // bytes that aren't valid UTF-8 — Alt-key escapes, etc. —
            // and JSON strings can't carry those losslessly).
            use base64::Engine;
            let data_b64 = msg.get("data").and_then(|v| v.as_str()).unwrap_or("");
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .unwrap_or_default();
            if !bytes.is_empty() {
                let _ = crate::shell_pty::write(&bytes);
            }
        }

        #[cfg(feature = "gui")]
        "pty_resize" => {
            let cols = msg.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
            let rows = msg.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
            let _ = crate::shell_pty::resize(cols, rows);
        }

        #[cfg(feature = "gui")]
        "pty_close" => {
            crate::shell_pty::close();
        }

        // ── AskUserQuestion modal response (M6.36 SERVE9f) ─────────
        "ask_user_response" => {
            let id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let text = msg
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Echo the reply into the Terminal tab so the cyan
            // "assistant asks" banner is paired with a visible answer.
            // Format mirrors how `UserPrompt` renders elsewhere:
            // dim `> ` marker on the first line, two-space indent on
            // continuations. The Chat tab already pushes its own
            // local user bubble (ChatView.handleSubmit), so this
            // dispatch only affects the terminal subscriber.
            if !text.trim().is_empty() {
                let mut lines = text.split('\n');
                let mut body = String::from("\r\n\x1b[2m> \x1b[0m");
                if let Some(first) = lines.next() {
                    body.push_str(first);
                }
                for line in lines {
                    body.push_str("\r\n  ");
                    body.push_str(line);
                }
                body.push_str("\r\n");
                (ctx.dispatch)(crate::event_render::terminal_data_envelope(&body));
            }
            let responder = ctx
                .pending_asks
                .lock()
                .ok()
                .and_then(|mut pending| pending.remove(&id));
            if let Some(responder) = responder {
                let _ = responder.send(text);
            }
        }

        // Manual settings reload — driven by a "Reload settings"
        // button (Settings menu). Re-runs the same code path as the
        // sidebar model picker's auto-reload: dispatches ReloadConfig
        // → worker re-reads .thclaws/settings.json + AppConfig defaults
        // → rebuilds the agent in place + broadcasts SettingsChanged so
        // App.tsx refetches dependent flags (shellTabEnabled, …).
        "settings_reload" => {
            let _ = ctx
                .shared
                .input_tx
                .send(crate::shared_session::ShellInput::ReloadConfig);
        }

        // ── Team feature toggle (M6.36 SERVE9f) ────────────────────
        "team_enabled_get" => {
            let enabled = crate::config::ProjectConfig::load()
                .and_then(|c| c.team_enabled)
                .unwrap_or(false);
            let payload = serde_json::json!({
                "type": "team_enabled",
                "enabled": enabled,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // Browser tab status (docs/browser Phase 1). Reports the
        // engine-managed Playwright MCP config so the tab can show
        // enabled/headed state + a setup hint when npx is missing.
        // Live activity is derived client-side from the existing
        // chat_tool_call/chat_tool_result stream (names `browser.*`).
        "browser_status_get" => {
            let cfg = crate::config::AppConfig::load().ok();
            let enabled = cfg.as_ref().map(|c| c.browser_enabled).unwrap_or(false);
            let server = crate::config::AppConfig::browser_mcp_config(
                cfg.as_ref().and_then(|c| c.browser_headless),
            );
            let headless = server.args.iter().any(|a| a == "--headless");
            // `npx` on desktop, the image-preinstalled `playwright-mcp`
            // when THCLAWS_BROWSER_MCP_CMD is set (cloud runners). Same
            // resolution the injection guard uses.
            let command_found = crate::config::command_on_path(&server.command);
            let payload = serde_json::json!({
                "type": "browser_status",
                "enabled": enabled,
                "headless": headless,
                "command": format!("{} {}", server.command, server.args.join(" ")),
                "command_found": command_found,
                // slice 3: engine owns the chromium → live screencast
                // + native CDP input are available.
                "cdp": crate::browser_cdp::cdp_active(),
            });
            (ctx.dispatch)(payload.to_string());
        }

        // docs/browser slice 3 — live view. Start pushes `browser_frame`
        // (JPEG base64) + `browser_console` + `browser_nav` envelopes
        // through this client's dispatch until stop. Each start
        // re-attaches to the currently active page, so toggling
        // takeover recovers from closed tabs.
        "browser_screencast_start" => {
            let dispatch = ctx.dispatch.clone();
            std::thread::spawn(move || {
                let result = crate::browser_cdp::screencast_start(dispatch.clone());
                let reply = match result {
                    Ok(()) => serde_json::json!({
                        "type": "browser_screencast", "ok": true, "active": true,
                    }),
                    Err(e) => serde_json::json!({
                        "type": "browser_screencast", "ok": false, "active": false, "error": e,
                    }),
                };
                dispatch(reply.to_string());
            });
        }

        "browser_screencast_stop" => {
            let dispatch = ctx.dispatch.clone();
            std::thread::spawn(move || {
                crate::browser_cdp::screencast_stop();
                dispatch(
                    serde_json::json!({
                        "type": "browser_screencast", "ok": true, "active": false,
                    })
                    .to_string(),
                );
            });
        }

        // Native input on the live page (mouse/keyboard via the CDP
        // Input domain — insertText types whole strings in one shot).
        // Same trust posture as browser_input_call: UI-initiated,
        // input + navigation only, no script-execution surface.
        "browser_cdp_input" => {
            let kind = msg
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = msg.get("args").cloned().unwrap_or(serde_json::json!({}));
            let dispatch = ctx.dispatch.clone();
            // One ordered queue, not a thread per event: a drag is a press,
            // a stream of moves and a release, and they must land in that
            // order. A move's success is not worth a frame back to the
            // page — dozens arrive a second — its failure is.
            let quiet_ok = kind == "move";
            crate::browser_cdp::input_queued(kind.clone(), args, move |result| {
                let reply = match result {
                    Ok(()) if quiet_ok => return,
                    Ok(()) => serde_json::json!({
                        "type": "browser_input_result", "ok": true,
                        "tool": format!("cdp_{kind}"),
                    }),
                    Err(e) => serde_json::json!({
                        "type": "browser_input_result", "ok": false,
                        "tool": format!("cdp_{kind}"), "error": e,
                    }),
                };
                dispatch(reply.to_string());
            });
        }

        // Browser-tab screenshot capture (docs/browser Phase 1). UI-
        // initiated + read-only, so it runs DIRECTLY on the managed
        // `browser` MCP client — not through the agent loop (no tokens)
        // and not through the worker input queue (which only drains
        // between turns; this works mid-run). Uses call_tool_raw
        // because the regular text path drops image content blocks.
        "browser_screenshot_get" => {
            let slot = ctx.shared.browser_mcp.clone();
            let dispatch = ctx.dispatch.clone();
            std::thread::spawn(move || {
                let outcome: std::result::Result<(String, String), String> = (|| {
                    let client = slot
                        .read()
                        .unwrap()
                        .clone()
                        .ok_or("browser MCP not connected yet")?;
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| format!("tokio runtime build: {e}"))?;
                    let result = rt
                        .block_on(
                            client.call_tool_raw("browser_take_screenshot", serde_json::json!({})),
                        )
                        .map_err(|e| e.to_string())?;
                    let content = result
                        .get("content")
                        .and_then(|c| c.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let img = content
                        .iter()
                        .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("image"))
                        .ok_or("screenshot returned no image block")?;
                    let data = img
                        .get("data")
                        .and_then(|d| d.as_str())
                        .ok_or("image block missing data")?
                        .to_string();
                    let mime = img
                        .get("mimeType")
                        .and_then(|m| m.as_str())
                        .unwrap_or("image/png")
                        .to_string();
                    Ok((data, mime))
                })();
                let reply = match outcome {
                    Ok((data, mime)) => serde_json::json!({
                        "type": "browser_screenshot",
                        "ok": true,
                        "data": data,
                        "mime": mime,
                    }),
                    Err(e) => serde_json::json!({
                        "type": "browser_screenshot",
                        "ok": false,
                        "error": e,
                    }),
                };
                dispatch(reply.to_string());
            });
        }

        // Browser-tab interactive takeover (docs/browser Phase 2
        // slice 2). UI-initiated mouse/keyboard/navigation on the
        // managed browser — direct MCP calls, same trust posture as
        // the screenshot arm. STRICT allowlist: only coordinate input
        // + navigation; nothing that touches the page DOM with
        // arbitrary code (no evaluate / run_code) and nothing
        // filesystem-shaped (no file_upload). The synthetic
        // `type_text` expands to per-character press_key calls so the
        // frontend can send a whole field's text in one round trip.
        "browser_input_call" => {
            const ALLOWED: &[&str] = &[
                "browser_mouse_click_xy",
                "browser_mouse_move_xy",
                "browser_mouse_drag_xy",
                "browser_mouse_down",
                "browser_mouse_up",
                "browser_mouse_wheel",
                "browser_press_key",
                "browser_navigate",
                "browser_navigate_back",
            ];
            let tool = msg
                .get("tool")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = msg.get("args").cloned().unwrap_or(serde_json::json!({}));
            let slot = ctx.shared.browser_mcp.clone();
            let dispatch = ctx.dispatch.clone();
            std::thread::spawn(move || {
                let tool_for_reply = tool.clone();
                let outcome: std::result::Result<(), String> =
                    (|| {
                        let is_type_text = tool == "type_text";
                        if !is_type_text && !ALLOWED.contains(&tool.as_str()) {
                            return Err(format!("tool '{tool}' is not an allowed takeover input"));
                        }
                        let client = slot
                            .read()
                            .unwrap()
                            .clone()
                            .ok_or("browser MCP not connected yet")?;
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|e| format!("tokio runtime build: {e}"))?;
                        if is_type_text {
                            let text = args
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                            if text.is_empty() || text.chars().count() > 500 {
                                return Err("type_text needs 1-500 characters".into());
                            }
                            for ch in text.chars() {
                                let key = if ch == '\n' {
                                    "Enter".to_string()
                                } else {
                                    ch.to_string()
                                };
                                rt.block_on(client.call_tool(
                                    "browser_press_key",
                                    serde_json::json!({ "key": key }),
                                ))
                                .map_err(|e| e.to_string())?;
                            }
                            return Ok(());
                        }
                        rt.block_on(client.call_tool(&tool, args))
                            .map_err(|e| e.to_string())?;
                        Ok(())
                    })();
                let reply = match outcome {
                    Ok(()) => serde_json::json!({
                        "type": "browser_input_result",
                        "ok": true,
                        "tool": tool_for_reply,
                    }),
                    Err(e) => serde_json::json!({
                        "type": "browser_input_result",
                        "ok": false,
                        "tool": tool_for_reply,
                        "error": e,
                    }),
                };
                dispatch(reply.to_string());
            });
        }

        "team_enabled_set" => {
            let enabled = msg
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut cfg = crate::config::ProjectConfig::load().unwrap_or_default();
            cfg.team_enabled = Some(enabled);
            let (ok, error) = match cfg.save() {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e.to_string()),
            };
            let payload = serde_json::json!({
                "type": "team_enabled_result",
                "enabled": enabled,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // Mirror of team_enabled_get/set for the PTY-backed Shell tab.
        // Opt-in: surface the tab only when `shellTabEnabled: true`
        // sits in .thclaws/settings.json. The pty_open handler also
        // checks this, so a stale frontend can't sneak a spawn past
        // the gate.
        "shell_tab_enabled_get" => {
            let enabled = crate::config::ProjectConfig::load()
                .and_then(|c| c.shell_tab_enabled)
                .unwrap_or(false);
            let payload = serde_json::json!({
                "type": "shell_tab_enabled",
                "enabled": enabled,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "shell_tab_enabled_set" => {
            let enabled = msg
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut cfg = crate::config::ProjectConfig::load().unwrap_or_default();
            cfg.shell_tab_enabled = Some(enabled);
            let (ok, error) = match cfg.save() {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e.to_string()),
            };
            let payload = serde_json::json!({
                "type": "shell_tab_enabled_result",
                "enabled": enabled,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // Mirror of team_enabled_get/set for the opt-in media-generation
        // tools (`imageToolsEnabled` / `mediaToolsEnabled`). Off by
        // default; the tools also self-hide without a GEMINI/GOOGLE key.
        "media_tools_enabled_get" => {
            let enabled = crate::config::ProjectConfig::load()
                .and_then(|c| c.image_tools_enabled)
                .unwrap_or(false);
            let payload = serde_json::json!({
                "type": "media_tools_enabled",
                "enabled": enabled,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "media_tools_enabled_set" => {
            let enabled = msg
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut cfg = crate::config::ProjectConfig::load().unwrap_or_default();
            cfg.image_tools_enabled = Some(enabled);
            let (ok, error) = match cfg.save() {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e.to_string()),
            };
            let payload = serde_json::json!({
                "type": "media_tools_enabled_result",
                "enabled": enabled,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "hal_enabled_get" => {
            let enabled = crate::config::ProjectConfig::load()
                .and_then(|c| c.hal_enabled)
                .unwrap_or(false);
            let payload = serde_json::json!({
                "type": "hal_enabled",
                "enabled": enabled,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "hal_enabled_set" => {
            let enabled = msg
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut cfg = crate::config::ProjectConfig::load().unwrap_or_default();
            cfg.hal_enabled = Some(enabled);
            let (ok, error) = match cfg.save() {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e.to_string()),
            };
            let payload = serde_json::json!({
                "type": "hal_enabled_result",
                "enabled": enabled,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // Sensitive-data masking (`sensitive.enabled`, dev-plan/55). Nested
        // block on disk so the later mode routing (tokenize vs gate) has a
        // home; the GUI only flips `enabled`.
        "sensitive_enabled_get" => {
            let enabled = crate::config::ProjectConfig::load()
                .and_then(|c| c.sensitive)
                .and_then(|s| s.enabled)
                .unwrap_or(false);
            let payload = serde_json::json!({
                "type": "sensitive_enabled",
                "enabled": enabled,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "sensitive_enabled_set" => {
            let enabled = msg
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut cfg = crate::config::ProjectConfig::load().unwrap_or_default();
            let mut block = cfg.sensitive.take().unwrap_or_default();
            block.enabled = Some(enabled);
            cfg.sensitive = Some(block);
            let (ok, error) = match cfg.save() {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e.to_string()),
            };
            let payload = serde_json::json!({
                "type": "sensitive_enabled_result",
                "enabled": enabled,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // Browser tools (`browserEnabled`) — the INVERSE of the
        // media/team toggles: opt-OUT, default ON. Same get/set shape so
        // the Settings menu can flip it; the Playwright MCP is injected at
        // startup, so a change needs a restart to add/remove its tools.
        "browser_enabled_get" => {
            let enabled = crate::config::ProjectConfig::load()
                .and_then(|c| c.browser_enabled)
                .unwrap_or(true);
            let payload = serde_json::json!({
                "type": "browser_enabled",
                "enabled": enabled,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // Read-only gate for the desktop SSO sign-in button. Defaults to false
        // (hidden) until `ssoSignInEnabled` is set in .thclaws/settings.json —
        // no `_set` on purpose, so an unready feature can't be toggled on from
        // the GUI.
        "sso_sign_in_enabled_get" => {
            let enabled = crate::config::ProjectConfig::load()
                .and_then(|c| c.sso_sign_in_enabled)
                .unwrap_or(false);
            let payload = serde_json::json!({
                "type": "sso_sign_in_enabled",
                "enabled": enabled,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "browser_enabled_set" => {
            // Default to ON (true) on a malformed payload — matches the
            // opt-out default so a bad message can't silently disable it.
            let enabled = msg.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            let mut cfg = crate::config::ProjectConfig::load().unwrap_or_default();
            cfg.browser_enabled = Some(enabled);
            let (ok, error) = match cfg.save() {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e.to_string()),
            };
            let payload = serde_json::json!({
                "type": "browser_enabled_result",
                "enabled": enabled,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "openrouter_free_only_get" => {
            let enabled = crate::config::AppConfig::load()
                .map(|c| c.openrouter_free_only)
                .unwrap_or(false);
            let payload = serde_json::json!({
                "type": "openrouter_free_only",
                "enabled": enabled,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // ── Auto-learn project setting ─────────────────────────────
        // Exposes `ProjectConfig.auto_learn` as a webui toggle so the
        // setting isn't desktop-GUI-only. See #105.
        // ── Mid-turn user input injection (issue #106) ──────────────
        // Push a user-typed message into the agent's injection queue
        // while the agent is busy. The agent drains the queue at the
        // next tool_result boundary inside `run_turn`. Frontend uses
        // this to let the user "steer" the leader between tool calls
        // without `/stop`-and-restart.
        "user_input_inject" => {
            let text = msg
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let id = msg
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if text.is_empty() {
                let payload = serde_json::json!({
                    "type": "user_input_inject_result",
                    "id": id,
                    "ok": false,
                    "error": "empty text",
                    "pending": 0,
                });
                (ctx.dispatch)(payload.to_string());
                return true;
            }
            let pending = {
                let mut q = ctx
                    .shared
                    .injection_queue
                    .lock()
                    .expect("injection_queue lock");
                q.push_back(text);
                q.len()
            };
            let payload = serde_json::json!({
                "type": "user_input_inject_result",
                "id": id,
                "ok": true,
                "pending": pending,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "auto_learn_get" => {
            let enabled = crate::config::AppConfig::load()
                .map(|c| c.auto_learn)
                .unwrap_or(false);
            let payload = serde_json::json!({
                "type": "auto_learn",
                "enabled": enabled,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "auto_learn_set" => {
            let enabled = msg
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut cfg = crate::config::ProjectConfig::load().unwrap_or_default();
            cfg.auto_learn = Some(enabled);
            let (ok, error) = match cfg.save() {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e.to_string()),
            };
            let payload = serde_json::json!({
                "type": "auto_learn_result",
                "enabled": enabled,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
            // Reload AppConfig so the next session-end ingest /
            // reconcile pass sees the new value without a restart.
            let _ = ctx
                .shared
                .input_tx
                .send(crate::shared_session::ShellInput::ReloadConfig);
        }

        "openrouter_free_only_set" => {
            let enabled = msg
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut cfg = crate::config::ProjectConfig::load().unwrap_or_default();
            cfg.openrouter_free_only = Some(enabled);
            let (ok, error) = match cfg.save() {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e.to_string()),
            };
            let payload = serde_json::json!({
                "type": "openrouter_free_only_result",
                "enabled": enabled,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
            // Reload AppConfig in the live shell so /models sees the
            // new flag without requiring a restart.
            let _ = ctx
                .shared
                .input_tx
                .send(crate::shared_session::ShellInput::ReloadConfig);
        }

        // ── openrouter/fusion+ configuration ────────────────────────
        // Read/write the FusionConfig block that drives the configurable
        // OpenRouter Fusion pseudo-model. The GUI's fusion config modal
        // (opened when the user selects `openrouter/fusion+`) round-trips
        // these. camelCase on the wire (matches settings.json keys).
        "fusion_config_get" => {
            let cfg = crate::config::AppConfig::load().unwrap_or_default();
            let payload = serde_json::json!({
                "type": "fusion_config",
                "config": cfg.openrouter_fusion,
            });
            (ctx.dispatch)(payload.to_string());
        }
        "fusion_config_set" => {
            let raw = msg.get("config").cloned().unwrap_or(serde_json::json!({}));
            let (ok, error) = match serde_json::from_value::<crate::config::FusionConfig>(raw) {
                Ok(fc) => {
                    let mut cfg = crate::config::ProjectConfig::load().unwrap_or_default();
                    cfg.openrouter_fusion = Some(fc);
                    match cfg.save() {
                        Ok(()) => (true, String::new()),
                        Err(e) => (false, e.to_string()),
                    }
                }
                Err(e) => (false, format!("invalid fusion config: {e}")),
            };
            let payload = serde_json::json!({
                "type": "fusion_config_result",
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
            if ok {
                let _ = ctx
                    .shared
                    .input_tx
                    .send(crate::shared_session::ShellInput::ReloadConfig);
            }
        }

        // ── thClaws Gateway settings ────────────────────────────────
        // Per-provider routing list lives in settings.json alongside
        // openrouterFreeOnly. The gateway access key is stored in the
        // OS keychain via the existing api_key_set pipeline (provider
        // name = "gateway"). The base URL is fixed at
        // `providers::thclaws_gateway::GATEWAY_BASE_URL` and never
        // user-configurable from the UI.
        "gateway_settings_get" => {
            let cfg = crate::config::AppConfig::load().unwrap_or_default();
            let payload = serde_json::json!({
                "type": "gateway_settings",
                "base_url": crate::providers::thclaws_gateway::GATEWAY_BASE_URL,
                "proxy": cfg.gateway_proxy,
                "has_cli_token": crate::providers::thclaws_gateway::has_access_key(),
            });
            (ctx.dispatch)(payload.to_string());
        }
        "gateway_settings_set" => {
            let proxy = msg.get("proxy").and_then(|v| v.as_bool()).unwrap_or(false);
            let mut cfg = crate::config::ProjectConfig::load().unwrap_or_default();
            cfg.set_gateway_proxy(proxy);
            let (ok, error) = match cfg.save() {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e.to_string()),
            };
            let payload = serde_json::json!({
                "type": "gateway_settings_result",
                "base_url": crate::providers::thclaws_gateway::GATEWAY_BASE_URL,
                "proxy": proxy,
                "has_cli_token": crate::providers::thclaws_gateway::has_access_key(),
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
            let _ = ctx
                .shared
                .input_tx
                .send(crate::shared_session::ShellInput::ReloadConfig);
        }

        // Set (or clear) the workspace default GUI Shell from the picker's
        // "Set as default" button. Writes the `guiShell` shorthand to the
        // project .thclaws/settings.json so this shell auto-opens in the
        // GUI tab and is the --serve default.
        "gui_shell_set_default" => {
            let shell_id = msg
                .get("shellId")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let clear = msg.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);
            let mut cfg = crate::config::ProjectConfig::load().unwrap_or_default();
            cfg.set_gui_shell_default(if clear { None } else { shell_id.as_deref() });
            let (ok, error) = match cfg.save() {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e.to_string()),
            };
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "gui_shell_set_default_result",
                    "shellId": shell_id,
                    "cleared": clear,
                    "ok": ok,
                    "error": error,
                })
                .to_string(),
            );
        }

        // ── KMS sidebar mutators (M6.36 SERVE9f) ───────────────────
        // Sidebar context menu. Both go through the shell so the
        // config/agent bookkeeping and the chat feedback are the same
        // as typing the command.
        "kms_rename" => {
            let name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let new_name = msg
                .get("new_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if !name.is_empty() && !new_name.is_empty() {
                let _ = ctx
                    .shared
                    .input_tx
                    .send(crate::shared_session::ShellInput::Line(format!(
                        "/kms rename {name} {new_name}"
                    )));
            }
        }
        "kms_drop" => {
            let name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if !name.is_empty() {
                let _ = ctx
                    .shared
                    .input_tx
                    .send(crate::shared_session::ShellInput::Line(format!(
                        "/kms drop {name} --force"
                    )));
            }
        }
        "kms_toggle" => {
            let name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let active = msg.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
            // The checkbox IS `/kms use` / `/kms off`: route through the
            // worker's slash handler so the live session (system prompt,
            // KMS tools, in-memory `kms_active`) changes together with the
            // persisted settings — a direct settings.json write left the
            // running chat and the next `/research` unaware of the tick.
            // The handler broadcasts `kms_update` itself when done.
            let (ok, error) = if name.is_empty() {
                (false, "name required".to_string())
            } else if active && crate::kms::resolve(name).is_none() {
                (false, format!("no KMS named '{name}'"))
            } else {
                let line = if active {
                    format!("/kms use {name}")
                } else {
                    format!("/kms off {name}")
                };
                match ctx.shared.input_tx.send(ShellInput::Line(line)) {
                    Ok(_) => (true, String::new()),
                    Err(_) => (false, "session input closed".to_string()),
                }
            };
            let payload = serde_json::json!({
                "type": "kms_toggle_result",
                "name": name,
                "active": active,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "kms_new" => {
            let name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            // Scope optional. Absent/blank → project default via
            // `ensure_default` (reuse existing same-named KMS in any scope,
            // else create project-scoped) so a "New KMS" without an explicit
            // scope can't mint a user-scope duplicate of the project base.
            let scope_opt = msg
                .get("scope")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let scope_str = scope_opt.unwrap_or("project");
            let (ok, error) = if name.is_empty() {
                (false, "name required".to_string())
            } else {
                let res = match scope_opt {
                    Some("user") => crate::kms::create(name, crate::kms::KmsScope::User),
                    _ => crate::kms::ensure_default(name),
                };
                match res {
                    Ok(_) => (true, String::new()),
                    Err(e) => (false, e.to_string()),
                }
            };
            let payload = serde_json::json!({
                "type": "kms_new_result",
                "name": name,
                "scope": scope_str,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
            (ctx.dispatch)(crate::kms::build_update_payload().to_string());
        }

        // Files-tab "Add to KMS" context action: ingest the selected file
        // into a named KMS (markdown only — mirrors the frontend gate and
        // `INGEST_EXTENSIONS`). Path is sandbox-checked. Echoes an
        // `kms_ingest_result` with the minted alias + local-image count so
        // the tab can toast the outcome; refreshes the KMS sidebar on success.
        "kms_ingest" => {
            let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let raw_path = msg.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let kms_name = msg
                .get("kms")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let force = msg.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            // `summary` (default): the main agent upgrades the stub page.
            // `atomic`: a research job digests the document and writes a
            // topic page + one note per idea (zettelkasten).
            let atomic = msg.get("mode").and_then(|v| v.as_str()) == Some("atomic");

            let (ok, alias, images_copied, overwrote, error): (bool, String, usize, bool, String) =
                (|| {
                    if kms_name.is_empty() {
                        return (
                            false,
                            String::new(),
                            0,
                            false,
                            "no KMS selected".to_string(),
                        );
                    }
                    let path = match crate::sandbox::Sandbox::check(raw_path) {
                        Ok(p) => p,
                        Err(e) => {
                            return (
                                false,
                                String::new(),
                                0,
                                false,
                                format!("access denied: {e}"),
                            )
                        }
                    };
                    let ext_ok = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "md" | "markdown"))
                        .unwrap_or(false);
                    if !ext_ok {
                        return (
                            false,
                            String::new(),
                            0,
                            false,
                            "only .md files can be added to a KMS".to_string(),
                        );
                    }
                    let Some(k) = crate::kms::resolve(&kms_name) else {
                        return (
                            false,
                            String::new(),
                            0,
                            false,
                            format!("KMS '{kms_name}' not found"),
                        );
                    };
                    match crate::kms::ingest(&k, &path, None, force) {
                        Ok(r) => (true, r.alias, r.images_copied, r.overwrote, String::new()),
                        Err(e) => (false, String::new(), 0, false, e.to_string()),
                    }
                })();

            // An alias collision is recoverable: the frontend surfaces a
            // "Replace" action that re-sends with `force: true`. Flag it so
            // the tab can tell a collision apart from a hard failure. Only
            // meaningful when the caller didn't already force.
            let collision = !ok && !force && error.contains("already exists");

            // On success, hand the tab an agent prompt that upgrades the bare
            // stub into a real curated page (summary + takeaways + wikilinks).
            // The tab relays it as a normal chat turn so the main agent authors
            // it with KmsRead/KmsSearch/KmsWrite. Empty when the ingest failed.
            let mut research_id = String::new();
            let mut research_err = String::new();
            if ok && atomic {
                let cfg = crate::config::AppConfig::load().unwrap_or_default();
                match crate::repl::build_provider(&cfg) {
                    Ok(provider) => {
                        let model = cfg.model.clone();
                        let kms = kms_name.clone();
                        let alias_c = alias.clone();
                        let dispatch = ctx.dispatch.clone();
                        tokio::spawn(async move {
                            match crate::research::start_ingest(
                                kms.clone(),
                                alias_c.clone(),
                                crate::research::JobConfig::default(),
                                provider,
                                model,
                                None,
                            )
                            .await
                            {
                                Ok(id) => {
                                    dispatch(
                                        serde_json::json!({
                                            "type": "research_focus", "id": id,
                                        })
                                        .to_string(),
                                    );
                                }
                                Err(e) => {
                                    eprintln!("[kms_ingest] atomic notes failed to start: {e}")
                                }
                            }
                        });
                        research_id = "pending".to_string();
                    }
                    Err(e) => research_err = format!("provider unavailable: {e}"),
                }
            }
            let summarize_prompt = if ok && !atomic {
                crate::kms::resolve(&kms_name)
                    .map(|k| {
                        let src = k.root.join("sources").join(format!("{alias}.md"));
                        crate::repl::build_kms_summarize_prompt(
                            &kms_name,
                            &alias,
                            &src.to_string_lossy(),
                        )
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };

            (ctx.dispatch)(
                serde_json::json!({
                    "type": "kms_ingest_result",
                    "id": id,
                    "path": raw_path,
                    "kms": kms_name,
                    "ok": ok,
                    "alias": alias,
                    "images_copied": images_copied,
                    "overwrote": overwrote,
                    "collision": collision,
                    "summarize_prompt": summarize_prompt,
                    "atomic": atomic,
                    "research": research_id,
                    "research_error": research_err,
                    "error": error,
                })
                .to_string(),
            );
            if ok {
                (ctx.dispatch)(crate::kms::build_update_payload().to_string());
            }
        }

        // Create a new blank KMS page from the per-KMS browser's `+`.
        // The browser is scoped to one KMS, so `kms` names the target.
        // `title` is required; `topic`/`category`/`tags` are optional
        // frontmatter. The page filename is the slugified title. An
        // empty body lets `write_page` stamp the canonical
        // `# {title}` / Description header. After writing we re-emit a
        // fresh `kms_browse_result` so the open browser refreshes.
        "kms_new_page" => {
            let kms = msg.get("kms").and_then(|v| v.as_str()).unwrap_or("");
            let title = msg
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let topic = msg
                .get("topic")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let category = msg
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let tags = msg
                .get("tags")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();

            let (ok, error, page_name) = if title.is_empty() {
                (false, "title required".to_string(), String::new())
            } else {
                match crate::kms::resolve(kms) {
                    None => (false, format!("KMS '{kms}' not found"), String::new()),
                    Some(kref) => {
                        let slug = crate::kms::sanitize_alias(title);
                        if slug.is_empty() {
                            (
                                false,
                                "title has no usable characters for a filename".to_string(),
                                String::new(),
                            )
                        } else {
                            // Build frontmatter; empty body → write_page
                            // injects the canonical title/Description header.
                            let mut fm = String::from("---\n");
                            fm.push_str(&format!("title: {title}\n"));
                            if !topic.is_empty() {
                                fm.push_str(&format!("topic: {topic}\n"));
                            }
                            if !category.is_empty() {
                                fm.push_str(&format!("category: {category}\n"));
                            }
                            if !tags.is_empty() {
                                fm.push_str(&format!("tags: {tags}\n"));
                            }
                            fm.push_str("---\n\n");
                            match crate::kms::write_page(&kref, &slug, &fm) {
                                Ok(_) => (true, String::new(), slug),
                                Err(e) => (false, e.to_string(), String::new()),
                            }
                        }
                    }
                }
            };
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "kms_new_page_result",
                    "kms": kms,
                    "name": page_name,
                    "ok": ok,
                    "error": error,
                })
                .to_string(),
            );
            // Refresh the browser listing if the write succeeded.
            if ok {
                if let Some(listing) = crate::kms::browse(kms) {
                    (ctx.dispatch)(
                        serde_json::json!({
                            "type": "kms_browse_result",
                            "kms": listing.kms,
                            "pages": listing.pages,
                            "sources": listing.sources,
                            "entry": listing.entry,
                            "ok": true,
                        })
                        .to_string(),
                    );
                }
            }
        }

        // Rename a KMS page from the browser's row context menu. Moves
        // the file + rewrites inbound links + the index. `name` is the
        // current page stem; `new_name` is slugified server-side.
        "kms_rename_page" => {
            let kms = msg.get("kms").and_then(|v| v.as_str()).unwrap_or("");
            let name = msg.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let new_name = msg
                .get("new_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let (ok, error) = if name.is_empty() || new_name.is_empty() {
                (false, "name and new_name required".to_string())
            } else {
                match crate::kms::resolve(kms) {
                    None => (false, format!("KMS '{kms}' not found")),
                    Some(kref) => match crate::kms::rename_page(&kref, name, new_name) {
                        Ok(_) => (true, String::new()),
                        Err(e) => (false, e.to_string()),
                    },
                }
            };
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "kms_rename_page_result",
                    "kms": kms,
                    "name": name,
                    "ok": ok,
                    "error": error,
                })
                .to_string(),
            );
            if ok {
                if let Some(listing) = crate::kms::browse(kms) {
                    (ctx.dispatch)(
                        serde_json::json!({
                            "type": "kms_browse_result",
                            "kms": listing.kms,
                            "pages": listing.pages,
                            "sources": listing.sources,
                            "entry": listing.entry,
                            "ok": true,
                        })
                        .to_string(),
                    );
                }
            }
        }

        // Overwrite a KMS page's full content (frontmatter + body) from
        // the viewer's edit mode. `content` is the recombined markdown
        // the frontend assembled (edited YAML frontmatter + TipTap body).
        // write_page re-stamps `updated:`, preserves `created:`, and is
        // idempotent on the canonical header. Edit never renames — the
        // filename stays `name` even if the frontmatter title changed.
        // Viewer selection → "Create page": link the phrase in the source
        // page, create a stub at the slug so the link resolves at once,
        // and research the phrase into that page (topic slug fixed).
        "kms_create_page_from_selection" => {
            let kms = msg
                .get("kms")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let page = msg
                .get("page")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let text: String = msg
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let slug = crate::research::digest::sanitize_slug(&text);
            // `summary` = one page only (no child notes, fewer rounds);
            // `atomic` = the full topic-first run with one note per idea.
            let atomic = msg.get("mode").and_then(|v| v.as_str()) == Some("atomic");
            let mut linked = false;
            let mut existed = false;
            let mut started = false;
            let result: Result<(), String> = (|| {
                if text.chars().count() < 2 || text.chars().count() > 120 || slug.is_empty() {
                    return Err("select a short phrase (2–120 characters)".into());
                }
                let kref =
                    crate::kms::resolve(&kms).ok_or_else(|| format!("KMS '{kms}' not found"))?;
                existed = kref.pages_dir().join(format!("{slug}.md")).is_file();
                linked = crate::kms::link_phrase(&kref, &page, &text, &slug)
                    .map_err(|e| e.to_string())?;
                if existed {
                    return Ok(());
                }
                let page_title =
                    std::fs::read_to_string(kref.page_path(&page).map_err(|e| e.to_string())?)
                        .ok()
                        .map(|raw| crate::research::pipeline_v2::local_source_title(&raw, &page))
                        .unwrap_or_else(|| page.clone());
                let stub = format!(
                    "---\ntitle: \"{}\"\ntype: note\nkind: concept\nstatus: researching\nrelated: [\"{page}\"]\n---\n\n\
                     Researching \"{text}\" — this page is being written by `/research`, linked from [[{page}|{page_title}]].\n",
                    text.replace('"', "'")
                );
                crate::kms::write_page(&kref, &slug, &stub).map_err(|e| e.to_string())?;
                let cfg = crate::config::AppConfig::load().unwrap_or_default();
                let provider = crate::repl::build_provider(&cfg)
                    .map_err(|e| format!("provider unavailable: {e}"))?;
                let model = cfg.model.clone();
                let mut rcfg = crate::research::JobConfig::default();
                rcfg.kms_target = Some(kms.clone());
                rcfg.topic_slug = Some(slug.clone());
                if !atomic {
                    rcfg.max_notes = 1;
                    rcfg.max_iter = rcfg.max_iter.min(2);
                }
                let query = if page_title.to_lowercase().contains(&text.to_lowercase()) {
                    text.clone()
                } else {
                    format!("{text} ({page_title})")
                };
                let dispatch = ctx.dispatch.clone();
                tokio::spawn(async move {
                    match crate::research::start(query, rcfg, provider, model, None).await {
                        Ok(id) => dispatch(
                            serde_json::json!({"type": "research_focus", "id": id}).to_string(),
                        ),
                        Err(e) => eprintln!("[kms_create_page] research failed to start: {e}"),
                    }
                });
                started = true;
                Ok(())
            })();
            let (ok, error) = match result {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e),
            };
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "kms_create_page_result",
                    "kms": kms,
                    "page": page,
                    "slug": slug,
                    "text": text,
                    "linked": linked,
                    "existed": existed,
                    "research_started": started,
                    "atomic": atomic,
                    "ok": ok,
                    "error": error,
                })
                .to_string(),
            );
            if ok {
                (ctx.dispatch)(crate::kms::build_update_payload().to_string());
                if let Some(listing) = crate::kms::browse(&kms) {
                    (ctx.dispatch)(
                        serde_json::json!({
                            "type": "kms_browse_result",
                            "kms": listing.kms,
                            "pages": listing.pages,
                            "sources": listing.sources,
                            "entry": listing.entry,
                            "ok": true,
                        })
                        .to_string(),
                    );
                }
            }
        }

        "kms_write_page" => {
            let kms = msg.get("kms").and_then(|v| v.as_str()).unwrap_or("");
            let name = msg.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, error) = if name.is_empty() {
                (false, "name required".to_string())
            } else {
                match crate::kms::resolve(kms) {
                    None => (false, format!("KMS '{kms}' not found")),
                    Some(kref) => match crate::kms::write_page(&kref, name, content) {
                        Ok(_) => (true, String::new()),
                        Err(e) => (false, e.to_string()),
                    },
                }
            };
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "kms_write_page_result",
                    "kms": kms,
                    "name": name,
                    "ok": ok,
                    "error": error,
                })
                .to_string(),
            );
            if ok {
                if let Some(listing) = crate::kms::browse(kms) {
                    (ctx.dispatch)(
                        serde_json::json!({
                            "type": "kms_browse_result",
                            "kms": listing.kms,
                            "pages": listing.pages,
                            "sources": listing.sources,
                            "entry": listing.entry,
                            "ok": true,
                        })
                        .to_string(),
                    );
                }
            }
        }

        // Delete a KMS page from the browser's row context menu.
        "kms_delete_page" => {
            let kms = msg.get("kms").and_then(|v| v.as_str()).unwrap_or("");
            let name = msg.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, error) = if name.is_empty() {
                (false, "name required".to_string())
            } else {
                match crate::kms::resolve(kms) {
                    None => (false, format!("KMS '{kms}' not found")),
                    Some(kref) => match crate::kms::delete_page(&kref, name) {
                        Ok(_) => (true, String::new()),
                        Err(e) => (false, e.to_string()),
                    },
                }
            };
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "kms_delete_page_result",
                    "kms": kms,
                    "name": name,
                    "ok": ok,
                    "error": error,
                })
                .to_string(),
            );
            if ok {
                if let Some(listing) = crate::kms::browse(kms) {
                    (ctx.dispatch)(
                        serde_json::json!({
                            "type": "kms_browse_result",
                            "kms": listing.kms,
                            "pages": listing.pages,
                            "sources": listing.sources,
                            "entry": listing.entry,
                            "ok": true,
                        })
                        .to_string(),
                    );
                }
            }
        }

        // ── api_key_set (M6.36 SERVE9f — full rich path) ──────────
        "api_key_set" => {
            let provider = msg.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            // Strip whitespace AND surrounding "…" / '…' quotes. Users
            // frequently paste from a quoted source (`.env` line, shell
            // export, screenshot caption) and don't notice the wrapping
            // chars. Issue #145: a key stored as `"sk-or-v1-…"` produced
            // `Authorization: Bearer "sk-or-v1-…"`, which OpenRouter
            // rejects with the exact message `Missing Authentication
            // header` (the bearer regex doesn't accept a quoted token).
            // Normalize once at write time so the on-disk / keychain
            // value is always the bare key.
            let raw = msg.get("key").and_then(|v| v.as_str()).unwrap_or("").trim();
            let key = strip_wrapping_quotes(raw);
            let (ok, error, storage) = store_provider_key(provider, key);
            announce_key_stored(provider, ok, &error, storage, ctx);
        }

        // ── Team tab data (M6.36 SERVE9g) ──────────────────────────
        "team_send_message" => {
            if let (Some(to), Some(text)) = (
                msg.get("to").and_then(|v| v.as_str()),
                msg.get("text").and_then(|v| v.as_str()),
            ) {
                if !crate::team::is_valid_agent_name(to) {
                    eprintln!(
                        "[team] team_send_message: rejecting invalid recipient '{}'",
                        to
                    );
                } else {
                    let team_dir = std::env::current_dir()
                        .unwrap_or_default()
                        .join(crate::team::Mailbox::default_dir());
                    let mailbox = crate::team::Mailbox::new(team_dir);
                    let tm = crate::team::TeamMessage::new("user", text);
                    let _ = mailbox.write_to_mailbox(to, tm);
                }
            }
        }

        "team_list" => {
            // Find the team dir — could be in cwd or a subdirectory.
            let team_dir = {
                let cwd = std::env::current_dir().unwrap_or_default();
                let default = crate::team::Mailbox::default_dir();
                let candidate = cwd.join(&default);
                if candidate.join("config.json").exists() {
                    candidate
                } else {
                    let mut found = candidate.clone();
                    if let Ok(entries) = std::fs::read_dir(&cwd) {
                        for entry in entries.flatten() {
                            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                let sub = entry.path().join(&default);
                                if sub.join("config.json").exists() {
                                    found = sub;
                                    break;
                                }
                            }
                        }
                    }
                    found
                }
            };
            let mailbox = crate::team::Mailbox::new(team_dir.clone());
            let agents: Vec<serde_json::Value> = mailbox
                .all_status()
                .unwrap_or_default()
                .into_iter()
                .map(|a| {
                    let log_path = mailbox.output_log_path(&a.agent);
                    let output: Vec<String> = std::fs::read_to_string(&log_path)
                        .unwrap_or_default()
                        .lines()
                        .rev()
                        .take(100)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .map(String::from)
                        .collect();
                    serde_json::json!({
                        "name": a.agent,
                        "status": a.status,
                        // `alive=false` when the heartbeat is stale (crashed /
                        // never booted) so the Team tab can flag it; the raw
                        // status word alone freezes on its last value.
                        "alive": a.agent == "lead" || a.status == "stopped" || !a.is_stale(),
                        "last_heartbeat": a.last_heartbeat,
                        "task": a.current_task,
                        "output": output,
                    })
                })
                .collect();
            let has_team = team_dir.join("config.json").exists();
            let payload = serde_json::json!({
                "type": "team_status",
                "has_team": has_team,
                "agents": agents,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // ── Slash command picker (M6.36 SERVE9g) ───────────────────
        "slash_commands_list" => {
            let mut entries: Vec<serde_json::Value> = Vec::new();
            for c in crate::repl::built_in_commands() {
                entries.push(serde_json::json!({
                    "name": c.name,
                    "description": c.description,
                    "category": c.category,
                    "usage": c.usage,
                    "source": "builtin",
                }));
            }
            let user_cmds = crate::commands::CommandStore::discover_with_extra(
                &crate::plugins::plugin_command_dirs(),
            );
            // Names already shown as built-ins above (e.g. the seeded `/quiz`)
            // must not be listed a second time as a "Custom" command.
            let builtin_names: std::collections::HashSet<&str> = crate::repl::built_in_commands()
                .iter()
                .map(|c| c.name)
                .collect();
            let mut user_names: Vec<&str> = user_cmds.commands.keys().map(String::as_str).collect();
            user_names.sort();
            for name in user_names {
                if builtin_names.contains(name) {
                    continue;
                }
                if let Some(cmd) = user_cmds.get(name) {
                    entries.push(serde_json::json!({
                        "name": cmd.name,
                        "description": cmd.description,
                        "category": "Custom",
                        "usage": "",
                        "source": "user",
                    }));
                }
            }
            let skill_store = crate::skills::SkillStore::discover();
            let mut skill_entries: Vec<&crate::skills::SkillDef> =
                skill_store.skills.values().collect();
            skill_entries.sort_by(|a, b| a.name.cmp(&b.name));
            for s in skill_entries {
                entries.push(serde_json::json!({
                    "name": s.name,
                    "description": s.description,
                    "category": "Skills",
                    "usage": "",
                    "source": "skill",
                }));
            }
            let payload = serde_json::json!({
                "type": "slash_commands",
                "commands": entries,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // ── Cross-provider model picker (M6.36 SERVE9g) ────────────
        "request_all_models" => {
            let dispatch = ctx.dispatch.clone();
            tokio::spawn(async move {
                let payload = crate::providers::build_all_models_payload().await;
                dispatch(payload);
            });
        }

        // ── MCP-Apps widget tool call (M6.36 SERVE9g) ──────────────
        "mcp_call_tool" => {
            let request_id = msg
                .get("requestId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let qualified_name = msg
                .get("qualifiedName")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = msg
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            if !request_id.is_empty() && !qualified_name.is_empty() {
                let _ = ctx.shared.input_tx.send(ShellInput::McpAppCallTool {
                    request_id,
                    qualified_name,
                    arguments,
                });
            }
        }

        // ── External URL opener (M6.36 SERVE9h) ────────────────────
        "open_external" => {
            // Tool output (MCP, web search) can produce URLs; accept
            // only http(s). Anything else dropped silently with stderr.
            // On a remote `--serve` host this still tries to open in
            // the SERVER's default browser — typically a no-op since
            // the server is headless. Browser users probably want
            // window.open() in JS instead; defer that frontend hint.
            if let Some(url) = msg.get("url").and_then(|v| v.as_str()) {
                if crate::external_url::is_safe_external_url(url) {
                    crate::external_url::open_external_url(url);
                } else {
                    eprintln!("\x1b[33m[ipc open_external] refusing non-http(s) url\x1b[0m");
                }
            }
        }

        // ── SSO sidebar (M6.36 SERVE9h) ────────────────────────────
        "sso_status" => {
            (ctx.dispatch)(crate::sso::build_state_payload().to_string());
        }

        "sso_login" => {
            let dispatch = ctx.dispatch.clone();
            // Optional `provider` field: chooses a builtin (google /
            // azure) when no EE policy is active. Ignored under EE
            // override — the org-pinned IdP wins regardless.
            let requested_provider = msg
                .get("provider")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            tokio::spawn(async move {
                let policy = match crate::policy::active()
                    .and_then(|a| a.policy.policies.sso.as_ref())
                    .cloned()
                {
                    Some(p) if p.enabled => p,
                    _ => {
                        // No EE policy → fall back to the standard
                        // builtin route (Google now; Azure once
                        // registered). The frontend should always send
                        // a `provider` field in this mode, but be
                        // defensive: default to the first configured
                        // builtin so a misbehaving client doesn't
                        // silently no-op.
                        let chosen = requested_provider
                            .as_deref()
                            .and_then(crate::sso::builtin::BuiltinProvider::from_id)
                            .or_else(|| crate::sso::builtin::available().into_iter().next());
                        let Some(provider) = chosen else {
                            let payload = serde_json::json!({
                                "type": "sso_state",
                                "enabled": true,
                                "managed": false,
                                "logged_in": false,
                                "providers": [],
                                "error": "no SSO provider configured (set GOOGLE_CLIENT_ID in .env)",
                            });
                            dispatch(payload.to_string());
                            return;
                        };
                        match provider.resolve() {
                            Ok(p) => p,
                            Err(e) => {
                                let payload = serde_json::json!({
                                    "type": "sso_state",
                                    "enabled": true,
                                    "managed": false,
                                    "logged_in": false,
                                    "error": format!("provider not configured: {e}"),
                                });
                                dispatch(payload.to_string());
                                return;
                            }
                        }
                    }
                };
                match crate::sso::login(&policy).await {
                    Ok(_) => {
                        dispatch(crate::sso::build_state_payload().to_string());
                    }
                    Err(e) => {
                        let payload = serde_json::json!({
                            "type": "sso_state",
                            "enabled": true,
                            "logged_in": false,
                            "issuer": policy.issuer_url,
                            "error": format!("login failed: {e}"),
                        });
                        dispatch(payload.to_string());
                    }
                }
            });
        }

        "sso_logout" => {
            // Clear the EE policy session (if any) and every builtin
            // session — keeps the keychain clean and the UI in a known
            // post-logout state regardless of which path produced the
            // active session. Errors are swallowed: a missing keychain
            // entry isn't a user-facing failure.
            if let Some(p) = crate::policy::active().and_then(|a| a.policy.policies.sso.as_ref()) {
                let _ = crate::sso::logout(p);
            }
            for provider in crate::sso::builtin::available() {
                if let Ok(p) = provider.resolve() {
                    let _ = crate::sso::logout(&p);
                }
            }
            (ctx.dispatch)(crate::sso::build_state_payload().to_string());
        }

        // ── File browser (M6.36 SERVE9i) ──────────────────────────
        "file_list" => {
            let raw_path = crate::file_preview::ospath(
                msg.get("path").and_then(|v| v.as_str()).unwrap_or("."),
            );
            // Opt-in: when `show_hidden: true` the listing includes
            // dot-prefixed entries (`.thclaws/`, `.claude/`, `.env`,
            // etc.). Default off — the agent workspace has dozens of
            // dot-paths the user doesn't usually want to see, but the
            // few important ones (config / per-project memory / agent
            // defs) are reachable behind this switch.
            let show_hidden = msg
                .get("show_hidden")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let resolved = crate::sandbox::Sandbox::check(&raw_path)
                .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
            if let Ok(entries) = std::fs::read_dir(&resolved) {
                let mut items: Vec<serde_json::Value> = entries
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if !show_hidden && name.starts_with('.') {
                            return None;
                        }
                        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                        Some(serde_json::json!({"name": name, "is_dir": is_dir}))
                    })
                    .collect();
                items.sort_by(|a, b| {
                    let a_dir = a["is_dir"].as_bool().unwrap_or(false);
                    let b_dir = b["is_dir"].as_bool().unwrap_or(false);
                    b_dir.cmp(&a_dir).then_with(|| {
                        a["name"]
                            .as_str()
                            .unwrap_or("")
                            .cmp(b["name"].as_str().unwrap_or(""))
                    })
                });
                let payload = serde_json::json!({
                    "type": "file_tree",
                    "path": resolved.to_string_lossy(),
                    "entries": items,
                });
                (ctx.dispatch)(payload.to_string());
            }
        }

        "file_read" => {
            let raw_path =
                crate::file_preview::ospath(msg.get("path").and_then(|v| v.as_str()).unwrap_or(""));
            let mode = msg
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("preview");
            let source_mode = mode == "source";
            let theme = msg.get("theme").and_then(|v| v.as_str()).unwrap_or("dark");
            let theme = if theme == "light" { "light" } else { "dark" };
            match crate::sandbox::Sandbox::check(&raw_path) {
                Ok(path) => {
                    let ext = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    let is_image = matches!(
                        ext.as_str(),
                        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "bmp"
                    );
                    let is_pdf = ext == "pdf";
                    let is_markdown = ext == "md" || ext == "markdown";
                    let is_docx = ext == "docx";
                    let is_xlsx = ext == "xlsx"
                        || ext == "xlsm"
                        || ext == "xlsb"
                        || ext == "xls"
                        || ext == "ods";
                    let is_pptx = ext == "pptx";
                    let is_office = is_docx || is_xlsx || is_pptx;
                    // Audio + video are streamed via the file-asset
                    // route, NOT base64-inlined here — a 50 MB MP4
                    // round-tripped through IPC + base64 would dwarf
                    // the actual playback. Frontend keys off `mime`
                    // and renders <audio>/<video> with assetUrl().
                    let is_audio = matches!(
                        ext.as_str(),
                        "mp3" | "wav" | "m4a" | "ogg" | "oga" | "opus" | "flac" | "aac" | "weba"
                    );
                    let is_video =
                        matches!(ext.as_str(), "mp4" | "m4v" | "webm" | "mov" | "mkv" | "ogv");
                    // EPUB is a zipped XHTML bundle — `read_to_string`
                    // would fail on the binary. Serve it off /file-asset
                    // (empty inline content); the frontend renders it
                    // with epub.js, which unzips client-side.
                    let is_epub = ext == "epub";
                    let mime = match ext.as_str() {
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "gif" => "image/gif",
                        "svg" => "image/svg+xml",
                        "webp" => "image/webp",
                        "ico" => "image/x-icon",
                        "bmp" => "image/bmp",
                        "pdf" => "application/pdf",
                        "mp3" => "audio/mpeg",
                        "wav" => "audio/wav",
                        "m4a" | "aac" => "audio/mp4",
                        "ogg" | "oga" => "audio/ogg",
                        "opus" => "audio/opus",
                        "flac" => "audio/flac",
                        "weba" => "audio/webm",
                        "mp4" | "m4v" => "video/mp4",
                        "webm" => "video/webm",
                        "mov" => "video/quicktime",
                        "mkv" => "video/x-matroska",
                        "ogv" => "video/ogg",
                        "epub" => "application/epub+zip",
                        "md" | "markdown" => {
                            if source_mode {
                                "text/markdown"
                            } else {
                                "text/html"
                            }
                        }
                        "html" | "htm" => "text/html",
                        "docx" | "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" | "pptx" => "text/html",
                        _ => "text/plain",
                    };
                    if is_audio || is_video || is_epub {
                        // No content payload — frontend mounts the
                        // file-asset URL into <audio>/<video> directly,
                        // or hands the EPUB URL to epub.js.
                        let payload = serde_json::json!({
                            "type": "file_content",
                            "path": raw_path,
                            "content": "",
                            "mime": mime,
                            "mode": mode,
                        });
                        (ctx.dispatch)(payload.to_string());
                    } else if is_image || is_pdf {
                        // PDFs render via an /file-asset iframe (Chrome
                        // refuses its viewer in data: iframes) — don't
                        // push megabytes of base64 through the WS for
                        // bytes the frontend never reads.
                        let b64 = if is_pdf {
                            String::new()
                        } else {
                            match std::fs::read(&path) {
                                Ok(bytes) => crate::file_preview::encode_bytes_b64(&bytes),
                                Err(_) => String::new(),
                            }
                        };
                        let payload = serde_json::json!({
                            "type": "file_content",
                            "path": raw_path,
                            "content": b64,
                            "mime": mime,
                            "mode": mode,
                        });
                        (ctx.dispatch)(payload.to_string());
                    } else if is_pptx && crate::tools::slide_render::pptx_preview_available() {
                        // Render the real slides instead of dumping the
                        // text we can scrape out of the XML. Needs the
                        // slide-render service (self-hosted URL or a
                        // gateway key) — without one we fall through to
                        // the extraction below, which is all a
                        // signed-out desktop can do: converting locally
                        // would mean LibreOffice on every user's
                        // machine, which is not a realistic ask on
                        // Windows.
                        //
                        // Cached per file-content hash, so re-opening a
                        // deck costs nothing.
                        // Rendering is a network round-trip (~13s for a
                        // 23-slide deck, uncached) — off the IPC thread
                        // so the Files tab stays responsive. The
                        // frontend shows a pending state until one of
                        // the two payloads below lands.
                        let workspace = std::env::current_dir().unwrap_or_default();
                        let deck = path.clone();
                        let raw = raw_path.to_string();
                        let mode_s = mode.to_string();
                        let theme_s = theme.to_string();

                        // Decide synchronously. The Files tab re-reads the
                        // open file on a timer, so a deck that can't be
                        // rendered must take the SAME branch every tick —
                        // otherwise the pane flips between "Rendering…"
                        // and the text fallback for as long as it's open.
                        match crate::tools::slide_render::pptx_preview_state(&workspace, &deck) {
                            crate::tools::slide_render::PptxPreview::Ready(pdf) => {
                                let rel = pdf
                                    .strip_prefix(&workspace)
                                    .unwrap_or(pdf.as_path())
                                    .to_string_lossy()
                                    .replace('\\', "/");
                                // Per-slide PNGs go along so the pane
                                // can page through the deck one slide
                                // at a time; the PDF stays the
                                // fallback when there are none.
                                let slides = rel_slide_pngs(&workspace, &pdf);
                                (ctx.dispatch)(
                                    serde_json::json!({
                                        "type": "file_content",
                                        "path": raw,
                                        "render_path": rel,
                                        "slides": slides,
                                        "content": "",
                                        "mime": "application/pdf",
                                        "mode": mode_s,
                                    })
                                    .to_string(),
                                );
                                return true;
                            }
                            crate::tools::slide_render::PptxPreview::Skip(reason) => {
                                let md = match crate::tools::pptx_read::extract_pptx(&deck) {
                                    Ok(text) => format!(
                                        "_Extracted preview · PPTX_\n\n\
                                         > Slides not rendered: {reason}\n\n{text}"
                                    ),
                                    Err(inner) => {
                                        format!("**Can't render or extract:** {reason}\n\n{inner}")
                                    }
                                };
                                let html =
                                    crate::file_preview::render_markdown_to_html(&md, &theme_s);
                                (ctx.dispatch)(
                                    serde_json::json!({
                                        "type": "file_content",
                                        "path": raw,
                                        "content": html,
                                        "mime": "text/html",
                                        "mode": mode_s,
                                    })
                                    .to_string(),
                                );
                                return true;
                            }
                            crate::tools::slide_render::PptxPreview::Renderable => {}
                        }

                        let dispatch = ctx.dispatch.clone();
                        (dispatch)(
                            serde_json::json!({
                                "type": "file_render_pending",
                                "path": raw,
                            })
                            .to_string(),
                        );
                        let dispatch2 = ctx.dispatch.clone();
                        let raw2 = raw_path.to_string();
                        tokio::spawn(async move {
                            match crate::tools::slide_render::render_pptx_preview(&workspace, &deck)
                                .await
                            {
                                Ok(pdf) => {
                                    let rel = pdf
                                        .strip_prefix(&workspace)
                                        .unwrap_or(pdf.as_path())
                                        .to_string_lossy()
                                        .replace('\\', "/");
                                    let slides = rel_slide_pngs(&workspace, &pdf);
                                    (dispatch2)(
                                        serde_json::json!({
                                            "type": "file_content",
                                            "path": raw2,
                                            // Mounted through /file-asset
                                            // like any other PDF — the
                                            // viewer is already there.
                                            "render_path": rel,
                                            "slides": slides,
                                            "content": "",
                                            "mime": "application/pdf",
                                            "mode": mode_s,
                                        })
                                        .to_string(),
                                    );
                                }
                                Err(e) => {
                                    // Never strand the user on an error
                                    // screen when a usable text preview
                                    // is one call away.
                                    eprintln!("[pptx] render failed, using extracted text: {e}");
                                    let md = match crate::tools::pptx_read::extract_pptx(&deck) {
                                        Ok(text) => format!(
                                            "_Extracted preview · PPTX_\n\n\
                                             > Slide rendering unavailable: {e}\n\n{text}"
                                        ),
                                        Err(inner) => {
                                            format!(
                                                "**Failed to render or extract:** {e}\n\n{inner}"
                                            )
                                        }
                                    };
                                    let html =
                                        crate::file_preview::render_markdown_to_html(&md, &theme_s);
                                    (dispatch2)(
                                        serde_json::json!({
                                            "type": "file_content",
                                            "path": raw2,
                                            "content": html,
                                            "mime": "text/html",
                                            "mode": mode_s,
                                        })
                                        .to_string(),
                                    );
                                }
                            }
                        });
                    } else if is_office {
                        let extracted = if is_docx {
                            crate::tools::docx_read::extract_docx(&path)
                        } else if is_xlsx {
                            crate::tools::xlsx_read::extract_xlsx(&path, None, "csv")
                                .map(|csv| crate::file_preview::csv_to_markdown_table(&csv))
                        } else {
                            crate::tools::pptx_read::extract_pptx(&path)
                        };
                        let (md, ok) = match extracted {
                            Ok(text) => (
                                format!("_Extracted preview · {}_\n\n{}", ext.to_uppercase(), text),
                                true,
                            ),
                            Err(e) => (
                                format!(
                                    "**Failed to extract preview:** {e}\n\nRaw bytes \
                                     aren't shown for binary OOXML formats."
                                ),
                                false,
                            ),
                        };
                        let html = crate::file_preview::render_markdown_to_html(&md, theme);
                        let payload = serde_json::json!({
                            "type": "file_content",
                            "path": raw_path,
                            "content": html,
                            "mime": mime,
                            "mode": mode,
                            "ok": ok,
                        });
                        (ctx.dispatch)(payload.to_string());
                    } else {
                        match std::fs::read_to_string(&path) {
                            Ok(text) => {
                                let content = if is_markdown && !source_mode {
                                    crate::file_preview::render_markdown_to_html(&text, theme)
                                } else {
                                    text
                                };
                                let payload = serde_json::json!({
                                    "type": "file_content",
                                    "path": raw_path,
                                    "content": content,
                                    "mime": mime,
                                    "mode": mode,
                                });
                                (ctx.dispatch)(payload.to_string());
                            }
                            Err(e) => {
                                let payload = serde_json::json!({
                                    "type": "file_content",
                                    "path": raw_path,
                                    "content": format!("Error reading file: {e}"),
                                    "mime": "text/plain",
                                    "mode": mode,
                                });
                                (ctx.dispatch)(payload.to_string());
                            }
                        }
                    }
                }
                Err(e) => {
                    let payload = serde_json::json!({
                        "type": "file_content",
                        "path": raw_path,
                        "content": format!("Access denied: {e}"),
                        "mime": "text/plain",
                    });
                    (ctx.dispatch)(payload.to_string());
                }
            }
        }

        "file_download" => {
            // Streams raw file bytes back as base64 so the frontend
            // can wrap them in a Blob and trigger a browser-side
            // <a download> click. Used by the Files-tab sidebar's
            // "Download" context-menu action. Separate from
            // `file_read` because that handler decides what to send
            // based on extension (text vs base64 vs office-extracted)
            // — for download we always want raw bytes, regardless
            // of how the preview chose to render them.
            let raw_path =
                crate::file_preview::ospath(msg.get("path").and_then(|v| v.as_str()).unwrap_or(""));
            let request_id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let (ok, content_b64, filename, mime, error) = match crate::sandbox::Sandbox::check(
                &raw_path,
            ) {
                Ok(path) => match std::fs::read(&path) {
                    Ok(bytes) => {
                        let b64 = crate::file_preview::encode_bytes_b64(&bytes);
                        let name = path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("download")
                            .to_string();
                        let ext = path
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("")
                            .to_lowercase();
                        // Generic MIME for download; the browser
                        // honours `download` attr regardless of
                        // mime, but a sensible value helps when
                        // the user opens the file directly from
                        // the download bar.
                        let mime = match ext.as_str() {
                                "png" => "image/png",
                                "jpg" | "jpeg" => "image/jpeg",
                                "gif" => "image/gif",
                                "svg" => "image/svg+xml",
                                "webp" => "image/webp",
                                "pdf" => "application/pdf",
                                "json" => "application/json",
                                "csv" => "text/csv",
                                "html" | "htm" => "text/html",
                                "md" | "markdown" => "text/markdown",
                                "txt" => "text/plain",
                                "zip" => "application/zip",
                                "tar" => "application/x-tar",
                                "gz" | "tgz" => "application/gzip",
                                "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                                "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                                "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                                _ => "application/octet-stream",
                            }
                            .to_string();
                        (true, b64, name, mime, String::new())
                    }
                    Err(e) => (
                        false,
                        String::new(),
                        String::new(),
                        String::new(),
                        format!("read: {e}"),
                    ),
                },
                Err(e) => (
                    false,
                    String::new(),
                    String::new(),
                    String::new(),
                    format!("access denied: {e}"),
                ),
            };
            let payload = serde_json::json!({
                "type": "file_download_result",
                "id": request_id,
                "ok": ok,
                "path": raw_path,
                "content": content_b64,
                "filename": filename,
                "mime": mime,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        "file_write" => {
            let raw_path = msg.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, error): (bool, Option<String>) = match crate::sandbox::Sandbox::check(raw_path)
            {
                Ok(path) => {
                    if let Some(parent) = path.parent() {
                        if let Err(e) = std::fs::create_dir_all(parent) {
                            (false, Some(format!("mkdir: {e}")))
                        } else {
                            match std::fs::write(&path, content.as_bytes()) {
                                Ok(()) => (true, None),
                                Err(e) => (false, Some(format!("write: {e}"))),
                            }
                        }
                    } else {
                        match std::fs::write(&path, content.as_bytes()) {
                            Ok(()) => (true, None),
                            Err(e) => (false, Some(format!("write: {e}"))),
                        }
                    }
                }
                Err(e) => (false, Some(format!("access denied: {e}"))),
            };
            let payload = serde_json::json!({
                "type": "file_written",
                "path": raw_path,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // Upload a dropped file (Files-tab drag-and-drop). Content arrives
        // base64-encoded so arbitrary binary (images, PDFs, …) round-trips
        // intact — `file_write` is text-only. Sandbox-checked; refuses to
        // clobber an existing name, like `file_create`. Echoes `id` so the
        // frontend can match the result to its per-upload listener.
        "file_upload" => {
            let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let raw_path = msg.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let data_b64 = msg.get("data").and_then(|v| v.as_str()).unwrap_or("");
            // Uniquify within the requested dir (a repeat drop of the same
            // filename shouldn't clash) and report back the ACTUAL saved path
            // so the composer injects the real one.
            let mut saved_rel = raw_path.to_string();
            // check_write (not check): uploads never target the reserved
            // .thclaws/ tree — defense-in-depth against a crafted client.
            let (ok, error): (bool, Option<String>) =
                match crate::sandbox::Sandbox::check_write(raw_path) {
                    Ok(desired) => {
                        use base64::Engine;
                        match base64::engine::general_purpose::STANDARD.decode(data_b64) {
                            Ok(bytes) if bytes.len() as u64 > crate::uploads::UPLOAD_MAX_BYTES => (
                                false,
                                Some(format!(
                                    "file exceeds the {} MB limit",
                                    crate::uploads::UPLOAD_MAX_BYTES / (1024 * 1024)
                                )),
                            ),
                            Ok(bytes) => {
                                let parent = desired
                                    .parent()
                                    .map(std::path::Path::to_path_buf)
                                    .unwrap_or_default();
                                let fname = desired
                                    .file_name()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "file".into());
                                let final_abs = crate::uploads::unique_path(&parent, &fname);
                                let final_fname = final_abs
                                    .file_name()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or(fname);
                                saved_rel = match std::path::Path::new(raw_path).parent() {
                                    Some(d) if !d.as_os_str().is_empty() => {
                                        format!("{}/{}", d.to_string_lossy(), final_fname)
                                    }
                                    _ => final_fname,
                                };
                                let made = std::fs::create_dir_all(&parent)
                                    .map_err(|e| format!("mkdir parent: {e}"));
                                match made {
                                    Err(e) => (false, Some(e)),
                                    Ok(()) => match std::fs::write(&final_abs, &bytes) {
                                        Ok(()) => (true, None),
                                        Err(e) => (false, Some(format!("write: {e}"))),
                                    },
                                }
                            }
                            Err(e) => (false, Some(format!("decode: {e}"))),
                        }
                    }
                    Err(e) => (false, Some(format!("access denied: {e}"))),
                };
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "file_upload_result",
                    "id": id,
                    "path": saved_rel,
                    "ok": ok,
                    "error": error,
                })
                .to_string(),
            );
        }

        // Delete a file or folder (Files-tab entry context menu). Sandbox-
        // checked; folders are removed recursively. Echoes `id` so the
        // frontend matches the result to its per-delete listener.
        "file_delete" => {
            let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let raw_path = msg.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, error): (bool, Option<String>) = match crate::sandbox::Sandbox::check(raw_path)
            {
                Ok(path) => {
                    if !path.exists() {
                        (false, Some("path no longer exists".into()))
                    } else {
                        let res = if path.is_dir() {
                            std::fs::remove_dir_all(&path)
                        } else {
                            std::fs::remove_file(&path)
                        };
                        match res {
                            Ok(()) => (true, None),
                            Err(e) => (false, Some(format!("delete: {e}"))),
                        }
                    }
                }
                Err(e) => (false, Some(format!("access denied: {e}"))),
            };
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "file_delete_result",
                    "id": id,
                    "path": raw_path,
                    "ok": ok,
                    "error": error,
                })
                .to_string(),
            );
        }

        // Rename / move a file or folder (Files-tab entry context menu).
        // Both endpoints are sandbox-checked; refuses to clobber an existing
        // destination. Echoes `id` + the new path for the frontend listener.
        "file_rename" => {
            let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let from_raw = msg.get("from").and_then(|v| v.as_str()).unwrap_or("");
            let to_raw = msg.get("to").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, error): (bool, Option<String>) = match (
                crate::sandbox::Sandbox::check(from_raw),
                crate::sandbox::Sandbox::check(to_raw),
            ) {
                (Ok(from), Ok(to)) => {
                    if !from.exists() {
                        (false, Some("source no longer exists".into()))
                    } else if to.exists() {
                        (
                            false,
                            Some("a file or folder with that name already exists".into()),
                        )
                    } else {
                        match std::fs::rename(&from, &to) {
                            Ok(()) => (true, None),
                            Err(e) => (false, Some(format!("rename: {e}"))),
                        }
                    }
                }
                (Err(e), _) | (_, Err(e)) => (false, Some(format!("access denied: {e}"))),
            };
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "file_rename_result",
                    "id": id,
                    "to": to_raw,
                    "ok": ok,
                    "error": error,
                })
                .to_string(),
            );
        }

        // Create a new directory (Files-tab explorer context menu).
        // Sandbox-checked; refuses to clobber an existing path.
        "file_mkdir" => {
            let raw_path = msg.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, error): (bool, Option<String>) = match crate::sandbox::Sandbox::check(raw_path)
            {
                Ok(path) => {
                    if path.exists() {
                        (
                            false,
                            Some("a file or folder with that name already exists".into()),
                        )
                    } else {
                        match std::fs::create_dir_all(&path) {
                            Ok(()) => (true, None),
                            Err(e) => (false, Some(format!("mkdir: {e}"))),
                        }
                    }
                }
                Err(e) => (false, Some(format!("access denied: {e}"))),
            };
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "file_mkdir_result",
                    "path": raw_path,
                    "ok": ok,
                    "error": error,
                })
                .to_string(),
            );
        }

        // Create a new empty file (Files-tab explorer context menu).
        // Sandbox-checked; creates parent dirs; refuses to clobber via
        // `create_new` (atomic exists-check).
        "file_create" => {
            let raw_path = msg.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, error): (bool, Option<String>) = match crate::sandbox::Sandbox::check(raw_path)
            {
                Ok(path) => {
                    let parent_made = match path.parent() {
                        Some(parent) => std::fs::create_dir_all(parent)
                            .map_err(|e| format!("mkdir parent: {e}")),
                        None => Ok(()),
                    };
                    match parent_made {
                        Err(e) => (false, Some(e)),
                        Ok(()) => match std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&path)
                        {
                            Ok(_) => (true, None),
                            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                                (false, Some("a file with that name already exists".into()))
                            }
                            Err(e) => (false, Some(format!("create: {e}"))),
                        },
                    }
                }
                Err(e) => (false, Some(format!("access denied: {e}"))),
            };
            (ctx.dispatch)(
                serde_json::json!({
                    "type": "file_create_result",
                    "path": raw_path,
                    "ok": ok,
                    "error": error,
                })
                .to_string(),
            );
        }

        // ── Session sidebar mutators (M6.36 SERVE9j) ──────────────
        "session_load" => {
            if let Some(id) = msg.get("id").and_then(Value::as_str) {
                if let Some(request_id) = msg.get("view_request").and_then(Value::as_str) {
                    let result = ipc_session_store(ctx)
                        .ok_or_else(|| "No session store".to_string())
                        .and_then(|store| {
                            store
                                .load(id)
                                .or_else(|error| draft_session(id, ctx).ok_or(error))
                                .map_err(|e| e.to_string())
                        });
                    match result {
                        Ok(session) => {
                            let _ = ctx.shared.events_tx.send(crate::shared_session::ViewEvent::SessionViewRequest {session:Box::new(session), request_id:request_id.to_string()});
                        }
                        Err(error) => (ctx.dispatch)(serde_json::json!({"type":"session_view_error", "request_id":request_id, "text":error}).to_string()),
                    }
                } else if crate::agent_activity::busy_meta()
                    .as_ref()
                    .is_none_or(|m| m.session_id != id)
                {
                    let _ = ctx
                        .shared
                        .input_tx
                        .send(ShellInput::LoadSession(id.to_string()));
                }
            }
        }

        "session_rename" => {
            let id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let title = msg.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, error) = if id.is_empty() {
                (false, "id required".to_string())
            } else {
                match ipc_session_store(ctx) {
                    Some(store) => match store.rename(id, title) {
                        Ok(_) => (true, String::new()),
                        Err(e) => (false, e.to_string()),
                    },
                    None => (false, "no session store".to_string()),
                }
            };
            let payload = serde_json::json!({
                "type": "session_rename_result",
                "id": id,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
            if ok {
                // M6.19 BUG M2: notify the worker so its in-memory
                // state.session.title stays in sync when the renamed
                // session is the active one.
                let _ = ctx.shared.input_tx.send(
                    crate::shared_session::ShellInput::SessionRenamedExternal {
                        id: id.to_string(),
                        title: title.to_string(),
                    },
                );
                let store = ipc_session_store(ctx);
                (ctx.dispatch)(crate::shared_session::build_session_list(&store, ""));
            }
        }

        "sessions_request" => {
            // Sidebar mount-time refresh: the component unmounts in
            // fullscreen (gui-shell tabs) and remounts after the
            // `initial_state` snapshot already passed — answer with a
            // fresh list so the history isn't blank until the next
            // worker-side push.
            let store = ipc_session_store(ctx);
            (ctx.dispatch)(crate::shared_session::build_session_list(&store, ""));
        }

        "session_delete" => {
            let id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, error) = if id.is_empty() {
                (false, "id required".to_string())
            } else {
                match ipc_session_store(ctx) {
                    Some(store) => match store.delete(id) {
                        Ok(()) => (true, String::new()),
                        Err(e) => (false, e.to_string()),
                    },
                    None => (false, "no session store".to_string()),
                }
            };
            let payload = serde_json::json!({
                "type": "session_delete_result",
                "id": id,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
            if ok {
                // M6.19 BUG M2: notify the worker so it can mint a
                // fresh session if the deleted id was the active one.
                let _ = ctx.shared.input_tx.send(
                    crate::shared_session::ShellInput::SessionDeletedExternal {
                        id: id.to_string(),
                    },
                );
                let store = ipc_session_store(ctx);
                (ctx.dispatch)(crate::shared_session::build_session_list(&store, ""));
            }
        }

        // SERVE9 staged migration: the rest of the dispatch table
        // continues to live in `gui.rs::with_ipc_handler` for now.
        // Each subsequent migration is incremental — `cargo test` is
        // the regression backstop.
        _ => {
            // Suppress unused-field warnings while the migration is
            // in-flight (some IpcContext fields aren't consumed by any
            // currently-migrated arm).
            let _ = (&ctx.pending_asks, &ctx.dispatch, &ctx.on_zoom, &msg);
            return false;
        }
    }
    // Migrated arm fired — tell the caller not to fall through.
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn session_attachments_reject_excess_or_malformed_images_without_truncation() {
        let image = serde_json::json!({"mediaType":"image/png", "data":"YWJj"});
        let ten = serde_json::json!({"attachments":vec![image.clone();10]});
        let images = parse_session_attachments(&ten).unwrap();
        assert_eq!(images.len(), 10);
        assert_eq!(images[0], ("image/png".into(), "YWJj".into()));
        assert!(
            parse_session_attachments(&serde_json::json!({"attachments":vec![image;11]}))
                .unwrap_err()
                .contains("10")
        );
        assert!(parse_session_attachments(
            &serde_json::json!({"attachments":[{"mediaType":"image/png"}]})
        )
        .is_err());
    }

    #[test]
    fn session_view_requests_do_not_enqueue_execution_or_cancel() {
        use crate::shared_session::{ReadyGate, ViewEvent};
        let temp = tempfile::tempdir().unwrap();
        let store = crate::session::SessionStore::new(temp.path().join("sessions"));
        let mut saved =
            crate::session::Session::new_detached("mock", temp.path().to_string_lossy());
        store.save(&mut saved).unwrap();
        let (input_tx, input_rx) = std::sync::mpsc::channel();
        let (events_tx, mut events_rx) = tokio::sync::broadcast::channel(32);
        let execution = Arc::new(Mutex::new("running-A".to_string()));
        let shared = Arc::new(SharedSessionHandle {
            view_events: tokio::sync::broadcast::channel(32).0,
            execution_session_id: execution.clone(),
            input_tx,
            events_tx,
            cancel: crate::cancel::CancelToken::new(),
            ready_gate: Arc::new(ReadyGate::new()),
            injection_queue: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
            session_roots: Some(crate::multi_tenant::SessionRoots {
                sessions_dir: temp.path().join("sessions"),
                storage_dir: temp.path().join("storage"),
                usage_dir: temp.path().join("usage"),
                workspace_root: Some(temp.path().to_path_buf()),
                member_id: None,
                member_name: None,
            }),
            browser_mcp: Arc::new(std::sync::RwLock::new(None)),
        });
        let (approver, _) = GuiApprover::new();
        let replies = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = replies.clone();
        let ctx = IpcContext {
            is_serve_mode: true,
            shared: shared.clone(),
            approver,
            pending_asks: Arc::new(Mutex::new(HashMap::new())),
            dispatch: Arc::new(move |frame| {
                captured
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&frame).unwrap())
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };
        assert!(handle_ipc(
            serde_json::json!({"type":"session_load","id":saved.id,"view_request":"view-B"}),
            &ctx
        ));
        match events_rx.try_recv().unwrap() {
            ViewEvent::SessionViewRequest {
                session,
                request_id,
            } => {
                assert_eq!(session.id, saved.id);
                assert_eq!(request_id, "view-B");
            }
            event => panic!("unexpected event: {event:?}"),
        }
        assert!(handle_ipc(
            serde_json::json!({"type":"new_session","view_request":"new-B"}),
            &ctx
        ));
        match events_rx.try_recv().unwrap() {
            ViewEvent::SessionViewRequest {
                session,
                request_id,
            } => {
                assert_eq!(request_id, "new-B");
                assert_ne!(session.id, "running-A");
                assert!(
                    store.load(&session.id).is_err(),
                    "preview must not persist an empty session"
                );
            }
            event => panic!("unexpected event: {event:?}"),
        }
        assert!(
            input_rx.try_recv().is_err(),
            "viewing must not queue a worker session switch"
        );
        let draft_id = format!("draft-{}", uuid::Uuid::new_v4());
        assert!(draft_session(&draft_id, &ctx).is_some());
        assert!(draft_session("draft-../../escape", &ctx).is_none());
        assert!(store.load(&draft_id).is_err());
        assert!(handle_ipc(
            serde_json::json!({"type":"shell_input", "session_id":draft_id, "text":"first prompt"}),
            &ctx
        ));
        assert!(matches!(
            input_rx.try_recv().unwrap(),
            ShellInput::SessionInput { .. }
        ));
        assert!(store.load(&draft_id).is_ok());
        assert_eq!(*execution.lock().unwrap(), "running-A");
        assert!(!shared.cancel.is_cancelled());
        let poisoned = execution.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("simulate poisoned identity lock");
        })
        .join();
        handle_ipc(
            serde_json::json!({"type":"shell_cancel","session_id":saved.id}),
            &ctx,
        );
        assert!(
            !shared.cancel.is_cancelled(),
            "Stop from B must not cancel A"
        );
        assert_eq!(
            replies.lock().unwrap().last().unwrap()["type"],
            "session_action_rejected"
        );
    }

    /// IpcContext can be constructed with stub closures for tests.
    /// Pin the type signature so future refactors that break Send +
    /// Sync surface in CI rather than in production.
    #[test]
    fn ipc_context_is_constructible_with_noop_transport() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let dispatch: DispatchFn = Arc::new(|_payload: String| {});
        let quit_fired = Arc::new(AtomicBool::new(false));
        let quit_fired_clone = quit_fired.clone();
        let on_quit: QuitFn = Arc::new(move || {
            quit_fired_clone.store(true, Ordering::SeqCst);
        });
        let on_send_initial_state: SendInitialStateFn = Arc::new(|| {});
        let on_zoom: ZoomFn = Arc::new(|_scale: f64| {});

        let ctx = IpcContext {
            is_serve_mode: false,
            shared,
            approver,
            pending_asks,
            dispatch,
            on_quit,
            on_send_initial_state,
            on_zoom,
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };

        // Exercise the only currently-wired arm.
        let handled = handle_ipc(serde_json::json!({"type": "app_close"}), &ctx);
        assert!(handled, "app_close is a migrated arm");
        assert!(
            quit_fired.load(Ordering::SeqCst),
            "app_close should fire on_quit"
        );
    }

    /// dev-plan/39 Tier 3: uploadFile decodes the blob into `_uploads/`
    /// and returns a servable file-asset URL.
    #[test]
    fn shell_upload_writes_and_returns_url() {
        use base64::Engine as _;
        let tmp = std::env::temp_dir().join(format!("gs-upload-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(b"hello-shell");
        let reply = shell_upload_into(&tmp, "note.txt", &data_b64).unwrap();
        let url = reply["url"].as_str().unwrap();
        let rel = reply["path"].as_str().unwrap();
        assert!(url.starts_with("file-asset/_uploads/note"), "{url}");
        assert!(rel.starts_with("_uploads/note"), "{rel}");
        assert_eq!(
            std::fs::read_to_string(tmp.join(rel)).unwrap(),
            "hello-shell"
        );
        // Bad base64 → error, not panic.
        assert!(shell_upload_into(&tmp, "x", "!!!not-base64!!!").is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// dev-plan/39 Tier 3: tool-invoke manifest gating.
    #[test]
    fn tool_allowlist_gates_only_when_declared() {
        // No tools.invoke:* declared → legacy/unfettered (allow all).
        let legacy = vec!["agent.run".to_string(), "storage".to_string()];
        assert!(tool_allowed_by_perms(&legacy, "Bash"));
        assert!(tool_allowed_by_perms(&[], "Read"));
        // Declared allowlist → only listed tools.
        let scoped = vec![
            "tools.invoke:Read".to_string(),
            "tools.invoke:Bash".to_string(),
        ];
        assert!(tool_allowed_by_perms(&scoped, "Bash"));
        assert!(tool_allowed_by_perms(&scoped, "Read"));
        assert!(!tool_allowed_by_perms(&scoped, "Write"));
        assert!(!tool_allowed_by_perms(&scoped, "KmsDelete"));
        // Wildcard grants everything.
        let wild = vec!["tools.invoke:*".to_string()];
        assert!(tool_allowed_by_perms(&wild, "Bash"));
        assert!(tool_allowed_by_perms(&wild, "AnythingElse"));
    }

    /// dev-plan/39 Tier 3: the `gui_shell_approval_respond` IPC arm
    /// resolves a pending inline approval so the awaiting tool-invoke
    /// gets the shell's verdict (not the system modal).
    #[tokio::test]
    async fn gui_shell_approval_respond_resolves_inline() {
        use crate::permissions::ApprovalDecision;
        let (id, rx) = crate::gui_shell::inline_approval::register();

        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let ctx = IpcContext {
            is_serve_mode: true,
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(|_| {}),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };
        let handled = handle_ipc(
            serde_json::json!({
                "type": "gui_shell_approval_respond",
                "approvalId": id,
                "decision": "allow",
            }),
            &ctx,
        );
        assert!(handled, "gui_shell_approval_respond is a migrated arm");
        assert_eq!(rx.await.unwrap(), ApprovalDecision::Allow);
    }

    /// A malformed / missing decision fails closed to Deny.
    #[tokio::test]
    async fn gui_shell_approval_respond_bad_decision_denies() {
        use crate::permissions::ApprovalDecision;
        let (id, rx) = crate::gui_shell::inline_approval::register();
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let ctx = IpcContext {
            is_serve_mode: true,
            shared,
            approver,
            pending_asks: Arc::new(Mutex::new(HashMap::new())),
            dispatch: Arc::new(|_| {}),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };
        let _ = handle_ipc(
            serde_json::json!({
                "type": "gui_shell_approval_respond",
                "approvalId": id,
                "decision": "wat",
            }),
            &ctx,
        );
        assert_eq!(rx.await.unwrap(), ApprovalDecision::Deny);
    }

    /// schedule_add_submit's validator branches: rejects empty fields
    /// and bad cron without ever calling ScheduleStore::save() (so
    /// the test can't pollute the real ~/.config/thclaws). Captures
    /// dispatched payloads via a Mutex<Vec<String>> and asserts the
    /// `ok: false` envelope shape.
    /// The one-shot completion bridge spends the user's credits, so an
    /// undeclared shell must not reach it.
    #[test]
    fn gui_shell_llm_complete_requires_permission() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let ctx = IpcContext {
            is_serve_mode: false,
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().unwrap().push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };
        let handled = handle_ipc(
            serde_json::json!({
                "type": "gui_shell_llm_complete",
                "id": 5,
                "shellId": "no-such-shell",
                "prompt": "rewrite this",
            }),
            &ctx,
        );
        assert!(handled);
        let payloads = captured.lock().unwrap();
        assert_eq!(payloads.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert!(
            parsed["error"].as_str().unwrap().contains("llm.complete"),
            "should name the missing permission: {parsed}"
        );
    }

    /// Plugin management is permission-gated, and a denial must not
    /// touch the registry or the files on disk.
    #[test]
    fn gui_shell_plugin_calls_require_permission() {
        for ty in [
            "gui_shell_plugins_list",
            "gui_shell_plugin_install",
            "gui_shell_plugin_set_enabled",
            "gui_shell_plugin_remove",
        ] {
            let shared = Arc::new(crate::shared_session::spawn());
            let (approver, _rx) = crate::permissions::GuiApprover::new();
            let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
            let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let captured_clone = captured.clone();
            let ctx = IpcContext {
                is_serve_mode: false,
                shared,
                approver,
                pending_asks,
                dispatch: Arc::new(move |payload| {
                    captured_clone.lock().unwrap().push(payload);
                }),
                on_quit: Arc::new(|| {}),
                on_send_initial_state: Arc::new(|| {}),
                on_zoom: Arc::new(|_| {}),
                workflow_approver: crate::workflow::WorkflowApprover::new(),
            };
            let handled = handle_ipc(
                serde_json::json!({
                    "type": ty,
                    "id": 11,
                    "shellId": "no-such-shell",
                    "name": "ops-pack",
                    "url": "https://github.com/x/ops-pack",
                    "enabled": false,
                }),
                &ctx,
            );
            assert!(handled, "{ty} should be a migrated arm");
            let payloads = captured.lock().unwrap();
            assert_eq!(payloads.len(), 1, "{ty} denial should reply once");
            let parsed: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
            assert!(
                parsed["error"].as_str().unwrap().contains("plugins."),
                "{ty} should name the missing permission: {parsed}"
            );
        }
    }

    /// Connector add/remove are permission-gated the same way BYOK is,
    /// and a denial must not touch mcp.json.
    #[test]
    fn gui_shell_connector_calls_require_permission() {
        for ty in [
            "gui_shell_connectors_list",
            "gui_shell_connector_add",
            "gui_shell_connector_remove",
        ] {
            let shared = Arc::new(crate::shared_session::spawn());
            let (approver, _rx) = crate::permissions::GuiApprover::new();
            let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
            let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let captured_clone = captured.clone();
            let ctx = IpcContext {
                is_serve_mode: false,
                shared,
                approver,
                pending_asks,
                dispatch: Arc::new(move |payload| {
                    captured_clone.lock().unwrap().push(payload);
                }),
                on_quit: Arc::new(|| {}),
                on_send_initial_state: Arc::new(|| {}),
                on_zoom: Arc::new(|_| {}),
                workflow_approver: crate::workflow::WorkflowApprover::new(),
            };
            let handled = handle_ipc(
                serde_json::json!({
                    "type": ty,
                    "id": 3,
                    "shellId": "no-such-shell",
                    "name": "evil",
                    "url": "https://example.com/mcp",
                }),
                &ctx,
            );
            assert!(handled, "{ty} should be a migrated arm");
            let payloads = captured.lock().unwrap();
            assert_eq!(payloads.len(), 1, "{ty} denial should reply once");
            let parsed: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
            assert!(
                parsed["error"].as_str().unwrap().contains("connectors."),
                "{ty} should name the missing permission: {parsed}"
            );
        }
    }

    /// The BYOK bridge is permission-gated: a shell whose manifest
    /// doesn't declare `keys.write` gets an error and NOTHING is
    /// stored — no `api_key_result` is even emitted, so the app can't
    /// mistake the attempt for a successful write.
    #[test]
    fn gui_shell_key_set_requires_permission() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let ctx = IpcContext {
            is_serve_mode: false,
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().unwrap().push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };
        let handled = handle_ipc(
            serde_json::json!({
                "type": "gui_shell_key_set",
                "id": 7,
                "shellId": "no-such-shell",
                "provider": "openai",
                "key": "sk-should-never-be-stored",
            }),
            &ctx,
        );
        assert!(handled);
        let payloads = captured.lock().unwrap();
        assert_eq!(payloads.len(), 1, "denial must not also announce a write");
        let parsed: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert_eq!(parsed["replyTo"], 7);
        assert!(
            parsed["error"].as_str().unwrap().contains("keys.write"),
            "error should name the missing permission: {parsed}"
        );
        assert!(parsed.get("result").is_none());
    }

    /// store_provider_key is the single write path shared by the
    /// Settings modal and the shell BYOK form; empty input must fail
    /// there rather than in each caller.
    #[test]
    fn store_provider_key_rejects_empty_input() {
        let (ok, err, storage) = store_provider_key("", "sk-x");
        assert!(!ok);
        assert_eq!(storage, "");
        assert!(err.contains("required"), "{err}");
        let (ok, _, _) = store_provider_key("openai", "");
        assert!(!ok);
    }

    /// schedule_cron_preview validates a cron expression and returns
    /// the next 3 fires when valid, or an inline error when not.
    /// Used by the schedule-add modal's live preview.
    #[test]
    fn schedule_cron_preview_valid() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let ctx = IpcContext {
            is_serve_mode: false,
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().unwrap().push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };
        let handled = handle_ipc(
            serde_json::json!({
                "type": "schedule_cron_preview",
                "cron": "0 9 * * *",
            }),
            &ctx,
        );
        assert!(handled);
        let payloads = captured.lock().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert_eq!(parsed["type"], "schedule_cron_preview_result");
        assert_eq!(parsed["ok"], true);
        let fires = parsed["fires"].as_array().unwrap();
        assert_eq!(fires.len(), 3);
        assert_eq!(parsed["cron"], "0 9 * * *");
    }

    #[test]
    fn schedule_cron_preview_invalid() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let ctx = IpcContext {
            is_serve_mode: false,
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().unwrap().push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };
        let _ = handle_ipc(
            serde_json::json!({
                "type": "schedule_cron_preview",
                "cron": "definitely not cron",
            }),
            &ctx,
        );
        let payloads = captured.lock().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert_eq!(parsed["ok"], false);
        let err = parsed["error"].as_str().unwrap();
        assert!(err.contains("invalid cron"), "got: {err}");
    }

    #[test]
    fn schedule_cron_preview_empty() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let ctx = IpcContext {
            is_serve_mode: false,
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().unwrap().push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };
        let _ = handle_ipc(
            serde_json::json!({
                "type": "schedule_cron_preview",
                "cron": "  ",
            }),
            &ctx,
        );
        let payloads = captured.lock().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["error"], "cron is empty");
    }

    /// `ask_user_response` must echo the user's typed answer into the
    /// Terminal tab so the cyan "assistant asks" banner pairs with a
    /// visible reply. The Chat tab is unaffected (it pushes the user
    /// bubble locally on submit).
    #[test]
    fn ask_user_response_echoes_to_terminal() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        // Pre-register a pending oneshot so resolve doesn't drop on
        // the floor — exercises the full path.
        let (tx, _rx) = tokio::sync::oneshot::channel::<String>();
        pending_asks.lock().unwrap().insert(42, tx);

        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let ctx = IpcContext {
            is_serve_mode: false,
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().expect("lock").push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };
        let handled = handle_ipc(
            serde_json::json!({
                "type": "ask_user_response",
                "id": 42,
                "text": "Try Hacker News",
            }),
            &ctx,
        );
        assert!(handled, "ask_user_response should be handled");
        let payloads = captured.lock().unwrap();
        assert_eq!(
            payloads.len(),
            1,
            "expected exactly 1 terminal_data dispatch"
        );
        let parsed: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert_eq!(parsed["type"], "terminal_data");
        let b64 = parsed["data"].as_str().unwrap();
        let bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
        let decoded = String::from_utf8(bytes).unwrap();
        assert!(
            decoded.contains("Try Hacker News"),
            "reply text missing: {decoded}"
        );
        assert!(
            decoded.contains("> "),
            "user-prompt marker missing: {decoded}"
        );
    }

    /// Empty / whitespace-only ask replies should NOT generate a
    /// stray terminal_data dispatch (otherwise an accidental enter on
    /// the chat input would emit a blank `> ` line).
    #[test]
    fn ask_user_response_empty_does_not_echo() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let ctx = IpcContext {
            is_serve_mode: false,
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().expect("lock").push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };
        let _ = handle_ipc(
            serde_json::json!({
                "type": "ask_user_response",
                "id": 1,
                "text": "   \n   ",
            }),
            &ctx,
        );
        assert!(
            captured.lock().unwrap().is_empty(),
            "whitespace-only reply must not produce terminal output"
        );
    }

    #[test]
    fn schedule_add_submit_rejects_missing_fields() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let ctx = IpcContext {
            is_serve_mode: false,
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().expect("lock").push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };

        // Empty form → must error before any save.
        let handled = handle_ipc(serde_json::json!({"type": "schedule_add_submit"}), &ctx);
        assert!(handled, "schedule_add_submit is a migrated arm");
        let payloads = captured.lock().unwrap();
        assert_eq!(payloads.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert_eq!(parsed["type"], "schedule_add_result");
        assert_eq!(parsed["ok"], false);
        let err = parsed["error"].as_str().unwrap();
        assert!(err.contains("id is required"), "got: {err}");
        assert!(err.contains("cron is required"), "got: {err}");
        assert!(err.contains("prompt is required"), "got: {err}");
        assert!(err.contains("cwd is required"), "got: {err}");
    }

    #[test]
    fn schedule_add_submit_rejects_bad_cron() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let ctx = IpcContext {
            is_serve_mode: false,
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().expect("lock").push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };

        // Use a tempdir so the cwd-exists check passes; cron is bad.
        let tmp = tempfile::tempdir().unwrap();
        let handled = handle_ipc(
            serde_json::json!({
                "type": "schedule_add_submit",
                "id": "test-bad-cron",
                "cron": "definitely not cron",
                "prompt": "hi",
                "cwd": tmp.path().to_string_lossy(),
            }),
            &ctx,
        );
        assert!(handled);
        let payloads = captured.lock().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert_eq!(parsed["ok"], false);
        let err = parsed["error"].as_str().unwrap();
        assert!(err.contains("invalid cron"), "got: {err}");
    }

    #[test]
    fn schedule_add_submit_rejects_missing_cwd() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let ctx = IpcContext {
            is_serve_mode: false,
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().expect("lock").push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };

        let handled = handle_ipc(
            serde_json::json!({
                "type": "schedule_add_submit",
                "id": "test-no-cwd",
                "cron": "* * * * *",
                "prompt": "hi",
                "cwd": "/this/path/does/not/exist/anywhere/abc123xyz",
            }),
            &ctx,
        );
        assert!(handled);
        let payloads = captured.lock().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert_eq!(parsed["ok"], false);
        let err = parsed["error"].as_str().unwrap();
        assert!(err.contains("cwd does not exist"), "got: {err}");
    }

    #[test]
    fn handle_ipc_ignores_unknown_type() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let ctx = IpcContext {
            is_serve_mode: false,
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(|_| {}),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
            workflow_approver: crate::workflow::WorkflowApprover::new(),
        };
        // Unmigrated / unknown types must return false so the wry
        // closure falls through to its own match.
        assert!(!handle_ipc(
            serde_json::json!({"type": "nonexistent_type"}),
            &ctx
        ));
        assert!(!handle_ipc(serde_json::json!({}), &ctx));
        assert!(!handle_ipc(serde_json::json!({"type": 42}), &ctx));
    }
}
