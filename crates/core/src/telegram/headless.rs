//! Headless Telegram bot (`thclaws --telegram`) — dev-plan/29 Tier 1
//! acceptance test #9: run the bridge with **no GUI feature**.
//!
//! The GUI path routes Telegram messages into the `shared_session`
//! worker, but that whole module is `#[cfg(feature = "gui")]`. So this
//! mode builds its own agent loop instead — the same construction
//! `repl::run_print_mode` uses (project context + memory + KMS system
//! prompt, builtin + KMS/memory tools, configured provider) — and drives
//! it directly from a [`TelegramMessageHandler`]. Turns are serialised
//! through a lock so two inbound messages can't race the agent's shared
//! history.
//!
//! Pairing note: headless has no GUI to approve pairing codes, so set
//! `TELEGRAM_OWNER_ID=<your numeric id>` for instant access. Other users
//! still get a pairing code, but approving it requires the GUI (or
//! pre-listing them in `~/.config/thclaws/telegram.json`).
//!
//! Tier 1 limits (vs the GUI path): no MCP servers, no session
//! persistence (history is in-memory for the process lifetime), single
//! shared session across chats (pairing gates who reaches it).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;

use crate::agent::{Agent, AgentEvent};
use crate::agent_defs::AgentDefsConfig;
use crate::cancel::CancelToken;
use crate::config::AppConfig;
use crate::context::ProjectContext;
use crate::error::Result;
use crate::memory::MemoryStore;
use crate::permissions::{ApprovalSink, PermissionMode};
use crate::subagent::{AgentFactory, ProductionAgentFactory};
use crate::tools::ToolRegistry;

use super::approver::TelegramApprover;
use super::client::{TelegramClient, TelegramClientError};
use super::config::{validate_token, TelegramConfig};
use super::pairing::PairingManager;
use super::session::{TelegramMessageHandler, TelegramSession};

/// Env var whose numeric value is auto-added to `allow_from` at startup
/// so the owner can DM the bot without the GUI pairing-approval step.
pub const OWNER_ID_ENV: &str = "TELEGRAM_OWNER_ID";

/// Drives in-process [`Agent`]s for inbound messages, capturing the
/// final assistant text. Tier 2: a forum-topic-routed `agent_id` selects
/// (and lazily builds, then caches) a per-AgentDef agent via the shared
/// [`ProductionAgentFactory`]; `None` uses the default agent. Turns are
/// serialised on `turn_lock` because each agent's history is shared
/// mutable state and they share one Telegram client + approver.
struct HeadlessAgentHandler {
    default_agent: Arc<Agent>,
    factory: Arc<ProductionAgentFactory>,
    agent_defs: AgentDefsConfig,
    /// agent_id → built agent, reused across turns so each topic-agent
    /// keeps its own conversation history.
    cache: tokio::sync::Mutex<HashMap<String, Arc<Agent>>>,
    turn_lock: tokio::sync::Mutex<()>,
}

impl HeadlessAgentHandler {
    /// Resolve the agent for a routed `agent_id`, building it from its
    /// AgentDef on first use. Falls back to the default agent when the
    /// id is absent, unknown, or fails to build.
    async fn agent_for(&self, agent_id: Option<String>) -> Arc<Agent> {
        let Some(id) = agent_id else {
            return self.default_agent.clone();
        };
        if let Some(cached) = self.cache.lock().await.get(&id).cloned() {
            return cached;
        }
        let Some(def) = self.agent_defs.get(&id).cloned() else {
            eprintln!(
                "[telegram] routed agent '{id}' not found in .thclaws/agents/; using default"
            );
            return self.default_agent.clone();
        };
        match self.factory.build("", Some(&def), 0).await {
            Ok(agent) => {
                let arc = Arc::new(agent);
                self.cache.lock().await.insert(id, arc.clone());
                arc
            }
            Err(e) => {
                eprintln!("[telegram] failed to build agent '{id}': {e}; using default");
                self.default_agent.clone()
            }
        }
    }
}

