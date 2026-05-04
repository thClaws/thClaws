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
    /// Widget-initiated tool call from an embedded MCP App. The
    /// originating widget called `app.callServerTool({name, arguments})`;
    /// we look up the qualified tool in the registry, run it, and
    /// broadcast a [`ViewEvent::McpAppCallToolResult`] keyed by the
    /// same `request_id` so the frontend can route the response back
    /// to the iframe. No approval gate — the trust check already
    /// happened at the marketplace install boundary (see dev-log/112).
    McpAppCallTool {
        request_id: String,
        qualified_name: String,
        arguments: serde_json::Value,
    },
    /// M6.19 BUG M2: a `session_delete` IPC just removed `id` from
    /// disk. If the worker's in-flight session matches, it must mint
    /// a fresh session — otherwise the next save_history would
    /// re-create the deleted file and the session would resurrect
    /// with stale state. No-op if `id` doesn't match the current
    /// session.
    SessionDeletedExternal { id: String },
    /// M6.19 BUG M2: a `session_rename` IPC just changed the title of
    /// `id` on disk. If the worker's in-flight session matches, sync
    /// the in-memory `state.session.title` so subsequent slash
    /// commands (e.g. `/sessions`) reflect the new value. No-op if
    /// `id` doesn't match the current session.
    SessionRenamedExternal { id: String, title: String },
}

/// What both tabs render. Each variant maps to a UI affordance:
/// Chat → bubbles + tool blocks, Terminal → ANSI-formatted bytes.
#[derive(Debug, Clone)]
pub enum ViewEvent {
    UserPrompt(String),
    AssistantTextDelta(String),
    /// A chunk of the model's reasoning (`reasoning_content` from
    /// DeepSeek v4 / OpenAI o-series / NVIDIA NIM glm4.7 / etc., or
    /// `<think>`-tagged spans from implicit thinking models). Chat
    /// renders it dimmed/collapsed above the assistant text; terminal
    /// renders it dim-italic so the live thinking is visible without
    /// looking like the model's final answer.
    AssistantThinkingDelta(String),
    ToolCallStart {
        name: String,
        label: String,
        /// Raw JSON input the model passed to the tool. Carried so the
        /// chat translator can render rich cards for tools whose input
        /// is itself the user-visible payload (TodoWrite's `todos`
        /// array, for instance). Most tools' inputs aren't worth
        /// surfacing — the translator decides per tool name.
        input: serde_json::Value,
    },
    ToolCallResult {
        name: String,
        output: String,
        /// MCP-Apps widget to embed inline alongside this tool's
        /// result. Carried verbatim from [`crate::agent::AgentEvent`]
        /// so the frontend translator can ship it on the
        /// `chat_tool_result` IPC envelope.
        ui_resource: Option<crate::tools::UiResource>,
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
    /// Sidebar MCP server list refresh — pre-built JSON payload shaped
    /// like `{type: "mcp_update", servers: [{name, tools}, ...]}`.
    /// Emitted after `/mcp add | remove` so the sidebar reflects the
    /// new state without waiting for the next full session_update.
    McpUpdate(String),
    /// Goal-state sidebar refresh (Phase A). Carries the latest snapshot
    /// of the active /goal — `None` means the goal was cleared. Frontend
    /// renders a compact indicator (objective, iterations, tokens
    /// used/budget, status) above the plan sidebar.
    GoalUpdate(Option<crate::goal_state::GoalState>),
    /// Open the GUI's interactive model picker — pre-built JSON payload
    /// shaped like `{type: "model_picker_open", provider, current,
    /// models: [{id, context, max_output}, ...]}`. Emitted by the
    /// `/model` slash command when invoked without arguments (#25).
    /// The CLI renderer ignores this — a CLI TUI picker is a future
    /// follow-up.
    ModelPickerOpen(String),
    /// The session's on-disk JSONL has crossed the fork threshold.
    /// Frontend renders a dismissible banner with a "Fork into new
    /// session with summary" action. Fired once per session.
    ContextWarning {
        file_size_mb: f64,
    },
    ErrorText(String),
    /// Result of a widget-initiated tool call. Pairs with a
    /// [`ShellInput::McpAppCallTool`] of the same `request_id`. The
    /// event translator converts this into an
    /// `mcp_call_tool_result` IPC envelope so the frontend's pending
    /// promise can resolve and the iframe gets its JSON-RPC reply.
    McpAppCallToolResult {
        request_id: String,
        /// MCP `CallToolResult.content` — array of content blocks
        /// shaped per spec (`{type:"text", text}`, etc.). Carried
        /// as raw JSON so the wire format is opaque to Rust.
        content: serde_json::Value,
        is_error: bool,
    },
    /// Worker → event-loop signal: the user invoked `/quit` in the
    /// chat input, the confirmation dialog was accepted, and the GUI
    /// should now shut down. The translator forwards this to a
    /// `UserEvent::QuitRequested` so the tao loop runs the same
    /// save-and-exit path as the window-close button. Issue #52.
    QuitRequested,
    /// Active plan changed. `Some(plan)` for submit / update_step,
    /// `None` for clear. The translator forwards this as a
    /// `chat_plan_update` IPC envelope to the right-side
    /// `PlanSidebar`. Plan-mode rebuild M1.
    PlanUpdate(Option<crate::tools::plan_state::Plan>),
    /// Permission mode changed (M2). Carried to the sidebar so the
    /// status pill / mode badge can update without polling. Fired by
    /// EnterPlanMode / ExitPlanMode, `/plan`, sidebar Approve / Cancel.
    PermissionModeChanged(crate::permissions::PermissionMode),
    /// Stalled-turn detector tripped (M4.4). The model has finished N
    /// consecutive turns without a plan mutation while a plan is
    /// active and a step is in progress. Sidebar shows a yellow
    /// "model seems stuck" banner with Continue / Abort buttons.
    /// `step_id` and `step_title` identify the step the model
    /// appears to be stuck on; `turns` is the consecutive count.
    PlanStalled {
        step_id: String,
        step_title: String,
        turns: usize,
    },
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
    /// Cooperative cancel handle (M6.17 BUGs H1 + M3). Replaces the
    /// bare `Arc<AtomicBool>` so the worker can `select!` on async
    /// cancellation rather than polling the flag only between stream
    /// events. Call `request_cancel()` to flip the flag AND wake
    /// every active `cancelled().await`.
    pub cancel: crate::cancel::CancelToken,
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
        self.cancel.cancel();
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
    /// Cooperative cancel handle, shared with the worker loop and
    /// (via `with_cancel`) the agent. M6.17 BUG H1 + M3 — wired into
    /// `rebuild_agent` so a `/model` swap doesn't lose the cancel
    /// plumbing.
    pub cancel: crate::cancel::CancelToken,
    /// M6.29: active iteration loop. `Some` when `/loop <interval>
    /// <body>` is running; the cancel handle aborts the spawned tokio
    /// task on `/loop stop` / session swap / goal-terminal.
    pub active_loop: Option<ActiveLoop>,
    /// Phase B2 (mirror of codex's empty-turn anti-loop): `true` if the
    /// most recent agent turn fired at least one ToolCallStart event.
    /// Set false at the start of each turn, flipped true on the first
    /// tool call. Read by the `/goal continue` intercept — when an
    /// active /loop fires it after a turn that produced no tool calls
    /// (model just monologued, no concrete progress), the next firing
    /// is suppressed once, so glm-class reasoning models can't burn the
    /// loop budget on pure thinking. Init `true` so the very first
    /// /loop /goal continue isn't pre-suppressed.
    pub last_turn_made_tool_calls: bool,
}

/// M6.29: handle to a running `/loop` task.
#[derive(Debug)]
pub struct ActiveLoop {
    pub interval_secs: Option<u64>,
    pub body: String,
    pub started_at: u64,
    pub iterations_fired: u64,
    pub abort: tokio::task::AbortHandle,
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
        .with_approver(self.approver.clone())
        .with_cancel(self.cancel.clone())
        // M6.35 HOOK1: re-snapshot config.hooks on rebuild — config
        // edits via Settings → save → ReloadConfig take effect on the
        // next agent. Pre-fix the snapshot was only at first boot.
        .with_hooks(std::sync::Arc::new(self.config.hooks.clone()));
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
            // dev-plan/06 P2: branch on the user's chosen strategy.
            // - "full" preserves the original behavior (every skill
            //   listed with name + description + trigger)
            // - "names-only" lists names only, refers the model to
            //   the SkillSearch / SkillList / Skill tools for detail
            // - "discover-tool-only" lists no skills at all; just
            //   names the discovery tools
            let strategy = config.skills_listing_strategy.as_str();
            append_skills_section(&mut system, store, strategy);
        }
    }

    system
}

