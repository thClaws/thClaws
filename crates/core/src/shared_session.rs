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
use crate::error::{Error, Result as CoreResult};
use crate::providers::{EventStream, Provider, StreamRequest};
use crate::repl::{build_provider, build_provider_with_fallback};
use crate::session::{Session, SessionStore};
use crate::tools::ToolRegistry;
use crate::types::{ContentBlock, Message, Role};
use async_trait::async_trait;
use futures::{FutureExt, StreamExt};
use std::panic::AssertUnwindSafe;
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
///
/// `Clone` is intentionally *not* derived: `LineMessage` carries
/// a `oneshot::Sender` which is move-only by design (each pending
/// reply has exactly one waiter). Plenty of other variants would
/// have to wrap their payloads in `Arc` just to satisfy `Clone`
/// even though nothing in the codebase actually clones a
/// `ShellInput`.
#[derive(Debug)]
pub enum ShellInput {
    /// Raw line submitted by the user. Slash-prefix → dispatched as
    /// command, anything else → fed to the agent as a prompt.
    Line(String),
    /// Like `Line`, but run on a throwaway child agent whose history
    /// starts empty and is discarded when the turn ends — the shared
    /// session and its running context are never touched. Lets a
    /// GUI-shell agent (e.g. tutorial-studio) fire many independent
    /// one-shot generations without the shared history — and per-turn
    /// input tokens — growing without bound. Streams the same
    /// text/tool_call/tool_result/done events as `Line`. Slash commands
    /// aren't supported here (there's no persistent session to act on).
    IsolatedLine(String),
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
    /// Connect ONE newly-configured MCP server into the running
    /// session — the Connectors surface writes `mcp.json` and sends
    /// this, so a connector added mid-conversation is usable on the
    /// very next turn instead of after a restart. Same spawn →
    /// `McpReady` fan-out the startup and `ChangeCwd` paths use.
    McpConnect(Box<crate::mcp::McpServerConfig>),
    /// Re-discover skills and rebuild around them. Sent after a skill
    /// is installed from a surface that isn't the slash command (the
    /// GUI-Shell skills panel), so the new skill is listed in
    /// `# Available skills` for the very next turn instead of after a
    /// restart.
    SkillsRefresh,
    /// Detach a server that was just removed from `mcp.json`: drop its
    /// tools from the live registry and its client, then rebuild. The
    /// config write alone would leave the tools advertised to the model
    /// (and callable) for the rest of the session.
    McpDisconnect { server_name: String },
    /// Reload `AppConfig` from disk and rebuild the agent's provider in
    /// place. Sent by the GUI after `api_key_set` / `api_key_clear` so
    /// the running session picks up the new key (and any auto-fallback
    /// model swap that just happened) without needing an app restart.
    /// Without this, the sidebar reflects the new provider while the
    /// worker keeps holding the stale one — the exact mismatch users
    /// see as "sidebar says openai but error mentions anthropic."
    ReloadConfig,
    /// The user just saved an AGENTS.md / CLAUDE.md (folder or global
    /// scope) via the GUI's instructions editor. Rebuild the running
    /// session's system prompt in place so the next turn sees the
    /// updated instructions — no restart, no `/new` required.
    /// Lighter than [`Self::ReloadConfig`] (no provider rebuild) — only
    /// touches `state.system_prompt`.
    InstructionsChanged,
    /// The user just saved an agent def via the GUI's `/agent new` /
    /// `/agent edit` editor. Reload `state.agent_defs` from disk so a
    /// newly-created agent is immediately spawnable via `/agent <name>`
    /// in the same session. The model-driven Task tool keeps the def
    /// snapshot it captured at init (refreshes on next session).
    AgentDefsChanged,
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
    /// Plan-07 Phase 1.3: IPC successfully redeemed a pairing
    /// code via the relay's `POST /pair`, saved the binding to
    /// `~/.config/thclaws/line.json`, and is asking the worker
    /// to spawn the WebSocket session. Worker stashes the
    /// `LineSessionHandle` on `state.line_session` and
    /// broadcasts `ViewEvent::LineStatus`.
    LineConnect(crate::line::LineConfig),
    /// Plan-07 Phase 1.3: IPC `line_disconnect` request. Worker
    /// cancels the active session (if any), drops the handle,
    /// deletes the on-disk config, and broadcasts the
    /// disconnected status.
    LineDisconnect,
    /// Plan-07 Phase 2: LINE user sent a message; the WS handler
    /// pushes the text into the worker so it drives the real
    /// `Agent::run_turn`. `respond` is a oneshot the worker fills
    /// with the captured final assistant text — the LineSession
    /// then POSTs it back to the relay's `/reply/{id}` endpoint
    /// (which calls LINE's Messaging API). One LINE turn = one
    /// worker turn = one OA reply.
    LineMessage {
        text: String,
        respond: tokio::sync::oneshot::Sender<String>,
    },
    /// dev-plan/44 Tier 1: connect the phone-home tunnel from a saved
    /// `PhoneHomeConfig` (boot auto-reconnect, or the GUI "Phone home"
    /// toggle). The worker spawns the bridge session and stashes the
    /// `PhoneHomeHandle`. Unlike LINE/Messenger it does NOT swap the
    /// permission posture — approvals stay local (the cloud drives the
    /// agent, so local approval prompts are the safety gate).
    PhoneHomeConnect(crate::phone_home::PhoneHomeConfig),
    /// dev-plan/44: IPC `phone_home_disconnect`. Worker cancels the
    /// session, drops the handle, and deletes the on-disk config.
    PhoneHomeDisconnect,
    /// dev-plan/44: IPC `phone_home_pair`. Worker exchanges the stored
    /// thClaws.cloud CLI token for a binding via the relay's `/ph/pair`,
    /// saves `.thclaws/phone-home.json`, then connects. Carried out in a
    /// spawned task (it does a network round-trip) that emits a
    /// `PhoneHomeConnect` on success.
    PhoneHomePair,
    /// dev-plan/29 Tier 1: connect the Telegram bridge from a saved
    /// `TelegramConfig` (GUI Connect modal, or boot auto-reconnect). The
    /// worker validates the token via `getMe`, spawns the polling
    /// session, stashes the `TelegramSessionHandle`, swaps the approver
    /// to `TelegramApprover`, and broadcasts `ViewEvent::TelegramStatus`.
    TelegramConnect(crate::telegram::TelegramConfig),
    /// dev-plan/29: IPC `telegram_disconnect`. Worker cancels the
    /// session, restores the pre-connect mode/approver, deletes the
    /// on-disk config, and broadcasts the disconnected status.
    TelegramDisconnect,
    /// dev-plan/29: a Telegram user sent text; the polling sink pushes it
    /// here so the worker drives the real `Agent::run_turn`. `respond`
    /// is filled with the captured final assistant text — the session
    /// then chunks + sends it back via `sendMessage`.
    TelegramMessage {
        text: String,
        /// Attachments as `(media_type, base64)` — a Telegram photo
        /// (public issue #187). Empty for a plain text message, which
        /// keeps that path on `handle_line` exactly as before.
        images: Vec<(String, String)>,
        respond: tokio::sync::oneshot::Sender<String>,
    },
    /// dev-plan/29: owner approved a pairing code in the GUI. Worker
    /// appends the user id to `allow_from`, persists, and DMs the user.
    TelegramPairingApprove { code: String },
    /// dev-plan/29: owner rejected a pairing code in the GUI.
    TelegramPairingReject { code: String },
    /// dev-plan/29: GUI requested a live status snapshot (pending
    /// pairings + approvals + chat counts live in the worker's in-memory
    /// handle, not on disk). The worker answers by broadcasting a
    /// `ViewEvent::TelegramStatus`. Polled by the connect modal.
    TelegramStatusRequest,
    /// dev-plan/31: connect the Facebook Page Messenger bridge from a
    /// saved `MessengerConfig` (GUI Connect modal after `/pair`, or boot
    /// auto-reconnect). The worker spawns the relay WS session, stashes
    /// the `MessengerSessionHandle`, swaps the approver to
    /// `MessengerApprover`, and broadcasts `ViewEvent::MessengerStatus`.
    MessengerConnect(crate::messenger::MessengerConfig),
    /// dev-plan/31: IPC `messenger_disconnect`. Worker cancels the
    /// session, restores the pre-connect mode/approver, deletes the
    /// on-disk config, and broadcasts the disconnected status.
    MessengerDisconnect,
    /// dev-plan/31: a Messenger user sent text; the relay WS sink pushes
    /// it here so the worker drives the real `Agent::run_turn`. `respond`
    /// is filled with the captured final assistant text — the session
    /// then chunks + sends it back via the relay's Send API.
    MessengerMessage {
        text: String,
        respond: tokio::sync::oneshot::Sender<String>,
    },
    /// dev-plan/31: GUI requested a live status snapshot. The worker
    /// answers by broadcasting a `ViewEvent::MessengerStatus`.
    MessengerStatusRequest,
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
    /// dev-plan/32 Tier 3 GUI approval. Fired by `/workflow run`
    /// from the chat surface — the frontend renders a review bubble
    /// with the script + Approve / Cancel / Re-author buttons. Each
    /// button click posts back a `workflow_decision` IPC message
    /// carrying this `id`, which the IPC handler routes to the
    /// `WorkflowApprover` waiting on the matching oneshot.
    WorkflowReviewRequest {
        id: String,
        prompt: String,
        script: String,
        model: String,
        revision: u32,
    },
    TurnDone,
    /// Per-turn token/cost footer (GUI parity with the CLI REPL's
    /// `[tokens: …in/…out · …s · $… session]` line). Carries the plain
    /// text; the chat surface renders it as a dim footer line and the
    /// terminal surface prints it dim. Emitted just before `TurnDone`.
    TurnUsage(String),
    /// The process-wide agent_activity busy state transitioned. The
    /// event-translator turns this into a `gui_busy_changed` IPC
    /// envelope carrying the current `busy_meta()` so the workspace
    /// UI's running chip + the cloud-dashboard pill can update
    /// without polling. Fired at user-facing turn boundaries (start
    /// + end). Side-channel turns (auto-learn ingest/reconcile) do
    /// not fire this — they don't change the surface meta.
    BusyChanged,
    HistoryReplaced(Vec<DisplayMessage>),
    SessionListRefresh(String),
    /// Sidebar provider/model update — carries a pre-built JSON
    /// payload shaped like `{type: "provider_update", provider, model,
    /// provider_ready}`. Emitted by the worker when it changes the
    /// active model (e.g. auto-switch during `/load`) so the sidebar
    /// reflects the new state without waiting for the 5 s config-poll.
    ProviderUpdate(String),
    /// Settings-derived UI flags (shellTabEnabled, teamEnabled, …) may
    /// have changed. Carries a pre-built `{"type":"settings_changed"}`
    /// envelope. Emitted after a `ShellInput::ReloadConfig` completes —
    /// either from the manual `settings_reload` IPC or from the file
    /// watcher on `.thclaws/settings.json`. App.tsx subscribes and
    /// re-fetches per-flag IPCs (shell_tab_enabled_get,
    /// team_enabled_get) so tab visibility refreshes without a page
    /// reload.
    SettingsChanged(String),
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
    /// LINE bridge status (plan-07 Phase 1.3). Pre-built JSON shaped
    /// like `{type: "line_status", state: "connected"|"disconnected",
    /// server_url: "...", pending_approvals: N}`. Emitted on pair /
    /// disconnect and whenever the bridge crosses a state boundary.
    LineStatus(String),
    /// Telegram bridge status (dev-plan/29). Pre-built JSON shaped like
    /// `{type: "telegram_status", state, bot_username, pending_approvals,
    /// pending_pairings, active_chats, pairings: [{code, display, …}]}`.
    /// Emitted on connect / disconnect / pairing change and in response
    /// to `TelegramStatusRequest`.
    TelegramStatus(String),
    /// Messenger bridge status (dev-plan/31). Pre-built JSON shaped like
    /// `{type: "messenger_status", state, server_url, pending_approvals}`.
    /// Emitted on connect / disconnect and in response to
    /// `MessengerStatusRequest`.
    MessengerStatus(String),
    /// Goal-state sidebar refresh (Phase A). Carries the latest snapshot
    /// of the active /goal — `None` means the goal was cleared. Frontend
    /// renders a compact indicator (objective, iterations, tokens
    /// used/budget, status) above the plan sidebar.
    GoalUpdate(Option<crate::goal_state::GoalState>),
    /// Research jobs sidebar refresh (M6.39.3). Pre-built JSON payload
    /// shaped like `{type: "research_update", jobs: [{id, status,
    /// phase, query, iterations_done, source_count, last_score,
    /// kms_target, result_page, error}, ...]}`. Emitted after every
    /// phase change inside the pipeline driver, plus on terminal
    /// transitions, so the sidebar panel reflects live progress
    /// without polling.
    ResearchUpdate(String),
    /// Open the GUI's interactive model picker — pre-built JSON payload
    /// shaped like `{type: "model_picker_open", provider, current,
    /// models: [{id, context, max_output}, ...]}`. Emitted by the
    /// `/model` slash command when invoked without arguments (#25).
    /// The CLI renderer ignores this — a CLI TUI picker is a future
    /// follow-up.
    ModelPickerOpen(String),
    /// Open the schedule-add modal — pre-built JSON payload shaped
    /// `{type: "schedule_add_open", defaults: {cwd, timeoutSecs}}`.
    /// Emitted by the `/schedule add` slash command from a GUI surface.
    /// CLI renderer ignores this; the REPL handler prints help text
    /// instead since a multi-field form doesn't fit a terminal line.
    ScheduleAddOpen(String),
    /// Open the agent-editor modal — pre-built JSON payload shaped
    /// `{type: "agent_editor_open", mode: "new"|"edit", name, body}`
    /// where `body` is the full `.md` (YAML frontmatter + system
    /// prompt). Emitted by `/agent new` / `/agent edit` from a GUI
    /// surface; the CLI renderer ignores it (the REPL handler prints a
    /// GUI-only hint). Save round-trips via the `agent_save` IPC.
    AgentEditorOpen(String),
    /// Open the unified marketplace browser modal — pre-built JSON
    /// payload `{type:"marketplace_open", source, cacheAge, catalog:{
    /// skills,mcp_servers,plugins,subagents}, installed:{skills,subagents}}`.
    /// Emitted by `/marketplace` from a GUI surface. Install + refresh
    /// happen by injecting slash commands through the `shell_input` IPC,
    /// so there's no dedicated install channel. CLI renderer ignores it
    /// (the REPL `/marketplace` prints a combined text summary instead).
    MarketplaceOpen(String),
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
    /// Worker → event-loop signal: GUI `/reload`. The loop persists the
    /// current window size (the re-exec otherwise restores the size from
    /// the last normal close, not the live one) then re-execs.
    ReloadRequested,
    /// Active plan changed. `Some(plan)` for submit / update_step,
    /// `None` for clear. The translator forwards this as a
    /// `chat_plan_update` IPC envelope to the right-side
    /// `PlanSidebar`. Plan-mode rebuild M1.
    PlanUpdate(Option<crate::tools::plan_state::Plan>),
    /// TodoWrite snapshot — emitted every time the model writes the
    /// scratchpad checklist (and once at worker boot from disk so the
    /// sidebar starts populated). The translator forwards as a
    /// `chat_todo_update` IPC envelope to the right-side `TodoSidebar`.
    /// Empty vec means "no todos" (file missing OR explicit empty
    /// list); the sidebar collapses to a chevron in that case.
    TodoUpdate(Vec<crate::tools::todo::TodoItem>),
    /// Status note emitted by the skill-model resolver when a skill
    /// with `model:` frontmatter is invoked. Carries a single-line
    /// human-readable string the chat surface renders inline (italic,
    /// muted) so the user sees swap / fallback decisions without
    /// digging through tool logs. Three flavors in practice — the
    /// resolver formats them so the worker doesn't repeat the prose:
    ///   - "[model → claude-sonnet-4-6 (skill: namecard-to-excel)]"
    ///   - "[skill 'namecard-to-excel' recommends claude-sonnet-4-6
    ///      (vision); using current gemini-2.5-flash]"
    ///   - "[model → gemini-2.5-flash (skill ended)]"
    SkillModelNote(String),
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
    /// User-spawned side-channel agent started running. `id` is the
    /// stable handle the user can reference in `/agent cancel <id>`;
    /// `agent_name` is the AgentDef name (e.g. `translator`). Frontend
    /// renders a "🔄 1 background agent running" status indicator above
    /// the chat input area when at least one side-channel is active.
    SideChannelStart {
        id: String,
        agent_name: String,
    },
    /// Streaming text delta from a side-channel agent's response.
    /// Frontend appends to the in-progress side-channel bubble.
    SideChannelTextDelta {
        id: String,
        text: String,
    },
    /// A tool call inside a side-channel agent. Surfaced for
    /// completeness so the per-thread drill-down stream has the
    /// same fidelity as the main agent's tool indicators. Most users
    /// will only see the final result, not these.
    SideChannelToolCall {
        id: String,
        tool_name: String,
        label: String,
    },
    /// Side-channel agent finished. `duration_ms` is wall-clock from
    /// spawn to idle; `result_text` is the final assistant message
    /// (concatenated text blocks). Frontend renders a "✓ done in
    /// 5m23s" header on the side-channel bubble.
    SideChannelDone {
        id: String,
        agent_name: String,
        duration_ms: u64,
        result_text: String,
    },
    /// Side-channel agent errored or was cancelled before completion.
    /// `error` is a one-line description; the bubble flips to a red
    /// header. Cancellation (via `/agent cancel`) lands here too with
    /// `error: "cancelled"`.
    SideChannelError {
        id: String,
        error: String,
    },
}

#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: String,
    pub content: String,
    /// The model's reasoning for this turn, when it produced any. Kept
    /// out of `content` so the surface can render it the way it renders
    /// live reasoning — a collapsed block above the answer — rather
    /// than inlining it into the reply text.
    pub thinking: Option<String>,
}

impl DisplayMessage {
    /// Same as [`from_messages`], plus the session's stored per-turn
    /// usage footers dropped back in as `system` rows at the message
    /// counts they were recorded at. This is what a reload should
    /// render: the live view shows those footers, so history that
    /// silently omits them is a different conversation.
    pub fn from_session(session: &crate::session::Session) -> Vec<Self> {
        let usage = &session.turn_usage;
        if usage.is_empty() {
            return Self::from_messages(&session.messages);
        }
        let mut out: Vec<Self> = Vec::new();
        let mut next = 0usize;
        for (i, _) in session.messages.iter().enumerate() {
            // Rebuild one message at a time so a footer recorded at
            // `after = N` lands after exactly N messages' worth of rows.
            out.extend(Self::from_messages(&session.messages[i..=i]));
            while next < usage.len() && usage[next].after == i + 1 {
                out.push(DisplayMessage {
                    role: "system".into(),
                    content: usage[next].text.clone(),
                    thinking: None,
                });
                next += 1;
            }
        }
        // Footers recorded past the end (history trimmed by a compaction
        // since) still belong at the bottom rather than being dropped.
        for u in &usage[next..] {
            out.push(DisplayMessage {
                role: "system".into(),
                content: u.text.clone(),
                thinking: None,
            });
        }
        out
    }

