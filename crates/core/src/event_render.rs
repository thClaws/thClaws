//! Translate `ViewEvent` → frontend-shaped JSON payloads.
//!
//! Pre-M6.36 this lived in `gui.rs` under `#[cfg(feature = "gui")]`,
//! reachable only by the wry transport. M6.36 SERVE3 promotes it to
//! a transport-agnostic module so the Axum WebSocket layer (`server.rs`)
//! can use the exact same renderer — both surfaces render identical
//! envelopes, so frontend code (React) doesn't care which transport
//! delivered the dispatch.
//!
//! Two render shapes:
//!
//! - [`render_chat_dispatches`] — chat-shaped JSON envelopes
//!   (`chat_text_delta`, `chat_tool_call`, `chat_history_replaced`, …)
//!   consumed by `ChatView.tsx`. Most events translate to one envelope;
//!   `HistoryReplaced` fans out as one big snapshot.
//! - [`render_terminal_ansi`] — terminal-shaped ANSI bytes consumed
//!   by `TerminalView.tsx` (xterm.js). Stateful — call sites pass an
//!   owned [`TerminalRenderState`] threaded across consecutive events
//!   so same-tool-label coalescing works.
//!
//! Both renderers strip ANSI escape sequences from chat text (chat
//! bubble is plain-text whitespace-pre-wrap and would render `\x1b[2m`
//! as visible junk) but pass them through to the terminal path.

use crate::shared_session::ViewEvent;
use base64::Engine;

// ── Chat-shaped translator ─────────────────────────────────────────

/// Build chat-shaped JSON message(s) for a single ViewEvent. Most
/// events translate to one message; `HistoryReplaced` fans out as a
/// single `chat_history_replaced` envelope carrying the full message
/// list.
///
/// All text fields are stripped of ANSI escape sequences — the chat
/// bubble renders raw text in `whitespace-pre-wrap` and would show
/// codes like `\x1b[2m...\x1b[0m` as visible `[2m...[0m` junk. The
/// terminal path (which xterm.js parses natively) is unaffected.
pub fn render_chat_dispatches(ev: &ViewEvent) -> Vec<String> {
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
        ViewEvent::AssistantThinkingDelta(text) => vec![serde_json::json!({
            "type": "chat_thinking_delta",
            "text": strip_ansi(text),
        })
        .to_string()],
        ViewEvent::ToolCallStart { name, label, input } => vec![serde_json::json!({
            "type": "chat_tool_call",
            "name": strip_ansi(label),
            "tool_name": name,
            "input": input,
        })
        .to_string()],
        ViewEvent::ToolCallResult {
            name,
            output,
            ui_resource,
        } => {
            let mut env = serde_json::json!({
                "type": "chat_tool_result",
                "name": name,
                "output": strip_ansi(output),
            });
            if let Some(ui) = ui_resource {
                env["ui_resource"] = serde_json::json!({
                    "uri": ui.uri,
                    "html": ui.html,
                    "mime": ui.mime,
                });
            }
            vec![env.to_string()]
        }
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
        ViewEvent::McpUpdate(json) => vec![json.clone()],
        ViewEvent::ModelPickerOpen(json) => vec![json.clone()],
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
        ViewEvent::McpAppCallToolResult {
            request_id,
            content,
            is_error,
        } => vec![serde_json::json!({
            "type": "mcp_call_tool_result",
            "requestId": request_id,
            "content": content,
            "isError": is_error,
        })
        .to_string()],
        // QuitRequested is intercepted by the translator before this
        // function is called — see the early-return in
        // `gui::spawn_event_translator` / the equivalent web hook.
        ViewEvent::QuitRequested => vec![],
        ViewEvent::PlanUpdate(plan) => {
            let payload = serde_json::json!({
                "type": "chat_plan_update",
                "plan": plan,
            });
            vec![payload.to_string()]
        }
        ViewEvent::PermissionModeChanged(mode) => {
            let mode_str = match mode {
                crate::permissions::PermissionMode::Auto => "auto",
                crate::permissions::PermissionMode::Ask => "ask",
                crate::permissions::PermissionMode::Plan => "plan",
            };
            let payload = serde_json::json!({
                "type": "chat_permission_mode",
                "mode": mode_str,
            });
            vec![payload.to_string()]
        }
        ViewEvent::PlanStalled {
            step_id,
            step_title,
            turns,
        } => {
            let payload = serde_json::json!({
                "type": "chat_plan_stalled",
                "step_id": step_id,
                "step_title": step_title,
                "turns": turns,
            });
            vec![payload.to_string()]
        }
    }
}

// ── ANSI strip ─────────────────────────────────────────────────────

