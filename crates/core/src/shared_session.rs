//! Shared in-process agent session that backs both the GUI's Terminal
//! and Chat tabs. One Agent, one Session, one history. Both tabs send
//! input through `ShellInput` and subscribe to `ViewEvent` broadcasts —
//! so typing in either tab contributes to the same conversation, and
//! /load replays the same transcript into both views.
//!
//! Only compiled with the `gui` feature because the previous
//! Terminal-tab REPL ran as a separate `--cli` PTY child; the
//! standalone CLI (`thclaws --cli`) is unchanged.

#![cfg(feature = "gui")]

use crate::agent::{Agent, AgentEvent};
use crate::config::AppConfig;
use crate::context::ProjectContext;
use crate::error::{Error, Result as CoreResult};
use crate::memory::MemoryStore;
use crate::providers::{EventStream, Provider, StreamRequest};
use crate::repl::{build_provider, build_provider_with_fallback};
use crate::session::{Session, SessionStore};
use crate::tools::ToolRegistry;
use crate::types::{ContentBlock, Message, Role};
use async_trait::async_trait;
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use tokio::sync::broadcast;

/// Signal gate that holds background work (MCP spawn, other heavy
/// startup tasks) until the frontend has finished its launch screens.
/// Using a flag + Notify so late waiters still unblock immediately
/// after the signal has fired.
pub struct ReadyGate {
    ready: AtomicBool,
    notify: tokio::sync::Notify,
}

impl ReadyGate {
    pub fn new() -> Self {
        Self {
            ready: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Resolves as soon as [`signal`] has been called (now or later).
    pub async fn wait(&self) {
        loop {
            if self.ready.load(Ordering::Relaxed) {
                return;
            }
            self.notify.notified().await;
        }
    }

    pub fn signal(&self) {
        self.ready.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }
}

impl Default for ReadyGate {
    fn default() -> Self {
        Self::new()
    }
}

/// Inputs to the shared session — produced by either tab.
#[derive(Debug, Clone)]
pub enum ShellInput {
    /// Raw line submitted by the user. Slash-prefix → dispatched as
    /// command, anything else → fed to the agent as a prompt.
    Line(String),
    /// Like `Line` but with one or more inline image attachments
    /// (paste / drag-drop into the chat composer). Each attachment is
    /// `(media_type, base64_data)`. Slash commands aren't expected
    /// here — the GUI only emits this when an image is attached, and
    /// it doesn't make sense to combine a slash command with images.
    LineWithImages {
        text: String,
        images: Vec<(String, String)>,
    },
    /// Save the current session to disk, clear history, start fresh.
    NewSession,
    /// Load a session by id and replace history.
    LoadSession(String),
    /// Save the current session (window-close path).
    SaveAndQuit,
    /// User changed the working directory via the GUI's "change directory"
    /// modal. The harness has already updated process cwd + sandbox; the
    /// worker reloads `ProjectConfig` from the new location, swaps the
    /// agent's provider to whatever the new project's settings.json
    /// specifies, and rebuilds the system prompt. Without this, the
    /// running session keeps the model loaded at startup even though the
    /// new project has different settings — violating the
    /// "project settings win" contract.
    ChangeCwd(std::path::PathBuf),
    /// Batch of unread messages the lead's inbox poller swept — fed
    /// into the agent as a synthetic turn so the lead actually reacts
    /// to teammate notifications in GUI mode (the CLI REPL has its
    /// own poller loop; this is GUI parity).
    TeamMessages(Vec<crate::team::TeamMessage>),
    /// A background task finished spawning an MCP server — register
    /// its tools into the live tool registry and rebuild the agent so
    /// the next turn sees them. This lets the worker start accepting
    /// prompts *before* MCP spawn approval returns, instead of
    /// blocking startup on an approval modal that hasn't mounted yet.
    McpReady {
        server_name: String,
        client: std::sync::Arc<crate::mcp::McpClient>,
        tools: Vec<crate::mcp::McpToolInfo>,
    },
    /// Background MCP spawn failed (approval denied, binary missing,
    /// etc.). Surface as a `ViewEvent::ErrorText` so the user sees
    /// *why* a configured MCP server never came online.
    McpFailed { server_name: String, error: String },
    /// Reload `AppConfig` from disk and rebuild the agent's provider in
    /// place. Sent by the GUI after `api_key_set` / `api_key_clear` so
    /// the running session picks up the new key (and any auto-fallback
    /// model swap that just happened) without needing an app restart.
    /// Without this, the sidebar reflects the new provider while the
    /// worker keeps holding the stale one — the exact mismatch users
    /// see as "sidebar says openai but error mentions anthropic."
    ReloadConfig,
}

/// What both tabs render. Each variant maps to a UI affordance:
/// Chat → bubbles + tool blocks, Terminal → ANSI-formatted bytes.
#[derive(Debug, Clone)]
pub enum ViewEvent {
    UserPrompt(String),
    AssistantTextDelta(String),
    ToolCallStart {
        name: String,
        label: String,
    },
    ToolCallResult {
        name: String,
        output: String,
    },
    SlashOutput(String),
    TurnDone,
    HistoryReplaced(Vec<DisplayMessage>),
    SessionListRefresh(String),
    /// Sidebar provider/model update — carries a pre-built JSON
    /// payload shaped like `{type: "provider_update", provider, model,
    /// provider_ready}`. Emitted by the worker when it changes the
    /// active model (e.g. auto-switch during `/load`) so the sidebar
    /// reflects the new state without waiting for the 5 s config-poll.
    ProviderUpdate(String),
    /// Sidebar KMS list refresh — pre-built JSON payload shaped like
    /// `{type: "kms_update", kmss: [{name, scope, active}, ...]}`.
    /// Emitted after `/kms new | use | off` so the sidebar reflects
    /// the new state without waiting for the next full session_update.
    KmsUpdate(String),
    /// The session's on-disk JSONL has crossed the fork threshold.
    /// Frontend renders a dismissible banner with a "Fork into new
    /// session with summary" action. Fired once per session.
    ContextWarning {
        file_size_mb: f64,
    },
    ErrorText(String),
}

#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: String,
    pub content: String,
}

impl DisplayMessage {
    pub fn from_messages(messages: &[Message]) -> Vec<Self> {
        let mut out: Vec<DisplayMessage> = Vec::new();
        for m in messages {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                // System prompts never render as chat bubbles.
                Role::System => continue,
            };

            // Walk content blocks. Text accumulates into a single bubble
            // for this canonical message; ToolUse blocks emit their own
            // `tool` entries (so they render the same compact ▸/✓
            // indicator as live AgentEvent::ToolCallStart in ChatView);
            // ToolResult is dropped entirely — the chat tab is for the
            // user↔assistant exchange, raw tool output lives on the
            // Terminal tab.
            let mut text_parts: Vec<String> = Vec::new();
            let mut deferred_tools: Vec<DisplayMessage> = Vec::new();
            for b in &m.content {
                match b {
                    ContentBlock::Text { text } => text_parts.push(text.clone()),
                    // Reasoning is model-internal scratch — don't show
                    // it in the chat-list display. When the GUI gets a
                    // dedicated "show thinking" toggle, surface this
                    // there instead of the main bubble.
                    ContentBlock::Thinking { .. } => {}
                    ContentBlock::ToolUse { name, .. } => {
                        deferred_tools.push(DisplayMessage {
                            role: "tool".into(),
                            content: name.clone(),
                        });
                    }
                    // Tool results don't surface on history restore.
                    ContentBlock::ToolResult { .. } => {}
                    // Inline image attached by the user (paste /
                    // drag-drop). Render as a brief placeholder in
                    // the chat-list digest; the actual pixels stay
                    // in the underlying ContentBlock for the model.
                    ContentBlock::Image { .. } => text_parts.push("[image]".into()),
                }
            }

            // Emit text bubble first (if any), then any tool indicators
            // — preserves the live-mode ordering where the assistant's
            // narration appears before the tool calls it triggered.
            let text = text_parts.join("\n");
            if !text.is_empty() {
                out.push(DisplayMessage {
                    role: role.to_string(),
                    content: text,
                });
            }
            out.extend(deferred_tools);
        }
        out
    }
}