    pub fn from_messages(messages: &[Message]) -> Vec<Self> {
        let mut out: Vec<DisplayMessage> = Vec::new();
        // Map tool_use_id → tool name so when we later see a
        // ToolResult we can ask "was the parent call AskUserQuestion?"
        // — that's the one tool whose result IS the user's reply
        // and so deserves to render as a user bubble in the chat tab.
        let mut tool_use_names: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

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
            // most ToolResults are dropped (raw tool output lives on
            // the Terminal tab) — except AskUserQuestion's, which IS
            // the user's typed reply and renders as a user bubble.
            let mut text_parts: Vec<String> = Vec::new();
            let mut thinking_parts: Vec<String> = Vec::new();
            let mut deferred_tools: Vec<DisplayMessage> = Vec::new();
            let mut deferred_user_replies: Vec<DisplayMessage> = Vec::new();
            for b in &m.content {
                match b {
                    ContentBlock::Text { text } => text_parts.push(text.clone()),
                    // Reasoning is model-internal scratch — don't show
                    // it in the chat-list display. When the GUI gets a
                    // dedicated "show thinking" toggle, surface this
                    // there instead of the main bubble.
                    ContentBlock::Thinking { content, .. } => thinking_parts.push(content.clone()),
                    ContentBlock::ToolUse { id, name, .. } => {
                        tool_use_names.insert(id.clone(), name.clone());
                        deferred_tools.push(DisplayMessage {
                            role: "tool".into(),
                            content: name.clone(),
                            thinking: None,
                        });
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        // AskUserQuestion's result IS what the user
                        // typed — surface it so chat history shows
                        // both the question and the answer rather
                        // than a question with an invisible reply.
                        // Other tools' raw output stays on Terminal.
                        if tool_use_names
                            .get(tool_use_id)
                            .map(|n| n == "AskUserQuestion")
                            .unwrap_or(false)
                        {
                            let reply = content.to_text();
                            let trimmed = reply.trim();
                            if !trimmed.is_empty() {
                                deferred_user_replies.push(DisplayMessage {
                                    role: "user".into(),
                                    content: trimmed.to_string(),
                                    thinking: None,
                                });
                            }
                        }
                    }
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
            // AskUserQuestion replies render LAST within their parent
            // user message so the prior assistant question reads
            // before the answer in the chat list.
            let text = text_parts.join("\n");
            let thinking = if thinking_parts.is_empty() {
                None
            } else {
                Some(thinking_parts.join("\n"))
            };
            if !text.is_empty() || thinking.is_some() {
                out.push(DisplayMessage {
                    role: role.to_string(),
                    content: text,
                    thinking,
                });
            }
            out.extend(deferred_tools);
            out.extend(deferred_user_replies);
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
    /// Mid-turn user input queue (issue #106). IPC pushes messages
    /// here while the agent is busy; the agent drains them at the
    /// next tool_result boundary. The same Arc is wired into the
    /// agent via `Agent::use_injection_queue` on every agent
    /// construction so a queue submission survives a session reload
    /// or cwd change.
    pub injection_queue: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    /// dev-plan/32 Tier 3 workflow approver, shared between the
    /// shared-session worker (`WorkerState.workflow_approver`) and
    /// the IPC dispatch (`IpcContext.workflow_approver`) so both
    /// reach the same pending-request map.
    pub workflow_approver: std::sync::Arc<crate::workflow::WorkflowApprover>,
    /// dev-plan/35 Tier 1 multi-tenant override: when `Some`,
    /// session JSONLs, gui-shell storage, and usage metering for
    /// this handle write to per-user roots instead of the cwd /
    /// HOME-relative defaults. `None` for single-tenant `--serve`,
    /// desktop GUI, and CLI — those use the legacy paths unchanged.
    pub session_roots: Option<crate::multi_tenant::SessionRoots>,
    /// docs/browser Phase 1: the engine-managed Playwright MCP client
    /// (server name `browser`), once connected. Lets the IPC layer
    /// drive UI-initiated, read-only calls (the Browser tab's
    /// screenshot capture) directly on the client without routing
    /// through the agent loop or the worker's input queue (which only
    /// drains between turns). `None` until `McpReady` for `browser`
    /// fires, and always `None` when `browserEnabled` is off.
    pub browser_mcp:
        std::sync::Arc<std::sync::RwLock<Option<std::sync::Arc<crate::mcp::McpClient>>>>,
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
    /// dev-plan/35 Tier 1: per-user roots when this worker belongs to
    /// a multi-tenant pod, `None` for single-tenant. Forwarded from
    /// [`spawn_with_roots`] and consulted at every site that would
    /// otherwise pick up `SessionStore::default_path()` /
    /// `UsageTracker::default_path()` — keeps the override sticky
    /// across `/new`, `/reload`, model swaps, and session forks.
    pub session_roots: Option<crate::multi_tenant::SessionRoots>,
    pub tool_registry: ToolRegistry,
    pub system_prompt: String,
    pub cwd: PathBuf,
    /// Approval sink attached to `agent`. Kept here so
    /// [`Self::rebuild_agent`] can re-wire it onto the fresh Agent — a
    /// `/model` or `/provider` swap must preserve the user's approval
    /// UI (GUI modal vs REPL prompt) without silently falling back to
    /// AutoApprover.
    pub approver: std::sync::Arc<dyn crate::permissions::ApprovalSink>,
    /// dev-plan/32 Tier 3: chat-surface `/workflow run` posts a
    /// review request and awaits the user's button click here. The
    /// IPC handler resolves pending requests via `resolve()` when the
    /// frontend posts back a `workflow_decision` message.
    pub workflow_approver: std::sync::Arc<crate::workflow::WorkflowApprover>,
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
    /// AgentFactory used to spawn subagents (`Task` tool) and side-
    /// channel agents (`/agent` slash command). Built once at worker
    /// init and cloned per spawn — reusing the factory means side
    /// channels inherit the same provider, base tools, system prompt,
    /// and approver as the main agent.
    pub agent_factory: std::sync::Arc<dyn crate::subagent::AgentFactory>,
    /// Live snapshot of `system_prompt` + `tool_registry` shared with
    /// the `agent_factory` above so subagents spawned via Task pick
    /// up `/mcp add` / `/skill install` / `/kms use` and folder-
    /// instructions / memory edits without a `/reload`. Pre-fix the
    /// factory captured these at worker init and never refreshed —
    /// `rebuild_system_prompt` updated `self.system_prompt` and the
    /// live `self.agent`, but the factory kept seeing the startup
    /// snapshot. See [`crate::subagent::FactorySnapshot`] docs.
    pub factory_snapshot: std::sync::Arc<std::sync::RwLock<crate::subagent::FactorySnapshot>>,
    /// Loaded agent definitions (`.thclaws/agents/*.md` + plugin agent
    /// dirs). Side-channel `/agent` validates names against this list
    /// before spawning; the factory uses it to register a Task tool
    /// for the spawned child.
    pub agent_defs: crate::agent_defs::AgentDefsConfig,
    /// Plan-07 Phase 1.3: active LINE-bridge session, if the user has
    /// paired their thClaws install to a LINE OA. `Some` only while
    /// the background WS task is running; `line_disconnect` cancels
    /// + clears it.
    pub line_session: Option<crate::line::LineSessionHandle>,
    /// Plan-07 Phase 2.1: pre-LINE-connect snapshot of the agent's
    /// permission mode + approver, so `LineDisconnect` can restore
    /// exactly where the user left off. `Some` only while a LINE
    /// session is active.
    pub line_pre_mode: Option<crate::permissions::PermissionMode>,
    pub line_pre_approver: Option<std::sync::Arc<dyn crate::permissions::ApprovalSink>>,
    /// dev-plan/29 Tier 1: active Telegram-bridge session. `Some` only
    /// while the polling task is running; `telegram_disconnect` cancels
    /// + clears it. Mirrors `line_session`. Running LINE and Telegram
    /// simultaneously isn't a Tier 1 goal — last-connect wins the
    /// approver routing.
    pub telegram_session: Option<crate::telegram::TelegramSessionHandle>,
    /// Pre-Telegram-connect snapshot of the agent's permission mode +
    /// approver, restored on disconnect. Mirrors `line_pre_*`.
    pub telegram_pre_mode: Option<crate::permissions::PermissionMode>,
    pub telegram_pre_approver: Option<std::sync::Arc<dyn crate::permissions::ApprovalSink>>,
    /// dev-plan/44 Tier 1: active phone-home tunnel session. `Some` only
    /// while the bridge WS task is running; `phone_home_disconnect`
    /// cancels + clears it. No pre-mode/approver stash — phone-home keeps
    /// approvals local (see `ShellInput::PhoneHomeConnect`).
    pub phone_home_session: Option<crate::phone_home::PhoneHomeHandle>,
    /// dev-plan/31: active Facebook Page Messenger bridge session. `Some`
    /// only while the relay WS task is running; `messenger_disconnect`
    /// cancels + clears it. Mirrors `line_session`.
    pub messenger_session: Option<crate::messenger::MessengerSessionHandle>,
    /// Pre-Messenger-connect snapshot of the agent's permission mode +
    /// approver, restored on disconnect. Mirrors `line_pre_*`.
    pub messenger_pre_mode: Option<crate::permissions::PermissionMode>,
    pub messenger_pre_approver: Option<std::sync::Arc<dyn crate::permissions::ApprovalSink>>,
    /// Externally-held mid-turn injection queue (issue #106). Kept on
    /// the state so `rebuild_agent` can re-wire it onto the new agent
    /// — without this, a `/model` swap or other rebuild would orphan
    /// the queue and any pending message would be lost.
    pub injection_queue: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    /// Running USD cost accumulator. Updated after each AgentEvent::Done
    /// via `EffectiveCatalogue::compute_cost_usd`; surfaced through the
    /// `/cost` slash command and pushed to the Cardputer display via
    /// `cost_bridge`. Zeroed by `/cost reset` or by a buddy-side reset.
    pub session_cost_usd: f64,
    /// SHA-fingerprint of the last `.thclaws/settings.json` bytes that
    /// drove a successful `ReloadConfig`. Used to dedup back-to-back
    /// reloads from the slash command that wrote the file plus the
    /// debounced file-watcher that picked the same write up 500ms
    /// later — both fire `ReloadConfig`, both did the work, the user
    /// saw "(provider reloaded: …)" twice. `None` until the first
    /// successful reload.
    pub last_settings_fingerprint: Option<u64>,
    /// Optional BLE bridge to a thClaws-Cost Cardputer. `Some` whenever
    /// the worker spawned a bridge at startup (default for both CLI and
    /// GUI modes when the `cost_bridge` feature is on); `None` when the
    /// feature is compiled out so the field is harmless to reference.
    #[cfg(feature = "cost_bridge")]
    pub cost_bridge: Option<crate::cost_bridge::CostBridge>,
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
        // Settings-gated tools must track the CURRENT config. The
        // agent-install path flips `imageToolsEnabled` mid-session and
        // the settings watcher lands here — pre-fix the registry was a
        // boot-time snapshot, so TextToImage answered "unknown tool"
        // until a full engine restart.
        if self.config.image_tools_enabled {
            self.tool_registry
                .register(std::sync::Arc::new(crate::tools::TextToImageTool));
            self.tool_registry
                .register(std::sync::Arc::new(crate::tools::ImageToImageTool));
            self.tool_registry
                .register(std::sync::Arc::new(crate::tools::TextToSpeechTool));
            self.tool_registry
                .register(std::sync::Arc::new(crate::tools::RenderSlidesTool));
            self.tool_registry
                .register(std::sync::Arc::new(crate::tools::TextToVideoTool));
            self.tool_registry
                .register(std::sync::Arc::new(crate::tools::ImageToVideoTool));
            self.tool_registry
                .register(std::sync::Arc::new(crate::tools::MediaJobStatusTool));
        } else {
            self.tool_registry.remove("TextToImage");
            self.tool_registry.remove("ImageToImage");
            self.tool_registry.remove("TextToVideo");
            self.tool_registry.remove("ImageToVideo");
            self.tool_registry.remove("MediaJobStatus");
        }

        if self.config.hal_enabled {
            self.tool_registry.register(std::sync::Arc::new(
                crate::tools::YouTubeTranscriptTool::new(),
            ));
            self.tool_registry
                .register(std::sync::Arc::new(crate::tools::WebScrapeTool::new()));
        } else {
            self.tool_registry.remove("YouTubeTranscript");
            self.tool_registry.remove("WebScrape");
        }
        let prev_perm = self.agent.permission_mode;
        let prev_thinking = self.agent.thinking_budget;
        let prev_ask_tools = self.agent.ask_tools.clone();
        let new_agent = Agent::new(
            provider,
            self.tool_registry.clone(),
            &self.config.model,
            &self.system_prompt,
        )
        .with_max_tokens(self.config.max_tokens)
        .with_approver(self.approver.clone())
        .with_cancel(self.cancel.clone())
        // M6.35 HOOK1: re-snapshot config.hooks on rebuild — config
        // edits via Settings → save → ReloadConfig take effect on the
        // next agent. Pre-fix the snapshot was only at first boot.
        .with_hooks(std::sync::Arc::new(self.config.hooks.clone()));
        self.agent = new_agent;
        self.agent.permission_mode = prev_perm;
        self.agent.thinking_budget = prev_thinking;
        self.agent.ask_tools = prev_ask_tools;
        // Re-wire the externally-held injection queue (#106) so
        // anything queued during the rebuild doesn't get orphaned on
        // the old agent's Vec.
        self.agent.use_injection_queue(self.injection_queue.clone());
        if let Some(h) = prev_history {
            self.agent.set_history(h);
        }
        // L5: `rebuild_agent` mutates `tool_registry` directly (the
        // image/video tool toggle above). Keep the subagent factory's
        // snapshot in step so a Task spawned after a settings-driven
        // rebuild (e.g. `imageToolsEnabled` flip via the settings
        // watcher) sees the same tool set the parent does. Callers that
        // also rebuild the prompt sync again — `sync_factory_snapshot`
        // is idempotent.
        self.sync_factory_snapshot();
        Ok(())
    }

    /// Recompute the system prompt from the current `config` (picks up
    /// updated `kms_active`, `team_enabled`, memory, skills, etc.) AND
    /// push it into the live Agent so the next provider.stream call
    /// sees it. Pre-fix this only updated `self.system_prompt`; the
    /// Agent's captured `system` was stale until a full rebuild
    /// (`/reload` or a model swap). Saving folder instructions from
    /// the Settings menu emitted "system prompt rebuilt" but the new
    /// content didn't actually reach the model until a restart.
    pub fn rebuild_system_prompt(&mut self) {
        let mcp_instructions = crate::mcp::collect_mcp_instructions(&self.mcp_clients);
        self.system_prompt = build_system_prompt(
            &self.config,
            &self.cwd,
            &self.skill_store,
            &mcp_instructions,
        );
        self.agent.set_system(self.system_prompt.clone());
        // Propagate to the subagent factory's live snapshot so a
        // subagent spawned after this sees the same system the parent
        // agent does. Pre-fix the factory captured system + base_tools
        // at worker init and never refreshed.
        self.sync_factory_snapshot();
    }

    /// Push the worker's current `system_prompt` + `tool_registry`
    /// into the factory's shared snapshot. Call after any path that
    /// mutates `tool_registry` (`/mcp add` register, `/mcp` disconnect
    /// remove, KMS tool shape-shift) so subagents see the live set
    /// of tools — Production AgentFactory's `base_tools` field used
    /// to be a one-shot snapshot at worker init.
    ///
    /// Cheap: ToolRegistry clone is just cloning a HashMap of Arc'd
    /// tools (refcount bumps, no tool work). The RwLock is held for
    /// a few field writes and nothing else.
    pub fn sync_factory_snapshot(&self) {
        // L2: recover from a poisoned lock rather than panicking.
        let mut snap = self
            .factory_snapshot
            .write()
            .unwrap_or_else(|e| e.into_inner());
        snap.system = self.system_prompt.clone();
        snap.tools = self.tool_registry.clone();
        // Model + provider too: `rebuild_agent` runs on ReloadConfig
        // (settings watcher / `/model`), so after a mid-session model
        // switch unpinned subagents must inherit the NEW model. Pre-fix
        // the factory kept the worker-init pair for the whole session —
        // grok main agent kept spawning deepseek workers.
        snap.model = self.config.model.clone();
        snap.provider = self.agent.provider().clone();
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
    mcp_instructions: &[(String, String)],
) -> String {
    // dev-plan/35 followup: delegate to the always-on unified builder
    // in `prompts::build_full_system_prompt` so GUI + REPL + print +
    // agent_runtime emit byte-identical text modulo the surface-tagged
    // priming. Pre-fix, four inline assemblies had drifted apart.
    let guard = skill_store.lock().ok();
    crate::prompts::build_full_system_prompt(
        config,
        cwd,
        guard.as_deref(),
        mcp_instructions,
        crate::prompts::SurfaceHints::Gui,
    )
}

/// True when two paths refer to the same on-disk directory. Prefers
/// `canonicalize` so symlinks / `..` segments / trailing slashes
/// don't cause spurious "different" verdicts. Falls back to literal
/// equality only when canonicalization fails (e.g. path doesn't
/// exist) — in which case the strict comparison is the safer guess.
///
/// Used by the `ChangeCwd` worker arm to short-circuit the no-op
/// path the StartupModal "Start" button takes on every launch (and
/// to keep that "Start" button cheap for any user who confirms an
/// unchanged cwd).
fn paths_equivalent(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
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
    spawn_with_roots(approver, None)
}

/// dev-plan/35 Tier 1 multi-tenant entry point. Same as
/// [`spawn_with_approver`] but threads a [`SessionRoots`] override
/// into the worker so session JSONLs, gui-shell storage, and usage
/// metering land under per-user prefixes instead of the cwd /
/// HOME-relative defaults. `None` → identical behaviour to
/// `spawn_with_approver` (single-tenant defaults).
pub fn spawn_with_roots(
    approver: std::sync::Arc<dyn crate::permissions::ApprovalSink>,
    session_roots: Option<crate::multi_tenant::SessionRoots>,
) -> SharedSessionHandle {
    let (input_tx, input_rx) = mpsc::channel::<ShellInput>();

    // File watcher on .thclaws/settings.json — any edit (Files tab
    // save, external editor, programmatic write) triggers an
    // automatic ReloadConfig. Closes the "I enabled
    // shellTabEnabled but the tab didn't appear" gap without a
    // manual restart. The `settings_reload` IPC arm is the explicit
    // fallback for users who want to force a reload.
    spawn_settings_watcher(input_tx.clone());

    // Capacity sized for fast multi-subscriber streaming. Team mode adds
    // collectors (LINE/Telegram bridges, web WS) alongside the GUI, and a
    // burst of small AssistantTextDelta tokens can outrun a slow consumer.
    // At 256 the laggy subscriber silently dropped text deltas (issue #163
    // Bug 1: thinking rendered, response text vanished); 2048 absorbs the
    // bursts. Lagged events are now also logged in the forwarders.
    let (events_tx, _) = broadcast::channel::<ViewEvent>(2048);
    let cancel = crate::cancel::CancelToken::new();
    let ready_gate = Arc::new(ReadyGate::new());
    // Mid-turn injection queue (issue #106) — shared between the IPC
    // layer (push) and the agent inside the worker (drain).
    let injection_queue: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    let workflow_approver = crate::workflow::WorkflowApprover::new();
    // docs/browser Phase 1: slot the worker fills when the engine-
    // managed `browser` MCP server connects, read by the IPC layer
    // for UI-initiated screenshot captures.
    let browser_mcp: std::sync::Arc<
        std::sync::RwLock<Option<std::sync::Arc<crate::mcp::McpClient>>>,
    > = std::sync::Arc::new(std::sync::RwLock::new(None));

    let events_tx_for_thread = events_tx.clone();
    let cancel_for_thread = cancel.clone();
    let input_tx_for_poller = input_tx.clone();
    let gate_for_thread = ready_gate.clone();
    let injection_queue_for_worker = injection_queue.clone();
    let workflow_approver_for_worker = workflow_approver.clone();
    let session_roots_for_worker = session_roots.clone();
    let browser_mcp_for_worker = browser_mcp.clone();
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
                injection_queue_for_worker,
                workflow_approver_for_worker,
                session_roots_for_worker,
                browser_mcp_for_worker,
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
        injection_queue,
        workflow_approver,
        session_roots,
        browser_mcp,
    }
}

/// Spawn a debounced filesystem watcher on `.thclaws/settings.json`.
/// Any modify event fires a `ShellInput::ReloadConfig` so the engine
/// re-reads project settings without a process restart. Paired with
/// the manual `settings_reload` IPC arm and the SettingsChanged
/// broadcast — the user can edit settings.json in any editor and
/// see the change take effect (tab visibility, model, …) within ~1 s.
///
/// The watcher leaks for the process lifetime — there's exactly one
/// worker per engine, and it should watch as long as the engine runs.
/// Re-firing ReloadConfig when a write was triggered by our own code
/// (e.g. sidebar model picker → `ProjectConfig::set_model`) is
/// harmless: the handler is idempotent.
fn spawn_settings_watcher(input_tx: mpsc::Sender<ShellInput>) {
    use notify_debouncer_mini::new_debouncer;
    use notify_debouncer_mini::notify::RecursiveMode;

    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[33m[settings-watch] cannot read cwd: {e}\x1b[0m");
            return;
        }
    };
    let thclaws_dir = cwd.join(".thclaws");
    // settings.json may not exist on first run; the parent dir does
    // because the engine's initContainer (or local startup) creates
    // it. Belt-and-braces ensure-exists so notify has a directory to
    // attach to.
    if let Err(e) = std::fs::create_dir_all(&thclaws_dir) {
        eprintln!(
            "\x1b[33m[settings-watch] mkdir {} failed: {e} (skipping watch)\x1b[0m",
            thclaws_dir.display()
        );
        return;
    }
    let settings_path = thclaws_dir.join("settings.json");

    let mut debouncer = match new_debouncer(
        std::time::Duration::from_millis(500),
        move |result: notify_debouncer_mini::DebounceEventResult| match result {
            Ok(events) => {
                for ev in events {
                    if ev.path == settings_path {
                        eprintln!(
                            "\x1b[36m[settings-watch] {} changed → ReloadConfig\x1b[0m",
                            settings_path.display()
                        );
                        let _ = input_tx.send(ShellInput::ReloadConfig);
                        // One dispatch per debounced batch; the
                        // handler is idempotent so additional events
                        // in the same batch would just no-op.
                        break;
                    }
                }
            }
            Err(e) => {
                eprintln!("\x1b[31m[settings-watch] notify error: {e}\x1b[0m");
            }
        },
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("\x1b[31m[settings-watch] could not start watcher: {e}\x1b[0m");
            return;
        }
    };
    if let Err(e) = debouncer
        .watcher()
        .watch(&thclaws_dir, RecursiveMode::NonRecursive)
    {
        eprintln!(
            "\x1b[31m[settings-watch] watch({}) failed: {e}\x1b[0m",
            thclaws_dir.display()
        );
        return;
    }
    eprintln!(
        "\x1b[36m[settings-watch] watching {}/settings.json (500ms debounce)\x1b[0m",
        thclaws_dir.display()
    );
    // Leak: the debouncer must outlive this function for the watcher
    // thread to keep firing. The engine process owns one of these for
    // its full lifetime so leaking is the right shape.
    Box::leak(Box::new(debouncer));
}

/// Build the live `telegram_status` JSON for the GUI from an active
/// bridge handle. Counts are read from the in-memory approver / pairing
/// manager / chat registry (dev-plan/29).
fn telegram_status_payload(handle: &crate::telegram::TelegramSessionHandle) -> serde_json::Value {
    serde_json::json!({
        "type": "telegram_status",
        "state": handle.status.state,
        "bot_username": handle.status.bot_username,
        "pending_approvals": handle.approver.pending_count(),
        "pending_pairings": handle.pairing.pending_list().len(),
        "active_chats": handle.registry.active_count(),
        "pairings": handle.pairing.pending_list(),
    })
}

fn telegram_disconnected_payload() -> serde_json::Value {
    serde_json::json!({
        "type": "telegram_status",
        "state": "disconnected",
        "bot_username": serde_json::Value::Null,
        "pending_approvals": 0,
        "pending_pairings": 0,
        "active_chats": 0,
        "pairings": [],
    })
}

fn messenger_status_payload(
    handle: &crate::messenger::MessengerSessionHandle,
) -> serde_json::Value {
    serde_json::json!({
        "type": "messenger_status",
        "state": handle.status.state,
        "server_url": handle.status.server_url,
        "pending_approvals": handle.approver.pending_count(),
    })
}

fn messenger_disconnected_payload() -> serde_json::Value {
    serde_json::json!({
        "type": "messenger_status",
        "state": "disconnected",
        "server_url": "",
        "pending_approvals": 0,
    })
}

async fn run_worker(
    input_rx: mpsc::Receiver<ShellInput>,
    input_tx_self: mpsc::Sender<ShellInput>,
    events_tx: broadcast::Sender<ViewEvent>,
    cancel: crate::cancel::CancelToken,
    approver: std::sync::Arc<dyn crate::permissions::ApprovalSink>,
    ready_gate: Arc<ReadyGate>,
    injection_queue: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    workflow_approver: std::sync::Arc<crate::workflow::WorkflowApprover>,
    session_roots: Option<crate::multi_tenant::SessionRoots>,
    browser_mcp: std::sync::Arc<std::sync::RwLock<Option<std::sync::Arc<crate::mcp::McpClient>>>>,
) {
    // dev-plan/42: when this worker belongs to a per-user workspace
    // (multiuser `--serve`), its working directory is that user's
    // `workspace-<id>/`, not the process cwd. Falls back to process cwd
    // for single-tenant `--serve`, desktop, and CLI (`workspace_root`
    // is `None`).
    let cwd = session_roots
        .as_ref()
        .and_then(|r| r.workspace_root.clone())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let config = AppConfig::load().unwrap_or_default();
    // Push the configured stream-chunk timeout into the global the
    // providers read on every `byte_stream.next()`. Live; subsequent
    // `/reload` paths re-apply via the same setter (see lines ~1877,
    // ~1965 where AppConfig::load is re-invoked).
    // Re-applied by the settings watcher below, so a Settings-menu toggle
    // lands without a restart.
    config.apply_process_globals();

    // Shared SkillTool store — we keep a handle in WorkerState so
    // `/skill install` can repopulate it without restarting.
    let skill_store =
        std::sync::Arc::new(std::sync::Mutex::new(crate::skills::SkillStore::discover()));

    let mut tools = ToolRegistry::with_builtins();

    // issue #182: re-register a cancel-aware Bash over the default so Stop /
    // Esc / Cmd+. kills an in-flight command (build, test, long sleep) instead
    // of letting it run to `timeout_ms`. `register()` overwrites by tool name,
    // and the token is the same session cancel the agent's main loop races.
    tools.register(std::sync::Arc::new(crate::tools::BashTool::with_cancel(
        cancel.clone(),
    )));

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

    // Todo-state → ViewEvent bridge. Mirrors the plan broadcaster
    // pattern but simpler: TodoWrite has no sequential gate, no
    // JSONL persistence (the markdown file IS the persistence
    // surface), so the closure just forwards the snapshot. Hydrate
    // from disk once at boot so the sidebar starts populated when
    // the user reopens a project that already has a todo list.
    {
        let todo_tx = events_tx.clone();
        crate::tools::todo_state::set_broadcaster(move |todos| {
            let _ = todo_tx.send(ViewEvent::TodoUpdate(todos));
        });
        let initial = crate::tools::todo::read_todos_from_disk(&cwd);
        let _ = events_tx.send(ViewEvent::TodoUpdate(initial));
    }

    // (Skill-model resolver registered below, after the agent has
    // been constructed — it needs the agent's `model_override`
    // handle.)

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

    // M6.39.3: research-jobs sidebar broadcaster. Pipeline driver
    // calls update_phase / record_iteration / finalize / cancel which
    // each fire this once with a fresh snapshot of all jobs. Frontend
    // gets a `research_update` IPC envelope with the JSON shape from
    // `gui::build_research_update_payload`.
    //
    // M6.39.5: same closure also fires `kms_update` when any job
    // transitions to Done — the pipeline may have just created or
    // refreshed a KMS, and the sidebar's Knowledge panel should
    // reflect it without a manual refresh. We track already-announced
    // Done ids so we don't republish on every subsequent broadcast
    // (each phase change fires the closure with the same Done id
    // present).
    {
        let research_tx = events_tx.clone();
        let known_done_ids: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        crate::research::manager().set_broadcaster(move |jobs| {
            let payload = crate::gui::build_research_update_payload();
            let _ = research_tx.send(ViewEvent::ResearchUpdate(payload.to_string()));

            // Detect any new Done transitions since last broadcast.
            // Fire kms_update once per detected transition so the
            // KMS sidebar picks up newly-created research KMSs.
            let mut new_done = false;
            if let Ok(mut known) = known_done_ids.lock() {
                for j in jobs {
                    if j.status == crate::research::JobStatus::Done && !known.contains(&j.id) {
                        known.insert(j.id.clone());
                        new_done = true;
                    }
                }
            }
            if new_done {
                let kms_payload = crate::gui::build_kms_update_payload();
                let _ = research_tx.send(ViewEvent::KmsUpdate(kms_payload.to_string()));
            }
        });
    }

    // KMS tools are always-on, not gated by `kms_active`. Pre-fix the
    // gate skipped registration when `kms_active` was empty — but the
    // /dream side-channel agent inherits this tool registry as
    // `base_tools`, and dream needs `KmsCreate`/`KmsWrite` to bootstrap
    // its `dreams` audit KMS *regardless* of whether the user has
    // run `/kms use ...` to mark anything active. Without these tools
    // available, /dream silently exits in 30-60s with no real work
    // done. The minor cost (a few extra tool defs in the system
    // prompt when no KMS is configured) is far smaller than /dream
    // appearing to succeed while doing nothing.
    tools.register(std::sync::Arc::new(crate::tools::KmsReadTool));
    tools.register(std::sync::Arc::new(crate::tools::KmsSearchTool));
    // M6.25 BUG #1: KmsWrite + KmsAppend make the LLM an active
    // wiki maintainer (not just a passive reader).
    tools.register(std::sync::Arc::new(crate::tools::KmsWriteTool));
    tools.register(std::sync::Arc::new(crate::tools::KmsWriteSourceTool));
    tools.register(std::sync::Arc::new(crate::tools::KmsAppendTool));
    tools.register(std::sync::Arc::new(crate::tools::KmsDeleteTool));
    // KmsCreate bootstraps the dedicated `dreams` KMS used by
    // /dream's Pass 4 audit page — defense-in-depth so a stale
    // build or filesystem race can't trap the dream agent in a
    // retry loop on "no KMS named 'dreams'".
    tools.register(std::sync::Arc::new(crate::tools::KmsCreateTool));

    // M6.26 BUG #1: Memory tools always-on. The model needs them even
    // when no entries exist yet (so it can create the first one). Sandbox
    // carve-out validated by `memory::writable_entry_path`.
    tools.register(std::sync::Arc::new(crate::tools::MemoryReadTool));
    tools.register(std::sync::Arc::new(crate::tools::MemoryWriteTool));
    tools.register(std::sync::Arc::new(crate::tools::MemoryAppendTool));
    // M6.46: SessionRename — for dream + power-user manual rename.
    tools.register(std::sync::Arc::new(crate::tools::SessionRenameTool));

    // Opt-in native Gemini image-gen tools (TextToImage,
    // ImageToImage). Gated on `imageToolsEnabled: true` in
    // settings.json AND a GEMINI_API_KEY (or GOOGLE_API_KEY) in
    // env — the tool's own `requires_env()` hides them if the key
    // is missing, but we don't even register them without the
    // settings opt-in so the model never sees them in the catalogue
    // unless the user actively asked for the surface.
    if config.image_tools_enabled {
        tools.register(std::sync::Arc::new(crate::tools::TextToImageTool));
        tools.register(std::sync::Arc::new(crate::tools::ImageToImageTool));
        tools.register(std::sync::Arc::new(crate::tools::TextToSpeechTool));
        tools.register(std::sync::Arc::new(crate::tools::RenderSlidesTool));
        tools.register(std::sync::Arc::new(crate::tools::TextToVideoTool));
        tools.register(std::sync::Arc::new(crate::tools::ImageToVideoTool));
        tools.register(std::sync::Arc::new(crate::tools::MediaJobStatusTool));
    }

    if config.hal_enabled {
        tools.register(std::sync::Arc::new(
            crate::tools::YouTubeTranscriptTool::new(),
        ));
        tools.register(std::sync::Arc::new(crate::tools::WebScrapeTool::new()));
    }

    // Tool-parity audit fix: respect `searchEngine` config override
    // (REPL had this; GUI/serve fell back to "auto" silently).
    // `HashMap::insert` in `ToolRegistry::register` overwrites the
    // default `WebSearchTool` from `with_builtins` by name.
    if config.search_engine != "auto" {
        tools.register(std::sync::Arc::new(crate::tools::WebSearchTool::new(
            &config.search_engine,
        )));
    }

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
    // M6.34 TEAM3: capture team_dir so the GUI's lead-process exit can
    // scope the kill to its own teammates only. The GUI DOES call
    // kill_my_teammates() at shutdown (gui.rs) — teammates run in
    // tmux-server-owned panes, not GUI child processes, so they are NOT
    // reclaimed when the GUI quits; the explicit scoped kill is required.
    if team_enabled && !is_teammate {
        let td = std::env::var("THCLAWS_TEAM_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| crate::team::Mailbox::default_dir());
        crate::team::set_lead_team_dir(&td);
    }
    let skill_tool = crate::skills::SkillTool::new_from_handle(skill_store.clone());
    // Capture the isolated-execution slot before the tool moves into the
    // registry; wired to the factory once it's built (below) so `isolated:
    // true` job skills run in a sub-agent.
    let skill_factory_slot = skill_tool.factory_slot();
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
    // Tool-parity audit fix: merge plugin-contributed MCPs with
    // config.mcp_servers (config wins on name clash). REPL + HTTP
    // both did this; GUI/serve silently dropped plugin MCPs —
    // installing a plugin that ships an MCP server only worked in
    // CLI mode, the GUI never saw it.
    let mut merged_mcp = config.mcp_servers.clone();
    for p_mcp in crate::plugins::plugin_mcp_servers() {
        if !merged_mcp.iter().any(|s| s.name == p_mcp.name) {
            merged_mcp.push(p_mcp);
        }
    }
    for server_cfg in merged_mcp {
        let approver_for_spawn = approver.clone();
        let input_tx_for_spawn = input_tx_self.clone();
        tokio::spawn(async move {
            let mut server_cfg = server_cfg;
            // docs/browser slice 3: for the engine-managed browser,
            // launch Chromium OURSELVES (DevTools port) and attach
            // playwright-mcp to it via --cdp-endpoint — the engine's
            // own CDP session then drives the Browser tab's live view.
            // Any failure falls back to MCP self-launch (current
            // behavior); THCLAWS_BROWSER_CDP=0 is the kill switch.
            if server_cfg.name == "browser"
                && server_cfg.engine_managed
                && std::env::var("THCLAWS_BROWSER_CDP").ok().as_deref() != Some("0")
                && !server_cfg
                    .args
                    .iter()
                    .any(|a| a.starts_with("--cdp-endpoint"))
            {
                let headless = server_cfg.args.iter().any(|a| a == "--headless");
                // arm() reserves the endpoint without launching
                // Chromium (lazy — first browser use launches it), but
                // it may probe a saved endpoint for ~1s, so keep it
                // off the async worker.
                let endpoint =
                    tokio::task::spawn_blocking(move || crate::browser_cdp::arm(headless))
                        .await
                        .ok()
                        .flatten();
                if let Some(endpoint) = endpoint {
                    server_cfg.args.push("--cdp-endpoint".into());
                    server_cfg.args.push(endpoint);
                }
            }
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

    // Initial assembly — MCP servers are still spawning in tokio tasks
    // above, so no instructions are available yet. Each McpReady arm
    // calls `rebuild_system_prompt` to fold the new server's
    // instructions in once initialize() returns.
    let system = build_system_prompt(&config, &cwd, &skill_store, &[]);

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
    let perm_mode = if config.permissions == "ask" {
        // Explicit Ask is honored even when hosted — it works via the serve
        // approval modal (delivered over the WS + re-emitted on reconnect).
        crate::permissions::PermissionMode::Ask
    } else if config.permissions == "auto" {
        crate::permissions::PermissionMode::Auto
    } else if std::env::var("THCLAWS_HOSTED").is_ok() {
        // Hosted safety net: an unexpected non-auto/non-ask value (e.g. a
        // settings.json parse fallback) must not leave a managed cloud
        // sandbox stuck in Ask with no approval UI — that's the "Write hangs,
        // no file" failure. Default hosted to Auto.
        crate::permissions::PermissionMode::Auto
    } else {
        crate::permissions::PermissionMode::Ask
    };
    let plugin_agent_dirs = crate::plugins::plugin_agent_dirs();
    let mut agent_defs_state =
        crate::agent_defs::AgentDefsConfig::load_with_extra(&plugin_agent_dirs);
    agent_defs_state.apply_builtin_subagent_overrides(&config);
    let agent_defs_state = agent_defs_state;
    let factory_snapshot_state =
        Arc::new(std::sync::RwLock::new(crate::subagent::FactorySnapshot {
            system: system.clone(),
            tools: tools.clone(),
            model: config.model.clone(),
            provider: provider.clone(),
        }));
    let factory_state: Arc<dyn crate::subagent::AgentFactory> = {
        let factory = Arc::new(crate::subagent::ProductionAgentFactory {
            snapshot: factory_snapshot_state.clone(),
            max_iterations: config.max_iterations,
            max_depth: crate::subagent::DEFAULT_MAX_DEPTH,
            max_tokens: config.max_tokens,
            agent_defs: agent_defs_state.clone(),
            approver: approver.clone(),
            permission_mode: perm_mode,
            cancel: Some(cancel.clone()),
            // M6.35 HOOK1: subagents inherit GUI worker's hooks so audit
            // hooks see Task-spawned tool calls.
            hooks: Some(hooks_arc.clone()),
        });
        let subagent_arc: std::sync::Arc<dyn crate::tools::Tool> = std::sync::Arc::new(
            crate::subagent::SubAgentTool::new(factory.clone())
                .with_depth(0)
                .with_agent_defs(agent_defs_state.clone()),
        );
        tools.register(subagent_arc.clone());
        // WorkflowRun: model-callable wrapper around `/workflow run`.
        // GUI / --serve get the same engine as the worker's agent
        // (provider + model) plus the live Task tool so scripts'
        // `thclaws.subagent(...)` calls dispatch correctly.
        tools.register(std::sync::Arc::new(
            crate::tools::WorkflowRunTool::new(
                provider.clone(),
                config.model.clone(),
                Some(subagent_arc),
            )
            // stream thclaws.log / subagent progress to the chat mid-run so a
            // multi-minute workflow (research) isn't silent between call+result
            .with_events_tx(events_tx.clone()),
        ));
        factory
    };
    // Wire isolated ("job") skills to the factory just built: the main
    // SkillTool gets a Weak handle so `isolated: true` skills spawn a
    // sub-agent instead of pasting their body + steps inline. Weak keeps
    // the factory → snapshot → registry → SkillTool → factory reference
    // from leaking. Until now the slot was empty ⇒ job skills ran inline.
    let _ = skill_factory_slot.set(std::sync::Arc::downgrade(&factory_state));
    // Apply `disallowed_tools` to the main agent's registry. Until
    // this was wired, the config field was parsed (config.rs maps
    // both flat `disallowedTools` and nested `permissions.deny`)
    // but ignored — only `subagent.rs` honored it. The user's
    // `disallowedTools: ["AskUserQuestion"]` setting now actually
    // takes effect on the main loop too.
    if let Some(denied) = &config.disallowed_tools {
        for name in denied {
            tools.remove(name);
        }
        if !denied.is_empty() {
            eprintln!(
                "[config] main agent disallowed_tools applied: {}",
                denied.join(", ")
            );
        }
    }
    let mut agent = Agent::new(provider, tools.clone(), &config.model, &system)
        .with_max_tokens(config.max_tokens)
        .with_approver(approver.clone())
        .with_cancel(cancel.clone())
        .with_hooks(hooks_arc.clone());
    // Wire the externally-held injection queue (issue #106). The
    // handle hands the same Arc to the IPC layer; the agent drains
    // from it at every tool_result boundary. Doing this BEFORE the
    // first turn (and on every subsequent rebuild — see ChangeCwd
    // and similar paths) means a queued message can't be lost to
    // an agent reconstruction.
    agent.use_injection_queue(injection_queue.clone());
    // Respect the user's configured permission mode (project
    // `.thclaws/settings.json` can set it to "ask"). Without this the
    // GUI's Ask mode flag had no effect because the Agent was built
    // with the default Auto.
    agent.permission_mode = perm_mode;
    // `askTools`: tools that always prompt even under Auto (interactive
    // GUI worker has a real approver, so this bites here).
    if let Some(ask) = &config.ask_tools {
        agent.ask_tools = ask.clone();
    }
    // Mirror the configured mode into the process-wide global so
    // `permissions::current_mode()` (read by the agent's tool-dispatch
    // gate, M2+) starts on the right value before any EnterPlanMode /
    // sidebar-Approve flip can change it.
    crate::permissions::set_current_mode(agent.permission_mode);

    // Per-skill model overrides from settings.json. Built-in skills
    // declare a default `model:` in their embedded SKILL.md
    // frontmatter; users can override per-skill via well-named
    // settings.json fields (e.g. `extract_save_skill_models`). Each
    // such field maps to a specific built-in skill name; populate
    // the generic `skills_state::skill_overrides` map here so
    // `SkillTool::call` can consult it before falling back to the
    // frontmatter. Each new built-in skill that needs settings
    // tunability adds a config field above and one entry here.
    {
        let mut overrides = std::collections::HashMap::new();
        if let Some(spec) = config.extract_save_skill_models.clone() {
            overrides.insert("extract-and-save".to_string(), spec);
        }
        crate::skills_state::set_skill_overrides(overrides);
    }

    // Skill-model resolver. When SkillTool loads a skill whose
    // frontmatter carries `model:`, it calls
    // `skills_state::request_model(spec)`. The closure registered
    // here probes each candidate via `ProviderKind::has_key_available`,
    // writes the first matching model into the agent's
    // `model_override` slot (read fresh by the agent's iteration
    // loop), and emits a chat status note. Falls back to a warning
    // note when no candidate has a usable key.
    {
        let skill_tx = events_tx.clone();
        let override_handle = agent.model_override_handle();
        let fallback_model = config.skill_model_fallback.clone();
        crate::skills_state::set_resolver(move |spec| {
            // A skill's `model:` ages out — the vendor retires the id and the
            // frontmatter still names it. Switching onto a model the
            // catalogue no longer knows turns a working turn into a provider
            // 400, so an unknown id is skipped like a missing key is, and the
            // configured fallback gets its turn at the end of the list.
            let mut candidates: Vec<String> = spec.candidates().to_vec();
            if let Some(fb) = fallback_model.as_ref() {
                if !candidates.iter().any(|c| c == fb) {
                    candidates.push(fb.clone());
                }
            }
            for candidate in &candidates {
                // `provider/model` is how people write model ids in settings
                // and skill frontmatter, but only a few prefixes are real
                // routing prefixes (`openrouter/…`, `ollama/…`) — for the
                // rest `detect` sees an unknown vendor and gives up. Retry on
                // the segment after the slash so `deepseek/deepseek-v4-flash`
                // resolves like the bare id it is.
                let Some(kind) = crate::providers::ProviderKind::detect(candidate).or_else(|| {
                    candidate
                        .split_once('/')
                        .and_then(|(_, rest)| crate::providers::ProviderKind::detect(rest))
                }) else {
                    continue;
                };
                if !crate::model_catalogue::is_known_model(candidate) {
                    continue;
                }
                // Usable if a local key exists OR the gateway proxy can serve
                // this (featured) candidate — so a proxy-only user gets the
                // skill's recommended model instead of falling through.
                let gateway_ok = {
                    let mut cfg = crate::config::AppConfig::load().unwrap_or_default();
                    cfg.model = candidate.clone();
                    crate::providers::thclaws_gateway::gateway_overlay_for_model(&cfg, kind)
                        .is_some()
                };
                if !kind.has_key_available() && !gateway_ok {
                    continue;
                }
                if let Ok(mut g) = override_handle.lock() {
                    *g = Some(candidate.clone());
                }
                crate::skills_state::mark_swap_active();
                let _ = skill_tx.send(ViewEvent::SkillModelNote(format!(
                    "[model → {candidate} (skill recommendation, reverts at end of turn)]"
                )));
                return crate::skills_state::SkillModelOutcome::Switched(candidate.clone());
            }
            let first = spec
                .candidates()
                .first()
                .cloned()
                .unwrap_or_else(|| "<unspecified>".into());
            let _ = skill_tx.send(ViewEvent::SkillModelNote(format!(
                "[skill recommends {first}; you don't have a key for that provider — using current model]"
            )));
            crate::skills_state::SkillModelOutcome::KeptCurrent { recommended: first }
        });
    }

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

    // dev-plan/35 Tier 1: per-user override beats `default_path()`
    // when this worker belongs to a multi-tenant pod.
    let session_store = session_roots
        .as_ref()
        .map(|r| r.sessions_dir.clone())
        .or_else(SessionStore::default_path)
        .map(SessionStore::new);
    // Reuse the most-recent session when it's still EMPTY instead of
    // minting a new one on every launch. Each mint writes a header below
    // (so SessionStore::list sees it), so without this an empty session
    // file would pile up every time the app starts. A non-empty latest is
    // left untouched — desktop still lands on a clean session (e14fdf8a),
    // just without spawning a duplicate empty one. The reused session
    // adopts the current default model.
    let mut current_session = Session::new(&config.model, cwd.to_string_lossy());
    if let Some(store) = session_store.as_ref() {
        if let Ok(Some(mut empty)) = store.reuse_empty_latest() {
            empty.model = config.model.clone();
            current_session = empty;
        }
    }
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
        session_roots: session_roots.clone(),
        tool_registry: tools,
        system_prompt: system,
        cwd,
        approver,
        workflow_approver,
        skill_store,
        mcp_clients,
        warned_file_size: false,
        lead_log,
        cancel: cancel.clone(),
        active_loop: None,
        injection_queue: injection_queue.clone(),
        // Init true: the very first /loop /goal continue firing
        // happens before any turn has run, so the suppression check
        // would otherwise gate the loop forever on iteration 0.
        last_turn_made_tool_calls: true,
        agent_factory: factory_state,
        factory_snapshot: factory_snapshot_state,
        agent_defs: agent_defs_state,
        line_session: None,
        line_pre_mode: None,
        line_pre_approver: None,
        phone_home_session: None,
        telegram_session: None,
        telegram_pre_mode: None,
        telegram_pre_approver: None,
        messenger_session: None,
        messenger_pre_mode: None,
        messenger_pre_approver: None,
        session_cost_usd: 0.0,
        last_settings_fingerprint: None,
        #[cfg(feature = "cost_bridge")]
        cost_bridge: Some(crate::cost_bridge::spawn()),
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

    // Plan-07 Phase 1.3: auto-reconnect the LINE bridge on worker
    // boot when a binding token is already on disk. `LineConfig::load`
    // returns Ok(None) when the file's absent — that's the common
    // case, and we just skip silently.
    match crate::line::LineConfig::load() {
        Ok(Some(cfg)) => {
            let _ = input_tx_self.send(ShellInput::LineConnect(cfg));
        }
        Ok(None) => {}
        Err(e) => eprintln!("[line] failed to load on-disk config: {e}"),
    }

    // dev-plan/29 Tier 1: auto-reconnect the Telegram bridge on boot
    // when a runtime config is on disk, enabled, and a token resolves
    // (file or `TELEGRAM_BOT_TOKEN`). Mirrors the LINE block above.
    match crate::telegram::TelegramConfig::load() {
        Ok(Some(cfg)) if cfg.enabled && cfg.resolved_token().is_some() => {
            let _ = input_tx_self.send(ShellInput::TelegramConnect(cfg));
        }
        Ok(_) => {}
        Err(e) => eprintln!("[telegram] failed to load on-disk config: {e}"),
    }

    // dev-plan/31: auto-reconnect the Messenger bridge on boot when a
    // binding token is already on disk. Mirrors the LINE block above.
    match crate::messenger::MessengerConfig::load() {
        Ok(Some(cfg)) if !cfg.binding_token.trim().is_empty() => {
            let _ = input_tx_self.send(ShellInput::MessengerConnect(cfg));
        }
        Ok(_) => {}
        Err(e) => eprintln!("[messenger] failed to load on-disk config: {e}"),
    }

    // dev-plan/44 Tier 1: auto-reconnect the phone-home tunnel on boot
    // when a binding is on disk. Mirrors the blocks above.
    match crate::phone_home::PhoneHomeConfig::load() {
        Ok(Some(cfg)) if !cfg.binding_token.trim().is_empty() => {
            let _ = input_tx_self.send(ShellInput::PhoneHomeConnect(cfg));
        }
        Ok(_) => {}
        Err(e) => eprintln!("[phone-home] failed to load on-disk config: {e}"),
    }

    // Lead inbox poller — parity with repl.rs:1524. Without this, teammates
    // message the lead, messages pile up in `.thclaws/team/inboxes/lead.json`
    // unread, and the team stalls waiting for the lead to react.
    if team_enabled {
        let poller_tx = input_tx_self.clone();
        tokio::spawn(async move {
            // Absolute team dir (not the relative default) so a mid-session
            // cwd change doesn't desync this poller from the teammates.
            let mailbox = crate::team::Mailbox::new(crate::team::resolved_team_dir());
            loop {
                // Lead heartbeat for liveness/staleness checks.
                let _ = mailbox.write_status("lead", "active", None);
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
                maybe_reset_session_per_turn(&mut state, &events_tx, &plan_persist_path);
                handle_line(text, &mut state, &events_tx, &cancel, &input_tx_self).await;
            }
            ShellInput::IsolatedLine(text) => {
                cancel.reset();
                handle_isolated_line(text, &mut state, &events_tx, &cancel, &input_tx_self).await;
            }
            ShellInput::LineWithImages { text, images } => {
                cancel.reset();
                maybe_reset_session_per_turn(&mut state, &events_tx, &plan_persist_path);
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
                // dev-plan/27: auto-learn before we lose the session's
                // history. The agent currently has it loaded; ingest
                // depends on that.
                run_auto_learn_pipeline(&mut state, &events_tx, &cancel, &input_tx_self).await;
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
                // Capture the outgoing session id BEFORE the load so we can
                // tell a real session switch from a same-id reload below.
                let prev_session_id = state.session.id.clone();
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
                    // Credentials are OK if the provider has a local key OR the
                    // gateway proxy can serve the session's (featured) model —
                    // mirror build_provider so a proxy-only user can load a
                    // session recorded against a featured model with no BYOK key.
                    let gateway_ok = {
                        let mut probe = state.config.clone();
                        probe.model = loaded.model.clone();
                        crate::providers::thclaws_gateway::gateway_overlay_for_model(
                            &probe,
                            target_kind,
                        )
                        .is_some()
                    };
                    if !kind_has_credentials(target_kind) && !gateway_ok {
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
                // Rehydrate the provider-side session id BEFORE
                // `state.session = loaded` overwrites the in-memory
                // session — the next `agent.run_turn` will then pass
                // `--resume <uuid>` to the SDK subprocess and the
                // server-side conversation comes back instead of
                // restarting from scratch. Pre-fix this hop was
                // missing and resumed sessions silently lost their
                // SDK-side history.
                state
                    .agent
                    .provider()
                    .set_provider_session_id(loaded.provider_session_id.clone());
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
                //
                // Guard on a REAL switch (id != prev): a same-id reload —
                // e.g. the frontend's startup auto-load firing right after
                // the user entered plan mode + submitted a plan — must NOT
                // clobber the current session's ephemeral yolo / plan mode.
                // That race reset the mode to Auto and dropped the sidebar
                // Approve button the first time /plan was used in a fresh
                // workspace (plan_state survives via restore_from_session
                // above, so "type approve" still worked — mode was the only
                // casualty).
                if id != prev_session_id {
                    state.approver.reset_session_flag();
                    let _ = crate::permissions::take_pre_plan_mode();
                    crate::permissions::set_current_mode_and_broadcast(state.agent.permission_mode);
                }
                let display = DisplayMessage::from_session(&state.session);
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
                let tool_count = tool_infos.len();
                for info in tool_infos {
                    let tool = crate::mcp::McpTool::new(client.clone(), info);
                    state.tool_registry.register(std::sync::Arc::new(tool));
                }
                // docs/browser Phase 1: expose the engine-managed
                // browser client to the IPC layer (Browser-tab
                // screenshot capture bypasses the agent loop).
                if server_name == "browser" {
                    *browser_mcp.write().unwrap() = Some(client.clone());
                }
                state.mcp_clients.push(client);
                // Re-assemble the system prompt FIRST so the new
                // server's `# MCP server instructions` section (if it
                // returned one via InitializeResult.instructions)
                // lands in the system text before `rebuild_agent`
                // captures it. rebuild_agent reuses self.system_prompt;
                // skipping rebuild_system_prompt here would leave the
                // instructions stranded until the next /reload.
                state.rebuild_system_prompt();
                // Rebuild so the agent actually sees the newly-registered
                // MCP tools on its next turn.
                if let Err(e) = state.rebuild_agent(true) {
                    // The MCP server is fine — its tools are already in the
                    // registry. rebuild_agent failed because the *current
                    // model's* provider couldn't be built (usually a missing
                    // key / unroutable model), which blocks normal turns too,
                    // not just MCP. Attribute it to the model, not the server,
                    // so the user fixes the right thing. Tools attach on the
                    // next successful rebuild (/reload after resolving).
                    let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                        "[mcp] '{server_name}' connected ({tool_count} tools)"
                    )));
                    let _ = events_tx.send(ViewEvent::ErrorText(format!(
                        "Current model '{}' isn't ready — {e}. Set a key or switch model with /model, then /reload.",
                        state.config.model
                    )));
                } else {
                    let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                        "[mcp] '{server_name}' connected ({tool_count} tools)"
                    )));
                }
                // Update sidebar with real tool count now that the server is live.
                // (No `cfg(feature = "gui")` — the whole module is already
                // gated at file scope; the inner cfg block was redundant.)
                crate::gui::update_mcp_tool_count(&server_name, tool_count);
                crate::gui::clear_mcp_failure(&server_name);
                let payload = crate::gui::build_mcp_update_payload();
                let _ = events_tx.send(ViewEvent::McpUpdate(payload.to_string()));
            }
            ShellInput::McpFailed { server_name, error } => {
                crate::gui::record_mcp_failure(&server_name, &error);
                let _ = events_tx.send(ViewEvent::ErrorText(format!(
                    "[mcp] '{server_name}' failed to start: {error}"
                )));
                let payload = crate::gui::build_mcp_update_payload();
                let _ = events_tx.send(ViewEvent::McpUpdate(payload.to_string()));
            }
            ShellInput::McpConnect(server_cfg) => {
                let approver_for_spawn = state.approver.clone();
                let input_tx_for_spawn = input_tx_self.clone();
                let server_cfg = *server_cfg;
                crate::gui::clear_mcp_failure(&server_cfg.name);
                tokio::spawn(async move {
                    let server_name = server_cfg.name.clone();
                    match crate::mcp::McpClient::spawn_with_approver(
                        server_cfg,
                        Some(approver_for_spawn),
                    )
                    .await
                    {
                        Ok(client) => match client.list_tools().await {
                            Ok(tools) => {
                                let _ = input_tx_for_spawn.send(ShellInput::McpReady {
                                    server_name,
                                    client,
                                    tools,
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
            ShellInput::SkillsRefresh => {
                // Same live-refresh the `/skill install` slash arm does:
                // swap the SkillTool's store contents, then recompute
                // the system prompt so the new skill is advertised.
                let refreshed = crate::skills::SkillStore::discover();
                let count = refreshed.skills.len();
                if let Ok(mut store) = state.skill_store.lock() {
                    *store = refreshed;
                }
                state.rebuild_system_prompt();
                if let Err(e) = state.rebuild_agent(true) {
                    let _ = events_tx.send(ViewEvent::ErrorText(format!(
                        "[skills] refresh failed: {e}"
                    )));
                } else {
                    let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                        "[skills] reloaded ({count} available)"
                    )));
                }
            }
            ShellInput::McpDisconnect { server_name } => {
                // Tool names are `<sanitized-server>__<tool>`, so the
                // prefix is what identifies this server's tools —
                // sanitize the same way the registration did or a
                // server named `my.server` never matches.
                let prefix = format!(
                    "{}{}",
                    crate::mcp::sanitize_tool_name_segment(&server_name),
                    crate::mcp::MCP_NAME_SEPARATOR
                );
                let doomed: Vec<String> = state
                    .tool_registry
                    .names()
                    .iter()
                    .filter(|n| n.starts_with(&prefix))
                    .map(|n| n.to_string())
                    .collect();
                for name in &doomed {
                    state.tool_registry.remove(name);
                }
                state.mcp_clients.retain(|c| c.name() != server_name);
                crate::gui::clear_mcp_failure(&server_name);
                crate::gui::update_mcp_tool_count(&server_name, 0);
                state.sync_factory_snapshot();
                // Drop the server's `# MCP server instructions` section
                // before the agent is rebuilt around the trimmed registry.
                state.rebuild_system_prompt();
                if let Err(e) = state.rebuild_agent(true) {
                    let _ = events_tx.send(ViewEvent::ErrorText(format!(
                        "[mcp] '{server_name}' detached but rebuild failed: {e}"
                    )));
                } else {
                    let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                        "[mcp] '{server_name}' disconnected ({} tool(s) removed)",
                        doomed.len()
                    )));
                }
                let payload = crate::gui::build_mcp_update_payload();
                let _ = events_tx.send(ViewEvent::McpUpdate(payload.to_string()));
            }
            ShellInput::LineConnect(line_cfg) => {
                // If a session is already running, cancel it
                // first — the new pair always wins.
                if let Some(prev) = state.line_session.take() {
                    prev.cancel.cancel();
                    // Don't restore mode/approver here — the new
                    // session will replace them in a moment. The
                    // stash from the *original* connect is what
                    // we want to keep so LineDisconnect lands
                    // back on the user's pre-LINE posture.
                }
                // Stash profile fields before we move `line_cfg`
                // into `bootstrap::spawn` — they go on the
                // `line_status` broadcast so the GUI sidebar pill
                // can render the display name + avatar.
                let pair_display_name = line_cfg.display_name.clone();
                let pair_picture_url = line_cfg.picture_url.clone();
                let handle = crate::line::bootstrap::spawn(line_cfg, input_tx_self.clone());

                // Plan-07 Phase 2.1: swap permission posture to
                // route approvals through LINE while the bridge
                // is connected. Stash the pre-existing values so
                // LineDisconnect can put them back.
                //
                // Critical: stash the *AGENT's* permission_mode,
                // not the global. `rebuild_agent` preserves
                // `agent.permission_mode` (line 611+627); if we
                // only update the global via
                // `set_current_mode_and_broadcast`, the agent
                // stays in its prior mode (typically `Auto`) and
                // `agent.permission_mode.asks_for_approval()`
                // returns false → mutating tools run silently.
                // This was C3 from the post-deploy audit.
                if state.line_pre_mode.is_none() {
                    state.line_pre_mode = Some(state.agent.permission_mode);
                    state.line_pre_approver = Some(state.approver.clone());
                }
                crate::permissions::set_current_mode_and_broadcast(
                    crate::permissions::PermissionMode::LineGated,
                );
                state.approver =
                    handle.approver.clone() as std::sync::Arc<dyn crate::permissions::ApprovalSink>;
                if let Err(e) = state.rebuild_agent(true) {
                    eprintln!("[line] rebuild_agent after mode swap failed: {e}");
                }
                // Force the rebuilt agent into LineGated. Without
                // this, `rebuild_agent`'s prev_perm restore puts
                // the agent back into whatever mode it was in
                // before the connect.
                state.agent.permission_mode = crate::permissions::PermissionMode::LineGated;

                let payload = serde_json::json!({
                    "type": "line_status",
                    "state": handle.status.state,
                    "server_url": handle.status.server_url,
                    "pending_approvals": handle.status.pending_approvals,
                    "display_name": pair_display_name,
                    "picture_url": pair_picture_url,
                });
                state.line_session = Some(handle);
                let _ = events_tx.send(ViewEvent::LineStatus(payload.to_string()));
                let _ = events_tx.send(ViewEvent::SlashOutput(
                    "[line] bridge connected · permissions routed to LINE".into(),
                ));
            }
            ShellInput::LineDisconnect => {
                // Tell the relay to drop our binding BEFORE we cancel
                // the WS / delete the on-disk JWT. Without this the
                // server still thinks the user is paired and would
                // route their next LINE message into a dead WS;
                // worse, the user couldn't re-pair until the 30-day
                // binding TTL expired. Best-effort — log a network
                // failure but continue with local cleanup; the
                // server's presence check will fall back to issuing
                // a fresh code when the user next messages the OA.
                if let Ok(Some(cfg)) = crate::line::LineConfig::load() {
                    let client = crate::line::LineClient::new(cfg);
                    tokio::spawn(async move {
                        if let Err(e) = client.unpair().await {
                            eprintln!("[line] /unpair failed (continuing): {e}");
                        }
                    });
                }
                if let Some(handle) = state.line_session.take() {
                    handle.cancel.cancel();
                }
                // Plan-07 Phase 2.1: restore the pre-connect mode
                // + approver so the local Ask/Auto/Plan posture
                // resumes immediately. No-op if no stash exists
                // (shouldn't happen, but defensively safe). Same
                // C3 fix as LineConnect — restore on the AGENT's
                // permission_mode, not just the global.
                if let Some(prev_mode) = state.line_pre_mode.take() {
                    crate::permissions::set_current_mode_and_broadcast(prev_mode);
                    state.agent.permission_mode = prev_mode;
                }
                if let Some(prev_approver) = state.line_pre_approver.take() {
                    state.approver = prev_approver;
                    if let Err(e) = state.rebuild_agent(true) {
                        eprintln!("[line] rebuild_agent after restore failed: {e}");
                    }
                }
                // Delete the on-disk config so the next worker
                // boot doesn't auto-reconnect.
                if let Err(e) = crate::line::LineConfig::delete() {
                    eprintln!("[line] delete on-disk config: {e}");
                }
                let payload = serde_json::json!({
                    "type": "line_status",
                    "state": "disconnected",
                    "server_url": "",
                    "pending_approvals": 0,
                });
                let _ = events_tx.send(ViewEvent::LineStatus(payload.to_string()));
                let _ = events_tx.send(ViewEvent::SlashOutput("[line] bridge disconnected".into()));
            }
            ShellInput::LineMessage { text, respond } => {
                // Plan-07 Phase 2: drive the live agent for an
                // inbound LINE message. Subscribe to `events_tx`
                // BEFORE the turn starts, accumulate
                // `AssistantTextDelta` until `TurnDone`, then
                // answer the LineSession via the oneshot. The
                // bridge POSTs the captured text back to the
                // relay's `/reply/{id}` endpoint inside its own
                // task — we just hand the text over.
                //
                // The collector runs in parallel to the turn so
                // it doesn't block the broadcast bus; the turn
                // itself goes through the existing `handle_line`
                // path so slash / bang / goal intercepts behave
                // identically to GUI-driven prompts.
                //
                // Plan-10 Phase 2: simultaneously fan each event
                // to `/chat-bridge/event` on the relay so a
                // browser chat (if connected) sees the streaming
                // reply. Server-side, the broker drops the message
                // when no browser is connected — so the fan-out
                // is harmless overhead for OA-only users.
                let bridge_client = state.line_session.as_ref().map(|s| s.client.clone());
                let mut event_rx = events_tx.subscribe();
                let collector = tokio::spawn(async move {
                    // Plan-07 Phase 2.2: capture only the FINAL
                    // assistant text — everything emitted after
                    // the last `ToolCallStart` of the turn.
                    // Intermediate "I'll do X next" narration
                    // between tool calls would just be noise in
                    // LINE chat, and the tool calls themselves
                    // are already gated through the LineApprover
                    // when LineGated mode is active.
                    //
                    // Plan-10 fan-out (browser chat).
                    //
                    // History: an earlier implementation called
                    // `tokio::spawn` per delta to POST the envelope
                    // to /chat-bridge/event. That detached each
                    // POST as an independent task, which raced
                    // through reqwest's connection pool — the relay
                    // received envelopes in non-deterministic
                    // order, and any POST that hit a transient
                    // error was eprintln'd and silently lost. The
                    // browser bubble accumulated a scrambled,
                    // partial transcript (May 2026 user report).
                    //
                    // Fix: a dedicated *sequencer* task pulls from
                    // an unbounded mpsc and POSTs envelopes one at
                    // a time, awaiting completion between sends.
                    // Order is preserved end-to-end (single TCP
                    // path → in-order arrival at the relay →
                    // relay's local mpsc + single-task WS handler
                    // are already in-order). The unbounded channel
                    // keeps the broadcast-bus consumer (this loop)
                    // non-blocking even if HTTP RTT spikes.
                    let bridge_tx = bridge_client.as_ref().map(|client| {
                        let (tx, mut rx) =
                            tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
                        let client = client.clone();
                        tokio::spawn(async move {
                            while let Some(envelope) = rx.recv().await {
                                // Single-shot retry covers transient
                                // failures (connection reset during
                                // pool churn, brief 502 from a relay
                                // mid-rollout). Permanent failures
                                // (4xx, sustained outage) still
                                // surface as eprintln after one retry
                                // and the chunk is lost — that's
                                // a separate Phase-2 hardening item.
                                if let Err(first) = client.push_chat_event(envelope.clone()).await {
                                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                                    if let Err(second) = client.push_chat_event(envelope).await {
                                        eprintln!(
                                            "[line] chat-bridge push failed (retry exhausted): \
                                             first={first}, second={second}"
                                        );
                                    }
                                }
                            }
                        });
                        tx
                    });

                    let mut buf = String::new();
                    while let Ok(ev) = event_rx.recv().await {
                        if let Some(tx) = &bridge_tx {
                            if let Some(envelope) = view_event_to_chat_envelope(&ev) {
                                // UnboundedSender::send only fails
                                // if the receiver dropped — which
                                // means the sequencer task has
                                // already exited. Ignore: the turn
                                // will end shortly anyway.
                                let _ = tx.send(envelope);
                            }
                        }
                        match ev {
                            ViewEvent::AssistantTextDelta(s) => buf.push_str(&s),
                            ViewEvent::ToolCallStart { .. } => buf.clear(),
                            ViewEvent::TurnDone => break,
                            // ErrorText also signals the turn
                            // ended (cancel / fatal). Capture
                            // the message so the user sees the
                            // failure in LINE instead of silence.
                            ViewEvent::ErrorText(s) => {
                                if buf.is_empty() {
                                    buf.push_str(&s);
                                } else {
                                    buf.push_str("\n\n");
                                    buf.push_str(&s);
                                }
                                break;
                            }
                            _ => {}
                        }
                    }
                    // Drop the tx so the sequencer task can drain
                    // its queue and exit cleanly. The outer
                    // `collector.await` later will block until both
                    // this task and the sequencer it owns finish —
                    // we don't await the sequencer directly because
                    // we want it to keep flushing in the background
                    // while the caller proceeds.
                    drop(bridge_tx);
                    buf
                });
                // Mark this turn as LINE-driven so AskUserQuestion
                // short-circuits with a "please ask in your reply"
                // message instead of routing to a GUI modal the
                // user can't see. Cleared after the turn finishes
                // — back-to-back GUI turns then behave normally.
                crate::tools::ask::set_line_driven_turn(true);
                handle_line(text, &mut state, &events_tx, &cancel, &input_tx_self).await;
                crate::tools::ask::set_line_driven_turn(false);
                let final_text = collector.await.unwrap_or_default();
                let _ = respond.send(final_text);
            }
            ShellInput::TelegramConnect(tg_cfg) => {
                // Validate the token via getMe before committing — a bad
                // token gets clear feedback instead of a silent dead
                // poller. Resolves env-first (TELEGRAM_BOT_TOKEN).
                let Some(token) = tg_cfg.resolved_token() else {
                    let payload = serde_json::json!({
                        "type": "telegram_status",
                        "state": "disconnected",
                        "error": "no bot token (set TELEGRAM_BOT_TOKEN or paste one in the modal)",
                    });
                    let _ = events_tx.send(ViewEvent::TelegramStatus(payload.to_string()));
                    continue;
                };
                let probe = crate::telegram::TelegramClient::new(token);
                let bot_username = match probe.get_me().await {
                    Ok(me) => me.username.map(|u| format!("@{u}")),
                    Err(e) => {
                        let payload = serde_json::json!({
                            "type": "telegram_status",
                            "state": "disconnected",
                            "error": format!("token rejected by Telegram: {e}"),
                        });
                        let _ = events_tx.send(ViewEvent::TelegramStatus(payload.to_string()));
                        let _ = events_tx.send(ViewEvent::ErrorText(format!(
                            "[telegram] connect failed: {e}"
                        )));
                        continue;
                    }
                };

                // New connect always wins — cancel any prior session.
                if let Some(prev) = state.telegram_session.take() {
                    prev.cancel.cancel();
                }
                let handle =
                    crate::telegram::bootstrap::spawn(tg_cfg, bot_username, input_tx_self.clone());

                // Swap permission posture so approvals route through
                // Telegram while connected. Stash the AGENT's mode +
                // approver (not just the global) so disconnect restores
                // exactly — same C3 fix the LINE path documents.
                if state.telegram_pre_mode.is_none() {
                    state.telegram_pre_mode = Some(state.agent.permission_mode);
                    state.telegram_pre_approver = Some(state.approver.clone());
                }
                crate::permissions::set_current_mode_and_broadcast(
                    crate::permissions::PermissionMode::TelegramGated,
                );
                state.approver =
                    handle.approver.clone() as std::sync::Arc<dyn crate::permissions::ApprovalSink>;
                if let Err(e) = state.rebuild_agent(true) {
                    eprintln!("[telegram] rebuild_agent after mode swap failed: {e}");
                }
                state.agent.permission_mode = crate::permissions::PermissionMode::TelegramGated;

                let payload = telegram_status_payload(&handle);
                state.telegram_session = Some(handle);
                let _ = events_tx.send(ViewEvent::TelegramStatus(payload.to_string()));
                let _ = events_tx.send(ViewEvent::SlashOutput(
                    "[telegram] bridge connected · permissions routed to Telegram".into(),
                ));
            }
            ShellInput::TelegramDisconnect => {
                if let Some(handle) = state.telegram_session.take() {
                    handle.cancel.cancel();
                }
                // Restore pre-connect mode + approver (same C3 fix as
                // LINE — restore on the agent's mode, not just global).
                if let Some(prev_mode) = state.telegram_pre_mode.take() {
                    crate::permissions::set_current_mode_and_broadcast(prev_mode);
                    state.agent.permission_mode = prev_mode;
                }
                if let Some(prev_approver) = state.telegram_pre_approver.take() {
                    state.approver = prev_approver;
                    if let Err(e) = state.rebuild_agent(true) {
                        eprintln!("[telegram] rebuild_agent after restore failed: {e}");
                    }
                }
                // Delete on-disk config so the next boot doesn't
                // auto-reconnect.
                if let Err(e) = crate::telegram::TelegramConfig::delete() {
                    eprintln!("[telegram] delete on-disk config: {e}");
                }
                let _ = events_tx.send(ViewEvent::TelegramStatus(
                    telegram_disconnected_payload().to_string(),
                ));
                let _ = events_tx.send(ViewEvent::SlashOutput(
                    "[telegram] bridge disconnected".into(),
                ));
            }
            ShellInput::TelegramMessage {
                text,
                images,
                respond,
            } => {
                // Drive the live agent for an inbound Telegram message.
                // Subscribe before the turn, accumulate the FINAL
                // assistant text (cleared on each ToolCallStart so only
                // post-last-tool narration survives), answer via the
                // oneshot. The session sink chunks + sends it back.
                let mut event_rx = events_tx.subscribe();
                let collector = tokio::spawn(async move {
                    let mut buf = String::new();
                    while let Ok(ev) = event_rx.recv().await {
                        match ev {
                            ViewEvent::AssistantTextDelta(s) => buf.push_str(&s),
                            ViewEvent::ToolCallStart { .. } => buf.clear(),
                            ViewEvent::TurnDone => break,
                            ViewEvent::ErrorText(s) => {
                                if buf.is_empty() {
                                    buf.push_str(&s);
                                } else {
                                    buf.push_str("\n\n");
                                    buf.push_str(&s);
                                }
                                break;
                            }
                            _ => {}
                        }
                    }
                    buf
                });
                // Remote-driven turn: AskUserQuestion short-circuits to a
                // "please ask in your reply" text instead of a GUI modal
                // the Telegram user can't see (reuses the LINE flag).
                crate::tools::ask::set_line_driven_turn(true);
                if images.is_empty() {
                    handle_line(text, &mut state, &events_tx, &cancel, &input_tx_self).await;
                } else {
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
                crate::tools::ask::set_line_driven_turn(false);
                let final_text = collector.await.unwrap_or_default();
                let _ = respond.send(final_text);
            }
            ShellInput::TelegramPairingApprove { code } => {
                if let Some(handle) = state.telegram_session.as_ref() {
                    if let Some(pair) = handle.pairing.approve(&code) {
                        // Append to allow_from on the shared config and
                        // persist so the approval survives restart.
                        let persisted = {
                            let mut cfg = handle.config.lock().ok();
                            cfg.as_mut().map(|c| {
                                c.add_allowed_user(pair.user_id);
                                (*c).clone()
                            })
                        };
                        if let Some(cfg) = persisted {
                            if let Err(e) = cfg.save() {
                                eprintln!("[telegram] save after pairing approve failed: {e}");
                            }
                        }
                        let client = handle.client.clone();
                        let chat_id = pair.chat_id;
                        tokio::spawn(async move {
                            let _ = client
                                .send_text(
                                    chat_id,
                                    "✅ You're approved! Send a message to start chatting with thClaws.",
                                )
                                .await;
                        });
                        let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                            "[telegram] paired {}",
                            pair.display
                        )));
                    }
                    let payload = telegram_status_payload(handle);
                    let _ = events_tx.send(ViewEvent::TelegramStatus(payload.to_string()));
                }
            }
            ShellInput::TelegramPairingReject { code } => {
                if let Some(handle) = state.telegram_session.as_ref() {
                    if let Some(pair) = handle.pairing.reject(&code) {
                        let client = handle.client.clone();
                        let chat_id = pair.chat_id;
                        tokio::spawn(async move {
                            let _ = client
                                .send_text(chat_id, "🚫 Your pairing request was declined.")
                                .await;
                        });
                    }
                    let payload = telegram_status_payload(handle);
                    let _ = events_tx.send(ViewEvent::TelegramStatus(payload.to_string()));
                }
            }
            ShellInput::TelegramStatusRequest => {
                let payload = match state.telegram_session.as_ref() {
                    Some(handle) => telegram_status_payload(handle),
                    None => telegram_disconnected_payload(),
                };
                let _ = events_tx.send(ViewEvent::TelegramStatus(payload.to_string()));
            }
            ShellInput::PhoneHomeConnect(ph_cfg) => {
                // Last connect wins — cancel any prior tunnel.
                if let Some(prev) = state.phone_home_session.take() {
                    prev.cancel.cancel();
                }
                let handle = crate::phone_home::spawn(ph_cfg, input_tx_self.clone());
                let server_url = handle.server_url.clone();

                // dev-plan/44 streaming: fan every ViewEvent up the tunnel
                // so a connected dashboard (/talk) mirrors the live
                // conversation — local turns included. A dedicated
                // sequencer keeps pushes ordered (same fix as the LINE
                // chat-bridge). Lives until the tunnel's cancel fires.
                {
                    let fan_client = handle.client.clone();
                    let fan_cancel = handle.cancel.clone();
                    let mut event_rx = events_tx.subscribe();
                    tokio::spawn(async move {
                        let (tx, mut rx) =
                            tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
                        let push_client = fan_client.clone();
                        tokio::spawn(async move {
                            while let Some(env) = rx.recv().await {
                                if let Err(e) = push_client.push_event(env).await {
                                    eprintln!("[phone-home] event push failed: {e}");
                                }
                            }
                        });
                        loop {
                            tokio::select! {
                                _ = fan_cancel.cancelled() => break,
                                ev = event_rx.recv() => match ev {
                                    Ok(ev) => {
                                        if let Some(env) = view_event_to_chat_envelope(&ev) {
                                            let _ = tx.send(env);
                                        }
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                        continue
                                    }
                                    Err(_) => break,
                                }
                            }
                        }
                    });
                }

                state.phone_home_session = Some(handle);
                // No permission-posture swap: approvals stay on the local
                // Ask/Auto/Plan posture so a cloud-driven turn still hits
                // the user's local approval prompts (dev-plan/44 security
                // note). A status pill / approval-over-bridge is follow-up;
                // for now a SlashOutput notice is enough.
                let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                    "[thClaws Remote] connected → {server_url} (approvals stay local)"
                )));
            }
            ShellInput::PhoneHomeDisconnect => {
                if let Some(handle) = state.phone_home_session.take() {
                    handle.cancel.cancel();
                }
                // Best-effort unpair so the relay drops the binding, then
                // delete the on-disk config so the next boot doesn't
                // auto-reconnect.
                if let Ok(Some(cfg)) = crate::phone_home::PhoneHomeConfig::load() {
                    let client = std::sync::Arc::new(crate::phone_home::build_client(
                        cfg,
                        crate::cancel::CancelToken::new(),
                    ));
                    tokio::spawn(async move {
                        if let Err(e) = client.unpair().await {
                            eprintln!("[phone-home] /unpair failed (continuing): {e}");
                        }
                    });
                }
                if let Err(e) = crate::phone_home::PhoneHomeConfig::delete() {
                    eprintln!("[phone-home] delete on-disk config: {e}");
                }
                let _ = events_tx.send(ViewEvent::SlashOutput(
                    "[thClaws Remote] disconnected".into(),
                ));
            }
            ShellInput::PhoneHomePair => {
                match crate::cloud::token() {
                    None => {
                        let _ = events_tx.send(ViewEvent::SlashOutput(
                            "[thClaws Remote] not enabled: log in to thClaws.cloud first (Settings → thClaws.cloud)".into(),
                        ));
                    }
                    Some(token) => {
                        let relay_base = std::env::var("THCLAWS_PHONE_HOME_SERVER")
                            .ok()
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or_else(|| {
                                crate::phone_home::config::DEFAULT_SERVER_URL.to_string()
                            });
                        let machine_label =
                            std::env::var("HOSTNAME").unwrap_or_else(|_| "thclaws".to_string());
                        let tx = input_tx_self.clone();
                        let events = events_tx.clone();
                        // Network round-trip — spawn so the worker loop
                        // keeps draining input. On success, connect.
                        tokio::spawn(async move {
                            match crate::phone_home::pair(&relay_base, &token, Some(machine_label))
                                .await
                            {
                                Ok(cfg) => {
                                    let _ = events.send(ViewEvent::SlashOutput(
                                        "[thClaws Remote] enabled — connecting…".into(),
                                    ));
                                    let _ = tx.send(ShellInput::PhoneHomeConnect(cfg));
                                }
                                Err(e) => {
                                    let _ = events.send(ViewEvent::SlashOutput(format!(
                                        "[thClaws Remote] enable failed: {e}"
                                    )));
                                }
                            }
                        });
                    }
                }
            }
            ShellInput::MessengerConnect(msgr_cfg) => {
                // New connect always wins — cancel any prior session.
                if let Some(prev) = state.messenger_session.take() {
                    prev.cancel.cancel();
                }
                let handle = crate::messenger::bootstrap::spawn(msgr_cfg, input_tx_self.clone());

                // Swap permission posture so approvals route through
                // Messenger while connected. Stash the AGENT's mode +
                // approver (not just the global) so disconnect restores
                // exactly — same C3 fix the LINE/Telegram paths document.
                if state.messenger_pre_mode.is_none() {
                    state.messenger_pre_mode = Some(state.agent.permission_mode);
                    state.messenger_pre_approver = Some(state.approver.clone());
                }
                crate::permissions::set_current_mode_and_broadcast(
                    crate::permissions::PermissionMode::MessengerGated,
                );
                state.approver =
                    handle.approver.clone() as std::sync::Arc<dyn crate::permissions::ApprovalSink>;
                if let Err(e) = state.rebuild_agent(true) {
                    eprintln!("[messenger] rebuild_agent after mode swap failed: {e}");
                }
                state.agent.permission_mode = crate::permissions::PermissionMode::MessengerGated;

                let payload = messenger_status_payload(&handle);
                state.messenger_session = Some(handle);
                let _ = events_tx.send(ViewEvent::MessengerStatus(payload.to_string()));
                let _ = events_tx.send(ViewEvent::SlashOutput(
                    "[messenger] bridge connected · permissions routed to Messenger".into(),
                ));
            }
            ShellInput::MessengerDisconnect => {
                // Tell the relay to drop our binding before local cleanup
                // so the next inbound message re-issues a pairing code
                // instead of routing into a dead WS. Best-effort.
                if let Ok(Some(cfg)) = crate::messenger::MessengerConfig::load() {
                    let client = crate::messenger::MessengerClient::new(cfg);
                    tokio::spawn(async move {
                        if let Err(e) = client.unpair().await {
                            eprintln!("[messenger] /unpair failed (continuing): {e}");
                        }
                    });
                }
                if let Some(handle) = state.messenger_session.take() {
                    handle.cancel.cancel();
                }
                // Restore pre-connect mode + approver (same C3 fix).
                if let Some(prev_mode) = state.messenger_pre_mode.take() {
                    crate::permissions::set_current_mode_and_broadcast(prev_mode);
                    state.agent.permission_mode = prev_mode;
                }
                if let Some(prev_approver) = state.messenger_pre_approver.take() {
                    state.approver = prev_approver;
                    if let Err(e) = state.rebuild_agent(true) {
                        eprintln!("[messenger] rebuild_agent after restore failed: {e}");
                    }
                }
                if let Err(e) = crate::messenger::MessengerConfig::delete() {
                    eprintln!("[messenger] delete on-disk config: {e}");
                }
                let _ = events_tx.send(ViewEvent::MessengerStatus(
                    messenger_disconnected_payload().to_string(),
                ));
                let _ = events_tx.send(ViewEvent::SlashOutput(
                    "[messenger] bridge disconnected".into(),
                ));
            }
            ShellInput::MessengerMessage { text, respond } => {
                // Drive the live agent for an inbound Messenger message.
                // Subscribe before the turn, accumulate the FINAL
                // assistant text (cleared on each ToolCallStart), answer
                // via the oneshot. The session sink chunks + sends it
                // back through the relay's Send API.
                let mut event_rx = events_tx.subscribe();
                let collector = tokio::spawn(async move {
                    let mut buf = String::new();
                    while let Ok(ev) = event_rx.recv().await {
                        match ev {
                            ViewEvent::AssistantTextDelta(s) => buf.push_str(&s),
                            ViewEvent::ToolCallStart { .. } => buf.clear(),
                            ViewEvent::TurnDone => break,
                            ViewEvent::ErrorText(s) => {
                                if buf.is_empty() {
                                    buf.push_str(&s);
                                } else {
                                    buf.push_str("\n\n");
                                    buf.push_str(&s);
                                }
                                break;
                            }
                            _ => {}
                        }
                    }
                    buf
                });
                // Remote-driven turn: AskUserQuestion short-circuits to a
                // text prompt instead of a GUI modal (reuses the LINE flag).
                crate::tools::ask::set_line_driven_turn(true);
                handle_line(text, &mut state, &events_tx, &cancel, &input_tx_self).await;
                crate::tools::ask::set_line_driven_turn(false);
                let final_text = collector.await.unwrap_or_default();
                let _ = respond.send(final_text);
            }
            ShellInput::MessengerStatusRequest => {
                let payload = match state.messenger_session.as_ref() {
                    Some(handle) => messenger_status_payload(handle),
                    None => messenger_disconnected_payload(),
                };
                let _ = events_tx.send(ViewEvent::MessengerStatus(payload.to_string()));
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
                                originator: crate::permissions::AgentOrigin::Main,
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
                    // Drop the deleted session's in-memory history first so
                    // neither the fallback load nor the fresh mint can
                    // resurrect the just-deleted file via `save_history`
                    // (which no-ops on empty history).
                    state.agent.clear_history();
                    // Prefer activating the most recent *remaining* session
                    // over minting a blank one — deleting a session and
                    // landing on a surprise empty session is poor UX. The
                    // store's `list()` is sorted newest-first and the
                    // deleted id is already gone from disk.
                    let latest_id = state
                        .session_store
                        .as_ref()
                        .and_then(|s| s.list().ok())
                        .and_then(|metas| metas.into_iter().next().map(|m| m.id));
                    if let Some(next_id) = latest_id {
                        // Reuse the full LoadSession path (provider auto-
                        // switch, history rehydrate, plan/goal restore,
                        // HistoryReplaced + sidebar refresh). Re-queued so
                        // the current handler returns first.
                        let _ = input_tx_self.send(ShellInput::LoadSession(next_id));
                    } else {
                        // Nothing left to fall back to — mint a fresh one.
                        state.session =
                            Session::new(&state.config.model, state.cwd.to_string_lossy());
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
                        crate::permissions::set_current_mode_and_broadcast(
                            state.agent.permission_mode,
                        );
                        let _ = events_tx.send(ViewEvent::HistoryReplaced(Vec::new()));
                        let _ = events_tx.send(ViewEvent::SessionListRefresh(build_session_list(
                            &state.session_store,
                            &state.session.id,
                        )));
                        let _ = events_tx.send(ViewEvent::SlashOutput(
                            "(deleted the last session; minted a fresh one)".into(),
                        ));
                    }
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

                // Dedup: `/model` and `/provider` slash commands write
                // settings.json and dispatch ReloadConfig synchronously;
                // the file-watcher then debounces the same write and
                // dispatches a second ReloadConfig 500 ms later. Both
                // events read identical bytes, do identical work, and —
                // critically — emit two "(provider reloaded: …)" lines.
                // Fingerprint the file content and skip a reload whose
                // bytes match the previous successful reload. Manual
                // edits to settings.json still go through (different
                // bytes → different fingerprint).
                let bytes = std::fs::read(crate::config::ProjectConfig::path()).ok();
                let fp = bytes.as_ref().map(|b| {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    b.hash(&mut h);
                    h.finish()
                });
                if fp.is_some() && fp == state.last_settings_fingerprint {
                    continue;
                }
                let prev_model = state.config.model.clone();
                match crate::config::AppConfig::load() {
                    Ok(new_config) => {
                        new_config.apply_process_globals();
                        state.config = new_config;
                    }
                    Err(e) => {
                        let _ = events_tx.send(ViewEvent::ErrorText(format!(
                            "[reload] config load failed, keeping old: {e}"
                        )));
                        continue;
                    }
                }
                let model_changed = state.config.model != prev_model;
                // Always preserve history across config reloads, including
                // when the model itself changed. The JSONL log is the
                // canonical conversation; each provider's stream() builds
                // its own wire payload from ContentBlocks per turn, so
                // mid-conversation provider swaps replay cleanly. Blocks
                // that don't map across providers (Anthropic Thinking on a
                // non-reasoning OpenAI model, etc.) are silently dropped
                // per provider. See thClaws/thClaws#142 — pre-fix this
                // path minted a fresh session on every model change.
                match state.rebuild_agent(true) {
                    Ok(()) => {
                        state.rebuild_system_prompt();
                        if model_changed {
                            // Keep the same session id + JSONL file; append a
                            // `model` event so the switch persists immediately
                            // (not only on the next assistant line) and a
                            // restore recovers it. Updates the label too.
                            if let Some(store) = state.session_store.as_ref() {
                                let path = store.path_for(&state.session.id);
                                let model = state.config.model.clone();
                                let _ = state.session.record_model_to(&path, &model);
                            } else {
                                state.session.model = state.config.model.clone();
                            }
                        }
                        // Record the fingerprint so the watcher's
                        // follow-up dispatch is recognised as a dup.
                        state.last_settings_fingerprint = fp;
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
                        // Tell the frontend that settings-derived flags
                        // (shellTabEnabled, teamEnabled, …) may have
                        // moved. App.tsx subscribes to this and re-
                        // fetches the per-flag IPCs so tab visibility
                        // and similar UI bits update without a page
                        // reload. Driven by the file watcher in
                        // shared_session::spawn_settings_watcher and by
                        // the manual `settings_reload` IPC.
                        let _ = events_tx.send(ViewEvent::SettingsChanged(
                            r#"{"type":"settings_changed"}"#.to_string(),
                        ));
                    }
                    Err(e) => {
                        let _ = events_tx.send(ViewEvent::ErrorText(format!(
                            "[reload] agent rebuild failed: {e}"
                        )));
                    }
                }
            }
            ShellInput::InstructionsChanged => {
                // The Settings menu's AGENTS.md editor (global or folder
                // scope) just saved. ProjectContext::discover re-runs
                // on rebuild and picks up the new file content. No
                // provider rebuild needed — only the system prompt
                // changes. Subsequent turns use the fresh prompt; an
                // already in-flight turn keeps the snapshot it
                // captured (per the agent loop's `let system =
                // self.system.clone();` pattern), which is the right
                // behavior — don't yank context out from under the
                // model mid-thought.
                state.rebuild_system_prompt();
                let _ = events_tx.send(ViewEvent::SlashOutput(
                    "[instructions] system prompt rebuilt — new content applies on next turn"
                        .into(),
                ));
            }
            ShellInput::AgentDefsChanged => {
                // The agent editor saved `.thclaws/agents/<name>.md`.
                // Reload the worker's def snapshot (cheap, no agent
                // rebuild) so side-channel `/agent <name>` spawns see
                // the new/edited def right away.
                let plugin_agent_dirs = crate::plugins::plugin_agent_dirs();
                let mut reloaded =
                    crate::agent_defs::AgentDefsConfig::load_with_extra(&plugin_agent_dirs);
                reloaded.apply_builtin_subagent_overrides(&state.config);
                state.agent_defs = reloaded;
            }
            ShellInput::ChangeCwd(new_cwd) => {
                // No-op short-circuit: the StartupModal's "Start"
                // button sends `set_cwd` even when the path is
                // unchanged, which used to trigger the full session-
                // reset flow below — minting a fresh session and
                // dropping a 248-byte header-only ghost on disk
                // every launch. When the canonical path matches the
                // worker's current cwd, there's nothing to do.
                if paths_equivalent(&new_cwd, &state.cwd) {
                    continue;
                }
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

                // Opening a project in the GUI: migrate its `.thclaws/`
                // layout (v1 flat → v2 `state/`) before the config reload
                // resolves any project paths. Process cwd was already
                // switched by the dispatcher, so this acts on the NEW
                // workspace. No-op once migrated / in multiuser.
                crate::config::ProjectConfig::migrate_workspace_if_needed();

                // Reload config — `AppConfig::load` reads project settings
                // via `ProjectConfig::project_dir()`, which honors
                // $THCLAWS_PROJECT_ROOT first and otherwise current_dir
                // (which the GUI just changed). Result: project settings
                // from the NEW workspace win.
                match crate::config::AppConfig::load() {
                    Ok(new_config) => {
                        new_config.apply_process_globals();
                        state.config = new_config;
                    }
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
                //
                // dev-plan/35 Tier 1: the per-user override (multi-
                // tenant pod) is sticky across cwd changes — a user's
                // session JSONL must keep landing in their own
                // <project>/.thclaws/users/<id>/sessions/ regardless
                // of any `/cd` inside the worker. The single-tenant
                // `None` path keeps the old cwd-relative default.
                state.session_store = state
                    .session_roots
                    .as_ref()
                    .map(|r| r.sessions_dir.clone())
                    .or_else(crate::session::SessionStore::default_path)
                    .map(SessionStore::new);

                // Re-discover skills against the NEW cwd. Without this,
                // project-scoped `<cwd>/.thclaws/skills/<name>/` is
                // pinned to whichever cwd the worker was spawned with
                // (typically the launcher cwd, NOT the project the
                // StartupModal selected). Symptom: `/<skill-name>` in
                // chat fails the `skill_store.contains_key` check at
                // line ~3613 and gets emitted as "unknown command",
                // even though the skill file is on disk in the new
                // project. CLI never hit this because `run_repl`
                // discovers AFTER the user has `cd`'d in their shell
                // and there's no in-session project switch.
                //
                // Skill discovery walks a handful of directories and is
                // cheap; safe to redo on every cwd change. Pre-existing
                // refresh helper at `shell_dispatch::refresh_skill_store
                // _and_rebuild` does the same thing for `/skill install`
                // / `/plugin install`; this mirrors it but skips the
                // separate `rebuild_agent` call because the unconditional
                // hygiene block + `rebuild_system_prompt` below already
                // rebuilds the agent's view via `set_system`.
                if let Ok(mut store) = state.skill_store.lock() {
                    *store = crate::skills::SkillStore::discover();
                }

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

                // Tear down the OLD project's MCP servers and spawn
                // the NEW project's. Pre-fix the cwd-change reloaded
                // config (so the sidebar listed the new project's
                // mcp.json entries) but never re-ran the startup
                // spawn loop — entries showed up with "(0) tools"
                // forever. Hits anyone who launches thClaws from
                // the macOS Dock and then picks a project that has
                // MCP servers, since the initial cwd has no MCP and
                // the project switch is the user's first chance to
                // see them.
                let prefixes_to_drop: Vec<String> = state
                    .mcp_clients
                    .iter()
                    .map(|c| {
                        format!(
                            "{}{}",
                            crate::mcp::sanitize_tool_name_segment(c.name()),
                            crate::mcp::MCP_NAME_SEPARATOR
                        )
                    })
                    .collect();
                let tool_names_to_remove: Vec<String> = state
                    .tool_registry
                    .names()
                    .iter()
                    .filter(|n| prefixes_to_drop.iter().any(|p| n.starts_with(p)))
                    .map(|n| n.to_string())
                    .collect();
                for name in tool_names_to_remove {
                    state.tool_registry.remove(&name);
                }
                // Dropping the Arc<McpClient>s here releases the
                // last refs the worker holds; the subprocesses
                // exit shortly after as their stdio is closed.
                state.mcp_clients.clear();
                crate::gui::clear_mcp_tool_counts();
                // Push the now-trimmed registry into the factory
                // snapshot so a subagent spawned between this remove
                // and the first McpReady doesn't briefly see the
                // old project's MCP tools.
                state.sync_factory_snapshot();

                // Spawn each MCP server in the new project — same
                // `tokio::spawn` + ShellInput::McpReady fan-out as
                // worker startup, so the McpReady handler does the
                // registry + rebuild + sidebar update.
                for server_cfg in state.config.mcp_servers.clone() {
                    let approver_for_spawn = state.approver.clone();
                    let input_tx_for_spawn = input_tx_self.clone();
                    tokio::spawn(async move {
                        let server_name = server_cfg.name.clone();
                        match crate::mcp::McpClient::spawn_with_approver(
                            server_cfg,
                            Some(approver_for_spawn),
                        )
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

                // Push the empty-counts payload now so the sidebar
                // immediately reflects the new project's server
                // list. McpReady → McpUpdate will overwrite with
                // real counts as each spawn completes.
                let payload = crate::gui::build_mcp_update_payload();
                let _ = events_tx.send(ViewEvent::McpUpdate(payload.to_string()));
            }
        }
    }

    // dev-plan/27: auto-learn on app-close path. Runs BEFORE the
    // discard-on-exit check so an empty session doesn't trigger
    // ingest (session_is_substantive guards too, but ordering keeps
    // the discard log accurate). Same agent state the NewSession
    // path uses — history still loaded.
    run_auto_learn_pipeline(&mut state, &events_tx, &cancel, &input_tx_self).await;

    // Discard-on-exit for sessions the user never engaged with.
    // Every thclaws launch mints a fresh session and writes its
    // header to disk on the first event (`write_header_if_missing`
    // fires from plan_state::clear's snapshot broadcaster + similar
    // boot-time events even before the user types anything).
    // Without this hook, opening + immediately closing the app
    // leaves behind a JSONL with just a header + a couple of null
    // plan/goal snapshots — clutter that piles up in the sessions
    // sidebar and gets auto-loaded as "the most recent session" on
    // next launch, which is confusing. If the user never sent a
    // single message AND never gave the session a title, the
    // session has zero user-meaningful content; delete it
    // entirely. Errors are logged but non-fatal — orphan empty
    // session is annoying but not damaging.
    if state.session.messages.is_empty() && state.session.title.is_none() {
        if let Some(ref store) = state.session_store {
            match store.delete(&state.session.id) {
                Ok(()) => eprintln!(
                    "\x1b[2m[session] discarded empty session {} on exit\x1b[0m",
                    state.session.id
                ),
                Err(e) => eprintln!(
                    "\x1b[33m[session] could not discard empty session {}: {e}\x1b[0m",
                    state.session.id
                ),
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

/// dev-plan/27: file the just-finished session into a dedicated KMS
/// and (throttled) run reconcile. Gated on `config.auto_learn`.
///
/// Called from two places, both with the agent still holding the
/// session's history:
///   - `ShellInput::NewSession` handler — before the agent's history
///     is cleared and the session reset.
///   - End of `run_worker` — before the worker tears down on app
///     close.
///
/// Best-effort: failures are appended to the auto-learn audit log
/// (`~/.config/thclaws/auto-learn.log`) but never propagate. The
/// pipeline blocks the calling path while it runs (ingest + reconcile
/// can take 30s–2min); acceptable for an explicitly opt-in feature.
async fn run_auto_learn_pipeline(
    state: &mut WorkerState,
    events_tx: &broadcast::Sender<ViewEvent>,
    cancel: &crate::cancel::CancelToken,
    input_tx: &mpsc::Sender<ShellInput>,
) {
    if !state.config.auto_learn {
        return;
    }
    let message_count = state.session.messages.len();
    if !crate::auto_learn::session_is_substantive(message_count) {
        crate::auto_learn::log_event(&format!(
            "skip ingest: session {} only had {} messages (threshold {})",
            state.session.id,
            message_count,
            crate::auto_learn::MIN_TURNS_FOR_INGEST
        ));
        return;
    }
    let kms_name = state.config.auto_learn_kms.clone();
    if kms_name.trim().is_empty() {
        crate::auto_learn::log_event("skip: auto_learn_kms is empty");
        return;
    }

    // Idempotent KMS bootstrap. Errors from `create` typically mean a
    // name conflict (the KMS already exists) which is the happy path
    // — `kms::resolve` confirms it.
    if crate::kms::resolve(&kms_name).is_none() {
        match crate::kms::create(&kms_name, crate::kms::KmsScope::Project) {
            Ok(_) => crate::auto_learn::log_event(&format!(
                "bootstrap: created project KMS `{kms_name}`"
            )),
            Err(e) => {
                crate::auto_learn::log_event(&format!("skip: KmsCreate({kms_name}) failed: {e}"));
                return;
            }
        }
    }

    // Run ingest synchronously through the main agent. The agent still
    // has the session's history loaded, so `/kms ingest $` semantics
    // work — the model summarizes the conversation and calls KmsWrite.
    let (page, source) =
        crate::repl::resolve_session_alias(None, state.session.title.as_deref(), &state.session.id);
    let ingest_prompt =
        crate::repl::build_kms_ingest_session_prompt(&kms_name, &page, source, false);
    let _ = events_tx.send(ViewEvent::SlashOutput(format!(
        "[auto-learn] filing session as `{kms_name}/{page}`…"
    )));
    let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
    let _ = lead_mb.write_status("lead", "working", None);
    let stream = Box::pin(state.agent.run_turn(ingest_prompt));
    drive_turn_stream(stream, state, events_tx, cancel, &lead_mb, input_tx, None).await;
    crate::auto_learn::mark_ingest_done();
    crate::auto_learn::log_event(&format!(
        "ingest ok: session={} kms={kms_name} page={page}",
        state.session.id
    ));

    // Reconcile — throttled. The reconcile pass is expensive
    // (multi-pass agent rewriting pages), so we cap frequency per
    // `auto_learn_reconcile_hours`.
    let hours = state.config.auto_learn_reconcile_hours;
    if !crate::auto_learn::is_reconcile_due(hours) {
        crate::auto_learn::log_event(&format!(
            "skip reconcile: throttle window {hours}h not elapsed yet"
        ));
        return;
    }
    let reconcile_prompt =
        crate::shell_dispatch::compose_kms_reconcile_prompt(&kms_name, None, true);
    let _ = events_tx.send(ViewEvent::SlashOutput(format!(
        "[auto-learn] reconciling `{kms_name}`…"
    )));
    let stream2 = Box::pin(state.agent.run_turn(reconcile_prompt));
    drive_turn_stream(stream2, state, events_tx, cancel, &lead_mb, input_tx, None).await;
    crate::auto_learn::mark_reconcile_done();
    crate::auto_learn::log_event(&format!(
        "reconcile ok: kms={kms_name} (next due in {hours}h)"
    ));
}

/// Job-runner mode (`config.session_reset_per_turn`): archive the current
/// session and start a fresh one BEFORE a user turn so repeated runs don't
/// accumulate (and re-send) each other's context. Archive-first ⇒ the old
/// session stays on disk, so the audit trail is complete (this is a reset,
/// not a delete). No-op when the mode is off or history is empty. Unlike
/// `NewSession` it deliberately skips the auto-learn pipeline — this fires
/// on every turn, so it must stay cheap.
/// Run one turn on a fresh throwaway child agent (empty history), then
/// discard it — the shared `state.agent`/`state.session` are set aside
/// untouched for the duration, so nothing accumulates. The child is
/// swapped in as the active agent so the shared `drive_turn_stream` path
/// runs verbatim (multi-tenant working-dir scope, gateway member billing
/// attribution, streaming, cancellation), then the real agent + session
/// are restored and the throwaway session is deleted so it never shows up
/// in `/sessions`.
async fn handle_isolated_line(
    text: String,
    state: &mut WorkerState,
    events_tx: &broadcast::Sender<ViewEvent>,
    cancel: &crate::cancel::CancelToken,
    input_tx: &mpsc::Sender<ShellInput>,
) {
    let text = text.trim().to_string();
    if text.is_empty() {
        let _ = events_tx.send(ViewEvent::TurnDone);
        return;
    }
    // Fresh child from the live factory snapshot — same provider / model /
    // tools / system prompt as the main agent, but no history.
    let mut child = match state.agent_factory.build(&text, None, 1).await {
        Ok(a) => a,
        Err(e) => {
            let _ = events_tx.send(ViewEvent::ErrorText(format!(
                "isolated turn could not start: {e}"
            )));
            let _ = events_tx.send(ViewEvent::TurnDone);
            return;
        }
    };
    child.permission_mode = state.agent.permission_mode;

    let saved_agent = std::mem::replace(&mut state.agent, child);
    let saved_session = std::mem::replace(
        &mut state.session,
        Session::new(&state.config.model, state.cwd.to_string_lossy()),
    );
    let throwaway_id = state.session.id.clone();

    let stream = Box::pin(state.agent.run_turn(text));
    let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
    drive_turn_stream(
        stream,
        state,
        events_tx,
        cancel,
        &lead_mb,
        input_tx,
        Some(throwaway_id.clone()),
    )
    .await;

    // Restore the real agent + session; drop the throwaway.
    state.agent = saved_agent;
    state.session = saved_session;
    crate::audit::set_session(&state.session.id);
    if let Some(store) = state.session_store.as_ref() {
        let _ = store.delete(&throwaway_id);
    }
    let _ = events_tx.send(ViewEvent::SessionListRefresh(build_session_list(
        &state.session_store,
        &state.session.id,
    )));
}

fn maybe_reset_session_per_turn(
    state: &mut WorkerState,
    events_tx: &broadcast::Sender<ViewEvent>,
    plan_persist_path: &std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
) {
    if !state.config.session_reset_per_turn || state.agent.history_snapshot().is_empty() {
        return;
    }
    save_history(&state.agent, &mut state.session, &state.session_store);
    state.agent.clear_history();
    state.session = Session::new(&state.config.model, state.cwd.to_string_lossy());
    if let (Some(store), Ok(mut g)) = (state.session_store.as_ref(), plan_persist_path.lock()) {
        let path = store.path_for(&state.session.id);
        let _ = state.session.write_header_if_missing(&path);
        *g = Some(path);
    }
    crate::tools::plan_state::clear();
    state.approver.reset_session_flag();
    let _ = events_tx.send(ViewEvent::HistoryReplaced(Vec::new()));
}

pub(crate) fn save_history(agent: &Agent, session: &mut Session, store: &Option<SessionStore>) {
    let history = agent.history_snapshot();
    if history.is_empty() {
        return;
    }
    session.sync(history);
    if let Some(ref store) = store {
        let _ = store.save(session);
        // Capture any provider-side session id that surfaced during
        // this turn (anthropic-agent SDK populates this from the
        // first response frame) and persist it to the JSONL so a
        // future `/load` can rehydrate it via
        // `Provider::set_provider_session_id`. Without this hop,
        // resume sessions started a fresh SDK conversation that saw
        // only the latest user message — the "LLM forgot previous
        // turns" bug. Skip the append when nothing changed to avoid
        // event-log spam.
        let provider_sid = agent.provider().provider_session_id();
        if provider_sid != session.provider_session_id {
            let path = store.path_for(&session.id);
            let _ = session.append_provider_state_to(&path, provider_sid);
        }
    }
}

pub(crate) fn build_session_list(store: &Option<SessionStore>, current_id: &str) -> String {
    // Was capped at 20 — but the sidebar shows the top 10 in the default
    // view and the rest are reachable only via the search box (#95 part
    // b). Bumping to 200 gives heavy users a meaningful searchable
    // window (each entry is ~100 bytes JSON ⇒ ~20KB payload, fine over
    // WebSocket). For workspaces with >200 sessions, a future change can
    // move filtering server-side; for now the on-disk list() ordering
    // (most-recently-updated first; see SessionStore::list) keeps the
    // newest 200 visible.
    let sessions: Vec<serde_json::Value> = store
        .as_ref()
        .and_then(|s| s.list().ok())
        .unwrap_or_default()
        .into_iter()
        .take(200)
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

    // dev-plan/32 Tier 3 Terminal-tab text approval. When a workflow
    // review is open and the user typed an approval response, resolve
    // it without forwarding to the agent. Non-matching text emits a
    // hint so users on the Terminal tab don't accidentally lose their
    // chat input to the void while the review is blocking.
    {
        let pending = state.workflow_approver.pending_ids();
        if !pending.is_empty() {
            match crate::workflow::parse_chat_decision(trimmed) {
                Some(decision) => {
                    // Resolve the most-recently-registered review.
                    // Typically there's only one open at a time.
                    let id = pending.into_iter().next_back().unwrap();
                    state.workflow_approver.resolve(&id, decision);
                    return;
                }
                None => {
                    let _ = events_tx.send(ViewEvent::SlashOutput(
                        "workflow review pending — type `approve`, `cancel`, or \
                         `rework: <note>` (or click in the Chat tab)"
                            .to_string(),
                    ));
                    return;
                }
            }
        }
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
                // Gap 1 — hard backstop. Past the iteration cap or a budget
                // overrun (1.5× grace beyond the soft GOAL_BUDGET_LIMIT
                // prompt), force the goal Blocked + abort the loop regardless
                // of the model, so an unattended /goal continue loop can't
                // burn unbounded cost.
                if let Some(reason) = g.hard_limit_reached() {
                    crate::goal_state::apply(|gg| {
                        gg.status = crate::goal_state::GoalStatus::Blocked;
                        gg.last_audit = Some(format!("auto-blocked (hard limit): {reason}"));
                        true
                    });
                    let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                        "/goal continue — hard limit: {reason}. Auto-blocking goal + stopping loop."
                    )));
                    if let Some(loop_state) = state.active_loop.take() {
                        loop_state.abort.abort();
                    }
                    let _ = events_tx.send(ViewEvent::TurnDone);
                    return;
                }
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
                drive_turn_stream(
                    stream,
                    state,
                    events_tx,
                    cancel,
                    &lead_mb,
                    input_tx,
                    Some(state.session.id.clone()),
                )
                .await;
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
        drive_turn_stream(
            stream,
            state,
            events_tx,
            cancel,
            &lead_mb,
            input_tx,
            Some(state.session.id.clone()),
        )
        .await;
        return;
    }

    // `/kms ingest <name> <file.pdf> --vision` intercept — the text layer is
    // garbled (e.g. a broken Thai ToUnicode font that swaps า/ำ), which no
    // text post-processing can repair. Route through the agent: PdfRead with
    // `vision` renders the glyphs, the model transcribes, KmsWrite stores it.
    if let Some(crate::repl::SlashCommand::KmsIngestPdf {
        name,
        file,
        alias,
        vision,
        ..
    }) = crate::repl::parse_slash(trimmed)
    {
        if vision {
            if crate::kms::resolve(&name).is_none() {
                let _ = events_tx.send(ViewEvent::SlashOutput(format!("no KMS named '{name}'")));
                let _ = events_tx.send(ViewEvent::TurnDone);
                return;
            }
            // KmsWrite registers only when a KMS is attached (kms_active).
            if state.config.kms_active.is_empty() {
                let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                    "/kms ingest {name} --vision: no KMS attached to this session. Run `/kms use {name}` first."
                )));
                let _ = events_tx.send(ViewEvent::TurnDone);
                return;
            }
            let page = alias.unwrap_or_else(|| {
                std::path::Path::new(&file)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("pdf-page")
                    .to_string()
            });
            let abs = {
                let p = std::path::PathBuf::from(&file);
                if p.is_absolute() {
                    p
                } else {
                    state.cwd.join(&p)
                }
            };
            let rewritten = crate::repl::build_kms_ingest_pdf_vision_prompt(
                &name,
                &page,
                &abs.to_string_lossy(),
            );
            let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                "(/kms ingest {name} {file} --vision → page `{page}` — render + transcribe via PdfRead vision)"
            )));
            let stream = Box::pin(state.agent.run_turn(rewritten));
            let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
            let _ = lead_mb.write_status("lead", "working", None);
            drive_turn_stream(
                stream,
                state,
                events_tx,
                cancel,
                &lead_mb,
                input_tx,
                Some(state.session.id.clone()),
            )
            .await;
            return;
        }
    }

    // `/kms dump <name> <text>` intercept — rewrite into a routing
    // prompt and run as a normal agent turn so KmsWrite/KmsAppend tools
    // execute against the live registry.
    if let Some(crate::repl::SlashCommand::KmsDump { name, text }) =
        crate::repl::parse_slash(trimmed)
    {
        if crate::kms::resolve(&name).is_none() {
            let _ = events_tx.send(ViewEvent::SlashOutput(format!("no KMS named '{name}'")));
            let _ = events_tx.send(ViewEvent::TurnDone);
            return;
        }
        // KMS tools register only when kms_active is non-empty. The
        // dump prompt instructs the agent to use KmsWrite/KmsAppend —
        // without active KMSes those tools aren't in the registry.
        if state.config.kms_active.is_empty() {
            let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                "/kms dump {name}: no KMS attached to this session. Run `/kms use {name}` first."
            )));
            let _ = events_tx.send(ViewEvent::TurnDone);
            return;
        }
        let rewritten = crate::repl::build_kms_dump_prompt(&name, &text);
        let _ = events_tx.send(ViewEvent::SlashOutput(format!(
            "(/kms dump {name} → routing {} char(s))",
            text.len()
        )));
        let stream = Box::pin(state.agent.run_turn(rewritten));
        let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
        let _ = lead_mb.write_status("lead", "working", None);
        drive_turn_stream(
            stream,
            state,
            events_tx,
            cancel,
            &lead_mb,
            input_tx,
            Some(state.session.id.clone()),
        )
        .await;
        return;
    }

    // `/kms ingest <name> <file>` intercept — do the deterministic ingest
    // (copy source, localize images, stub page), then fire an agent turn
    // that upgrades the stub into a curated page (summary + takeaways +
    // wikilinks) — the same auto-summary the Files-tab "Add to KMS" gets.
    if let Some(crate::repl::SlashCommand::KmsIngest {
        name,
        file,
        alias,
        force,
    }) = crate::repl::parse_slash(trimmed)
    {
        let Some(k) = crate::kms::resolve(&name) else {
            let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                "no KMS named '{name}' (try /kms list or /kms new {name})"
            )));
            let _ = events_tx.send(ViewEvent::TurnDone);
            return;
        };
        let source = {
            let p = std::path::PathBuf::from(&file);
            if p.is_absolute() {
                p
            } else {
                state.cwd.join(&p)
            }
        };
        match crate::kms::ingest(&k, &source, alias.as_deref(), force) {
            Ok(r) => {
                let images = if r.images_copied > 0 {
                    format!(" (+{} local image(s))", r.images_copied)
                } else {
                    String::new()
                };
                let verb = if r.overwrote { "replaced" } else { "ingested" };
                let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                    "{verb} → {} — {}{images} · summarizing…",
                    r.target.display(),
                    r.summary
                )));
                let src = k.root.join("sources").join(format!("{}.md", r.alias));
                let rewritten = crate::repl::build_kms_summarize_prompt(
                    &name,
                    &r.alias,
                    &src.to_string_lossy(),
                );
                let stream = Box::pin(state.agent.run_turn(rewritten));
                let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
                let _ = lead_mb.write_status("lead", "working", None);
                drive_turn_stream(
                    stream,
                    state,
                    events_tx,
                    cancel,
                    &lead_mb,
                    input_tx,
                    Some(state.session.id.clone()),
                )
                .await;
            }
            Err(e) => {
                let _ = events_tx.send(ViewEvent::SlashOutput(format!("ingest failed: {e}")));
                let _ = events_tx.send(ViewEvent::TurnDone);
            }
        }
        return;
    }

    // `/kms html <name> [<output-dir>]` intercept — same agent-loop
    // rewrite path. Agent reads the KMS via tools and writes the
    // result via the regular `Write` tool to a workspace directory
    // (default `./<name>-site/`).
    if let Some(crate::repl::SlashCommand::KmsHtml { name, output_dir }) =
        crate::repl::parse_slash(trimmed)
    {
        if crate::kms::resolve(&name).is_none() {
            let _ = events_tx.send(ViewEvent::SlashOutput(format!("no KMS named '{name}'")));
            let _ = events_tx.send(ViewEvent::TurnDone);
            return;
        }
        if state.config.kms_active.is_empty() {
            let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                "/kms html {name}: no KMS attached to this session. Run `/kms use {name}` first."
            )));
            let _ = events_tx.send(ViewEvent::TurnDone);
            return;
        }
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let outdir = match output_dir.as_deref() {
            Some(p) if std::path::Path::new(p).is_absolute() => std::path::PathBuf::from(p),
            Some(p) => cwd.join(p),
            None => cwd.join(format!("{name}-site")),
        };
        let outdir_str = outdir.to_string_lossy().to_string();
        let rewritten = crate::repl::build_kms_html_prompt(&name, &outdir_str);
        let _ = events_tx.send(ViewEvent::SlashOutput(format!(
            "(/kms html {name} → workspace site at {outdir_str})"
        )));
        let stream = Box::pin(state.agent.run_turn(rewritten));
        let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
        let _ = lead_mb.write_status("lead", "working", None);
        drive_turn_stream(
            stream,
            state,
            events_tx,
            cancel,
            &lead_mb,
            input_tx,
            Some(state.session.id.clone()),
        )
        .await;
        return;
    }

    // `/kms challenge <name> <idea>` intercept — same agent-loop rewrite
    // path as KmsDump, but read-only (search + analysis, no writes).
    if let Some(crate::repl::SlashCommand::KmsChallenge { name, idea }) =
        crate::repl::parse_slash(trimmed)
    {
        if crate::kms::resolve(&name).is_none() {
            let _ = events_tx.send(ViewEvent::SlashOutput(format!("no KMS named '{name}'")));
            let _ = events_tx.send(ViewEvent::TurnDone);
            return;
        }
        // KMS tools register only when kms_active is non-empty. The
        // challenge prompt instructs the agent to use KmsSearch/KmsRead.
        if state.config.kms_active.is_empty() {
            let _ = events_tx.send(ViewEvent::SlashOutput(format!(
                "/kms challenge {name}: no KMS attached to this session. Run `/kms use {name}` first."
            )));
            let _ = events_tx.send(ViewEvent::TurnDone);
            return;
        }
        let rewritten = crate::repl::build_kms_challenge_prompt(&name, &idea);
        let _ = events_tx.send(ViewEvent::SlashOutput(format!(
            "(/kms challenge {name} → red-team {} char(s))",
            idea.len()
        )));
        let stream = Box::pin(state.agent.run_turn(rewritten));
        let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
        let _ = lead_mb.write_status("lead", "working", None);
        drive_turn_stream(
            stream,
            state,
            events_tx,
            cancel,
            &lead_mb,
            input_tx,
            Some(state.session.id.clone()),
        )
        .await;
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

            // (1) Skill lookup. `state.skill_store` is snapshotted at
            // worker spawn (run_worker line 1144) and refreshed on
            // cwd-change + `/skill install`. But cloud workspaces can
            // install agents into a RUNNING engine via the API's
            // install_agent path (kubectl exec untar), which writes
            // .thclaws/skills/ without touching cwd — the snapshot
            // goes stale and `/foo` dispatch incorrectly returns
            // "unknown command". On cache miss, do one fresh
            // discover-and-retry (cheap: ~5ms FS walk) so install +
            // immediate invocation works without a pod restart. The
            // refreshed snapshot is saved back into state so the next
            // popup / autocomplete sees the same view.
            let mut skill_present = state
                .skill_store
                .lock()
                .ok()
                .map(|s| s.skills.contains_key(&word))
                .unwrap_or(false);
            if !skill_present {
                if let Ok(mut store) = state.skill_store.lock() {
                    *store = crate::skills::SkillStore::discover();
                    skill_present = store.skills.contains_key(&word);
                }
            }
            if skill_present {
                // Shared rewrite-text helper (`repl::make_skill_rewrite_prompt`)
                // so CLI and GUI / --serve send byte-identical
                // instructions to the model. Pre-extract, two parallel
                // copies of this format string could drift silently.
                let rewritten = crate::repl::make_skill_rewrite_prompt(&word, args);
                emit_skill_resolution_hint(events_tx, &word);
                let stream = Box::pin(state.agent.run_turn(rewritten));
                let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
                let _ = lead_mb.write_status("lead", "working", None);
                drive_turn_stream(
                    stream,
                    state,
                    events_tx,
                    cancel,
                    &lead_mb,
                    input_tx,
                    Some(state.session.id.clone()),
                )
                .await;
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
                drive_turn_stream(
                    stream,
                    state,
                    events_tx,
                    cancel,
                    &lead_mb,
                    input_tx,
                    Some(state.session.id.clone()),
                )
                .await;
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
    drive_turn_stream(
        stream,
        state,
        events_tx,
        cancel,
        &lead_mb,
        input_tx,
        Some(state.session.id.clone()),
    )
    .await;
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
    drive_turn_stream(
        stream,
        state,
        events_tx,
        cancel,
        &lead_mb,
        input_tx,
        Some(state.session.id.clone()),
    )
    .await;
}

/// Drive an agent run_turn stream to completion, emitting ViewEvents
/// to both the chat and terminal tabs. Extracted so handle_line and
/// handle_line_with_images share the streaming loop unchanged.
/// Regex matching the `[i/N] subject — done|cached|failed` lines the
/// image-generator / brand-presentation / research agent shells emit.
/// Compiled once on first use; matches are cheap (~µs each).
static PROGRESS_LINE_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"\[(\d+)/(\d+)\]\s+[^—\n]+?\s*—\s*(?:done|cached|failed[^\n]*)")
        .expect("PROGRESS_LINE_RE compiles")
});

