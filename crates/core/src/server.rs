//! HTTP + WebSocket server for `--serve` mode (M6.36).
//!
//! Serves the embedded React frontend over HTTP and bridges IPC over a
//! WebSocket to the same `SharedSessionHandle` engine the desktop GUI
//! uses. One process per project — `cd <project> && thclaws --serve
//! --port <N>` is the deployment unit.
//!
//! ## Routes
//!
//! - `GET /` — serves the frontend `index.html` (single-file vite
//!   build, embedded via `include_str!`)
//! - `GET /healthz` — `200 ok` liveness probe
//! - `GET /ws` — WebSocket upgrade. Inbound JSON frames route through
//!   [`crate::ipc::handle_ipc`] with a WS-flavored [`IpcContext`].
//!   Outbound event rendering (subscribing to `events_tx`, translating
//!   ViewEvents to chat/terminal-shaped JSON) lands in SERVE3.
//!
//! ## Trust model
//!
//! Single-user. Phase 1 binds to `127.0.0.1` only — operator runs an
//! SSH tunnel for remote access (no app-side auth). Anyone reaching
//! the bound socket has full access to the engine: BashTool runs as
//! the server user, file tools touch the server filesystem. Treat the
//! tunnel as the auth boundary.

use crate::config::AppConfig;
use crate::event_render::{
    render_chat_dispatches, render_terminal_ansi, terminal_data_envelope,
    terminal_history_replaced_envelope, TerminalRenderState,
};
use crate::ipc::{handle_ipc, IpcContext, PendingAsks};
use crate::providers::provider_has_credentials;
use crate::session::SessionStore;
use crate::shared_session::{SharedSessionHandle, ShellInput, ViewEvent};
use crate::uploads::{
    ensure_uploads_dir, render_upload_message, unique_path, UploadedFile, UPLOADS_DIRNAME,
    UPLOAD_MAX_BYTES, UPLOAD_MAX_FILES,
};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

/// The same single-file React build the desktop GUI embeds. Re-embedded
/// here under the always-on `crate::server` module so the frontend is
/// bundled regardless of the `gui` feature.
const FRONTEND_HTML: &str = include_str!("../../../frontend/dist/index.html");

#[derive(Clone)]
pub struct ServeConfig {
    pub bind: SocketAddr,
    /// Workspace root used for upload destinations and (future)
    /// sandbox scoping. `None` means "use process cwd at `run` time",
    /// which is the production default. Tests inject a tempdir to
    /// avoid touching global cwd.
    pub workspace: Option<std::path::PathBuf>,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            // Localhost-only by default — Phase 1 trust model is "SSH
            // tunnel handles auth". Override via --bind if you know
            // what you're doing.
            bind: ([127, 0, 0, 1], 8443).into(),
            workspace: None,
        }
    }
}

/// State shared across HTTP / WS handlers. The `SharedSessionHandle`
/// IS the engine — same Arc lives in every WS connection so multi-tab
/// browsers see the same conversation.
///
/// `ask_broadcast` carries `ask_user_question` JSON envelopes to every
/// connected WS client. Pre-fix the standalone `--serve` path never
/// wired `set_gui_ask_sender`, so the agent's `AskUserQuestion` tool
/// posted to a `None` sender and stalled the turn waiting for a
/// oneshot that was never created (issue #82). The forwarder spawned
/// in [`run`] reads from the global ask channel and pushes JSON
/// frames to this broadcast; [`handle_socket`] subscribes per
/// connection so every browser tab sees the question.
#[derive(Clone)]
struct ServeState {
    shared: Arc<SharedSessionHandle>,
    approver: Arc<crate::permissions::GuiApprover>,
    pending_asks: PendingAsks,
    ask_broadcast: broadcast::Sender<String>,
    workspace: Arc<std::path::PathBuf>,
}

