//! Desktop GUI mode: wry webview serving the embedded React frontend.
//!
//! The React dist/ is embedded at compile time via `include_str!` and
//! served via a wry custom protocol (`thclaws://`). We intentionally
//! avoid `with_html` because WebView2's `NavigateToString` caps payloads
//! at 2 MB on Windows and our inlined bundle is ~3 MB — it would panic
//! at build-time with `HRESULT(0x80070057) "parameter is incorrect"`.
//! A single `SharedSession` (in `crate::shared_session`) owns one Agent
//! and one Session that both the Terminal and Chat tabs render. Both
//! tabs send user input via the `shell_input` IPC; both subscribe to a
//! broadcast event stream that this module fans out to chat-shaped and
//! terminal-shaped frontend dispatches.
//!
//! Only compiled when the `gui` feature is enabled.

#![cfg(feature = "gui")]

use crate::config::AppConfig;
use crate::event_render::{
    render_chat_dispatches, render_terminal_ansi, terminal_data_envelope,
    terminal_history_replaced_envelope, TerminalRenderState,
};
use crate::session::SessionStore;
use crate::shared_session::{SharedSessionHandle, ShellInput, ViewEvent};
use base64::Engine;
use std::borrow::Cow;
use std::sync::Arc;
use tao::dpi::LogicalSize;
#[cfg(target_os = "macos")]
use tao::event::ElementState;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
#[cfg(target_os = "macos")]
use tao::keyboard::{Key, ModifiersState};
use tao::window::WindowBuilder;
use wry::http::Response;
use wry::WebViewBuilder;

// Linux-only wry/tao extensions: WebKit2GTK can't be attached to a raw
// window handle the way AppKit (macOS) and WebView2 (Windows) can —
// it's a GTK widget that has to be packed into a GTK container. Without
// these, `builder.build(&window)` panics at startup with
// `UnsupportedWindowHandle` on every Linux build (reported on Ubuntu
// 22.04). `default_vbox()` (from `WindowExtUnix`) gives us the GTK box
// owned by the tao window, and `build_gtk` (from `WebViewBuilderExtUnix`)
// is the Linux-only constructor that takes a GTK container.
#[cfg(target_os = "linux")]
use tao::platform::unix::WindowExtUnix;
#[cfg(target_os = "linux")]
use wry::WebViewBuilderExtUnix;

// Native cross-platform file/dialog crates — replace the per-platform
// shell-out paths (osascript / zenity / PowerShell) used by
// pick_directory_native and the Windows branch of native_confirm.
// Backported from public repo (commits 0c592ab + 7339bc0) so Windows
// users get a working folder picker + confirm dialog via Win32 instead
// of a brittle PowerShell escape-fest. native_dialog is only consulted
// from the Windows branch of native_confirm; gate its import too so
// macOS/Linux builds don't warn about unused imports.
#[cfg(target_os = "windows")]
use native_dialog::{DialogBuilder, MessageLevel};
use rfd::FileDialog;

/// Embed the single-file React frontend (JS+CSS inlined by vite-plugin-singlefile).
const FRONTEND_HTML: &str = include_str!("../../../frontend/dist/index.html");

enum UserEvent {
    /// Generic frontend dispatch — payload is a complete JSON message
    /// the frontend's `__thclaws_dispatch` will parse and route.
    Dispatch(String),
    SendInitialState,
    SessionLoaded(String),
    SessionListRefresh(String),
    FileTree(String),
    FileContent(String),
    QuitRequested,
    /// Settings → Appearance changed `guiScale`. Carries the new
    /// (clamped) factor so the event loop can apply it via
    /// `webview.zoom()` without re-reading config. Issue #47.
    ZoomChanged(f64),
}

// MAX_RECENT_DIRS moved to crate::recent_dirs.

// ── Event translator ────────────────────────────────────────────────
// Subscribes to the SharedSession's broadcast channel and fans each
// ViewEvent out to two frontend dispatches: a chat-shaped JSON message
// (`chat_text_delta`, `chat_tool_call`, `chat_history_replaced`, …)
// and a terminal-shaped one (`terminal_data` carrying base64 ANSI
// bytes). Both tabs subscribe to their respective shapes and render
// the same conversation.

fn spawn_event_translator(handle: &SharedSessionHandle, proxy: EventLoopProxy<UserEvent>) {
    let mut rx = handle.subscribe();
    std::thread::spawn(move || {
        // tokio runtime so we can `.recv().await` on the broadcast.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("translator runtime");
        rt.block_on(async move {
            let mut term_state = TerminalRenderState::default();
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        // /quit confirmed by the worker — forward to
                        // the tao event loop so the window runs the
                        // same save-and-exit path as the close button
                        // (#52). No chat / terminal rendering needed.
                        if matches!(ev, ViewEvent::QuitRequested) {
                            let _ = proxy.send_event(UserEvent::QuitRequested);
                            continue;
                        }
                        for dispatch in render_chat_dispatches(&ev) {
                            let _ = proxy.send_event(UserEvent::Dispatch(dispatch));
                        }
                        if let Some(ansi) = render_terminal_ansi(&mut term_state, &ev) {
                            // HistoryReplaced needs a distinct envelope
                            // so the frontend always re-renders the
                            // prompt at the end — empty-history loads
                            // (new session / loaded session with no
                            // messages) otherwise leave the terminal
                            // with no `❯ ` and the user has to press a
                            // key before they realize it's responsive.
                            let envelope = if matches!(ev, ViewEvent::HistoryReplaced(_)) {
                                terminal_history_replaced_envelope(&ansi)
                            } else {
                                terminal_data_envelope(&ansi)
                            };
                            let _ = proxy.send_event(UserEvent::Dispatch(envelope));
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Slow consumer dropped events; resync by replaying
                        // a fresh "history replaced" with the agent's view
                        // would need agent access — skip for now and hope
                        // the next live event keeps state in sync.
                        continue;
                    }
                    Err(_) => break,
                }
            }
        });
    });
}

#[cfg(test)]
mod csv_table_tests {
    use super::csv_to_markdown_table;

    #[test]
    fn renders_basic_csv_as_markdown_table() {
        let md = csv_to_markdown_table("name,age\nAlice,30\nBob,25");
        // Header row + separator + 2 data rows.
        assert!(md.contains("| name | age |"));
        assert!(md.contains("| --- | --- |"));
        assert!(md.contains("| Alice | 30 |"));
    }

    #[test]
    fn preserves_thai_cells() {
        let md = csv_to_markdown_table("ชื่อ,อายุ\nสมชาย,25");
        assert!(md.contains("ชื่อ"));
        assert!(md.contains("สมชาย"));
    }

    #[test]
    fn escapes_pipe_characters_in_cells() {
        let md = csv_to_markdown_table("col1,col2\n\"a|b\",c");
        // Pipe inside a cell becomes \| so the row structure stays valid.
        assert!(md.contains("a\\|b"));
    }

    #[test]
    fn empty_input_yields_empty_string() {
        assert_eq!(csv_to_markdown_table(""), "");
    }
}

/// Convert a markdown string to a full standalone HTML document so the
/// Files-tab iframe can render it without any client-side markdown
/// library. GFM extensions are enabled (tables, task lists,
/// strikethrough, autolinks); raw HTML in the source is stripped
/// (`render.unsafe_ = false`) so `<script>` in a `.md` file we're
/// previewing can't escape the iframe sandbox.
/// Convert a CSV string to a GFM markdown pipe-table so the comrak
/// renderer (which has the `table` extension on) emits a proper grid.
/// First row is treated as the header. Pipe characters in cells are
/// escaped (`\|`) so they don't break the row structure. Empty input
/// yields an empty string.
// csv_to_markdown_table + render_markdown_to_html + ospath moved to
// crate::file_preview in M6.36 SERVE9i so the WS transport's file_*
// IPC arms can call them from the always-on dispatch table.
use crate::file_preview::{csv_to_markdown_table, ospath, render_markdown_to_html};
// build_sso_state_payload moved to crate::sso::build_state_payload in
// M6.36 SERVE9h. Re-export the old name so legacy gui.rs callers keep
// compiling — they switch to the new path when their arm migrates.
use crate::sso::build_state_payload as build_sso_state_payload;

