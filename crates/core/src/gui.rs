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
}

const MAX_RECENT_DIRS: usize = 3;

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
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        for dispatch in render_chat_dispatches(&ev) {
                            let _ = proxy.send_event(UserEvent::Dispatch(dispatch));
                        }
                        if let Some(ansi) = render_terminal_ansi(&ev) {
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

/// Build chat-shaped JSON message(s) for a single ViewEvent. Most
/// events translate to one message; HistoryReplaced fans out as a
/// single `chat_history_replaced` envelope carrying the full message
/// list.
///
/// All text fields are stripped of ANSI escape sequences — the chat
/// bubble renders raw text in `whitespace-pre-wrap` and would show
/// codes like `\x1b[2m...\x1b[0m` as visible `[2m...[0m` junk. The
/// terminal path (which xterm.js parses natively) is unaffected.
fn render_chat_dispatches(ev: &ViewEvent) -> Vec<String> {
    match ev {
        ViewEvent::UserPrompt(text) => vec![serde_json::json!({
            "type": "chat_user_message",
            "text": strip_ansi(text),
        })
        .to_string()],
        ViewEvent::AssistantTextDelta(text) => vec![serde_json::json!({
            "type": "chat_text_delta",
            "text": strip_ansi(text),
        })
        .to_string()],
        ViewEvent::ToolCallStart { name: _, label } => vec![serde_json::json!({
            "type": "chat_tool_call",
            "name": strip_ansi(label),
        })
        .to_string()],
        ViewEvent::ToolCallResult { name, output } => vec![serde_json::json!({
            "type": "chat_tool_result",
            "name": name,
            "output": strip_ansi(output),
        })
        .to_string()],
        ViewEvent::SlashOutput(text) => vec![serde_json::json!({
            "type": "chat_slash_output",
            "text": strip_ansi(text),
        })
        .to_string()],
        ViewEvent::TurnDone => vec![serde_json::json!({"type": "chat_done"}).to_string()],
        ViewEvent::HistoryReplaced(messages) => {
            let arr: Vec<serde_json::Value> = messages
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "role": m.role,
                        "content": strip_ansi(&m.content),
                    })
                })
                .collect();
            vec![serde_json::json!({
                "type": "chat_history_replaced",
                "messages": arr,
            })
            .to_string()]
        }
        ViewEvent::SessionListRefresh(json) => vec![json.clone()],
        ViewEvent::ProviderUpdate(json) => vec![json.clone()],
        ViewEvent::KmsUpdate(json) => vec![json.clone()],
        ViewEvent::ContextWarning { file_size_mb } => vec![serde_json::json!({
            "type": "chat_context_warning",
            "file_size_mb": file_size_mb,
        })
        .to_string()],
        ViewEvent::ErrorText(text) => vec![serde_json::json!({
            "type": "chat_text_delta",
            "text": format!("\n{}\n", strip_ansi(text)),
        })
        .to_string()],
    }
}

/// Strip ANSI escape sequences from a string. Handles the common forms
/// emitted by `repl::render_help` and tool output:
///   - CSI sequences:   `ESC [ … (digits/semicolons) … (final byte 0x40-0x7e)`
///   - OSC sequences:   `ESC ] … (terminator BEL or ST)`
///   - Bare `ESC X`     where X is any single byte (Fe escape)
///
/// Doesn't try to convert colours into anything else — the chat bubble
/// is plain text, and the user is just asking us to stop leaking
/// terminal junk into it.
fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    // CSI: skip parameters/intermediates until a final
                    // byte in 0x40..=0x7e.
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1; // consume the final byte
                    }
                    continue;
                }
                b']' => {
                    // OSC: terminate on BEL (0x07) or ST (ESC \).
                    i += 2;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }
                _ => {
                    // Two-byte Fe escape: drop both.
                    i += 2;
                    continue;
                }
            }
        }
        // Pass through. Multi-byte UTF-8 sequences are preserved
        // intact because we operate at the byte level and only consume
        // ESC-prefixed sequences.
        out.push(bytes[i]);
        i += 1;
    }
    // Output bytes are guaranteed valid UTF-8: we either passed through
    // bytes from the original (valid) UTF-8 input or skipped them. The
    // ASCII escape bytes we drop are never inside a multi-byte run.
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod ansi_strip_tests {
    use super::strip_ansi;

    #[test]
    fn strips_csi_sgr() {
        assert_eq!(strip_ansi("\x1b[2mhello\x1b[0m"), "hello");
        assert_eq!(
            strip_ansi("\x1b[31;1mred bold\x1b[0m text"),
            "red bold text"
        );
    }

    #[test]
    fn strips_cursor_moves() {
        assert_eq!(strip_ansi("a\x1b[2K\rb"), "a\rb");
    }

    #[test]
    fn passes_plain_text_through() {
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("with\nnewlines"), "with\nnewlines");
    }

    #[test]
    fn strips_osc_with_bel() {
        assert_eq!(strip_ansi("\x1b]0;title\x07after"), "after");
    }
}