#[async_trait]
impl TelegramMessageHandler for HeadlessAgentHandler {
    async fn handle_message(
        &self,
        text: String,
        agent_id: Option<String>,
        preview: Option<Arc<dyn super::stream::PreviewSink>>,
    ) -> Option<String> {
        let agent = self.agent_for(agent_id).await;
        let _turn = self.turn_lock.lock().await;
        // Issue #164: a provider error (e.g. HTTP 400 when the history
        // grows too large or gets structurally poisoned) keeps replaying
        // every turn, with no way to recover from Telegram — only a
        // process restart. Let the user wipe this agent's conversation
        // with `/new` (or `/reset` / `/clear`) so a stuck chat recovers
        // without VPS-console access. Held under turn_lock so it can't
        // race an in-flight turn.
        if is_reset_command(&text) {
            agent.clear_history();
            return Some("🧹 Conversation reset — fresh start.".into());
        }
        let mut stream = Box::pin(agent.run_turn(text));
        // Capture the FINAL assistant text — cleared on each tool call so
        // only post-last-tool narration survives (matches the GUI worker).
        // Tier 3.1: when a `preview` sink is present, feed it the running
        // text so it can stream a rate-limited in-place edit.
        let mut buf = String::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(AgentEvent::Text(s)) => {
                    buf.push_str(&s);
                    if let Some(p) = &preview {
                        p.update(&buf).await;
                    }
                }
                Ok(AgentEvent::ToolCallStart { .. }) => buf.clear(),
                Ok(AgentEvent::Done { .. }) => break,
                Err(e) => return Some(format!("⚠️ thClaws hit an error: {e}")),
                _ => {}
            }
        }
        Some(buf)
    }

    /// Photo turns (public issue #187). Same loop as `handle_message`,
    /// but the turn starts from a mixed Text + Image content vec —
    /// `run_turn_multipart`, exactly what the GUI paste path calls.
    async fn handle_message_with_images(
        &self,
        text: String,
        images: Vec<(String, String)>,
        agent_id: Option<String>,
        preview: Option<Arc<dyn super::stream::PreviewSink>>,
    ) -> Option<String> {
        use crate::types::{ContentBlock, ImageSource};

        let agent = self.agent_for(agent_id).await;
        let _turn = self.turn_lock.lock().await;

        let mut content: Vec<ContentBlock> = Vec::new();
        if !text.trim().is_empty() {
            content.push(ContentBlock::text(text));
        }
        for (media_type, data) in images {
            content.push(ContentBlock::Image {
                source: ImageSource::Base64 { media_type, data },
            });
        }
        if content.is_empty() {
            return None;
        }

        let mut stream = Box::pin(agent.run_turn_multipart(content));
        let mut buf = String::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(AgentEvent::Text(s)) => {
                    buf.push_str(&s);
                    if let Some(p) = &preview {
                        p.update(&buf).await;
                    }
                }
                Ok(AgentEvent::ToolCallStart { .. }) => buf.clear(),
                Ok(AgentEvent::Done { .. }) => break,
                Err(e) => return Some(format!("⚠️ thClaws hit an error: {e}")),
                _ => {}
            }
        }
        Some(buf)
    }
}

/// True when a Telegram message is a conversation-reset command
/// (issue #164): `/new`, `/reset`, or `/clear`. Tolerates the
/// group-mention suffix Telegram appends (`/reset@mybot`) and any
/// trailing text, but only matches the bare command as the first token
/// (so `/resethard` or `hello /reset` don't trigger it).
fn is_reset_command(text: &str) -> bool {
    let first = text.trim().split_whitespace().next().unwrap_or("");
    let cmd = first.split('@').next().unwrap_or(first);
    matches!(cmd, "/new" | "/reset" | "/clear")
}

/// Permission mode for the headless bot. An explicit `auto` (from
/// `--accept-all`, `--permission-mode auto`, or `settings.json`
/// `permissions:auto`) means run with NO approval prompts — the right
/// choice for an unattended bot. Everything else routes approvals to
/// the chat as inline buttons (`TelegramGated`). Issue #160: this used
/// to be hardcoded to TelegramGated, so `auto` was silently ignored.
fn resolve_perm_mode(permissions: &str) -> PermissionMode {
    if permissions.eq_ignore_ascii_case("auto") {
        PermissionMode::Auto
    } else {
        PermissionMode::TelegramGated
    }
}