/// Pull a human-readable `&str` out of a `Box<dyn Any + Send>` panic
/// payload. Falls back to a generic string for non-string payloads.
/// Used by `drive_turn_stream`'s `catch_unwind` arm so the user sees
/// why the turn died (and the lead-log records it for the post-mortem).
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "internal error: agent turn panicked".to_string()
    }
}

/// Public turn driver — wraps `drive_turn_stream_inner` in
/// `catch_unwind` so a panic mid-stream still flushes the session
/// (otherwise `save_history` in the `Done` arm never runs and the
/// in-progress turn — sometimes the whole session — disappears on
/// restart, per issue #148).
///
/// On panic: log the cause, surface it to the user, flush the session
/// JSONL, refresh the sidebar, clear the busy spinner, release the lead
/// agent, then re-raise so the process is still free to unwind cleanly.
/// The inner helper's RAII guards (`_busy`, `_broadcast_on_drop`) drop
/// normally on the unwind path — `catch_unwind` returns normally from
/// the future's perspective.
async fn drive_turn_stream(
    stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<AgentEvent, crate::error::Error>> + Send>,
    >,
    state: &mut WorkerState,
    events_tx: &broadcast::Sender<ViewEvent>,
    cancel: &crate::cancel::CancelToken,
    lead_mb: &crate::team::Mailbox,
    input_tx: &mpsc::Sender<ShellInput>,
    surface_session: Option<String>,
) {
    // dev-plan/42: in a multiuser pod, run the whole turn under this
    // session's task-local working dir (`workspace-<id>/`) so every tool
    // — bash, write, kms, pdf/epub, workflow — resolves paths against the
    // user's own folder, never the process cwd / shared SANDBOX_ROOT.
    // dev-plan/45 A2: also under the member scope, so outbound gateway
    // calls carry `X-Thclaws-Member` for billing attribution.
    // Single-tenant leaves both scopes unset (process cwd, unchanged).
    let root = state.cwd.clone();
    let member = state
        .session_roots
        .as_ref()
        .and_then(|r| r.member_id.clone());
    let inner = drive_turn_stream_inner(
        stream,
        state,
        events_tx,
        cancel,
        lead_mb,
        input_tx,
        surface_session,
    );
    let turn_result = if crate::workdir::is_multiuser() {
        match member {
            Some(id) => {
                AssertUnwindSafe(crate::workdir::scope_workdir(
                    root,
                    crate::multi_tenant::scope_member(id, inner),
                ))
                .catch_unwind()
                .await
            }
            None => {
                AssertUnwindSafe(crate::workdir::scope_workdir(root, inner))
                    .catch_unwind()
                    .await
            }
        }
    } else {
        AssertUnwindSafe(inner).catch_unwind().await
    };

    if let Err(payload) = turn_result {
        let msg = panic_message(&payload);
        write_lead_log(
            &state.lead_log,
            &format!("\n\x1b[31m[panic]\x1b[0m {msg}\n"),
        );
        let _ = events_tx.send(ViewEvent::ErrorText(msg.clone()));
        save_history(&state.agent, &mut state.session, &state.session_store);
        let _ = events_tx.send(ViewEvent::SessionListRefresh(build_session_list(
            &state.session_store,
            &state.session.id,
        )));
        let _ = events_tx.send(ViewEvent::TurnDone);
        let _ = lead_mb.write_status("lead", "idle", Some(&msg));
        std::panic::resume_unwind(payload);
    }

    // After a turn that ran tools, push a fresh KMS snapshot so the sidebar
    // reflects any KMS an agent/workflow just created or wrote — e.g. the
    // research agent's synthesizer/fetcher write pages via `Write` into
    // `.thclaws/kms/**`, which no KMS-tool hook catches. `build_kms_update_payload`
    // re-scans disk, so plain-Write pages show up without a `/reload`. The native
    // `/research` job manager already pushes this on Done (see set_broadcaster
    // above); this covers the catalog-agent path that runs via WorkflowRun.
    if state.last_turn_made_tool_calls {
        let _ = events_tx.send(ViewEvent::KmsUpdate(
            crate::gui::build_kms_update_payload().to_string(),
        ));
    }
}