/// Convert a ViewEvent into ANSI bytes suitable for xterm.js. Returns
/// None when the event is metadata-only (e.g. a SessionListRefresh —
/// the sidebar handles that via its own dispatch shape).
fn render_terminal_ansi(ev: &ViewEvent) -> Option<String> {
    match ev {
        ViewEvent::UserPrompt(text) => {
            // Multi-line prompts (typical from a paste): `> ` marker on
            // the first line only, two-space indent on continuations
            // so the block reads as a single message. Convert `\n` →
            // `\r\n` so xterm returns to column 0 instead of staircasing.
            let marker = "\x1b[2m> \x1b[0m";
            let indent = "  ";
            let mut lines = text.split('\n');
            let mut body = String::new();
            if let Some(first) = lines.next() {
                body.push_str(&format!("{marker}{first}"));
            }
            for line in lines {
                body.push_str("\r\n");
                body.push_str(indent);
                body.push_str(line);
            }
            body.push_str("\r\n");
            Some(body)
        }
        ViewEvent::AssistantTextDelta(text) => {
            // Newlines from the model arrive as plain `\n`; xterm needs
            // `\r\n` to start a fresh line at column 0.
            Some(text.replace('\n', "\r\n"))
        }
        ViewEvent::ToolCallStart { name: _, label } => {
            Some(format!("\r\n\x1b[2m[tool: {label}]\x1b[0m"))
        }
        ViewEvent::ToolCallResult { .. } => Some(" \x1b[32m✓\x1b[0m".to_string()),
        ViewEvent::SlashOutput(text) => {
            let body = text.replace('\n', "\r\n");
            Some(format!("\x1b[2m{body}\x1b[0m\r\n"))
        }
        // TurnDone doesn't emit terminal bytes — TerminalView's
        // `chat_done` handler writes the next prompt (and restores any
        // line buffer the user typed during streaming). Doubling up
        // here would print an extra blank line.
        ViewEvent::TurnDone => None,
        ViewEvent::HistoryReplaced(messages) => {
            // Clear scrollback + screen + cursor home, then replay each
            // historical message in the same ANSI shapes the live stream
            // uses.
            let mut out = String::from("\x1b[3J\x1b[2J\x1b[H");
            for m in messages {
                let line = match m.role.as_str() {
                    "user" => {
                        // Match the live UserPrompt rendering: `> ` on
                        // the first line only, two-space indent on
                        // continuations so the whole block reads as a
                        // single message.
                        let marker = "\x1b[2m> \x1b[0m";
                        let indent = "  ";
                        let mut lines = m.content.split('\n');
                        let mut body = String::new();
                        if let Some(first) = lines.next() {
                            body.push_str(&format!("{marker}{first}"));
                        }
                        for l in lines {
                            body.push_str("\r\n");
                            body.push_str(indent);
                            body.push_str(l);
                        }
                        body.push_str("\r\n");
                        body
                    }
                    "assistant" => {
                        format!("{}\r\n", m.content.replace('\n', "\r\n"))
                    }
                    _ => format!(
                        "\x1b[2m{}\x1b[0m\r\n",
                        m.content.replace('\n', "\r\n")
                    ),
                };
                out.push_str(&line);
            }
            Some(out)
        }
        ViewEvent::ErrorText(text) => {
            Some(format!("\r\n\x1b[31m{text}\x1b[0m\r\n"))
        }
        ViewEvent::SessionListRefresh(_) => None,
        ViewEvent::ProviderUpdate(_) => None,
        ViewEvent::KmsUpdate(_) => None,
        ViewEvent::ContextWarning { file_size_mb } => Some(format!(
            "\r\n\x1b[33m[ session {:.1} MB — /fork to continue in a new session with summary ]\x1b[0m\r\n",
            file_size_mb
        )),
    }
}

