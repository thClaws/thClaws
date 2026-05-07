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
    /// Pre-M6.36 there were also `SessionListRefresh`, `FileTree`,
    /// `SessionLoaded` variants — the consumer arms in the event
    /// loop treated all four identically (escape + evaluate_script
    /// the dispatch). After SERVE9k migrated their construction
    /// sites to handle_ipc (which calls `ctx.dispatch`, wired here
    /// to `Dispatch(String)`), only the legacy gui-only handlers
    /// (`pick_directory`, `confirm`) still emit a typed variant —
    /// `SessionLoaded` survives for those.
    SessionLoaded(String),
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

// csv_table_tests moved to crate::file_preview::tests in M6.36 SERVE9k
// alongside the function they exercise.

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
// csv_to_markdown_table + render_markdown_to_html + ospath migrated
// to crate::file_preview (M6.36 SERVE9i); their gui.rs arms (file_*)
// now live in crate::ipc::handle_ipc, so the re-imports here became
// dead and were removed in SERVE9k.
//
// build_sso_state_payload migrated to crate::sso::build_state_payload
// (M6.36 SERVE9h); same disposition.

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

// build_session_list removed in M6.36 SERVE9k — its only callers were
// the migrated session_rename / session_delete / config_poll arms.
// crate::shared_session::build_session_list (the worker-side variant
// that includes current_id) is still consumed by the worker loop.

// provider_has_credentials / kind_has_credentials / auto_fallback_model
// moved to crate::providers in M6.36 SERVE9e so the WS transport can
// share the same readiness logic. Re-import here to keep gui.rs's
// existing call sites compiling unchanged.
// kind_has_credentials migrated to handle_ipc; the SendInitialState
// builder still uses auto_fallback_model + provider_has_credentials.
use crate::providers::{auto_fallback_model, provider_has_credentials};

/// Resolve the AGENTS.md path for the Settings → Instructions editor.
/// `scope="global"` → `~/.config/thclaws/AGENTS.md`, `scope="folder"` →
/// `./AGENTS.md` in the current working directory.
// instructions_path moved to crate::instructions in M6.36 SERVE9d.
// instructions_path migrated; arms removed in SERVE9k.

/// Per-server tool count, populated when `McpReady` fires in the worker loop.
/// Keyed by server name; value is the number of tools the server exposed.
static MCP_TOOL_COUNTS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, usize>>,
> = std::sync::OnceLock::new();

fn mcp_tool_counts() -> &'static std::sync::Mutex<std::collections::HashMap<String, usize>> {
    MCP_TOOL_COUNTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Record the tool count for a connected MCP server. Called from the
/// `McpReady` handler in `shared_session.rs` so `build_mcp_update_payload`
/// can surface real counts instead of zeros.
pub(crate) fn update_mcp_tool_count(server_name: &str, count: usize) {
    if let Ok(mut map) = mcp_tool_counts().lock() {
        map.insert(server_name.to_string(), count);
    }
}