/// Spin up the server. Spawns the worker, builds the Axum router,
/// blocks until the listener returns (Ctrl-C / panic / shutdown).
pub async fn run(config: ServeConfig) -> crate::error::Result<()> {
    // M6.36 SERVE6 hint: keychain access doesn't make sense on a
    // headless server (no user session, often no Secret Service
    // running). Skip the keychain probe by default; users put API
    // keys in `.thclaws/.env` instead. CLI flag override TBD.
    if std::env::var_os("THCLAWS_DISABLE_KEYCHAIN").is_none() {
        std::env::set_var("THCLAWS_DISABLE_KEYCHAIN", "1");
    }

    let (approver, _approval_rx) = crate::permissions::GuiApprover::new();
    let shared = Arc::new(crate::shared_session::spawn_with_approver(approver.clone()));
    // The frontend's "I'm ready" handshake unblocks deferred startup
    // (MCP spawn, etc.). Without a UI to wait on, signal immediately
    // so the worker doesn't sit waiting for a frontend that won't
    // appear until the first browser tab connects.
    shared.ready_gate.signal();
    let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));

    // AskUserQuestion bridge (issue #82). Mirrors gui.rs:541-543 +
    // 576-610. Pre-fix `set_gui_ask_sender` was never called in the
    // standalone serve path, so the tool's `GUI_ASK_SENDER` static
    // stayed `None` and `AskUserRequest` posts had nowhere to go —
    // the agent hung on its oneshot waiting for a response that
    // could never arrive. The forwarder below reads ask requests
    // from the global channel, stashes the oneshot responder in
    // `pending_asks` (so `ipc::handle_ipc`'s `ask_user_response`
    // arm can resolve it when the frontend replies), and broadcasts
    // the question JSON to every connected WS client via
    // `ask_broadcast`. Capacity 16 is generous — multiple in-flight
    // ask questions are rare, and lag is logged but tolerated.
    let (ask_tx, mut ask_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::tools::AskUserRequest>();
    crate::tools::set_gui_ask_sender(Some(ask_tx));
    let (ask_broadcast, _) = broadcast::channel::<String>(16);
    {
        let ask_broadcast_for_fwd = ask_broadcast.clone();
        let pending_asks_for_fwd = pending_asks.clone();
        tokio::spawn(async move {
            while let Some(req) = ask_rx.recv().await {
                let id = req.id;
                let question = req.question.clone();
                if let Ok(mut pending) = pending_asks_for_fwd.lock() {
                    pending.insert(id, req.response);
                }
                let payload = serde_json::json!({
                    "type": "ask_user_question",
                    "id": id,
                    "question": question,
                });
                // No-op when zero subscribers — early questions before
                // any tab connects are silently dropped (the agent
                // will still time out on its own retry path; can't
                // queue indefinitely without losing the oneshot to
                // GC).
                let _ = ask_broadcast_for_fwd.send(payload.to_string());
            }
        });
    }

    run_with_engine(config, approver, shared, pending_asks, ask_broadcast).await
}

