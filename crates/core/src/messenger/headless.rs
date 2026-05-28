//! Headless Messenger bridge (`thclaws --messenger`) — dev-plan/31
//! Tier 1, runnable with **no GUI feature**.
//!
//! The GUI path routes Messenger messages into the `shared_session`
//! worker (gui-gated). This mode builds its own agent loop instead —
//! the same construction `repl::run_print_mode` / the headless Telegram
//! bot use — and drives it from a [`MessengerMessageHandler`]. Turns
//! are serialised on a lock so two inbound messages can't race the
//! agent's shared history.
//!
//! Connection model: the desktop connects to the relay over the
//! WebSocket using a binding JWT persisted at
//! `~/.config/thclaws/messenger.json`. If none is present, the user
//! pairs first (the relay DMs a pairing code to whoever messages the
//! Page; redeem it via the GUI Messenger Connect modal — headless
//! pairing redemption is a follow-up). Tier 1 uses a single shared
//! session for the Page; the relay gates who reaches it.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;

use crate::agent::{Agent, AgentEvent};
use crate::cancel::CancelToken;
use crate::config::AppConfig;
use crate::context::ProjectContext;
use crate::error::Result;
use crate::memory::MemoryStore;
use crate::permissions::{ApprovalSink, PermissionMode};
use crate::tools::ToolRegistry;

use super::approver::MessengerApprover;
use super::client::{MessengerClient, MessengerClientError};
use super::config::MessengerConfig;
use super::session::{MessengerMessageHandler, MessengerSession};

/// Drives one in-process [`Agent`] for inbound messages, capturing the
/// final assistant text. Turns are serialised on `turn_lock` because
/// the agent's history is shared mutable state.
struct HeadlessAgentHandler {
    agent: Arc<Agent>,
    turn_lock: tokio::sync::Mutex<()>,
}

#[async_trait]
impl MessengerMessageHandler for HeadlessAgentHandler {
    async fn handle_message(&self, text: String) -> Option<String> {
        let _turn = self.turn_lock.lock().await;
        let mut stream = Box::pin(self.agent.run_turn(text));
        // Capture the FINAL assistant text — cleared on each tool call
        // so only post-last-tool narration survives (matches the GUI
        // worker + headless Telegram).
        let mut buf = String::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(AgentEvent::Text(s)) => buf.push_str(&s),
                Ok(AgentEvent::ToolCallStart { .. }) => buf.clear(),
                Ok(AgentEvent::Done { .. }) => break,
                Err(e) => return Some(format!("⚠️ thClaws hit an error: {e}")),
                _ => {}
            }
        }
        Some(buf)
    }
}

/// Run the headless Messenger bridge until Ctrl-C or a fatal error.
/// Blocks for the process lifetime.
pub async fn run(config: AppConfig) -> Result<()> {
    // 1. Resolve the binding config (relay URL + binding JWT).
    let Some(msgr_cfg) = MessengerConfig::load().ok().flatten() else {
        eprintln!(
            "\x1b[31m[messenger] not configured. Pair your Facebook Page first \
             (run `thclaws messenger pair` for setup help), then retry.\x1b[0m"
        );
        std::process::exit(1);
    };
    if msgr_cfg.binding_token.trim().is_empty() {
        eprintln!(
            "\x1b[31m[messenger] no binding token in ~/.config/thclaws/messenger.json. \
             Pair via the GUI Messenger Connect modal first.\x1b[0m"
        );
        std::process::exit(1);
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
    tools.register(Arc::new(crate::tools::KmsAppendTool));
    tools.register(Arc::new(crate::tools::KmsDeleteTool));
    tools.register(Arc::new(crate::tools::KmsCreateTool));
    tools.register(Arc::new(crate::tools::MemoryReadTool));
    tools.register(Arc::new(crate::tools::MemoryWriteTool));
    tools.register(Arc::new(crate::tools::MemoryAppendTool));

    let provider = crate::repl::build_provider(&config)?;

    // 3. Transport: one client shared by the WS pump, the approver
    //    (pushes prompts), and the session sink (sends replies).
    let cancel = CancelToken::new();
    let client = Arc::new(MessengerClient::new(msgr_cfg.clone()).with_cancel(cancel.clone()));
    let approver = Arc::new(MessengerApprover::new(client.clone()));

    // 4. Agent gated by the Messenger approver. Set the process-global
    //    mode too — the agent loop consults `current_mode()` at each gate.
    crate::permissions::set_current_mode(PermissionMode::MessengerGated);
    let agent = Agent::new(provider, tools, config.model.clone(), system)
        .with_max_iterations(config.max_iterations)
        .with_max_tokens(config.max_tokens)
        .with_permission_mode(PermissionMode::MessengerGated)
        .with_approver(approver.clone() as Arc<dyn ApprovalSink>);

    let handler: Arc<dyn MessengerMessageHandler> = Arc::new(HeadlessAgentHandler {
        agent: Arc::new(agent),
        turn_lock: tokio::sync::Mutex::new(()),
    });

    // 5. Ctrl-C → cancel the WS loop for a clean shutdown.
    let cancel_for_signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n[messenger] shutting down…");
            cancel_for_signal.cancel();
        }
    });

    eprintln!(
        "[messenger] headless bridge running ({}) — Ctrl-C to stop. \
         Tool approvals appear in-chat.",
        msgr_cfg.resolved_server_url()
    );

    let session = Arc::new(
        MessengerSession::new(msgr_cfg, handler)
            .with_approver(approver)
            .with_cancel(cancel),
    );
    match session.run().await {
        Ok(()) | Err(MessengerClientError::Cancelled) => Ok(()),
        Err(e) => {
            eprintln!("\x1b[31m[messenger] session ended: {e}\x1b[0m");
            std::process::exit(1);
        }
    }
}