pub struct SharedSessionHandle {
    pub input_tx: mpsc::Sender<ShellInput>,
    pub events_tx: broadcast::Sender<ViewEvent>,
    pub cancel: Arc<AtomicBool>,
    /// Frontend signals this once it's past the launch modals so
    /// deferred startup (MCP spawn, etc.) can start making user-facing
    /// prompts. Calling `signal()` multiple times is fine.
    pub ready_gate: Arc<ReadyGate>,
}

impl SharedSessionHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<ViewEvent> {
        self.events_tx.subscribe()
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Bundle of owned state the worker loop passes by `&mut` down into
/// slash-command dispatch. Having one struct keeps the dispatch
/// signature readable as we port every REPL command — each of which
/// may mutate any subset of these fields (agent for /model, config
/// for /permissions, session for /load, etc.) or rebuild the agent
/// outright (/model, /provider, /permissions after applying, …).
pub struct WorkerState {
    pub agent: Agent,
    pub config: AppConfig,
    pub session: Session,
    pub session_store: Option<SessionStore>,
    pub tool_registry: ToolRegistry,
    pub system_prompt: String,
    pub cwd: PathBuf,
    /// Approval sink attached to `agent`. Kept here so
    /// [`Self::rebuild_agent`] can re-wire it onto the fresh Agent — a
    /// `/model` or `/provider` swap must preserve the user's approval
    /// UI (GUI modal vs REPL prompt) without silently falling back to
    /// AutoApprover.
    pub approver: std::sync::Arc<dyn crate::permissions::ApprovalSink>,
    /// Shared handle into the SkillTool's internal store. `/skill
    /// install` replaces the store contents through this handle so a
    /// fresh skill is callable in the same session without restart.
    pub skill_store: std::sync::Arc<std::sync::Mutex<crate::skills::SkillStore>>,
    /// Live MCP client subprocesses. Kept so `/mcp add` can append new
    /// clients whose tools are wired into `tool_registry`; dropping
    /// the Vec shuts them all down.
    pub mcp_clients: Vec<std::sync::Arc<crate::mcp::McpClient>>,
    /// Sticky flag: once the session's on-disk JSONL crosses the fork
    /// threshold (5 MB) we emit a single `ContextWarning` and set this
    /// to `true`. Reset when a fresh session becomes active (new /
    /// load / fork) so the next session starts with a clean slate.
    pub warned_file_size: bool,
    /// Handle to `.thclaws/team/agents/lead/output.log` — agent output
    /// is mirrored here so the GUI Team tab can show a lead pane
    /// alongside spawned teammates. The CLI REPL writes the same file
    /// from its own loop; GUI-mode never runs that loop, so without
    /// this mirror the Team tab has no lead entry. `None` inside the
    /// mutex means the file could not be opened; writes are silent.
    pub lead_log: std::sync::Arc<std::sync::Mutex<Option<std::fs::File>>>,
}

impl WorkerState {
    /// Rebuild `agent` with a freshly-built provider from `self.config`,
    /// reusing the current tool registry + system prompt. Preserves
    /// `permission_mode` and `thinking_budget`.
    ///
    /// `preserve_history = true` carries the current conversation into
    /// the new Agent (used by mutations that change the tool roster or
    /// system prompt mid-conversation — /mcp add, /kms use, etc.).
    /// `false` clears history (used by /model and /provider switches
    /// where the new provider's schema may differ).
    pub fn rebuild_agent(&mut self, preserve_history: bool) -> crate::error::Result<()> {
        let prev_history = if preserve_history {
            Some(self.agent.history_snapshot())
        } else {
            None
        };
        let provider = build_provider(&self.config)?;
        let prev_perm = self.agent.permission_mode;
        let prev_thinking = self.agent.thinking_budget;
        let new_agent = Agent::new(
            provider,
            self.tool_registry.clone(),
            &self.config.model,
            &self.system_prompt,
        )
        .with_approver(self.approver.clone());
        self.agent = new_agent;
        self.agent.permission_mode = prev_perm;
        self.agent.thinking_budget = prev_thinking;
        if let Some(h) = prev_history {
            self.agent.set_history(h);
        }
        Ok(())
    }

    /// Recompute the system prompt from the current `config` (picks up
    /// updated `kms_active`, `team_enabled`, memory, skills, etc.).
    /// Call after any dispatcher mutation that should land in the next
    /// turn's system prompt.
    pub fn rebuild_system_prompt(&mut self) {
        self.system_prompt = build_system_prompt(&self.config, &self.cwd, &self.skill_store);
    }
}

/// Assemble the system prompt from the project context, memory, KMS
/// attachments, team grounding, and skill catalogue. Extracted so both
/// initial spawn and runtime rebuilds (`/kms use`, `/mcp add`, etc.)
/// share the same shape.
pub fn build_system_prompt(
    config: &AppConfig,
    cwd: &std::path::Path,
    skill_store: &std::sync::Arc<std::sync::Mutex<crate::skills::SkillStore>>,
) -> String {
    let ctx = ProjectContext::discover(cwd).unwrap_or(ProjectContext {
        cwd: cwd.to_path_buf(),
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

    let team_enabled = crate::config::ProjectConfig::load()
        .and_then(|c| c.team_enabled)
        .unwrap_or(false);
    let team_section = team_grounding_prompt(&config.model, team_enabled);
    if !team_section.is_empty() {
        system.push_str("\n\n");
        system.push_str(&team_section);
    }

    let guard = skill_store.lock().ok();
    if let Some(store) = guard.as_ref() {
        if !store.skills.is_empty() {
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
            let mut entries: Vec<&crate::skills::SkillDef> = store.skills.values().collect();
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            for skill in entries {
                system.push_str(&format!("- **{}** — {}", skill.name, skill.description));
                if !skill.when_to_use.is_empty() {
                    system.push_str(&format!("\n  Trigger: {}", skill.when_to_use));
                }
                system.push('\n');
            }
        }
    }

    system
}

pub fn spawn() -> SharedSessionHandle {
    spawn_with_approver(std::sync::Arc::new(crate::permissions::AutoApprover))
}

/// Spawn the shared session worker with an explicit approval sink.
/// GUI mode uses this to wire a `GuiApprover` that drives a frontend
/// modal; the zero-arg [`spawn`] falls back to `AutoApprover` for
/// callers that don't implement interactive approval.
pub fn spawn_with_approver(
    approver: std::sync::Arc<dyn crate::permissions::ApprovalSink>,
) -> SharedSessionHandle {
    let (input_tx, input_rx) = mpsc::channel::<ShellInput>();
    let (events_tx, _) = broadcast::channel::<ViewEvent>(256);
    let cancel = Arc::new(AtomicBool::new(false));
    let ready_gate = Arc::new(ReadyGate::new());

    let events_tx_for_thread = events_tx.clone();
    let cancel_for_thread = cancel.clone();
    let input_tx_for_poller = input_tx.clone();
    let gate_for_thread = ready_gate.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(run_worker(
                input_rx,
                input_tx_for_poller,
                events_tx_for_thread.clone(),
                cancel_for_thread,
                approver,
                gate_for_thread,
            ));
        }));
        if let Err(payload) = result {
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "shared session panicked".to_string()
            };
            let _ =
                events_tx_for_thread.send(ViewEvent::ErrorText(format!("internal error: {msg}")));
        }
    });

    SharedSessionHandle {
        input_tx,
        events_tx,
        cancel,
        ready_gate,
    }
}