fn terminal_data_envelope(ansi: &str) -> String {
    let bytes = ansi.as_bytes();
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    serde_json::json!({"type": "terminal_data", "data": b64}).to_string()
}

/// Like `terminal_data_envelope` but the frontend handler always
/// writes a fresh prompt at the end — used for session load / new-
/// session events so an empty history doesn't leave the user staring
/// at a blank terminal with no chevron.
fn terminal_history_replaced_envelope(ansi: &str) -> String {
    let bytes = ansi.as_bytes();
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    serde_json::json!({"type": "terminal_history_replaced", "data": b64}).to_string()
}

/// Convert a markdown string to a full standalone HTML document so the
/// Files-tab iframe can render it without any client-side markdown
/// library. GFM extensions are enabled (tables, task lists,
/// strikethrough, autolinks); raw HTML in the source is stripped
/// (`render.unsafe_ = false`) so `<script>` in a `.md` file we're
/// previewing can't escape the iframe sandbox.
fn render_markdown_to_html(md: &str, theme: &str) -> String {
    let mut opts = comrak::ComrakOptions::default();
    opts.extension.table = true;
    opts.extension.strikethrough = true;
    opts.extension.tasklist = true;
    opts.extension.autolink = true;
    opts.extension.footnotes = true;
    opts.extension.header_ids = Some(String::new());
    opts.render.unsafe_ = false;
    let body = comrak::markdown_to_html(md, &opts);

    // Preview is rendered inside a sandboxed iframe, so it lives in its
    // own document with its own palette. The frontend passes the
    // *resolved* theme ("light" | "dark") — "system" is resolved client-
    // side to one of those so this function never has to inspect any
    // runtime signal. We emit a single palette rather than a media
    // query so that a user explicitly choosing Light while their OS is
    // Dark (or vice versa) is honoured.
    let (fg, bg, muted, accent, code_bg, border, color_scheme) = if theme == "light" {
        (
            "#1a1a1a", "#ffffff", "#606366", "#2867c4", "#f3f4f6", "#d0d7de", "light",
        )
    } else {
        (
            "#e6e6e6", "#1a1a1a", "#9aa0a6", "#6cb0ff", "#2a2a2a", "#333", "dark",
        )
    };

    format!(
        r##"<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  :root {{
    color-scheme: {color_scheme};
    --fg: {fg};
    --bg: {bg};
    --muted: {muted};
    --accent: {accent};
    --code-bg: {code_bg};
    --border: {border};
  }}
  html, body {{ margin: 0; padding: 0; }}
  body {{
    font: 14px/1.65 -apple-system, BlinkMacSystemFont, "Segoe UI",
          "Helvetica Neue", Arial, "Noto Sans Thai", sans-serif;
    color: var(--fg); background: var(--bg); padding: 24px 32px;
    max-width: 880px; margin: 0 auto;
  }}
  h1, h2, h3, h4, h5, h6 {{ line-height: 1.25; margin: 1.4em 0 0.5em; }}
  h1 {{ font-size: 1.8em; border-bottom: 1px solid var(--border); padding-bottom: 0.3em; }}
  h2 {{ font-size: 1.4em; border-bottom: 1px solid var(--border); padding-bottom: 0.25em; }}
  h3 {{ font-size: 1.2em; }}
  p, ul, ol, blockquote, pre, table {{ margin: 0.8em 0; }}
  a {{ color: var(--accent); text-decoration: none; }}
  a:hover {{ text-decoration: underline; }}
  code {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
          font-size: 0.92em; background: var(--code-bg);
          padding: 2px 5px; border-radius: 3px; }}
  pre {{ background: var(--code-bg); padding: 12px 14px; border-radius: 6px;
         overflow-x: auto; }}
  pre code {{ background: transparent; padding: 0; font-size: 0.9em; }}
  blockquote {{ margin: 0.8em 0; padding: 0 1em; color: var(--muted);
                border-left: 3px solid var(--border); }}
  table {{ border-collapse: collapse; }}
  th, td {{ border: 1px solid var(--border); padding: 6px 12px; text-align: left; }}
  th {{ background: var(--code-bg); font-weight: 600; }}
  hr {{ border: 0; border-top: 1px solid var(--border); margin: 2em 0; }}
  img {{ max-width: 100%; height: auto; }}
  ul.contains-task-list {{ list-style: none; padding-left: 1em; }}
  .task-list-item input[type="checkbox"] {{ margin-right: 0.5em; }}
