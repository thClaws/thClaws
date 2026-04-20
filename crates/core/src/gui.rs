//! Desktop GUI mode: wry webview serving embedded React frontend + PTY bridge.
//!
//! The React dist/ is embedded at compile time via `include_dir!` and served
//! via wry's custom protocol (`thclaws://`). The PTY bridge spawns `thclaws`
//! (this same binary, without `--gui`) as a child process and bridges
//! keystrokes / output between the webview and the PTY.
//!
//! Only compiled when the `gui` feature is enabled.

#![cfg(feature = "gui")]

use crate::agent::{Agent, AgentEvent};
use crate::config::AppConfig;
use crate::context::ProjectContext;
use crate::memory::MemoryStore;
use crate::repl::build_provider;
use crate::session::SessionStore;
use crate::tools::ToolRegistry;
use base64::Engine;
use futures::StreamExt;
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

/// Embed the single-file React frontend (JS+CSS inlined by vite-plugin-singlefile).
const FRONTEND_HTML: &str = include_str!("../../../frontend/dist/index.html");

enum UserEvent {
    PtyData(String),
    PtyExit,
    SendInitialState,
    ChatTextDelta(String),
    ChatToolCall(String),
    ChatToolResult(String, String),
    ChatDone,
    SessionLoaded(String),
    SessionListRefresh(String),
    FileTree(String),
    FileContent(String),
}

enum ChatCommand {
    Prompt(String),
    LoadHistory(Vec<crate::types::Message>),
    NewSession,
    SaveAndQuit,
}

struct PtyBridge {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl PtyBridge {
    fn spawn(cmd: &str, args: &[&str], cols: u16, rows: u16) -> Result<Self, String> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty: {e}"))?;
        let mut builder = CommandBuilder::new(cmd);
        for a in args {
            builder.arg(*a);
        }
        // Inherit parent's cwd + full environment so the child process
        // finds ~/.config/thclaws/settings.json, .env files, and PATH.
        if let Ok(cwd) = std::env::current_dir() {
            builder.cwd(cwd);
        }
        for (key, val) in std::env::vars() {
            builder.env(key, val);
        }
        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| format!("spawn: {e}"))?;
        drop(pair.slave);
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("writer: {e}"))?;
        Ok(Self {
            master: pair.master,
            writer,
            child,
        })
    }

    fn start_reader(
        &self,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<thread::JoinHandle<()>, String> {
        let mut reader = self
            .master
            .try_clone_reader()
            .map_err(|e| format!("reader: {e}"))?;
        Ok(thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let encoded = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
                        let _ = proxy.send_event(UserEvent::PtyData(encoded));
                    }
                    Err(_) => break,
                }
            }
            let _ = proxy.send_event(UserEvent::PtyExit);
        }))
    }

    fn write(&mut self, data: &[u8]) {
        let _ = self.writer.write_all(data);
        let _ = self.writer.flush();
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

const MAX_RECENT_DIRS: usize = 3;

fn recent_dirs_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(std::path::PathBuf::from(home).join(".config/thclaws/recent_dirs.json"))
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

/// Open a native OS directory picker dialog. Returns the selected path or
/// `None` if the user cancelled. No extra crate dependency — shells out to
/// the platform's built-in dialog tool.
fn pick_directory_native(start_dir: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "POSIX path of (choose folder with prompt \"Select working directory\" \
             default location POSIX file \"{}\")",
            start_dir.replace('\\', "\\\\").replace('"', "\\\"")
        );
        let out = std::process::Command::new("osascript")
            .args(["-e", &script])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let path = path.trim_end_matches('/').to_string();
        if path.is_empty() {
            None
        } else {
            Some(path)
        }
    }
    #[cfg(target_os = "linux")]
    {
        let out = std::process::Command::new("zenity")
            .args([
                "--file-selection",
                "--directory",
                "--title=Select working directory",
                &format!("--filename={}/", start_dir),
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if path.is_empty() {
            None
        } else {
            Some(path)
        }
    }
    #[cfg(target_os = "windows")]
    {
        let ps_start = start_dir.replace('\'', "''");
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!(
                "[System.Reflection.Assembly]::LoadWithPartialName('System.Windows.Forms') | Out-Null; \
                 $d = New-Object System.Windows.Forms.FolderBrowserDialog; \
                 $d.Description = 'Select working directory'; \
                 $d.SelectedPath = '{ps_start}'; \
                 if ($d.ShowDialog() -eq 'OK') {{ $d.SelectedPath }} else {{ '' }}")])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if path.is_empty() {
            None
        } else {
            Some(path)
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

fn child_command() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "thclaws".to_string())
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
        "global" => {
            let home = std::env::var("HOME").ok()?;
            Some(std::path::PathBuf::from(home).join(".config/thclaws/AGENTS.md"))
        }
        _ => std::env::current_dir().ok().map(|d| d.join("AGENTS.md")),
    }
}