/// Strip ANSI escape sequences from a string. Handles the common forms
/// emitted by `repl::render_help` and tool output:
///   - CSI sequences:   `ESC [ … (digits/semicolons) … (final byte 0x40-0x7e)`
///   - OSC sequences:   `ESC ] … (terminator BEL or ST)`
///   - Bare `ESC X`     where X is any single byte (Fe escape)
pub fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                    continue;
                }
                b']' => {
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
                    i += 2;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── Terminal-shaped translator (stateful) ──────────────────────────

/// State carried across calls to `render_terminal_ansi` so consecutive
/// tool calls with the same label coalesce into a single line with a
/// `×N` count, instead of stacking N copies of `[tool: Ls] ✓`.
#[derive(Default)]
pub struct TerminalRenderState {
    last_tool_label: Option<String>,
    last_tool_count: u32,
    merging: bool,
    pending_newline_after_tool: bool,
}

/// Convert a ViewEvent into ANSI bytes suitable for xterm.js. Returns
/// None when the event is metadata-only (e.g. a `SessionListRefresh` —
/// the sidebar handles that via its own dispatch shape).
pub fn render_terminal_ansi(state: &mut TerminalRenderState, ev: &ViewEvent) -> Option<String> {
    // Tool-call coalescing lives ahead of the generic event match so
    // it can suppress / rewrite output without going through the
    // pending-newline flush path below.
    match ev {
        ViewEvent::ToolCallStart {
            name: _,
            label,
            input: _,
        } => {
            if state.pending_newline_after_tool
                && state.last_tool_label.as_deref() == Some(label.as_str())
                && state.last_tool_count >= 1
            {
                state.pending_newline_after_tool = false;
                state.merging = true;
                return None;
            }
            state.last_tool_label = Some(label.clone());
            state.last_tool_count = 0;
            state.merging = false;
            state.pending_newline_after_tool = false;
            return Some(format!("\r\n\x1b[2m[tool: {label}]\x1b[0m"));
        }
        ViewEvent::ToolCallResult { .. } => {
            if state.merging {
                state.merging = false;
                state.last_tool_count += 1;
                state.pending_newline_after_tool = true;
                let label = state.last_tool_label.clone().unwrap_or_default();
                let count = state.last_tool_count;
                return Some(format!(
                    "\r\x1b[2K\x1b[2m[tool: {label}]\x1b[0m \x1b[32m✓\x1b[0m \x1b[2m×{count}\x1b[0m"
                ));
            }
            state.last_tool_count = 1;
            state.pending_newline_after_tool = true;
            return Some(" \x1b[32m✓\x1b[0m".to_string());
        }
        _ => {}
    }

    let inner = match ev {
        ViewEvent::UserPrompt(text) => {
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
        ViewEvent::AssistantTextDelta(text) => Some(text.replace('\n', "\r\n")),
        ViewEvent::AssistantThinkingDelta(text) => {
            // Reasoning rendered dim-italic so it's visibly distinct from
            // the assistant's final answer in the terminal stream.
            let body = text.replace('\n', "\r\n");
            Some(format!("\x1b[2;3m{body}\x1b[0m"))
        }
        ViewEvent::ToolCallStart { .. } | ViewEvent::ToolCallResult { .. } => {
            unreachable!("handled above")
        }
        ViewEvent::SlashOutput(text) => {
            let body = text.replace('\n', "\r\n");
            Some(format!("\x1b[2m{body}\x1b[0m\r\n"))
        }
        ViewEvent::TurnDone => None,
        ViewEvent::HistoryReplaced(messages) => {
            let mut out = String::from("\x1b[3J\x1b[2J\x1b[H");
            for m in messages {
                let line = match m.role.as_str() {
                    "user" => {
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
                    "assistant" => format!("{}\r\n", m.content.replace('\n', "\r\n")),
                    _ => format!("\x1b[2m{}\x1b[0m\r\n", m.content.replace('\n', "\r\n")),
                };
                out.push_str(&line);
            }
            Some(out)
        }
        ViewEvent::ErrorText(text) => Some(format!("\r\n\x1b[31m{text}\x1b[0m\r\n")),
        ViewEvent::SessionListRefresh(_) => None,
        ViewEvent::ProviderUpdate(_) => None,
        ViewEvent::KmsUpdate(_) => None,
        ViewEvent::McpUpdate(_) => None,
        ViewEvent::ModelPickerOpen(_) => None,
        ViewEvent::ContextWarning { file_size_mb } => Some(format!(
            "\r\n\x1b[33m[ session {:.1} MB — /fork to continue in a new session with summary ]\x1b[0m\r\n",
            file_size_mb
        )),
        ViewEvent::McpAppCallToolResult { .. } => None,
        ViewEvent::QuitRequested => None,
        ViewEvent::PlanUpdate(_) => None,
        ViewEvent::PermissionModeChanged(_) => None,
        ViewEvent::PlanStalled { .. } => None,
    };

    match inner {
        Some(text) => {
            state.last_tool_label = None;
            state.last_tool_count = 0;
            state.merging = false;
            if state.pending_newline_after_tool {
                state.pending_newline_after_tool = false;
                Some(format!("\r\n{text}"))
            } else {
                Some(text)
            }
        }
        None => None,
    }
}

/// Wrap an ANSI-bytes blob into the standard `terminal_data` envelope
/// (base64-encoded `data` field) that `TerminalView.tsx` writes
/// straight to xterm.
pub fn terminal_data_envelope(ansi: &str) -> String {
    let bytes = ansi.as_bytes();
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    serde_json::json!({"type": "terminal_data", "data": b64}).to_string()
}

/// Like `terminal_data_envelope` but the frontend handler always writes
/// a fresh prompt at the end — used for session load / new-session
/// events so an empty history doesn't leave the user staring at a
/// blank terminal with no chevron.
pub fn terminal_history_replaced_envelope(ansi: &str) -> String {
    let bytes = ansi.as_bytes();
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    serde_json::json!({"type": "terminal_history_replaced", "data": b64}).to_string()
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

#[cfg(test)]
mod chat_render_tests {
    use super::*;

    #[test]
    fn user_prompt_renders_chat_user_message_with_ansi_stripped() {
        let dispatches =
            render_chat_dispatches(&ViewEvent::UserPrompt("\x1b[2mfoo\x1b[0m bar".into()));
        assert_eq!(dispatches.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&dispatches[0]).unwrap();
        assert_eq!(parsed["type"], "chat_user_message");
        assert_eq!(parsed["text"], "foo bar");
    }

    #[test]
    fn turn_done_renders_chat_done() {
        let dispatches = render_chat_dispatches(&ViewEvent::TurnDone);
        assert_eq!(dispatches.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&dispatches[0]).unwrap();
        assert_eq!(parsed["type"], "chat_done");
    }
}