</style>
</head><body>
{body}
</body></html>"##,
        body = body
    )
}

fn recent_dirs_path() -> Option<std::path::PathBuf> {
    crate::util::home_dir().map(|h| h.join(".config/thclaws/recent_dirs.json"))
}

fn load_recent_dirs() -> Vec<String> {
    let Some(path) = recent_dirs_path() else {
        return vec![];
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return vec![];
    };
    serde_json::from_str::<Vec<String>>(&contents).unwrap_or_default()
}

fn save_recent_dir(dir: &str) {
    let Some(path) = recent_dirs_path() else {
        return;
    };
    let mut dirs = load_recent_dirs();
    // Remove duplicate if present, then prepend.
    dirs.retain(|d| d != dir);
    dirs.insert(0, dir.to_string());
    dirs.truncate(MAX_RECENT_DIRS);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        path,
        serde_json::to_string_pretty(&dirs).unwrap_or_default(),
    );
}

// ── UI theme persistence ─────────────────────────────────────────────
// Lives in its own tiny file under `~/.config/thclaws/` rather than
// settings.json because theme is a per-user UI preference, not an
// agent-runtime knob — keeping it separate avoids polluting any
// project-committed settings.json.

fn theme_path() -> Option<std::path::PathBuf> {
    crate::util::home_dir().map(|h| h.join(".config/thclaws/theme.json"))
}

fn normalize_theme(raw: &str) -> &'static str {
    match raw {
        "light" => "light",
        "dark" => "dark",
        _ => "system",
    }
}

fn load_theme() -> String {
    let Some(path) = theme_path() else {
        return "system".to_string();
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return "system".to_string();
    };
    let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap_or_default();
    let mode = parsed
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("system");
    normalize_theme(mode).to_string()
}

fn save_theme(mode: &str) {
    let Some(path) = theme_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let payload = serde_json::json!({ "mode": normalize_theme(mode) });
    let _ = std::fs::write(
        path,
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    );
}

