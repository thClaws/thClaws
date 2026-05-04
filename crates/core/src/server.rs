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

use crate::event_render::{
    render_chat_dispatches, render_terminal_ansi, terminal_data_envelope,
    terminal_history_replaced_envelope, TerminalRenderState,
};
use crate::ipc::{handle_ipc, IpcContext, PendingAsks};
use crate::shared_session::{SharedSessionHandle, ViewEvent};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// The same single-file React build the desktop GUI embeds. Re-embedded
/// here under the always-on `crate::server` module so the frontend is
/// bundled regardless of the `gui` feature.
const FRONTEND_HTML: &str = include_str!("../../../frontend/dist/index.html");

#[derive(Clone)]
pub struct ServeConfig {
    pub bind: SocketAddr,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            // Localhost-only by default — Phase 1 trust model is "SSH
            // tunnel handles auth". Override via --bind if you know
            // what you're doing.
            bind: ([127, 0, 0, 1], 8443).into(),
        }
    }
}

/// State shared across HTTP / WS handlers. The `SharedSessionHandle`
/// IS the engine — same Arc lives in every WS connection so multi-tab
/// browsers see the same conversation.
#[derive(Clone)]
struct ServeState {
    shared: Arc<SharedSessionHandle>,
    approver: Arc<crate::permissions::GuiApprover>,
    pending_asks: PendingAsks,
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
    let state = ServeState {
        shared,
        approver,
        pending_asks,
    };

    let app = Router::new()
        .route("/", get(serve_index))
        .route("/healthz", get(serve_health))
        .route("/ws", get(ws_handler))
        .with_state(state);

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

async fn serve_index() -> impl IntoResponse {
    Html(FRONTEND_HTML)
}

async fn serve_health() -> impl IntoResponse {
    "ok"
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
        on_send_initial_state: Arc::new(|| {
            // SERVE3 will replace this stub with the snapshot-frame
            // builder. For now: no-op; frontend sees nothing back when
            // it sends `frontend_ready`. Smoke test scope.
        }),
        on_zoom: Arc::new(|_scale| {
            // Browser handles its own zoom (Cmd-+/-); no server-side
            // hook needed unless we want to persist the scale across
            // sessions. Defer.
        }),
    };

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
    writer.abort();
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

        let cfg = ServeConfig { bind: addr };
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
}
