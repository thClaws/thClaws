//! Claude Agent SDK provider — runs Claude Code as a local subprocess over
//! the Agent SDK control protocol.
//!
//! Spawns:
//!   `claude --output-format stream-json --input-format stream-json --verbose
//!    --permission-mode bypassPermissions --system-prompt <sys> [--model <m>]
//!    [--session-id <sid>]`
//!
//! Protocol (matches anthropics/claude-agent-sdk-python):
//!
//!   1. We send an `initialize` control_request on stdin:
//!        `{"type":"control_request","request_id":"req_<n>","request":{"subtype":"initialize","hooks":null}}`
//!   2. CLI replies with a matching `control_response` on stdout. We wait for
//!      it before anything else — otherwise the CLI ignores user input.
//!   3. We write a user message envelope on stdin:
//!        `{"type":"user","session_id":"","message":{"role":"user","content":"..."},"parent_tool_use_id":null}`
//!   4. We close stdin (no bidirectional hooks / SDK MCP servers, so we
//!      don't need it open past the first message).
//!   5. We stream stdout lines and parse events. Terminal event is
//!      `{"type":"result",...}` — emit MessageStop with usage.
//!
//! Model prefix: `agent/` (e.g. `agent/claude-sonnet-4-6`). Billing goes
//! through the user's Claude subscription — no ANTHROPIC_API_KEY required.
//!
//! Session persistence: we capture `session_id` from whichever frame
//! surfaces it first, store it in an `Arc<Mutex<Option<String>>>` owned by
//! the provider, and pass `--session-id <uuid>` on subsequent turns so the
//! CLI resumes the same conversation server-side.

use super::{EventStream, ModelInfo, Provider, ProviderEvent, StreamRequest};
use crate::error::{Error, Result};
use crate::tools::ToolRegistry;
use async_stream::try_stream;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdout, Command};

pub struct AgentSdkProvider {
    /// Path to the `claude` CLI binary.
    claude_bin: String,
    /// Reusable session ID for context persistence.
    session_id: Arc<Mutex<Option<String>>>,
    /// Monotonic request id counter for control_request messages.
    next_req: Arc<Mutex<u64>>,
    /// Tools to expose to the subprocess via the in-process SDK MCP
    /// server (`crate::sdk_mcp`). `None` → no bridge: built-ins stay
    /// enabled and the subprocess can't see thClaws's tools.
    tools: Option<Arc<ToolRegistry>>,
}

impl AgentSdkProvider {
    pub fn new() -> Self {
        let bin = std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
        Self {
            claude_bin: bin,
            session_id: Arc::new(Mutex::new(None)),
            next_req: Arc::new(Mutex::new(0)),
            tools: None,
        }
    }

    pub fn with_bin(mut self, bin: impl Into<String>) -> Self {
        self.claude_bin = bin.into();
        self
    }