/// Run the headless Telegram bot until Ctrl-C or a fatal error (bad
/// token). Blocks for the process lifetime.
pub async fn run(config: AppConfig) -> Result<()> {
    if let Some(why) = crate::bridge::policy_refusal("Telegram") {
        eprintln!("\x1b[31m[telegram] refused: {why}\x1b[0m");
        std::process::exit(1);
    }
    // 1. Resolve the Telegram config + token (env beats file).
    let mut tg_cfg = TelegramConfig::load().ok().flatten().unwrap_or_default();
    let Some(token) = tg_cfg.resolved_token() else {
        eprintln!(
            "\x1b[31m[telegram] no bot token. Set TELEGRAM_BOT_TOKEN, or run \
             `thclaws telegram pair` for setup help.\x1b[0m"
        );
        std::process::exit(1);
    };
    if let Err(e) = validate_token(&token) {
        eprintln!("\x1b[31m[telegram] {e}\x1b[0m");
        std::process::exit(1);
    }
    // Owner id → allowlist (headless can't approve pairing via the GUI).
    if let Ok(owner) = std::env::var(OWNER_ID_ENV) {
        let owner = owner.trim();
        match owner.parse::<i64>() {
            Ok(id) if tg_cfg.add_allowed_user(id) => {
                eprintln!("[telegram] owner {id} allowlisted (from {OWNER_ID_ENV})");
            }
            Ok(_) => {}
            Err(_) if owner.is_empty() => {}
            Err(_) => eprintln!(
                "\x1b[33m[telegram] {OWNER_ID_ENV}='{owner}' is not a numeric user id; ignoring\x1b[0m"
            ),
        }
    }

    // 2. Build the agent (mirrors repl::run_print_mode construction).
    let cwd = std::env::current_dir()?;
    let ctx = ProjectContext::discover(&cwd)?;
    let memory_store = MemoryStore::default_path().map(MemoryStore::new);
    let system_fallback = if config.system_prompt.is_empty() {
        crate::prompts::defaults::SYSTEM
    } else {
        config.system_prompt.as_str()
    };
    let base_prompt = crate::prompts::load("system", system_fallback);
    let mut system = ctx.build_system_prompt(&base_prompt);
    if let Some(store) = &memory_store {
        if let Some(sec) = store.system_prompt_section() {
            system.push_str("\n\n# Memory\n");
            system.push_str(&sec);
        }
    }
    let kms_section = crate::kms::system_prompt_section(&config.kms_active);
    if !kms_section.is_empty() {
        system.push_str("\n\n");
        system.push_str(&kms_section);
    }

    let mut tools = ToolRegistry::with_builtins();
    tools.register(Arc::new(crate::tools::KmsReadTool));
    tools.register(Arc::new(crate::tools::KmsSearchTool));
    tools.register(Arc::new(crate::tools::KmsWriteTool));
    tools.register(Arc::new(crate::tools::KmsWriteSourceTool));
    tools.register(Arc::new(crate::tools::KmsAppendTool));
    tools.register(Arc::new(crate::tools::KmsEditTool));
    tools.register(Arc::new(crate::tools::KmsDeleteTool));
    tools.register(Arc::new(crate::tools::KmsCreateTool));
    tools.register(Arc::new(crate::tools::MemoryReadTool));
    tools.register(Arc::new(crate::tools::MemoryWriteTool));
    tools.register(Arc::new(crate::tools::MemoryAppendTool));

    let provider = crate::repl::build_provider(&config)?;

    // 3. Telegram transport: one client shared by the poller, the
    //    approver (sends prompts), and the session sink (sends replies).
    let cancel = CancelToken::new();
    let client = Arc::new(TelegramClient::new(token).with_cancel(cancel.clone()));
    match client.get_me().await {
        Ok(me) => {
            let label = me
                .username
                .map(|u| format!("@{u}"))
                .unwrap_or_else(|| me.first_name.clone());
            eprintln!("[telegram] connected as {label} (id {})", me.id);
        }
        Err(e) => {
            eprintln!("\x1b[31m[telegram] token rejected by Telegram: {e}\x1b[0m");
            std::process::exit(1);
        }
    }
    let approver = Arc::new(TelegramApprover::new(client.clone()));
    let pairing = Arc::new(PairingManager::new());
    let shared_cfg = Arc::new(Mutex::new(tg_cfg));

    // 4. Agent with the Telegram approver + gated permission mode. Set
    //    the process-global mode too — the agent loop consults
    //    `current_mode()` at each tool gate.
    //
    //    Respect an explicit `auto`: when the operator chose auto via
    //    `--accept-all`, `--permission-mode auto`, or
    //    `settings.json::permissions:auto`, run with NO prompts. A
    //    headless bot on a small VPS can't pop a GUI to approve, and
    //    forcing TelegramGated regardless meant `auto` was silently
    //    ignored and every tool call still demanded an inline-button
    //    tap (issue #160). Otherwise default to TelegramGated so
    //    approvals route to the chat as buttons.
    let perm_mode = resolve_perm_mode(&config.permissions);
    crate::permissions::set_current_mode(perm_mode);

    // Tier 2: a ProductionAgentFactory + AgentDefs registry so a
    // forum-topic-routed `agentId` can spin up (and reuse) a per-AgentDef
    // agent. Clone the inputs the factory needs before they move into the
    // default agent below.
    // I2: match CLI/GUI — surface plugin-contributed agent defs and apply
    // settings.json built-in subagent model overrides. Plain `load()`
    // left headless surfaces blind to plugin agents and the
    // `*_subagent_model` overrides.
    let mut agent_defs = AgentDefsConfig::load_with_extra(&crate::plugins::plugin_agent_dirs());
    agent_defs.apply_builtin_subagent_overrides(&config);
    // Telegram headless doesn't currently mutate system/tools
    // mid-run — Arc is owned solely by the factory. If we add
    // mid-run mutators later, hoist this clone next to the worker
    // state the way GUI/CLI do.
    let factory_snapshot = Arc::new(std::sync::RwLock::new(crate::subagent::FactorySnapshot {
        system: system.clone(),
        tools: tools.clone(),
        model: config.model.clone(),
        provider: provider.clone(),
    }));
    let factory = Arc::new(ProductionAgentFactory {
        snapshot: factory_snapshot,
        max_iterations: config.max_iterations,
        max_depth: crate::subagent::DEFAULT_MAX_DEPTH,
        max_tokens: config.max_tokens,
        agent_defs: agent_defs.clone(),
        approver: approver.clone() as Arc<dyn ApprovalSink>,
        permission_mode: perm_mode,
        cancel: Some(cancel.clone()),
        hooks: None,
    });

    let default_agent = Agent::new(provider, tools, config.model.clone(), system)
        .with_max_iterations(config.max_iterations)
        .with_max_tokens(config.max_tokens)
        .with_permission_mode(perm_mode)
        .with_approver(approver.clone() as Arc<dyn ApprovalSink>);

    let handler: Arc<dyn TelegramMessageHandler> = Arc::new(HeadlessAgentHandler {
        default_agent: Arc::new(default_agent),
        factory,
        agent_defs,
        cache: tokio::sync::Mutex::new(HashMap::new()),
        turn_lock: tokio::sync::Mutex::new(()),
    });

    // 5. Ctrl-C → cancel the poll loop for a clean shutdown.
    let cancel_for_signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n[telegram] shutting down…");
            cancel_for_signal.cancel();
        }
    });

    eprintln!("[telegram] headless bot running — Ctrl-C to stop. Tool approvals appear in-chat.");
    if shared_cfg
        .lock()
        .map(|c| c.allow_from.is_empty())
        .unwrap_or(true)
    {
        eprintln!(
            "\x1b[33m[telegram] no allowlisted users yet — set {OWNER_ID_ENV}=<your id> for \
             instant access, or DM the bot to mint a pairing code (approve it from the GUI).\x1b[0m"
        );
    }

    let session = Arc::new(
        TelegramSession::new(client, handler, shared_cfg, pairing).with_approver(approver),
    );
    match session.run().await {
        // A clean Ctrl-C cancellation is success, not an error.
        Ok(()) | Err(TelegramClientError::Cancelled) => Ok(()),
        Err(e) => {
            eprintln!("\x1b[31m[telegram] session ended: {e}\x1b[0m");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_disables_prompts_else_telegram_gated() {
        // Issue #160: an explicit auto must NOT be overridden by the
        // headless bot's default approval-routing.
        assert_eq!(resolve_perm_mode("auto"), PermissionMode::Auto);
        assert_eq!(resolve_perm_mode("AUTO"), PermissionMode::Auto);
        assert_eq!(resolve_perm_mode("ask"), PermissionMode::TelegramGated);
        assert_eq!(resolve_perm_mode(""), PermissionMode::TelegramGated);
        assert_eq!(resolve_perm_mode("plan"), PermissionMode::TelegramGated);
    }

    #[test]
    fn reset_command_detection() {
        // Triggers (issue #164).
        assert!(is_reset_command("/new"));
        assert!(is_reset_command("/reset"));
        assert!(is_reset_command("/clear"));
        assert!(is_reset_command("  /reset  "));
        assert!(is_reset_command("/clear@mybot")); // group mention suffix
        assert!(is_reset_command("/reset please"));
        // Does NOT trigger.
        assert!(!is_reset_command("reset"));
        assert!(!is_reset_command("/resethard"));
        assert!(!is_reset_command("hello /reset"));
        assert!(!is_reset_command("/help"));
        assert!(!is_reset_command(""));
    }
}