async fn run_worker(
    input_rx: mpsc::Receiver<ShellInput>,
    input_tx_self: mpsc::Sender<ShellInput>,
    events_tx: broadcast::Sender<ViewEvent>,
    cancel: Arc<AtomicBool>,
    approver: std::sync::Arc<dyn crate::permissions::ApprovalSink>,
    ready_gate: Arc<ReadyGate>,
) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let config = AppConfig::load().unwrap_or_default();

    // Shared SkillTool store — we keep a handle in WorkerState so
    // `/skill install` can repopulate it without restarting.
    let skill_store =
        std::sync::Arc::new(std::sync::Mutex::new(crate::skills::SkillStore::discover()));

    let mut tools = ToolRegistry::with_builtins();
    if !config.kms_active.is_empty() {
        tools.register(std::sync::Arc::new(crate::tools::KmsReadTool));
        tools.register(std::sync::Arc::new(crate::tools::KmsSearchTool));
    }
    let team_enabled = crate::config::ProjectConfig::load()
        .and_then(|c| c.team_enabled)
        .unwrap_or(false);
    if team_enabled {
        let _ = crate::team::register_team_tools(&mut tools, "lead");
    }
    // Mark this GUI worker as the team lead when team mode is on. The CLI
    // path sets this in repl.rs; the GUI path was missing the call, which
    // left BashTool's `lead_forbidden_command` guard inert — the LLM lead
    // could (and did) run `rm -rf tests/`, `git reset --hard`, etc., wiping
    // teammate work. The `&& !is_teammate()` keeps the flag off for any
    // teammate process that happened to share this code path.
    let is_teammate = std::env::var("THCLAWS_TEAM_AGENT").is_ok();
    crate::team::set_is_team_lead(team_enabled && !is_teammate);
    let skill_tool = crate::skills::SkillTool::new_from_handle(skill_store.clone());
    tools.register(std::sync::Arc::new(skill_tool));

    // MCP servers are spawned in background tasks so a pending
    // approval modal can't block worker startup. The worker's main
    // loop handles `ShellInput::McpReady` / `McpFailed` to register
    // tools as each server comes online; until then the agent simply
    // runs without MCP tools. Previous blocking loop meant: if the
    // user hadn't yet clicked through the startup modal when the
    // approval request fired, the frontend dropped the dispatch (no
    // subscriber mounted) and the whole worker deadlocked.
    let mcp_clients: Vec<std::sync::Arc<crate::mcp::McpClient>> = Vec::new();
    // Give the caller's event-translator a chance to subscribe before we
    // emit anything — tokio broadcast drops messages sent before any
    // receiver exists, so the first handful of events at startup race
    // against gui.rs's `spawn_event_translator`. 250 ms is plenty for
    // the main thread to wire up the subscribe.
    tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
    // …then hold here until the frontend reports its launch screens are
    // done. Otherwise an MCP spawn approval modal can pop up *on top*
    // of the working-directory picker before the user has even chosen
    // a project — visible but confusing UX.
    ready_gate.wait().await;
    // CLAUDE.md / AGENTS.md size advisory — fire once at startup if
    // any team-memory file is past the soft 40 KB threshold. Doesn't
    // truncate (Claude Code also doesn't — CLAUDE.md is assumed to
    // be worth loading in full). The nudge just surfaces "this file
    // is large enough the model may skim past it" so the user
    // notices and trims if they want.
    {
        let oversize = crate::context::scan_claude_md_oversize(&cwd);
        for hit in oversize {
            let kb = hit.bytes / 1024;
            let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                "⚠ large memory file: {} ({} KB > {} KB soft cap). Consider splitting into topic files or trimming — Claude is less likely to read it carefully at this size.",
                hit.path.display(),
                kb,
                crate::context::CLAUDE_MD_WARN_BYTES / 1024,
            )));
        }
    }

    // Daily model-catalogue refresh. Runs once per worker start if
    // the cache is missing or older than 24 h. Fully silent — success
    // just updates the cache, failure leaves whatever's there. The
    // next Agent built (rebuild_agent / switch) picks up the new data.
    tokio::spawn(async move {
        let should_refresh = match crate::model_catalogue::cache_age() {
            Some(age) => age > crate::model_catalogue::AUTO_REFRESH_INTERVAL,
            None => true, // no cache yet → attempt
        };
        if should_refresh {
            let _ = crate::model_catalogue::refresh_from_remote().await;
        }
    });
    for server_cfg in config.mcp_servers.clone() {
        let approver_for_spawn = approver.clone();
        let input_tx_for_spawn = input_tx_self.clone();
        tokio::spawn(async move {
            let server_name = server_cfg.name.clone();
            match crate::mcp::McpClient::spawn_with_approver(server_cfg, Some(approver_for_spawn))
                .await
            {
                Ok(client) => match client.list_tools().await {
                    Ok(tool_infos) => {
                        let _ = input_tx_for_spawn.send(ShellInput::McpReady {
                            server_name,
                            client,
                            tools: tool_infos,
                        });
                    }
                    Err(e) => {
                        let _ = input_tx_for_spawn.send(ShellInput::McpFailed {
                            server_name,
                            error: format!("list_tools failed: {e}"),
                        });
                    }
                },
                Err(e) => {
                    let _ = input_tx_for_spawn.send(ShellInput::McpFailed {
                        server_name,
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    let system = build_system_prompt(&config, &cwd, &skill_store);

    // `build_provider_with_fallback` walks the configured model first,
    // then any provider whose key is actually present, before giving
    // up. If everything fails we install a `NoopProvider` that errors
    // on stream() with a clear "configure a key" message — this keeps
    // the worker loop alive so the user can recover via Settings →
    // API key (which sends `ReloadConfig` and rebuilds the agent in
    // place). The previous `return` here killed the chat for the rest
    // of the session.
    let mut config = config;
    let (maybe_provider, warning) = build_provider_with_fallback(&mut config).await;
    if let Some(w) = &warning {
        let _ = events_tx.send(ViewEvent::ErrorText(format!("Provider: {w}")));
    }
    let provider: Arc<dyn Provider> = maybe_provider.unwrap_or_else(|| {
        Arc::new(NoopProvider::new(
            "no LLM provider configured — open Settings → Provider API keys to add one",
        ))
    });
    let mut agent =
        Agent::new(provider, tools.clone(), &config.model, &system).with_approver(approver.clone());
    // Respect the user's configured permission mode (project
    // `.thclaws/settings.json` can set it to "ask"). Without this the
    // GUI's Ask mode flag had no effect because the Agent was built
    // with the default Auto.
    agent.permission_mode = if config.permissions == "auto" {
        crate::permissions::PermissionMode::Auto
    } else {
        crate::permissions::PermissionMode::Ask
    };

    let session_store = SessionStore::default_path().map(SessionStore::new);
    let current_session = Session::new(&config.model, cwd.to_string_lossy());

    // Lead status + output log so the Team tab can show a 'lead' pane.
    // `run_repl` writes these from the CLI loop; in GUI mode nobody does,
    // so all_status() came back without a lead entry and the Team tab
    // rendered teammates only.
    let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
    let _ = lead_mb.write_status("lead", "active", None);
    let lead_log_path = lead_mb.output_log_path("lead");
    if let Some(parent) = lead_log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let lead_log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&lead_log_path)
        .ok();
    let lead_log = std::sync::Arc::new(std::sync::Mutex::new(lead_log_file));

    let mut state = WorkerState {
        agent,
        config,
        session: current_session,
        session_store,
        tool_registry: tools,
        system_prompt: system,
        cwd,
        approver,
        skill_store,
        mcp_clients,
        warned_file_size: false,
        lead_log,
    };

    // Lead inbox poller — parity with repl.rs:1524. Without this, teammates
    // message the lead, messages pile up in `.thclaws/team/inboxes/lead.json`
    // unread, and the team stalls waiting for the lead to react.
    if team_enabled {
        let poller_tx = input_tx_self.clone();
        tokio::spawn(async move {
            let mailbox = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
            loop {
                let unread = mailbox.read_unread("lead").unwrap_or_default();
                if !unread.is_empty() {
                    let ids: Vec<String> = unread.iter().map(|m| m.id.clone()).collect();
                    let _ = mailbox.mark_as_read("lead", &ids);
                    if poller_tx.send(ShellInput::TeamMessages(unread)).is_err() {
                        // Receiver dropped — session ended.
                        return;
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(
                    crate::team::POLL_INTERVAL_MS,
                ))
                .await;
            }
        });
    }

    while let Ok(input) = input_rx.recv() {
        match input {
            ShellInput::Line(text) => {
                cancel.store(false, Ordering::Relaxed);
                handle_line(text, &mut state, &events_tx, &cancel).await;
            }
            ShellInput::LineWithImages { text, images } => {
                cancel.store(false, Ordering::Relaxed);
                handle_line_with_images(text, images, &mut state, &events_tx, &cancel).await;
            }
            ShellInput::NewSession => {
                save_history(&state.agent, &mut state.session, &state.session_store);
                state.agent.clear_history();
                state.session = Session::new(&state.config.model, state.cwd.to_string_lossy());
                state.warned_file_size = false;
                let _ = events_tx.send(ViewEvent::HistoryReplaced(Vec::new()));
                let _ = events_tx.send(ViewEvent::SessionListRefresh(build_session_list(
                    &state.session_store,
                    &state.session.id,
                )));
            }
            ShellInput::LoadSession(id) => {
                let Some(ref store) = state.session_store else {
                    continue;
                };
                let Ok(loaded) = store.load(&id) else {
                    let _ = events_tx.send(ViewEvent::ErrorText(format!(
                        "Failed to load session '{id}'"
                    )));
                    continue;
                };
                // If the session was recorded against a different
                // provider than what's active, the stored messages
                // carry wire-specific shapes (Anthropic content
                // blocks, OpenAI tool_calls arrays, Gemini parts, …)
                // that won't replay cleanly through another provider.
                // Auto-switch to the session's original model. If that
                // provider has no credentials configured, refuse the
                // load rather than swap to something that will hard-
                // error on the next turn.
                let current_kind = crate::providers::ProviderKind::detect(&state.config.model);
                let loaded_kind = crate::providers::ProviderKind::detect(&loaded.model);
                let needs_switch = loaded_kind.is_some() && current_kind != loaded_kind;
                if needs_switch {
                    let Some(target_kind) = loaded_kind else {
                        continue;
                    };
                    if !kind_has_credentials(target_kind) {
                        let provider_name = target_kind.name();
                        let env_hint = target_kind
                            .api_key_env()
                            .map(|v| format!(" (set {v})"))
                            .unwrap_or_default();
                        let _ = events_tx.send(ViewEvent::ErrorText(format!(
                            "Can't load session '{id}' — it was recorded against {provider_name} ({}), but no API key for that provider is configured{env_hint}.",
                            loaded.model
                        )));
                        continue;
                    }
                    // Flush whatever the active session had so we don't
                    // lose a turn or two just because the user clicked
                    // another session.
                    save_history(&state.agent, &mut state.session, &state.session_store);
                    state.config.model = loaded.model.clone();
                    if let Err(e) = state.rebuild_agent(false) {
                        let _ = events_tx.send(ViewEvent::ErrorText(format!(
                            "Auto-switch to {} failed: {e}",
                            loaded.model
                        )));
                        continue;
                    }
                    let provider_name = target_kind.name();
                    let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                        "(auto-switched to {provider_name}/{} to match session)",
                        loaded.model
                    )));
                    // Keep `.thclaws/settings.json` in sync so a
                    // restart lands on the same provider/model.
                    let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
                    project.set_model(&state.config.model);
                    let _ = project.save();
                    // Push the sidebar immediately so the Provider /
                    // model display reflects the switch without
                    // waiting for the 5 s config_poll.
                    let payload = serde_json::json!({
                        "type": "provider_update",
                        "provider": provider_name,
                        "model": state.config.model,
                        "provider_ready": true,
                    });
                    let _ = events_tx.send(ViewEvent::ProviderUpdate(payload.to_string()));
                }
                state.agent.set_history(loaded.messages.clone());
                state.session = loaded;
                state.warned_file_size = false;
                let display = DisplayMessage::from_messages(&state.session.messages);
                let _ = events_tx.send(ViewEvent::HistoryReplaced(display));
                // Refresh so the sidebar's "current session" highlight
                // moves to the freshly-loaded id.
                let _ = events_tx.send(ViewEvent::SessionListRefresh(build_session_list(
                    &state.session_store,
                    &state.session.id,
                )));
            }
            ShellInput::SaveAndQuit => {
                save_history(&state.agent, &mut state.session, &state.session_store);
                break;
            }
            ShellInput::TeamMessages(msgs) => {
                cancel.store(false, Ordering::Relaxed);
                handle_team_messages(msgs, &mut state, &events_tx, &cancel).await;
            }
            ShellInput::McpReady {
                server_name,
                client,
                tools: tool_infos,
            } => {
                for info in tool_infos {
                    let tool = crate::mcp::McpTool::new(client.clone(), info);
                    state.tool_registry.register(std::sync::Arc::new(tool));
                }
                state.mcp_clients.push(client);
                // Rebuild so the agent actually sees the newly-registered
                // MCP tools on its next turn.
                if let Err(e) = state.rebuild_agent(true) {
                    let _ = events_tx.send(ViewEvent::ErrorText(format!(
                        "[mcp] '{server_name}' tools registered but rebuild failed: {e}"
                    )));
                } else {
                    let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                        "[mcp] '{server_name}' connected"
                    )));
                }
            }
            ShellInput::McpFailed { server_name, error } => {
                let _ = events_tx.send(ViewEvent::ErrorText(format!(
                    "[mcp] '{server_name}' failed to start: {error}"
                )));
            }
            ShellInput::ReloadConfig => {
                // Pull the on-disk settings (api_key_set may have just
                // auto-switched the model in `.thclaws/settings.json`)
                // and rebuild the agent's provider in place. Without
                // this, the worker keeps holding whatever provider it
                // built at startup — usually the placeholder NoopProvider
                // when the user launched without any keys configured.
                let prev_model = state.config.model.clone();
                match crate::config::AppConfig::load() {
                    Ok(new_config) => state.config = new_config,
                    Err(e) => {
                        let _ = events_tx.send(ViewEvent::ErrorText(format!(
                            "[reload] config load failed, keeping old: {e}"
                        )));
                        continue;
                    }
                }
                let model_changed = state.config.model != prev_model;
                // Preserve history when only the auth changed under the
                // same model — wire format is unchanged. Drop history
                // when the model itself flipped, since the new
                // provider's message schema may not replay cleanly.
                match state.rebuild_agent(!model_changed) {
                    Ok(()) => {
                        state.rebuild_system_prompt();
                        if model_changed {
                            // Mint a fresh session so its stored
                            // `model` field matches the active
                            // provider — same logic as ChangeCwd.
                            state.session = crate::session::Session::new(
                                &state.config.model,
                                state.cwd.to_string_lossy(),
                            );
                            state.warned_file_size = false;
                            let _ = events_tx.send(ViewEvent::HistoryReplaced(Vec::new()));
                        }
                        let provider_name = state.config.detect_provider().unwrap_or("unknown");
                        let payload = serde_json::json!({
                            "type": "provider_update",
                            "provider": provider_name,
                            "model": state.config.model,
                            "provider_ready": true,
                        });
                        let _ = events_tx.send(ViewEvent::ProviderUpdate(payload.to_string()));
                        let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                            "(provider reloaded: {provider_name}/{})",
                            state.config.model
                        )));
                    }
                    Err(e) => {
                        let _ = events_tx.send(ViewEvent::ErrorText(format!(
                            "[reload] agent rebuild failed: {e}"
                        )));
                    }
                }
            }
            ShellInput::ChangeCwd(new_cwd) => {
                // Process cwd + sandbox were already updated by the GUI
                // dispatcher before sending this. Here we only refresh the
                // worker's view: model, system prompt, session metadata.
                let prev_model = state.config.model.clone();
                state.cwd = new_cwd.clone();

                // Reload config — `AppConfig::load` reads project settings
                // via `ProjectConfig::project_dir()`, which honors
                // $THCLAWS_PROJECT_ROOT first and otherwise current_dir
                // (which the GUI just changed). Result: project settings
                // from the NEW workspace win.
                match crate::config::AppConfig::load() {
                    Ok(new_config) => state.config = new_config,
                    Err(e) => {
                        let _ = events_tx.send(ViewEvent::ErrorText(format!(
                            "[cwd-change] config reload failed, keeping old: {e}"
                        )));
                    }
                }

                // If the model changed, rebuild the agent without history
                // — the new provider's message schema may not match the
                // old conversation, same logic as `/model` swap.
                let model_changed = state.config.model != prev_model;
                if model_changed {
                    if let Err(e) = state.rebuild_agent(false) {
                        let _ = events_tx.send(ViewEvent::ErrorText(format!(
                            "[cwd-change] agent rebuild failed: {e} (model stays on '{prev_model}')"
                        )));
                    } else {
                        // Mint a fresh session — the new model's id and
                        // empty history shouldn't share the old session.
                        state.session = crate::session::Session::new(
                            &state.config.model,
                            state.cwd.to_string_lossy(),
                        );
                    }
                }

                // Always rebuild the system prompt — the cwd it embeds
                // changed, even if the model didn't.
                state.rebuild_system_prompt();

                let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                    "[cwd] {} → model: {} (was: {})",
                    new_cwd.display(),
                    state.config.model,
                    prev_model
                )));
            }
        }
    }
}