/// dev-plan/06 P2 helper. Renders the Available-skills section of the
/// system prompt according to the configured strategy.
fn append_skills_section(system: &mut String, store: &crate::skills::SkillStore, strategy: &str) {
    let mut entries: Vec<&crate::skills::SkillDef> = store.skills.values().collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    match strategy {
        "discover-tool-only" => {
            system.push_str("\n\n# Available skills (MANDATORY usage)\n");
            system.push_str(
                "Bundled skills are available but not listed inline (you have \
                 a large catalog). Discover them via `SkillList()` for the full \
                 catalog or `SkillSearch(query: \"...\")` for a substring \
                 lookup. When a user request sounds like it might match a \
                 bundled workflow (\"make a PDF\", \"scaffold a skill\", \
                 \"extract data from xlsx\", etc.), you MUST call SkillList \
                 or SkillSearch FIRST before implementing the task manually. \
                 Once you find a relevant skill, call `Skill(name: \"<name>\")` \
                 to load its expert instructions and follow them.\n",
            );
        }
        "names-only" => {
            system.push_str("\n\n# Available skills (MANDATORY usage)\n");
            system.push_str(
                "The `Skill` tool loads expert instructions for a bundled \
                 workflow. Skill names are listed below; for descriptions and \
                 trigger criteria call `SkillSearch(query: \"...\")` or \
                 `SkillList()`. If a user request might match any of these \
                 skills, you MUST call Skill (or SkillSearch first) FIRST — \
                 before any Bash, Write, Edit, or other tool calls for that \
                 task. Announce the skill at the start of your reply.\n\n",
            );
            let names: Vec<&str> = entries.iter().map(|s| s.name.as_str()).collect();
            // Render as a comma-separated list to keep token cost minimal
            // — one line per N skills, ~30 chars per name.
            system.push_str(&names.join(", "));
            system.push('\n');
        }
        _ => {
            // "full" (default) — preserves the original behavior.
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
            for skill in entries {
                system.push_str(&format!("- **{}** — {}", skill.name, skill.description));
                if !skill.when_to_use.is_empty() {
                    system.push_str(&format!("\n  Trigger: {}", skill.when_to_use));
                }
                system.push('\n');
            }
        }
    }
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
    let cancel = crate::cancel::CancelToken::new();
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
    cancel: crate::cancel::CancelToken,
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

    // Plan-state → ViewEvent bridge + JSONL persistence (M1). Every
    // time a plan tool calls `submit` / `update_step` / `clear`, the
    // broadcaster registered here:
    //   1. turns the snapshot into a `ViewEvent::PlanUpdate` so the
    //      right-side sidebar redraws
    //   2. appends a `plan_snapshot` event to the active session's
    //      JSONL (path tracked via the arc below; updated whenever
    //      `state.session` is reassigned — `/new`, `/load`, `/fork`)
    //
    // Registered before any tool can run so an early SubmitPlan call
    // from the model still gets both the broadcast and the persisted
    // snapshot. Replaces any prior registration — there's only one
    // active worker per GUI process at a time.
    let plan_persist_path: std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    {
        let plan_tx = events_tx.clone();
        let path_arc = plan_persist_path.clone();
        crate::tools::plan_state::set_broadcaster(move |plan_opt| {
            let _ = plan_tx.send(ViewEvent::PlanUpdate(plan_opt.clone()));
            if let Ok(g) = path_arc.lock() {
                if let Some(p) = g.as_ref() {
                    let _ = crate::session::append_plan_snapshot(p, plan_opt.as_ref());
                }
            }
        });
    }

    // Goal-state → ViewEvent bridge + JSONL persistence (Phase A). Same
    // pattern as plan_state above; reuses `plan_persist_path` because
    // both snapshot kinds always target the same session JSONL — every
    // session swap (via /new, /load, /fork) needs to retarget both at
    // once anyway, and sharing the Arc means we don't have two paths
    // that can drift out of sync. Locks are independent per-call so
    // there's no extra contention.
    {
        let goal_tx = events_tx.clone();
        let path_arc = plan_persist_path.clone();
        crate::goal_state::set_broadcaster(move |goal_opt| {
            let _ = goal_tx.send(ViewEvent::GoalUpdate(goal_opt.cloned()));
            if let Ok(g) = path_arc.lock() {
                if let Some(p) = g.as_ref() {
                    let _ = crate::session::append_goal_snapshot(p, goal_opt);
                }
            }
        });
    }

    if !config.kms_active.is_empty() {
        tools.register(std::sync::Arc::new(crate::tools::KmsReadTool));
        tools.register(std::sync::Arc::new(crate::tools::KmsSearchTool));
        // M6.25 BUG #1: KmsWrite + KmsAppend make the LLM an active
        // wiki maintainer (not just a passive reader).
        tools.register(std::sync::Arc::new(crate::tools::KmsWriteTool));
        tools.register(std::sync::Arc::new(crate::tools::KmsAppendTool));
    }

    // M6.26 BUG #1: Memory tools always-on. The model needs them even
    // when no entries exist yet (so it can create the first one). Sandbox
    // carve-out validated by `memory::writable_entry_path`.
    tools.register(std::sync::Arc::new(crate::tools::MemoryReadTool));
    tools.register(std::sync::Arc::new(crate::tools::MemoryWriteTool));
    tools.register(std::sync::Arc::new(crate::tools::MemoryAppendTool));

    // M6.11 (H1): daily auto-refresh of the marketplace catalog. No-op
    // when the cache is < 24h old; otherwise spawns a fail-silent
    // background fetch so newly-added skills appear without the user
    // having to remember /skill marketplace --refresh. Mirrors the
    // pattern the model catalogue uses.
    crate::marketplace::spawn_daily_auto_refresh();
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
    // M6.34 TEAM3: capture team_dir so the GUI's lead-process exit
    // can scope the kill to its own teammates only. Even though the
    // GUI doesn't currently call kill_my_teammates() at shutdown
    // (the OS reclaims child processes when the GUI quits), recording
    // the dir keeps parity with the CLI lead and unblocks future
    // explicit "Stop all teammates" UI affordances.
    if team_enabled && !is_teammate {
        let td = std::env::var("THCLAWS_TEAM_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| crate::team::Mailbox::default_dir());
        crate::team::set_lead_team_dir(&td);
    }
    let skill_tool = crate::skills::SkillTool::new_from_handle(skill_store.clone());
    tools.register(std::sync::Arc::new(skill_tool));
    // dev-plan/06 P2: SkillList + SkillSearch are always registered
    // (regardless of skills_listing_strategy) so any strategy can use
    // them. Under "names-only" / "discover-tool-only" the system
    // prompt explicitly directs the model to call these.
    let skill_list = crate::skills::SkillListTool::new_from_handle(skill_store.clone());
    tools.register(std::sync::Arc::new(skill_list));
    let skill_search = crate::skills::SkillSearchTool::new_from_handle(skill_store.clone());
    tools.register(std::sync::Arc::new(skill_search));

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

    // M6.35 HOOK1+HOOK10: snapshot HooksConfig in an Arc so the agent +
    // every subagent factory build shares one immutable copy. Register
    // a broadcaster that forwards hook errors (spawn fail / non-zero
    // exit / timeout) to the chat surface so users see broken hooks
    // without tailing stderr.
    let hooks_arc = std::sync::Arc::new(config.hooks.clone());
    {
        let err_tx = events_tx.clone();
        crate::hooks::set_error_broadcaster(move |msg| {
            let _ = err_tx.send(ViewEvent::SlashOutput(format!("⚠ {msg}")));
        });
    }
    let provider: Arc<dyn Provider> = maybe_provider.unwrap_or_else(|| {
        Arc::new(NoopProvider::new(
            "no LLM provider configured — open Settings → Provider API keys to add one",
        ))
    });
    // M6.33 SUB1 + SUB4: register the Task tool in the GUI worker.
    // Pre-fix the Task tool was only registered in the CLI's run_repl,
    // so the GUI agent silently lacked subagents — any agent_def call
    // came back "unknown tool: Task". SUB4: cancel is threaded into
    // the factory so ctrl-C in the GUI stops in-flight subagents
    // (CLI passes None — no cancel plumbing there yet).
    let perm_mode = if config.permissions == "auto" {
        crate::permissions::PermissionMode::Auto
    } else {
        crate::permissions::PermissionMode::Ask
    };
    {
        let plugin_agent_dirs = crate::plugins::plugin_agent_dirs();
        let agent_defs = crate::agent_defs::AgentDefsConfig::load_with_extra(&plugin_agent_dirs);
        let base_tools = tools.clone();
        let factory = Arc::new(crate::subagent::ProductionAgentFactory {
            provider: provider.clone(),
            base_tools,
            model: config.model.clone(),
            system: system.clone(),
            max_iterations: config.max_iterations,
            max_depth: crate::subagent::DEFAULT_MAX_DEPTH,
            agent_defs: agent_defs.clone(),
            approver: approver.clone(),
            permission_mode: perm_mode,
            cancel: Some(cancel.clone()),
            // M6.35 HOOK1: subagents inherit GUI worker's hooks so audit
            // hooks see Task-spawned tool calls.
            hooks: Some(hooks_arc.clone()),
        });
        tools.register(std::sync::Arc::new(
            crate::subagent::SubAgentTool::new(factory)
                .with_depth(0)
                .with_agent_defs(agent_defs),
        ));
    }
    let mut agent = Agent::new(provider, tools.clone(), &config.model, &system)
        .with_approver(approver.clone())
        .with_cancel(cancel.clone())
        .with_hooks(hooks_arc.clone());
    // Respect the user's configured permission mode (project
    // `.thclaws/settings.json` can set it to "ask"). Without this the
    // GUI's Ask mode flag had no effect because the Agent was built
    // with the default Auto.
    agent.permission_mode = perm_mode;
    // Mirror the configured mode into the process-wide global so
    // `permissions::current_mode()` (read by the agent's tool-dispatch
    // gate, M2+) starts on the right value before any EnterPlanMode /
    // sidebar-Approve flip can change it.
    crate::permissions::set_current_mode(agent.permission_mode);

    // Permission-mode → ViewEvent bridge (M2). Mirrors the plan-state
    // broadcaster — every set_current_mode_and_broadcast() call
    // (EnterPlanMode, ExitPlanMode, /plan, sidebar Approve/Cancel)
    // turns into a `ViewEvent::PermissionModeChanged` so the sidebar
    // status pill updates without polling.
    {
        let mode_tx = events_tx.clone();
        crate::permissions::set_mode_broadcaster(move |mode| {
            let _ = mode_tx.send(ViewEvent::PermissionModeChanged(mode));
        });
    }

    let session_store = SessionStore::default_path().map(SessionStore::new);
    let current_session = Session::new(&config.model, cwd.to_string_lossy());
    // Point the plan-persistence arc at the initial session's JSONL
    // path so any SubmitPlan / UpdatePlanStep call before the first
    // /load gets persisted. Subsequent session swaps reassign this
    // arc — see the helper at the call sites below.
    if let (Some(store), Ok(mut g)) = (session_store.as_ref(), plan_persist_path.lock()) {
        let path = store.path_for(&current_session.id);
        // Write the header BEFORE pointing plan_persist_path at this
        // file. Otherwise the first plan_state mutation (typically
        // restore_from_session below) races append_plan_snapshot to
        // the empty path, creates the file headerless, and the
        // session becomes invisible to SessionStore::list. Same
        // pattern at every other Session::new site below.
        let _ = current_session.write_header_if_missing(&path);
        *g = Some(path);
    }
    // Reset plan_state to whatever the initial session has (None for
    // a fresh `Session::new`, but Some(plan) for a session loaded
    // off disk that already had a plan_snapshot in its JSONL).
    crate::tools::plan_state::restore_from_session(current_session.plan.clone());
    // Same restore for goal_state — the broadcaster fires
    // ViewEvent::GoalUpdate so the sidebar picks up a /load.
    crate::goal_state::restore_from_session(current_session.goal.clone());

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
        cancel: cancel.clone(),
        active_loop: None,
        // Init true: the very first /loop /goal continue firing
        // happens before any turn has run, so the suppression check
        // would otherwise gate the loop forever on iteration 0.
        last_turn_made_tool_calls: true,
    };

    // M6.35 HOOK2: fire session_start hook now that WorkerState is
    // built (state.session.id + state.config.model are stable). Pre-fix
    // the entire hooks subsystem was orphaned — this is the first
    // place a session_start hook ever runs.
    crate::hooks::fire_session(
        &hooks_arc,
        crate::hooks::HookEvent::SessionStart,
        &state.session.id,
        &state.config.model,
    );

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
                    // M6.34 TEAM5: send to the worker channel BEFORE
                    // marking as read on disk. Pre-fix order was
                    // mark-then-send: if `send` failed (worker
                    // dropped), the messages were already flagged read
                    // on disk so a subsequent session would never
                    // surface them — silent message loss. Post-fix:
                    // only mark when the send succeeded; if the
                    // channel is closed, leave the messages unread so
                    // a future session sees them.
                    if poller_tx.send(ShellInput::TeamMessages(unread)).is_err() {
                        return;
                    }
                    let _ = mailbox.mark_as_read("lead", &ids);
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
                cancel.reset();
                handle_line(text, &mut state, &events_tx, &cancel, &input_tx_self).await;
            }
            ShellInput::LineWithImages { text, images } => {
                cancel.reset();
                handle_line_with_images(
                    text,
                    images,
                    &mut state,
                    &events_tx,
                    &cancel,
                    &input_tx_self,
                )
                .await;
            }
            ShellInput::NewSession => {
                save_history(&state.agent, &mut state.session, &state.session_store);
                state.agent.clear_history();
                state.session = Session::new(&state.config.model, state.cwd.to_string_lossy());
                state.warned_file_size = false;
                // New session = clean slate for plan state and the
                // persistence path. Broadcasts `PlanUpdate(None)` so
                // the sidebar dismisses if it was open.
                if let (Some(store), Ok(mut g)) =
                    (state.session_store.as_ref(), plan_persist_path.lock())
                {
                    let path = store.path_for(&state.session.id);
                    let _ = state.session.write_header_if_missing(&path);
                    *g = Some(path);
                }
                crate::tools::plan_state::clear();
                // M6.20 BUG M2: clear any "allow for session" yolo flag
                // from the prior session — a fresh session must prompt
                // again rather than silently auto-approving inherited
                // from session A.
                state.approver.reset_session_flag();
                // M6.20 BUG M3: reset permission mode + clear pre-plan
                // stash. Plan-mode entry from the prior session would
                // otherwise leak into the fresh session, leaving the
                // user in Plan with no plan-state to submit against.
                let _ = crate::permissions::take_pre_plan_mode();
                crate::permissions::set_current_mode_and_broadcast(state.agent.permission_mode);
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
                    // M6.19 BUG M1: capture prev_model BEFORE the
                    // assignment so rebuild_agent failure can roll the
                    // config back. Pre-fix the in-memory state.config
                    // got the new model but the agent kept the old
                    // provider — subsequent turns ran the old agent
                    // against config.model that no longer matched, and
                    // the on-disk settings.json wasn't yet written, so
                    // restart silently lost the swap.
                    let prev_model =
                        std::mem::replace(&mut state.config.model, loaded.model.clone());
                    if let Err(e) = state.rebuild_agent(false) {
                        // Roll back the config so it matches the still-
                        // active agent. The user sees the error and the
                        // session stays on its previous model.
                        state.config.model = prev_model;
                        let _ = events_tx.send(ViewEvent::ErrorText(format!(
                            "Auto-switch to {} failed: {e}",
                            loaded.model
                        )));
                        continue;
                    }
                    let provider_name = target_kind.name();
                    let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                        "(auto-switched to {} to match session)",
                        format_provider_model(provider_name, &loaded.model)
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
                // /load: repoint persistence at the loaded session's
                // JSONL and restore plan_state so the sidebar comes
                // back populated if the loaded session had a plan
                // snapshot. M1+ — decision #1 in dev-plan/03.
                if let (Some(store), Ok(mut g)) =
                    (state.session_store.as_ref(), plan_persist_path.lock())
                {
                    *g = Some(store.path_for(&state.session.id));
                }
                crate::tools::plan_state::restore_from_session(state.session.plan.clone());
                crate::goal_state::restore_from_session(state.session.goal.clone());
                // M6.9 (Bug E1): reset the per-step attempt counter
                // on session swap. The counter is process-global and
                // would otherwise leak across sessions — if the prior
                // session had attempts at 2/3 on a step.id that the
                // loaded session also uses, the driver would
                // immediately force-Failed on its first nudge.
                crate::tools::plan_state::reset_step_attempts_external();
                // M6.20 BUG M2 + M3: clear yolo flag and reset
                // permission mode from the prior session. Pre-fix the
                // user's "allow for session" decision from session A
                // continued to auto-approve in session B, and a Plan
                // mode set in A leaked into B with no plan to submit.
                state.approver.reset_session_flag();
                let _ = crate::permissions::take_pre_plan_mode();
                crate::permissions::set_current_mode_and_broadcast(state.agent.permission_mode);
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
                cancel.reset();
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
            ShellInput::McpAppCallTool {
                request_id,
                qualified_name,
                arguments,
            } => {
                // Widget asked us to invoke a tool on its originating
                // MCP server (app.callServerTool). Trust at widget-
                // render time only gates HTML rendering, NOT
                // unattended tool execution — M6.15 BUG 2 routes
                // widget tool-calls through the same approval gate
                // the agent loop uses so a trusted server's widget
                // can't silently invoke `delete_*`-style tools when
                // the user has set permission_mode = "ask".
                let tool = state.tool_registry.get(&qualified_name);
                let (content, is_error) = match tool {
                    Some(t) => {
                        let mode = crate::permissions::current_mode();
                        // M6.24 BUG M4: in Plan mode, structurally
                        // BLOCK mutating widget tool calls — match
                        // the agent loop's behavior at agent.rs:1133.
                        // Pre-fix the widget path treated Plan as
                        // "ask" (prompted via approval modal), but a
                        // user could click Allow on a widget-side
                        // button while believing they were just
                        // exploring. Plan mode = read-only
                        // exploration, period.
                        if matches!(mode, crate::permissions::PermissionMode::Plan)
                            && t.requires_approval(&arguments)
                        {
                            let blocked = format!(
                                "Blocked: {qualified_name} is not available in plan mode. \
                                 Plan mode is read-only exploration — exit plan mode \
                                 (sidebar Approve/Cancel) before triggering tool actions \
                                 from MCP widgets.",
                            );
                            let _ = events_tx.send(ViewEvent::McpAppCallToolResult {
                                request_id,
                                content: serde_json::json!([{
                                    "type": "text",
                                    "text": blocked,
                                }]),
                                is_error: true,
                            });
                            continue;
                        }
                        let needs_approval =
                            matches!(mode, crate::permissions::PermissionMode::Ask,)
                                && t.requires_approval(&arguments);
                        let denied = if needs_approval {
                            let req = crate::permissions::ApprovalRequest {
                                tool_name: qualified_name.clone(),
                                input: arguments.clone(),
                                summary: Some(format!(
                                    "MCP-App widget requested `{qualified_name}`. Allow?"
                                )),
                            };
                            matches!(
                                state.approver.approve(&req).await,
                                crate::permissions::ApprovalDecision::Deny
                            )
                        } else {
                            false
                        };
                        if denied {
                            (
                                serde_json::json!([{
                                    "type": "text",
                                    "text": format!("denied by user: {qualified_name}"),
                                }]),
                                true,
                            )
                        } else {
                            match t.call_multimodal(arguments).await {
                                Ok(result) => {
                                    // Convert ToolResultContent → MCP
                                    // CallToolResult.content shape.
                                    // Phase 1 is text-only — image
                                    // blocks degrade to their text
                                    // summary via to_text. Pinn.ai
                                    // image2image returns a URL
                                    // string, so text-only suffices.
                                    let text = result.to_text();
                                    (serde_json::json!([{ "type": "text", "text": text }]), false)
                                }
                                Err(e) => (
                                    serde_json::json!([{ "type": "text", "text": format!("error: {e}") }]),
                                    true,
                                ),
                            }
                        }
                    }
                    None => (
                        serde_json::json!([{ "type": "text", "text": format!("unknown tool: {qualified_name}") }]),
                        true,
                    ),
                };
                let _ = events_tx.send(ViewEvent::McpAppCallToolResult {
                    request_id,
                    content,
                    is_error,
                });
            }
            ShellInput::SessionDeletedExternal { id } => {
                // M6.19 BUG M2: a session_delete IPC just removed `id`
                // from disk. If it matches the worker's current
                // session, mint a fresh one — otherwise the next
                // save_history would resurrect the deleted file with
                // stale state. No-op if the deleted id wasn't
                // current.
                if state.session.id == id {
                    save_history(&state.agent, &mut state.session, &state.session_store);
                    state.agent.clear_history();
                    state.session = Session::new(&state.config.model, state.cwd.to_string_lossy());
                    state.warned_file_size = false;
                    if let (Some(store), Ok(mut g)) =
                        (state.session_store.as_ref(), plan_persist_path.lock())
                    {
                        let path = store.path_for(&state.session.id);
                        let _ = state.session.write_header_if_missing(&path);
                        *g = Some(path);
                    }
                    crate::tools::plan_state::clear();
                    // M6.20 BUG M2 + M3: same reset on external delete
                    // of the active session (sidebar trash icon while
                    // in yolo mode would otherwise carry the flag into
                    // the freshly-minted replacement).
                    state.approver.reset_session_flag();
                    let _ = crate::permissions::take_pre_plan_mode();
                    crate::permissions::set_current_mode_and_broadcast(state.agent.permission_mode);
                    let _ = events_tx.send(ViewEvent::HistoryReplaced(Vec::new()));
                    let _ = events_tx.send(ViewEvent::SessionListRefresh(build_session_list(
                        &state.session_store,
                        &state.session.id,
                    )));
                    let _ = events_tx.send(ViewEvent::SlashOutput(
                        "(active session was deleted; minted a fresh session)".into(),
                    ));
                }
            }
            ShellInput::SessionRenamedExternal { id, title } => {
                // M6.19 BUG M2: keep the worker's in-memory title in
                // sync after a session_rename IPC. No-op when the
                // renamed id isn't the current session.
                if state.session.id == id {
                    let trimmed = title.trim();
                    state.session.title = if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    };
                }
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
                            if let (Some(store), Ok(mut g)) =
                                (state.session_store.as_ref(), plan_persist_path.lock())
                            {
                                let path = store.path_for(&state.session.id);
                                let _ = state.session.write_header_if_missing(&path);
                                *g = Some(path);
                            }
                            crate::tools::plan_state::clear();
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
                            "(provider reloaded: {})",
                            format_provider_model(provider_name, &state.config.model)
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
                // dispatcher before sending this. Here we refresh the
                // worker's view: save the OLD session, then mint a
                // fresh session under the new project, clear plan +
                // ephemeral mode state, and rebuild the agent.
                let prev_model = state.config.model.clone();

                // M6.31 PM1: save the OLD session FIRST, while
                // session_store still points at the OLD project. Any
                // unsaved messages land in the OLD project's session
                // file rather than getting silently re-routed to the
                // NEW project.
                save_history(&state.agent, &mut state.session, &state.session_store);

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

                // Rebuild `session_store` against the NEW cwd. Without
                // this, `save_history` and `build_session_list` stay
                // pinned to the previous workspace's `.thclaws/sessions/`,
                // so saves land in the wrong project and the sidebar
                // never reflects the new project's sessions.
                state.session_store =
                    crate::session::SessionStore::default_path().map(SessionStore::new);

                // If the model changed, rebuild the agent without history
                // — the new provider's message schema may not match the
                // old conversation, same logic as `/model` swap. Even if
                // rebuild_agent fails, fall through to the unconditional
                // hygiene block so plan state from the OLD project doesn't
                // leak (PM1).
                let model_changed = state.config.model != prev_model;
                if model_changed {
                    if let Err(e) = state.rebuild_agent(false) {
                        let _ = events_tx.send(ViewEvent::ErrorText(format!(
                            "[cwd-change] agent rebuild failed: {e} (model stays on '{prev_model}')"
                        )));
                    }
                }

                // M6.31 PM1: UNCONDITIONAL hygiene block. Pre-fix this
                // ran only when model_changed; same-model workspace
                // switch left state.session pointing at OLD session id +
                // plan_persist_path pointing at OLD project's .jsonl +
                // plan_state holding OLD project's plan + pre_plan stash
                // + approver yolo flag all leaked. Resulted in writes to
                // the wrong location and OLD plan appearing in NEW
                // project's sidebar. Same hygiene as NewSession +
                // LoadSession.
                state.agent.clear_history();
                state.session =
                    crate::session::Session::new(&state.config.model, state.cwd.to_string_lossy());
                state.warned_file_size = false;
                if let (Some(store), Ok(mut g)) =
                    (state.session_store.as_ref(), plan_persist_path.lock())
                {
                    let path = store.path_for(&state.session.id);
                    let _ = state.session.write_header_if_missing(&path);
                    *g = Some(path);
                }
                crate::tools::plan_state::clear();
                crate::tools::plan_state::reset_step_attempts_external();
                state.approver.reset_session_flag();
                let _ = crate::permissions::take_pre_plan_mode();
                crate::permissions::set_current_mode_and_broadcast(state.agent.permission_mode);
                let _ = events_tx.send(ViewEvent::HistoryReplaced(Vec::new()));

                // Always rebuild the system prompt — the cwd it embeds
                // changed, even if the model didn't.
                state.rebuild_system_prompt();

                // Broadcast the new project's session list so the
                // sidebar redraws. Mirrors what `/new` and `/load` do
                // after they mutate the same store.
                let _ = events_tx.send(ViewEvent::SessionListRefresh(build_session_list(
                    &state.session_store,
                    &state.session.id,
                )));

                let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                    "[cwd] {} → model: {} (was: {})",
                    new_cwd.display(),
                    state.config.model,
                    prev_model
                )));
            }
        }
    }

    // M6.35 HOOK2: input_rx loop exited (channel closed by handle drop /
    // GUI shutdown). Fire session_end so audit hooks can record the
    // close. Best-effort — the hook spawn is fire-and-forget and the
    // tokio runtime is about to shut down with the worker, so any
    // hook child that's still booting may be killed by the runtime
    // teardown. For long-running notification hooks, prefer foreground
    // commands that exec quickly (`notify-send`, `osascript -e ...`)
    // over slow shell pipelines.
    crate::hooks::fire_session(
        &hooks_arc,
        crate::hooks::HookEvent::SessionEnd,
        &state.session.id,
        &state.config.model,
    );
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
    cancel: &crate::cancel::CancelToken,
    input_tx: &mpsc::Sender<ShellInput>,
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

    // `!<cmd>` shell escape — user-initiated shell command that doesn't
    // touch the agent. Output is shown via SlashOutput and is NOT
    // pushed to agent history (same shape as slash commands). Routes
    // through BashTool so it inherits sandbox cwd restriction, the
    // M6.8 non-interactive env vars, venv auto-activation, and the
    // destructive-command + lead/teammate guards.
    if let Some(cmd) = crate::shell_bang::parse_bang(trimmed) {
        match crate::shell_bang::run_bang_command(cmd).await {
            Ok(output) => {
                let _ = events_tx.send(ViewEvent::SlashOutput(format!("[!] {cmd}\n{output}")));
            }
            Err(e) => {
                let _ = events_tx.send(ViewEvent::ErrorText(format!("[!] {cmd}\n{e}")));
            }
        }
        let _ = events_tx.send(ViewEvent::TurnDone);
        return;
    }

    // M6.27: `# <name>:<body>` memory-shortcut intercept (Claude Code
    // parity). `parse_slash` recognizes the shortcut and returns
    // `SlashCommand::MemoryWrite`; route through `shell_dispatch` so
    // the same write path runs as `/memory write --body ...`. Strict
    // pattern (slug-only name + colon) means real markdown headers
    // like `# Architecture Plan: ...` fall through to the agent
    // unchanged.
    if matches!(
        crate::repl::parse_slash(trimmed),
        Some(crate::repl::SlashCommand::MemoryWrite { .. })
    ) && !trimmed.starts_with('/')
    {
        crate::shell_dispatch::dispatch(trimmed, state, events_tx, input_tx).await;
        let _ = events_tx.send(ViewEvent::TurnDone);
        return;
    }

    // M6.29: `/goal continue` intercept — fires the audit prompt as
    // an agent turn (composes with `/loop /goal continue`). Same
    // rewrite-before-match pattern as `/kms ingest <name> $`. If no
    // active goal or goal already terminal, surface a notice and
    // stop the active loop.
    if matches!(
        crate::repl::parse_slash(trimmed),
        Some(crate::repl::SlashCommand::GoalContinue)
    ) {
        match crate::goal_state::current() {
            Some(g) if g.status.is_terminal() => {
                let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                    "/goal continue — goal already {}. Stopping loop if active.",
                    g.status.as_str(),
                )));
                if let Some(loop_state) = state.active_loop.take() {
                    loop_state.abort.abort();
                }
                let _ = events_tx.send(ViewEvent::TurnDone);
                return;
            }
            Some(g) => {
                // Phase B2: anti-loop guard mirroring codex's runtime
                // continuation suppression. If a /loop is wrapping us
                // AND the previous turn produced zero tool calls (model
                // monologued without doing anything concrete), skip
                // this firing once and let the next interval try again.
                // Reset the flag on suppression so we don't dead-loop.
                if state.active_loop.is_some() && !state.last_turn_made_tool_calls {
                    let _ = events_tx.send(ViewEvent::SlashOutput(
                        "(/goal continue suppressed: prior turn made no tool calls — \
                         model just monologued. Will retry next /loop firing.)"
                            .into(),
                    ));
                    state.last_turn_made_tool_calls = true;
                    let _ = events_tx.send(ViewEvent::TurnDone);
                    return;
                }
                let prompt = crate::goal_state::build_audit_prompt(&g);
                crate::goal_state::record_iteration(0);
                let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                    "(/goal continue → audit prompt fired — iteration {}, {}s elapsed)",
                    g.iterations_done.saturating_add(1),
                    g.time_used_secs(),
                )));
                if let Some(l) = state.active_loop.as_mut() {
                    l.iterations_fired = l.iterations_fired.saturating_add(1);
                }
                let stream = Box::pin(state.agent.run_turn(prompt));
                let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
                let _ = lead_mb.write_status("lead", "working", None);
                drive_turn_stream(stream, state, events_tx, cancel, &lead_mb, input_tx).await;
                // Post-turn: if the model called MarkGoalComplete /
                // MarkGoalBlocked (or any path that mutated status to
                // terminal), stop the loop so the next firing doesn't run.
                if let Some(g) = crate::goal_state::current() {
                    if g.status.is_terminal() {
                        if let Some(loop_state) = state.active_loop.take() {
                            loop_state.abort.abort();
                            let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                                "loop auto-stopped (goal {})",
                                g.status.as_str(),
                            )));
                        }
                    } else if g.auto_continue
                        && state.active_loop.is_none()
                        && state.last_turn_made_tool_calls
                        && !cancel.is_cancelled()
                    {
                        // Phase D1: opt-in auto-continuation. The goal
                        // was started with --auto, no /loop is wrapping
                        // (would double-fire), the just-finished turn
                        // made tool calls (Phase B2 empty-turn guard
                        // would otherwise re-trigger here too), and the
                        // user didn't cancel. Queue another /goal
                        // continue immediately so the next iteration
                        // fires without waiting for /loop interval.
                        // std::sync::mpsc — sync send, no .await. If the
                        // worker channel is somehow disconnected the send
                        // errors silently and the user can fire /goal
                        // continue manually to recover.
                        let _ = input_tx.send(crate::shared_session::ShellInput::Line(
                            "/goal continue".into(),
                        ));
                    }
                }
                return;
            }
            None => {
                let _ = events_tx.send(ViewEvent::SlashOutput(
                    "/goal continue — no active goal. Try /goal start \"<objective>\" first."
                        .into(),
                ));
                let _ = events_tx.send(ViewEvent::TurnDone);
                return;
            }
        }
    }

    // M6.28: `/kms ingest <name> $` intercept — the `$` source means
    // "the current chat session". Page slug resolves from
    // session.title (if set) or session.id (fallback). Rewrite into a
    // turn-starting prompt that instructs the model to summarize
    // history and call `KmsWrite`.
    if let Some(crate::repl::SlashCommand::KmsIngestSession { name, alias, force }) =
        crate::repl::parse_slash(trimmed)
    {
        let (page, source) = crate::repl::resolve_session_alias(
            alias.as_deref(),
            state.session.title.as_deref(),
            &state.session.id,
        );
        let rewritten = crate::repl::build_kms_ingest_session_prompt(&name, &page, source, force);
        let _ = events_tx.send(ViewEvent::SlashOutput(format!(
            "(/kms ingest {name} $ → page `{page}` — summarize and KmsWrite)"
        )));
        let stream = Box::pin(state.agent.run_turn(rewritten));
        let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
        let _ = lead_mb.write_status("lead", "working", None);
        drive_turn_stream(stream, state, events_tx, cancel, &lead_mb, input_tx).await;
        return;
    }

    if trimmed.starts_with('/') {
        // `/<word> [args]` shortcut — same UX + resolution order as
        // the CLI repl (see repl.rs:2601-2632). If `parse_slash`
        // returns Unknown, try to resolve `<word>` against:
        //   1. installed skills → rewrite into a "call Skill(...)"
        //      prompt
        //   2. user / plugin prompt commands (.md templates) →
        //      render the template body with $ARGUMENTS substitution
        // and fall through to the regular agent pipeline. Without
        // this fallback, every custom command surfaced via the
        // slash popup landed as "unknown command" in the GUI even
        // though the command was discoverable in the popup.
        if let Some(crate::repl::SlashCommand::Unknown(what)) = crate::repl::parse_slash(trimmed) {
            let word = what.split_whitespace().next().unwrap_or("").to_string();
            let body = trimmed.strip_prefix('/').unwrap_or("").trim_start();
            let args = body.strip_prefix(&word).unwrap_or("").trim();

            // (1) Skill lookup.
            let skill_present = state
                .skill_store
                .lock()
                .ok()
                .map(|s| s.skills.contains_key(&word))
                .unwrap_or(false);
            if skill_present {
                let args_note = if args.is_empty() {
                    String::new()
                } else {
                    format!(" The user's task for this skill: {args}")
                };
                let rewritten = format!(
                    "The user ran the `/{word}` slash command. Call `Skill(name: \"{word}\")` right away and follow the instructions it returns.{args_note}"
                );
                emit_skill_resolution_hint(events_tx, &word);
                let stream = Box::pin(state.agent.run_turn(rewritten));
                let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
                let _ = lead_mb.write_status("lead", "working", None);
                drive_turn_stream(stream, state, events_tx, cancel, &lead_mb, input_tx).await;
                return;
            }

            // (2) Custom command (.md template) lookup. Re-discover
            // each call so freshly-installed plugins surface without
            // a restart — matches the popup's discover-each-render
            // pattern in gui.rs:1835. The plugin_command_dirs
            // extras include both user-scope and project-scope
            // plugin contributions.
            let command_store = crate::commands::CommandStore::discover_with_extra(
                &crate::plugins::plugin_command_dirs(),
            );
            if let Some(cmd) = command_store.get(&word).cloned() {
                let rewritten = cmd.render(args);
                emit_command_resolution_hint(events_tx, &word, &cmd.source);
                let stream = Box::pin(state.agent.run_turn(rewritten));
                let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
                let _ = lead_mb.write_status("lead", "working", None);
                drive_turn_stream(stream, state, events_tx, cancel, &lead_mb, input_tx).await;
                return;
            }
        }

        crate::shell_dispatch::dispatch(trimmed, state, events_tx, input_tx).await;
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
    drive_turn_stream(stream, state, events_tx, cancel, &lead_mb, input_tx).await;
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
    cancel: &crate::cancel::CancelToken,
    input_tx: &mpsc::Sender<ShellInput>,
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
    drive_turn_stream(stream, state, events_tx, cancel, &lead_mb, input_tx).await;
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
    cancel: &crate::cancel::CancelToken,
    lead_mb: &crate::team::Mailbox,
    input_tx: &mpsc::Sender<ShellInput>,
) {
    // Phase B2: reset the empty-turn flag at the start of every turn.
    // Flipped to true on the first ToolCallStart below; if the model
    // produces zero tool calls during this turn, the next /loop /goal
    // continue firing gets suppressed once.
    state.last_turn_made_tool_calls = false;
    loop {
        // M6.17 BUG H1: race the next stream event against the cancel
        // signal so a long tool run / stalled provider stream doesn't
        // delay the user's Stop button. Pre-fix the cancel flag was
        // only checked between events, so the user could click Stop
        // and wait seconds to minutes before anything happened.
        let ev = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
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
            ev = stream.next() => ev,
        };
        let Some(ev) = ev else { break };
        match ev {
            Ok(AgentEvent::Text(s)) => {
                write_lead_log(&state.lead_log, &s);
                let _ = events_tx.send(ViewEvent::AssistantTextDelta(s));
            }
            Ok(AgentEvent::Thinking(s)) => {
                let _ = events_tx.send(ViewEvent::AssistantThinkingDelta(s));
            }
            Ok(AgentEvent::ToolCallStart { name, input, .. }) => {
                state.last_turn_made_tool_calls = true;
                let label = format_tool_label(&name, &input);
                write_lead_log(
                    &state.lead_log,
                    &format!("\x1b[0m\n\x1b[90m[tool: {name}]\x1b[0m "),
                );
                let _ = events_tx.send(ViewEvent::ToolCallStart { name, label, input });
            }
            Ok(AgentEvent::ToolCallResult {
                name,
                output,
                ui_resource,
                ..
            }) => {
                let out = output.unwrap_or_else(|e| e);
                write_lead_log(&state.lead_log, "\x1b[90m✓\x1b[0m\n\x1b[32m");
                let _ = events_tx.send(ViewEvent::ToolCallResult {
                    name,
                    output: out,
                    ui_resource,
                });
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

    // Stalled-turn detector (M4.4). After every agent turn that ended
    // naturally (not via cancellation — the early return above skips
    // this branch), check if the plan made progress. If the active
    // plan still has an InProgress step and no UpdatePlanStep
    // mutation reset the counter, increment. Cross the threshold and
    // we broadcast PlanStalled so the sidebar can prompt the user.
    if let Some(plan) = crate::tools::plan_state::get() {
        let in_progress = plan
            .steps
            .iter()
            .find(|s| s.status == crate::tools::plan_state::StepStatus::InProgress);
        if let Some(step) = in_progress {
            let turns = crate::tools::plan_state::note_turn_completed_without_progress();
            // M6.31 PM2: rising-edge only. Pre-fix `>=` fired
            // PlanStalled on every subsequent turn after crossing the
            // threshold (turn 3 → fire, turn 4 → fire again, turn 5 →
            // fire again, …) — sidebar saw repeated banners until the
            // user clicked Continue. `==` fires once when the counter
            // first hits the threshold; any plan mutation
            // (UpdatePlanStep, force_step_done, the sidebar's
            // Continue button) resets the counter and re-arms the
            // detector for the next 3 unproductive turns.
            if turns == crate::tools::plan_state::STALL_TURN_THRESHOLD {
                let _ = events_tx.send(ViewEvent::PlanStalled {
                    step_id: step.id.clone(),
                    step_title: step.title.clone(),
                    turns,
                });
            }
        }
    }

    // Plan-execution driver (M6.1, "Ralph loop"). Replaces the older
    // dumb "Continue with the plan." nudge with a step-aware loop:
    // each turn end, we look at the plan, find the next actionable
    // step, and push a focused per-step continuation prompt that wakes
    // the worker loop with that one step in scope.
    //
    // Why this shape: the worker is an event loop driven by the
    // `input_rx` channel. Pushing a `ShellInput::Line` here is the
    // existing path for "run another turn" — we keep that, but make
    // the message specific to the next step instead of a generic
    // continue. The agent's system reminder (via build_execution_
    // reminder) already narrows the model's view to the focused step,
    // so the per-step user message is intentionally terse — it just
    // says "go, your focus is step N".
    //
    // Per-step retry budget: `note_step_attempt` returns 1 on the
    // first nudge for a given step id, 2 on the second, etc. Once we
    // exceed `MAX_RETRIES_PER_STEP` (3 by default) on the same step
    // without it transitioning to Done or Failed, we mark the step
    // Failed automatically — the user gets the standard Retry / Skip /
    // Abort sidebar path instead of the loop spinning forever. This
    // is the "force iteration to completion" guarantee the Ralph
    // architecture provides over the prior monolithic auto-continue.
    //
    // Bounded by:
    //   - Plan completion (auto-restore flips mode out of Auto when
    //     the last step transitions to Done — see plan_state).
    //   - User cancel (clears the plan).
    //   - User Approve flow (mode == Plan keeps the driver dormant
    //     while the sidebar buttons are the contract).
    //   - Per-step retry budget (force-Failed after N nudges).
    //   - Stalled-turn detector — fires PlanStalled banner above so
    //     the user can intervene via Continue / Abort if a step's
    //     budget hasn't run out yet but the model is clearly stuck.
    //   - Agent's own max_iterations cap (per inner run_turn call).
    if let Some(plan) = crate::tools::plan_state::get() {
        let mode = crate::permissions::current_mode();
        let waiting_for_approval = matches!(mode, crate::permissions::PermissionMode::Plan);
        if !waiting_for_approval {
            // M6.7: yield to the user when the earliest non-Done step
            // is Failed. The Layer-1 gate would reject any attempt to
            // start a downstream Todo while a prior step is Failed, so
            // pushing per-step prompts there only burns the retry
            // budget on a step that can't possibly start. The user
            // owns recovery via the sidebar's Retry / Skip / Abort
            // buttons; the driver waits.
            //
            // Without this, the prior real-world test session
            // bounced between attempt-1/2/3 prompts on step 3 while
            // step 2 stayed Failed, eventually marking step 3 Failed
            // for "max retries exceeded" — when step 3 was never
            // actually unblocked.
            use crate::tools::plan_state::StepStatus;
            let earliest_unfinished = plan.steps.iter().find(|s| s.status != StepStatus::Done);
            let upstream_failed = matches!(
                earliest_unfinished.map(|s| s.status),
                Some(StepStatus::Failed),
            );
            if upstream_failed {
                // Plan blocked on user action — don't push another
                // prompt. The sidebar already shows the Failed step
                // with Retry / Skip / Abort.
                return;
            }
            // Find the next actionable step: first one that's still
            // Todo or InProgress. Failed and Done are skipped — Failed
            // because the user owns that recovery, Done because we're
            // moving past it.
            let next = plan
                .steps
                .iter()
                .find(|s| matches!(s.status, StepStatus::Todo | StepStatus::InProgress));
            if let Some(step) = next {
                let attempt = crate::tools::plan_state::note_step_attempt(&step.id);

                // M6.2 step-boundary compaction. `attempt == 1` means
                // the per-step counter just reset, which only happens
                // when we cross a step boundary (different step id
                // from last time). Combined with "at least one step
                // is now Done" — there's actual completed work in
                // history worth compacting — this fires the structural
                // shrink before pushing the next per-step prompt, so
                // the agent's upcoming turn starts with a leaner
                // history. Plan-tool tool_results are preserved
                // untouched (they're the breadcrumbs the model uses to
                // know what's done); non-plan tool_results from
                // pre-boundary messages are replaced with a short
                // placeholder.
                let any_done = plan.steps.iter().any(|s| s.status == StepStatus::Done);
                if attempt == 1 && any_done {
                    let mut history = state.agent.history_snapshot();
                    // M6.4: strategy picked from config. Defaults to
                    // "compact" (M6.2 structural shrink); "clear"
                    // wipes history outright keeping only the first
                    // user message for project-level grounding.
                    let (changed, notice) = match state.config.plan_context_strategy.as_str() {
                        "clear" => {
                            let dropped = crate::compaction::clear_for_step_boundary(&mut history);
                            (
                                dropped > 0,
                                format!("[step-boundary cleared: dropped {dropped} messages]"),
                            )
                        }
                        _ => {
                            let saved = crate::compaction::compact_for_step_boundary(&mut history);
                            (
                                saved > 0,
                                format!("[step-boundary compacted: ~{saved} bytes saved]"),
                            )
                        }
                    };
                    if changed {
                        state.agent.set_history(history.clone());
                        // Persist the compaction marker into the
                        // session JSONL so a `/load` after the fact
                        // restores the trimmed history (matches the
                        // existing `maybe_auto_compact` pattern).
                        if let Some(store) = &state.session_store {
                            let path = store.path_for(&state.session.id);
                            let _ = state.session.append_compaction_to(&path, &history);
                        }
                        let _ = events_tx.send(ViewEvent::SlashOutput(notice));
                    }
                }

                if attempt > crate::tools::plan_state::MAX_RETRIES_PER_STEP {
                    // Budget exhausted on this step. Force-mark it
                    // Failed so the user gets a recovery path; the
                    // sidebar's Retry button resets the attempt
                    // counter and lets the model try again.
                    let reason = format!(
                        "max retries per step exceeded ({} attempts) — \
                         the agent looped without committing to done or \
                         failed. Use the sidebar Retry / Skip / Abort \
                         buttons to recover.",
                        crate::tools::plan_state::MAX_RETRIES_PER_STEP,
                    );
                    let _ = crate::tools::plan_state::update_step(
                        &step.id,
                        StepStatus::Failed,
                        Some(reason),
                    );
                    // Don't push another ShellInput — the Failed step
                    // is now waiting on the user.
                } else {
                    let prompt = crate::agent::build_step_continuation_prompt(&plan, step, attempt);
                    let _ = input_tx.send(ShellInput::Line(prompt));
                }
            }
            // No next-actionable step → either all Done (the auto-
            // restore in plan_state already flipped mode and cleared
            // the path), or the only remaining work is Failed (user
            // owns it). Either way: don't nudge.
        }
    }
}

/// Surface the `/skill → Skill(name: …)` resolution to the user the
/// same way the CLI does, so it's clear which skill is about to fire.
fn emit_skill_resolution_hint(events_tx: &broadcast::Sender<ViewEvent>, name: &str) {
    let _ = events_tx.send(ViewEvent::SlashOutput(format!(
        "(/{name} → Skill(name: \"{name}\"))"
    )));
}

fn emit_command_resolution_hint(
    events_tx: &broadcast::Sender<ViewEvent>,
    name: &str,
    source: &std::path::Path,
) {
    let _ = events_tx.send(ViewEvent::SlashOutput(format!(
        "(/{name} → prompt from {})",
        source.display()
    )));
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
    cancel: &crate::cancel::CancelToken,
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
    loop {
        // M6.17 BUG H1: race the next stream event against the cancel
        // signal — same fix as drive_turn_stream above. handle_team_messages
        // calls this function-shaped path inline rather than through
        // drive_turn_stream so it needs its own select! arm.
        let ev = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
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
            ev = stream.next() => ev,
        };
        let Some(ev) = ev else { break };
        match ev {
            Ok(AgentEvent::Text(s)) => {
                write_lead_log(&state.lead_log, &s);
                let _ = events_tx.send(ViewEvent::AssistantTextDelta(s));
            }
            Ok(AgentEvent::Thinking(s)) => {
                let _ = events_tx.send(ViewEvent::AssistantThinkingDelta(s));
            }
            Ok(AgentEvent::ToolCallStart { name, input, .. }) => {
                let label = format_tool_label(&name, &input);
                write_lead_log(
                    &state.lead_log,
                    &format!("\x1b[0m\n\x1b[90m[tool: {name}]\x1b[0m "),
                );
                let _ = events_tx.send(ViewEvent::ToolCallStart { name, label, input });
            }
            Ok(AgentEvent::ToolCallResult {
                name,
                output,
                ui_resource,
                ..
            }) => {
                let out = output.unwrap_or_else(|e| e);
                write_lead_log(&state.lead_log, "\x1b[90m✓\x1b[0m\n\x1b[32m");
                let _ = events_tx.send(ViewEvent::ToolCallResult {
                    name,
                    output: out,
                    ui_resource,
                });
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

/// Squash any control char (newline, carriage return, tab, ESC, etc.)
/// to a single space so a multi-line tool argument renders as one
/// line in the terminal. Keeps printable Unicode (Thai, emoji, etc.)
/// intact — only ASCII control chars get replaced. Then collapses
/// runs of whitespace so a sanitized multi-line string doesn't read
/// as `Line 1   Line 2  ` after stripping.
/// Render `<provider>/<model>` for status-line messages without doubling
/// the provider segment when the model id already carries a routing
/// prefix. Most prefix-routed providers (ollama, ollama-cloud, thaillm,
/// nvidia, openrouter) embed the provider name in the model id; naively
/// prepending it again gives `nvidia/nvidia/<owner>/<name>` which reads
/// like a bug.
fn format_provider_model(provider: &str, model: &str) -> String {
    let prefix = format!("{provider}/");
    if model.starts_with(&prefix) {
        model.to_string()
    } else {
        format!("{prefix}{model}")
    }
}

fn sanitize_label_field(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
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
            // Same control-char strip as AskUserQuestion — bash
            // commands often contain heredocs (`<<'PY' ... PY`) whose
            // newlines break the single-line label.
            let cleaned = sanitize_label_field(c);
            let first: String = cleaned.chars().take(40).collect();
            format!(
                "({first}{})",
                if cleaned.chars().count() > 40 {
                    "…"
                } else {
                    ""
                }
            )
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
            // Strip newlines / control chars first — agents often pass
            // multi-line prompts here, and the raw text breaks the
            // single-line tool label in xterm.
            let cleaned = sanitize_label_field(q);
            let first: String = cleaned.chars().take(60).collect();
            format!(
                "({first}{})",
                if cleaned.chars().count() > 60 {
                    "..."
                } else {
                    ""
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_two() -> crate::skills::SkillStore {
        let mut store = crate::skills::SkillStore::default();
        store.skills.insert(
            "pdf".into(),
            crate::skills::SkillDef::new_eager(
                "pdf".into(),
                "Render PDFs".into(),
                "When user wants a PDF".into(),
                std::path::PathBuf::from("/tmp/pdf"),
                "body-pdf".into(),
            ),
        );
        store.skills.insert(
            "xlsx".into(),
            crate::skills::SkillDef::new_eager(
                "xlsx".into(),
                "Read xlsx files".into(),
                String::new(),
                std::path::PathBuf::from("/tmp/xlsx"),
                "body-xlsx".into(),
            ),
        );
        store
    }

    #[test]
    fn skills_section_full_strategy_lists_descriptions_and_triggers() {
        // dev-plan/06 P2: "full" strategy preserves the original
        // behavior — every skill listed with description + trigger.
        let mut out = String::new();
        let store = store_with_two();
        append_skills_section(&mut out, &store, "full");
        assert!(out.contains("# Available skills (MANDATORY usage)"));
        assert!(out.contains("**pdf**"), "name not bolded: {out}");
        assert!(out.contains("Render PDFs"), "description missing: {out}");
        assert!(out.contains("Trigger:"), "trigger missing: {out}");
        assert!(
            out.contains("ACTUALLY") || out.contains("MUST"),
            "discipline weak: {out}"
        );
    }

    #[test]
    fn skills_section_names_only_strategy_omits_descriptions() {
        // dev-plan/06 P2: "names-only" lists only names, points the
        // model at SkillSearch / SkillList for detail. Big token
        // savings for users with many skills.
        let mut out = String::new();
        let store = store_with_two();
        append_skills_section(&mut out, &store, "names-only");
        assert!(out.contains("# Available skills (MANDATORY usage)"));
        // Names ARE listed.
        assert!(out.contains("pdf"), "name missing: {out}");
        assert!(out.contains("xlsx"), "name missing: {out}");
        // Descriptions / triggers are NOT.
        assert!(!out.contains("Render PDFs"), "description leaked: {out}");
        assert!(!out.contains("Trigger:"), "trigger leaked: {out}");
        // Discovery tools mentioned.
        assert!(
            out.contains("SkillSearch") || out.contains("SkillList"),
            "no discovery hint: {out}"
        );
    }

    #[test]
    fn skills_section_discover_tool_only_omits_names_too() {
        // dev-plan/06 P2: most aggressive — no skill names at all in
        // the listing form. Constant-size system prompt regardless of
        // skill count.
        //
        // Note: the discovery-hint copy contains illustrative examples
        // ("make a PDF", "extract data from xlsx") that mention skill-
        // adjacent words by design. The test asserts the LISTING
        // format isn't present (no "- pdf —" / "**pdf**" / standalone
        // skill name on a line), not raw substring absence.
        let mut out = String::new();
        let store = store_with_two();
        append_skills_section(&mut out, &store, "discover-tool-only");
        assert!(out.contains("# Available skills (MANDATORY usage)"));
        // No skill listing — bullet markers + bolded names + comma
        // joins shouldn't appear.
        assert!(!out.contains("**pdf**"), "bolded listing leaked: {out}");
        assert!(!out.contains("- pdf"), "bullet listing leaked: {out}");
        assert!(!out.contains("- xlsx"), "bullet listing leaked: {out}");
        // Discovery tools mentioned.
        assert!(out.contains("SkillList"), "SkillList not named: {out}");
        assert!(out.contains("SkillSearch"), "SkillSearch not named: {out}");
        // MUST-call discipline preserved.
        assert!(out.contains("MUST"), "MUST discipline missing: {out}");
    }

    #[test]
    fn skills_section_unknown_strategy_falls_back_to_full() {
        // Defensive: unknown strategy strings shouldn't break the
        // system prompt. They should fall back to the safe "full"
        // behavior. The config layer also validates and falls back to
        // "full" silently, but defense-in-depth.
        let mut out = String::new();
        let store = store_with_two();
        append_skills_section(&mut out, &store, "totally-bogus-strategy");
        // Should look like the full-strategy output.
        assert!(out.contains("**pdf**"));
        assert!(out.contains("Render PDFs"));
    }
}