/// Show a native OS confirmation dialog. Returns `true` on affirmative.
///
/// Same shell-out pattern as `pick_directory_native`: osascript on macOS,
/// zenity on Linux, PowerShell/MessageBox on Windows — no extra crate
/// dependency. Blocks the calling thread until the user dismisses the
/// dialog, so this MUST be called from the IPC worker thread, never
/// from the tao event loop.
///
/// Windows MessageBox enforces "Yes"/"No" labels; `yes_label`/`no_label`
/// are only honoured on macOS and Linux, with the message text carrying
/// the intent on Windows.
pub(crate) fn native_confirm(title: &str, message: &str, yes_label: &str, no_label: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display dialog \"{}\" with title \"{}\" buttons {{\"{}\", \"{}\"}} default button \"{}\"",
            esc(message),
            esc(title),
            esc(no_label),
            esc(yes_label),
            esc(yes_label),
        );
        match std::process::Command::new("osascript")
            .args(["-e", &script])
            .output()
        {
            Ok(out) if out.status.success() => {
                let s = String::from_utf8_lossy(&out.stdout);
                s.contains(&format!("button returned:{yes_label}"))
            }
            _ => false,
        }
    }
    #[cfg(target_os = "linux")]
    {
        match std::process::Command::new("zenity")
            .args([
                "--question",
                "--title",
                title,
                "--text",
                message,
                "--ok-label",
                yes_label,
                "--cancel-label",
                no_label,
            ])
            .status()
        {
            Ok(s) => s.success(),
            Err(_) => false,
        }
    }
    #[cfg(target_os = "windows")]
    {
        // MessageBox button labels are fixed ("Yes"/"No") by the OS; the
        // message string has to carry the yes/no semantics. Prefix the
        // user's label onto the message so they know which button does
        // what. Backported from public repo (commit 7339bc0): replaces
        // PowerShell shell-out with the `native_dialog` crate, dodging
        // PowerShell's quote-escaping quirks.
        let prompt = format!("{}\n\nYes = {}   No = {}", message, yes_label, no_label,);
        DialogBuilder::message()
            .set_level(MessageLevel::Info)
            .set_title(title)
            .set_text(prompt)
            .confirm()
            .show()
            .unwrap_or(false)
    }
}