pub(crate) fn save_history(agent: &Agent, session: &mut Session, store: &Option<SessionStore>) {
    let history = agent.history_snapshot();
    if history.is_empty() {
        return;
    }
    session.sync(history);
    if let Some(ref store) = store {
        let _ = store.save(session);
    }
}

pub(crate) fn build_session_list(store: &Option<SessionStore>, current_id: &str) -> String {
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
    serde_json::json!({
        "type": "sessions_list",
        "sessions": sessions,
        "current_id": current_id,
    })
    .to_string()
}

async fn handle_line(
    text: String,
    state: &mut WorkerState,
    events_tx: &broadcast::Sender<ViewEvent>,
    cancel: &Arc<AtomicBool>,
) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }

    let _ = events_tx.send(ViewEvent::UserPrompt(trimmed.to_string()));
    write_lead_log(
        &state.lead_log,
        &format!("\n\x1b[36m❯ {trimmed}\x1b[0m\n\x1b[32m"),
    );

    if trimmed.starts_with('/') {
        crate::shell_dispatch::dispatch(trimmed, state, events_tx).await;
        let _ = events_tx.send(ViewEvent::TurnDone);
        return;
    }

    // Before each turn: if the in-memory history is over the soft
    // threshold (80% of budget_tokens), run a cheap drop-oldest
    // compaction and persist the checkpoint. Keeps the wire request
    // small and the in-memory history bounded. Silent except for a
    // dim `[compacted: …]` notice — users should know when earlier
    // messages stop reaching the model.
    maybe_auto_compact(state, events_tx);

    let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
    let _ = lead_mb.write_status("lead", "working", None);

    let stream = Box::pin(state.agent.run_turn(trimmed.to_string()));
    drive_turn_stream(stream, state, events_tx, cancel, &lead_mb).await;
}