async fn drive_turn_stream_inner(
    mut stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<AgentEvent, crate::error::Error>> + Send>,
    >,
    state: &mut WorkerState,
    events_tx: &broadcast::Sender<ViewEvent>,
    cancel: &crate::cancel::CancelToken,
    lead_mb: &crate::team::Mailbox,
    input_tx: &mpsc::Sender<ShellInput>,
    surface_session: Option<String>,
) {
    // Process-wide busy counter — the cloud heartbeat (server.rs) uses
    // this so a closed-browser batch keeps pinging `/keepalive` and the
    // cloud reaper doesn't pause the pod mid-turn. RAII drop covers
    // every return path below (cancel, end-of-stream, panic unwind).
    //
    // Surface vs side-channel: user-facing turns (handle_line +
    // handle_line_with_images) pass `Some(session.id)` so the UI's
    // running chip + cloud dashboard pill point at the user's
    // session. Auto-learn ingest/reconcile pass `None` — they count
    // toward the heartbeat's busy signal but don't overwrite the
    // surface meta (the user keeps landing in their own session on
    // reconnect, not the autonomous background work).
    // Drop-broadcast pair. Field-declaration order matters: `_busy`
    // drops FIRST (decrements counter + clears meta) so by the time
    // `_broadcast_on_drop` fires, subscribers re-querying
    // `busy_meta()` see `None`. Side-channel turns construct an
    // empty `BroadcastOnDrop` so their guard drop is silent — the
    // counter changes (heartbeat sees it) but the UI doesn't blink.
    struct BroadcastOnDrop(Option<tokio::sync::broadcast::Sender<ViewEvent>>);
    impl Drop for BroadcastOnDrop {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(ViewEvent::BusyChanged);
            }
        }
    }
    struct BusyBroadcast {
        _busy: crate::agent_activity::BusyGuard,
        _broadcast_on_drop: BroadcastOnDrop,
    }
    let _busy = match surface_session {
        Some(id) => {
            let guard = crate::agent_activity::BusyGuard::for_session(id);
            let _ = events_tx.send(ViewEvent::BusyChanged);
            BusyBroadcast {
                _busy: guard,
                _broadcast_on_drop: BroadcastOnDrop(Some(events_tx.clone())),
            }
        }
        None => BusyBroadcast {
            _busy: crate::agent_activity::BusyGuard::for_side_channel(),
            _broadcast_on_drop: BroadcastOnDrop(None),
        },
    };

    // Wall-clock start so the per-turn usage footer (emitted on Done)
    // can report elapsed time, matching the CLI REPL's token line.
    let turn_start = std::time::Instant::now();

    // Rolling buffer for progress-line extraction. Bounded so the
    // regex doesn't scan unbounded text on long turns; we only care
    // about the LATEST `[i/N]` line, so a small window is enough.
    let mut progress_buf = String::with_capacity(1024);
    const PROGRESS_BUF_CAP: usize = 4096;

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
                // Cheap progress-line extraction for the UI chip /
                // dashboard pill. One regex pass per chunk; buffer
                // capped so long turns don't slow down. Only the
                // LATEST match is kept — the chip shows "what's
                // happening right now," not history.
                progress_buf.push_str(&s);
                if progress_buf.len() > PROGRESS_BUF_CAP {
                    // Issue #148: the naive `len() - cap/2` byte offset
                    // could land mid-codepoint when the model streams
                    // multi-byte UTF-8 (Thai / CJK / emoji), tripping
                    // `String::drain`'s `is_char_boundary(end)` assertion
                    // and panicking the whole turn (which then lost the
                    // session because `save_history` runs in the `Done`
                    // arm we never reached). `str::floor_char_boundary`
                    // (stable since 1.79) snaps the offset to the
                    // largest boundary ≤ target, so drain is always safe.
                    let target = progress_buf.len() - PROGRESS_BUF_CAP / 2;
                    let safe = progress_buf.floor_char_boundary(target.min(progress_buf.len()));
                    progress_buf.drain(..safe);
                }
                if let Some(m) = PROGRESS_LINE_RE.find_iter(&progress_buf).last() {
                    crate::agent_activity::update_progress(m.as_str());
                }
                let _ = events_tx.send(ViewEvent::AssistantTextDelta(s));
            }
            Ok(AgentEvent::Thinking(s)) => {
                let _ = events_tx.send(ViewEvent::AssistantThinkingDelta(s));
            }
            Ok(AgentEvent::UserMessageInjected { text }) => {
                // Surface the drained mid-turn user message as a
                // normal user-bubble event (issue #106). The
                // frontend's optimistic queued bubble matches by
                // content and flips its badge from "queued" to
                // "delivered" on this event.
                let _ = events_tx.send(ViewEvent::UserPrompt(text));
            }
            Ok(AgentEvent::ToolCallStart { name, input, .. }) => {
                state.last_turn_made_tool_calls = true;
                let label = crate::tool_display::tool_label(&name, &input);
                write_lead_log(
                    &state.lead_log,
                    &format!("\x1b[0m\n\x1b[90m[tool: {label}]\x1b[0m "),
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
                let tracker = crate::usage::UsageTracker::new(
                    state
                        .session_roots
                        .as_ref()
                        .map(|r| r.usage_dir.clone())
                        .unwrap_or_else(crate::usage::UsageTracker::default_path),
                );
                tracker.record(provider_name, &state.config.model, &usage);
                if let Ok(cwd) = std::env::current_dir() {
                    crate::usage::append_usage_ledger(
                        &cwd,
                        "main",
                        provider_name,
                        &state.config.model,
                        &usage,
                    );
                }

                // Cost accounting (GUI parity with the CLI REPL). Drain
                // any pending buddy resets first so a mid-turn Backspace
                // on the Cardputer takes effect before this turn's
                // contribution lands. Then accumulate, then push the new
                // total to the buddy if a bridge is attached.
                #[cfg(feature = "cost_bridge")]
                if let Some(ref mut bridge) = state.cost_bridge {
                    while bridge.rx_reset.try_recv().is_ok() {
                        state.session_cost_usd = 0.0;
                    }
                }
                let token_usage = crate::model_catalogue::TokenUsage {
                    prompt_tokens: usage.input_tokens,
                    completion_tokens: usage.output_tokens,
                    cached_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
                    cache_creation_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
                    reasoning_tokens: usage.reasoning_output_tokens.unwrap_or(0),
                };
                let catalogue = crate::model_catalogue::EffectiveCatalogue::load();
                if let Some(c) = catalogue.compute_cost_usd(&state.config.model, &token_usage) {
                    state.session_cost_usd += c;
                }
                #[cfg(feature = "cost_bridge")]
                if let Some(ref bridge) = state.cost_bridge {
                    let _ = bridge.tx_cost.send(state.session_cost_usd);
                }

                // If a skill applied a model override this turn, emit
                // a revert chat note so the user sees the active
                // model returning to their baseline. The agent itself
                // already cleared the override slot before yielding
                // Done; this only handles the user-visible signaling.
                if crate::skills_state::take_swap_active() {
                    let _ = events_tx.send(ViewEvent::SkillModelNote(format!(
                        "[model → {} (skill ended)]",
                        state.config.model
                    )));
                }

                save_history(&state.agent, &mut state.session, &state.session_store);
                maybe_warn_file_size(state, events_tx);
                let _ = events_tx.send(ViewEvent::SessionListRefresh(build_session_list(
                    &state.session_store,
                    &state.session.id,
                )));

                // Per-turn usage footer (GUI parity with the CLI REPL).
                let cache_info = match (
                    usage.cache_creation_input_tokens,
                    usage.cache_read_input_tokens,
                ) {
                    (Some(c), Some(r)) if c > 0 || r > 0 => format!(" · cache: +{c}w/{r}r"),
                    _ => String::new(),
                };
                let cost_str = if state.session_cost_usd > 0.0 {
                    format!(" · ${:.4} session", state.session_cost_usd)
                } else {
                    String::new()
                };
                let usage_line = format!(
                    "[tokens: {}in/{}out{} · {}{}]",
                    usage.input_tokens,
                    usage.output_tokens,
                    cache_info,
                    crate::tool_display::format_duration(turn_start.elapsed()),
                    cost_str
                );
                // Persist alongside the turn so reopening the session
                // shows what each turn cost, instead of a conversation
                // with every cost line stripped out.
                if let Some(store) = &state.session_store {
                    let path = store.path_for(&state.session.id);
                    let _ = state.session.append_turn_usage_to(&path, &usage_line);
                }
                let _ = events_tx.send(ViewEvent::TurnUsage(usage_line));

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
            Ok(AgentEvent::UserMessageInjected { text }) => {
                // Surface the drained mid-turn user message as a
                // normal user-bubble event (issue #106). The
                // frontend's optimistic queued bubble matches by
                // content and flips its badge from "queued" to
                // "delivered" on this event.
                let _ = events_tx.send(ViewEvent::UserPrompt(text));
            }
            Ok(AgentEvent::ToolCallStart { name, input, .. }) => {
                let label = crate::tool_display::tool_label(&name, &input);
                write_lead_log(
                    &state.lead_log,
                    &format!("\x1b[0m\n\x1b[90m[tool: {label}]\x1b[0m "),
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
                let tracker = crate::usage::UsageTracker::new(
                    state
                        .session_roots
                        .as_ref()
                        .map(|r| r.usage_dir.clone())
                        .unwrap_or_else(crate::usage::UsageTracker::default_path),
                );
                tracker.record(provider_name, &state.config.model, &usage);
                if let Ok(cwd) = std::env::current_dir() {
                    crate::usage::append_usage_ledger(
                        &cwd,
                        "main",
                        provider_name,
                        &state.config.model,
                        &usage,
                    );
                }
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

/// Translate the worker's `ViewEvent` into the chat envelope
/// shape the plan-10 browser SPA expects. Subset of all events —
/// only the ones that drive the user-visible chat (text deltas,
/// tool indicators, turn boundary, errors). Returns `None` for
/// events the browser doesn't render (PlanUpdate, KmsUpdate, etc.).
fn view_event_to_chat_envelope(ev: &ViewEvent) -> Option<serde_json::Value> {
    use serde_json::json;
    match ev {
        // The user's own message — so a connected surface (phone-home
        // /talk) mirrors what was typed locally. The LINE browser SPA has
        // no `user_message` case so it ignores this; the phone-home /talk
        // page renders it as a user bubble. (dev-plan/44 streaming)
        ViewEvent::UserPrompt(text) => Some(json!({
            "type": "user_message",
            "text": text,
        })),
        ViewEvent::AssistantTextDelta(text) => {
            // Apply the same ANSI + tool-narration strip the LINE
            // OA path uses, otherwise the model's echoed
            // `\x1b[2m🔧 [Bash]\x1b[0m` indicators leak into the
            // browser bubble verbatim. `clean_for_stream` is the
            // truncate-free variant suitable for per-chunk
            // streaming.
            let cleaned = crate::line::clean_for_stream(text);
            // Drop ONLY if the strip pipeline produced an exactly
            // empty string (e.g. chunk was nothing but a
            // tool-narration line that the filter ate whole).
            //
            // Earlier this condition was `cleaned.trim().is_empty()`,
            // which also dropped chunks that were pure whitespace
            // (single space, single newline). That broke streams
            // where Anthropic emits a standalone space/newline
            // token between two word tokens: the browser
            // accumulated "wordAwordB" because the separator
            // chunk was silently dropped. Surfaced in the same
            // May 2026 report as the ordering bug.
            if cleaned.is_empty() {
                return None;
            }
            Some(json!({ "type": "assistant_delta", "text": cleaned }))
        }
        ViewEvent::ToolCallStart { name, label, .. } => Some(json!({
            "type": "tool_call_start",
            "name": name,
            "label": label,
        })),
        ViewEvent::ToolCallResult { name, output, .. } => Some(json!({
            "type": "tool_call_result",
            "name": name,
            "output": output,
        })),
        ViewEvent::TurnDone => Some(json!({ "type": "turn_done" })),
        ViewEvent::ErrorText(text) => Some(json!({
            "type": "error",
            "text": crate::providers::humanize_provider_error(text),
        })),
        _ => None,
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
/// own auth). Delegates to the canonical `providers::kind_has_credentials`
/// so the shared worker, GUI, and CLI all agree (incl. file-based
/// ChatGptCodex auth).
fn kind_has_credentials(kind: crate::providers::ProviderKind) -> bool {
    crate::providers::kind_has_credentials(Some(kind))
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

    /// Serialises tests that mutate `HAL_API_KEY` so they don't race
    /// each other or any other test reading the env var in parallel.
    // Alias for the crate-wide env lock. These tests mutate
    // HAL_API_KEY / TAVILY_API_KEY / BRAVE_SEARCH_API_KEY, which the
    // prompt-builder reads via `services_prompt_section()` — racing
    // against the prompt test in repl::tests would flip the section
    // size mid-build. The HAL bullet is ~1700 chars, which is exactly
    // the size mismatch we saw before this was unified.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::kms::test_env_lock()
    }

    #[test]
    fn services_section_always_mentions_web_search() {
        // WebSearch is always-available (DuckDuckGo fallback) so the
        // services section should ALWAYS surface it, even when no
        // HAL/Tavily/Brave key is set. This is the new floor:
        // services section is no longer HAL-conditional — it's a
        // capability index that's never empty.
        let _g = env_lock();
        let prev_hal = std::env::var("HAL_API_KEY").ok();
        let prev_tav = std::env::var("TAVILY_API_KEY").ok();
        let prev_brv = std::env::var("BRAVE_SEARCH_API_KEY").ok();
        std::env::remove_var("HAL_API_KEY");
        std::env::remove_var("TAVILY_API_KEY");
        std::env::remove_var("BRAVE_SEARCH_API_KEY");
        let section = crate::prompts::services_prompt_section(false);
        assert!(
            section.contains("WebSearch"),
            "section must always mention WebSearch (DuckDuckGo fallback is always available): {section}"
        );
        assert!(
            section.contains("DuckDuckGo"),
            "should note the no-key fallback so the user understands what's running: {section}"
        );
        assert!(
            !section.contains("HAL Public API"),
            "should NOT mention HAL when key is absent: {section}"
        );
        match prev_hal {
            Some(p) => std::env::set_var("HAL_API_KEY", p),
            None => std::env::remove_var("HAL_API_KEY"),
        }
        match prev_tav {
            Some(p) => std::env::set_var("TAVILY_API_KEY", p),
            None => std::env::remove_var("TAVILY_API_KEY"),
        }
        match prev_brv {
            Some(p) => std::env::set_var("BRAVE_SEARCH_API_KEY", p),
            None => std::env::remove_var("BRAVE_SEARCH_API_KEY"),
        }
    }

    #[test]
    fn services_section_picks_tavily_when_key_set() {
        let _g = env_lock();
        let prev = std::env::var("TAVILY_API_KEY").ok();
        std::env::set_var("TAVILY_API_KEY", "test-key");
        let section = crate::prompts::services_prompt_section(false);
        assert!(
            section.contains("Tavily (best quality)"),
            "should highlight Tavily as active backend when key set: {section}"
        );
        match prev {
            Some(p) => std::env::set_var("TAVILY_API_KEY", p),
            None => std::env::remove_var("TAVILY_API_KEY"),
        }
    }

    #[test]
    fn services_section_mentions_hal_tools_when_key_set() {
        let _g = env_lock();
        let prev = std::env::var("HAL_API_KEY").ok();
        std::env::set_var("HAL_API_KEY", "test-key");
        let section = crate::prompts::services_prompt_section(false);
        assert!(
            section.contains("# External services"),
            "missing header: {section}"
        );
        assert!(
            section.contains("HAL Public API"),
            "missing HAL marker: {section}"
        );
        assert!(
            section.contains("WebScrape"),
            "must surface WebScrape so model knows it exists: {section}"
        );
        assert!(
            section.contains("YouTubeTranscript"),
            "must surface YouTubeTranscript: {section}"
        );
        assert!(
            section.contains("WebFetch"),
            "must explain WebFetch behavior so model picks the right tool: {section}"
        );
        // Combined-mode language — model needs to understand that
        // WebFetch returns two labelled sections, not one or the
        // other. Without this hint the model would be confused by
        // the dual-section output.
        assert!(
            section.contains("both") || section.contains("parallel"),
            "services section must surface the combined fetch behavior: {section}"
        );
        match prev {
            Some(p) => std::env::set_var("HAL_API_KEY", p),
            None => std::env::remove_var("HAL_API_KEY"),
        }
    }

    #[test]
    fn services_section_treats_blank_hal_key_as_unset() {
        let _g = env_lock();
        let prev = std::env::var("HAL_API_KEY").ok();
        std::env::set_var("HAL_API_KEY", "   ");
        let section = crate::prompts::services_prompt_section(false);
        // Section is no longer empty (WebSearch always mentioned),
        // but the HAL-specific bullet should NOT appear with a
        // whitespace-only key.
        assert!(
            !section.contains("HAL Public API"),
            "whitespace-only HAL key should not light up HAL bullet; got: {section}"
        );
        // WebSearch should still be there as the always-on floor.
        assert!(
            section.contains("WebSearch"),
            "WebSearch always-on bullet should remain regardless of HAL key state: {section}"
        );
        match prev {
            Some(p) => std::env::set_var("HAL_API_KEY", p),
            None => std::env::remove_var("HAL_API_KEY"),
        }
    }

    #[test]
    fn documents_section_lists_all_format_pairs() {
        // Documents section is unconditional so we don't need env
        // setup. Each format pair (Docx / Xlsx / Pptx / Pdf) must be
        // mentioned with both Create and Read variants — the model
        // looks for "DocxCreate" specifically when the user asks for
        // a .docx, and for "DocxRead" when given one to parse.
        let section = crate::prompts::documents_prompt_section();
        assert!(
            section.contains("# Document & spreadsheet generation"),
            "missing header: {section}"
        );
        for name in [
            "DocxCreate",
            "DocxRead",
            "XlsxCreate",
            "XlsxRead",
            "PptxCreate",
            "PptxRead",
            "PdfCreate",
            "PdfRead",
        ] {
            assert!(
                section.contains(name),
                "documents section must surface `{name}` so the model finds it without scanning the tools-param list: {section}"
            );
        }
        // The anti-pattern guard — explicit nudge away from calling
        // generic `Read` on these binaries.
        assert!(
            section.contains("Do NOT call generic `Read`"),
            "must warn against calling generic Read on binary doc formats: {section}"
        );
    }

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
        crate::prompts::append_skills_section(&mut out, &store, "full");
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
        crate::prompts::append_skills_section(&mut out, &store, "names-only");
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
        crate::prompts::append_skills_section(&mut out, &store, "discover-tool-only");
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
        crate::prompts::append_skills_section(&mut out, &store, "totally-bogus-strategy");
        // Should look like the full-strategy output.
        assert!(out.contains("**pdf**"));
        assert!(out.contains("Render PDFs"));
    }

    /// AskUserQuestion's tool_result IS what the user typed back —
    /// chat history must surface it as a user bubble so the answer
    /// stays paired with the question across reloads / forks /
    /// /clear-then-/load. Other tools' results stay hidden.
    #[test]
    fn display_messages_surface_ask_user_replies() {
        use crate::types::{ContentBlock, Role};
        let messages = vec![
            // Initial user prompt.
            crate::types::Message {
                role: Role::User,
                content: vec![ContentBlock::text("search for ai news and summarize")],
            },
            // Assistant calls Bash (raw tool output stays hidden) and
            // then AskUserQuestion (its result becomes a user bubble).
            crate::types::Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ToolUse {
                        id: "call_bash_1".into(),
                        name: "Bash".into(),
                        input: serde_json::json!({}),
                        thought_signature: None,
                    },
                    ContentBlock::ToolUse {
                        id: "call_ask_1".into(),
                        name: "AskUserQuestion".into(),
                        input: serde_json::json!({"question": "Reuters or HN?"}),
                        thought_signature: None,
                    },
                ],
            },
            // Tool results — Bash's gets dropped, AskUser's becomes
            // a user bubble.
            crate::types::Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "call_bash_1".into(),
                        content: "raw bash stdout 12345".to_string().into(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call_ask_1".into(),
                        content: "Try Hacker News".to_string().into(),
                        is_error: false,
                    },
                ],
            },
        ];

        let display = DisplayMessage::from_messages(&messages);
        // Expect: initial user prompt, then 2 tool indicators (Bash,
        // AskUserQuestion), then the AskUser reply as a user bubble.
        // The Bash tool_result content stays hidden.
        let kinds_and_content: Vec<(&str, &str)> = display
            .iter()
            .map(|d| (d.role.as_str(), d.content.as_str()))
            .collect();
        assert_eq!(
            kinds_and_content,
            vec![
                ("user", "search for ai news and summarize"),
                ("tool", "Bash"),
                ("tool", "AskUserQuestion"),
                ("user", "Try Hacker News"),
            ],
            "AskUser reply should surface; bash output should not"
        );
    }

    /// Tool results without a known parent tool_use are silently
    /// dropped (defensive: a malformed history shouldn't crash or
    /// leak random tool output as user bubbles).
    #[test]
    fn display_messages_drops_orphan_tool_results() {
        use crate::types::{ContentBlock, Role};
        let messages = vec![crate::types::Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "no_matching_tool_use".into(),
                content: "orphaned content".to_string().into(),
                is_error: false,
            }],
        }];
        let display = DisplayMessage::from_messages(&messages);
        assert!(display.is_empty(), "orphan tool_result must not render");
    }

    #[test]
    fn paths_equivalent_identical() {
        let p = std::env::temp_dir();
        assert!(paths_equivalent(&p, &p));
    }

    #[test]
    fn paths_equivalent_different_dirs_not_equal() {
        let a = std::env::temp_dir();
        let b = std::path::PathBuf::from("/");
        assert!(!paths_equivalent(&a, &b));
    }

    #[test]
    fn paths_equivalent_handles_trailing_slash() {
        // Two PathBufs that point at the same dir but have different
        // string forms (with and without trailing slash) should
        // compare equal via canonicalize.
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().to_path_buf();
        let b: std::path::PathBuf = format!("{}/", a.to_string_lossy()).into();
        assert!(paths_equivalent(&a, &b));
    }

    #[test]
    fn paths_equivalent_falls_back_to_strict_equality_on_missing() {
        // Non-existent paths can't canonicalize; helper falls back
        // to literal comparison so we don't panic.
        let a = std::path::PathBuf::from("/nope/does/not/exist/aaa");
        let b = std::path::PathBuf::from("/nope/does/not/exist/aaa");
        assert!(paths_equivalent(&a, &b));
        let c = std::path::PathBuf::from("/nope/does/not/exist/bbb");
        assert!(!paths_equivalent(&a, &c));
    }

    /// Empty AskUser replies (user submitted blank) shouldn't render
    /// a stray empty user bubble.
    #[test]
    fn display_messages_skips_empty_ask_user_replies() {
        use crate::types::{ContentBlock, Role};
        let messages = vec![
            crate::types::Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_ask".into(),
                    name: "AskUserQuestion".into(),
                    input: serde_json::json!({"question": "?"}),
                    thought_signature: None,
                }],
            },
            crate::types::Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_ask".into(),
                    content: "   \n  ".to_string().into(),
                    is_error: false,
                }],
            },
        ];
        let display = DisplayMessage::from_messages(&messages);
        // Should have just the AskUserQuestion tool indicator, no
        // user bubble for the whitespace-only reply.
        assert_eq!(display.len(), 1);
        assert_eq!(display[0].role, "tool");
        assert_eq!(display[0].content, "AskUserQuestion");
    }
    /// A reloaded conversation has to look like the live one: the
    /// model's reasoning comes back as a collapsed block, and each
    /// turn's cost footer lands under the turn it belongs to.
    #[test]
    fn from_session_restores_thinking_and_usage_footers() {
        use crate::types::{ContentBlock, Message, Role};
        let mut session = crate::session::Session::new("m", "/tmp");
        session.messages = vec![
            Message::user("first"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        content: "weighing it up".into(),
                        signature: None,
                    },
                    ContentBlock::Text { text: "one".into() },
                ],
            },
            Message::user("second"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "two".into() }],
            },
        ];
        session.turn_usage = vec![
            crate::session::TurnUsage {
                after: 2,
                text: "[tokens: 10in/2out · 1s]".into(),
            },
            crate::session::TurnUsage {
                after: 4,
                text: "[tokens: 20in/4out · 2s]".into(),
            },
        ];

        let display = DisplayMessage::from_session(&session);
        let shape: Vec<(&str, &str)> = display
            .iter()
            .map(|d| (d.role.as_str(), d.content.as_str()))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("user", "first"),
                ("assistant", "one"),
                ("system", "[tokens: 10in/2out · 1s]"),
                ("user", "second"),
                ("assistant", "two"),
                ("system", "[tokens: 20in/4out · 2s]"),
            ],
            "each footer belongs under its own turn"
        );
        assert_eq!(
            display[1].thinking.as_deref(),
            Some("weighing it up"),
            "reasoning survives the reload"
        );
        assert!(display[4].thinking.is_none());
    }

    /// A footer recorded past the end — history trimmed by a compaction
    /// after the fact — still renders rather than disappearing.
    #[test]
    fn from_session_keeps_footers_recorded_past_the_end() {
        let mut session = crate::session::Session::new("m", "/tmp");
        session.messages = vec![crate::types::Message::user("only")];
        session.turn_usage = vec![crate::session::TurnUsage {
            after: 9,
            text: "[tokens: 1in/1out · 0s]".into(),
        }];
        let display = DisplayMessage::from_session(&session);
        assert_eq!(display.len(), 2);
        assert_eq!(display[1].role, "system");
    }

    // Regression test for issue #148: `progress_buf.drain(..drain)` in
    // drive_turn_stream_inner panicked with `is_char_boundary` failure
    // when the byte offset fell inside a multi-byte UTF-8 codepoint
    // (Thai/CJK/emoji from the model). Fix uses `floor_char_boundary`
    // to snap the offset down to a safe boundary first. This test
    // exercises the exact drain pattern the production code uses.
    #[test]
    fn progress_buf_drain_handles_multibyte_text() {
        const PROGRESS_BUF_CAP: usize = 256;
        let mut progress_buf = String::with_capacity(1024);
        // 2000 crab emojis = 8000 bytes, all 4-byte UTF-8 — the
        // worst case for the old naive `len() - cap/2` math.
        progress_buf.push_str(&"🦀".repeat(2000));
        let original_chars = progress_buf.chars().count();

        // Mirror the production idiom (post-fix):
        if progress_buf.len() > PROGRESS_BUF_CAP {
            let target = progress_buf.len() - PROGRESS_BUF_CAP / 2;
            let safe = progress_buf.floor_char_boundary(target.min(progress_buf.len()));
            progress_buf.drain(..safe);
        }
        // After drain, len() must be well under the cap and every
        // remaining char must still be a complete codepoint.
        assert!(progress_buf.len() <= PROGRESS_BUF_CAP);
        assert!(progress_buf.chars().count() < original_chars);
        // `is_char_boundary` on the drain point is the exact
        // assertion Vec::drain uses internally — catches a future
        // regression to the un-snapped offset.
        assert!(progress_buf.is_char_boundary(progress_buf.len()));
    }

    #[test]
    fn floor_char_boundary_snap_is_safe_for_mid_codepoint_offset() {
        // 4-byte emoji repeated. Char boundaries are at 0, 4, 8, ... —
        // `floor_char_boundary` snaps to the largest boundary ≤ target.
        let s = "🦀".repeat(2000);

        // A target inside a codepoint must snap to the previous
        // boundary (byte 1 or 2 inside the first 4-byte emoji → 0).
        let mid_target = 1;
        let safe = s.floor_char_boundary(mid_target);
        assert!(s.is_char_boundary(safe));
        assert_eq!(safe, 0, "must snap to previous char boundary");
        assert!(safe < mid_target);

        // A target already on a boundary stays put.
        let on_boundary = 4000; // 1000th emoji, byte offset
        let safe = s.floor_char_boundary(on_boundary);
        assert!(s.is_char_boundary(safe));
        assert_eq!(safe, on_boundary);

        // Target past end clamps to len() (still a valid boundary).
        let past_end = s.len() + 100;
        let safe = s.floor_char_boundary(past_end);
        assert_eq!(safe, s.len());
    }
}