/// Convert a frontend-supplied path (always slash-separated, since it
/// comes from JSON / the React tree) to the OS-native form before
/// passing it to filesystem APIs. No-op on macOS/Linux. On Windows,
/// translates `/` → `\` so paths like `src/api/foo.ts` resolve via
/// `Sandbox::check` instead of being rejected as malformed.
/// Backported from public repo (commit 8ad6f80).
fn ospath(path: &str) -> String {
    #[cfg(not(target_os = "windows"))]
    {
        path.to_string()
    }
    #[cfg(target_os = "windows")]
    {
        path.replace('/', "\\")
    }
}

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
fn native_confirm(title: &str, message: &str, yes_label: &str, no_label: &str) -> bool {
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

/// Does the active provider have credentials (env var set) or is it
/// a no-auth local provider? Used to tell the sidebar whether to show
/// the provider name normally or flag it as "no key configured".
fn provider_has_credentials(cfg: &AppConfig) -> bool {
    kind_has_credentials(cfg.detect_provider_kind().ok())
}

fn kind_has_credentials(kind: Option<crate::providers::ProviderKind>) -> bool {
    use crate::providers::ProviderKind;
    let Some(kind) = kind else { return false };
    match kind {
        // Agent SDK uses Claude Code's own auth — assume present.
        ProviderKind::AgentSdk => true,
        // Ollama variants don't need auth; reachability is surfaced
        // on first prompt, not here.
        ProviderKind::Ollama | ProviderKind::OllamaAnthropic => true,
        // Every other provider's readiness == "its env var is set".
        other => other
            .api_key_env()
            .map(|v| std::env::var(v).is_ok())
            .unwrap_or(false),
    }
}

/// If `cfg.model`'s provider has no credentials, pick the first provider
/// that does and return its default model. Returns `None` when the
/// current model is already fine or nothing else is usable.
///
/// Intended for the GUI — it gets called at startup and after every
/// `api_key_set` so the sidebar's active-provider indicator and the
/// persisted `.thclaws/settings.json` settle onto whatever the user
/// actually has configured.
fn auto_fallback_model(cfg: &AppConfig) -> Option<String> {
    use crate::providers::ProviderKind;
    if provider_has_credentials(cfg) {
        return None;
    }
    const ORDER: &[ProviderKind] = &[
        ProviderKind::Anthropic,
        ProviderKind::OpenAI,
        ProviderKind::AgenticPress,
        ProviderKind::OpenRouter,
        ProviderKind::Gemini,
        ProviderKind::DashScope,
        // Local providers omitted here: if the user explicitly
        // configured one of them, they're already "ready" above; we
        // don't want to auto-fall-back to Ollama for a user who has
        // no local Ollama running.
    ];
    for kind in ORDER {
        if kind_has_credentials(Some(*kind)) {
            return Some(kind.default_model().to_string());
        }
    }
    None
}

/// Resolve the AGENTS.md path for the Settings → Instructions editor.
/// `scope="global"` → `~/.config/thclaws/AGENTS.md`, `scope="folder"` →
/// `./AGENTS.md` in the current working directory.
fn instructions_path(scope: &str) -> Option<std::path::PathBuf> {
    match scope {
        "global" => crate::util::home_dir().map(|h| h.join(".config/thclaws/AGENTS.md")),
        _ => std::env::current_dir().ok().map(|d| d.join("AGENTS.md")),
    }
}

/// Build the `kms_update` IPC payload: every discoverable KMS tagged with
/// whether it's currently attached to this project.
pub(crate) fn build_kms_update_payload() -> serde_json::Value {
    let active: std::collections::HashSet<String> = crate::config::ProjectConfig::load()
        .and_then(|c| c.kms.map(|k| k.active))
        .unwrap_or_default()
        .into_iter()
        .collect();
    let kmss: Vec<serde_json::Value> = crate::kms::list_all()
        .into_iter()
        .map(|k| {
            serde_json::json!({
                "name": k.name,
                "scope": k.scope.as_str(),
                "active": active.contains(&k.name),
            })
        })
        .collect();
    serde_json::json!({
        "type": "kms_update",
        "kmss": kmss,
    })
}

fn escape_for_js(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
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

    let (win_w, win_h) = crate::config::ProjectConfig::load()
        .map(|c| {
            (
                c.window_width.unwrap_or(1200.0),
                c.window_height.unwrap_or(800.0),
            )
        })
        .unwrap_or((1200.0, 800.0));
    let window = WindowBuilder::new()
        .with_title("thClaws")
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
            let ty = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");

            match ty {
                "app_close" => {
                    let _ = proxy_for_ipc.send_event(UserEvent::QuitRequested);
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
                "get_cwd" => {
                    let cwd = std::env::current_dir()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| ".".into());
                    let needs_modal = true;
                    let recent = load_recent_dirs();
                    let payload = serde_json::json!({
                        "type": "current_cwd",
                        "path": cwd,
                        "needs_modal": needs_modal,
                        "recent_dirs": recent,
                    });
                    let _ = proxy_for_ipc.send_event(
                        UserEvent::SessionLoaded(payload.to_string()),
                    );
                }
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
                "set_cwd" => {
                    if let Some(path) = msg.get("path").and_then(|v| v.as_str()) {
                        let p = std::path::Path::new(path);
                        if p.is_dir() {
                            let _ = std::env::set_current_dir(p);
                            let _ = crate::sandbox::Sandbox::init();
                            save_recent_dir(path);
                            // Tell the running worker to reload project
                            // settings from the new cwd and swap its model
                            // accordingly — without this, the session keeps
                            // whatever model was loaded at startup, even
                            // when the new project's settings.json says
                            // something different. Project settings must
                            // win — that's the contract.
                            // Tell the worker to reload settings + swap
                            // model from the new project's settings.json.
                            let _ = shared_for_ipc
                                .input_tx
                                .send(ShellInput::ChangeCwd(p.to_path_buf()));
                            let payload = serde_json::json!({
                                "type": "cwd_changed",
                                "path": path,
                                "ok": true,
                            });
                            let _ = proxy_for_ipc.send_event(
                                UserEvent::SessionLoaded(payload.to_string()),
                            );
                        } else {
                            let payload = serde_json::json!({
                                "type": "cwd_changed",
                                "path": path,
                                "ok": false,
                                "error": format!("'{}' is not a valid directory", path),
                            });
                            let _ = proxy_for_ipc.send_event(
                                UserEvent::SessionLoaded(payload.to_string()),
                            );
                        }
                    }
                }
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
                "shell_cancel" => {
                    // Ctrl+C on an empty line in the Terminal tab (or an
                    // explicit cancel action from Chat) — request the
                    // current turn stop at its next streaming event.
                    shared_for_ipc.request_cancel();
                }
                "frontend_ready" => {
                    // The frontend calls this once it's past the startup
                    // modals (working-directory + secrets-backend pickers),
                    // which releases deferred startup work like MCP spawn
                    // approval. Idempotent — safe to call on every mount.
                    shared_for_ipc.ready_gate.signal();
                }
                "approval_response" => {
                    // Frontend approval modal resolved. Parse out the
                    // request id + decision and hand them to the
                    // GuiApprover, which unblocks the waiting agent call.
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
                    approver_for_ipc.resolve(id, decision);
                }
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
                "new_session" => {
                    let _ = shared_for_ipc.input_tx.send(ShellInput::NewSession);
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        serde_json::json!({"type": "new_session_ack"}).to_string(),
                    ));
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        serde_json::json!({"type": "terminal_clear"}).to_string(),
                    ));
                }
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
                "instructions_get" => {
                    let scope = msg.get("scope").and_then(|v| v.as_str()).unwrap_or("folder");
                    let path = instructions_path(scope);
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
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                }
                "instructions_save" => {
                    let scope = msg.get("scope").and_then(|v| v.as_str()).unwrap_or("folder");
                    let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    let (ok, error, path) = match instructions_path(scope) {
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
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                }
                "kms_list" => {
                    let payload = build_kms_update_payload();
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                }
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
                "theme_get" => {
                    let payload = serde_json::json!({
                        "type": "theme",
                        "mode": load_theme(),
                    });
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                }
                "theme_set" => {
                    let requested = msg.get("mode").and_then(|v| v.as_str()).unwrap_or("system");
                    let normalized = normalize_theme(requested).to_string();
                    save_theme(&normalized);
                    let payload = serde_json::json!({
                        "type": "theme",
                        "mode": normalized,
                    });
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        payload.to_string()
                    ));
                }
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
                        let team_dir = std::env::current_dir()
                            .unwrap_or_default()
                            .join(crate::team::Mailbox::default_dir());
                        let mailbox = crate::team::Mailbox::new(team_dir);
                        let tm = crate::team::TeamMessage::new("user", text);
                        let _ = mailbox.write_to_mailbox(to, tm);
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
                    // Broadcast the refreshed list so the sidebar picks up the new title.
                    if ok {
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
        });
    // wry exposes a different constructor on Linux because WebKit2GTK
    // mounts as a GTK widget rather than over a raw window handle.
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window).expect("webview build");
    #[cfg(target_os = "linux")]
    let webview = builder
        .build_gtk(window.default_vbox().unwrap())
        .expect("webview build (gtk)");

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
                    state.to_string().replace('\\', "\\\\").replace('\'', "\\'")
                );
                let _ = webview.evaluate_script(&js);
            }
            Event::UserEvent(UserEvent::QuitRequested) => {
                request_gui_shutdown(&shared_for_events, control_flow);
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