/// Multipart variant of `handle_line` — used when the chat composer
/// attaches one or more images to a user message (Phase 4 paste/drag-
/// drop). Skips slash-command dispatch (a slash command + image makes
/// no sense) and feeds a mixed Text + Image content vec into the
/// agent's `run_turn_multipart`.
async fn handle_line_with_images(
    text: String,
    images: Vec<(String, String)>,
    state: &mut WorkerState,
    events_tx: &broadcast::Sender<ViewEvent>,
    cancel: &Arc<AtomicBool>,
) {
    let trimmed = text.trim();
    if trimmed.is_empty() && images.is_empty() {
        return;
    }

    // Display digest for the chat-list — show the user's text plus a
    // compact "[+N image(s)]" tail so they see what they actually sent.
    let display = if images.is_empty() {
        trimmed.to_string()
    } else if trimmed.is_empty() {
        format!(
            "[{} image{}]",
            images.len(),
            if images.len() == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "{trimmed} [+{} image{}]",
            images.len(),
            if images.len() == 1 { "" } else { "s" }
        )
    };
    let _ = events_tx.send(ViewEvent::UserPrompt(display.clone()));
    write_lead_log(
        &state.lead_log,
        &format!("\n\x1b[36m❯ {display}\x1b[0m\n\x1b[32m"),
    );

    maybe_auto_compact(state, events_tx);

    let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
    let _ = lead_mb.write_status("lead", "working", None);

    // Build the user message: text first (if any), then one Image
    // block per attachment. Some providers (Anthropic) prefer images
    // before text for cache efficiency, but the agent's history is
    // logical — providers serialize whatever order is best for them.
    let mut user_content: Vec<ContentBlock> = Vec::new();
    if !trimmed.is_empty() {
        user_content.push(ContentBlock::text(trimmed));
    }
    for (media_type, data) in images {
        user_content.push(ContentBlock::Image {
            source: crate::types::ImageSource::Base64 { media_type, data },
        });
    }

    let stream = Box::pin(state.agent.run_turn_multipart(user_content));
    drive_turn_stream(stream, state, events_tx, cancel, &lead_mb).await;
}

