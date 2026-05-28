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
                Ok(read) => serde_json::json!({
                    "type": "kms_file_content",
                    "kms": kms_name,
                    "kind": kind,
                    "name": file,
                    "content": read.content,
                    "total_bytes": read.total_bytes,
                    "truncated": read.truncated,
                    "ok": true,
                }),
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
                        "kind": s.kind,
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
                "use_for": cfg.gateway_use_for,
            });
            (ctx.dispatch)(payload.to_string());
        }
        "gateway_settings_set" => {
            let use_for: Vec<String> = msg
                .get("use_for")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.trim().to_lowercase()))
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let mut cfg = crate::config::ProjectConfig::load().unwrap_or_default();
            cfg.set_gateway_use_for(use_for.clone());
            let (ok, error) = match cfg.save() {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e.to_string()),
            };
            let payload = serde_json::json!({
                "type": "gateway_settings_result",
                "base_url": crate::providers::thclaws_gateway::GATEWAY_BASE_URL,
                "use_for": use_for,
                "ok": ok,
                "error": error,
            });
            (ctx.dispatch)(payload.to_string());
            let _ = ctx
                .shared
                .input_tx
                .send(crate::shared_session::ShellInput::ReloadConfig);
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
            let key = msg.get("key").and_then(|v| v.as_str()).unwrap_or("").trim();
            // Route strictly by the user's stored backend choice.
            // Keychain is tried only when the user opted into it; dotenv
            // users never trigger an OS keychain prompt.
            let (ok, error, storage) = if provider.is_empty() || key.is_empty() {
                (false, "provider and key are required".to_string(), "")
            } else {
                let env_var = crate::providers::ProviderKind::from_name(provider)
                    .and_then(|k| k.api_key_env())
                    .or_else(|| crate::secrets::service_env_var(provider));
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
                    let mut models = cat.list_models_for_provider(provider);
                    models.retain(|(_, e)| e.chat != Some(false));
                    if provider == "openrouter" && new_cfg.openrouter_free_only {
                        models.retain(|(_, e)| e.free == Some(true));
                    }
                    let runtime_loaded =
                        matches!(provider, "ollama" | "ollama-anthropic" | "lmstudio");
                    if models.len() >= 3 && !runtime_loaded {
                        let _ = crate::providers::ProviderKind::detect(&new_cfg.model);
                        let model_rows: Vec<serde_json::Value> = models
                            .iter()
                            .map(|(id, e)| {
                                let canonical =
                                    crate::model_catalogue::canonical_model_id(provider, id);
                                serde_json::json!({
                                    "id": canonical,
                                    "context": e.context,
                                    "max_output": e.max_output,
                                    // Plan-10: surfaced for the
                                    // OpenRouter "Free only" toggle
                                    // in the Settings modal. Other
                                    // providers leave this None.
                                    "free": e.free,
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

    /// schedule_add_submit's validator branches: rejects empty fields
    /// and bad cron without ever calling ScheduleStore::save() (so
    /// the test can't pollute the real ~/.config/thclaws). Captures
    /// dispatched payloads via a Mutex<Vec<String>> and asserts the
    /// `ok: false` envelope shape.
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
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().unwrap().push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
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
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().unwrap().push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
        };
        handle_ipc(
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
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().unwrap().push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
        };
        handle_ipc(
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
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().expect("lock").push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
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
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().expect("lock").push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
        };
        handle_ipc(
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
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().expect("lock").push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
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
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().expect("lock").push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
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
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(move |payload| {
                captured_clone.lock().expect("lock").push(payload);
            }),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
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
