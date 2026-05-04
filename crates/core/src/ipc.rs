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

use crate::permissions::GuiApprover;
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

/// Everything the IPC dispatch needs from its surrounding transport.
/// Construct one per session in the transport's setup; pass `&` to
/// [`handle_ipc`] for each inbound message.
#[derive(Clone)]
pub struct IpcContext {
    pub shared: Arc<SharedSessionHandle>,
    pub approver: Arc<GuiApprover>,
    pub pending_asks: PendingAsks,
    pub dispatch: DispatchFn,
    pub on_quit: QuitFn,
    pub on_send_initial_state: SendInitialStateFn,
    pub on_zoom: ZoomFn,
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
pub fn handle_ipc(msg: Value, ctx: &IpcContext) -> bool {
    let ty = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");
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
            if has_attachments {
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
            if !trimmed.is_empty() {
                let _ = ctx.shared.input_tx.send(ShellInput::Line(trimmed));
            }
        }

        "frontend_ready" => {
            // Wry: just signal the ready_gate (idempotent).
            // WS: also fire on_send_initial_state so the frontend gets
            // its initial snapshot. The wry path's send_event arm
            // synthesises the same JSON via gui.rs's event-loop.
            ctx.shared.ready_gate.signal();
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
            ctx.approver.resolve(id, decision);
        }

        "shell_cancel" => {
            // Worker observes ctrl-C / cancel via the cancel token.
            ctx.shared.request_cancel();
        }

        "new_session" => {
            let _ = ctx.shared.input_tx.send(ShellInput::NewSession);
            // Mirror gui.rs's prior behavior — frontend expects an
            // ack envelope so the modal closes + a terminal_clear so
            // xterm.js wipes its scrollback.
            (ctx.dispatch)(serde_json::json!({"type": "new_session_ack"}).to_string());
            (ctx.dispatch)(serde_json::json!({"type": "terminal_clear"}).to_string());
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

        // ── Working directory (M6.36 SERVE9d — migrated from gui.rs) ─
        "get_cwd" => {
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".into());
            let payload = serde_json::json!({
                "type": "current_cwd",
                "path": cwd,
                "needs_modal": true,
                "recent_dirs": crate::recent_dirs::load_recent_dirs(),
            });
            (ctx.dispatch)(payload.to_string());
        }

        "set_cwd" => {
            if let Some(path) = msg.get("path").and_then(|v| v.as_str()) {
                let p = std::path::Path::new(path);
                if p.is_dir() {
                    let _ = std::env::set_current_dir(p);
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
            let payload = serde_json::json!({
                "type": "instructions_save_result",
                "scope": scope,
                "path": path,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
        }

        // ── Settings panel (M6.36 SERVE9e — migrated from gui.rs) ──
        "secrets_backend_get" => {
            let backend = crate::secrets::get_backend().map(|b| b.as_str().to_string());
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
            let env_var =
                crate::providers::ProviderKind::from_name(provider).and_then(|k| k.api_key_env());
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
                    "provider_ready": ready,
                });
                (ctx.dispatch)(broadcast.to_string());
                let _ = ctx
                    .shared
                    .input_tx
                    .send(crate::shared_session::ShellInput::ReloadConfig);
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

        // ── AskUserQuestion modal response (M6.36 SERVE9f) ─────────
        "ask_user_response" => {
            let id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let text = msg
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let responder = ctx
                .pending_asks
                .lock()
                .ok()
                .and_then(|mut pending| pending.remove(&id));
            if let Some(responder) = responder {
                let _ = responder.send(text);
            }
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

        // ── KMS sidebar mutators (M6.36 SERVE9f) ───────────────────
        "kms_toggle" => {
            let name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let active = msg.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
            let (ok, error) = if name.is_empty() {
                (false, "name required".to_string())
            } else {
                let mut current: Vec<String> = crate::config::ProjectConfig::load()
                    .and_then(|c| c.kms.map(|k| k.active))
                    .unwrap_or_default();
                let already = current.iter().any(|n| n == name);
                if active && !already {
                    if crate::kms::resolve(name).is_none() {
                        (false, format!("no KMS named '{name}'"))
                    } else {
                        current.push(name.to_string());
                        match crate::config::ProjectConfig::set_active_kms(current) {
                            Ok(()) => (true, String::new()),
                            Err(e) => (false, e.to_string()),
                        }
                    }
                } else if !active && already {
                    current.retain(|n| n != name);
                    match crate::config::ProjectConfig::set_active_kms(current) {
                        Ok(()) => (true, String::new()),
                        Err(e) => (false, e.to_string()),
                    }
                } else {
                    (true, String::new())
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
            // Follow up with a fresh list so the UI reflects persisted state.
            (ctx.dispatch)(crate::kms::build_update_payload().to_string());
        }

        "kms_new" => {
            let name = msg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let scope_str = msg.get("scope").and_then(|v| v.as_str()).unwrap_or("user");
            let scope = match scope_str {
                "project" => crate::kms::KmsScope::Project,
                _ => crate::kms::KmsScope::User,
            };
            let (ok, error) = if name.is_empty() {
                (false, "name required".to_string())
            } else {
                match crate::kms::create(name, scope) {
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

        // ── api_key_set (M6.36 SERVE9f — full rich path) ──────────
        "api_key_set" => {
            let provider = msg.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            let key = msg.get("key").and_then(|v| v.as_str()).unwrap_or("").trim();
            // Route strictly by the user's stored backend choice.
            // Keychain is tried only when the user opted into it; dotenv
            // users never trigger an OS keychain prompt.
            let (ok, error, storage) = if provider.is_empty() || key.is_empty() {
                (false, "provider and key are required".to_string(), "")
            } else {
                let env_var = crate::providers::ProviderKind::from_name(provider)
                    .and_then(|k| k.api_key_env());
                let backend =
                    crate::secrets::get_backend().unwrap_or(crate::secrets::Backend::Keychain);
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
            };
            let payload = serde_json::json!({
                "type": "api_key_result",
                "action": "set",
                "provider": provider,
                "ok": ok,
                "error": error,
                "storage": storage,
            });
            (ctx.dispatch)(payload.to_string());
            // Auto-switch + post-key model picker, mirroring gui.rs.
            if ok {
                let cfg = crate::config::AppConfig::load().unwrap_or_default();
                if let Some(new_model) = crate::providers::auto_fallback_model(&cfg) {
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
                        "provider_ready": ready,
                    });
                    (ctx.dispatch)(broadcast.to_string());
                    let cat = crate::model_catalogue::EffectiveCatalogue::load();
                    let models = cat.list_models_for_provider(provider);
                    let runtime_loaded =
                        matches!(provider, "ollama" | "ollama-anthropic" | "lmstudio");
                    if models.len() >= 3 && !runtime_loaded {
                        let model_rows: Vec<serde_json::Value> = models
                            .iter()
                            .map(|(id, e)| {
                                serde_json::json!({
                                    "id": id,
                                    "context": e.context,
                                    "max_output": e.max_output,
                                })
                            })
                            .collect();
                        let picker = serde_json::json!({
                            "type": "model_picker_open",
                            "provider": provider,
                            "current": new_cfg.model,
                            "models": model_rows,
                        });
                        (ctx.dispatch)(picker.to_string());
                    }
                } else {
                    let provider_name = cfg.detect_provider().unwrap_or("unknown");
                    let ready = crate::providers::provider_has_credentials(&cfg);
                    let broadcast = serde_json::json!({
                        "type": "provider_update",
                        "provider": provider_name,
                        "model": cfg.model,
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
            let mut user_names: Vec<&str> = user_cmds.commands.keys().map(String::as_str).collect();
            user_names.sort();
            for name in user_names {
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
            tokio::spawn(async move {
                let policy = match crate::policy::active()
                    .and_then(|a| a.policy.policies.sso.as_ref())
                    .cloned()
                {
                    Some(p) if p.enabled => p,
                    _ => {
                        let payload = serde_json::json!({
                            "type": "sso_state",
                            "enabled": false,
                            "logged_in": false,
                            "error": "SSO not enabled in org policy",
                        });
                        dispatch(payload.to_string());
                        return;
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
            if let Some(p) = crate::policy::active().and_then(|a| a.policy.policies.sso.as_ref()) {
                let _ = crate::sso::logout(p);
            }
            (ctx.dispatch)(crate::sso::build_state_payload().to_string());
        }

        // ── File browser (M6.36 SERVE9i) ──────────────────────────
        "file_list" => {
            let raw_path = crate::file_preview::ospath(
                msg.get("path").and_then(|v| v.as_str()).unwrap_or("."),
            );
            let resolved = crate::sandbox::Sandbox::check(&raw_path)
                .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
            if let Ok(entries) = std::fs::read_dir(&resolved) {
                let mut items: Vec<serde_json::Value> = entries
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if name.starts_with('.') {
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
                    let mime = match ext.as_str() {
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "gif" => "image/gif",
                        "svg" => "image/svg+xml",
                        "webp" => "image/webp",
                        "ico" => "image/x-icon",
                        "bmp" => "image/bmp",
                        "pdf" => "application/pdf",
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
                    if is_image || is_pdf {
                        if let Ok(bytes) = std::fs::read(&path) {
                            let b64 = crate::file_preview::encode_bytes_b64(&bytes);
                            let payload = serde_json::json!({
                                "type": "file_content",
                                "path": raw_path,
                                "content": b64,
                                "mime": mime,
                                "mode": mode,
                            });
                            (ctx.dispatch)(payload.to_string());
                        }
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

        // ── Session sidebar mutators (M6.36 SERVE9j) ──────────────
        "session_load" => {
            if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                let _ = ctx
                    .shared
                    .input_tx
                    .send(crate::shared_session::ShellInput::LoadSession(
                        id.to_string(),
                    ));
            }
        }

        "session_rename" => {
            let id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let title = msg.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, error) = if id.is_empty() {
                (false, "id required".to_string())
            } else {
                match crate::session::SessionStore::default_path()
                    .map(crate::session::SessionStore::new)
                {
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
                let store = crate::session::SessionStore::default_path()
                    .map(crate::session::SessionStore::new);
                (ctx.dispatch)(crate::shared_session::build_session_list(&store, ""));
            }
        }

        "session_delete" => {
            let id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let (ok, error) = if id.is_empty() {
                (false, "id required".to_string())
            } else {
                match crate::session::SessionStore::default_path()
                    .map(crate::session::SessionStore::new)
                {
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
                let store = crate::session::SessionStore::default_path()
                    .map(crate::session::SessionStore::new);
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
            shared,
            approver,
            pending_asks,
            dispatch,
            on_quit,
            on_send_initial_state,
            on_zoom,
        };

        // Exercise the only currently-wired arm.
        let handled = handle_ipc(serde_json::json!({"type": "app_close"}), &ctx);
        assert!(handled, "app_close is a migrated arm");
        assert!(
            quit_fired.load(Ordering::SeqCst),
            "app_close should fire on_quit"
        );
    }

    #[test]
    fn handle_ipc_ignores_unknown_type() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let ctx = IpcContext {
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(|_| {}),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
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