/// Drive an agent run_turn stream to completion, emitting ViewEvents
/// to both the chat and terminal tabs. Extracted so handle_line and
/// handle_line_with_images share the streaming loop unchanged.
async fn drive_turn_stream(
    mut stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<AgentEvent, crate::error::Error>> + Send>,
    >,
    state: &mut WorkerState,
    events_tx: &broadcast::Sender<ViewEvent>,
    cancel: &Arc<AtomicBool>,
    lead_mb: &crate::team::Mailbox,
) {
    while let Some(ev) = stream.next().await {
        if cancel.load(Ordering::Relaxed) {
            let _ = events_tx.send(ViewEvent::ErrorText("(interrupted)".into()));
            write_lead_log(&state.lead_log, "\x1b[0m\n\x1b[33m[cancelled]\x1b[0m\n");
            save_history(&state.agent, &mut state.session, &state.session_store);
            let _ = events_tx.send(ViewEvent::SessionListRefresh(build_session_list(
                &state.session_store,
                &state.session.id,
            )));
            let _ = events_tx.send(ViewEvent::TurnDone);
            let _ = lead_mb.write_status("lead", "active", None);
            return;
        }
        match ev {
            Ok(AgentEvent::Text(s)) => {
                write_lead_log(&state.lead_log, &s);
                let _ = events_tx.send(ViewEvent::AssistantTextDelta(s));
            }
            Ok(AgentEvent::ToolCallStart { name, input, .. }) => {
                let label = format_tool_label(&name, &input);
                write_lead_log(
                    &state.lead_log,
                    &format!("\x1b[0m\n\x1b[90m[tool: {name}]\x1b[0m "),
                );
                let _ = events_tx.send(ViewEvent::ToolCallStart { name, label });
            }
            Ok(AgentEvent::ToolCallResult { name, output, .. }) => {
                let out = output.unwrap_or_else(|e| e);
                write_lead_log(&state.lead_log, "\x1b[90m✓\x1b[0m\n\x1b[32m");
                let _ = events_tx.send(ViewEvent::ToolCallResult { name, output: out });
            }
            Ok(AgentEvent::Done { usage, .. }) => {
                write_lead_log(&state.lead_log, "\x1b[0m\n");
                let _ = lead_mb.write_status("lead", "active", None);
                // Record token usage for /usage (parity with the CLI
                // REPL — option C's chat port missed this, so the
                // GUI shell silently dropped every turn's usage
                // regardless of provider).
                let provider_name = state.config.detect_provider().unwrap_or("unknown");
                let tracker =
                    crate::usage::UsageTracker::new(crate::usage::UsageTracker::default_path());
                tracker.record(provider_name, &state.config.model, &usage);

                save_history(&state.agent, &mut state.session, &state.session_store);
                maybe_warn_file_size(state, events_tx);
                let _ = events_tx.send(ViewEvent::SessionListRefresh(build_session_list(
                    &state.session_store,
                    &state.session.id,
                )));
                let _ = events_tx.send(ViewEvent::TurnDone);
            }
            Err(e) => {
                write_lead_log(
                    &state.lead_log,
                    &format!("\x1b[0m\n\x1b[33merror: {e}\x1b[0m\n"),
                );
                let _ = lead_mb.write_status("lead", "active", None);
                let _ = events_tx.send(ViewEvent::ErrorText(format!("Error: {e}")));
                let _ = events_tx.send(ViewEvent::TurnDone);
            }
            _ => {}
        }
    }
}

fn write_lead_log(log: &std::sync::Arc<std::sync::Mutex<Option<std::fs::File>>>, s: &str) {
    use std::io::Write;
    if let Ok(mut guard) = log.lock() {
        if let Some(ref mut f) = *guard {
            let _ = f.write_all(s.as_bytes());
            let _ = f.flush();
        }
    }
}

async fn handle_team_messages(
    msgs: Vec<crate::team::TeamMessage>,
    state: &mut WorkerState,
    events_tx: &broadcast::Sender<ViewEvent>,
    cancel: &Arc<AtomicBool>,
) {
    if msgs.is_empty() {
        return;
    }

    // UI-friendly header (chat/terminal) — don't dump the raw XML wrappers.
    let senders: Vec<String> = {
        let mut seen = Vec::<String>::new();
        for m in &msgs {
            if !seen.iter().any(|s| s == &m.from) {
                seen.push(m.from.clone());
            }
        }
        seen
    };
    let header = format!("[teammate messages from: {}]", senders.join(", "));
    let _ = events_tx.send(ViewEvent::SlashOutput(header.clone()));
    write_lead_log(&state.lead_log, &format!("\n\x1b[36m{header}\x1b[0m\n"));
    for m in &msgs {
        let preview: String = m.content().chars().take(300).collect();
        write_lead_log(
            &state.lead_log,
            &format!("\x1b[36m[from {}]\x1b[0m {}\n", m.from, preview),
        );
    }
    write_lead_log(&state.lead_log, "\x1b[32m");

    // Agent-facing prompt — same XML framing repl.rs uses so the model
    // sees a consistent format for teammate reports across CLI and GUI.
    let combined: Vec<String> = msgs
        .iter()
        .map(|m| {
            let summary = m.summary.as_deref().unwrap_or("");
            format!(
                "<teammate_message from=\"{}\" summary=\"{}\">\n{}\n</teammate_message>",
                m.from,
                summary,
                m.content()
            )
        })
        .collect();
    let prompt = combined.join("\n\n");

    let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
    let _ = lead_mb.write_status("lead", "working", None);

    let mut stream = Box::pin(state.agent.run_turn(prompt));
    while let Some(ev) = stream.next().await {
        if cancel.load(Ordering::Relaxed) {
            let _ = events_tx.send(ViewEvent::ErrorText("(interrupted)".into()));
            write_lead_log(&state.lead_log, "\x1b[0m\n\x1b[33m[cancelled]\x1b[0m\n");
            save_history(&state.agent, &mut state.session, &state.session_store);
            let _ = events_tx.send(ViewEvent::SessionListRefresh(build_session_list(
                &state.session_store,
                &state.session.id,
            )));
            let _ = events_tx.send(ViewEvent::TurnDone);
            let _ = lead_mb.write_status("lead", "active", None);
            return;
        }
        match ev {
            Ok(AgentEvent::Text(s)) => {
                write_lead_log(&state.lead_log, &s);
                let _ = events_tx.send(ViewEvent::AssistantTextDelta(s));
            }
            Ok(AgentEvent::ToolCallStart { name, input, .. }) => {
                let label = format_tool_label(&name, &input);
                write_lead_log(
                    &state.lead_log,
                    &format!("\x1b[0m\n\x1b[90m[tool: {name}]\x1b[0m "),
                );
                let _ = events_tx.send(ViewEvent::ToolCallStart { name, label });
            }
            Ok(AgentEvent::ToolCallResult { name, output, .. }) => {
                let out = output.unwrap_or_else(|e| e);
                write_lead_log(&state.lead_log, "\x1b[90m✓\x1b[0m\n\x1b[32m");
                let _ = events_tx.send(ViewEvent::ToolCallResult { name, output: out });
            }
            Ok(AgentEvent::Done { usage, .. }) => {
                write_lead_log(&state.lead_log, "\x1b[0m\n");
                let _ = lead_mb.write_status("lead", "active", None);
                let provider_name = state.config.detect_provider().unwrap_or("unknown");
                let tracker =
                    crate::usage::UsageTracker::new(crate::usage::UsageTracker::default_path());
                tracker.record(provider_name, &state.config.model, &usage);
                save_history(&state.agent, &mut state.session, &state.session_store);
                let _ = events_tx.send(ViewEvent::SessionListRefresh(build_session_list(
                    &state.session_store,
                    &state.session.id,
                )));
                let _ = events_tx.send(ViewEvent::TurnDone);
            }
            Err(e) => {
                write_lead_log(
                    &state.lead_log,
                    &format!("\x1b[0m\n\x1b[33merror: {e}\x1b[0m\n"),
                );
                let _ = lead_mb.write_status("lead", "active", None);
                let _ = events_tx.send(ViewEvent::ErrorText(format!("Error: {e}")));
                let _ = events_tx.send(ViewEvent::TurnDone);
            }
            _ => {}
        }
    }
}