/// Same as [`run`], but reuses an engine constructed by the caller. Used
/// by the `--serve --gui` combo path so the desktop window and any
/// browser tab share one Agent + Session — i.e. the same conversation
/// is visible from both surfaces.
pub async fn run_with_engine(
    config: ServeConfig,
    approver: Arc<crate::permissions::GuiApprover>,
    shared: Arc<SharedSessionHandle>,
    pending_asks: PendingAsks,
    ask_broadcast: broadcast::Sender<String>,
) -> crate::error::Result<()> {
    let workspace = match config.workspace.clone() {
        Some(p) => p,
        None => std::env::current_dir()
            .map_err(|e| crate::error::Error::Tool(format!("workspace cwd unavailable: {e}")))?,
    };
    let state = ServeState {
        shared,
        approver,
        pending_asks,
        ask_broadcast,
        workspace: Arc::new(workspace),
    };

    // Loopback-only safety check for the API auth-bypass token. The
    // bypass mode (`THCLAWS_API_TOKEN=disable-auth`) makes the OpenAI
    // endpoints reachable to anyone who can hit the socket — refuse to
    // start if the bind isn't loopback, so a misconfigured deploy fails
    // loud instead of silently exposing the agent runtime.
    if crate::api_v1::auth_is_bypassed() && !is_loopback(&config.bind) {
        return Err(crate::error::Error::Tool(format!(
            "THCLAWS_API_TOKEN=disable-auth is only allowed on a loopback bind, but server is bound to {}. \
             Set a real token or use --bind 127.0.0.1.",
            config.bind
        )));
    }

    let app = Router::new()
        .route("/", get(serve_index))
        .route("/healthz", get(serve_health))
        .route("/ws", get(ws_handler))
        .route("/upload", post(serve_upload))
        .with_state(state)
        // /v1/* OpenAI-compatible endpoints. Merged at the end so the
        // existing routes win on path collisions (none today).
        .merge(crate::api_v1::router());

    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .map_err(|e| crate::error::Error::Tool(format!("bind {}: {e}", config.bind)))?;
    eprintln!(
        "\x1b[36m[serve] thClaws listening on http://{}\x1b[0m",
        config.bind
    );
    eprintln!("\x1b[36m[serve] open the URL above in your browser (over an SSH tunnel for remote access)\x1b[0m");
    axum::serve(listener, app)
        .await
        .map_err(|e| crate::error::Error::Tool(format!("serve: {e}")))?;
    Ok(())
}

fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

async fn serve_index() -> impl IntoResponse {
    // No-cache headers so users always see the bundle from the
    // running binary. Pre-fix, no `Cache-Control` was set and
    // browsers applied heuristic caching → after `make install` +
    // `--serve` restart, an already-open tab kept serving the old
    // HTML and users thought new UI was missing (May 2026 report:
    // the Gemma settings gear "didn't appear" until hard-refresh).
    // The bundle is embedded in the binary, so the right
    // freshness signal is "the binary mtime" — easiest to express
    // as `no-store` for this single, small endpoint.
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (
                axum::http::header::CACHE_CONTROL,
                "no-store, must-revalidate",
            ),
        ],
        FRONTEND_HTML,
    )
}

async fn serve_health() -> impl IntoResponse {
    "ok"
}

