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
                    "allow_same_origin": ui.allow_same_origin,
                    "auto_size": ui.auto_size,
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
        ViewEvent::LineStatus(json) => vec![json.clone()],
        ViewEvent::TelegramStatus(json) => vec![json.clone()],
        ViewEvent::MessengerStatus(json) => vec![json.clone()],
        ViewEvent::ResearchUpdate(json) => vec![json.clone()],
        ViewEvent::ModelPickerOpen(json) => vec![json.clone()],
        ViewEvent::ScheduleAddOpen(json) => vec![json.clone()],
        ViewEvent::ContextWarning { file_size_mb } => vec![serde_json::json!({
            "type": "chat_context_warning",
            "file_size_mb": file_size_mb,
        })
        .to_string()],
        ViewEvent::ErrorText(text) => {
            // Distinct envelope so the chat UI can render a red-bordered
            // error bubble — previously these were folded into
            // `chat_text_delta` and appeared as ordinary assistant text,
            // so users couldn't tell a 429/auth-failure from an actual
            // response. Provider-shaped errors are humanized server-side
            // (OpenRouter's `error.metadata.raw`, OpenAI's
            // `error.message`); free-form text passes through unchanged.
            let cleaned = strip_ansi(text);
            let humanized = crate::providers::humanize_provider_error(&cleaned);
            vec![serde_json::json!({
                "type": "chat_error",
                "text": humanized,
            })
            .to_string()]
        }
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
        ViewEvent::TodoUpdate(todos) => {
            // TodoSidebar consumes this to render the live checklist.
            // Empty `todos` is meaningful (collapses the sidebar to a
            // chevron tab); the frontend distinguishes "no todos yet"
            // from "todos cleared explicitly" by tracking whether any
            // update has been received.
            let payload = serde_json::json!({
                "type": "chat_todo_update",
                "todos": todos,
            });
            vec![payload.to_string()]
        }
        ViewEvent::SkillModelNote(text) => {
            // ChatView renders this inline as a muted system note so
            // the user sees model-swap decisions in context without
            // a popup or sidebar.
            let payload = serde_json::json!({
                "type": "chat_skill_model_note",
                "text": text,
            });
            vec![payload.to_string()]
        }
        ViewEvent::GoalUpdate(goal) => {
            // Phase A: sidebar refresh whenever /goal mutates. Goal is
            // serialized as the full GoalState shape — frontend reads
            // objective, status, tokens_used / budget_tokens,
            // iterations_done, time_used (computed from started_at).
            let payload = serde_json::json!({
                "type": "chat_goal_update",
                "goal": goal,
            });
            vec![payload.to_string()]
        }
        ViewEvent::PermissionModeChanged(mode) => {
            let mode_str = match mode {
                crate::permissions::PermissionMode::Auto => "auto",
                crate::permissions::PermissionMode::Ask => "ask",
                crate::permissions::PermissionMode::Plan => "plan",
                crate::permissions::PermissionMode::LineGated => "linegated",
                crate::permissions::PermissionMode::TelegramGated => "telegramgated",
                crate::permissions::PermissionMode::MessengerGated => "messengergated",
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
        ViewEvent::SideChannelStart { id, agent_name } => vec![serde_json::json!({
            "type": "chat_side_channel_start",
            "id": id,
            "agent_name": agent_name,
        })
        .to_string()],
        ViewEvent::SideChannelTextDelta { id, text } => vec![serde_json::json!({
            "type": "chat_side_channel_text_delta",
            "id": id,
            "text": text,
        })
        .to_string()],
        ViewEvent::SideChannelToolCall {
            id,
            tool_name,
            label,
        } => vec![serde_json::json!({
            "type": "chat_side_channel_tool_call",
            "id": id,
            "tool_name": tool_name,
            "label": label,
        })
        .to_string()],
        ViewEvent::SideChannelDone {
            id,
            agent_name,
            duration_ms,
            result_text,
        } => vec![serde_json::json!({
            "type": "chat_side_channel_done",
            "id": id,
            "agent_name": agent_name,
            "duration_ms": duration_ms,
            "result_text": result_text,
        })
        .to_string()],
        ViewEvent::SideChannelError { id, error } => vec![serde_json::json!({
            "type": "chat_side_channel_error",
            "id": id,
            "error": error,
        })
        .to_string()],
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
    /// `true` when the most recent emitted bytes were dim-italic
    /// reasoning. The next non-thinking event prepends a `\r\n` so the
    /// final answer (or tool call) starts on a fresh line instead of
    /// running into the reasoning text. Cleared by any non-thinking
    /// emission.
    last_was_thinking: bool,
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
            // Tool call output already starts with \r\n, so any prior
            // thinking is naturally separated. Clear the flag so the
            // next text delta doesn't add a redundant blank line.
            state.last_was_thinking = false;
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
        ViewEvent::ToolCallResult { output, .. } => {
            state.last_was_thinking = false;
            // M6.38.9: surface upstream source (e.g. "via Tavily")
            // next to the ✓ when the tool emits a `Source: <engine>`
            // line. Independent of whether the model surfaces it.
            let src_suffix = crate::tools::extract_tool_source(output)
                .map(|s| format!(" \x1b[2m(via {s})\x1b[0m"))
                .unwrap_or_default();
            if state.merging {
                state.merging = false;
                state.last_tool_count += 1;
                state.pending_newline_after_tool = true;
                let label = state.last_tool_label.clone().unwrap_or_default();
                let count = state.last_tool_count;
                return Some(format!(
                    "\r\x1b[2K\x1b[2m[tool: {label}]\x1b[0m \x1b[32m✓\x1b[0m{src_suffix} \x1b[2m×{count}\x1b[0m"
                ));
            }
            state.last_tool_count = 1;
            state.pending_newline_after_tool = true;
            return Some(format!(" \x1b[32m✓\x1b[0m{src_suffix}"));
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
            for (i, m) in messages.iter().enumerate() {
                let line = match m.role.as_str() {
                    "user" => {
                        // Prepend a blank line before every user
                        // message except the first — restored history
                        // can be a wall of tool indicators between
                        // turns and the gap makes conversation
                        // boundaries scannable. The very first
                        // message doesn't need it (no scroll above).
                        let lead = if i == 0 { "" } else { "\r\n" };
                        let marker = "\x1b[2m> \x1b[0m";
                        let indent = "  ";
                        let mut lines = m.content.split('\n');
                        let mut body = String::from(lead);
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
        ViewEvent::LineStatus(_) => None,
        ViewEvent::TelegramStatus(_) => None,
        ViewEvent::MessengerStatus(_) => None,
        ViewEvent::ResearchUpdate(_) => None,
        ViewEvent::ModelPickerOpen(_) => None,
        ViewEvent::ScheduleAddOpen(_) => None,
        ViewEvent::ContextWarning { file_size_mb } => Some(format!(
            "\r\n\x1b[33m[ session {:.1} MB — /fork to continue in a new session with summary ]\x1b[0m\r\n",
            file_size_mb
        )),
        ViewEvent::McpAppCallToolResult { .. } => None,
        ViewEvent::QuitRequested => None,
        ViewEvent::PlanUpdate(_) => None,
        ViewEvent::TodoUpdate(_) => None,
        ViewEvent::SkillModelNote(text) => Some(format!("\r\n\x1b[2;3m{text}\x1b[0m\r\n")),
        ViewEvent::GoalUpdate(_) => None,
        ViewEvent::PermissionModeChanged(_) => None,
        ViewEvent::PlanStalled { .. } => None,
        // Side-channel events surface only on chat-shaped renderer.
        // CLI REPL / terminal pane gets a one-line ANSI marker for
        // start + done so users running thclaws --cli still see
        // background-agent activity without a custom renderer. Text
        // deltas and intermediate tool calls are dropped on terminal
        // — too noisy without a separate panel.
        ViewEvent::SideChannelStart { id, agent_name } => Some(format!(
            "\r\n\x1b[2m[agent {agent_name} ({id}) — running in background]\x1b[0m\r\n"
        )),
        ViewEvent::SideChannelTextDelta { .. } => None,
        ViewEvent::SideChannelToolCall { .. } => None,
        ViewEvent::SideChannelDone {
            id,
            agent_name,
            duration_ms,
            result_text,
        } => {
            let secs = *duration_ms as f64 / 1000.0;
            // Two-line emit: status header + result body. Result is
            // displayed in dim italic so it's distinguishable from
            // main agent's stream. Long results stay on a single
            // panel — user can grep terminal scrollback.
            let body = result_text.replace('\n', "\r\n");
            Some(format!(
                "\r\n\x1b[36m[agent {agent_name} ({id}) ✓ done in {secs:.2}s]\x1b[0m\r\n\
                 \x1b[2;3m{body}\x1b[0m\r\n"
            ))
        }
        ViewEvent::SideChannelError { id, error } => Some(format!(
            "\r\n\x1b[31m[agent {id} ✗ {error}]\x1b[0m\r\n"
        )),
    };

    match inner {
        Some(text) => {
            state.last_tool_label = None;
            state.last_tool_count = 0;
            state.merging = false;
            // Track whether this emission was reasoning so the NEXT
            // non-thinking event can prepend a newline. Done before the
            // pending_newline injection so the new flag reflects what
            // we're actually about to write.
            let is_thinking = matches!(ev, ViewEvent::AssistantThinkingDelta(_));
            let needs_thinking_break = state.last_was_thinking && !is_thinking;
            state.last_was_thinking = is_thinking;
            let mut prefix = String::new();
            if state.pending_newline_after_tool {
                state.pending_newline_after_tool = false;
                prefix.push_str("\r\n");
            }
            if needs_thinking_break {
                // Reasoning just ended; the assistant's actual answer
                // (or tool call, or slash output) starts on a fresh
                // line so the dim italic doesn't run into normal text.
                prefix.push_str("\r\n");
            }
            if prefix.is_empty() {
                Some(text)
            } else {
                Some(format!("{prefix}{text}"))
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

    #[test]
    fn error_text_renders_humanized_chat_error() {
        let raw = r#"Error: provider error: http 429 Too Many Requests: {"error":{"message":"Provider returned error","code":429,"metadata":{"raw":"foo is temporarily rate-limited upstream."}}}"#;
        let dispatches = render_chat_dispatches(&ViewEvent::ErrorText(raw.into()));
        assert_eq!(dispatches.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&dispatches[0]).unwrap();
        assert_eq!(parsed["type"], "chat_error");
        assert_eq!(
            parsed["text"],
            "Rate limited: foo is temporarily rate-limited upstream."
        );
    }

    #[test]
    fn error_text_falls_back_to_raw_when_unparseable() {
        let raw = "Error: agent error: tool dispatcher panicked";
        let dispatches = render_chat_dispatches(&ViewEvent::ErrorText(raw.into()));
        assert_eq!(dispatches.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&dispatches[0]).unwrap();
        assert_eq!(parsed["type"], "chat_error");
        // No JSON to extract → original text passes through unchanged.
        assert_eq!(parsed["text"], raw);
    }

    /// Restored chat history is rendered into the terminal as one
    /// linear ANSI string. Each user message after the first should
    /// start with a blank line so conversation turns are visually
    /// separated from the dim tool / assistant rows between them.
    #[test]
    fn history_replaced_blank_line_before_user_messages() {
        use crate::shared_session::DisplayMessage;
        let mut state = TerminalRenderState::default();
        let msgs = vec![
            DisplayMessage {
                role: "user".into(),
                content: "first prompt".into(),
            },
            DisplayMessage {
                role: "assistant".into(),
                content: "ok".into(),
            },
            DisplayMessage {
                role: "tool".into(),
                content: "Bash".into(),
            },
            DisplayMessage {
                role: "user".into(),
                content: "follow-up".into(),
            },
        ];
        let out = render_terminal_ansi(&mut state, &ViewEvent::HistoryReplaced(msgs))
            .expect("HistoryReplaced should produce ANSI");
        // First user message: no leading blank line (it follows the
        // clear-screen escapes). Second user message: leading \r\n
        // before the `> ` marker.
        let stripped = strip_ansi(&out);
        assert!(stripped.contains("> first prompt"));
        assert!(
            stripped.contains("\r\n\r\n> follow-up"),
            "expected blank line before second user prompt; got: {stripped:?}"
        );
        // No double-blank before the FIRST user message — that would
        // look weird at the very top of restored scrollback.
        assert!(
            !stripped.contains("\r\n\r\n> first prompt"),
            "first user prompt should not have a leading blank line; got: {stripped:?}"
        );
    }
}