/// Open a native OS directory picker dialog. Returns the selected path or
/// `None` if the user cancelled. Backported from public repo (commit
/// 0c592ab): replaces the per-platform shell-out (osascript / zenity /
/// PowerShell `FolderBrowserDialog`) with the `rfd` crate, which calls
/// the same OS APIs natively. Eliminates dependence on `osascript` /
/// `zenity` being installed and PowerShell quote-escaping bugs.
fn pick_directory_native(start_dir: &str) -> Option<String> {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        FileDialog::new()
            .set_title("Select working directory")
            .set_directory(start_dir)
            .pick_folder()
            .map(|p| p.to_string_lossy().into_owned())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

fn build_session_list(store: &Option<SessionStore>) -> String {
    let sessions: Vec<serde_json::Value> = store
        .as_ref()
        .and_then(|s| s.list().ok())
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
    // `current_id` is omitted here — this path (config_poll, session_load)
    // runs on the main thread and doesn't own the worker's current-session
    // state. Frontend keeps the last known `current_id` when the field is
    // absent; the worker's own SessionListRefresh events provide the
    // authoritative highlight.
    serde_json::json!({"type": "sessions_list", "sessions": sessions}).to_string()
}

// provider_has_credentials / kind_has_credentials / auto_fallback_model
// moved to crate::providers in M6.36 SERVE9e so the WS transport can
// share the same readiness logic. Re-import here to keep gui.rs's
// existing call sites compiling unchanged.
use crate::providers::{auto_fallback_model, kind_has_credentials, provider_has_credentials};

/// Resolve the AGENTS.md path for the Settings → Instructions editor.
/// `scope="global"` → `~/.config/thclaws/AGENTS.md`, `scope="folder"` →
/// `./AGENTS.md` in the current working directory.
// instructions_path moved to crate::instructions in M6.36 SERVE9d.
use crate::instructions::instructions_path;

/// Build the `mcp_update` IPC payload: the configured MCP servers for
/// this session (read fresh from disk so removals via `/mcp remove` are
/// reflected immediately, not after a restart). Tool count is reported
/// as 0 for now — the live registry doesn't track which tool came from
/// which MCP server, so we'd have to hold a separate name-to-server
/// map to do better. The sidebar today only renders the name, so 0 is
/// a non-misleading placeholder.
pub(crate) fn build_mcp_update_payload() -> serde_json::Value {
    let config = crate::config::AppConfig::load().unwrap_or_default();
    let servers: Vec<serde_json::Value> = config
        .mcp_servers
        .iter()
        .map(|s| serde_json::json!({"name": s.name, "tools": 0}))
        .collect();
    serde_json::json!({
        "type": "mcp_update",
        "servers": servers,
    })
}

/// Build the `kms_update` IPC payload: every discoverable KMS tagged with
/// whether it's currently attached to this project.
// build_kms_update_payload moved to crate::kms::build_update_payload
// in M6.36 SERVE9c. Re-export the old name as a thin alias so existing
// gui.rs callers (kms_list / kms_toggle / kms_new arms still here, the
// SendInitialState builder) keep compiling unchanged.
pub(crate) fn build_kms_update_payload() -> serde_json::Value {
    crate::kms::build_update_payload()
}

fn escape_for_js(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\0', "\\0")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

#[cfg(target_os = "macos")]
fn is_macos_close_shortcut(event: &tao::event::KeyEvent, modifiers: ModifiersState) -> bool {
    if event.state != ElementState::Pressed || !modifiers.super_key() {
        return false;
    }
    match event.key_without_modifiers() {
        Key::Character(ch) => ch.eq_ignore_ascii_case("q") || ch.eq_ignore_ascii_case("w"),
        _ => false,
    }
}

/// Whitelist external URLs to `http://` / `https://` only. Tool output is
/// untrusted, so this rejects `file://`, `javascript:`, custom schemes,
/// and anything that doesn't parse as a real URL — preventing a hostile
/// MCP server from getting the user to launch arbitrary local handlers
/// just because they clicked a link in chat.
// is_safe_external_url + open_external_url moved to crate::external_url
// in M6.36 SERVE9h.
use crate::external_url::{is_safe_external_url, open_external_url};

/// Assemble the cross-provider model list payload for the sidebar's
/// inline picker dropdown (#49). Catalogue rows for every known
/// provider, plus a live Ollama probe so models added via `ollama pull`
/// after launch are visible without restart. The Ollama probe uses a
/// short timeout — failure just falls back to whatever rows are in the
/// baseline catalogue.
// build_all_models_payload moved to crate::providers in M6.36 SERVE9g
// so the WS transport's request_all_models IPC arm can call it from
// the always-on dispatch table.
use crate::providers::build_all_models_payload;

fn request_gui_shutdown(shared: &SharedSessionHandle, control_flow: &mut ControlFlow) {
    let _ = shared.input_tx.send(ShellInput::SaveAndQuit);
    // Kill any spawned teammate processes.
    let _ = std::process::Command::new("pkill")
        .args(["-f", "team-agent"])
        .status();
    *control_flow = ControlFlow::Exit;
}

pub fn run_gui() {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let (win_w, win_h, initial_zoom) = crate::config::ProjectConfig::load()
        .map(|c| {
            (
                c.window_width.unwrap_or(1200.0),
                c.window_height.unwrap_or(800.0),
                c.gui_scale.unwrap_or(1.0),
            )
        })
        .unwrap_or((1200.0, 800.0, 1.0));
    let window = WindowBuilder::new()
        .with_title(&crate::branding::current().name)
        .with_inner_size(LogicalSize::new(win_w, win_h))
        .build(&event_loop)
        .expect("window build");

    let proxy_for_ipc = proxy.clone();

    // Single shared session backing both Terminal + Chat tabs. The
    // worker owns one Agent + Session + AppConfig and broadcasts every
    // ViewEvent to subscribers; the event translator below fans those
    // out as chat-shaped and terminal-shaped frontend dispatches.
    //
    // GuiApprover bridges the Agent's async `approve()` call to the
    // frontend: requests go out on `approval_rx` → dispatched as
    // `approval_request` JSON; responses come back via the
    // `approval_response` IPC and are pushed into the approver's
    // internal oneshot responders.
    let (approver, mut approval_rx) = crate::permissions::GuiApprover::new();
    let approver_for_ipc = approver.clone();
    let shared = Arc::new(crate::shared_session::spawn_with_approver(approver.clone()));
    spawn_event_translator(&shared, proxy.clone());
    let shared_for_ipc = shared.clone();
    let shared_for_events = shared.clone();
    let (ask_tx, mut ask_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::tools::AskUserRequest>();
    crate::tools::set_gui_ask_sender(Some(ask_tx));
    let pending_asks = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
        u64,
        tokio::sync::oneshot::Sender<String>,
    >::new()));
    let pending_asks_for_ipc = pending_asks.clone();

    // Forwarder: AskUserQuestion tool calls -> frontend composer handoff.
    let proxy_for_ask = proxy.clone();
    let pending_asks_for_forwarder = pending_asks.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("ask-user forwarder runtime");
        rt.block_on(async move {
            while let Some(req) = ask_rx.recv().await {
                let id = req.id;
                let question = req.question.clone();
                if let Ok(mut pending) = pending_asks_for_forwarder.lock() {
                    pending.insert(id, req.response);
                }
                let payload = serde_json::json!({
                    "type": "ask_user_question",
                    "id": id,
                    "question": question,
                });
                let _ = proxy_for_ask.send_event(UserEvent::Dispatch(payload.to_string()));

                // Also render the question as ANSI in the terminal tab
                // so users on the Terminal surface aren't left wondering
                // why a tool stalled silently. Cyan banner + the full
                // question body, then a "↩ switch to Chat tab to reply"
                // hint since we don't have an inline answer affordance
                // in the terminal yet.
                let terminal_block = format!(
                    "\r\n\x1b[36m─── assistant asks ─────────────────────\x1b[0m\r\n\x1b[36m{}\x1b[0m\r\n\x1b[36m─── reply via the Chat tab ─────────────\x1b[0m\r\n",
                    question.replace('\n', "\r\n"),
                );
                let _ = proxy_for_ask.send_event(UserEvent::Dispatch(
                    terminal_data_envelope(&terminal_block),
                ));
            }
        });
    });

    // Forwarder: approval requests → frontend dispatches. Spawned on a
    // dedicated tokio runtime thread so we can `await` the mpsc without
    // blocking the main event loop.
    let proxy_for_approval = proxy.clone();
    let approver_for_redispatch = approver.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("approval forwarder runtime");
        rt.block_on(async move {
            let proxy_inner = proxy_for_approval.clone();
            let approver_inner = approver_for_redispatch.clone();
            // Periodic redispatch: the initial `evaluate_script` can
            // fire before the webview finishes its first React mount,
            // at which point `window.__thclaws_dispatch` is undefined
            // and the call silently drops. Re-sending every second
            // until the user responds (tracked by id on the backend)
            // is a cheap race-proof backstop.
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                    let pending = approver_inner.unresolved_requests();
                    if pending.is_empty() {
                        continue;
                    }
                    for req in pending {
                        let payload = serde_json::json!({
                            "type": "approval_request",
                            "id": req.id,
                            "tool_name": req.tool_name,
                            "input": req.input,
                            "summary": req.summary,
                        });
                        let _ = proxy_inner.send_event(UserEvent::Dispatch(payload.to_string()));
                    }
                }
            });
            while let Some(req) = approval_rx.recv().await {
                let payload = serde_json::json!({
                    "type": "approval_request",
                    "id": req.id,
                    "tool_name": req.tool_name,
                    "input": req.input,
                    "summary": req.summary,
                });
                let _ = proxy_for_approval.send_event(UserEvent::Dispatch(payload.to_string()));
            }
        });
    });

    // Enable devtools when the env opt-in is set — lets users diagnose
    // a blank/black screen (Inspect → Console) without us shipping a
    // different build. Set THCLAWS_DEVTOOLS=1 and relaunch.
    let devtools_on = std::env::var("THCLAWS_DEVTOOLS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    // Windows (WebView2) exposes custom protocols as `http://<scheme>.<host>`;
    // mac/Linux use the raw `<scheme>://<host>` form.
    #[cfg(windows)]
    let start_url = "http://thclaws.localhost/";
    #[cfg(not(windows))]
    let start_url = "thclaws://localhost/";

    let builder = WebViewBuilder::new()
        .with_url(start_url)
        .with_custom_protocol("thclaws".into(), |_webview_id, request| {
            // File-asset route: serves on-disk files so previewed HTML
            // can load its sibling CSS/JS with relative URLs. Example:
            // `thclaws://localhost/file-asset/Users/jimmy/site/index.html`
            // → reads `/Users/jimmy/site/index.html`. Every request is
            // validated through the sandbox before hitting disk.
            let req_path = request.uri().path();
            if let Some(rest) = req_path.strip_prefix("/file-asset/") {
                let decoded = urlencoding::decode(rest)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| rest.to_string());
                let abs = format!("/{decoded}");
                match crate::sandbox::Sandbox::check(&abs) {
                    Ok(resolved) => match std::fs::read(&resolved) {
                        Ok(bytes) => {
                            let ext = resolved.extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("")
                                .to_lowercase();
                            let mime = match ext.as_str() {
                                "html" | "htm" => "text/html; charset=utf-8",
                                "css" => "text/css; charset=utf-8",
                                "js" | "mjs" => "application/javascript; charset=utf-8",
                                "json" => "application/json; charset=utf-8",
                                "svg" => "image/svg+xml",
                                "png" => "image/png",
                                "jpg" | "jpeg" => "image/jpeg",
                                "gif" => "image/gif",
                                "webp" => "image/webp",
                                "ico" => "image/x-icon",
                                "woff" => "font/woff",
                                "woff2" => "font/woff2",
                                "ttf" => "font/ttf",
                                "otf" => "font/otf",
                                _ => "application/octet-stream",
                            };
                            return Response::builder()
                                .header("Content-Type", mime)
                                .body(Cow::Owned(bytes))
                                .expect("build file-asset response");
                        }
                        Err(_) => {
                            return Response::builder()
                                .status(404)
                                .body(Cow::Borrowed(&b"not found"[..]))
                                .expect("build 404");
                        }
                    },
                    Err(_) => {
                        return Response::builder()
                            .status(403)
                            .body(Cow::Borrowed(&b"forbidden"[..]))
                            .expect("build 403");
                    }
                }
            }
            Response::builder()
                .header("Content-Type", "text/html; charset=utf-8")
                .body(Cow::Borrowed(FRONTEND_HTML.as_bytes()))
                .expect("build frontend response")
        })
        .with_devtools(devtools_on)
        .with_ipc_handler(move |req| {
            let body = req.body();
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(body) else {
                return;
            };

            // M6.36 SERVE9: delegate to the transport-agnostic dispatch
            // first. Migrated arms (plan-sidebar, app_close, etc.) are
            // handled there; if `handle_ipc` returns true we're done.
            // Anything not yet migrated returns false and falls through
            // to the wry-only match below.
            //
            // The wry-flavored IpcContext built here mirrors what
            // server.rs::handle_socket builds for WebSocket clients —
            // same dispatch table, transport-specific bridges only
            // differ in their callback bodies.
            {
                let proxy_dispatch = proxy_for_ipc.clone();
                let dispatch: crate::ipc::DispatchFn = Arc::new(move |payload: String| {
                    let _ = proxy_dispatch.send_event(UserEvent::Dispatch(payload));
                });
                let proxy_quit = proxy_for_ipc.clone();
                let on_quit: crate::ipc::QuitFn = Arc::new(move || {
                    let _ = proxy_quit.send_event(UserEvent::QuitRequested);
                });
                let proxy_init = proxy_for_ipc.clone();
                let on_send_initial_state: crate::ipc::SendInitialStateFn = Arc::new(move || {
                    let _ = proxy_init.send_event(UserEvent::SendInitialState);
                });
                // Zoom is wry-specific — the on_zoom callback fires
                // via the existing `gui_set_zoom` arm below (not yet
                // migrated), so this stub closure is never called from
                // the shared dispatch path today.
                let on_zoom: crate::ipc::ZoomFn = Arc::new(|_scale: f64| {});
                let ipc_ctx = crate::ipc::IpcContext {
                    shared: shared_for_ipc.clone(),
                    approver: approver_for_ipc.clone(),
                    pending_asks: pending_asks_for_ipc.clone(),
                    dispatch,
                    on_quit,
                    on_send_initial_state,
                    on_zoom,
                };
                if crate::ipc::handle_ipc(msg.clone(), &ipc_ctx) {
                    return;
                }
            }

            let ty = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");

            match ty {
                // Note: app_close, shell_input, frontend_ready,
                // approval_response, shell_cancel, new_session,
                // plan_approve, plan_cancel, plan_retry_step,
                // plan_skip_step, plan_stalled_continue all migrated to
                // crate::ipc::handle_ipc and handled above. Remaining
                // arms continue to live here pending SERVE9 follow-ups.
                "mcp_call_tool" => {
                    // Widget-initiated tool call from an embedded MCP
                    // App. Forward to the worker; the response comes
                    // back asynchronously as a `mcp_call_tool_result`
                    // dispatch keyed by the same `requestId`. Trust
                    // gating already happened at widget render time —
                    // only trusted servers ship widgets, so any tool
                    // call originating from a rendered widget is
                    // implicitly trusted.
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
                        let _ = shared_for_ipc.input_tx.send(
                            ShellInput::McpAppCallTool {
                                request_id,
                                qualified_name,
                                arguments,
                            },
                        );
                    }
                }
                "open_external" => {
                    // Open a URL in the OS default browser. The URL is
                    // model-attributable (it can come from MCP tool
                    // output rendered in chat), so we accept only
                    // http(s). Anything else — `file://`, `javascript:`,
                    // shell metacharacters — is dropped silently.
                    if let Some(url) = msg.get("url").and_then(|v| v.as_str()) {
                        if is_safe_external_url(url) {
                            open_external_url(url);
                        } else {
                            eprintln!(
                                "\x1b[33m[ipc open_external] refusing non-http(s) url\x1b[0m"
                            );
                        }
                    }
                }
                "model_set" => {
                    // Frontend-driven model change (e.g. ModelPickerModal
                    // pick after api_key_set, or any future picker UI).
                    // Routes through the same persistence path as the
                    // /model slash command: project config write +
                    // ReloadConfig nudge to the worker + provider_update
                    // broadcast so the sidebar reflects the new state.
                    let model = msg
                        .get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if !model.is_empty() {
                        let mut project = crate::config::ProjectConfig::load()
                            .unwrap_or_default();
                        project.set_model(&model);
                        let _ = project.save();
                        let new_cfg = AppConfig::load().unwrap_or_default();
                        let provider_name =
                            new_cfg.detect_provider().unwrap_or("unknown");
                        let ready = provider_has_credentials(&new_cfg);
                        let broadcast = serde_json::json!({
                            "type": "provider_update",
                            "provider": provider_name,
                            "model": new_cfg.model,
                            "provider_ready": ready,
                        });
                        let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                            broadcast.to_string(),
                        ));
                        let _ = shared_for_ipc.input_tx.send(ShellInput::ReloadConfig);
                    }
                }
                "gui_scale_get" => {
                    // Settings menu asking for the persisted zoom on
                    // mount so the dropdown shows the right preset.
                    let scale = crate::config::ProjectConfig::load()
                        .and_then(|c| c.gui_scale)
                        .unwrap_or(1.0);
                    let payload = serde_json::json!({
                        "type": "gui_scale_value",
                        "scale": scale,
                    });
                    let _ = proxy_for_ipc
                        .send_event(UserEvent::SessionLoaded(payload.to_string()));
                }
                "gui_set_zoom" => {
                    // Settings panel slider / hotkey reset asking us to
                    // change the GUI zoom factor. Persist to project
                    // config and apply live; webview.zoom() is on the
                    // event-loop side, so emit a UserEvent the loop
                    // picks up below. Issue #47.
                    let scale = msg.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0);
                    let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
                    project.set_gui_scale(scale);
                    let _ = project.save();
                    let clamped = project.gui_scale.unwrap_or(scale);
                    let _ = proxy_for_ipc.send_event(UserEvent::ZoomChanged(clamped));
                }
                "request_all_models" => {
                    // Sidebar's inline model picker dropdown asking for
                    // the cross-provider model list. Catalogue rows for
                    // every known provider plus a live Ollama probe so
                    // local models the user just `ollama pull`-ed show
                    // up without restart. Issue #49.
                    let proxy = proxy_for_ipc.clone();
                    tokio::spawn(async move {
                        let payload = build_all_models_payload().await;
                        let _ = proxy.send_event(UserEvent::SessionLoaded(payload));
                    });
                }
                "ask_user_response" => {
                    let id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                    let text = msg
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let responder = pending_asks_for_ipc
                        .lock()
                        .ok()
                        .and_then(|mut pending| pending.remove(&id));
                    if let Some(responder) = responder {
                        let _ = responder.send(text);
                    }
                }
                "slash_commands_list" => {
                    // Build the autocomplete catalogue for the chat-tab
                    // `/` popup. Three sources:
                    //   1. built-in commands (hard-coded in repl.rs so
                    //      the parser and the popup stay in lock-step),
                    //   2. user commands from .claude/commands/ etc.,
                    //   3. installed skills (also reachable as /<name>).
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
                    // Include plugin-contributed prompt commands so the
                    // popup matches what `/<name>` resolution will accept
                    // (CommandStore::discover_with_extra is what the worker
                    // path uses too — see shared_session.rs slash handler).
                    let user_cmds = crate::commands::CommandStore::discover_with_extra(
                        &crate::plugins::plugin_command_dirs(),
                    );
                    let mut user_names: Vec<&str> = user_cmds.commands.keys()
                        .map(String::as_str)
                        .collect();
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
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string(),
                    ));
                }
                // ─── EE Phase 4: org-policy SSO (IPC surface for sidebar) ─
                "sso_status" => {
                    let payload = build_sso_state_payload();
                    let _ = proxy_for_ipc
                        .send_event(UserEvent::SessionLoaded(payload.to_string()));
                }
                "sso_login" => {
                    let proxy = proxy_for_ipc.clone();
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
                                let _ = proxy.send_event(UserEvent::SessionLoaded(
                                    payload.to_string(),
                                ));
                                return;
                            }
                        };
                        match crate::sso::login(&policy).await {
                            Ok(_) => {
                                let payload = build_sso_state_payload();
                                let _ = proxy.send_event(UserEvent::SessionLoaded(
                                    payload.to_string(),
                                ));
                            }
                            Err(e) => {
                                let payload = serde_json::json!({
                                    "type": "sso_state",
                                    "enabled": true,
                                    "logged_in": false,
                                    "issuer": policy.issuer_url,
                                    "error": format!("login failed: {e}"),
                                });
                                let _ = proxy.send_event(UserEvent::SessionLoaded(
                                    payload.to_string(),
                                ));
                            }
                        }
                    });
                }
                "sso_logout" => {
                    if let Some(p) = crate::policy::active()
                        .and_then(|a| a.policy.policies.sso.as_ref())
                    {
                        let _ = crate::sso::logout(p);
                    }
                    let payload = build_sso_state_payload();
                    let _ = proxy_for_ipc
                        .send_event(UserEvent::SessionLoaded(payload.to_string()));
                }
                // get_cwd migrated to crate::ipc::handle_ipc.
                "pick_directory" => {
                    let start_dir = msg.get("start").and_then(|v| v.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| std::env::current_dir()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|_| ".".into()));
                    let result = pick_directory_native(&start_dir);
                    let payload = match result {
                        Some(path) => serde_json::json!({
                            "type": "directory_picked",
                            "path": path,
                        }),
                        None => serde_json::json!({
                            "type": "directory_picked",
                            "path": null,
                        }),
                    };
                    let _ = proxy_for_ipc.send_event(
                        UserEvent::SessionLoaded(payload.to_string()),
                    );
                }
                "confirm" => {
                    // Native OS confirmation dialog. Frontend sends an
                    // `id` so it can match the async reply; we echo it
                    // back in the result event. Default labels are
                    // "OK"/"Cancel" if the caller doesn't override.
                    let id = msg
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let title = msg.get("title").and_then(|v| v.as_str()).unwrap_or("Confirm");
                    let message = msg.get("message").and_then(|v| v.as_str()).unwrap_or("");
                    let yes_label = msg
                        .get("yes_label")
                        .and_then(|v| v.as_str())
                        .unwrap_or("OK");
                    let no_label = msg
                        .get("no_label")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Cancel");
                    let ok = native_confirm(title, message, yes_label, no_label);
                    let payload = serde_json::json!({
                        "type": "confirm_result",
                        "id": id,
                        "ok": ok,
                    });
                    let _ = proxy_for_ipc
                        .send_event(UserEvent::FileContent(payload.to_string()));
                }
                // set_cwd migrated to crate::ipc::handle_ipc.
                "shell_input" | "chat_prompt" | "pty_write" => {
                    // Unified entry point: a line of user input from
                    // either tab. `chat_prompt` and `pty_write` are
                    // legacy aliases kept so the frontend can roll over
                    // without a flag-day. `pty_write` historically sent
                    // a base64 chunk per keystroke — for backward compat
                    // with any in-flight callers we accept both
                    // `text` (new) and `data` (base64 of the line).
                    let line = if let Some(t) = msg.get("text").and_then(|v| v.as_str()) {
                        t.to_string()
                    } else if let Some(b64) = msg.get("data").and_then(|v| v.as_str()) {
                        base64::engine::general_purpose::STANDARD
                            .decode(b64)
                            .ok()
                            .and_then(|b| String::from_utf8(b).ok())
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    let trimmed = line.trim_end_matches(['\r', '\n']);

                    // Optional image attachments shipped alongside the
                    // text (Phase 4 paste/drag-drop). Frontend sends
                    // `attachments: [{mediaType, data}, ...]` where
                    // data is the base64 of the raw image bytes (no
                    // data: prefix). Only the chat tab emits this
                    // field; the terminal tab never has attachments.
                    //
                    // Caps below are defense-in-depth against a
                    // malicious / buggy frontend bypassing the
                    // ChatView per-image 10 MB cap. With both caps,
                    // the worst-case payload is bounded at ~67 MB
                    // base64 (50 MB raw) per IPC message, which the
                    // agent can ingest without OOM on common dev
                    // hardware.
                    const MAX_ATTACHMENTS_PER_MESSAGE: usize = 10;
                    const MAX_ATTACHMENTS_TOTAL_B64_BYTES: usize = 67 * 1024 * 1024;

                    let mut attachments: Vec<(String, String)> = msg
                        .get("attachments")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|a| {
                                    let media_type = a
                                        .get("mediaType")
                                        .and_then(|v| v.as_str())?
                                        .to_string();
                                    let data =
                                        a.get("data").and_then(|v| v.as_str())?.to_string();
                                    if data.is_empty() {
                                        None
                                    } else {
                                        Some((media_type, data))
                                    }
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                    if attachments.len() > MAX_ATTACHMENTS_PER_MESSAGE {
                        eprintln!(
                            "[ipc chat_user_message] dropping {} attachments over the {}-per-message cap",
                            attachments.len() - MAX_ATTACHMENTS_PER_MESSAGE,
                            MAX_ATTACHMENTS_PER_MESSAGE,
                        );
                        attachments.truncate(MAX_ATTACHMENTS_PER_MESSAGE);
                    }
                    let total_b64: usize =
                        attachments.iter().map(|(_, d)| d.len()).sum();
                    if total_b64 > MAX_ATTACHMENTS_TOTAL_B64_BYTES {
                        eprintln!(
                            "[ipc chat_user_message] attachments total {} bytes (b64) exceed {} cap; dropping all",
                            total_b64, MAX_ATTACHMENTS_TOTAL_B64_BYTES,
                        );
                        attachments.clear();
                    }

                    if !attachments.is_empty() {
                        let _ = shared_for_ipc.input_tx.send(ShellInput::LineWithImages {
                            text: trimmed.to_string(),
                            images: attachments,
                        });
                    } else if !trimmed.is_empty() {
                        let _ = shared_for_ipc
                            .input_tx
                            .send(ShellInput::Line(trimmed.to_string()));
                    }
                }
                "pty_spawn" => {
                    // Legacy ack: the frontend sends this on Terminal-tab
                    // mount to trigger initial sidebar state. The shared
                    // session is already running by this point.
                    let _ = proxy_for_ipc.send_event(UserEvent::SendInitialState);
                }
                // shell_cancel + frontend_ready + approval_response
                // migrated to crate::ipc::handle_ipc and handled at the
                // top of this closure via the delegate.
                "team_enabled_get" => {
                    let enabled = crate::config::ProjectConfig::load()
                        .and_then(|c| c.team_enabled)
                        .unwrap_or(false);
                    let payload = serde_json::json!({
                        "type": "team_enabled",
                        "enabled": enabled,
                    });
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string(),
                    ));
                }
                "team_enabled_set" => {
                    // Flip the project-scoped teamEnabled flag. Team tools
                    // are registered at SharedSession spawn, so this takes
                    // effect after a restart; the frontend shows a hint.
                    let enabled = msg.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
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
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string(),
                    ));
                }
                "pty_kill" | "pty_resize" | "restart" => {
                    // PTY-era hooks. The shared in-process session has
                    // no PTY to kill or resize; ignore quietly so the
                    // frontend can keep emitting them during transition.
                }
                // new_session migrated to crate::ipc::handle_ipc.
                "config_poll" => {
                    // Re-read config so sidebar picks up model/provider changes.
                    let cfg = AppConfig::load().unwrap_or_default();
                    let provider = cfg.detect_provider().unwrap_or("unknown");
                    let has_key = provider_has_credentials(&cfg);
                    let payload = serde_json::json!({
                        "type": "provider_update",
                        "provider": provider,
                        "model": cfg.model,
                        "provider_ready": has_key,
                    });
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                    // Also refresh the session list so renames made via the
                    // Terminal tab's `/rename` (which writes directly to disk,
                    // bypassing the in-process chat agent) show up in the
                    // sidebar without requiring a chat turn.
                    let store = SessionStore::default_path().map(SessionStore::new);
                    let list = build_session_list(&store);
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionListRefresh(list));
                }
                "endpoint_status" => {
                    let statuses: Vec<serde_json::Value> = crate::endpoints::status()
                        .into_iter()
                        .map(|e| serde_json::json!({
                            "provider": e.provider,
                            "env_var": e.env_var,
                            "configured_url": e.configured_url,
                            "default_url": e.default_url,
                        }))
                        .collect();
                    let payload = serde_json::json!({
                        "type": "endpoint_status",
                        "endpoints": statuses,
                    });
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
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
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
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
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                }
                // instructions_get + instructions_save migrated to crate::ipc::handle_ipc.
                // kms_list migrated to crate::ipc::handle_ipc.
                "kms_toggle" => {
                    let name = msg.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
                    let active = msg.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
                    let (ok, error) = if name.is_empty() {
                        (false, "name required".to_string())
                    } else {
                        let mut current: Vec<String> =
                            crate::config::ProjectConfig::load()
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
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                    // Follow up with a fresh list so the UI reflects persisted state.
                    let list_payload = build_kms_update_payload();
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        list_payload.to_string()
                    ));
                }
                "kms_new" => {
                    let name = msg.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
                    let scope_str =
                        msg.get("scope").and_then(|v| v.as_str()).unwrap_or("user");
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
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                    let list_payload = build_kms_update_payload();
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        list_payload.to_string()
                    ));
                }
                "clipboard_read" => {
                    // Return both `text` (for short payloads / back-compat)
                    // and `text_b64` — the frontend prefers the base64 path
                    // so the JS-bridge escape function doesn't have to
                    // survive U+2028 / U+2029 line separators or the size
                    // quirks of `evaluate_script`'s single-quoted string.
                    let (ok, text) = match arboard::Clipboard::new()
                        .and_then(|mut c| c.get_text())
                    {
                        Ok(t) => (true, t),
                        Err(_) => (false, String::new()),
                    };
                    let text_b64 = base64::engine::general_purpose::STANDARD
                        .encode(text.as_bytes());
                    let payload = serde_json::json!({
                        "type": "clipboard_text",
                        "ok": ok,
                        "text": text,
                        "text_b64": text_b64,
                    });
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                }
                "clipboard_write" => {
                    let text = msg.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let _ = arboard::Clipboard::new()
                        .and_then(|mut c| c.set_text(text.to_string()));
                }
                // theme_get + theme_set migrated to crate::ipc::handle_ipc.
                "secrets_backend_get" => {
                    let backend = crate::secrets::get_backend()
                        .map(|b| b.as_str().to_string());
                    let payload = serde_json::json!({
                        "type": "secrets_backend",
                        "backend": backend,
                    });
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
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
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                }
                "api_key_status" => {
                    let statuses: Vec<serde_json::Value> = crate::secrets::status()
                        .into_iter()
                        .map(|s| serde_json::json!({
                            "provider": s.provider,
                            "env_var": s.env_var,
                            "configured_in_keychain": s.configured_in_keychain,
                            "env_set": matches!(s.env_source, crate::secrets::KeySource::Environment),
                            "key_length": s.key_length,
                        }))
                        .collect();
                    let payload = serde_json::json!({
                        "type": "api_key_status",
                        "keys": statuses,
                    });
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                }
                "api_key_set" => {
                    let provider = msg.get("provider").and_then(|v| v.as_str()).unwrap_or("");
                    let key = msg.get("key").and_then(|v| v.as_str()).unwrap_or("").trim();
                    // Route strictly by the user's stored backend choice.
                    // Keychain is tried only when the user opted into it;
                    // dotenv users never trigger an OS keychain prompt.
                    let (ok, error, storage) = if provider.is_empty() || key.is_empty() {
                        (false, "provider and key are required".to_string(), "")
                    } else {
                        let env_var = crate::providers::ProviderKind::from_name(provider)
                            .and_then(|k| k.api_key_env());
                        let backend = crate::secrets::get_backend()
                            .unwrap_or(crate::secrets::Backend::Keychain);
                        match backend {
                            crate::secrets::Backend::Keychain => {
                                match crate::secrets::set(provider, key) {
                                    Ok(()) => {
                                        if let Some(var) = env_var {
                                            std::env::set_var(var, key);
                                        }
                                        (true, String::new(), "keychain")
                                    }
                                    Err(e) => (false, format!("keychain failed: {e}"), ""),
                                }
                            }
                            crate::secrets::Backend::Dotenv => match env_var {
                                Some(var) => match crate::dotenv::upsert_user_env(var, key) {
                                    Ok(_) => {
                                        std::env::set_var(var, key);
                                        (true, String::new(), "dotenv")
                                    }
                                    Err(e) => (false, format!(".env write failed: {e}"), ""),
                                },
                                None => (
                                    false,
                                    format!("provider '{provider}' has no env var"),
                                    "",
                                ),
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
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                    // If the save succeeded and the currently-configured
                    // provider still has no key, auto-switch to whichever
                    // provider just became usable (likely the one we just
                    // set). Persist the new model and broadcast so the
                    // sidebar flips from "no API key" to ready without a
                    // restart.
                    if ok {
                        let cfg = AppConfig::load().unwrap_or_default();
                        if let Some(new_model) = auto_fallback_model(&cfg) {
                            let mut project = crate::config::ProjectConfig::load()
                                .unwrap_or_default();
                            project.set_model(&new_model);
                            let _ = project.save();
                            let new_cfg = AppConfig::load().unwrap_or_default();
                            let provider_name = new_cfg.detect_provider().unwrap_or("unknown");
                            let ready = provider_has_credentials(&new_cfg);
                            let broadcast = serde_json::json!({
                                "type": "provider_update",
                                "provider": provider_name,
                                "model": new_cfg.model,
                                "provider_ready": ready,
                            });
                            let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                                broadcast.to_string()
                            ));
                            // Post-key-entry model picker (closes #13).
                            // For providers with a non-trivial catalogue
                            // (OpenRouter has dozens; OpenAI/Anthropic/Gemini
                            // each have several variants), open a modal so
                            // the user picks a default rather than landing
                            // on whatever auto_fallback_model chose. Skipped
                            // for tiny catalogues (single model = no choice
                            // to make) and for runtime-loaded backends
                            // (Ollama / LMStudio — their model list comes
                            // from the running runtime, not the catalogue).
                            let cat = crate::model_catalogue::EffectiveCatalogue::load();
                            let models = cat.list_models_for_provider(provider);
                            let runtime_loaded = matches!(
                                provider,
                                "ollama" | "ollama-anthropic" | "lmstudio",
                            );
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
                                let _ = proxy_for_ipc.send_event(
                                    UserEvent::SessionLoaded(picker.to_string()),
                                );
                            }
                        } else {
                            // No auto-switch needed, but readiness may
                            // have flipped for the current provider —
                            // re-broadcast so the sidebar updates.
                            let provider_name = cfg.detect_provider().unwrap_or("unknown");
                            let ready = provider_has_credentials(&cfg);
                            let broadcast = serde_json::json!({
                                "type": "provider_update",
                                "provider": provider_name,
                                "model": cfg.model,
                                "provider_ready": ready,
                            });
                            let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                                broadcast.to_string()
                            ));
                        }
                        // Tell the running worker to reload its config
                        // and rebuild its provider. Without this, the
                        // sidebar reflects the new key but the agent
                        // keeps streaming through the stale (or noop)
                        // provider it was constructed with at startup.
                        let _ = shared_for_ipc.input_tx.send(ShellInput::ReloadConfig);
                    }
                }
                "api_key_clear" => {
                    let provider = msg.get("provider").and_then(|v| v.as_str()).unwrap_or("");
                    // Clear from every storage: keychain (if present),
                    // user-scope .env (if present), and the running
                    // process env.
                    let keychain = crate::secrets::clear(provider);
                    let env_var = crate::providers::ProviderKind::from_name(provider)
                        .and_then(|k| k.api_key_env());
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
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                    // Mirror api_key_set: tell the worker to re-pick a
                    // provider. If the cleared key was the active one,
                    // the rebuild will land on a fallback (or the
                    // NoopProvider) consistent with what the sidebar
                    // shows after readiness flips.
                    let _ = shared_for_ipc.input_tx.send(ShellInput::ReloadConfig);
                }
                "team_send_message" => {
                    // Send a message from the user to a teammate's inbox.
                    if let (Some(to), Some(text)) = (
                        msg.get("to").and_then(|v| v.as_str()),
                        msg.get("text").and_then(|v| v.as_str()),
                    ) {
                        // M6.34 TEAM1: reject path-traversal recipient
                        // names from the frontend before they reach
                        // write_to_mailbox. Mailbox validates too, but
                        // catching here surfaces the error at the IPC
                        // boundary instead of swallowing it.
                        if !crate::team::is_valid_agent_name(to) {
                            // Silently drop — the frontend's only legal
                            // sources are the team-tab agent list (validated)
                            // and the lead-typed user message picker.
                            // Logging via stderr would surface to the
                            // terminal pane and isn't worth a structured
                            // error event for an internal path.
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
                    // Find the team dir — could be in cwd or a subdirectory
                    // (user may have cd'd inside the PTY).
                    let team_dir = {
                        let cwd = std::env::current_dir().unwrap_or_default();
                        let default = crate::team::Mailbox::default_dir();
                        let candidate = cwd.join(&default);
                        if candidate.join("config.json").exists() {
                            candidate
                        } else {
                            // Search one level of subdirectories.
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
                            let status = a.status.clone();

                            // Read the last N lines of the output log.
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
                                "status": status,
                                "task": a.current_task,
                                "output": output,
                            })
                        })
                        .collect();
                    // The Team tab auto-shows whenever a team config
                    // exists on disk — the agent's TeamCreate tool just
                    // writes that config, so the tab needs to follow
                    // suit without any settings.json edit. The
                    // `teamEnabled` flag still gates whether the team
                    // *tools* are registered (so the agent can or can't
                    // spawn teams), but once a team exists, hiding the
                    // UI for it is hostile — the user can dismiss the
                    // tab by deleting `.thclaws/team/`.
                    let has_team = team_dir.join("config.json").exists();
                    let payload = serde_json::json!({
                        "type": "team_status",
                        "has_team": has_team,
                        "agents": agents,
                    });
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                }
                "file_list" => {
                    // ospath() converts JSON-source `/` paths to `\` on
                    // Windows so Sandbox::check accepts them.
                    let raw_path = ospath(msg.get("path").and_then(|v| v.as_str()).unwrap_or("."));
                    let resolved = crate::sandbox::Sandbox::check(&raw_path)
                        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
                    if let Ok(entries) = std::fs::read_dir(&resolved) {
                        let mut items: Vec<serde_json::Value> = entries
                            .flatten()
                            .filter_map(|e| {
                                let name = e.file_name().to_string_lossy().into_owned();
                                // Skip hidden files
                                if name.starts_with('.') { return None; }
                                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                                Some(serde_json::json!({"name": name, "is_dir": is_dir}))
                            })
                            .collect();
                        items.sort_by(|a, b| {
                            let a_dir = a["is_dir"].as_bool().unwrap_or(false);
                            let b_dir = b["is_dir"].as_bool().unwrap_or(false);
                            b_dir.cmp(&a_dir).then_with(|| {
                                a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))
                            })
                        });
                        let payload = serde_json::json!({
                            "type": "file_tree",
                            "path": resolved.to_string_lossy(),
                            "entries": items,
                        });
                        let _ = proxy_for_ipc.send_event(UserEvent::FileTree(payload.to_string()));
                    }
                }
                "file_read" => {
                    // ospath() converts JSON-source `/` paths to `\` on
                    // Windows so Sandbox::check accepts them.
                    let raw_path = ospath(msg.get("path").and_then(|v| v.as_str()).unwrap_or(""));
                    // `mode` is optional. "preview" (default) renders .md
                    // to themed HTML; "source" returns the raw text so the
                    // frontend can hand it to a CodeMirror / TipTap editor.
                    let mode = msg.get("mode").and_then(|v| v.as_str()).unwrap_or("preview");
                    let source_mode = mode == "source";
                    // Resolved theme ("light" | "dark") for the iframe
                    // shell. The frontend maps "system" to the concrete
                    // value before sending so we don't need OS detection
                    // here. Default = dark for backwards compat when the
                    // caller omits the field.
                    let theme = msg.get("theme").and_then(|v| v.as_str()).unwrap_or("dark");
                    let theme = if theme == "light" { "light" } else { "dark" };
                    match crate::sandbox::Sandbox::check(&raw_path) {
                        Ok(path) => {
                            let ext = path.extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("")
                                .to_lowercase();
                            let is_image = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "bmp");
                            let is_pdf = ext == "pdf";
                            let is_markdown = ext == "md" || ext == "markdown";
                            // Office formats: extracted to text via the
                            // Tier 2 read tools, then handed to the same
                            // markdown→HTML pipeline as `.md` files. In
                            // source mode we skip extraction (no useful
                            // editor surface for binary OOXML).
                            let is_docx = ext == "docx";
                            let is_xlsx = ext == "xlsx" || ext == "xlsm" || ext == "xlsb" || ext == "xls" || ext == "ods";
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
                                // In source mode, give `.md` its real mime
                                // so the frontend sends it to the markdown
                                // editor; preview mode renders to HTML.
                                "md" | "markdown" => {
                                    if source_mode { "text/markdown" } else { "text/html" }
                                }
                                "html" | "htm" => "text/html",
                                "docx" | "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" | "pptx" => {
                                    "text/html"
                                }
                                _ => "text/plain",
                            };
                            if is_image || is_pdf {
                                if let Ok(bytes) = std::fs::read(&path) {
                                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                    let payload = serde_json::json!({
                                        "type": "file_content",
                                        "path": raw_path,
                                        "content": b64,
                                        "mime": mime,
                                        "mode": mode,
                                    });
                                    let _ = proxy_for_ipc.send_event(UserEvent::FileContent(payload.to_string()));
                                }
                            } else if is_office {
                                // Extract text via the Tier 2 read tools,
                                // wrap as markdown (with a header showing
                                // the format + path so the user sees they
                                // got an extracted preview, not the raw
                                // bytes), render to themed HTML.
                                let extracted = if is_docx {
                                    crate::tools::docx_read::extract_docx(&path)
                                } else if is_xlsx {
                                    crate::tools::xlsx_read::extract_xlsx(&path, None, "csv")
                                        .map(|csv| csv_to_markdown_table(&csv))
                                } else {
                                    crate::tools::pptx_read::extract_pptx(&path)
                                };
                                let (md, ok) = match extracted {
                                    Ok(text) => {
                                        let header = format!(
                                            "_Extracted preview · {}_\n\n",
                                            ext.to_uppercase()
                                        );
                                        (format!("{header}{text}"), true)
                                    }
                                    Err(e) => (
                                        format!(
                                            "**Failed to extract preview:** {e}\n\nRaw bytes \
                                             aren't shown for binary OOXML formats."
                                        ),
                                        false,
                                    ),
                                };
                                let html = render_markdown_to_html(&md, theme);
                                let payload = serde_json::json!({
                                    "type": "file_content",
                                    "path": raw_path,
                                    "content": html,
                                    "mime": mime,
                                    "mode": mode,
                                    "ok": ok,
                                });
                                let _ = proxy_for_ipc.send_event(UserEvent::FileContent(payload.to_string()));
                            } else {
                                match std::fs::read_to_string(&path) {
                                    Ok(text) => {
                                        let content = if is_markdown && !source_mode {
                                            render_markdown_to_html(&text, theme)
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
                                        let _ = proxy_for_ipc.send_event(UserEvent::FileContent(payload.to_string()));
                                    }
                                    Err(e) => {
                                        let payload = serde_json::json!({
                                            "type": "file_content",
                                            "path": raw_path,
                                            "content": format!("Error reading file: {e}"),
                                            "mime": "text/plain",
                                            "mode": mode,
                                        });
                                        let _ = proxy_for_ipc.send_event(UserEvent::FileContent(payload.to_string()));
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
                            let _ = proxy_for_ipc.send_event(UserEvent::FileContent(payload.to_string()));
                        }
                    }
                }
                "file_write" => {
                    // User-initiated write from the Files-tab editor. The
                    // sandbox gate keeps us inside the working directory;
                    // nothing here bypasses approvals that apply to the
                    // agent — the agent's Write/Edit tools still go
                    // through the permission prompt.
                    let raw_path = msg.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    let (ok, error): (bool, Option<String>) =
                        match crate::sandbox::Sandbox::check(raw_path) {
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
                    let _ = proxy_for_ipc.send_event(UserEvent::FileContent(payload.to_string()));
                }
                "session_rename" => {
                    let id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let title = msg.get("title").and_then(|v| v.as_str()).unwrap_or("");
                    let (ok, error) = if id.is_empty() {
                        (false, "id required".to_string())
                    } else {
                        match SessionStore::default_path().map(SessionStore::new) {
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
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(payload.to_string()));
                    if ok {
                        // M6.19 BUG M2: notify the worker so its
                        // in-memory state.session.title stays in sync
                        // when the renamed session is the active one.
                        let _ = shared_for_ipc.input_tx.send(
                            ShellInput::SessionRenamedExternal {
                                id: id.to_string(),
                                title: title.to_string(),
                            },
                        );
                        // Broadcast the refreshed list so the sidebar picks up the new title.
                        let store = SessionStore::default_path().map(SessionStore::new);
                        let list = build_session_list(&store);
                        let _ = proxy_for_ipc.send_event(UserEvent::SessionListRefresh(list));
                    }
                }
                "session_delete" => {
                    let id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let (ok, error) = if id.is_empty() {
                        (false, "id required".to_string())
                    } else {
                        match SessionStore::default_path().map(SessionStore::new) {
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
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(payload.to_string()));
                    if ok {
                        // M6.19 BUG M2: notify the worker so it can
                        // mint a fresh session if the deleted id was
                        // the active one. Without this, the next save
                        // would re-create the file from cached state
                        // and the "deleted" session would resurrect.
                        let _ = shared_for_ipc.input_tx.send(
                            ShellInput::SessionDeletedExternal {
                                id: id.to_string(),
                            },
                        );
                        let store = SessionStore::default_path().map(SessionStore::new);
                        let list = build_session_list(&store);
                        let _ = proxy_for_ipc.send_event(UserEvent::SessionListRefresh(list));
                    }
                }
                "session_load" => {
                    if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                        // Single source of truth: ask the shared session
                        // to load. It rebuilds agent history + emits a
                        // HistoryReplaced ViewEvent which the translator
                        // fans out to both Terminal (clear scrollback +
                        // ANSI replay) and Chat (clear bubbles + render
                        // each message as its role-coloured bubble).
                        let _ = shared_for_ipc
                            .input_tx
                            .send(ShellInput::LoadSession(id.to_string()));
                    }
                }
                _ => {}
            }
        })
        .with_navigation_handler(|url: String| {
            // Allow any http(s) target. wry's macOS navigation delegate
            // fires for iframe `src` loads as well as top-level
            // navigations — and the closure signature hides which —
            // so blocking http(s) here would also block the lightbox
            // iframe used to render MCP preview viewer pages
            // (e.g. `https://pinn.ai/mcp/preview/<uuid>`).
            //
            // Top-level navigation away from the chat is prevented at
            // the React layer (ChatView.handleChatLinkClick calls
            // preventDefault on every link click and routes to the
            // in-app lightbox). The only role left for this handler
            // is rejecting clearly-out-of-scope schemes — `file://`,
            // `javascript:`, custom protocols — so a hostile MCP
            // server can't smuggle one in via injected HTML.
            url.starts_with("thclaws://")
                || url.starts_with("http://")
                || url.starts_with("https://")
                || url.starts_with("about:")
                || url.starts_with("data:")
                || url.starts_with("blob:")
        });
    // wry exposes a different constructor on Linux because WebKit2GTK
    // mounts as a GTK widget rather than over a raw window handle.
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window).expect("webview build");
    #[cfg(target_os = "linux")]
    let webview = builder
        .build_gtk(window.default_vbox().unwrap())
        .expect("webview build (gtk)");

    // Apply persisted GUI zoom so HiDPI / 4K users get the scale they
    // last picked instead of the WebView's native 1.0 every launch.
    // Skips the call when `guiScale` is exactly 1.0 — saves the
    // round-trip on the common case. Issue #47.
    if (initial_zoom - 1.0).abs() > f64::EPSILON {
        let _ = webview.zoom(initial_zoom);
    }

    #[cfg(target_os = "macos")]
    let mut macos_modifiers = ModifiersState::empty();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::UserEvent(UserEvent::Dispatch(json)) => {
                let escaped = escape_for_js(&json);
                let _ = webview.evaluate_script(&format!(
                    "window.__thclaws_dispatch('{escaped}')"
                ));
            }
            Event::UserEvent(UserEvent::SessionListRefresh(json))
            | Event::UserEvent(UserEvent::FileTree(json))
            | Event::UserEvent(UserEvent::FileContent(json))
            | Event::UserEvent(UserEvent::SessionLoaded(json)) => {
                let escaped = escape_for_js(&json);
                let _ = webview.evaluate_script(&format!(
                    "window.__thclaws_dispatch('{escaped}')"
                ));
            }
            Event::UserEvent(UserEvent::SendInitialState) => {
                let mut config = AppConfig::load().unwrap_or_default();
                // If the saved model's provider has no key but another
                // provider does, auto-switch and persist. Keeps the
                // sidebar's "ready" indicator honest across restarts —
                // after the user sets (say) an Agentic Press key, the
                // next launch lands on ap/* instead of showing a stuck
                // "no API key" on the OpenAI default.
                if let Some(new_model) = auto_fallback_model(&config) {
                    let mut project = crate::config::ProjectConfig::load()
                        .unwrap_or_default();
                    project.set_model(&new_model);
                    let _ = project.save();
                    config = AppConfig::load().unwrap_or_default();
                }
                let provider_name = config.detect_provider().unwrap_or("unknown");
                let provider_ready = provider_has_credentials(&config);
                let mcp_servers: Vec<serde_json::Value> = config
                    .mcp_servers
                    .iter()
                    .map(|s| serde_json::json!({"name": s.name, "tools": 0}))
                    .collect();
                let sessions: Vec<serde_json::Value> = SessionStore::default_path()
                    .map(SessionStore::new)
                    .and_then(|store| store.list().ok())
                    .unwrap_or_default()
                    .into_iter()
                    .take(20)
                    .map(|s| serde_json::json!({
                        "id": s.id,
                        "model": s.model,
                        "messages": s.message_count,
                        "title": s.title,
                    }))
                    .collect();
                let kms_update = build_kms_update_payload();
                let state = serde_json::json!({
                    "type": "initial_state",
                    "provider": provider_name,
                    "model": config.model,
                    "provider_ready": provider_ready,
                    "mcp_servers": mcp_servers,
                    "sessions": sessions,
                    "kmss": kms_update.get("kmss").cloned().unwrap_or(serde_json::Value::Array(vec![])),
                });
                let js = format!(
                    "window.__thclaws_dispatch('{}')",
                    escape_for_js(&state.to_string())
                );
                let _ = webview.evaluate_script(&js);
            }
            Event::UserEvent(UserEvent::QuitRequested) => {
                request_gui_shutdown(&shared_for_events, control_flow);
            }
            Event::UserEvent(UserEvent::ZoomChanged(scale)) => {
                let _ = webview.zoom(scale);
            }
            #[cfg(target_os = "macos")]
            Event::WindowEvent {
                event: WindowEvent::ModifiersChanged(modifiers),
                ..
            } => {
                macos_modifiers = modifiers;
            }
            #[cfg(target_os = "macos")]
            Event::WindowEvent {
                event: WindowEvent::KeyboardInput { event, .. },
                ..
            } if is_macos_close_shortcut(&event, macos_modifiers) => {
                request_gui_shutdown(&shared_for_events, control_flow);
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                request_gui_shutdown(&shared_for_events, control_flow);
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tool_coalesce_tests {
    use super::*;

    fn start(label: &str) -> ViewEvent {
        ViewEvent::ToolCallStart {
            name: label.to_string(),
            label: label.to_string(),
            input: serde_json::Value::Null,
        }
    }

    fn ok() -> ViewEvent {
        ViewEvent::ToolCallResult {
            name: "Ls".to_string(),
            output: String::new(),
            ui_resource: None,
        }
    }

    #[test]
    fn first_tool_call_renders_normally() {
        let mut s = TerminalRenderState::default();
        let out = render_terminal_ansi(&mut s, &start("Ls")).unwrap();
        assert!(out.contains("[tool: Ls]"));
        assert!(out.starts_with("\r\n"));
        let res = render_terminal_ansi(&mut s, &ok()).unwrap();
        assert_eq!(res, " \x1b[32m✓\x1b[0m");
    }

    #[test]
    fn repeated_tool_coalesces_with_count() {
        let mut s = TerminalRenderState::default();
        // First call: full line + ✓ (no trailing CRLF, parked).
        render_terminal_ansi(&mut s, &start("Ls")).unwrap();
        render_terminal_ansi(&mut s, &ok()).unwrap();
        // Second call: start suppressed, result rewrites with ×2.
        assert!(render_terminal_ansi(&mut s, &start("Ls")).is_none());
        let merged = render_terminal_ansi(&mut s, &ok()).unwrap();
        assert!(merged.starts_with("\r\x1b[2K"));
        assert!(merged.contains("×2"));
        // Third call: ×3.
        assert!(render_terminal_ansi(&mut s, &start("Ls")).is_none());
        let merged3 = render_terminal_ansi(&mut s, &ok()).unwrap();
        assert!(merged3.contains("×3"));
    }

    #[test]
    fn different_tool_breaks_coalesce_and_flushes_newline() {
        let mut s = TerminalRenderState::default();
        render_terminal_ansi(&mut s, &start("Ls")).unwrap();
        render_terminal_ansi(&mut s, &ok()).unwrap();
        // Different tool: leading \r\n acts as the line break.
        let next = render_terminal_ansi(&mut s, &start("Read")).unwrap();
        assert!(next.starts_with("\r\n"));
        assert!(next.contains("[tool: Read]"));
    }

    #[test]
    fn text_after_tool_starts_on_fresh_line() {
        let mut s = TerminalRenderState::default();
        render_terminal_ansi(&mut s, &start("Ls")).unwrap();
        render_terminal_ansi(&mut s, &ok()).unwrap();
        let text =
            render_terminal_ansi(&mut s, &ViewEvent::AssistantTextDelta("Done.".to_string()))
                .unwrap();
        assert!(text.starts_with("\r\n"));
        assert!(text.contains("Done."));
    }

    #[test]
    fn chat_dispatch_carries_tool_name_and_input_for_todowrite() {
        // Frontend keys on `tool_name === "TodoWrite"` to render the
        // checklist card. The IPC envelope must carry both the
        // unmangled tool name and the raw input so the renderer has
        // everything it needs without a follow-up round-trip.
        let ev = ViewEvent::ToolCallStart {
            name: "TodoWrite".to_string(),
            label: "TodoWrite".to_string(),
            input: serde_json::json!({
                "todos": [
                    { "id": "1", "content": "Investigate bug", "status": "in_progress" },
                    { "id": "2", "content": "Write fix", "status": "pending" },
                ]
            }),
        };
        let dispatches = render_chat_dispatches(&ev);
        assert_eq!(dispatches.len(), 1);
        let envelope: serde_json::Value =
            serde_json::from_str(&dispatches[0]).expect("valid JSON envelope");
        assert_eq!(envelope["type"], "chat_tool_call");
        assert_eq!(
            envelope["tool_name"], "TodoWrite",
            "frontend keys on tool_name to pick the custom render path",
        );
        let todos = &envelope["input"]["todos"];
        assert!(todos.is_array(), "todos array missing in input: {envelope}");
        assert_eq!(todos[0]["content"], "Investigate bug");
        assert_eq!(todos[0]["status"], "in_progress");
    }
}
