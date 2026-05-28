//! Boot a [`MessengerSession`] from a [`MessengerConfig`] and own its
//! lifetime for the GUI worker. Mirrors [`crate::line::bootstrap`]
//! (dev-plan/31) — Messenger is relay-based like LINE.
//!
//! `WorkerForwardHandler` routes each inbound Messenger message into the
//! worker's `ShellInput::MessengerMessage` channel; the worker drives
//! `Agent::run_turn`, captures the final assistant text, and answers via
//! a `oneshot::Sender`, which this handler returns so the session sink
//! chunks + sends the reply through the relay's Send API.
//!
//! [`MessengerSessionHandle`] is what the worker stashes — the cancel
//! token (for disconnect), a status snapshot, and the shared approver /
//! client so the IPC layer resolves approvals against the same live
//! state.

use std::sync::mpsc;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::oneshot;

use super::approver::MessengerApprover;
use super::client::MessengerClient;
use super::config::MessengerConfig;
use super::session::{MessengerMessageHandler, MessengerSession};
use crate::cancel::CancelToken;

/// Forward Messenger text to the worker, wait for the captured agent
/// reply, return it. On worker-channel closure return a fallback so the
/// Messenger user sees *something* rather than dead silence.
struct WorkerForwardHandler {
    input_tx: mpsc::Sender<crate::shared_session::ShellInput>,
}

#[async_trait]
impl MessengerMessageHandler for WorkerForwardHandler {
    async fn handle_message(&self, text: String) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        if self
            .input_tx
            .send(crate::shared_session::ShellInput::MessengerMessage { text, respond: tx })
            .is_err()
        {
            return Some("⚠️ thClaws worker is unavailable; restart thClaws and try again.".into());
        }
        match rx.await {
            Ok(s) if !s.trim().is_empty() => Some(s),
            _ => Some("(thClaws agent finished the turn without a text reply.)".into()),
        }
    }
}

/// Live Messenger-bridge handle stored on the worker. Dropping it alone
/// won't stop the session — fire `cancel.cancel()` first (the IPC
/// `messenger_disconnect` arm does this).
pub struct MessengerSessionHandle {
    pub cancel: CancelToken,
    pub status: MessengerStatus,
    pub join: tokio::task::JoinHandle<()>,
    /// Shared approver — the agent's `ApprovalSink` swaps to this while
    /// Messenger is connected, so quick-reply taps resolve the same
    /// pending decisions the agent loop is awaiting.
    pub approver: Arc<MessengerApprover>,
    /// Shared client (used by the worker for the disconnect/unpair path).
    pub client: Arc<MessengerClient>,
}

/// Snapshot of the bridge state, serialised into the `messenger_status`
/// IPC payload for the GUI sidebar / connect modal.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MessengerStatus {
    pub state: &'static str,
    pub server_url: String,
    pub pending_approvals: usize,
}

impl MessengerStatus {
    pub fn disconnected() -> Self {
        Self {
            state: "disconnected",
            server_url: String::new(),
            pending_approvals: 0,
        }
    }

    pub fn connected(server_url: String) -> Self {
        Self {
            state: "connected",
            server_url,
            pending_approvals: 0,
        }
    }
}

/// Spawn a `MessengerSession` on the tokio runtime with the worker-
/// forwarding handler + `MessengerApprover`. Returns the handle so the
/// caller can stash it on `WorkerState` and cancel later.
pub fn spawn(
    config: MessengerConfig,
    input_tx: mpsc::Sender<crate::shared_session::ShellInput>,
) -> MessengerSessionHandle {
    let cancel = CancelToken::new();
    let server_url = config.resolved_server_url();
    let handler: Arc<dyn MessengerMessageHandler> = Arc::new(WorkerForwardHandler { input_tx });

    let client = Arc::new(MessengerClient::new(config.clone()).with_cancel(cancel.clone()));
    let approver = Arc::new(MessengerApprover::new(client.clone()));

    let session = Arc::new(
        MessengerSession::new(config, handler)
            .with_approver(approver.clone())
            .with_cancel(cancel.clone()),
    );
    let cancel_for_task = cancel.clone();
    let join = tokio::spawn(async move {
        if let Err(e) = session.run().await {
            eprintln!("[messenger] session ended: {e}");
        }
        cancel_for_task.cancel();
    });

    MessengerSessionHandle {
        cancel,
        status: MessengerStatus::connected(server_url),
        join,
        approver,
        client,
    }
}