/// System-prompt addendum that grounds the model in thClaws's team
/// feature and pushes back against Claude Code training-data bias.
fn team_grounding_prompt(model: &str, team_enabled: bool) -> String {
    let kind = crate::providers::ProviderKind::detect(model);
    let on_claude_sdk = matches!(kind, Some(crate::providers::ProviderKind::AgentSdk));

    if !team_enabled && !on_claude_sdk {
        return String::new();
    }

    // Special case: teamEnabled is on, but the user picked agent/* —
    // which shells to the local `claude` CLI subprocess. That
    // subprocess uses Claude Code's own built-in toolset and does NOT
    // see thClaws's tool registry. So our `TeamCreate` /
    // `SpawnTeammate` / etc. are registered in our registry but are
    // unreachable by the model. Telling the model to use them would
    // be telling it to call tools it cannot see.
    if team_enabled && on_claude_sdk {
        return String::from(
            "# Agent Teams — UNREACHABLE on this provider\n\n\
             The user has enabled thClaws's team feature \
             (`teamEnabled: true`), but they are also running on the \
             `agent/*` provider — which shells to the local `claude` \
             CLI as a subprocess. That subprocess uses Claude Code's \
             own built-in toolset (`Agent`, `Bash`, `Edit`, `Read`, \
             `ScheduleWakeup`, `Skill`, `ToolSearch`, `Write`) and \
             does NOT see thClaws's tool registry.\n\n\
             This means thClaws's `TeamCreate`, `SpawnTeammate`, \
             `SendMessage`, `CheckInbox`, `TeamStatus`, \
             `TeamTaskCreate`/`List`/`Claim`/`Complete`, and \
             `TeamMerge` tools are REGISTERED in thClaws but are \
             unreachable from your current toolset. You literally \
             cannot call them.\n\n\
             Claude Code's own `TeamCreate` / `Agent` / `TodoWrite` / \
             `AskUserQuestion` / `ToolSearch` / `SendMessage` \
             built-ins are available to you, but they write state \
             under `~/.claude/teams/` and `~/.claude/tasks/` which is \
             invisible to the thClaws Team tab. Calling them produces \
             a fabricated success — the user sees an empty Team tab.\n\n\
             If the user asks you to \"create a team\" / \"spawn agents\":\n\
             - Explain that thClaws's team tools are unreachable from \
             the `agent/*` provider (their tool registry doesn't \
             cross the CLI subprocess boundary).\n\
             - Tell them to switch to a non-`agent/*` provider — e.g. \
             `claude-sonnet-4-6`, `claude-opus-4-7`, `gpt-4o`, etc. — \
             via `/model` or `/provider`. Once switched, thClaws's \
             team tools are directly callable.\n\
             - Offer to proceed sequentially without a team if they \
             prefer to stay on the `agent/*` model.\n\n\
             Do NOT pretend a team has been created. Do NOT call \
             Claude Code's built-in `TeamCreate` etc. as a substitute. \
             The honest answer is the only useful one.\n",
        );
    }

    if !team_enabled {
        return String::from(
            "# Agent Teams — DISABLED in this workspace\n\n\
             The user has NOT enabled thClaws's team feature \
             (`teamEnabled: true` is missing from `.thclaws/settings.json`). \
             thClaws's team tools (`TeamCreate`, `SpawnTeammate`, `SendMessage`, \
             `CheckInbox`, `TeamStatus`, `TeamTaskCreate/List/Claim/Complete`, \
             `TeamMerge`) are NOT registered in this session and you cannot \
             call them.\n\n\
             You are running under the local `claude` CLI subprocess \
             (Anthropic Agent SDK), which DOES ship its own `TeamCreate`, \
             `Agent`, `TodoWrite`, `AskUserQuestion`, `ToolSearch`, \
             `SendMessage` built-ins backed by `~/.claude/teams/` and \
             `~/.claude/tasks/`. DO NOT CALL THEM. Their state is invisible \
             to thClaws — the Team tab polls `.thclaws/team/agents/` locally \
             and will never see an SDK-created team, so the user gets a \
             fabricated success story with nothing behind it.\n\n\
             If the user asks you to \"create a team\" / \"spawn agents\" / \
             \"set up a team of subagents\", respond in plain text:\n\
             - Explain that thClaws's team feature is off in this workspace.\n\
             - Tell them to set `teamEnabled: true` in `.thclaws/settings.json` \
             (or globally in `~/.config/thclaws/settings.json`) and restart \
             the app.\n\
             - Offer to proceed WITHOUT a team by handling the task yourself \
             sequentially.\n\n\
             Do NOT claim to have created a team, spawned teammates, written \
             config, or stored state. Do NOT reference `~/.claude/teams/` or \
             `~/.claude/tasks/` paths. The only honest response is \"teams are \
             disabled\" — anything else is a hallucination.\n",
        );
    }

    let mut out = String::from(
        "# Agent Teams (thClaws native)\n\n\
         This workspace has thClaws's team feature ENABLED. When the user asks for \
         parallel work via a team, use ONLY these thClaws tools — they are the \
         canonical implementation and their state is visible in the Team tab:\n\n\
         - `TeamCreate` — define a team (name + member agents with roles/prompts). \
         Writes `.thclaws/team/config.json` in the current project root.\n\
         - `SpawnTeammate` — start one named teammate. Spawns a thClaws subprocess \
         that polls its inbox in a tmux pane (or background).\n\
         - `SendMessage` — deliver a message to a teammate's inbox.\n\
         - `CheckInbox` — read your own inbox.\n\
         - `TeamStatus` — summarise the team.\n\
         - `TeamTaskCreate` / `TeamTaskList` / `TeamTaskClaim` / `TeamTaskComplete` — \
         a shared task queue teammates can claim from.\n\
         - `TeamMerge` — (lead only) merge each teammate's git worktree back into \
         the main branch.\n\n\
         Team state lives under `.thclaws/team/` **in the current project root** — \
         NOT under `~/.claude/teams/`, NOT under `~/.claude/tasks/`. Do not reference \
         those paths; they are from a different product.\n\n\
         You are the team **lead**. After `TeamCreate`:\n\
         1. Do NOT use `Bash`/`Write`/`Edit` to build code — delegate via `SendMessage`.\n\
         2. Use `TeamTaskCreate` to queue work; teammates claim via `TeamTaskClaim`.\n\
         3. Use `Read`/`Glob`/`Grep` only for review and verification.\n\
         4. Watch `CheckInbox` / `TeamStatus` between coordination rounds.\n\
         \n\
         **Worktree isolation is declarative.** If a teammate should work on \
         an isolated branch, set `isolation: \"worktree\"` on that member when \
         you call `TeamCreate`. `SpawnTeammate` then creates \
         `.worktrees/{name}` on branch `team/{name}` automatically and \
         launches the teammate there. DO NOT write `git worktree add …` or \
         `cd ../{name}` into teammate prompts — the teammate will execute them \
         as shell and the worktree will land somewhere wrong (project root, a \
         sibling dir) and be invisible to `TeamMerge`.\n\
         \n\
         # CRITICAL: do NOT call Claude Code's Agent SDK team tools\n\n\
         Your training data contains references to an Anthropic Managed Agents \
         SDK server-side toolset (`agent_toolset_20260401`) that ships its own \
         `TeamCreate`, `Agent`, `AskUserQuestion`, `TodoWrite`, `ToolSearch`, \
         `SendMessage` tools backed by `~/.claude/teams/` and `~/.claude/tasks/`. \
         Those are a DIFFERENT SYSTEM, invisible to thClaws — if you call them \
         (or claim to have called them in your text output), the user will see \
         an empty Team tab and think nothing happened.\n\n\
         Rules that apply regardless of which provider you are running on:\n\
         - When the user asks about \"teams\" / \"agents\" / \"task queue\", use \
         the thClaws tools listed above. `TeamCreate` and `SendMessage` in this \
         workspace mean the thClaws versions — never the SDK's.\n\
         - Never reference `~/.claude/teams/`, `~/.claude/tasks/`, or \
         `~/.config/thclaws/teams/` paths in your replies. Teams live in \
         `.thclaws/team/`.\n\
         - Do not call `AskUserQuestion`, `TodoWrite`, `ToolSearch`, or a bare \
         `Agent` tool. Those belong to Claude Code's interactive flow and do \
         not exist in thClaws. If you need a task list, use `TeamTaskCreate`. \
         If you need to ask the user, just ask them in plain text.\n\
         - Do not claim to have created a team, spawned agents, or stored \
         config unless you actually called the corresponding thClaws tool and \
         got a success response back.\n",
    );

    if on_claude_sdk {
        out.push_str(
            "\n# Additional note for the Claude Agent SDK provider\n\n\
             You ARE running under the local `claude` CLI subprocess right now, \
             which ships its own `TeamCreate`, `Agent`, `AskUserQuestion`, \
             `TodoWrite`, and `ToolSearch` built-ins. Calling them will appear \
             to succeed inside Claude Code's own world, but the thClaws Team \
             tab polls `.thclaws/team/agents/` and will never see a team \
             created that way. Treat any impulse to call those tools as a bug.\n",
        );
    }

    out
}