/// `POST /upload` — multipart file upload from the --serve browser
/// surface. Each part lands at `<workspace>/uploads/<name>` (with
/// `_N` suffix on collision). After all parts are saved, the handler
/// synthesizes a chat-shaped user message and pushes it through the
/// shared session input pipe — the agent reacts as if the user had
/// typed a description of what they just uploaded, and project
/// `AGENTS.md` instructions steer what happens next.
///
/// Returns `{ "ok": true, "files": [{ "path": …, "size": … }, …] }`
/// so the frontend can show a confirmation chip per file. Caps:
/// [`UPLOAD_MAX_BYTES`] per file, [`UPLOAD_MAX_FILES`] per request.
/// Oversize / overflow is rejected with 413.
async fn serve_upload(
    State(state): State<ServeState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let workspace = state.workspace.as_ref();
    let uploads_dir = match ensure_uploads_dir(workspace) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("cannot create uploads dir: {e}"),
                })),
            )
                .into_response();
        }
    };

    let mut saved: Vec<UploadedFile> = Vec::new();
    while let Some(field) = match multipart.next_field().await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("malformed multipart: {e}"),
                })),
            )
                .into_response();
        }
    } {
        if saved.len() >= UPLOAD_MAX_FILES {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("at most {UPLOAD_MAX_FILES} files per request"),
                })),
            )
                .into_response();
        }
        let filename = field
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "upload".to_string());
        let media_type = field.content_type().map(|s| s.to_string());
        let dest = unique_path(&uploads_dir, &filename);
        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "ok": false,
                        "error": format!("read part bytes: {e}"),
                    })),
                )
                    .into_response();
            }
        };
        if bytes.len() as u64 > UPLOAD_MAX_BYTES {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "{} exceeds {}-byte cap",
                        filename, UPLOAD_MAX_BYTES
                    ),
                })),
            )
                .into_response();
        }
        if let Err(e) = std::fs::write(&dest, &bytes) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("write {}: {e}", dest.display()),
                })),
            )
                .into_response();
        }
        let relative_path = dest
            .strip_prefix(workspace)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| format!("{UPLOADS_DIRNAME}/{filename}"));
        saved.push(UploadedFile {
            relative_path,
            media_type,
            size_bytes: bytes.len() as u64,
        });
    }

    if saved.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": "no files in request",
            })),
        )
            .into_response();
    }

    let synth = render_upload_message("serve", &saved);
    let _ = state.shared.input_tx.send(ShellInput::Line(synth));

    let files: Vec<serde_json::Value> = saved
        .iter()
        .map(|f| {
            serde_json::json!({
                "path": f.relative_path,
                "size": f.size_bytes,
                "media_type": f.media_type,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "files": files })),
    )
        .into_response()
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<ServeState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// One task per WS connection. Receives inbound frames, parses JSON,
/// routes through `handle_ipc` with a WS-flavored `IpcContext` whose
/// `dispatch` closure pushes payloads back over the socket.
///
/// Outbound event subscription (events_tx → WS frames) lands in SERVE3
/// alongside the snapshot frame. SERVE2's WS is half-duplex (inbound
/// only) so the IpcContext + handle_ipc plumbing can be smoke-tested
/// before the rendering layer is wired.
async fn handle_socket(socket: WebSocket, state: ServeState) {
    let (mut sink, mut stream) = socket.split();
    // Outbound channel: every dispatch closure invocation lands here;
    // a single task drains it to the sink so concurrent dispatches
    // don't race on the WS write side.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let dispatch = {
        let tx = out_tx.clone();
        Arc::new(move |payload: String| {
            let _ = tx.send(payload);
        })
    };
    // Snapshot builder for `frontend_ready` handshake (issue #80).
    // Mirrors gui.rs:1060-1109's `UserEvent::SendInitialState` arm:
    // gathers provider/model + readiness + MCP servers + recent
    // sessions + active KMSes into one JSON envelope and ships it
    // back. Pre-fix this was a no-op stub (M6.36 SERVE3 deferred
    // implementation), so every fresh browser connect (including
    // an F5 refresh on an existing session) landed on a fully
    // hydrated worker but rendered an empty sidebar — sessions /
    // MCP / KMS were all wiped from the user's perspective even
    // though the engine still had them.
    let initial_dispatch = {
        let tx = out_tx.clone();
        Arc::new(move |payload: String| {
            let _ = tx.send(payload);
        })
    };
    let ctx = IpcContext {
        shared: state.shared.clone(),
        approver: state.approver.clone(),
        pending_asks: state.pending_asks.clone(),
        dispatch,
        on_quit: Arc::new(|| {
            eprintln!(
                "\x1b[36m[serve] frontend requested app_close — closing WS connection\x1b[0m"
            );
        }),
        on_send_initial_state: Arc::new(move || {
            let payload = build_initial_state_payload();
            let _ = initial_dispatch(payload);
        }),
        on_zoom: Arc::new(|_scale| {
            // Browser handles its own zoom (Cmd-+/-); no server-side
            // hook needed unless we want to persist the scale across
            // sessions. Defer.
        }),
        workflow_approver: state.shared.workflow_approver.clone(),
    };

    // Ask-user broadcast subscription (issue #82). Each WS connection
    // gets its own receiver; the forwarder spawned in [`run`] pushes
    // one envelope per `AskUserQuestion` tool call.
    let mut ask_rx = state.ask_broadcast.subscribe();
    let ask_tx = out_tx.clone();
    let ask_forwarder = tokio::spawn(async move {
        loop {
            match ask_rx.recv().await {
                Ok(payload) => {
                    if ask_tx.send(payload).is_err() {
                        return;
                    }
                }
                // Slow consumer dropped frames; resume — the agent
                // re-asks on retry, and lagged ask-frames are no
                // worse than the pre-fix state (which was complete
                // silence).
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            }
        }
    });

    // M6.36 SERVE3: subscribe to the broadcast and translate every
    // ViewEvent into chat-shaped + terminal-shaped envelopes, identical
    // to gui::spawn_event_translator's path. Both translators feed the
    // same outbound channel so the writer task serializes WS writes.
    let mut events_rx = state.shared.subscribe();
    let event_tx = out_tx.clone();
    let event_forwarder = tokio::spawn(async move {
        let mut term_state = TerminalRenderState::default();
        loop {
            match events_rx.recv().await {
                Ok(ev) => {
                    // QuitRequested is a worker-side signal that the
                    // user typed `/quit` — we close the WS so the
                    // browser sees the disconnect and can decide what
                    // to do next (today: nothing; future: snapshot
                    // re-fetch on reconnect handles state).
                    if matches!(ev, ViewEvent::QuitRequested) {
                        break;
                    }
                    for dispatch in render_chat_dispatches(&ev) {
                        if event_tx.send(dispatch).is_err() {
                            return;
                        }
                    }
                    if let Some(ansi) = render_terminal_ansi(&mut term_state, &ev) {
                        let envelope = if matches!(ev, ViewEvent::HistoryReplaced(_)) {
                            terminal_history_replaced_envelope(&ansi)
                        } else {
                            terminal_data_envelope(&ansi)
                        };
                        if event_tx.send(envelope).is_err() {
                            return;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Slow consumer dropped events; ignore and resume
                    // — Phase 1A reconnect-with-snapshot-replay will
                    // re-sync state on next ws drop.
                    continue;
                }
                Err(_) => break,
            }
        }
    });

    // Outbound writer task — serializes every payload to the WS sink.
    let writer = tokio::spawn(async move {
        while let Some(payload) = out_rx.recv().await {
            if sink.send(Message::text(payload)).await.is_err() {
                break;
            }
        }
    });

    // Inbound reader loop.
    while let Some(frame) = stream.next().await {
        match frame {
            Ok(Message::Text(text)) => {
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(text.as_str()) else {
                    continue;
                };
                // Web has no fall-through transport — anything
                // handle_ipc doesn't recognize is silently dropped.
                let _handled = handle_ipc(msg, &ctx);
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {} // ignore Ping/Pong/Binary for now
        }
    }
    event_forwarder.abort();
    ask_forwarder.abort();
    writer.abort();
}

/// Build the `initial_state` JSON envelope ported from gui.rs's
/// `UserEvent::SendInitialState` arm (gui.rs:1060-1109). Loaded
/// fresh from disk on every WS connect so an F5 refresh always
/// reflects the current `AppConfig` / sessions / MCP / KMS state.
///
/// Auto-fallback model: if the saved model's provider has no
/// credentials but another provider does, switch + persist so the
/// "ready" indicator in the sidebar is accurate after the user adds
/// a key.
fn build_initial_state_payload() -> String {
    let mut config = AppConfig::load().unwrap_or_default();
    if let Some(new_model) = crate::providers::auto_fallback_model(&config) {
        let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
        project.set_model(&new_model);
        let _ = project.save();
        config = AppConfig::load().unwrap_or_default();
    }
    let provider_name = config.detect_provider().unwrap_or("unknown");
    let provider_ready = provider_has_credentials(&config);
    // Consult the live MCP_TOOL_COUNTS cache (populated by the
    // McpReady worker event) so reconnect-after-startup ships real
    // counts instead of the hardcoded zeros that surfaced as issue #86.
    let mcp_servers = crate::gui::build_mcp_servers_payload(&config);
    let sessions: Vec<serde_json::Value> = SessionStore::default_path()
        .map(SessionStore::new)
        .and_then(|store| store.list().ok())
        .unwrap_or_default()
        .into_iter()
        .take(20)
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "model": s.model,
                "messages": s.message_count,
                "title": s.title,
            })
        })
        .collect();
    let kmss = build_kms_initial_payload(&config);
    // #95(c): on WS open the frontend's mount-time `team_enabled_get`
    // request can be dropped if the socket is still CONNECTING — the
    // wsSend guard logs and discards (frontend/src/hooks/useIPC.ts:114).
    // Ship the flag here so every (re)connect-driven initial_state
    // carries it and the Team tab heals automatically without the user
    // having to open Settings to incidentally refire the get.
    let team_enabled = crate::config::ProjectConfig::load()
        .and_then(|c| c.team_enabled)
        .unwrap_or(false);
    serde_json::json!({
        "type": "initial_state",
        "provider": provider_name,
        "model": config.model,
        "provider_ready": provider_ready,
        "mcp_servers": mcp_servers,
        "sessions": sessions,
        "kmss": kmss,
        "team_enabled": team_enabled,
        "version": crate::version::VERSION,
    })
    .to_string()
}