    /// Wire up the SDK MCP bridge: Claude Code's built-in tools are
    /// disabled, thClaws's tools are exposed via in-process MCP.
    pub fn with_tools(mut self, tools: Arc<ToolRegistry>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Default bridge registry: every always-on tool registered in
    /// the standard built-in set plus the KMS + Memory + Todo
    /// surfaces. Excluded-from-bridge tools (Task, Team*, AskUser,
    /// plan-mode tools) are filtered at `sdk_mcp::handle_mcp_message`
    /// time. Used by `build_provider` so the bridge stands up
    /// without the caller threading their own registry.
    pub fn with_default_thclaws_tools() -> Arc<ToolRegistry> {
        let mut r = ToolRegistry::with_builtins();
        r.register(Arc::new(crate::tools::KmsReadTool));
        r.register(Arc::new(crate::tools::KmsSearchTool));
        r.register(Arc::new(crate::tools::KmsWriteTool));
        r.register(Arc::new(crate::tools::KmsAppendTool));
        r.register(Arc::new(crate::tools::KmsDeleteTool));
        r.register(Arc::new(crate::tools::KmsCreateTool));
        Arc::new(r)
    }

    fn next_request_id(&self) -> String {
        let mut n = self.next_req.lock().unwrap();
        *n += 1;
        // Random-ish suffix to avoid collisions across runs of the same
        // provider instance.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("req_{}_{:08x}", *n, nanos)
    }
}

impl Default for AgentSdkProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for AgentSdkProvider {
    async fn stream(&self, req: StreamRequest) -> Result<EventStream> {
        // Pull the user's latest turn. Prior history lives server-side under
        // --session-id, so we only send the new user message.
        let user_text = req
            .messages
            .last()
            .and_then(|m| {
                m.content.iter().find_map(|b| match b {
                    crate::types::ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
            })
            .unwrap_or_default();

        // Build the CLI command.
        let mut cmd = Command::new(&self.claude_bin);
        cmd.arg("--output-format")
            .arg("stream-json")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--verbose")
            // Don't block on permission prompts — thClaws is the surface the
            // user interacts with; let Claude just run.
            .arg("--permission-mode")
            .arg("bypassPermissions");

        // Write system prompt to a temp file to avoid OS ARG_MAX limits.
        // With MCP tools + CLAUDE.md + skills, the prompt can exceed 128KB
        // which hits MAX_ARG_STRLEN when passed as a CLI argument.
        // Uses tempfile for: unique name (no race), mode 0600 (no leak),
        // O_EXCL (no symlink attack). Kept alive until after spawn.
        let system = req.system.clone().unwrap_or_default();
        let _prompt_guard: Option<tempfile::TempPath> = if system.is_empty() {
            cmd.arg("--system-prompt").arg("");
            None
        } else {
            use std::io::Write as _;
            let mut tmp = tempfile::Builder::new()
                .prefix("thclaws-sdk-prompt-")
                .suffix(".txt")
                .tempfile()
                .map_err(|e| Error::Provider(format!("failed to create prompt temp file: {e}")))?;
            tmp.write_all(system.as_bytes()).map_err(|e| {
                Error::Provider(format!(
                    "failed to write system prompt file {}: {e}",
                    tmp.path().display()
                ))
            })?;
            let prompt_path = tmp.into_temp_path();
            cmd.arg("--system-prompt-file").arg(prompt_path.as_os_str());
            Some(prompt_path)
        };

        // Strip the `agent/` prefix for the actual model name.
        let model = req.model.strip_prefix("agent/").unwrap_or(&req.model);
        if !model.is_empty() {
            cmd.arg("--model").arg(model);
        }

        // Resume the previous session when we have one. `--resume <uuid>`
        // reattaches to an existing session; `--session-id` is for *setting*
        // a new session's id and errors with "Session ID is already in use"
        // if we re-pass it on turn 2.
        {
            let guard = self.session_id.lock().unwrap();
            if let Some(ref sid) = *guard {
                cmd.arg("--resume").arg(sid);
            }
        }

        // SDK MCP bridge. When a tool registry is attached, expose
        // thClaws's tools to the subprocess and pin the allowedTools
        // filter to ONLY those bridged names — Claude Code's
        // built-in tools (Read/Edit/Bash/etc) are disabled so the
        // model can't bypass thClaws's sandbox + hooks by using
        // them. Without the registry, this whole block is skipped
        // and Claude Code runs with its native tools.
        if let Some(tools) = &self.tools {
            let mcp_config = crate::sdk_mcp::mcp_config_value();
            cmd.arg("--mcp-config").arg(mcp_config.to_string());
            let patterns = crate::sdk_mcp::allowed_tool_patterns(tools);
            cmd.arg("--allowedTools").arg(patterns.join(","));
        }

        // Mirror the Python SDK env hygiene: identify ourselves via
        // CLAUDE_CODE_ENTRYPOINT, and scrub CLAUDECODE so the child doesn't
        // think it's nested inside another Claude Code session.
        cmd.env("CLAUDE_CODE_ENTRYPOINT", "sdk-thclaws");
        cmd.env_remove("CLAUDECODE");

        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Provider(format!("spawn claude: {e}")))?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Provider("no stdin handle".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Provider("no stdout handle".into()))?;
        let stderr = child.stderr.take();

        // Pipe child stderr to our stderr for visibility. Warnings about
        // outdated CLI version, auth problems, etc. surface here.
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while let Ok(n) = reader.read_line(&mut line).await {
                    if n == 0 {
                        break;
                    }
                    eprint!("\x1b[2m[claude] {}\x1b[0m", line);
                    line.clear();
                }
            });
        }