fn format_tool_label(name: &str, input: &serde_json::Value) -> String {
    let detail = match name {
        "Skill" => input
            .get("name")
            .and_then(|v| v.as_str())
            .map(|n| format!("({n})")),
        "Task" => input
            .get("agent")
            .and_then(|v| v.as_str())
            .map(|a| format!("(agent={a})")),
        "Bash" => input.get("command").and_then(|v| v.as_str()).map(|c| {
            let first: String = c.chars().take(40).collect();
            format!("({first}{})", if c.chars().count() > 40 { "…" } else { "" })
        }),
        "Read" | "Write" | "Edit" => input
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| format!("({p})")),
        "Grep" | "Glob" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|p| format!("({p})")),
        "WebFetch" => input
            .get("url")
            .and_then(|v| v.as_str())
            .map(|u| format!("({})", u.chars().take(60).collect::<String>())),
        "WebSearch" => input
            .get("query")
            .and_then(|v| v.as_str())
            .map(|q| format!("({q})")),
        "AskUserQuestion" => input.get("question").and_then(|v| v.as_str()).map(|q| {
            let first: String = q.chars().take(60).collect();
            format!(
                "({first}{})",
                if q.chars().count() > 60 { "..." } else { "" }
            )
        }),
        _ => None,
    }
    .unwrap_or_default();
    if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name} {detail}")
    }
}

/// Placeholder provider used when the worker starts without any usable
/// LLM credentials. `stream()` immediately errors with a
/// configure-a-key message so the user sees actionable feedback on the
/// first send instead of an infinitely spinning request. The agent and
/// loop are kept alive so a `ReloadConfig` (sent by the GUI after
/// `api_key_set`) can swap this out for a real provider in place.
struct NoopProvider {
    msg: String,
}

impl NoopProvider {
    fn new(msg: impl Into<String>) -> Self {
        Self { msg: msg.into() }
    }
}

#[async_trait]
impl Provider for NoopProvider {
    async fn stream(&self, _req: StreamRequest) -> CoreResult<EventStream> {
        Err(Error::Provider(self.msg.clone()))
    }
}

/// True if this provider is usable without further setup — either
/// because the env var holding its API key is set, or because it
/// doesn't need one (Ollama variants, Agent SDK using Claude Code's
/// own auth). Mirrors `gui::kind_has_credentials` without the
/// `#[cfg(feature = "gui")]` gate so the shared worker can call it.
fn kind_has_credentials(kind: crate::providers::ProviderKind) -> bool {
    use crate::providers::ProviderKind;
    match kind {
        ProviderKind::AgentSdk => true,
        ProviderKind::Ollama | ProviderKind::OllamaAnthropic => true,
        other => other
            .api_key_env()
            .map(|v| std::env::var(v).is_ok())
            .unwrap_or(false),
    }
}

/// Auto-compact at 80% of `agent.budget_tokens`. Cheap drop-oldest
/// (no LLM call), persists a checkpoint event so the next `/load`
/// starts from the compacted view. Emits a dim `[compacted: N → M]`
/// slash-output so the user knows earlier messages dropped out of the
/// provider's context window.
pub(crate) fn maybe_auto_compact(
    state: &mut WorkerState,
    events_tx: &broadcast::Sender<ViewEvent>,
) {
    let history = state.agent.history_snapshot();
    if history.is_empty() {
        return;
    }
    let budget = state.agent.budget_tokens;
    let current = crate::compaction::estimate_messages_tokens(&history);
    let threshold = (budget as f64 * 0.8) as usize;
    if current <= threshold {
        return;
    }
    // Target a shrink to ~50% of budget so we don't retrigger
    // on the very next turn just because we added one more.
    let target = budget / 2;
    let compacted = crate::compaction::compact(&history, target);
    if compacted.len() >= history.len() {
        // `compact()` couldn't find anywhere to trim (e.g. all
        // history is one big recent turn). Nothing to persist.
        return;
    }
    state.agent.set_history(compacted.clone());
    if let Some(store) = &state.session_store {
        let path = store.path_for(&state.session.id);
        let _ = state.session.append_compaction_to(&path, &compacted);
    }
    let _ = events_tx.send(ViewEvent::SlashOutput(format!(
        "[compacted: {} → {} messages — context over 80% of budget]",
        history.len(),
        compacted.len()
    )));
}

/// 5 MB fork suggestion. Checks the session file's byte size after
/// saves. Fires [`ViewEvent::ContextWarning`] exactly once per
/// session (sticky `warned_file_size` flag on WorkerState).
pub(crate) fn maybe_warn_file_size(
    state: &mut WorkerState,
    events_tx: &broadcast::Sender<ViewEvent>,
) {
    if state.warned_file_size {
        return;
    }
    const THRESHOLD_BYTES: u64 = 5 * 1024 * 1024;
    let Some(store) = &state.session_store else {
        return;
    };
    let path = store.path_for(&state.session.id);
    let Ok(meta) = std::fs::metadata(&path) else {
        return;
    };
    if meta.len() < THRESHOLD_BYTES {
        return;
    }
    state.warned_file_size = true;
    let mb = meta.len() as f64 / (1024.0 * 1024.0);
    let _ = events_tx.send(ViewEvent::ContextWarning { file_size_mb: mb });
}