/// KMS list for the initial-state payload. Mirrors the structure
/// the GUI emits in `ViewEvent::KmsUpdate` (gui.rs uses
/// `build_kms_update_payload`, which lives behind the `gui` feature
/// flag and isn't reachable from the always-on `server` module).
/// One inline implementation here keeps the build feature-free.
///
/// Uses `kms::list_all()` which returns project entries first then
/// user (matching the resolve-priority order). Dedup by name —
/// project wins on collision since `list_all` emits them first.
fn build_kms_initial_payload(config: &AppConfig) -> Vec<serde_json::Value> {
    let active: std::collections::HashSet<&str> =
        config.kms_active.iter().map(String::as_str).collect();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut all: Vec<(String, &'static str, bool)> = Vec::new();
    for kref in crate::kms::list_all() {
        if !seen.insert(kref.name.clone()) {
            // Already saw this name in a higher-priority scope.
            continue;
        }
        let scope = match kref.scope {
            crate::kms::KmsScope::Project => "project",
            crate::kms::KmsScope::User => "user",
        };
        let active_flag = active.contains(kref.name.as_str());
        all.push((kref.name, scope, active_flag));
    }
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all.into_iter()
        .map(|(name, scope, active)| {
            serde_json::json!({ "name": name, "scope": scope, "active": active })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ServeConfig defaults bind to localhost — security-relevant
    /// invariant (Phase 1 trust model). Pin so a future refactor that
    /// loosens the default surfaces in CI.
    #[test]
    fn default_serve_config_binds_localhost() {
        let cfg = ServeConfig::default();
        assert_eq!(cfg.bind.ip(), std::net::IpAddr::from([127, 0, 0, 1]));
        assert_eq!(cfg.bind.port(), 8443);
    }

    /// M6.36 SERVE7: end-to-end WS round-trip integration test.
    ///
    /// Spins up `server::run` in a background task on an OS-assigned
    /// port, opens a WebSocket client via tokio-tungstenite, sends
    /// `frontend_ready` + a `/help` slash command, asserts the server
    /// fires the expected chat-shaped envelopes back. This is the
    /// regression backstop for the WS pipeline — any future refactor
    /// that breaks the inbound dispatch, the outbound translator, or
    /// the per-connection writer task will fail this test in CI.
    #[tokio::test]
    async fn ws_round_trip_processes_slash_command() {
        use futures::{SinkExt, StreamExt};
        use std::time::Duration;
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

        // Bind to an OS-assigned port so concurrent test runs don't
        // collide. We pre-bind a TcpListener to discover the port,
        // then drop the listener and let server::run rebind. Tiny
        // window for race; in practice fine for unit tests.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let cfg = ServeConfig {
            bind: addr,
            ..Default::default()
        };
        let server_handle = tokio::spawn(async move {
            let _ = run(cfg).await;
        });

        // Give the server a beat to bind. Healthz poll loop catches
        // the race more reliably than a fixed sleep.
        let url = format!("ws://{addr}/ws");
        let healthz_url = format!("http://{addr}/healthz");
        let mut bound = false;
        for _ in 0..50 {
            if reqwest::get(&healthz_url).await.is_ok() {
                bound = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(bound, "server didn't bind within 2.5s");

        let (mut ws, _resp) = connect_async(&url).await.expect("ws connect");

        // Frontend's typical opening handshake.
        ws.send(WsMessage::text(
            serde_json::json!({"type": "frontend_ready"}).to_string(),
        ))
        .await
        .expect("ws send frontend_ready");

        // Slash command — produces SlashOutput events without needing
        // any LLM provider configured (no API keys in CI).
        ws.send(WsMessage::text(
            serde_json::json!({"type": "shell_input", "text": "/help"}).to_string(),
        ))
        .await
        .expect("ws send shell_input");

        // Drain frames for up to 3s collecting `type` values; assert
        // the canonical sequence shows up.
        let mut seen: Vec<String> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), ws.next()).await {
                Ok(Some(Ok(WsMessage::Text(text)))) => {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text.as_str()) {
                        if let Some(t) = parsed.get("type").and_then(|v| v.as_str()) {
                            seen.push(t.to_string());
                            if t == "chat_done" {
                                break;
                            }
                        }
                    }
                }
                Ok(Some(Ok(_other))) => {} // ping/pong/binary — ignore
                Ok(Some(Err(_))) | Ok(None) => break,
                Err(_) => continue, // timeout — keep polling until deadline
            }
        }

        // Echo back what we observed so failure messages are debuggable.
        assert!(
            seen.contains(&"chat_user_message".to_string()),
            "missing chat_user_message; saw: {seen:?}"
        );
        assert!(
            seen.contains(&"chat_slash_output".to_string()),
            "missing chat_slash_output (slash command body); saw: {seen:?}"
        );
        assert!(
            seen.contains(&"chat_done".to_string()),
            "missing chat_done (turn termination); saw: {seen:?}"
        );

        // Clean shutdown.
        let _ = ws.send(WsMessage::Close(None)).await;
        server_handle.abort();
    }

    /// `POST /upload` saves a multipart file to `<workspace>/uploads/`,
    /// applies `_N` suffix on collision. Workspace is injected via
    /// `ServeConfig.workspace` so the test doesn't touch process cwd
    /// (which would race with other tests in the same binary).
    #[tokio::test]
    async fn upload_post_saves_to_workspace_uploads_dir() {
        use std::time::Duration;

        let td = tempfile::tempdir().unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let cfg = ServeConfig {
            bind: addr,
            workspace: Some(td.path().to_path_buf()),
        };
        let server_handle = tokio::spawn(async move {
            let _ = run(cfg).await;
        });

        let healthz_url = format!("http://{addr}/healthz");
        for _ in 0..50 {
            if reqwest::get(&healthz_url).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let upload_url = format!("http://{addr}/upload");
        let body_a = vec![0u8; 16];
        let part_a = reqwest::multipart::Part::bytes(body_a.clone())
            .file_name("photo.jpg")
            .mime_str("image/jpeg")
            .unwrap();
        let form = reqwest::multipart::Form::new().part("file", part_a);

        let resp = reqwest::Client::new()
            .post(&upload_url)
            .multipart(form)
            .send()
            .await
            .expect("upload POST");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let json: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(json["ok"], serde_json::Value::Bool(true));
        assert_eq!(json["files"][0]["path"], "uploads/photo.jpg");
        assert_eq!(json["files"][0]["size"], 16);

        assert!(td.path().join("uploads").join("photo.jpg").exists());

        // Second upload with the same name → `_1` suffix.
        let part_b = reqwest::multipart::Part::bytes(vec![1u8; 8])
            .file_name("photo.jpg")
            .mime_str("image/jpeg")
            .unwrap();
        let form2 = reqwest::multipart::Form::new().part("file", part_b);
        let resp2 = reqwest::Client::new()
            .post(&upload_url)
            .multipart(form2)
            .send()
            .await
            .expect("upload POST 2");
        assert_eq!(resp2.status(), reqwest::StatusCode::OK);
        let json2: serde_json::Value = resp2.json().await.unwrap();
        assert_eq!(json2["files"][0]["path"], "uploads/photo_1.jpg");
        assert!(td.path().join("uploads").join("photo_1.jpg").exists());

        server_handle.abort();
    }
}