/// Build the `mcp_update` IPC payload: the configured MCP servers for
/// this session (read fresh from disk so removals via `/mcp remove` are
/// reflected immediately, not after a restart). Tool counts come from
/// `MCP_TOOL_COUNTS`, which is updated by the `McpReady` worker event
/// once each server successfully connects and lists its tools.
pub(crate) fn build_mcp_update_payload() -> serde_json::Value {
    let config = crate::config::AppConfig::load().unwrap_or_default();
    let counts = mcp_tool_counts().lock().unwrap_or_else(|e| e.into_inner());
    let servers: Vec<serde_json::Value> = config
        .mcp_servers
        .iter()
        .map(|s| {
            let tool_count = counts.get(&s.name).copied().unwrap_or(0);
            serde_json::json!({"name": s.name, "tools": tool_count})
        })
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
// external_url helpers migrated; open_external arm removed in SERVE9k.

/// Assemble the cross-provider model list payload for the sidebar's
/// inline picker dropdown (#49). Catalogue rows for every known
/// provider, plus a live Ollama probe so models added via `ollama pull`
/// after launch are visible without restart. The Ollama probe uses a
/// short timeout — failure just falls back to whatever rows are in the
/// baseline catalogue.
// build_all_models_payload moved to crate::providers in M6.36 SERVE9g
// so the WS transport's request_all_models IPC arm can call it from
// the always-on dispatch table.
// build_all_models_payload migrated; request_all_models removed in SERVE9k.

fn request_gui_shutdown(
    shared: &SharedSessionHandle,
    control_flow: &mut ControlFlow,
    latest_window_size: Option<(f64, f64)>,
) {
    // Persist the latest window size so the next launch restores it.
    // Only writes when the size actually changed from what's on disk —
    // avoids a no-op rewrite that would touch the file's mtime.
    if let Some((w, h)) = latest_window_size {
        let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
        if project.window_width != Some(w) || project.window_height != Some(h) {
            project.window_width = Some(w);
            project.window_height = Some(h);
            let _ = project.save();
        }
    }
    let _ = shared.input_tx.send(ShellInput::SaveAndQuit);
    // Kill any spawned teammate processes.
    let _ = std::process::Command::new("pkill")
        .args(["-f", "team-agent"])
        .status();
    *control_flow = ControlFlow::Exit;
}

pub fn run_gui() {
    run_gui_inner(None);
}

/// Combo entry point for `--serve --gui`: builds the desktop window and
/// the HTTP/WebSocket server in the same process, sharing one engine.
/// Browser tabs and the desktop window see the same conversation; tool
/// approvals raised on the desktop apply to both.
///
/// Note: GuiApprover's approval-request channel is owned by the desktop
/// window's forwarder, so today the browser surface does not get its
/// own `approval_request` dispatches. Same trade-off as `--serve` alone,
/// where approval forwarding is also unwired (the rx is dropped). For
/// the combo, this means: approve on the desktop window, the action
/// applies to whichever surface triggered it.
pub fn run_gui_with_serve(config: crate::server::ServeConfig) {
    run_gui_inner(Some(config));
}

fn run_gui_inner(serve: Option<crate::server::ServeConfig>) {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Pick a sensible default window size based on the primary
    // monitor's logical resolution: roomy on workstation-class
    // displays (>=1920x1080), conservative on laptop screens. Only
    // applies when no explicit size lives in `.thclaws/settings.json`.
    let (default_w, default_h) = {
        let big = event_loop
            .primary_monitor()
            .map(|m| {
                let size = m.size();
                let scale = m.scale_factor().max(0.0001);
                let logical_w = size.width as f64 / scale;
                let logical_h = size.height as f64 / scale;
                logical_w >= 1920.0 && logical_h >= 1080.0
            })
            .unwrap_or(false);
        if big {
            (1760.0, 962.0)
        } else {
            (1200.0, 800.0)
        }
    };

    let (win_w, win_h, initial_zoom) = crate::config::ProjectConfig::load()
        .map(|c| {
            (
                c.window_width.unwrap_or(default_w),
                c.window_height.unwrap_or(default_h),
                c.gui_scale.unwrap_or(1.0),
            )
        })
        .unwrap_or((default_w, default_h, 1.0));
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

    // Combo mode: spin up the HTTP/WS server on the active tokio runtime
    // sharing the same engine (approver, shared session, pending asks)
    // as the desktop window. Browser tabs land in the same conversation
    // the user is looking at on the desktop. Errors print to stderr and
    // the GUI keeps running — losing the server is degraded, not fatal.
    if let Some(serve_config) = serve {
        let approver_for_serve = approver.clone();
        let shared_for_serve = shared.clone();
        let pending_asks_for_serve = pending_asks.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::server::run_with_engine(
                serve_config,
                approver_for_serve,
                shared_for_serve,
                pending_asks_for_serve,
            )
            .await
            {
                eprintln!("\x1b[31m[serve] error: {e}\x1b[0m");
            }
        });
    }

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
                            "originator": req.originator,
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
                    "originator": req.originator,
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
                // ─── EE Phase 4: org-policy SSO (IPC surface for sidebar) ─
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
                "pty_kill" | "pty_resize" | "restart" => {
                    // PTY-era hooks. The shared in-process session has
                    // no PTY to kill or resize; ignore quietly so the
                    // frontend can keep emitting them during transition.
                }
                // new_session migrated to crate::ipc::handle_ipc.
                // instructions_get + instructions_save migrated to crate::ipc::handle_ipc.
                // kms_list migrated to crate::ipc::handle_ipc.
                // theme_get + theme_set migrated to crate::ipc::handle_ipc.
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

    // Latest logical window size, updated on every WindowEvent::Resized
    // and persisted to .thclaws/settings.json on shutdown so the next
    // launch restores it.
    let mut latest_window_size: Option<(f64, f64)> = None;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::UserEvent(UserEvent::Dispatch(json)) => {
                let escaped = escape_for_js(&json);
                let _ = webview.evaluate_script(&format!(
                    "window.__thclaws_dispatch('{escaped}')"
                ));
            }
            Event::UserEvent(UserEvent::FileContent(json))
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
                request_gui_shutdown(&shared_for_events, control_flow, latest_window_size);
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
                request_gui_shutdown(&shared_for_events, control_flow, latest_window_size);
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                request_gui_shutdown(&shared_for_events, control_flow, latest_window_size);
            }
            Event::WindowEvent {
                event: WindowEvent::Resized(physical_size),
                ..
            } => {
                let scale_factor = window.scale_factor();
                let logical = physical_size.to_logical::<f64>(scale_factor);
                latest_window_size = Some((logical.width, logical.height));
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