        let mut reader = BufReader::new(stdout);

        // ── 1. Send initialize ────────────────────────────────────────────
        let init_id = self.next_request_id();
        let init_req = json!({
            "type": "control_request",
            "request_id": init_id,
            "request": { "subtype": "initialize", "hooks": null }
        });
        stdin
            .write_all(init_req.to_string().as_bytes())
            .await
            .map_err(|e| Error::Provider(format!("write initialize: {e}")))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| Error::Provider(format!("write initialize: {e}")))?;

        // ── 2. Wait for initialize response ──────────────────────────────
        // Anything that arrives before the response is discarded — the CLI
        // shouldn't emit anything else prior to it, but we stay defensive.
        let mut ack_line = String::new();
        loop {
            ack_line.clear();
            let n = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                reader.read_line(&mut ack_line),
            )
            .await
            .map_err(|_| {
                Error::Provider(
                    "claude agent SDK: timed out waiting for initialize response. \
                     Is the claude CLI version current? (`claude --version`)"
                        .into(),
                )
            })?
            .map_err(|e| Error::Provider(format!("read initialize: {e}")))?;
            if n == 0 {
                return Err(Error::Provider(
                    "claude agent SDK: process exited before initialize response".into(),
                ));
            }
            let trimmed = ack_line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) != Some("control_response") {
                continue;
            }
            if v.pointer("/response/request_id").and_then(Value::as_str) == Some(&init_id) {
                break;
            }
        }

        // ── 3. Send user message ─────────────────────────────────────────
        let user_msg = json!({
            "type": "user",
            "session_id": "",
            "message": { "role": "user", "content": user_text },
            "parent_tool_use_id": null,
        });
        stdin
            .write_all(user_msg.to_string().as_bytes())
            .await
            .map_err(|e| Error::Provider(format!("write user message: {e}")))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| Error::Provider(format!("write user message: {e}")))?;

        // Keep stdin open when the SDK MCP bridge is wired up: Claude
        // Code sends `control_request { subtype: "mcp_message" }`
        // back to us for every `tools/list` and `tools/call`. Drop
        // stdin only when no bridge is configured — preserves the
        // pre-bridge behavior of letting the CLI commit the session
        // file on EOF.
        let stdin_for_stream = if self.tools.is_some() {
            Some(stdin)
        } else {
            drop(stdin);
            None
        };

        // ── 4. Stream stdout until `result` ──────────────────────────────
        let session_for_stream = self.session_id.clone();
        let tools_for_stream = self.tools.clone();
        let raw_dump = super::RawDump::new(format!("agent-sdk {}", req.model));
        let event_stream = try_stream! {
            // Hold the child handle so the subprocess is reaped when the
            // stream is dropped.
            let _child = child;
            let mut reader: BufReader<ChildStdout> = reader;
            let mut stdin_handle = stdin_for_stream;
            let bridge_tools = tools_for_stream;
            let mut line_buf = String::new();
            let mut seen_start = false;
            let mut first_text_yielded = false;
            let mut raw = raw_dump;
            let mut active_tools: std::collections::HashMap<String, crate::tool_display::ActiveToolDisplay> =
                std::collections::HashMap::new();
            // Fallback counter for tool_use blocks the CLI emits without an id.
            let mut anon_counter: u32 = 0;
            loop {
                line_buf.clear();

                let heartbeat_delay = if !active_tools.is_empty() {
                    crate::tool_display::next_heartbeat_delay(&active_tools)
                } else {
                    std::time::Duration::from_secs(300)
                };

                let got_line: std::result::Result<Option<usize>, std::io::Error> =
                    tokio::select! {
                        biased;
                        result = reader.read_line(&mut line_buf) => { result.map(Some) }
                        _ = tokio::time::sleep(heartbeat_delay) => { Ok(None) }
                    };
                let got_line = got_line
                    .map_err(|e| Error::Provider(format!("read stdout: {e}")))?;

                if let Some(0) = got_line { break; } // EOF

                if got_line.is_none() {
                    yield ProviderEvent::Progress(super::ProgressKind::Thinking);
                    continue;
                }

                let trimmed = line_buf.trim();
                if trimmed.is_empty() { continue; }
                let Ok(v) = serde_json::from_str::<Value>(trimmed) else { continue };
                let msg_type = v.get("type").and_then(Value::as_str).unwrap_or("");

                // Capture the session id the first time it appears so the
                // next turn can pass --session-id <uuid>.
                if let Some(sid) = v.get("session_id").and_then(Value::as_str) {
                    if !sid.is_empty() {
                        *session_for_stream.lock().unwrap() = Some(sid.to_string());
                    }
                }

                if !seen_start {
                    yield ProviderEvent::MessageStart { model: req.model.clone() };
                    seen_start = true;
                }

                match msg_type {
                    // Full assistant message (claude -p emits one per reply,
                    // not per token). Content is an array of typed blocks.
                    "assistant" => {
                        if let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) {
                            for block in blocks {
                                let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
                                match btype {
                                    "text" => {
                                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                                            if !text.is_empty() {
                                                raw.push(text);
                                                let s = if first_text_yielded {
                                                    format!("\n\n{text}")
                                                } else {
                                                    first_text_yielded = true;
                                                    text.to_string()
                                                };
                                                yield ProviderEvent::TextDelta(s);
                                            }
                                        }
                                    }
                                    "tool_use" => {
                                        let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                                        let id = block.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                                        let input = block.get("input").cloned().unwrap_or(Value::Null);
                                        let label = crate::tool_display::tool_label(name, &input);
                                        let tracking_id = if id.is_empty() {
                                            anon_counter += 1;
                                            format!("__anon_{anon_counter}")
                                        } else {
                                            id
                                        };
                                        yield ProviderEvent::Progress(super::ProgressKind::ToolStart {
                                            id: tracking_id.clone(),
                                            label: label.clone(),
                                        });
                                        active_tools.insert(
                                            tracking_id,
                                            crate::tool_display::ActiveToolDisplay::new(label),
                                        );
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    "user" => {
                        if let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) {
                            for block in blocks {
                                let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
                                if btype == "tool_result" {
                                    let tu_id = block.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                                    let is_error = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                                    if let Some(td) = active_tools.remove(tu_id) {
                                        yield ProviderEvent::Progress(super::ProgressKind::ToolDone {
                                            id: tu_id.to_string(),
                                            label: td.label.clone(),
                                            is_error,
                                        });
                                    }
                                }
                            }
                        }
                        // no-op: active_tools draining handled above
                    }
                    "control_request" => {
                        let req_id = v.get("request_id").and_then(Value::as_str).unwrap_or("").to_string();
                        let subtype = v.pointer("/request/subtype").and_then(Value::as_str).unwrap_or("");
                        if subtype == "mcp_message" {
                            let server_name = v.pointer("/request/server_name").and_then(Value::as_str).unwrap_or("");
                            if server_name == crate::sdk_mcp::SERVER_NAME {
                                let mcp_msg = v.pointer("/request/message").cloned().unwrap_or(Value::Null);
                                let mcp_resp = match &bridge_tools {
                                    Some(reg) => crate::sdk_mcp::handle_mcp_message(reg.clone(), &mcp_msg).await,
                                    None => json!({
                                        "jsonrpc":"2.0",
                                        "id": mcp_msg.get("id").cloned().unwrap_or(Value::Null),
                                        "error": { "code": -32601, "message": "no tools registry attached" },
                                    }),
                                };
                                let envelope = json!({
                                    "type": "control_response",
                                    "response": {
                                        "subtype": "success",
                                        "request_id": req_id,
                                        "response": { "mcp_response": mcp_resp },
                                    },
                                });
                                if let Some(stdin) = stdin_handle.as_mut() {
                                    if let Err(e) = stdin.write_all(envelope.to_string().as_bytes()).await {
                                        eprintln!("[agent-sdk] mcp bridge: stdin write failed: {e}");
                                    } else {
                                        let _ = stdin.write_all(b"\n").await;
                                        let _ = stdin.flush().await;
                                    }
                                }
                            }
                        }
                    }
                    "control_response" => {}
                    // Terminal frame: the turn is done.
                    "result" => {
                        let usage = v.get("usage").map(|u| super::Usage {
                            input_tokens: u.get("input_tokens")
                                .and_then(Value::as_u64).unwrap_or(0) as u32,
                            output_tokens: u.get("output_tokens")
                                .and_then(Value::as_u64).unwrap_or(0) as u32,
                            cache_creation_input_tokens: u.get("cache_creation_input_tokens")
                                .and_then(Value::as_u64).map(|v| v as u32),
                            cache_read_input_tokens: u.get("cache_read_input_tokens")
                                .and_then(Value::as_u64).map(|v| v as u32),
                            reasoning_output_tokens: None,
                        });
                        yield ProviderEvent::MessageStop {
                            stop_reason: Some("end_turn".into()),
                            usage,
                        };
                        break;
                    }
                    // Known non-terminal events we don't need to surface.
                    "system" | "rate_limit_event" | "keep_alive"
                    | "stream_event" | "task_started" | "task_progress"
                    | "task_notification" => {}
                    _ => {}
                }
            }

            // Stream closed without a `result` frame — emit a stop anyway so
            // the agent's turn doesn't hang.
            if seen_start {
                yield ProviderEvent::MessageStop {
                    stop_reason: Some("end_turn".into()),
                    usage: None,
                };
            }
        };

        Ok(Box::pin(event_stream))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
            let anthropic = crate::providers::anthropic::AnthropicProvider::new(api_key);
            let mut models = anthropic.list_models().await?;
            for m in &mut models {
                m.id = format!("agent/{}", m.id);
                if let Some(ref name) = m.display_name {
                    m.display_name = Some(format!("{} (Agent SDK)", name));
                }
            }
            Ok(models)
        } else {
            Err(crate::error::Error::Provider(
                "set ANTHROPIC_API_KEY to list models (or hard-code a `agent/<name>` in settings)"
                    .into(),
            ))
        }
    }

    /// Expose the captured Claude-side session UUID so the worker can
    /// persist it to the session JSONL after every turn. Pre-fix this
    /// value only lived in the `Arc<Mutex<>>` for the lifetime of the
    /// provider instance — process restart or `/load` lost it and the
    /// SDK started a fresh conversation that saw only the latest
    /// user message.
    fn provider_session_id(&self) -> Option<String> {
        self.session_id.lock().ok().and_then(|g| g.clone())
    }

    /// Rehydrate the Claude-side session UUID from a previously
    /// persisted value (typically a freshly-loaded session JSONL).
    /// The next `stream()` call will pass `--resume <uuid>` to the
    /// subprocess and the SDK restores its server-side history.
    /// `None` clears the slot (used on provider switch or explicit
    /// reset).
    fn set_provider_session_id(&self, id: Option<String>) {
        if let Ok(mut g) = self.session_id.lock() {
            *g = id;
        }
    }
}