/// Build the `kms_update` IPC payload: every discoverable KMS tagged with
/// whether it's currently attached to this project.
fn build_kms_update_payload() -> serde_json::Value {
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

pub fn run_gui() {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let (win_w, win_h) = crate::config::ProjectConfig::load()
        .map(|c| {
            (
                c.window_width.unwrap_or(1760.0),
                c.window_height.unwrap_or(962.0),
            )
        })
        .unwrap_or((1760.0, 962.0));
    let window = WindowBuilder::new()
        .with_title("thClaws")
        .with_inner_size(LogicalSize::new(win_w, win_h))
        .build(&event_loop)
        .expect("window build");

    let bridge: Arc<Mutex<Option<PtyBridge>>> = Arc::new(Mutex::new(None));
    let bridge_for_ipc = bridge.clone();
    // Flag: true when the PTY was killed intentionally (directory change).
    // Suppresses the PtyExit → ControlFlow::Exit path so the window stays.
    let pty_restart = Arc::new(AtomicBool::new(false));
    let pty_restart_for_ipc = pty_restart.clone();
    let proxy_for_ipc = proxy.clone();
    let cmd = child_command();
    let cmd_for_ipc = cmd.clone();

    // Chat mode: background tokio runtime + agent. Prompts arrive via channel.
    let (chat_tx, chat_rx) = std::sync::mpsc::channel::<ChatCommand>();
    let proxy_for_chat = proxy.clone();
    std::thread::spawn(move || {
        // Catch panics so a bug in the agent / provider stream doesn't take
        // the whole GUI window down with it. We surface the panic message in
        // the chat pane so the user sees what happened.
        let proxy_panic = proxy_for_chat.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(async move {
                let config = AppConfig::load().unwrap_or_default();
                let cwd = std::env::current_dir().unwrap_or_default();
                let ctx = ProjectContext::discover(&cwd).unwrap_or(ProjectContext {
                    cwd: cwd.clone(),
                    git: None,
                    project_instructions: None,
                });
                let system_fallback = if config.system_prompt.is_empty() {
                    crate::prompts::defaults::SYSTEM
                } else {
                    config.system_prompt.as_str()
                };
                let base_prompt = crate::prompts::load("system", system_fallback);
                let mut system = ctx.build_system_prompt(&base_prompt);
                if let Some(store) = MemoryStore::default_path().map(MemoryStore::new) {
                    if let Some(mem) = store.system_prompt_section() {
                        system.push_str("\n\n# Memory\n");
                        system.push_str(&mem);
                    }
                }
                let kms_section = crate::kms::system_prompt_section(&config.kms_active);
                if !kms_section.is_empty() {
                    system.push_str("\n\n");
                    system.push_str(&kms_section);
                }

                let provider = match build_provider(&config) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = proxy_for_chat
                            .send_event(UserEvent::ChatTextDelta(format!("Provider error: {e}")));
                        let _ = proxy_for_chat.send_event(UserEvent::ChatDone);
                        return;
                    }
                };

                let mut tools = ToolRegistry::with_builtins();
                if !config.kms_active.is_empty() {
                    tools.register(std::sync::Arc::new(crate::tools::KmsReadTool));
                    tools.register(std::sync::Arc::new(crate::tools::KmsSearchTool));
                }
                // Register skills + surface them in the system prompt so the Chat
                // tab agent knows what's available (same as the Terminal REPL).
                let skill_store = crate::skills::SkillStore::discover();
                if !skill_store.skills.is_empty() {
                    system.push_str("\n\n# Available skills (MANDATORY usage)\n");
                    system.push_str(
                        "The `Skill` tool loads expert instructions for a bundled workflow. \
                     If a user request matches the trigger criteria of any skill below, \
                     you MUST:\n\
                     1. Call `Skill(name: \"<skill-name>\")` FIRST — before any Bash, \
                        Write, Edit, or other tool calls for that task.\n\
                     2. Follow the instructions returned by that skill for the rest of \
                        the task. They override your default approach.\n\
                     3. Announce the skill at the start of your reply, e.g. \
                        \"Using the `pdf` skill to …\".\n\
                     Do NOT implement the task yourself when a matching skill exists — \
                     the skill encodes conventions and scripts you don't have built in.\n\n",
                    );
                    let mut entries: Vec<&crate::skills::SkillDef> =
                        skill_store.skills.values().collect();
                    entries.sort_by(|a, b| a.name.cmp(&b.name));
                    for skill in entries {
                        system.push_str(&format!("- **{}** — {}", skill.name, skill.description));
                        if !skill.when_to_use.is_empty() {
                            system.push_str(&format!("\n  Trigger: {}", skill.when_to_use));
                        }
                        system.push('\n');
                    }
                    tools.register(std::sync::Arc::new(crate::skills::SkillTool::new(
                        skill_store,
                    )));
                }
                let agent = Agent::new(provider, tools, &config.model, &system);

                let session_store = crate::session::SessionStore::default_path()
                    .map(crate::session::SessionStore::new);
                let mut current_session =
                    crate::session::Session::new(&config.model, cwd.to_string_lossy());

                while let Ok(cmd) = chat_rx.recv() {
                    let prompt = match cmd {
                        ChatCommand::Prompt(p) => p,
                        ChatCommand::LoadHistory(msgs) => {
                            agent.set_history(msgs);
                            continue;
                        }
                        ChatCommand::NewSession => {
                            let history = agent.history_snapshot();
                            if !history.is_empty() {
                                current_session.sync(history);
                                if let Some(ref store) = session_store {
                                    let _ = store.save(&mut current_session);
                                }
                            }
                            agent.clear_history();
                            current_session =
                                crate::session::Session::new(&config.model, cwd.to_string_lossy());
                            // Broadcast updated session list.
                            let list = build_session_list(&session_store);
                            let _ = proxy_for_chat.send_event(UserEvent::SessionListRefresh(list));
                            continue;
                        }
                        ChatCommand::SaveAndQuit => {
                            let history = agent.history_snapshot();
                            if !history.is_empty() {
                                current_session.sync(history);
                                if let Some(ref store) = session_store {
                                    let _ = store.save(&mut current_session);
                                }
                            }
                            break;
                        }
                    };
                    let mut stream = Box::pin(agent.run_turn(prompt));
                    while let Some(ev) = stream.next().await {
                        match ev {
                            Ok(AgentEvent::Text(s)) => {
                                let _ = proxy_for_chat.send_event(UserEvent::ChatTextDelta(s));
                            }
                            Ok(AgentEvent::ToolCallStart { name, input, .. }) => {
                                // Annotate the tool name with a short detail for a
                                // few high-signal tools so the user can see which
                                // skill / sub-agent / path is being used — not
                                // just "Skill".
                                let detail = match name.as_str() {
                                    "Skill" => input
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .map(|n| format!("({n})")),
                                    "Task" => input
                                        .get("agent")
                                        .and_then(|v| v.as_str())
                                        .map(|a| format!("(agent={a})")),
                                    "Bash" => {
                                        input.get("command").and_then(|v| v.as_str()).map(|c| {
                                            let first: String = c.chars().take(40).collect();
                                            format!(
                                                "({first}{})",
                                                if c.chars().count() > 40 { "…" } else { "" }
                                            )
                                        })
                                    }
                                    "Read" | "Write" | "Edit" => input
                                        .get("path")
                                        .and_then(|v| v.as_str())
                                        .map(|p| format!("({p})")),
                                    "Grep" | "Glob" => input
                                        .get("pattern")
                                        .and_then(|v| v.as_str())
                                        .map(|p| format!("({p})")),
                                    "WebFetch" => {
                                        input.get("url").and_then(|v| v.as_str()).map(|u| {
                                            format!("({})", u.chars().take(60).collect::<String>())
                                        })
                                    }
                                    "WebSearch" => input
                                        .get("query")
                                        .and_then(|v| v.as_str())
                                        .map(|q| format!("({q})")),
                                    _ => None,
                                }
                                .unwrap_or_default();
                                let label = if detail.is_empty() {
                                    name
                                } else {
                                    format!("{name} {detail}")
                                };
                                let _ = proxy_for_chat.send_event(UserEvent::ChatToolCall(label));
                            }
                            Ok(AgentEvent::ToolCallResult { name, output, .. }) => {
                                let out = output.unwrap_or_else(|e| e);
                                let _ =
                                    proxy_for_chat.send_event(UserEvent::ChatToolResult(name, out));
                            }
                            Ok(AgentEvent::Done { .. }) => {
                                // Auto-save session after each turn.
                                let history = agent.history_snapshot();
                                if !history.is_empty() {
                                    current_session.sync(history);
                                    if let Some(ref store) = session_store {
                                        let _ = store.save(&mut current_session);
                                    }
                                    // Broadcast updated session list.
                                    let list = build_session_list(&session_store);
                                    let _ = proxy_for_chat
                                        .send_event(UserEvent::SessionListRefresh(list));
                                }
                                let _ = proxy_for_chat.send_event(UserEvent::ChatDone);
                            }
                            Err(e) => {
                                let _ = proxy_for_chat
                                    .send_event(UserEvent::ChatTextDelta(format!("\nError: {e}")));
                                let _ = proxy_for_chat.send_event(UserEvent::ChatDone);
                            }
                            _ => {}
                        }
                    }
                }
            });
        }));
        if let Err(panic) = result {
            let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = panic.downcast_ref::<String>() {
                s.clone()
            } else {
                "agent thread panicked".to_string()
            };
            eprintln!("\x1b[31m[chat agent panicked: {msg}]\x1b[0m");
            let _ = proxy_panic.send_event(UserEvent::ChatTextDelta(format!(
                "\n\n⚠ chat agent crashed: {msg}\nrestart the app to recover."
            )));
            let _ = proxy_panic.send_event(UserEvent::ChatDone);
        }
    });
    let chat_tx_for_ipc = chat_tx.clone();
    let chat_tx_for_events = chat_tx;

    let webview = WebViewBuilder::new()
        .with_html(FRONTEND_HTML)
        .with_ipc_handler(move |req| {
            let body = req.body();
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(body) else {
                return;
            };
            let ty = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");

            match ty {
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
                "set_cwd" => {
                    if let Some(path) = msg.get("path").and_then(|v| v.as_str()) {
                        let p = std::path::Path::new(path);
                        if p.is_dir() {
                            let _ = std::env::set_current_dir(p);
                            let _ = crate::sandbox::Sandbox::init();
                            save_recent_dir(path);
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
                "pty_spawn" => {
                    let cols = msg.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
                    let rows = msg.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
                    if let Ok(pty) = PtyBridge::spawn(&cmd_for_ipc, &["--cli"], cols, rows) {
                        let _ = pty.start_reader(proxy_for_ipc.clone());
                        *bridge_for_ipc.lock().unwrap() = Some(pty);
                    }
                    // Send initial sidebar state after PTY is up.
                    let _ = proxy_for_ipc.send_event(UserEvent::SendInitialState);
                }
                "pty_write" => {
                    if let Some(data_b64) = msg.get("data").and_then(|v| v.as_str()) {
                        if let Ok(bytes) =
                            base64::engine::general_purpose::STANDARD.decode(data_b64)
                        {
                            if let Some(ref mut pty) = *bridge_for_ipc.lock().unwrap() {
                                pty.write(&bytes);
                            }
                        }
                    }
                }
                "pty_kill" => {
                    // Kill the current PTY child so the user can change
                    // directory via the startup modal and spawn a fresh one.
                    pty_restart_for_ipc.store(true, Ordering::SeqCst);
                    if let Some(ref mut pty) = bridge_for_ipc.lock().unwrap().take() {
                        pty.kill();
                    }
                }
                "pty_resize" => {
                    let cols = msg.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
                    let rows = msg.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
                    if let Some(ref pty) = *bridge_for_ipc.lock().unwrap() {
                        pty.resize(cols, rows);
                    }
                }
                "restart" => {
                    if let Some(ref mut pty) = bridge_for_ipc.lock().unwrap().take() {
                        pty.kill();
                    }
                    let cols = msg.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
                    let rows = msg.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
                    if let Ok(pty) = PtyBridge::spawn(&cmd_for_ipc, &["--cli"], cols, rows) {
                        let _ = pty.start_reader(proxy_for_ipc.clone());
                        *bridge_for_ipc.lock().unwrap() = Some(pty);
                    }
                }
                "chat_prompt" => {
                    if let Some(text) = msg.get("text").and_then(|v| v.as_str()) {
                        let _ = chat_tx_for_ipc.send(ChatCommand::Prompt(text.to_string()));
                    }
                }
                "new_session" => {
                    let _ = chat_tx_for_ipc.send(ChatCommand::NewSession);
                    // NewSession in agent thread saves + clears + broadcasts
                    // session list. Also send ack to clear chat UI.
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        serde_json::json!({"type": "new_session_ack"}).to_string()
                    ));
                    // Also clear the Terminal tab's REPL: send `/clear\n` to
                    // the PTY child so its in-process agent drops its history,
                    // then tell the frontend to wipe the xterm scrollback so
                    // prior output disappears visually too.
                    if let Ok(mut guard) = bridge_for_ipc.lock() {
                        if let Some(bridge) = guard.as_mut() {
                            bridge.write(b"/clear\n");
                        }
                    }
                    let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(
                        serde_json::json!({"type": "terminal_clear"}).to_string()
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
                        None => (false, "path not resolvable (HOME not set?)".into(), None),
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
                    let (ok, text) = match arboard::Clipboard::new()
                        .and_then(|mut c| c.get_text())
                    {
                        Ok(t) => (true, t),
                        Err(_) => (false, String::new()),
                    };
                    let payload = serde_json::json!({
                        "type": "clipboard_text",
                        "ok": ok,
                        "text": text,
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
                    // Team feature is opt-in; if the project hasn't set
                    // teamEnabled: true, report has_team = false so the
                    // frontend hides the Team tab entirely.
                    let team_feature_on = crate::config::ProjectConfig::load()
                        .and_then(|c| c.team_enabled)
                        .unwrap_or(false);
                    let has_team = team_feature_on && team_dir.join("config.json").exists();
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
                    let raw_path = msg.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                    let resolved = crate::sandbox::Sandbox::check(raw_path)
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
                    let raw_path = msg.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    match crate::sandbox::Sandbox::check(raw_path) {
                        Ok(path) => {
                            let ext = path.extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("")
                                .to_lowercase();
                            let is_image = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "bmp");
                            let is_pdf = ext == "pdf";
                            let mime = match ext.as_str() {
                                "png" => "image/png",
                                "jpg" | "jpeg" => "image/jpeg",
                                "gif" => "image/gif",
                                "svg" => "image/svg+xml",
                                "webp" => "image/webp",
                                "ico" => "image/x-icon",
                                "bmp" => "image/bmp",
                                "pdf" => "application/pdf",
                                "md" => "text/markdown",
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
                                    });
                                    let _ = proxy_for_ipc.send_event(UserEvent::FileContent(payload.to_string()));
                                }
                            } else {
                                match std::fs::read_to_string(&path) {
                                    Ok(text) => {
                                        let payload = serde_json::json!({
                                            "type": "file_content",
                                            "path": raw_path,
                                            "content": text,
                                            "mime": mime,
                                        });
                                        let _ = proxy_for_ipc.send_event(UserEvent::FileContent(payload.to_string()));
                                    }
                                    Err(e) => {
                                        let payload = serde_json::json!({
                                            "type": "file_content",
                                            "path": raw_path,
                                            "content": format!("Error reading file: {e}"),
                                            "mime": "text/plain",
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
                "session_load" => {
                    if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                        if let Some(store) = SessionStore::default_path().map(SessionStore::new) {
                            if let Ok(session) = store.load(id) {
                                // Send history to chat agent thread.
                                let _ = chat_tx_for_ipc.send(ChatCommand::LoadHistory(session.messages.clone()));
                                // Also send /load command to the terminal PTY.
                                if let Some(ref mut pty) = *bridge_for_ipc.lock().unwrap() {
                                    let cmd = format!("/load {}\n", id);
                                    pty.write(cmd.as_bytes());
                                }
                                // Build display messages for the frontend.
                                let display: Vec<serde_json::Value> = session.messages.iter().map(|m| {
                                    let role = match m.role {
                                        crate::types::Role::User => "user",
                                        crate::types::Role::Assistant => "assistant",
                                        crate::types::Role::System => "system",
                                    };
                                    let text: String = m.content.iter().filter_map(|b| match b {
                                        crate::types::ContentBlock::Text { text } => Some(text.clone()),
                                        crate::types::ContentBlock::ToolUse { name, .. } => Some(format!("[tool: {name}]")),
                                        crate::types::ContentBlock::ToolResult { content, .. } => Some(format!("[result: {}]", &content[..content.len().min(100)])),
                                    }).collect::<Vec<_>>().join("\n");
                                    serde_json::json!({"role": role, "content": text})
                                }).collect();
                                let payload = serde_json::json!({
                                    "type": "session_loaded",
                                    "id": session.id,
                                    "messages": display,
                                });
                                let _ = proxy_for_ipc.send_event(UserEvent::SessionLoaded(payload.to_string()));
                            }
                        }
                    }
                }
                _ => {}
            }
        })
        .build(&window)
        .expect("webview build");

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::UserEvent(UserEvent::PtyData(b64)) => {
                let escaped = b64.replace('\\', "\\\\").replace('\'', "\\'");
                let js = format!(
                    "window.__thclaws_dispatch(JSON.stringify({{type:'pty_data',data:'{escaped}'}}))"
                );
                let _ = webview.evaluate_script(&js);
            }
            Event::UserEvent(UserEvent::ChatTextDelta(text)) => {
                let escaped = escape_for_js(&text);
                let _ = webview.evaluate_script(&format!(
                    "window.__thclaws_dispatch(JSON.stringify({{type:'chat_text_delta',text:'{escaped}'}}))"
                ));
            }
            Event::UserEvent(UserEvent::ChatToolCall(name)) => {
                let escaped = escape_for_js(&name);
                let _ = webview.evaluate_script(&format!(
                    "window.__thclaws_dispatch(JSON.stringify({{type:'chat_tool_call',name:'{escaped}'}}))"
                ));
            }
            Event::UserEvent(UserEvent::ChatToolResult(name, output)) => {
                let n = escape_for_js(&name);
                let o = escape_for_js(&output);
                let _ = webview.evaluate_script(&format!(
                    "window.__thclaws_dispatch(JSON.stringify({{type:'chat_tool_result',name:'{n}',output:'{o}'}}))"
                ));
            }
            Event::UserEvent(UserEvent::ChatDone) => {
                let _ = webview.evaluate_script(
                    "window.__thclaws_dispatch(JSON.stringify({type:'chat_done'}))",
                );
            }
            Event::UserEvent(UserEvent::SessionListRefresh(json)) => {
                let escaped = escape_for_js(&json);
                let _ = webview.evaluate_script(&format!(
                    "window.__thclaws_dispatch('{escaped}')"
                ));
            }
            Event::UserEvent(UserEvent::FileTree(json)) | Event::UserEvent(UserEvent::FileContent(json)) => {
                let escaped = escape_for_js(&json);
                let _ = webview.evaluate_script(&format!(
                    "window.__thclaws_dispatch('{escaped}')"
                ));
            }
            Event::UserEvent(UserEvent::SessionLoaded(json)) => {
                let escaped = escape_for_js(&json);
                let _ = webview.evaluate_script(&format!(
                    "window.__thclaws_dispatch('{escaped}')"
                ));
            }
            Event::UserEvent(UserEvent::PtyExit) => {
                if pty_restart.swap(false, Ordering::SeqCst) {
                    // Intentional kill (directory change via status bar).
                    // Don't close the window — the frontend will re-show the
                    // startup modal and spawn a fresh PTY after set_cwd.
                } else {
                    // /quit or /exit — save chat session then close.
                    let _ = chat_tx_for_events.send(ChatCommand::SaveAndQuit);
                    if let Some(ref mut pty) = bridge.lock().unwrap().take() {
                        pty.kill();
                    }
                    *control_flow = ControlFlow::Exit;
                }
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
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                // Window close — save chat session too.
                let _ = chat_tx_for_events.send(ChatCommand::SaveAndQuit);
                if let Some(ref mut pty) = bridge.lock().unwrap().take() {
                    pty.kill();
                }
                // Kill any spawned teammate processes.
                let _ = std::process::Command::new("pkill")
                    .args(["-f", "team-agent"])
                    .status();
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}
