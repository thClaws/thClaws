//! HTTP + WebSocket server for `--serve` mode (M6.36).
//!
//! Serves the embedded React frontend over HTTP and bridges IPC over a
//! WebSocket to the same `SharedSessionHandle` engine the desktop GUI
//! uses. One process per project — `cd <project> && thclaws --serve
//! --port <N>` is the deployment unit.
//!
//! ## Routes
//!
//! - `GET /` — serves the frontend `index.html` (single-file vite
//!   build, embedded via `include_str!`)
//! - `GET /healthz` — `200 ok` liveness probe
//! - `GET /ws` — WebSocket upgrade. Inbound JSON frames route through
//!   [`crate::ipc::handle_ipc`] with a WS-flavored [`IpcContext`].
//!   Outbound event rendering (subscribing to `events_tx`, translating
//!   ViewEvents to chat/terminal-shaped JSON) lands in SERVE3.
//!
//! ## Trust model
//!
//! Single-user. Phase 1 binds to `127.0.0.1` only — operator runs an
//! SSH tunnel for remote access (no app-side auth). Anyone reaching
//! the bound socket has full access to the engine: BashTool runs as
//! the server user, file tools touch the server filesystem. Treat the
//! tunnel as the auth boundary.

use crate::config::AppConfig;
use crate::event_render::render_chat_dispatches;
use crate::ipc::{handle_ipc, IpcContext, PendingAsks};
use crate::providers::provider_has_credentials;
use crate::session::SessionStore;
use crate::shared_session::{SharedSessionHandle, ShellInput, ViewEvent};
use crate::uploads::{
    ensure_target_dir, ensure_uploads_dir, render_upload_message, unique_path, UploadedFile,
    UPLOADS_DIRNAME, UPLOAD_MAX_BYTES, UPLOAD_MAX_FILES,
};
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Multipart, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{any, get, post};
use axum::Router;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

/// The same single-file React build the desktop GUI embeds. Re-embedded
/// here under the always-on `crate::server` module so the frontend is
/// bundled regardless of the `gui` feature.
const FRONTEND_HTML: &str = include_str!("../../../frontend/dist/index.html");

#[derive(Clone)]
pub struct ServeConfig {
    pub bind: SocketAddr,
    /// Workspace root used for upload destinations and (future)
    /// sandbox scoping. `None` means "use process cwd at `run` time",
    /// which is the production default. Tests inject a tempdir to
    /// avoid touching global cwd.
    pub workspace: Option<std::path::PathBuf>,
    /// dev-plan/33 Tier 2 Mode B: bind a single GUI Shell as the
    /// served frontend. `None` → serve React (existing behaviour).
    /// `Some(id)` → mount the shell at `/t/<token>/` (or `/` if
    /// `gui_shell_no_auth`), 404 everything else.
    #[doc(alias = "gui-shell")]
    pub gui_shell: Option<ShellServeMode>,
    /// dev-plan/35 Tier 1: enable multi-tenant routing — pod accepts
    /// HMAC-signed user identity headers + spawns per-user sessions.
    /// `None` → single-tenant (today's behaviour).
    pub multi_tenant: Option<MultiTenantMode>,
}

/// dev-plan/35 Tier 1: multi-tenant `--serve` configuration. When
/// `Some`, the pod expects HMAC-signed X-Thclaws-User headers on
/// every WS upgrade and routes each request to a per-user
/// `SharedSessionHandle` from the [`UserSessionRegistry`]. When
/// `None`, --serve is single-tenant (today's behaviour).
#[derive(Clone)]
pub struct MultiTenantMode {
    /// Shared HMAC secret. Verifies X-Thclaws-User-Proof headers
    /// from the cloud routing layer.
    pub hmac_secret: Vec<u8>,
    /// LRU cap on concurrent resident sessions.
    pub max_users: usize,
    /// Idle-TTL for session eviction.
    pub idle_timeout: std::time::Duration,
    /// dev-plan/42: when `Some`, each user gets their own working
    /// directory `<workspaces_base>/workspace-<user_id>/` (the "a
    /// workspace per user" model). `None` keeps the dev-plan/35 layout
    /// (one shared cwd + per-user state subtrees).
    pub workspaces_base: Option<std::path::PathBuf>,
    /// dev-plan/42: read-only agent-def source seeded into each new
    /// per-user workspace (frozen snapshot). `None` → empty workspaces.
    pub def_source: Option<std::path::PathBuf>,
    /// dev-plan/42 Phase 5: the workspace owner's user id — their seeded
    /// def is writable (they author + publish); everyone else's is
    /// read-only. `None` → all read-only.
    pub owner_user_id: Option<String>,
}

impl std::fmt::Debug for MultiTenantMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never log the HMAC secret — accidental leak risk.
        f.debug_struct("MultiTenantMode")
            .field("hmac_secret", &"<redacted>")
            .field("max_users", &self.max_users)
            .field("idle_timeout", &self.idle_timeout)
            .field("workspaces_base", &self.workspaces_base)
            .field("def_source", &self.def_source)
            .field("owner_user_id", &self.owner_user_id)
            .finish()
    }
}

/// Bound-shell configuration for Mode B serve. Built from CLI flags +
/// `settings.json::guiShell.serveDefault` fallback.
#[derive(Debug, Clone)]
pub struct ShellServeMode {
    /// Shell id to bind (resolved against the registry at launch time).
    pub shell_id: String,
    /// Pinned token (from `--gui-shell-token`). When `None`, the token
    /// store generates / loads via `(shell_id, port)`.
    pub pinned_token: Option<String>,
    /// TTL for newly-generated tokens, parsed from
    /// `--gui-shell-token-ttl`. `None` = use the default (30d).
    pub token_ttl_secs: Option<u64>,
    /// `--gui-shell-no-auth` — skip the `/t/<token>/` prefix, mount
    /// at `/`. Refuses non-loopback binds without
    /// `no_auth_allow_public`.
    pub no_auth: bool,
    /// `--gui-shell-no-auth-allow-public` — override the loopback
    /// guard on `no_auth` for trusted reverse-proxy setups.
    pub no_auth_allow_public: bool,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            // Localhost-only by default — Phase 1 trust model is "SSH
            // tunnel handles auth". Override via --bind if you know
            // what you're doing.
            bind: ([127, 0, 0, 1], 8443).into(),
            workspace: None,
            gui_shell: None,
            multi_tenant: None,
        }
    }
}

/// State shared across HTTP / WS handlers. The `SharedSessionHandle`
/// IS the engine — same Arc lives in every WS connection so multi-tab
/// browsers see the same conversation.
///
/// `ask_broadcast` carries `ask_user_question` JSON envelopes to every
/// connected WS client. Pre-fix the standalone `--serve` path never
/// wired `set_gui_ask_sender`, so the agent's `AskUserQuestion` tool
/// posted to a `None` sender and stalled the turn waiting for a
/// oneshot that was never created (issue #82). The forwarder spawned
/// in [`run`] reads from the global ask channel and pushes JSON
/// frames to this broadcast; [`handle_socket`] subscribes per
/// connection so every browser tab sees the question.
#[derive(Clone)]
struct ServeState {
    shared: Arc<SharedSessionHandle>,
    approver: Arc<crate::permissions::GuiApprover>,
    pending_asks: PendingAsks,
    ask_broadcast: broadcast::Sender<String>,
    workspace: Arc<std::path::PathBuf>,
    /// dev-plan/35 Tier 1: when `Some`, the WS handler verifies
    /// X-Thclaws-User HMAC headers and routes to a per-user
    /// session from this registry instead of using `shared`.
    /// `None` = single-tenant (use `shared`).
    multi_tenant: Option<MultiTenantState>,
    /// Count of active browser WS connections. Incremented on
    /// `handle_socket` entry, decremented on exit (via `WsGuard`).
    /// Read by the optional `cloud_heartbeat` task to decide whether
    /// to ping `last_active_at` on the thclaws.cloud control plane —
    /// see `spawn_cloud_heartbeat` for the full path. Zero connections
    /// means "no browser tab is currently watching this engine"; the
    /// cloud reaper is then free to pause the workspace after its
    /// idle-timeout window.
    ws_connections: Arc<AtomicUsize>,
    /// One-line notice surfaced to each connecting client when the
    /// configured permission mode had to be overridden (e.g. a multiuser
    /// pod where "ask" can't be routed per-user, so it falls back to
    /// auto-approve). `None` when nothing was overridden.
    permission_notice: Option<String>,
}

/// State derived from [`MultiTenantMode`] at server bootstrap. Held
/// inside [`ServeState`] when multi-tenant mode is enabled; absent
/// otherwise (single-tenant path unchanged).
#[derive(Clone)]
struct MultiTenantState {
    registry: crate::multi_tenant::UserSessionRegistry,
    verifier: Arc<crate::multi_tenant::IdentityVerifier>,
}

/// Spin up the server. Spawns the worker, builds the Axum router,
/// blocks until the listener returns (Ctrl-C / panic / shutdown).
pub async fn run(config: ServeConfig) -> crate::error::Result<()> {
    // Phase 8: `--serve` opens an HTTP port carrying the web UI and the
    // OpenAI-compatible API. An org that forbids it needs the refusal
    // here, before the bind, not a note in the docs.
    if !crate::policy::serve_allowed() {
        return Err(crate::error::Error::Tool(
            "--serve is disabled by org policy (policies.runtime.allow_serve = false)".into(),
        ));
    }
    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .map_err(|e| crate::error::Error::Tool(format!("bind {}: {e}", config.bind)))?;
    run_on(config, listener).await
}

/// Serve on a listener the caller already bound.
///
/// Exists so a caller can own the bind. Asking the OS for port 0, dropping the
/// listener, and letting `run` re-bind leaves a window where anything on the
/// machine can take that port — which is exactly what made the round-trip test
/// fail at random under a parallel suite. Holding the listener from allocation
/// through to serving closes it.
pub async fn run_on(
    config: ServeConfig,
    listener: tokio::net::TcpListener,
) -> crate::error::Result<()> {
    // dev-plan/33 Tier 2 Mode B: a shell lives INSIDE a project folder;
    // the project root is where the agent's context comes from.
    //
    //   gui-shell-test/image-gen/      ← project root (AGENTS.md, etc.)
    //     AGENTS.md
    //     .thclaws/settings.json
    //     .thclaws/gui-shell/my-bot/   ← shell asset folder
    //     output/                       ← agent-produced files
    //
    // Resolution by shell source:
    //   - Project (`./.thclaws/gui-shell/<id>/`) → already in project
    //     root; do nothing. The user launched from the project dir.
    //   - User (`~/.config/thclaws/gui-shell/<id>/`) → no external
    //     project; treat the shell folder itself as the project root.
    //   - Embedded built-in → materialise to ~/.cache/thclaws/gui-shell/
    //     <id>/ and treat the shadow as the project root.
    //
    // Either way, agent loaders (AGENTS.md, .thclaws/settings.json,
    // MCP, KMS, .env) end up looking at the right directory.
    if let Some(mode) = &config.gui_shell {
        let shell = crate::gui_shell::serve::resolve_bound_shell(&mode.shell_id)?;
        match shell.source() {
            crate::gui_shell::ShellSource::Project => {
                eprintln!(
                    "\x1b[36m[serve] gui-shell project root: {} (cwd)\x1b[0m",
                    std::env::current_dir().unwrap_or_default().display()
                );
            }
            crate::gui_shell::ShellSource::User | crate::gui_shell::ShellSource::Builtin => {
                let root = shell.ensure_shadow_root()?;
                std::env::set_current_dir(&root).map_err(|e| {
                    crate::error::Error::Tool(format!(
                        "gui-shell: cannot chdir to '{}': {e}",
                        root.display()
                    ))
                })?;
                // Re-init sandbox at the new root so file tools
                // operate on the shell folder; reload dotenv so a
                // `<shell>/.env` is picked up.
                crate::sandbox::Sandbox::init().map_err(|e| {
                    crate::error::Error::Tool(format!("sandbox re-init at shell root: {e}"))
                })?;
                crate::dotenv::load_dotenv();
                eprintln!(
                    "\x1b[36m[serve] gui-shell project root: {} (chdir'd)\x1b[0m",
                    root.display()
                );
            }
        }
    }

    let (approver, mut approval_rx) = crate::permissions::GuiApprover::new();
    let shared = Arc::new(crate::shared_session::spawn_with_approver(approver.clone()));
    // The frontend's "I'm ready" handshake unblocks deferred startup
    // (MCP spawn, etc.). Without a UI to wait on, signal immediately
    // so the worker doesn't sit waiting for a frontend that won't
    // appear until the first browser tab connects.
    shared.ready_gate.signal();
    let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));

    // AskUserQuestion bridge (issue #82). Mirrors gui.rs:541-543 +
    // 576-610. Pre-fix `set_gui_ask_sender` was never called in the
    // standalone serve path, so the tool's `GUI_ASK_SENDER` static
    // stayed `None` and `AskUserRequest` posts had nowhere to go —
    // the agent hung on its oneshot waiting for a response that
    // could never arrive. The forwarder below reads ask requests
    // from the global channel, stashes the oneshot responder in
    // `pending_asks` (so `ipc::handle_ipc`'s `ask_user_response`
    // arm can resolve it when the frontend replies), and broadcasts
    // the question JSON to every connected WS client via
    // `ask_broadcast`. Capacity 16 is generous — multiple in-flight
    // ask questions are rare, and lag is logged but tolerated.
    let (ask_tx, mut ask_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::tools::AskUserRequest>();
    crate::tools::set_gui_ask_sender(Some(ask_tx));
    let (ask_broadcast, _) = broadcast::channel::<String>(16);
    {
        let ask_broadcast_for_fwd = ask_broadcast.clone();
        let pending_asks_for_fwd = pending_asks.clone();
        tokio::spawn(async move {
            while let Some(req) = ask_rx.recv().await {
                let id = req.id;
                let question = req.question.clone();
                if let Ok(mut pending) = pending_asks_for_fwd.lock() {
                    pending.insert(id, req.response);
                }
                let payload = serde_json::json!({
                    "type": "ask_user_question",
                    "session_id": crate::agent_activity::busy_meta().map(|m| m.session_id),
                    "id": id,
                    "question": question,
                });
                // No-op when zero subscribers — early questions before
                // any tab connects are silently dropped (the agent
                // will still time out on its own retry path; can't
                // queue indefinitely without losing the oneshot to
                // GC).
                let _ = ask_broadcast_for_fwd.send(payload.to_string());
            }
        });
    }

    // Approval bridge — the fix for the serve hang. Pre-fix this rx was
    // dropped (`_approval_rx`), so when a tool required approval (any time
    // the permission mode is Ask) `GuiApprover::approve()` sent the request
    // into an unserviced channel and blocked on its oneshot FOREVER: a hung
    // turn with no file written. We now drain the channel and broadcast each
    // `approval_request` to every WS client; the frontend renders the modal
    // and the decision returns via `handle_ipc`'s `approval_response` arm,
    // which calls `approver.resolve(id, decision)`. Mirrors the AskUser
    // bridge above (the responder oneshots live inside the GuiApprover, so
    // no `pending_asks` bookkeeping is needed here).
    {
        let approval_broadcast = ask_broadcast.clone();
        tokio::spawn(async move {
            while let Some(req) = approval_rx.recv().await {
                let payload = serde_json::json!({
                    "type": "approval_request",
                    "session_id": req.session_id,
                    "id": req.id,
                    "tool_name": req.tool_name,
                    "input": req.input,
                    "summary": req.summary,
                    "originator": req.originator,
                });
                let _ = approval_broadcast.send(payload.to_string());
            }
        });
    }

    run_with_engine(
        config,
        approver,
        shared,
        pending_asks,
        ask_broadcast,
        Some(listener),
    )
    .await
}

/// Same as [`run`], but reuses an engine constructed by the caller. Used
/// by the `--serve --gui` combo path so the desktop window and any
/// browser tab share one Agent + Session — i.e. the same conversation
/// is visible from both surfaces.
pub async fn run_with_engine(
    config: ServeConfig,
    approver: Arc<crate::permissions::GuiApprover>,
    shared: Arc<SharedSessionHandle>,
    pending_asks: PendingAsks,
    ask_broadcast: broadcast::Sender<String>,
    // Pre-bound listener, or `None` to bind `config.bind` here. See
    // `run_on` for why a caller might want to own the bind.
    listener: Option<tokio::net::TcpListener>,
) -> crate::error::Result<()> {
    let workspace = match config.workspace.clone() {
        Some(p) => p,
        None => Ok::<_, std::io::Error>(crate::workdir::workspace_root())
            .map_err(|e| crate::error::Error::Tool(format!("workspace cwd unavailable: {e}")))?,
    };
    // dev-plan/42: flag the process as multiuser so global cwd/sandbox
    // mutators (IPC `set_cwd`) stay no-ops — per-session roots come from
    // the task-local working dir, not process-global state.
    let mut permission_notice: Option<String> = None;
    if config.multi_tenant.is_some() {
        crate::workdir::set_multiuser(true);
        // MULTIUSER approval safety net. One shared worker processes every
        // user's turns and ShellInput carries no user_id, so an approval
        // request can't be routed to the user who triggered it: broadcasting
        // it pod-wide would leak one user's prompt to another, and blocking
        // would hang. Auto-approve instead — multiuser isolates each user in
        // their own `workspace-<user_id>/` folder, so an auto-approved tool
        // is bounded to that user's own sandbox. `spawn_with_approver` set
        // the mode from config.permissions; override it to Auto here.
        let was_ask = crate::permissions::current_mode() == crate::permissions::PermissionMode::Ask;
        crate::permissions::set_current_mode(crate::permissions::PermissionMode::Auto);
        if was_ask {
            // The pod was configured for "ask" — surface why it isn't honored
            // (rare, but shouldn't be silent).
            permission_notice = Some(
                "“ask” permission mode isn't supported in shared (multiuser) workspaces — \
                 tool approvals can't be routed to an individual user, so thClaws uses \
                 auto-approve here. Your files stay isolated to your own workspace folder."
                    .to_string(),
            );
            eprintln!(
                "\x1b[33m[serve] multiuser: configured 'ask' overridden to auto-approve (not per-user routable)\x1b[0m"
            );
        } else {
            eprintln!(
                "\x1b[36m[serve] multiuser: auto-approve (folder-isolated; approvals aren't per-user routable)\x1b[0m"
            );
        }
    }
    // dev-plan/35 Tier 1: construct the multi-tenant registry +
    // background evictor when multi-tenant mode is configured.
    let multi_tenant_state = config.multi_tenant.as_ref().map(|cfg| {
        let registry =
            crate::multi_tenant::UserSessionRegistry::new(crate::multi_tenant::RegistryConfig {
                max_users: cfg.max_users,
                idle_timeout: cfg.idle_timeout,
                approver: approver.clone() as Arc<dyn crate::permissions::ApprovalSink>,
                // Per-user JSONLs / storage / usage will land under
                // <workspace>/.thclaws/users/<user_id>/... so a pod
                // restart preserves every user's session.
                project_root: workspace.clone(),
                // dev-plan/42: per-user working dirs + def seed source.
                workspaces_base: cfg.workspaces_base.clone(),
                def_source: cfg.def_source.clone(),
                owner_user_id: cfg.owner_user_id.clone(),
            });
        // Sweep every 30s — fine for 30m default idle_timeout, will
        // need re-tuning if Tier 3 wants sub-minute sessions.
        let _evictor = registry.spawn_evictor(std::time::Duration::from_secs(30));
        eprintln!(
            "\x1b[36m[serve] multi-tenant on — max_users={}, idle_timeout={:?}\x1b[0m",
            cfg.max_users, cfg.idle_timeout
        );
        // dev-plan/45 B: prefer the asymmetric verifier when the pod is
        // provisioned with THCLAWS_CLOUD_PUBKEY; a malformed pubkey is a
        // hard startup error (never silently downgrade to the forgeable
        // symmetric proof).
        let verifier = crate::multi_tenant::IdentityVerifier::from_secret_and_pubkey(
            &cfg.hmac_secret,
            std::env::var("THCLAWS_CLOUD_PUBKEY").ok().as_deref(),
        )
        .expect("THCLAWS_CLOUD_PUBKEY is set but not a valid 32-byte hex Ed25519 key");
        MultiTenantState {
            registry,
            verifier: Arc::new(verifier),
        }
    });
    let ws_connections = Arc::new(AtomicUsize::new(0));
    let _ = SERVE_WORKSPACE.set(workspace.clone()); // for /healthz background-job probe
    let state = ServeState {
        shared,
        approver,
        pending_asks,
        ask_broadcast,
        workspace: Arc::new(workspace),
        multi_tenant: multi_tenant_state,
        ws_connections: ws_connections.clone(),
        permission_notice,
    };
    // Grab the identity verifier BEFORE `state` moves into the router —
    // the multiuser auth layer below needs it.
    let mt_verifier = state.multi_tenant.as_ref().map(|m| m.verifier.clone());

    // Cloud heartbeat: when running inside a thclaws.cloud workspace
    // pod, periodically POST to `/api/hosted/workspaces/<id>/keepalive`
    // while at least one browser tab is connected. The provisioner
    // injects THCLAWS_CLOUD_URL / THCLAWS_CLOUD_TOKEN / THCLAWS_WORKSPACE_ID
    // at provision time. Outside cloud (any env var missing), this is
    // a no-op — local CLI / desktop GUI runs don't need it.
    // dev-plan/60 G2: an agent under a workspace host leaves the keepalive to
    // the host. Each agent sending its own meant the last one to report won —
    // an idle agent's `busy: false` could land while another was mid-turn.
    if std::env::var("THCLAWS_SUPERVISED").ok().as_deref() != Some("1") {
        spawn_cloud_heartbeat(ws_connections);
    }

    // Loopback-only safety check for the API auth-bypass token. The
    // bypass mode (`THCLAWS_API_TOKEN=disable-auth`) makes the OpenAI
    // endpoints reachable to anyone who can hit the socket — refuse to
    // start if the bind isn't loopback, so a misconfigured deploy fails
    // loud instead of silently exposing the agent runtime.
    if crate::api_v1::auth_is_bypassed() && !is_loopback(&config.bind) {
        return Err(crate::error::Error::Tool(format!(
            "THCLAWS_API_TOKEN=disable-auth is only allowed on a loopback bind, but server is bound to {}. \
             Set a real token or use --bind 127.0.0.1.",
            config.bind
        )));
    }

    // dev-plan/33 Tier 2 Mode B: when a shell is bound, swap the
    // React-frontend routes for the gui_shell::serve mount. The
    // OpenAI-compat /v1/* surface is preserved either way (api_v1
    // is merged unconditionally).
    let app = if let Some(mode) = config.gui_shell.clone() {
        build_shell_router(&config.bind, state, mode)?
    } else {
        classic_router(state)
    };

    // dev-plan/42: gate the ENTIRE surface behind HMAC identity in a
    // multiuser pod (both router shapes above), so no route is reachable
    // without a verified user. /healthz stays open for k8s probes.
    let app = if let Some(verifier) = mt_verifier {
        app.layer(axum::middleware::from_fn_with_state(
            verifier,
            multiuser_auth,
        ))
    } else {
        app
    };

    let listener = match listener {
        Some(l) => l,
        None => tokio::net::TcpListener::bind(&config.bind)
            .await
            .map_err(|e| crate::error::Error::Tool(format!("bind {}: {e}", config.bind)))?,
    };
    // The address actually bound, not the one asked for: with `--port 0` the
    // two differ, and the banner used to print `:0`.
    let bound = listener.local_addr().unwrap_or(config.bind);
    if config.gui_shell.is_none() {
        eprintln!("\x1b[36m[serve] thClaws listening on http://{bound}\x1b[0m");
        eprintln!("\x1b[36m[serve] open the URL above in your browser (over an SSH tunnel for remote access)\x1b[0m");
    }
    publish_bound_addr(bound);
    // dev-plan/59: a supervised child outlives a host that was killed
    // outright unless it watches for the closed pipe itself. It then leaves
    // through axum's graceful shutdown rather than `process::exit`, so this
    // function returns, the runtime unwinds, and the bot's own MCP children
    // — held with `kill_on_drop` — are reaped instead of orphaned.
    let supervised = std::env::var("THCLAWS_SUPERVISED").ok().as_deref() == Some("1");
    let stdin_closed = async move {
        if supervised {
            crate::bots::supervisor::stdin_closed().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(stdin_closed)
        .await
        .map_err(|e| crate::error::Error::Tool(format!("serve: {e}")))?;
    Ok(())
}

// ---- Workspace sync handlers (dev-plan/51) ----
// Tar/untar the workspace dir for `/cloud push|pull`. Reachable wherever the
// engine serves; auth is the serving layer's job (cloud ingress ForwardAuth for
// hosted runners — single- or multi-tenant — and api_v1/loopback for a local
// `--serve`), same as the adjacent /upload route.

#[derive(serde::Serialize)]
struct SyncStatResp {
    file_count: usize,
    bytes: u64,
    empty: bool,
    busy: bool,
    engine_version: &'static str,
    workspace_id: Option<String>,
    /// Last sync revision this runner agreed to (see `sync_revision`).
    revision: Option<u64>,
}

/// What the `/workspace/sync/*` handlers need: the directory they teleport,
/// and whether an agent is mid-turn in it. A plain serve answers the second
/// from its own counter; a host has to ask its agents, because the turns run
/// in their processes, not its own.
#[derive(Clone)]
struct SyncRoot {
    workspace: Arc<std::path::PathBuf>,
    host: Option<Arc<crate::bots::supervisor::BotSupervisor>>,
}

impl SyncRoot {
    async fn busy(&self) -> bool {
        match &self.host {
            Some(sup) => agents_busy(sup).await.0,
            None => crate::agent_activity::busy_count() > 0,
        }
    }
}

impl axum::extract::FromRef<ServeState> for SyncRoot {
    fn from_ref(state: &ServeState) -> Self {
        Self {
            workspace: state.workspace.clone(),
            host: None,
        }
    }
}

impl axum::extract::FromRef<Arc<crate::bots::supervisor::BotSupervisor>> for SyncRoot {
    fn from_ref(sup: &Arc<crate::bots::supervisor::BotSupervisor>) -> Self {
        Self {
            workspace: Arc::new(sup.workspace().to_path_buf()),
            host: Some(sup.clone()),
        }
    }
}

/// Workspace sync (dev-plan/51): /cloud push|pull against the workspace dir.
/// Mounted by the classic router and by a host, which teleports the whole
/// workspace — every agent in it — exactly as a plain serve teleports its one.
/// `THCLAWS_SYNC_REQUIRE_AUTH=1` opts the group into the /v1 bearer.
fn sync_routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    SyncRoot: axum::extract::FromRef<S>,
{
    Router::new()
        .route("/workspace/sync/stat", get(sync_stat))
        .route("/workspace/sync/pull", get(sync_pull))
        .route(
            "/workspace/sync/push",
            post(sync_push).layer(DefaultBodyLimit::max(
                crate::cloud::wssync::MAX_SYNC_BYTES as usize,
            )),
        )
        // P2 incremental: manifest diff + per-subset transfer/trash.
        .route("/workspace/sync/manifest", get(sync_manifest))
        .route(
            "/workspace/sync/export",
            post(sync_export).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route(
            "/workspace/sync/trash",
            post(sync_trash).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        // Agreed sync revision, recorded after a sync lands (both
        // directions) so the two ends name the same number.
        .route("/workspace/sync/revision", post(sync_revision))
        .route_layer(axum::middleware::from_fn(sync_bearer_gate))
}

async fn sync_stat(State(sync): State<SyncRoot>) -> Response {
    let busy = sync.busy().await;
    let root = sync.workspace.as_path();
    match crate::cloud::wssync::stat_workspace(root) {
        Ok(s) => {
            let binding = crate::cloud::wssync::read_binding(root);
            Json(SyncStatResp {
                file_count: s.file_count,
                bytes: s.bytes,
                empty: crate::cloud::wssync::is_empty(root).unwrap_or(false),
                busy: busy,
                engine_version: env!("CARGO_PKG_VERSION"),
                workspace_id: binding.workspace_id,
                revision: binding.revision,
            })
            .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct PullQuery {
    #[serde(default)]
    include_runtime: bool,
}

/// Streams a temp file as an `application/gzip` body, deleting it once the
/// response finishes sending (the `TempPath` rides along and drops with the
/// stream).
struct TempFileStream {
    inner: tokio_util::io::ReaderStream<tokio::fs::File>,
    _path: tempfile::TempPath,
}

impl futures::Stream for TempFileStream {
    type Item = std::io::Result<Bytes>;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_next(cx)
    }
}

async fn stream_tar_temp(tmp: tempfile::NamedTempFile) -> Response {
    let path = tmp.into_temp_path();
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("open temp: {e}")).into_response()
        }
    };
    let stream = TempFileStream {
        inner: tokio_util::io::ReaderStream::new(file),
        _path: path,
    };
    (
        [(header::CONTENT_TYPE, "application/gzip")],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn sync_pull(State(sync): State<SyncRoot>, Query(q): Query<PullQuery>) -> Response {
    let busy = sync.busy().await;
    if busy {
        return (StatusCode::CONFLICT, "workspace busy (active turn)").into_response();
    }
    let root = sync.workspace.as_path().to_path_buf();
    let include_runtime = q.include_runtime;
    let tmp = tokio::task::spawn_blocking(move || {
        let tmp = tempfile::NamedTempFile::new().map_err(|e| format!("temp: {e}"))?;
        crate::cloud::wssync::tar_workspace_to(&root, include_runtime, tmp.as_file())?;
        Ok::<_, String>(tmp)
    })
    .await;
    match tmp {
        Ok(Ok(tmp)) => stream_tar_temp(tmp).await,
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct PushQuery {
    #[serde(default)]
    delete: bool,
    workspace_id: Option<String>,
}

#[derive(serde::Serialize)]
struct PushResp {
    written: usize,
    deleted: usize,
    trashed: bool,
}

async fn sync_push(
    State(sync): State<SyncRoot>,
    Query(q): Query<PushQuery>,
    body: Body,
) -> Response {
    let busy = sync.busy().await;
    if busy {
        return (StatusCode::CONFLICT, "workspace busy (active turn)").into_response();
    }
    // Stream the upload to a temp file so a large tarball never rides in memory.
    let tmp = match tempfile::NamedTempFile::new() {
        Ok(t) => t,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("temp: {e}")).into_response(),
    };
    let write_handle = match tmp.reopen() {
        Ok(f) => f,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("temp: {e}")).into_response(),
    };
    let mut afile = tokio::fs::File::from_std(write_handle);
    let mut stream = body.into_data_stream();
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => return (StatusCode::BAD_REQUEST, format!("body: {e}")).into_response(),
        };
        if let Err(e) = afile.write_all(&chunk).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("write temp: {e}"),
            )
                .into_response();
        }
    }
    if let Err(e) = afile.flush().await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("flush temp: {e}"),
        )
            .into_response();
    }
    drop(afile);

    let root = sync.workspace.as_path().to_path_buf();
    let delete = q.delete;
    let path = tmp.into_temp_path();
    let res = tokio::task::spawn_blocking(move || {
        let f = std::fs::File::open(&path).map_err(|e| format!("open temp: {e}"))?;
        crate::cloud::wssync::untar_workspace_from(f, &root, delete)
    })
    .await;
    match res {
        Ok(Ok(r)) => {
            let root = sync.workspace.as_path();
            let mut b = crate::cloud::wssync::read_binding(root);
            if let Some(id) = q.workspace_id {
                b.workspace_id = Some(id);
            }
            b.last_push = Some(unix_now_string());
            let _ = crate::cloud::wssync::write_binding(root, &b);
            Json(PushResp {
                written: r.written,
                deleted: r.deleted,
                trashed: r.trash_dir.is_some(),
            })
            .into_response()
        }
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct RevisionReq {
    revision: u64,
    workspace_id: Option<String>,
}

#[derive(serde::Serialize)]
struct RevisionResp {
    revision: u64,
}

/// Record the revision the client just completed. Monotonic on purpose: a
/// late or replayed call can only raise the counter, never walk it back to a
/// number a user has already seen quoted.
async fn sync_revision(State(sync): State<SyncRoot>, Json(req): Json<RevisionReq>) -> Response {
    let root = sync.workspace.as_path();
    let mut b = crate::cloud::wssync::read_binding(root);
    let revision = req.revision.max(b.revision.unwrap_or(0));
    b.revision = Some(revision);
    if let Some(id) = req.workspace_id {
        b.workspace_id = Some(id);
    }
    match crate::cloud::wssync::write_binding(root, &b) {
        Ok(()) => Json(RevisionResp { revision }).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

fn unix_now_string() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

// P2 incremental endpoints.

async fn sync_manifest(State(sync): State<SyncRoot>) -> Response {
    let busy = sync.busy().await;
    if busy {
        return (StatusCode::CONFLICT, "workspace busy (active turn)").into_response();
    }
    match crate::cloud::wssync::build_manifest(sync.workspace.as_path()) {
        Ok(m) => Json(m).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn sync_export(State(sync): State<SyncRoot>, Json(paths): Json<Vec<String>>) -> Response {
    let busy = sync.busy().await;
    if busy {
        return (StatusCode::CONFLICT, "workspace busy (active turn)").into_response();
    }
    let root = sync.workspace.as_path().to_path_buf();
    let tmp = tokio::task::spawn_blocking(move || {
        let tmp = tempfile::NamedTempFile::new().map_err(|e| format!("temp: {e}"))?;
        crate::cloud::wssync::tar_paths_to(&root, &paths, tmp.as_file())?;
        Ok::<_, String>(tmp)
    })
    .await;
    match tmp {
        Ok(Ok(tmp)) => stream_tar_temp(tmp).await,
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")).into_response(),
    }
}

async fn sync_trash(State(sync): State<SyncRoot>, Json(paths): Json<Vec<String>>) -> Response {
    let busy = sync.busy().await;
    if busy {
        return (StatusCode::CONFLICT, "workspace busy (active turn)").into_response();
    }
    match crate::cloud::wssync::trash_paths(sync.workspace.as_path(), &paths) {
        Ok(r) => Json(PushResp {
            written: r.written,
            deleted: r.deleted,
            trashed: r.trash_dir.is_some(),
        })
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// The classic `--serve` router: React webapp over HTTP/WS, gui-shell
/// folders, workspace file passthrough and `/cloud push|pull` sync.
///
/// dev-plan/59 Step 2: when `THCLAWS_SERVE_TOKEN` is set, every route
/// except `/healthz` sits behind `serve_token_gate`. The review of the
/// first cut found the bearer on `/ws` + `/upload` alone left
/// `/file-asset/{*rel}` (any workspace file, including a bot's
/// chromium cookies) and `/workspace/sync/pull` (the whole workspace as
/// a tarball) open on the same port, so the gate is a layer over the
/// group rather than a check copied into handlers. `/v1/*` keeps its
/// own `THCLAWS_API_TOKEN` policy and is merged outside the gate.
fn classic_router(state: ServeState) -> Router {
    let single_user = state.multi_tenant.is_none();
    // Mode C (cloud serve / browser SSH-tunnel): the React webapp
    // hosts gui-shells in iframes. Browser has no `thclaws://`
    // protocol handler, so expose the shell folders over HTTP
    // under `/gui-shell/<id>/...`. The bridge runs in postMessage
    // mode (inline-injected) — no per-shell WS.
    let gated = Router::new()
        .route("/", get(serve_index))
        .route("/ws", get(ws_handler))
        .route("/upload", post(serve_upload))
        .route("/gui-shell/{shell_id}", get(serve_gui_shell_index))
        .route("/gui-shell/{shell_id}/", get(serve_gui_shell_index))
        .route(
            "/gui-shell/{shell_id}/index.html",
            get(serve_gui_shell_index),
        )
        .route("/gui-shell/{shell_id}/{*rest}", get(serve_gui_shell_asset))
        // Workspace file passthrough — GUI shells can render
        // agent-produced files (image-batch's images/<slug>/*.png,
        // generated PDFs, contact-sheet HTML) via direct
        // <img src="/file-asset/images/foo/bar.png"> tags. The
        // server-side check is `Sandbox::check_in(cwd, rel)` so
        // paths can't escape the workspace. Single-tenant per
        // pod, so no cross-user concern (multi-tenant adds the
        // HMAC layer in build_shell_router).
        .route("/file-asset/{*rel}", get(serve_file_asset))
        // Workspace sync (dev-plan/51): /cloud push|pull against the
        // workspace dir. Same auth surface as /upload — the cloud ingress
        // ForwardAuth gates hosted runners (and the multiuser_auth layer
        // below covers multiuser pods); local --serve relies on
        // api_v1/loopback. push raises the body limit for the tarball.
        // job-artifacts Tier 1: `THCLAWS_SYNC_REQUIRE_AUTH=1` opts the
        // whole sync group into the SAME Bearer policy as /v1 (the
        // route_layer below), so an external orchestrator can use
        // export/push holding only THCLAWS_API_TOKEN — no tunnel /
        // ForwardAuth. Unset = classic trusted-network behavior,
        // existing deployments unaffected.
        .merge(sync_routes())
        .route_layer(axum::middleware::from_fn_with_state(
            single_user,
            serve_token_gate,
        ));
    Router::new()
        .route("/healthz", get(serve_health))
        .merge(gated)
        .with_state(state)
        .merge(crate::api_v1::router())
}

/// dev-plan/59 Step 3: run this process as the workspace HOST.
///
/// The host is a supervisor, not an agent: it spawns one `--serve` child per
/// bot, proxies the browser's socket to the focused one, and restarts it when
/// it dies. It initialises no agent, no model and no MCP — the authority to
/// reach every bot's folder is held by deterministic code rather than by
/// something a web page can talk to.
///
/// Step 3 starts exactly one bot, named by a hand-edited `.thclaws/bots.json`.
pub async fn run_supervisor(bind: SocketAddr) -> crate::error::Result<()> {
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| crate::error::Error::Tool(format!("bind {bind}: {e}")))?;
    run_supervisor_on(listener).await
}

/// Serve the host on a listener the caller already bound.
///
/// The desktop binds port 0 itself so it can put the real port in the
/// webview's URL before the server starts — asking the OS for a port, closing
/// it and re-binding is the race `run_on`'s own doc comment warns about.
pub async fn run_supervisor_on(listener: tokio::net::TcpListener) -> crate::error::Result<()> {
    if !crate::policy::serve_allowed() {
        return Err(crate::error::Error::Tool(
            "--serve is disabled by org policy (policies.runtime.allow_serve = false)".into(),
        ));
    }
    let workspace = std::env::current_dir()
        .map_err(|e| crate::error::Error::Tool(format!("workspace cwd unavailable: {e}")))?;
    // dev-plan/59 Step 4. Two ways to have no bot list, and they must not be
    // confused: a directory with nothing in it is a new workspace and gets a
    // host plus one empty bot; a directory that already holds an agent is a v2
    // workspace, and starting a host over it would leave that agent
    // unreachable at a root the host now owns.
    if !workspace.join(crate::bots::CONFIG_REL).exists() {
        if crate::bots::migrate::looks_like_v2_agent(&workspace) {
            return Err(crate::error::Error::Config(format!(
                "{} is a single-agent (v2) workspace: its agent lives at the root, where the host \
                 belongs in v3.\n  Run `thclaws bots migrate` to move it into \
                 .thclaws/bots/main/ first.",
                workspace.display()
            )));
        }
        let dest =
            crate::bots::migrate::mint_new_workspace(&workspace, crate::bots::migrate::MAIN_SLUG)?;
        eprintln!(
            "\x1b[36m[bots] new workspace — minted a host and one agent at {}\x1b[0m",
            dest.display()
        );
    }
    // Held for the life of the host. Two hosts on one workspace would both
    // spawn children and fight over bots.json and the address files.
    let _lock = crate::bots::lock_workspace(&workspace, "this host")?;
    // dev-plan/61: files the first multi-agent upgrade hid under
    // `.thclaws/bots/main/` go back to the root every agent shares.
    match crate::bots::migrate::restore_shared_files(&workspace) {
        Ok(r) if !r.moved.is_empty() || r.removed_tombstone || r.stamped_v4 => eprintln!(
            "\x1b[36m[bots] put {} item(s) back at the workspace root{}\x1b[0m",
            r.moved.len(),
            if r.clashes.is_empty() {
                String::new()
            } else {
                format!(
                    "; left in .thclaws/bots/main/ because the root has the same name: {}",
                    r.clashes.join(", ")
                )
            }
        ),
        Ok(_) => {}
        Err(e) => {
            eprintln!("\x1b[33m[bots] could not put files back at the workspace root: {e}\x1b[0m")
        }
    }
    let cfg = crate::bots::BotsConfig::load(&workspace)?;
    let sup = crate::bots::supervisor::BotSupervisor::new(&workspace)?;
    let _ = HOST_SUPERVISOR.set(sup.clone());
    spawn_host_heartbeat(sup.clone());
    let cap = crate::bots::supervisor::MAX_LIVE_BOTS;
    if cfg.bots.len() > cap {
        eprintln!(
            "\x1b[33m[bots] {} agents listed; starting the first {cap}. An idle policy that tears \
             children down is what lifts this, not a bigger number.\x1b[0m",
            cfg.bots.len()
        );
    }
    // The first entry is the default a browser reaches — `bots.json` order,
    // not alphabetical.
    sup.set_default(&cfg.bots[0].slug);
    // One bot that cannot start — a folder the user deleted by hand, say —
    // must not keep the whole workspace from opening. It is skipped loudly;
    // only a workspace where NOTHING starts is an error.
    let mut started = 0usize;
    for def in cfg.bots.iter().take(cap) {
        match sup.start(def) {
            Ok(_) => {
                started += 1;
                eprintln!(
                    "\x1b[36m[bots] supervising '{}' from {}\x1b[0m",
                    def.slug,
                    crate::bots::bot_dir(&workspace, &def.slug).display()
                );
            }
            Err(e) => eprintln!("\x1b[33m[bots] '{}' not started: {e}\x1b[0m", def.slug),
        }
    }
    if started == 0 {
        return Err(crate::error::Error::Config(
            "none of this workspace's agents could be started — see the lines above".into(),
        ));
    }

    let bound = listener
        .local_addr()
        .map_err(|e| crate::error::Error::Tool(format!("listener address: {e}")))?;
    eprintln!("\x1b[36m[serve] thClaws host listening on http://{bound}\x1b[0m");
    publish_bound_addr(bound);
    axum::serve(listener, supervisor_router(sup))
        .await
        .map_err(|e| crate::error::Error::Tool(format!("serve: {e}")))?;
    Ok(())
}

/// The host's surface: the React bundle, the proxied socket, the bot list and
/// its mutations, the file routes forwarded to a bot with that bot's own
/// bearer, workspace sync over the whole workspace, and `/v1` carried to an
/// agent with the caller's bearer.
fn supervisor_router(sup: Arc<crate::bots::supervisor::BotSupervisor>) -> Router {
    Router::new()
        .route("/", get(serve_index))
        .route("/ws", get(supervisor_ws))
        .route("/bots", get(supervisor_bots).post(supervisor_add_bot))
        .route("/bots/{slug}", axum::routing::delete(supervisor_remove_bot))
        .route("/bots/{slug}/restart", post(supervisor_restart_bot))
        .route("/file-asset/{*rel}", get(supervisor_forward))
        // A bot's shells live under the bot, and the bot's own `--serve`
        // already resolves and bridges them, so the host only carries the
        // request there. Without these a bot with a default shell opened
        // to an empty iframe.
        .route("/gui-shell/{shell_id}", get(supervisor_forward))
        .route("/gui-shell/{shell_id}/", get(supervisor_forward))
        .route("/gui-shell/{shell_id}/{*rel}", get(supervisor_forward))
        // No body-limit layer, matching the classic router's `/upload`: the
        // host must not be a different size of pipe than serving directly.
        .route("/upload", post(supervisor_forward))
        // dev-plan/60 G5: /cloud push|pull teleports the whole workspace, so
        // under a host it is the host's to serve — no single agent's tree is
        // the workspace.
        .merge(sync_routes())
        // Same opt-in bearer as the classic router. `route_layer` covers the
        // routes registered above it, so `/healthz` below stays open for a
        // parent supervisor's probe.
        .route_layer(axum::middleware::from_fn_with_state(true, serve_token_gate))
        .route("/healthz", get(supervisor_health))
        // dev-plan/60 G6: the OpenAI-compatible API is an agent's, carried to
        // one (`?bot=`, else the default). Outside the serve gate like the
        // classic router's `/v1`: it has its own `THCLAWS_API_TOKEN` policy,
        // which the agent enforces against the caller's bearer.
        .route("/v1/{*rest}", any(supervisor_forward_api))
        .route("/agent/run", post(supervisor_forward_api))
        .with_state(sup)
}

/// Which bot a request is for. `?bot=<slug>` names one; without it the
/// caller gets the default — the first entry in `bots.json`, not the first
/// alphabetically.
#[derive(serde::Deserialize, Default)]
struct BotQuery {
    #[serde(default)]
    bot: Option<String>,
}

fn pick_bot(
    sup: &crate::bots::supervisor::BotSupervisor,
    want: Option<&str>,
) -> std::result::Result<Arc<crate::bots::supervisor::Bot>, Response> {
    match want.map(str::trim).filter(|s| !s.is_empty()) {
        Some(slug) => sup.get(slug).ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("no agent '{slug}' is running in this workspace"),
            )
                .into_response()
        }),
        None => sup.focused().ok_or_else(|| {
            (StatusCode::SERVICE_UNAVAILABLE, "no agents are running").into_response()
        }),
    }
}

async fn supervisor_ws(
    ws: WebSocketUpgrade,
    State(sup): State<Arc<crate::bots::supervisor::BotSupervisor>>,
    Query(q): Query<BotQuery>,
) -> Response {
    // dev-plan/59 §6.3: the UI holds one socket per bot, so the slug travels
    // in the URL rather than in every frame. No `?bot=` is the plain
    // single-bot case and the pre-UI default.
    let bot = match pick_bot(&sup, q.bot.as_deref()) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    // Counted for the keepalive's `activity`, the way a plain serve counts
    // its own sockets.
    let counter = host_ws_connections();
    ws.on_upgrade(move |socket| async move {
        let _guard = WsGuard::new(counter);
        crate::bots::proxy::relay(socket, bot).await
    })
}

/// `GET /file-asset/<rel>` and `POST /upload` against the focused bot.
///
/// §7.3 left these 404ing, which meant image previews and uploads did not
/// work under a host. They are forwarded rather than served here: the host
/// has no business reading inside a bot's tree, and the bot already serves
/// both behind its own bearer — which the host attaches, because the browser
/// never learns a child's token.
/// The `bot=` a same-origin subresource request inherited: a shell's
/// `app.js` or a previewed page's stylesheet is fetched relative to its
/// document, which drops the query the page put there, but the Referer still
/// carries it.
/// `(slug, dir)` for every agent this host supervises.
fn bot_dirs(sup: &crate::bots::supervisor::BotSupervisor) -> Vec<(String, std::path::PathBuf)> {
    sup.list()
        .into_iter()
        .map(|b| (b.slug.clone(), b.dir.clone()))
        .collect()
}

/// The shell id in a `/gui-shell/<id>/…` path, if this is one.
fn shell_id_from_path(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/gui-shell/")?.split('/').next()?;
    // A traversal here would let a crafted URL probe directories outside the
    // shelf; an empty id matches nothing and would only waste the walk.
    if id.is_empty() || id.contains("..") || id.contains('\\') {
        return None;
    }
    Some(id)
}

/// Which agent ships shell `id`, read off disk.
///
/// The host already knows this: a shell id is a folder inside one agent's
/// tree. Asking the browser instead — `?bot=` on the index, the Referer on
/// everything relative under it — puts the answer in a header the shell's own
/// `Referrer-Policy` suppresses, and every sub-asset then goes to whichever
/// agent happens to be first.
///
/// `None` when no agent has it (a user-level or built-in shell belongs to no
/// single agent) and also when more than one does — both are for the caller's
/// remaining layers to settle rather than for this one to guess at.
fn owner_of_shell(bots: &[(String, std::path::PathBuf)], id: &str) -> Option<String> {
    let mut owner: Option<&str> = None;
    for (slug, dir) in bots {
        if dir.join(".thclaws").join("gui-shell").join(id).is_dir() {
            if owner.is_some() {
                return None;
            }
            owner = Some(slug);
        }
    }
    owner.map(str::to_string)
}

fn bot_from_referer(req: &Request) -> Option<String> {
    let referer = req
        .headers()
        .get(axum::http::header::REFERER)?
        .to_str()
        .ok()?;
    let query = referer.split_once('?')?.1;
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == "bot")
        .and_then(|(_, v)| urlencoding::decode(v).ok())
        .map(|v| v.into_owned())
        .filter(|v| !v.is_empty())
}

/// `/v1/*` and `/agent/run`, carried to an agent. Unlike the file routes the
/// caller's `Authorization` goes through untouched: the agent checks it
/// against the API token, which is not the agent's own serve token.
async fn supervisor_forward_api(
    State(sup): State<Arc<crate::bots::supervisor::BotSupervisor>>,
    Query(q): Query<BotQuery>,
    req: Request,
) -> Response {
    let bot = match pick_bot(&sup, q.bot.as_deref()) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let (addr, _) = match bot.wait_ready(std::time::Duration::from_secs(30)).await {
        Ok(v) => v,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    match crate::bots::proxy::forward_http_as_caller(&addr, req).await {
        Ok(resp) => resp,
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("agent '{}': {e}", bot.slug),
        )
            .into_response(),
    }
}

async fn supervisor_forward(
    State(sup): State<Arc<crate::bots::supervisor::BotSupervisor>>,
    Query(q): Query<BotQuery>,
    req: Request,
) -> Response {
    // Layered, most explicit first: what the caller asked for, then what the
    // filesystem says owns this shell, then the Referer, then the default.
    // The disk lookup sits above the Referer because it cannot be stripped by
    // a browser, an extension or a proxy — the header is the fragile one, and
    // relying on it alone is what shipped shells that rendered unstyled.
    let want = q
        .bot
        .clone()
        .or_else(|| {
            shell_id_from_path(req.uri().path()).and_then(|id| owner_of_shell(&bot_dirs(&sup), id))
        })
        .or_else(|| bot_from_referer(&req));
    let bot = match pick_bot(&sup, want.as_deref()) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let (addr, token) = match bot.wait_ready(std::time::Duration::from_secs(30)).await {
        Ok(v) => v,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    match crate::bots::proxy::forward_http(&addr, &token, req).await {
        Ok(resp) => resp,
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("agent '{}': {e}", bot.slug),
        )
            .into_response(),
    }
}

/// dev-plan/59 Step 5: `POST /bots {"slug": …}` installs a catalogue agent
/// into `.thclaws/bots/<slug>/`, lists it, and starts supervising it — the
/// call the host UI makes. It is a host action rather than a slash command
/// because a bot's sandbox root is its own folder: a bot cannot write to a
/// sibling's, which is the isolation working, not a gap.
#[derive(serde::Deserialize)]
struct AddBotBody {
    slug: String,
    #[serde(default)]
    version: Option<String>,
    /// Overwrite a folder bound to a different agent.
    #[serde(default)]
    force: bool,
    /// No catalogue agent: an empty bot, like a new folder.
    #[serde(default)]
    blank: bool,
}

async fn supervisor_add_bot(
    State(sup): State<Arc<crate::bots::supervisor::BotSupervisor>>,
    Json(body): Json<AddBotBody>,
) -> Response {
    let added = if body.blank {
        sup.add_blank_bot(&body.slug).await
    } else {
        sup.add_bot(&body.slug, body.version.as_deref(), body.force)
            .await
    };
    match added {
        Ok(done) => Json(serde_json::json!({
            "ok": true,
            "slug": done.slug,
            "dir": done.dir.display().to_string(),
            "newly_registered": done.newly_registered,
            "log": done.lines,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize, Default)]
struct RemoveBotQuery {
    /// Also delete the bot's folder — its sessions, KMS and browser logins.
    #[serde(default)]
    purge: bool,
}

async fn supervisor_remove_bot(
    State(sup): State<Arc<crate::bots::supervisor::BotSupervisor>>,
    axum::extract::Path(slug): axum::extract::Path<String>,
    Query(q): Query<RemoveBotQuery>,
) -> Response {
    match sup.remove_bot(&slug, q.purge).await {
        Ok(()) => Json(serde_json::json!({ "ok": true, "slug": slug })).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn supervisor_restart_bot(
    State(sup): State<Arc<crate::bots::supervisor::BotSupervisor>>,
    axum::extract::Path(slug): axum::extract::Path<String>,
) -> Response {
    match sup.restart_bot(&slug).await {
        Ok(_) => Json(serde_json::json!({ "ok": true, "slug": slug })).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(serde::Serialize)]
struct BotRow {
    slug: String,
    #[serde(flatten)]
    state: crate::bots::supervisor::BotState,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stderr_tail: Vec<String>,
}

fn bot_rows(sup: &crate::bots::supervisor::BotSupervisor) -> Vec<BotRow> {
    sup.list()
        .into_iter()
        .map(|b| BotRow {
            slug: b.slug.clone(),
            state: b.state(),
            stderr_tail: b.stderr_tail(),
        })
        .collect()
}

async fn supervisor_bots(
    State(sup): State<Arc<crate::bots::supervisor::BotSupervisor>>,
) -> impl IntoResponse {
    Json(serde_json::json!({ "bots": bot_rows(&sup) }))
}

/// A host is healthy while it has any bot that is serving or still trying.
/// Every bot parked is a host that will never serve again on its own — a
/// liveness probe should restart that pod rather than keep it. Merely
/// starting is not unhealthy: a cold bot can take longer than a probe's
/// grace period, and restarting it for that would loop forever.
fn host_ok(states: &[crate::bots::supervisor::BotState]) -> bool {
    states.is_empty()
        || !states
            .iter()
            .all(|s| matches!(s, crate::bots::supervisor::BotState::CrashLooped { .. }))
}

async fn supervisor_health(
    State(sup): State<Arc<crate::bots::supervisor::BotSupervisor>>,
) -> Response {
    let rows = bot_rows(&sup);
    let ready = rows
        .iter()
        .filter(|r| matches!(r.state, crate::bots::supervisor::BotState::Ready { .. }))
        .count();
    let states: Vec<_> = rows.iter().map(|r| r.state.clone()).collect();
    let ok = host_ok(&states);
    // dev-plan/60 G3: the same `busy` a plain serve reports, so anything that
    // reads it — the cloud reaper's probe — sees an agent working through the
    // host rather than an idle host.
    let (busy, busy_count) = agents_busy(&sup).await;
    let body = Json(serde_json::json!({
        "ok": ok,
        "role": "host",
        "bots": rows.len(),
        "ready": ready,
        "busy": busy,
        "busy_count": busy_count,
    }));
    if ok {
        body.into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
    }
}

/// WebSockets the host is relaying, across every agent.
fn host_ws_connections() -> Arc<AtomicUsize> {
    static COUNTER: std::sync::OnceLock<Arc<AtomicUsize>> = std::sync::OnceLock::new();
    COUNTER
        .get_or_init(|| Arc::new(AtomicUsize::new(0)))
        .clone()
}

/// A host runs no agent of its own; it is busy when any of its agents is.
///
/// Asked of every ready agent at once and capped as a whole, because the
/// answer lands in `/healthz`, whose Kubernetes probe gives up after a second
/// — one agent slow to answer must not get the whole pod restarted.
async fn agents_busy(sup: &crate::bots::supervisor::BotSupervisor) -> (bool, usize) {
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(600))
        .build()
    else {
        return (false, 0);
    };
    let asks = sup.list().into_iter().filter_map(|bot| {
        let crate::bots::supervisor::BotState::Ready { addr } = bot.state() else {
            return None;
        };
        let request = client
            .get(format!("http://{addr}/healthz"))
            .bearer_auth(bot.token());
        Some(async move {
            let v: serde_json::Value = request.send().await.ok()?.json().await.ok()?;
            Some((
                v.get("busy").and_then(|b| b.as_bool()).unwrap_or(false),
                v.get("busy_count").and_then(|c| c.as_u64()).unwrap_or(0) as usize,
            ))
        })
    });
    let answers = tokio::time::timeout(Duration::from_millis(700), futures::future::join_all(asks))
        .await
        .unwrap_or_default();
    answers
        .into_iter()
        .flatten()
        .fold((false, 0), |(any, n), (busy, count)| {
            (any || busy, n + count)
        })
}

/// dev-plan/60 G2: a hosted workspace's one keepalive, sent by the host.
///
/// Same contract as `spawn_cloud_heartbeat`: a ping every minute while a
/// browser is connected, an agent is busy, or a schedule is pending
/// (`activity: false` for the last), and one straight away when busy flips so
/// the dashboard's "running" pill is prompt. The host cannot be woken by an
/// agent's turn starting, so it looks every ten seconds.
fn spawn_host_heartbeat(sup: Arc<crate::bots::supervisor::BotSupervisor>) {
    let Some((endpoint, token)) = cloud_keepalive_target() else {
        return;
    };
    eprintln!("\x1b[36m[bots] cloud heartbeat → {endpoint} (for the whole workspace)\x1b[0m");
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[bots] heartbeat client build failed: {e}");
                return;
            }
        };
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_busy = false;
        let mut last_agents: Option<serde_json::Value> = None;
        let mut last_sent: Option<std::time::Instant> = None;
        loop {
            tick.tick().await;
            let (busy, _) = agents_busy(&sup).await;
            let connected = host_ws_connections().load(Ordering::SeqCst) > 0;
            let next_schedule_at = crate::schedule::ScheduleStore::load()
                .ok()
                .and_then(|st| st.next_fire_across_all(chrono::Utc::now()))
                .map(|t| t.to_rfc3339());
            let activity = connected || busy;
            // dev-plan/60 G9: the dashboard shows what a workspace holds, even
            // paused, from the list the host last sent. A changed list is sent
            // straight away, like a busy flip.
            let agents = crate::bots::BotsConfig::load(sup.workspace())
                .ok()
                .map(|cfg| {
                    serde_json::Value::Array(
                        cfg.bots
                            .iter()
                            .map(|b| serde_json::json!({ "slug": b.slug, "name": b.name }))
                            .collect(),
                    )
                });
            let flipped = busy != last_busy || agents != last_agents;
            let due = last_sent.map_or(true, |t| t.elapsed() >= Duration::from_secs(60));
            if !flipped && (!due || (!activity && next_schedule_at.is_none())) {
                continue;
            }
            match client
                .post(&endpoint)
                .bearer_auth(&token)
                .json(&serde_json::json!({
                    "busy": busy,
                    "activity": activity,
                    "next_schedule_at": next_schedule_at,
                    "agents": agents,
                }))
                .send()
                .await
            {
                // Remembered only once the cloud has it: a first ping that
                // fails (the API restarting, say) is retried on the next tick
                // instead of waiting for the next change.
                Ok(r) if r.status().is_success() => {
                    last_busy = busy;
                    last_agents = agents;
                }
                Ok(r) => eprintln!("[bots] heartbeat {endpoint} returned HTTP {}", r.status()),
                Err(e) => eprintln!("[bots] heartbeat {endpoint} failed: {e}"),
            }
            last_sent = Some(std::time::Instant::now());
        }
    });
}

/// dev-plan/59: write the address this process actually bound to the file a
/// supervisor named, so it can be reached without the supervisor having to
/// pre-bind a port and hand the number down — which would leave a window for
/// anything on the machine to take it. Written last, once the listener exists,
/// so the file appearing means the port is real.
fn publish_bound_addr(bound: SocketAddr) {
    let Some(path) = std::env::var_os("THCLAWS_SERVE_ADDR_FILE") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, bound.to_string()) {
        eprintln!(
            "\x1b[33m[serve] cannot write THCLAWS_SERVE_ADDR_FILE {}: {e}\x1b[0m",
            path.display()
        );
    }
}

/// dev-plan/59 Step 2: single-user bearer over the classic router.
/// Runs before a WS upgrade so a wrong token never opens a socket. No
/// token configured → passes, which is every existing deployment. A
/// multiuser pod is skipped: `multiuser_auth` already proves identity
/// there and the supervisor never runs one as a child.
async fn serve_token_gate(State(single_user): State<bool>, req: Request, next: Next) -> Response {
    if single_user && !serve_token_ok(req.headers(), req.uri().query()) {
        eprintln!(
            "\x1b[33m[serve] {} rejected: bad or missing bearer\x1b[0m",
            req.uri().path()
        );
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(req).await
}

/// Build the Mode B Axum router. Mounts the bound shell at
/// `/t/<token>/` (or `/` when `no_auth`) and silently 404s
/// everything else — `/gui-shell/<id>/...` from Mode A's internal
/// protocol path is *not* mounted, so direct URLs to other shells
/// fail closed.
fn build_shell_router(
    bind: &SocketAddr,
    state: ServeState,
    mode: ShellServeMode,
) -> crate::error::Result<Router> {
    // Resolve bound shell + token + safety guards before binding.
    let shell = crate::gui_shell::serve::resolve_bound_shell(&mode.shell_id)?;
    crate::gui_shell::serve::check_no_auth_safety(bind, mode.no_auth, mode.no_auth_allow_public)?;

    // Token: pinned > stored > generated.
    let token: Option<crate::gui_shell::ShellToken> = if mode.no_auth {
        None
    } else if let Some(pinned) = mode.pinned_token.clone() {
        Some(crate::gui_shell::tokens::pin(
            &mode.shell_id,
            bind.port(),
            pinned,
            mode.token_ttl_secs,
        )?)
    } else {
        // Default TTL = 30 days when nothing else is specified.
        let ttl = mode.token_ttl_secs.or(Some(30 * 24 * 60 * 60));
        let (t, _was_generated) =
            crate::gui_shell::tokens::resolve_or_generate(&mode.shell_id, bind.port(), ttl)?;
        Some(t)
    };

    // Build prefixed routes. We could use Router::nest, but explicit
    // route strings keep the URL surface visible in source — important
    // because the "no /gui-shell/<id>/" rule is the security model.
    let prefix = crate::gui_shell::serve::url_prefix(token.as_ref());
    let ws_url_path = format!("{prefix}/__ws");
    let bridge_url_path = format!("{prefix}/__bridge.js");
    let index_path = if prefix.is_empty() {
        "/".to_string()
    } else {
        format!("{prefix}/")
    };
    let asset_path = format!("{prefix}/{{*rel}}");

    let shell_clone1 = shell.clone();
    let shell_clone2 = shell.clone();
    let ws_url_for_index = ws_url_path.clone();

    // /t/<token>/file-asset/<rel> — serves files from the shell's
    // current workspace (the cwd, set by `run` when a shell is
    // bound). Used by shell frontends to render agent-produced files
    // (generated images, outputs, etc.) via direct <img src> tags.
    //
    // dev-plan/35 Tier 1 multi-tenant: when multi_tenant is on,
    // every file-asset request must (a) carry the same HMAC-signed
    // headers as the WS upgrade and (b) request a path under
    // `users/<that_user_id>/...`. Cloud routing layer attaches the
    // headers automatically (proxied through). User A can't fetch
    // user B's files because the path validator rejects the
    // mismatched user_id prefix.
    let file_asset_path = format!("{prefix}/file-asset/{{*rel}}");
    let workspace_for_files = state.workspace.clone();
    let multi_tenant_for_files = state.multi_tenant.clone();
    let file_asset_route = file_asset_path.clone();

    let mut router = Router::new()
        .route(
            &index_path,
            get(move || {
                let s = shell_clone1.clone();
                let u = ws_url_for_index.clone();
                async move { crate::gui_shell::serve::serve_shell_index(&s, &u) }
            }),
        )
        .route(
            &bridge_url_path,
            get(|| async { crate::gui_shell::serve::serve_bridge_runtime() }),
        )
        .route(
            &file_asset_route,
            get(
                move |axum::extract::Path(rel): axum::extract::Path<String>,
                      headers: axum::http::HeaderMap| {
                    let workspace = workspace_for_files.clone();
                    let mt = multi_tenant_for_files.clone();
                    async move {
                        // Multi-tenant: verify HMAC and confirm
                        // the path is scoped to this user.
                        if let Some(mt) = mt {
                            if let Err(status) = verify_file_asset_for_user(&headers, &mt, &rel) {
                                return axum::response::Response::builder()
                                    .status(status)
                                    .body(axum::body::Body::from("forbidden"))
                                    .expect("build file-asset 4xx");
                            }
                        }
                        let range = headers
                            .get(axum::http::header::RANGE)
                            .and_then(|v| v.to_str().ok());
                        crate::gui_shell::serve::serve_project_asset(
                            workspace.as_ref(),
                            &rel,
                            range,
                        )
                    }
                },
            ),
        )
        .route(
            &asset_path,
            get(
                move |axum::extract::Path(rel): axum::extract::Path<String>| {
                    let s = shell_clone2.clone();
                    async move { crate::gui_shell::serve::serve_shell_asset(&s, &rel) }
                },
            ),
        )
        .route(&ws_url_path, get(ws_handler))
        .route("/healthz", get(serve_health));

    // dev-plan/39 Tier 1: keep classic chat reachable at /chat/ when a
    // shell is bound at /. Only safe under no_auth — auth-gated shells
    // would otherwise let users bypass the token by hitting /chat/. For
    // hosted workspaces (the primary Tier 1 target) no_auth is always
    // true because the workspace URL is auth-gated upstream by Caddy.
    if mode.no_auth {
        router = router
            .route("/chat/", get(serve_index))
            .route("/chat", get(serve_index))
            .route("/chat/ws", get(ws_handler))
            .route("/chat/upload", post(serve_upload));
    }

    let mut router = router.with_state(state);

    // /v1/* OpenAI-compat surface stays available regardless of Mode B —
    // it has its own auth (THCLAWS_API_TOKEN) independent of the shell
    // token, and removing it would break automation clients that don't
    // know or care about the shell binding.
    router = router.merge(crate::api_v1::router());

    // Print the launch URL on stdout so the operator can copy it.
    let launch = crate::gui_shell::serve::launch_url(*bind, token.as_ref());
    eprintln!(
        "\x1b[36m[serve] Serving {} ({}) at\n        {}\x1b[0m",
        shell.manifest().name,
        shell.manifest().version,
        launch
    );
    if token.is_some() {
        eprintln!(
            "\x1b[36m[serve] Token persisted to ~/.config/thclaws/gui-shell-tokens.json (rotate with `thclaws shell rotate-token {}`).\x1b[0m",
            mode.shell_id
        );
    }

    Ok(router)
}

/// dev-plan/42: in `--multiuser`, EVERY HTTP request must carry a valid
/// HMAC-signed identity (the cloud routing layer injects
/// `X-Thclaws-User` / `-ts` / `-proof`) — otherwise 401. Without this a
/// multiuser pod would serve the index, bridge, `/upload`, and `/v1/*`
/// to anyone, since those routes have no per-route HMAC check (only WS +
/// file-asset did). Single-tenant `--serve` never installs this layer
/// (keeps its `THCLAWS_API_TOKEN` Bearer model). `/healthz` is exempt so
/// k8s liveness/readiness probes — which carry no identity — still pass.
async fn multiuser_auth(
    State(verifier): State<Arc<crate::multi_tenant::IdentityVerifier>>,
    req: Request,
    next: Next,
) -> Response {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if multiuser_request_authed(req.uri().path(), req.headers(), &verifier, now) {
        next.run(req).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

/// The auth decision for [`multiuser_auth`], split out so it's unit
/// testable without driving a live router. `/healthz` is exempt (k8s
/// probes); everything else needs a valid HMAC identity triple.
fn multiuser_request_authed(
    path: &str,
    headers: &axum::http::HeaderMap,
    verifier: &crate::multi_tenant::IdentityVerifier,
    now_secs: u64,
) -> bool {
    if path == "/healthz" {
        return true;
    }
    let get = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    match (
        get("x-thclaws-user"),
        get("x-thclaws-user-ts"),
        get("x-thclaws-user-proof"),
    ) {
        (Some(u), Some(ts), Some(p)) => crate::multi_tenant::verify_identity(
            u,
            ts,
            p,
            get("x-thclaws-user-sig"),
            verifier,
            now_secs,
        )
        .is_ok(),
        _ => false,
    }
}

fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// RAII guard that increments the WS-connection counter on
/// construction and decrements on drop. Used inside `handle_socket`
/// so the counter is panic-safe — any early return / unwind still
/// releases the count, which the cloud heartbeat task observes on its
/// next tick.
struct WsGuard(Arc<AtomicUsize>);

impl WsGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for WsGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Spawn a background task that pings the thclaws.cloud control plane
/// every 60s while either (a) at least one WS connection is active OR
/// (b) the engine has an in-flight agent turn. Used as the activity
/// signal for the cloud's idle reaper: a workspace pod that nobody is
/// watching AND has no work to do stops pinging, `last_active_at`
/// ages out, the reaper scales the pod to 0.
///
/// Pre-fix this only checked WS connections, so closing the browser
/// during a long batch (50-subject image gen takes ~7 min — past the
/// 30-min reaper window if the user steps away) eventually killed
/// the pod mid-loop. The "busy" half of the condition keeps the pod
/// alive while there's actual work in flight, regardless of who's
/// watching.
///
/// Requires three env vars from the provisioner:
///   - `THCLAWS_CLOUD_URL`     — e.g. `https://thclaws.cloud`
///   - `THCLAWS_CLOUD_TOKEN`   — CliToken minted at provision time
///   - `THCLAWS_WORKSPACE_ID`  — UUID of this workspace
///
/// Any missing var = local / non-cloud run; the task no-ops and
/// returns immediately so we don't burn a tokio worker on idle.
/// Where a hosted workspace's keepalive goes, and the token it carries. `None`
/// outside thclaws.cloud — any of the three env vars missing.
fn cloud_keepalive_target() -> Option<(String, String)> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let url = get("THCLAWS_CLOUD_URL")?;
    let token = get("THCLAWS_CLOUD_TOKEN")?;
    let workspace_id = get("THCLAWS_WORKSPACE_ID")?;
    let endpoint = format!(
        "{}/api/hosted/workspaces/{}/keepalive",
        url.trim_end_matches('/'),
        workspace_id
    );
    Some((endpoint, token))
}

fn spawn_cloud_heartbeat(connections: Arc<AtomicUsize>) {
    let Some((endpoint, token)) = cloud_keepalive_target() else {
        return;
    };
    eprintln!(
        "\x1b[36m[serve] cloud heartbeat → {endpoint} (every 60s while WS connected or agent busy)\x1b[0m"
    );
    tokio::spawn(async move {
        // Re-using the global stream client would pull in stream
        // timeout knobs we don't want; build a small dedicated one.
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[serve] heartbeat client build failed: {e}");
                return;
            }
        };
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Race the 60s tick against busy-transition notifies so the
        // dashboard "running" pill (dev-plan/36) appears within
        // seconds of a turn starting instead of waiting up to a
        // minute for the next periodic ping.
        let transition = crate::agent_activity::busy_transition();
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = transition.notified() => {}
            }
            let connected = connections.load(Ordering::SeqCst) > 0;
            let busy = crate::agent_activity::is_agent_busy();
            // Earliest pending `/schedule` fire, reported so the cloud
            // reaper can resume this workspace in time to run it. On a
            // paused runner nothing is executing when the cron comes
            // due — `pause()` scales the Deployment to 0 — so without
            // this the job simply never fires.
            let next_schedule_at = crate::schedule::ScheduleStore::load()
                .ok()
                .and_then(|st| st.next_fire_across_all(chrono::Utc::now()))
                .map(|t| t.to_rfc3339());
            // Idle with nothing scheduled: stay quiet and let the
            // reaper do its job. Idle WITH something scheduled: still
            // ping, but flagged `activity: false` so the API records
            // the time without treating it as someone being here —
            // otherwise the pod would never be allowed to pause and
            // every scheduled workspace would run 24/7.
            let activity = connected || busy;
            if !activity && next_schedule_at.is_none() {
                continue;
            }
            // Body carries the current busy state so the cloud
            // dashboard can render a "running" pill on this
            // workspace's row in the user's workspace list. The
            // API tolerates an empty body for backwards-compat with
            // older engines (busy field is optional server-side).
            match client
                .post(&endpoint)
                .bearer_auth(&token)
                .json(&serde_json::json!({
                    "busy": busy,
                    "activity": activity,
                    "next_schedule_at": next_schedule_at,
                }))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => {
                    eprintln!("[serve] heartbeat {endpoint} returned HTTP {}", r.status());
                }
                Err(e) => {
                    eprintln!("[serve] heartbeat {endpoint} failed: {e}");
                }
            }
        }
    });
}

async fn serve_index() -> impl IntoResponse {
    // No-cache headers so users always see the bundle from the
    // running binary. Pre-fix, no `Cache-Control` was set and
    // browsers applied heuristic caching → after `make install` +
    // `--serve` restart, an already-open tab kept serving the old
    // HTML and users thought new UI was missing (May 2026 report:
    // the Gemma settings gear "didn't appear" until hard-refresh).
    // The bundle is embedded in the binary, so the right
    // freshness signal is "the binary mtime" — easiest to express
    // as `no-store` for this single, small endpoint.
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (
                axum::http::header::CACHE_CONTROL,
                "no-store, must-revalidate",
            ),
        ],
        FRONTEND_HTML,
    )
}

/// Liveness + busy probe. k8s probes only check the status code, so the
/// JSON body is free for the cloud reaper's pre-pause busy check: it GETs
/// this in-cluster and skips pausing a pod with a turn in flight
/// (defense-in-depth alongside the busy keepalive heartbeat).
/// job-artifacts Tier 1: opt-in Bearer gate for `/workspace/sync/*`.
/// `THCLAWS_SYNC_REQUIRE_AUTH=1` → every sync request must carry the same
/// `Authorization: Bearer $THCLAWS_API_TOKEN` as `/v1`. Unset/other → pass
/// through (the classic trusted-network / ForwardAuth / multiuser-HMAC model).
async fn sync_bearer_gate(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // dev-plan/42+45 isolation: every `/workspace/sync/*` handler operates on
    // `state.workspace`, which in a multiuser pod is the SHARED root holding
    // every tenant's `workspace-<id>/`. `multiuser_auth` proves who the caller
    // is but the handlers don't scope to them — so an authenticated co-tenant
    // could pull a tarball of everyone's workspace (`.env`, `state/kms/`,
    // sessions) or push over all of them. Per-tenant sync isn't defined for a
    // shared workspace, so the surface is closed rather than half-scoped.
    if crate::workdir::is_multiuser() {
        return (
            StatusCode::FORBIDDEN,
            "workspace sync is disabled on a multiuser workspace — /cloud push|pull operate on \
             the whole pod directory, which is shared across tenants here",
        )
            .into_response();
    }
    let required = std::env::var("THCLAWS_SYNC_REQUIRE_AUTH")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if required {
        if let Err(resp) = crate::api_v1::check_bearer_headers(req.headers()) {
            return resp;
        }
    }
    next.run(req).await
}

/// Workspace root, captured at serve startup so `serve_health` (no `State`) can
/// probe for detached background jobs. Set once in `run_with_engine`.
static SERVE_WORKSPACE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// The running host, for the desktop's quit path (#209).
static HOST_SUPERVISOR: std::sync::OnceLock<Arc<crate::bots::supervisor::BotSupervisor>> =
    std::sync::OnceLock::new();

/// #209/#210: stop every agent and wait for it, from a plain thread.
///
/// The desktop window closes on the GUI thread, which is the async runtime's
/// own thread — it cannot await. It also must not simply leave: an agent's
/// stderr is a pipe into this process, so exiting first kills the pipe under
/// the agent and its next log line aborts it. Signalling is async-free, and
/// the wait is a poll, so the supervisor's tasks keep running while this
/// blocks. `true` when every agent stopped inside `timeout`.
pub fn stop_host_agents(timeout: Duration) -> bool {
    match HOST_SUPERVISOR.get() {
        Some(sup) => stop_and_wait(sup, timeout),
        None => true,
    }
}

fn stop_and_wait(sup: &crate::bots::supervisor::BotSupervisor, timeout: Duration) -> bool {
    use crate::bots::supervisor::BotState;
    sup.shutdown();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let settled = sup
            .list()
            .iter()
            .all(|b| matches!(b.state(), BotState::Stopped | BotState::CrashLooped { .. }));
        if settled {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// True if a detached background job is still running in the workspace. Any
/// agent that spawns one drops a `<dir>/.jobs/<id>.json` carrying a live `pid`
/// (course.py's `job start` does this); while any such pid is alive the cloud
/// idle-reaper must NOT pause the pod — `/healthz` reports it as busy.
fn background_jobs_alive() -> bool {
    let Some(root) = SERVE_WORKSPACE.get() else {
        return false;
    };
    fn scan(dir: &std::path::Path, depth: u8) -> bool {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return false;
        };
        for e in rd.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = e.file_name();
            let p = e.path();
            if name == ".jobs" {
                if jobs_dir_has_live_pid(&p) {
                    return true;
                }
            } else if depth > 0
                && !matches!(
                    name.to_str(),
                    Some("node_modules" | "target" | ".git" | ".venv" | ".sync-trash")
                )
                && scan(&p, depth - 1)
            {
                return true;
            }
        }
        false
    }
    scan(root, 4)
}

fn jobs_dir_has_live_pid(dir: &std::path::Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let pid = std::fs::read_to_string(&p)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("pid").and_then(|x| x.as_i64()));
        if let Some(pid) = pid {
            if pid_alive(pid as i32) {
                return true;
            }
        }
    }
    false
}

#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    // kill(pid, 0): 0 → alive; EPERM → alive (not ours); ESRCH → dead.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
#[cfg(not(unix))]
fn pid_alive(_pid: i32) -> bool {
    false
}

#[cfg(all(test, unix))]
mod background_job_probe_tests {
    use super::*;

    #[test]
    fn pid_alive_self_true_bogus_false() {
        assert!(pid_alive(std::process::id() as i32));
        assert!(!pid_alive(2_147_480_000)); // a pid that won't exist
        assert!(!pid_alive(0));
    }

    #[test]
    fn jobs_dir_busy_only_when_a_pid_is_live() {
        let d = tempfile::tempdir().unwrap();
        let live = d.path().join("store/.jobs");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(
            live.join("import-book.json"),
            format!(
                r#"{{"pid": {}, "argv": ["import-book"]}}"#,
                std::process::id()
            ),
        )
        .unwrap();
        assert!(jobs_dir_has_live_pid(&live), "own pid should read as live");

        let dead = d.path().join("other/.jobs");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::write(dead.join("old.json"), r#"{"pid": 2147480000, "argv": []}"#).unwrap();
        assert!(
            !jobs_dir_has_live_pid(&dead),
            "dead pid must not read as busy"
        );
    }
}

async fn serve_health() -> impl IntoResponse {
    let busy = crate::agent_activity::is_agent_busy() || background_jobs_alive();
    axum::Json(serde_json::json!({
        "ok": true,
        "busy": busy,
        "busy_count": crate::agent_activity::busy_count(),
    }))
}

/// `GET /gui-shell/<id>/` — serve a shell's index.html for Mode C
/// (cloud serve / iframe-in-React-parent). Bridge runtime inlined so
/// no relative-path resolution across traefik strip-prefixes.
async fn serve_gui_shell_index(
    axum::extract::Path(shell_id): axum::extract::Path<String>,
) -> Response {
    let shell = match crate::gui_shell::serve::resolve_bound_shell(&shell_id) {
        Ok(s) => s,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(axum::body::Body::from("shell not found"))
                .expect("build 404");
        }
    };
    crate::gui_shell::serve::serve_shell_index_inline(&shell)
}

/// `GET /gui-shell/<id>/<rel>` — serve a shell asset. Sandbox-checked
/// against the shell folder by `serve_shell_asset`.
async fn serve_gui_shell_asset(
    axum::extract::Path((shell_id, rel)): axum::extract::Path<(String, String)>,
) -> Response {
    let shell = match crate::gui_shell::serve::resolve_bound_shell(&shell_id) {
        Ok(s) => s,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(axum::body::Body::from("shell not found"))
                .expect("build 404");
        }
    };
    crate::gui_shell::serve::serve_shell_asset(&shell, &rel)
}

/// `GET /file-asset/<rel>` — serve a workspace-relative file so GUI
/// shells can render agent-produced output (image-batch's
/// `images/<slug>/*.png`, generated PDFs, contact-sheet HTML, etc.)
/// via plain `<img src>` / `<a href>` tags. Path is
/// `Sandbox::check_in`-validated against the current cwd so a
/// crafted `../etc/passwd` can't escape. Single-tenant per --serve
/// process; multi-tenant adds HMAC in `build_shell_router`.
async fn serve_file_asset(
    axum::extract::Path(rel): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let cwd = crate::workdir::workspace_root();
    let range = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok());
    crate::gui_shell::serve::serve_project_asset(&cwd, &rel, range)
}

/// `POST /upload` — multipart file upload from the --serve browser
/// surface. Each part lands at `<workspace>/uploads/<name>` (with
/// `_N` suffix on collision). After all parts are saved, the handler
/// synthesizes a chat-shaped user message and pushes it through the
/// shared session input pipe — the agent reacts as if the user had
/// typed a description of what they just uploaded, and project
/// `AGENTS.md` instructions steer what happens next.
///
/// Returns `{ "ok": true, "files": [{ "path": …, "size": … }, …] }`
/// so the frontend can show a confirmation chip per file. Caps:
/// [`UPLOAD_MAX_BYTES`] per file, [`UPLOAD_MAX_FILES`] per request.
/// Oversize / overflow is rejected with 413.
/// `?dir=<rel>` lets a caller (e.g. a GUI shell staging files) drop the
/// upload into a specific workspace subfolder instead of `uploads/`.
/// When set, the chat-message synthesis is skipped — the files are
/// staged silently for the shell to act on, not announced to the agent.
#[derive(serde::Deserialize, Default)]
struct UploadQuery {
    dir: Option<String>,
}

async fn serve_upload(
    State(state): State<ServeState>,
    Query(q): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Response {
    let workspace = state.workspace.as_ref();
    let target_dir = q.dir.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let uploads_dir = match target_dir {
        Some(rel) => ensure_target_dir(workspace, rel),
        None => ensure_uploads_dir(workspace),
    };
    let uploads_dir = match uploads_dir {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("cannot use upload dir: {e}"),
                })),
            )
                .into_response();
        }
    };

    let mut saved: Vec<UploadedFile> = Vec::new();
    while let Some(field) = match multipart.next_field().await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("malformed multipart: {e}"),
                })),
            )
                .into_response();
        }
    } {
        if saved.len() >= UPLOAD_MAX_FILES {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("at most {UPLOAD_MAX_FILES} files per request"),
                })),
            )
                .into_response();
        }
        let filename = field
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "upload".to_string());
        let media_type = field.content_type().map(|s| s.to_string());
        let dest = unique_path(&uploads_dir, &filename);
        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "ok": false,
                        "error": format!("read part bytes: {e}"),
                    })),
                )
                    .into_response();
            }
        };
        if bytes.len() as u64 > UPLOAD_MAX_BYTES {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "{} exceeds {}-byte cap",
                        filename, UPLOAD_MAX_BYTES
                    ),
                })),
            )
                .into_response();
        }
        if let Err(e) = std::fs::write(&dest, &bytes) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("write {}: {e}", dest.display()),
                })),
            )
                .into_response();
        }
        let relative_path = dest
            .strip_prefix(workspace)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| format!("{UPLOADS_DIRNAME}/{filename}"));
        saved.push(UploadedFile {
            relative_path,
            media_type,
            size_bytes: bytes.len() as u64,
        });
    }

    if saved.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": "no files in request",
            })),
        )
            .into_response();
    }

    // Only announce uploads to the agent for the default `uploads/`
    // drop. A `?dir=` upload is the shell staging files for itself
    // (e.g. book-author's raw/ sources) — synthesizing a turn here
    // would make the agent react to files it isn't meant to see yet.
    if target_dir.is_none() {
        let synth = render_upload_message("serve", &saved);
        let _ = state.shared.input_tx.send(ShellInput::Line(synth));
    }

    let files: Vec<serde_json::Value> = saved
        .iter()
        .map(|f| {
            serde_json::json!({
                "path": f.relative_path,
                "size": f.size_bytes,
                "media_type": f.media_type,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "files": files })),
    )
        .into_response()
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    State(state): State<ServeState>,
) -> Response {
    // dev-plan/35 Tier 1: when multi-tenant mode is on, verify the
    // cloud routing layer's HMAC-signed user-identity headers BEFORE
    // accepting the WS upgrade. Bad / missing headers → 401 without
    // ever opening a socket.
    let resolved_shared = match resolve_session_handle(&state, &headers) {
        Ok(handle) => handle,
        Err(status) => return status.into_response(),
    };
    ws.on_upgrade(move |socket| handle_socket(socket, state, resolved_shared))
}

/// dev-plan/35 Tier 1: verify HMAC headers + confirm the requested
/// file-asset path begins with `users/<authenticated_user_id>/`.
/// Rejects cross-user file access even if the user knows the path.
fn verify_file_asset_for_user(
    headers: &axum::http::HeaderMap,
    mt: &MultiTenantState,
    rel: &str,
) -> Result<(), StatusCode> {
    let get = |name: &str| -> Result<&str, StatusCode> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .ok_or(StatusCode::UNAUTHORIZED)
    };
    let user_id_h = get("x-thclaws-user")?;
    let ts_h = get("x-thclaws-user-ts")?;
    let proof_h = get("x-thclaws-user-proof")?;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let user_id = crate::multi_tenant::verify_identity(
        user_id_h,
        ts_h,
        proof_h,
        headers
            .get("x-thclaws-user-sig")
            .and_then(|v| v.to_str().ok()),
        &mt.verifier,
        now_secs,
    )
    .map_err(|e| {
        eprintln!("\x1b[33m[file-asset] HMAC rejected: {e}\x1b[0m");
        StatusCode::UNAUTHORIZED
    })?;
    // URL-decode rel (Axum's Path extractor already does this for us,
    // but be defensive) and ensure it begins with users/<user_id>/.
    // Two valid prefixes: output/users/<id>/ and .thclaws/users/<id>/.
    let normalised = rel.trim_start_matches('/');
    let user_segment = format!("users/{}/", user_id.as_str());
    let valid = normalised.starts_with(&format!("output/{user_segment}"))
        || normalised.starts_with(&format!(".thclaws/{user_segment}"));
    if !valid {
        eprintln!(
            "\x1b[33m[file-asset] user={} attempted cross-user fetch: {rel}\x1b[0m",
            user_id.as_str()
        );
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}

/// Single-tenant: return the default shared session handle.
/// Multi-tenant: verify the three cloud-routing headers and look up
/// (or spawn) the per-user session in the registry.
/// dev-plan/59 Step 2: optional bearer for single-user `--serve`.
///
/// Single-user `--serve` has never authenticated `/ws`: the router binds it
/// with no auth layer, so anything that can reach the port drives the engine
/// — runs tools, reads the workspace. That was defensible while the posture
/// was "127.0.0.1, one user, one project". It stops being defensible when a
/// supervisor runs several engines on loopback for different bots, because
/// then any one of them is reachable by anything else on the machine.
///
/// Configured by `THCLAWS_SERVE_TOKEN`, deliberately an env var and NOT a
/// flag: argv is world-readable through `ps`, while the environment of a
/// process is not readable by other users on either macOS or Linux.
///
/// **Unset or empty = no auth = exactly today's behaviour.** This is
/// additive; an existing `--serve` user sees no change unless they opt in.
fn serve_token() -> Option<String> {
    std::env::var("THCLAWS_SERVE_TOKEN")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Constant-time compare so a wrong token cannot be recovered byte by byte
/// from response timing.
fn token_matches(expected: &str, given: &str) -> bool {
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Check the bearer on a request that carries one, when one is required.
///
/// Accepts either `Authorization: Bearer <t>` (what the supervisor's Rust WS
/// client sends) or `?token=<t>` (what a browser could send, since the
/// WebSocket API cannot set headers). Multi-tenant mode has its own HMAC
/// identity check and is left alone — adding a second shared secret there
/// would be a downgrade, not an upgrade.
fn serve_token_ok(headers: &axum::http::HeaderMap, query: Option<&str>) -> bool {
    let Some(expected) = serve_token() else {
        return true; // not configured — unchanged behaviour
    };
    if let Some(v) = headers.get(axum::http::header::AUTHORIZATION) {
        if let Some(t) = v.to_str().ok().and_then(|s| s.strip_prefix("Bearer ")) {
            if token_matches(&expected, t.trim()) {
                return true;
            }
        }
    }
    if let Some(q) = query {
        for pair in q.split('&') {
            if let Some(t) = pair.strip_prefix("token=") {
                // The value arrives percent-encoded from a browser.
                let decoded = urlencoding::decode(t).unwrap_or(std::borrow::Cow::Borrowed(t));
                if token_matches(&expected, decoded.trim()) {
                    return true;
                }
            }
        }
    }
    false
}

fn resolve_session_handle(
    state: &ServeState,
    headers: &axum::http::HeaderMap,
) -> Result<Arc<SharedSessionHandle>, StatusCode> {
    let Some(mt) = state.multi_tenant.as_ref() else {
        return Ok(state.shared.clone());
    };
    let get = |name: &str| -> Result<&str, StatusCode> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .ok_or(StatusCode::UNAUTHORIZED)
    };
    let user_id_h = get("x-thclaws-user")?;
    let ts_h = get("x-thclaws-user-ts")?;
    let proof_h = get("x-thclaws-user-proof")?;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let user_id = crate::multi_tenant::verify_identity(
        user_id_h,
        ts_h,
        proof_h,
        headers
            .get("x-thclaws-user-sig")
            .and_then(|v| v.to_str().ok()),
        &mt.verifier,
        now_secs,
    )
    .map_err(|e| {
        eprintln!("\x1b[33m[serve] HMAC rejected: {e}\x1b[0m");
        StatusCode::UNAUTHORIZED
    })?;
    // Display name is NOT part of the signed triple — it's advisory,
    // for greeting the user. The id is what's authenticated.
    // Percent-encoded UTF-8: HTTP header values are latin-1 and most of
    // our users have Thai names, so the API encodes and we decode. A
    // malformed value degrades to no name rather than failing the
    // connection — it's a greeting, not an auth input.
    let display_name = headers
        .get("x-thclaws-user-name")
        .and_then(|v| v.to_str().ok())
        .map(|raw| {
            urlencoding::decode(raw)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| raw.to_string())
        });
    let session = mt.registry.get_or_spawn(&user_id, display_name.as_deref());
    Ok(session.handle.clone())
}

/// One task per WS connection. Receives inbound frames, parses JSON,
/// routes through `handle_ipc` with a WS-flavored `IpcContext` whose
/// `dispatch` closure pushes payloads back over the socket.
///
/// Outbound event subscription (events_tx → WS frames) lands in SERVE3
/// alongside the snapshot frame. SERVE2's WS is half-duplex (inbound
/// only) so the IpcContext + handle_ipc plumbing can be smoke-tested
/// before the rendering layer is wired.
async fn handle_socket(socket: WebSocket, state: ServeState, shared: Arc<SharedSessionHandle>) {
    // Tick the cloud-heartbeat counter for the lifetime of this socket.
    // `_ws_guard` decrements on drop so panics / early returns can't
    // leak a stale count. The cloud heartbeat task reads this to
    // decide whether to ping keepalive on every 60s tick.
    let _ws_guard = WsGuard::new(state.ws_connections.clone());

    let (mut sink, mut stream) = socket.split();
    // Outbound channel: every dispatch closure invocation lands here;
    // a single task drains it to the sink so concurrent dispatches
    // don't race on the WS write side.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let dispatch = {
        let tx = out_tx.clone();
        Arc::new(move |payload: String| {
            let _ = tx.send(payload);
        })
    };
    // Snapshot builder for `frontend_ready` handshake (issue #80).
    // Mirrors gui.rs:1060-1109's `UserEvent::SendInitialState` arm:
    // gathers provider/model + readiness + MCP servers + recent
    // sessions + active KMSes into one JSON envelope and ships it
    // back. Pre-fix this was a no-op stub (M6.36 SERVE3 deferred
    // implementation), so every fresh browser connect (including
    // an F5 refresh on an existing session) landed on a fully
    // hydrated worker but rendered an empty sidebar — sessions /
    // MCP / KMS were all wiped from the user's perspective even
    // though the engine still had them.
    let initial_dispatch = {
        let tx = out_tx.clone();
        Arc::new(move |payload: String| {
            let _ = tx.send(payload);
        })
    };
    // dev-plan/42: capture the resolved (per-user) session roots for the
    // initial-state snapshot closure below.
    let initial_session_roots = shared.session_roots.clone();
    let ctx = IpcContext {
        is_serve_mode: true,
        // dev-plan/35 Tier 1: `shared` here is the RESOLVED handle
        // (per-user in multi-tenant mode; the default in single-
        // tenant mode). Subsequent state.shared references below
        // (events subscription, workflow_approver lookup) use the
        // same resolved handle so per-user isolation holds end-to-end.
        shared: shared.clone(),
        approver: state.approver.clone(),
        pending_asks: state.pending_asks.clone(),
        dispatch,
        on_quit: {
            let tx = out_tx.clone();
            Arc::new(move || {
                let _ = tx.send(serde_json::json!({"type":"session_quit"}).to_string());
            })
        },
        on_send_initial_state: Arc::new(move || {
            // dev-plan/42: per-user sessions dir from the resolved handle
            // (multiuser) so the snapshot lists this user's history.
            let sessions_dir = initial_session_roots
                .as_ref()
                .map(|r: &crate::multi_tenant::SessionRoots| r.sessions_dir.clone());
            let payload = build_initial_state_payload(sessions_dir.clone());
            let _ = initial_dispatch(payload);
            // Hydrate a chat-first gui-shell (`<thc-chat>`) with the active
            // session's transcript on (re)connect. The worker's input queue
            // only drains between turns, so we read the session from disk
            // here rather than asking the worker to re-emit — a browser
            // reconnecting MID-run gets its history immediately instead of
            // after the (possibly minutes-long) turn finishes. Targeted to
            // THIS client; no broadcast, no worker involvement.
            if let Some(hist) = build_gui_shell_history_payload(sessions_dir) {
                let _ = initial_dispatch(hist);
            }
            // A newer release, when the last check found one. Same shape as
            // the desktop's: the cached answer is a file read, so nothing on
            // the path to a usable page waits on github.com, and the refresh
            // behind it only has to land before the next connect.
            //
            // `update_check::enabled()` is false inside our container images,
            // so a hosted workspace — whose engine is ours and not the user's
            // to upgrade — stays quiet without a check here.
            if let Some(up) = crate::update_check::cached() {
                let _ = initial_dispatch(
                    serde_json::json!({
                        "type": "update_available",
                        "version": up.version,
                        "url": up.url,
                    })
                    .to_string(),
                );
            }
            tokio::spawn(async { crate::update_check::refresh().await });
        }),
        on_zoom: Arc::new(|_scale| {
            // Browser handles its own zoom (Cmd-+/-); no server-side
            // hook needed unless we want to persist the scale across
            // sessions. Defer.
        }),
        workflow_approver: shared.workflow_approver.clone(),
    };

    // Ask-user broadcast subscription (issue #82). Each WS connection
    // gets its own receiver; the forwarder spawned in [`run`] pushes
    // one envelope per `AskUserQuestion` tool call.
    let mut ask_rx = state.ask_broadcast.subscribe();
    let ask_tx = out_tx.clone();
    let ask_forwarder = tokio::spawn(async move {
        loop {
            match ask_rx.recv().await {
                Ok(payload) => {
                    if ask_tx.send(payload).is_err() {
                        return;
                    }
                }
                // Slow consumer dropped frames; resume — the agent
                // re-asks on retry, and lagged ask-frames are no
                // worse than the pre-fix state (which was complete
                // silence). Log the drop so lag is diagnosable
                // (issue #163 Bug 1) rather than vanishing silently.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("[event_forwarder:ws] lagged: dropped {n} events");
                    continue;
                }
                Err(_) => return,
            }
        }
    });

    // Surface a one-time permission notice (e.g. a multiuser pod where a
    // configured "ask" was overridden to auto-approve) as a chat/system line
    // so the user understands why their setting wasn't honored.
    if let Some(notice) = &state.permission_notice {
        for dispatch in render_chat_dispatches(&ViewEvent::SlashOutput(notice.clone())) {
            let _ = out_tx.send(dispatch);
        }
    }

    // Re-emit any STILL-PENDING approval requests to this freshly-connected
    // client. The broadcast above only reaches tabs connected at the moment
    // a request fires, so a browser that connects later — e.g. after the
    // user refreshes mid-turn — would never see the modal and the turn would
    // stay hung. Replaying the unresolved set lets the reconnected tab
    // approve and unblock it.
    for req in state.approver.unresolved_requests() {
        let payload = serde_json::json!({
            "type": "approval_request",
            "session_id": req.session_id,
            "id": req.id,
            "tool_name": req.tool_name,
            "input": req.input,
            "summary": req.summary,
            "originator": req.originator,
        });
        let _ = out_tx.send(payload.to_string());
    }

    // M6.36 SERVE3: subscribe to the broadcast and translate every
    // ViewEvent into chat-shaped + terminal-shaped envelopes, identical
    // to gui::spawn_event_translator's path. Both translators feed the
    // same outbound channel so the writer task serializes WS writes.
    // dev-plan/35 Tier 1: subscribe to the RESOLVED handle (per-user
    // in multi-tenant; default in single-tenant). Critical for
    // isolation — without this, every user's translator would
    // subscribe to the default handle and see everyone's events.
    let mut events_rx = shared.view_events.subscribe();
    let event_tx = out_tx.clone();
    let event_forwarder = tokio::spawn(async move {
        loop {
            match events_rx.recv().await {
                Ok(frame) => {
                    if event_tx.send(frame).is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let _ = event_tx
                        .send(serde_json::json!({"type":"session_view_invalidated"}).to_string());
                }
                Err(_) => break,
            }
        }
    });

    // Outbound writer task — serializes every payload to the WS sink.
    let writer = tokio::spawn(async move {
        while let Some(payload) = out_rx.recv().await {
            if serde_json::from_str::<serde_json::Value>(&payload)
                .is_ok_and(|frame| frame["type"] == "session_quit")
            {
                let _ = sink.send(Message::Close(None)).await;
                break;
            }
            if sink.send(Message::text(payload)).await.is_err() {
                break;
            }
        }
    });

    // Inbound reader loop.
    while let Some(frame) = stream.next().await {
        match frame {
            Ok(Message::Text(text)) => {
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(text.as_str()) else {
                    continue;
                };
                // Web has no fall-through transport — anything
                // handle_ipc doesn't recognize is silently dropped.
                let _handled = handle_ipc(msg, &ctx);
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {} // ignore Ping/Pong/Binary for now
        }
    }
    event_forwarder.abort();
    ask_forwarder.abort();
    writer.abort();
}

/// Build the `initial_state` JSON envelope ported from gui.rs's
/// `UserEvent::SendInitialState` arm (gui.rs:1060-1109). Loaded
/// fresh from disk on every WS connect so an F5 refresh always
/// reflects the current `AppConfig` / sessions / MCP / KMS state.
///
/// Reports the saved model's readiness as-is — no auto-switch. The
/// previous behaviour rewrote settings.json to a local runtime whenever
/// the active provider lacked credentials, without probing that the
/// runtime existed; see the matching note in gui.rs's SendInitialState.
fn build_initial_state_payload(sessions_dir: Option<std::path::PathBuf>) -> String {
    let config = AppConfig::load().unwrap_or_default();
    let provider_name = config.detect_provider().unwrap_or("unknown");
    let provider_ready = provider_has_credentials(&config);
    // Consult the live MCP_TOOL_COUNTS cache (populated by the
    // McpReady worker event) so reconnect-after-startup ships real
    // counts instead of the hardcoded zeros that surfaced as issue #86.
    let mcp_servers = crate::gui::build_mcp_servers_payload(&config);
    // dev-plan/42: in multiuser `--serve` the WS-connect snapshot must
    // list THIS user's sessions (their per-user `sessions_dir`), not the
    // process-cwd default (the owner's shared `/workspace/.thclaws/
    // sessions/`). `None` → single-tenant default.
    let sessions: Vec<serde_json::Value> = sessions_dir
        .map(SessionStore::new)
        .or_else(|| SessionStore::default_path().map(SessionStore::new))
        .and_then(|store| store.list().ok())
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
    let kmss = build_kms_initial_payload(&config);
    // #95(c) + #168: the frontend's mount-time tab-visibility `*_get`
    // requests (team / shell / browser) can be dropped if the socket is
    // still CONNECTING — the wsSend guard logs and discards
    // (frontend/src/hooks/useIPC.ts). That race is far more likely over a
    // high-latency tunnel (e.g. ngrok), which left the Browser/Shell tabs
    // hidden there but visible on localhost. Ship every flag in the
    // (re)connect-driven initial_state so the tabs self-heal regardless of
    // WS timing, without the user re-firing the get via Settings.
    let project = crate::config::ProjectConfig::load();
    let team_enabled = project
        .as_ref()
        .and_then(|c| c.team_enabled)
        .unwrap_or(false);
    let shell_tab_enabled = project
        .as_ref()
        .and_then(|c| c.shell_tab_enabled)
        .unwrap_or(false);
    serde_json::json!({
        "type": "initial_state",
        "provider": provider_name,
        "model": config.model,
        "thinking": crate::providers::ThinkingLevel::json(config.thinking_budget),
        "provider_ready": provider_ready,
        "mcp_servers": mcp_servers,
        "sessions": sessions,
        "kmss": kmss,
        "team_enabled": team_enabled,
        "shell_tab_enabled": shell_tab_enabled,
        "browser_enabled": config.browser_enabled,
        // Whether the worker has an agent turn in flight RIGHT NOW. A
        // browser that (re)connects mid-turn — e.g. after detaching during
        // a long TextToSpeech/video render — must restore its "working"
        // indicator, otherwise the still-running turn looks stopped even
        // though it keeps producing output server-side and the result
        // streams in when it finishes. The frontend re-subscribes to the
        // live event stream on connect, so the terminal `done` clears it.
        "agent_busy": crate::agent_activity::is_agent_busy(),
        "version": crate::version::VERSION,
    })
    .to_string()
}

/// Build the `gui_shell_event`/`history` envelope for the active session so
/// a reconnecting chat-first shell (`<thc-chat>`) repaints its transcript.
///
/// The "active" session is the most-recent non-empty one on disk — in
/// serve single-tenant the running turn keeps saving it, so it IS the
/// worker's current session. This mirrors the frontend's dev-plan/36
/// "restore most-recent non-empty session" heuristic (App.tsx), just for
/// the gui-shell path. Reads disk (not the turn-blocked worker queue), so
/// it works even while a run is streaming — completed turns come from the
/// file, the in-flight turn keeps streaming live over the same WS.
/// `None` when there's no non-empty session (fresh workspace) so the
/// shell's own intro/"Welcome" line stands.
fn build_gui_shell_history_payload(sessions_dir: Option<std::path::PathBuf>) -> Option<String> {
    let store = sessions_dir
        .map(SessionStore::new)
        .or_else(|| SessionStore::default_path().map(SessionStore::new))?;
    let meta = store
        .list()
        .ok()?
        .into_iter()
        .find(|m| m.message_count > 0)?;
    let session = store.load(&meta.id).ok()?;
    let display = crate::shared_session::DisplayMessage::from_session(&session);
    if display.is_empty() {
        return None;
    }
    Some(crate::event_render::gui_shell_history_envelope(&display))
}

/// KMS list for the initial-state payload. Mirrors the structure
/// the GUI emits in `ViewEvent::KmsUpdate` (gui.rs uses
/// `build_kms_update_payload`, which lives behind the `gui` feature
/// flag and isn't reachable from the always-on `server` module).
/// One inline implementation here keeps the build feature-free.
///
/// Uses `kms::list_all()` which returns project entries first then
/// user (matching the resolve-priority order). Dedup by name —
/// project wins on collision since `list_all` emits them first.
fn build_kms_initial_payload(config: &AppConfig) -> Vec<serde_json::Value> {
    let active: std::collections::HashSet<&str> =
        config.kms_active.iter().map(String::as_str).collect();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut all: Vec<(String, &'static str, bool)> = Vec::new();
    for kref in crate::kms::list_all() {
        if !seen.insert(kref.name.clone()) {
            // Already saw this name in a higher-priority scope.
            continue;
        }
        let scope = kref.scope.as_str();
        let active_flag = active.contains(kref.name.as_str());
        all.push((kref.name, scope, active_flag));
    }
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all.into_iter()
        .map(|(name, scope, active)| {
            serde_json::json!({ "name": name, "scope": scope, "active": active })
        })
        .collect()
}

#[cfg(test)]
mod tests {

    /// The host can name a shell's agent without asking the browser, which is
    /// the point: the Referer it used to rely on is suppressed by the shell's
    /// own `Referrer-Policy`, and every sub-asset went to the wrong agent.
    #[test]
    fn a_shell_is_traced_back_to_the_agent_that_ships_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mk = |slug: &str, shell: &str| {
            let dir = tmp.path().join(slug);
            std::fs::create_dir_all(dir.join(".thclaws").join("gui-shell").join(shell)).unwrap();
            (slug.to_string(), dir)
        };
        let main = mk("main", "session-explorer");
        let writer = mk("writer", "book-studio");
        let bots = vec![main, writer];

        assert_eq!(
            owner_of_shell(&bots, "book-studio").as_deref(),
            Some("writer"),
            "the second agent's shell must not resolve to the first"
        );
        assert_eq!(
            owner_of_shell(&bots, "session-explorer").as_deref(),
            Some("main")
        );
        // A built-in or user-level shell belongs to no one agent: leave it to
        // the caller's remaining layers rather than guess.
        assert_eq!(owner_of_shell(&bots, "not-installed"), None);
    }

    #[test]
    fn a_shell_id_two_agents_both_ship_is_left_unresolved() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut bots = Vec::new();
        for slug in ["main", "writer"] {
            let dir = tmp.path().join(slug);
            std::fs::create_dir_all(dir.join(".thclaws").join("gui-shell").join("desk")).unwrap();
            bots.push((slug.to_string(), dir));
        }
        // Ambiguous — picking either would be a coin flip presented as a fact.
        assert_eq!(owner_of_shell(&bots, "desk"), None);
    }

    #[test]
    fn only_a_gui_shell_path_yields_an_id_and_never_a_traversal() {
        assert_eq!(
            shell_id_from_path("/gui-shell/book-studio/style.css"),
            Some("book-studio")
        );
        assert_eq!(
            shell_id_from_path("/gui-shell/book-studio/"),
            Some("book-studio")
        );
        assert_eq!(
            shell_id_from_path("/gui-shell/book-studio"),
            Some("book-studio")
        );
        // Not a shell path at all.
        assert_eq!(shell_id_from_path("/file-asset/out/a.png"), None);
        assert_eq!(shell_id_from_path("/ws"), None);
        // A crafted id must not send the walk outside the shelf.
        assert_eq!(shell_id_from_path("/gui-shell/../../etc/passwd"), None);
        assert_eq!(shell_id_from_path("/gui-shell//style.css"), None);
    }
    use super::*;

    /// ServeConfig defaults bind to localhost — security-relevant
    /// invariant (Phase 1 trust model). Pin so a future refactor that
    /// loosens the default surfaces in CI.
    #[test]
    fn default_serve_config_binds_localhost() {
        let cfg = ServeConfig::default();
        assert_eq!(cfg.bind.ip(), std::net::IpAddr::from([127, 0, 0, 1]));
        assert_eq!(cfg.bind.port(), 8443);
    }

    /// M6.36 SERVE7: end-to-end WS round-trip integration test.
    ///
    /// Spins up `server::run` in a background task on an OS-assigned
    /// port, opens a WebSocket client via tokio-tungstenite, sends
    /// `frontend_ready` + a `/help` slash command, asserts the server
    /// fires the expected chat-shaped envelopes back. This is the
    /// regression backstop for the WS pipeline — any future refactor
    /// that breaks the inbound dispatch, the outbound translator, or
    /// the per-connection writer task will fail this test in CI.
    #[tokio::test]
    async fn ws_round_trip_processes_slash_command() {
        use futures::{SinkExt, StreamExt};
        use std::time::Duration;
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

        // Bind port 0 and KEEP the listener, handing it to the server.
        // Dropping it to let `run` re-bind used to lose the port to another
        // test in the parallel suite often enough to fail ~1 run in 3.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let cfg = ServeConfig {
            bind: addr,
            ..Default::default()
        };
        let server_handle = tokio::spawn(async move {
            let _ = run_on(cfg, listener).await;
        });

        // Give the server a beat to bind. Healthz poll loop catches
        // the race more reliably than a fixed sleep.
        let url = format!("ws://{addr}/ws");
        let healthz_url = format!("http://{addr}/healthz");
        let mut bound = false;
        for _ in 0..50 {
            if reqwest::get(&healthz_url).await.is_ok() {
                bound = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(bound, "server didn't bind within 2.5s");

        let (mut ws, _resp) = connect_async(&url).await.expect("ws connect");

        // Frontend's typical opening handshake.
        ws.send(WsMessage::text(
            serde_json::json!({"type": "frontend_ready"}).to_string(),
        ))
        .await
        .expect("ws send frontend_ready");

        // Slash command — produces SlashOutput events without needing
        // any LLM provider configured (no API keys in CI).
        ws.send(WsMessage::text(
            serde_json::json!({"type": "shell_input", "session_id": "", "text": "/help"})
                .to_string(),
        ))
        .await
        .expect("ws send shell_input");

        // Drain frames collecting `type` values; assert the canonical
        // sequence shows up. The deadline is a *ceiling*, not a wait —
        // the loop breaks the moment `chat_done` lands, so a generous
        // one costs a passing run nothing. 3s was tight enough to lose
        // roughly one run in five (~1 in 3 under the full suite's load),
        // and the failure read as "saw: [initial_state]" — nothing had
        // arrived yet, not a wrong value.
        let mut seen: Vec<String> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), ws.next()).await {
                Ok(Some(Ok(WsMessage::Text(text)))) => {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text.as_str()) {
                        if parsed["type"] == "session_event" {
                            assert!(parsed["session_id"].as_str().is_some_and(|s| !s.is_empty()));
                            assert!(parsed["sequence"].as_u64().is_some_and(|n| n > 0));
                            for event in parsed["events"].as_array().expect("session event batch") {
                                if let Some(t) = event["type"].as_str() {
                                    seen.push(t.to_string());
                                }
                            }
                        } else if let Some(t) = parsed["type"].as_str() {
                            seen.push(t.to_string());
                        }
                        if seen.iter().any(|t| t == "chat_done") {
                            break;
                        }
                    }
                }
                Ok(Some(Ok(_other))) => {} // ping/pong/binary — ignore
                Ok(Some(Err(_))) | Ok(None) => break,
                Err(_) => continue, // timeout — keep polling until deadline
            }
        }

        // Echo back what we observed so failure messages are debuggable.
        assert!(
            seen.contains(&"chat_user_message".to_string()),
            "missing chat_user_message; saw: {seen:?}"
        );
        assert!(
            seen.contains(&"chat_slash_output".to_string()),
            "missing chat_slash_output (slash command body); saw: {seen:?}"
        );
        assert!(
            seen.contains(&"chat_done".to_string()),
            "missing chat_done (turn termination); saw: {seen:?}"
        );

        // Reconnect after activation: the original activation broadcast is
        // gone, but the new frontend must learn a nonempty execution ID.
        let _ = ws.send(WsMessage::Close(None)).await;
        let (mut reconnected, _) = connect_async(&url).await.unwrap();
        reconnected
            .send(WsMessage::text(
                serde_json::json!({"type":"frontend_ready"}).to_string(),
            ))
            .await
            .unwrap();
        let identity = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(Ok(WsMessage::Text(text))) = reconnected.next().await {
                let frame: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                if frame["type"] == "session_execution" {
                    return frame["session_id"].as_str().unwrap().to_string();
                }
            }
            panic!("connection closed without execution identity")
        })
        .await
        .expect("reconnect must replay execution identity");
        assert!(!identity.is_empty());
        reconnected
            .send(WsMessage::text(
                serde_json::json!({"type":"shell_input", "text":"/quit"}).to_string(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(frame) = reconnected.next().await {
                if matches!(frame, Ok(WsMessage::Close(_))) {
                    return;
                }
            }
            panic!("expected a WebSocket close frame after /quit");
        })
        .await
        .expect("/quit must close the WebSocket");
        server_handle.abort();
    }

    /// `POST /upload` saves a multipart file to `<workspace>/uploads/`,
    /// applies `_N` suffix on collision. Workspace is injected via
    /// `ServeConfig.workspace` so the test doesn't touch process cwd
    /// (which would race with other tests in the same binary).
    #[tokio::test]
    async fn upload_post_saves_to_workspace_uploads_dir() {
        use std::time::Duration;

        let td = tempfile::tempdir().unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let cfg = ServeConfig {
            bind: addr,
            workspace: Some(td.path().to_path_buf()),
            gui_shell: None,
            multi_tenant: None,
        };
        let server_handle = tokio::spawn(async move {
            let _ = run(cfg).await;
        });

        let healthz_url = format!("http://{addr}/healthz");
        for _ in 0..50 {
            if reqwest::get(&healthz_url).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let upload_url = format!("http://{addr}/upload");
        let body_a = vec![0u8; 16];
        let part_a = reqwest::multipart::Part::bytes(body_a.clone())
            .file_name("photo.jpg")
            .mime_str("image/jpeg")
            .unwrap();
        let form = reqwest::multipart::Form::new().part("file", part_a);

        let resp = reqwest::Client::new()
            .post(&upload_url)
            .multipart(form)
            .send()
            .await
            .expect("upload POST");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let json: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(json["ok"], serde_json::Value::Bool(true));
        assert_eq!(json["files"][0]["path"], "_uploads/photo.jpg");
        assert_eq!(json["files"][0]["size"], 16);

        assert!(td.path().join("_uploads").join("photo.jpg").exists());

        // Second upload with the same name → `_1` suffix.
        let part_b = reqwest::multipart::Part::bytes(vec![1u8; 8])
            .file_name("photo.jpg")
            .mime_str("image/jpeg")
            .unwrap();
        let form2 = reqwest::multipart::Form::new().part("file", part_b);
        let resp2 = reqwest::Client::new()
            .post(&upload_url)
            .multipart(form2)
            .send()
            .await
            .expect("upload POST 2");
        assert_eq!(resp2.status(), reqwest::StatusCode::OK);
        let json2: serde_json::Value = resp2.json().await.unwrap();
        assert_eq!(json2["files"][0]["path"], "_uploads/photo_1.jpg");
        assert!(td.path().join("_uploads").join("photo_1.jpg").exists());

        server_handle.abort();
    }

    #[tokio::test]
    async fn upload_with_dir_param_saves_to_subdirectory() {
        use std::time::Duration;

        let td = tempfile::tempdir().unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let cfg = ServeConfig {
            bind: addr,
            workspace: Some(td.path().to_path_buf()),
            gui_shell: None,
            multi_tenant: None,
        };
        let server_handle = tokio::spawn(async move {
            let _ = run(cfg).await;
        });

        let healthz_url = format!("http://{addr}/healthz");
        for _ in 0..50 {
            if reqwest::get(&healthz_url).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let upload_url = format!("http://{addr}/upload?dir=raw");
        let part = reqwest::multipart::Part::bytes(vec![0u8; 16])
            .file_name("photo.jpg")
            .mime_str("image/jpeg")
            .unwrap();
        let form = reqwest::multipart::Form::new().part("file", part);

        let resp = reqwest::Client::new()
            .post(&upload_url)
            .multipart(form)
            .send()
            .await
            .expect("upload POST with dir param");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let json: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(json["ok"], serde_json::Value::Bool(true));
        assert_eq!(json["files"][0]["path"], "raw/photo.jpg");

        assert!(td.path().join("raw").join("photo.jpg").exists());
        assert!(!td.path().join("uploads").exists());

        server_handle.abort();
    }

    #[tokio::test]
    async fn upload_with_dir_param_collision_suffix() {
        use std::time::Duration;

        let td = tempfile::tempdir().unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let cfg = ServeConfig {
            bind: addr,
            workspace: Some(td.path().to_path_buf()),
            gui_shell: None,
            multi_tenant: None,
        };
        let server_handle = tokio::spawn(async move {
            let _ = run(cfg).await;
        });

        let healthz_url = format!("http://{addr}/healthz");
        for _ in 0..50 {
            if reqwest::get(&healthz_url).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let upload_url = format!("http://{addr}/upload?dir=raw");

        let part_a = reqwest::multipart::Part::bytes(vec![0u8; 16])
            .file_name("photo.jpg")
            .mime_str("image/jpeg")
            .unwrap();
        let resp_a = reqwest::Client::new()
            .post(&upload_url)
            .multipart(reqwest::multipart::Form::new().part("file", part_a))
            .send()
            .await
            .expect("upload POST 1");
        assert_eq!(resp_a.status(), reqwest::StatusCode::OK);
        let json_a: serde_json::Value = resp_a.json().await.unwrap();
        assert_eq!(json_a["files"][0]["path"], "raw/photo.jpg");

        let part_b = reqwest::multipart::Part::bytes(vec![1u8; 8])
            .file_name("photo.jpg")
            .mime_str("image/jpeg")
            .unwrap();
        let resp_b = reqwest::Client::new()
            .post(&upload_url)
            .multipart(reqwest::multipart::Form::new().part("file", part_b))
            .send()
            .await
            .expect("upload POST 2");
        assert_eq!(resp_b.status(), reqwest::StatusCode::OK);
        let json_b: serde_json::Value = resp_b.json().await.unwrap();
        assert_eq!(json_b["files"][0]["path"], "raw/photo_1.jpg");

        assert!(td.path().join("raw").join("photo_1.jpg").exists());

        server_handle.abort();
    }

    #[tokio::test]
    async fn upload_with_dir_escape_rejected() {
        use std::time::Duration;

        let td = tempfile::tempdir().unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let cfg = ServeConfig {
            bind: addr,
            workspace: Some(td.path().to_path_buf()),
            gui_shell: None,
            multi_tenant: None,
        };
        let server_handle = tokio::spawn(async move {
            let _ = run(cfg).await;
        });

        let healthz_url = format!("http://{addr}/healthz");
        for _ in 0..50 {
            if reqwest::get(&healthz_url).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // `..` component must be rejected.
        let part = reqwest::multipart::Part::bytes(vec![0u8; 8])
            .file_name("evil.txt")
            .mime_str("text/plain")
            .unwrap();
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/upload?dir=../etc"))
            .multipart(reqwest::multipart::Form::new().part("file", part))
            .send()
            .await
            .expect("upload POST with path traversal");
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
        let json: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(json["ok"], serde_json::Value::Bool(false));

        // Leading `/` is stripped by ensure_target_dir (trim_matches('/')),
        // so `/absolute` is treated as the subdirectory `absolute` — not rejected.
        let part2 = reqwest::multipart::Part::bytes(vec![0u8; 8])
            .file_name("file.txt")
            .mime_str("text/plain")
            .unwrap();
        let resp2 = reqwest::Client::new()
            .post(format!("http://{addr}/upload?dir=/absolute"))
            .multipart(reqwest::multipart::Form::new().part("file", part2))
            .send()
            .await
            .expect("upload POST with leading slash");
        assert_eq!(resp2.status(), reqwest::StatusCode::OK);
        let json2: serde_json::Value = resp2.json().await.unwrap();
        assert_eq!(json2["ok"], serde_json::Value::Bool(true));
        assert_eq!(json2["files"][0]["path"], "absolute/file.txt");

        server_handle.abort();
    }

    // ── dev-plan/35 Tier 1 multi-tenant tests ────────────────────
    //
    // These unit-test the per-user routing and file-asset isolation
    // helpers without spinning up a full TCP+WebSocket harness. The
    // helpers do all the security-relevant work (HMAC verify, path
    // scoping); a real-server end-to-end test in Task 32 confirms
    // the wiring; these tests confirm the per-helper invariants
    // that wiring depends on.

    use crate::multi_tenant::auth::sign_user_header;
    use axum::http::HeaderMap;

    const TEST_SECRET: &[u8] = b"test-hmac-secret-for-unit-tests-only";

    fn dummy_state(multi_tenant: Option<MultiTenantState>) -> ServeState {
        let approver = std::sync::Arc::new(crate::permissions::AutoApprover);
        let shared =
            std::sync::Arc::new(crate::shared_session::spawn_with_approver(approver.clone()));
        let (ask_broadcast, _) = tokio::sync::broadcast::channel::<String>(16);
        // ServeState wants GuiApprover (concrete type), not AutoApprover.
        // For these tests we only exercise the multi_tenant + routing
        // paths that don't touch `state.approver` — construct a fresh
        // GuiApprover and discard the receiver.
        let (gui_approver, _approval_rx) = crate::permissions::GuiApprover::new();
        ServeState {
            shared,
            approver: gui_approver,
            pending_asks: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            ask_broadcast,
            workspace: std::sync::Arc::new(std::env::temp_dir()),
            multi_tenant,
            ws_connections: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            permission_notice: None,
        }
    }

    fn multi_tenant_state() -> MultiTenantState {
        let approver = std::sync::Arc::new(crate::permissions::AutoApprover);
        let registry =
            crate::multi_tenant::UserSessionRegistry::new(crate::multi_tenant::RegistryConfig {
                max_users: 10,
                idle_timeout: std::time::Duration::from_secs(60),
                approver,
                // Existing 9 integration tests are HMAC + URL-prefix
                // checks that never write per-user state — temp_dir
                // is fine, nothing lands on disk.
                project_root: std::env::temp_dir(),
                workspaces_base: None,
                def_source: None,
                owner_user_id: None,
            });
        MultiTenantState {
            registry,
            verifier: std::sync::Arc::new(crate::multi_tenant::IdentityVerifier::Hmac {
                secret: TEST_SECRET.to_vec(),
            }),
        }
    }

    fn headers_for(user_id: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let proof = sign_user_header(user_id, ts, TEST_SECRET);
        headers.insert("x-thclaws-user", user_id.parse().unwrap());
        headers.insert("x-thclaws-user-ts", ts.to_string().parse().unwrap());
        headers.insert("x-thclaws-user-proof", proof.parse().unwrap());
        headers
    }

    // dev-plan/42: the global multiuser auth gate.
    #[test]
    fn multiuser_auth_requires_valid_identity_on_every_route() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let empty = HeaderMap::new();

        let v = crate::multi_tenant::IdentityVerifier::Hmac {
            secret: TEST_SECRET.to_vec(),
        };
        let wrong = crate::multi_tenant::IdentityVerifier::Hmac {
            secret: b"a-different-secret".to_vec(),
        };
        // No identity → rejected on a normal route.
        assert!(!multiuser_request_authed("/", &empty, &v, now));
        assert!(!multiuser_request_authed(
            "/v1/chat/completions",
            &empty,
            &v,
            now
        ));
        // Health probe is exempt (k8s has no identity to present).
        assert!(multiuser_request_authed("/healthz", &empty, &v, now));
        // Valid signed identity → allowed.
        assert!(multiuser_request_authed(
            "/",
            &headers_for("alice"),
            &v,
            now
        ));
        // Wrong secret → forged → rejected.
        assert!(!multiuser_request_authed(
            "/",
            &headers_for("alice"),
            &wrong,
            now
        ));
        // dev-plan/45 B: a pubkey-configured pod REQUIRES the Ed25519
        // sig — a valid HMAC triple alone no longer passes.
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let ed = crate::multi_tenant::IdentityVerifier::Ed25519 {
            key: sk.verifying_key(),
        };
        assert!(!multiuser_request_authed(
            "/",
            &headers_for("alice"),
            &ed,
            now
        ));
        let mut signed = headers_for("alice");
        let ts: u64 = signed["x-thclaws-user-ts"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        use ed25519_dalek::Signer;
        let sig = sk.sign(format!("alice:{ts}").as_bytes());
        signed.insert(
            "x-thclaws-user-sig",
            crate::multi_tenant::auth::hex_encode(&sig.to_bytes())
                .parse()
                .unwrap(),
        );
        assert!(multiuser_request_authed("/", &signed, &ed, now));
    }

    /// dev-plan/59 Step 2. Single-user `--serve` authenticated nothing:
    /// anything that could reach the port could drive the engine. The guard
    /// has to be opt-in, because every existing deployment runs without it —
    /// The host's own surface gets the same treatment as the classic one:
    /// `/healthz` open for a parent probe, everything else behind the bearer.
    /// `/file-asset` is forwarded to a bot; `/workspace/sync/*` is the host's
    /// own, over the whole workspace.
    /// #209/#210: the desktop's quit path stops agents from the GUI thread,
    /// which cannot await. Signal, then wait by polling, so the host's pipes
    /// outlive the children that log into them.
    #[tokio::test]
    async fn stopping_the_host_waits_for_its_agents() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(crate::bots::bot_dir(dir.path(), "main")).unwrap();
        let program = dir.path().join("stub-engine");
        std::fs::write(&program, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

        let sup = crate::bots::supervisor::BotSupervisor::with_program(dir.path(), &program);
        let bot = sup
            .start(&crate::bots::BotDef {
                slug: "main".into(),
                name: None,
            })
            .unwrap();

        let sup2 = sup.clone();
        let stopped =
            tokio::task::spawn_blocking(move || stop_and_wait(&sup2, Duration::from_secs(10)))
                .await
                .unwrap();
        assert!(stopped, "every agent should stop inside the timeout");
        assert!(matches!(
            bot.state(),
            crate::bots::supervisor::BotState::Stopped
        ));
    }

    #[tokio::test]
    async fn supervisor_router_gates_everything_but_healthz() {
        use tower::ServiceExt as _;
        let _g = crate::kms::test_env_lock();
        let prev = std::env::var("THCLAWS_SERVE_TOKEN").ok();
        let dir = tempfile::tempdir().unwrap();
        let app = supervisor_router(crate::bots::supervisor::BotSupervisor::with_program(
            dir.path(),
            "/nonexistent",
        ));

        async fn status(app: &Router, uri: &str, auth: Option<&str>) -> StatusCode {
            let mut req = axum::http::Request::builder().uri(uri);
            if let Some(a) = auth {
                req = req.header(axum::http::header::AUTHORIZATION, a);
            }
            app.clone()
                .oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        }

        std::env::set_var("THCLAWS_SERVE_TOKEN", "host-secret");
        for uri in [
            "/",
            "/ws",
            "/bots",
            "/bots/research",
            "/file-asset/x.png",
            "/upload",
        ] {
            assert_eq!(
                status(&app, uri, None).await,
                StatusCode::UNAUTHORIZED,
                "{uri}"
            );
        }
        // dev-plan/59 Step 5: the mutating routes install and delete folders,
        // so they must sit behind the same bearer as everything else.
        for (m, uri) in [("POST", "/bots"), ("DELETE", "/bots/research")] {
            let req = axum::http::Request::builder()
                .method(m)
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let got = app.clone().oneshot(req).await.unwrap().status();
            assert_eq!(got, StatusCode::UNAUTHORIZED, "{m} {uri}");
        }
        assert_eq!(status(&app, "/healthz", None).await, StatusCode::OK);
        // dev-plan/60 G3: a host reports busy like a plain serve does. With no
        // agent running, that is not busy — and the fields are there to read.
        let health = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(health.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["role"], "host");
        assert_eq!(body["busy"], false);
        assert_eq!(body["busy_count"], 0);
        // Every bot parked is a host that will never serve on its own;
        // merely starting is not that.
        use crate::bots::supervisor::BotState as S;
        assert!(host_ok(&[]));
        assert!(host_ok(&[S::Starting]));
        assert!(host_ok(&[
            S::CrashLooped { reason: "x".into() },
            S::Starting
        ]));
        assert!(!host_ok(&[S::CrashLooped { reason: "x".into() }]));
        assert!(!host_ok(&[
            S::CrashLooped { reason: "x".into() },
            S::CrashLooped { reason: "y".into() }
        ]));
        // dev-plan/59 §6.3: `?bot=` names a bot; an unknown one is a 404, not
        // a silent fall-through to whichever bot happens to be default.
        assert_eq!(
            status(
                &app,
                "/file-asset/x.png?bot=nope",
                Some("Bearer host-secret")
            )
            .await,
            StatusCode::NOT_FOUND
        );
        assert_ne!(
            status(&app, "/bots", Some("Bearer host-secret")).await,
            StatusCode::UNAUTHORIZED
        );
        // `/file-asset` is forwarded to a bot, so with none running it is
        // "nothing to forward to", not "no such route".
        assert_eq!(
            status(&app, "/file-asset/anything", Some("Bearer host-secret")).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        // A bot's shells are forwarded the same way — a bot with a default
        // shell opened to an empty iframe while the host had no such route.
        for uri in [
            "/gui-shell/book-studio",
            "/gui-shell/book-studio/",
            "/gui-shell/book-studio/app.js",
        ] {
            assert_eq!(
                status(&app, uri, Some("Bearer host-secret")).await,
                StatusCode::SERVICE_UNAVAILABLE,
                "{uri}"
            );
        }
        // …and the bot a shell's sub-asset belongs to rides on the Referer,
        // because `app.js` is fetched relative to a document whose `?bot=`
        // the browser drops from the sub-request.
        let referred = axum::http::Request::builder()
            .uri("/gui-shell/book-studio/app.js")
            .header(axum::http::header::AUTHORIZATION, "Bearer host-secret")
            .header(
                axum::http::header::REFERER,
                "http://127.0.0.1:1/gui-shell/book-studio/?session=x&bot=nope&token=t",
            )
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(referred).await.unwrap().status(),
            StatusCode::NOT_FOUND,
            "the Referer's bot is looked up, and 'nope' is not a bot"
        );
        // dev-plan/60 G5: sync is the host's, over the whole workspace, behind
        // the same bearer as everything else.
        assert_eq!(
            status(&app, "/workspace/sync/stat", None).await,
            StatusCode::UNAUTHORIZED
        );
        let stat = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/workspace/sync/stat")
                    .header(axum::http::header::AUTHORIZATION, "Bearer host-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stat.status(), StatusCode::OK);
        let stat: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(stat.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stat["busy"], false, "no agent is running, so none is busy");
        // dev-plan/60 G6: `/v1` is carried to an agent — with none running that
        // is "nothing to carry it to", not "no such route" — and it is outside
        // the serve bearer, whose token a `/v1` caller does not hold.
        assert_eq!(
            status(&app, "/v1/models", None).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(&app, "/v1/models?bot=nope", None).await,
            StatusCode::NOT_FOUND
        );

        match prev {
            Some(x) => std::env::set_var("THCLAWS_SERVE_TOKEN", x),
            None => std::env::remove_var("THCLAWS_SERVE_TOKEN"),
        }
    }

    /// Route-level proof of the gate, through the real router. The first
    /// cut checked the bearer inside `ws_handler` + `serve_upload` only;
    /// this pins every other route on the port (`/file-asset`, sync,
    /// gui-shell, index) as gated too, `/healthz` as open, and a
    /// multiuser state as skipped.
    #[tokio::test]
    async fn serve_bearer_gates_every_classic_route_except_healthz() {
        use tower::ServiceExt as _;
        let _g = crate::kms::test_env_lock();
        let prev = std::env::var("THCLAWS_SERVE_TOKEN").ok();
        // The sync handlers walk `state.workspace`; keep it an empty dir.
        // One state only — `dummy_state` spawns a shared session (~25 s);
        // the gate reads the env per request, so one router serves both
        // halves.
        let ws = tempfile::tempdir().unwrap();
        let mut st = dummy_state(None);
        st.workspace = std::sync::Arc::new(ws.path().to_path_buf());
        let app = classic_router(st);

        async fn status(app: &Router, method: &str, uri: &str, auth: Option<&str>) -> StatusCode {
            let mut req = axum::http::Request::builder().method(method).uri(uri);
            if let Some(a) = auth {
                req = req.header(axum::http::header::AUTHORIZATION, a);
            }
            app.clone()
                .oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        }

        // Unconfigured: nothing on the port answers 401.
        std::env::remove_var("THCLAWS_SERVE_TOKEN");
        for (m, u) in [
            ("GET", "/file-asset/no-such-file"),
            ("GET", "/workspace/sync/stat"),
            ("GET", "/gui-shell/nope/x.js"),
            ("GET", "/"),
        ] {
            assert_ne!(
                status(&app, m, u, None).await,
                StatusCode::UNAUTHORIZED,
                "{m} {u}"
            );
        }

        std::env::set_var("THCLAWS_SERVE_TOKEN", "route-secret");
        for (m, u) in [
            ("GET", "/"),
            ("GET", "/ws"),
            ("POST", "/upload"),
            ("GET", "/file-asset/no-such-file"),
            ("GET", "/gui-shell/nope/x.js"),
            ("GET", "/workspace/sync/stat"),
            ("GET", "/workspace/sync/pull"),
            ("GET", "/workspace/sync/manifest"),
        ] {
            assert_eq!(
                status(&app, m, u, None).await,
                StatusCode::UNAUTHORIZED,
                "{m} {u}"
            );
            assert_eq!(
                status(&app, m, u, Some("Bearer route-secrex")).await,
                StatusCode::UNAUTHORIZED,
                "wrong token {m} {u}"
            );
        }
        assert_eq!(status(&app, "GET", "/healthz", None).await, StatusCode::OK);
        assert_ne!(
            status(
                &app,
                "GET",
                "/file-asset/no-such-file",
                Some("Bearer route-secret")
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
        assert_ne!(
            status(
                &app,
                "GET",
                "/file-asset/no-such-file?token=route-secret",
                None
            )
            .await,
            StatusCode::UNAUTHORIZED
        );

        // Multiuser: identity comes from `multiuser_auth`, the gate steps
        // aside. `classic_router` hands the gate `multi_tenant.is_none()`,
        // so `false` here is the multiuser shape.
        let mt = Router::new()
            .route("/probe", get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn_with_state(
                false,
                serve_token_gate,
            ));
        assert_eq!(status(&mt, "GET", "/probe", None).await, StatusCode::OK);

        match prev {
            Some(x) => std::env::set_var("THCLAWS_SERVE_TOKEN", x),
            None => std::env::remove_var("THCLAWS_SERVE_TOKEN"),
        }
    }

    /// so "no token configured" must behave exactly as before.
    #[test]
    fn serve_bearer_is_opt_in_and_accepts_header_or_query() {
        let _g = crate::kms::test_env_lock();
        let prev = std::env::var("THCLAWS_SERVE_TOKEN").ok();
        let restore = |v: &Option<String>| match v {
            Some(x) => std::env::set_var("THCLAWS_SERVE_TOKEN", x),
            None => std::env::remove_var("THCLAWS_SERVE_TOKEN"),
        };

        // Unconfigured: everything passes. This is today's behaviour and the
        // reason the change is safe to ship on its own.
        std::env::remove_var("THCLAWS_SERVE_TOKEN");
        assert!(serve_token_ok(&axum::http::HeaderMap::new(), None));
        assert!(serve_token_ok(
            &axum::http::HeaderMap::new(),
            Some("token=anything")
        ));

        std::env::set_var("THCLAWS_SERVE_TOKEN", "s3cret-value");

        // Nothing presented → refused.
        assert!(!serve_token_ok(&axum::http::HeaderMap::new(), None));

        // Header form — what the supervisor's Rust WS client sends.
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer s3cret-value".parse().unwrap(),
        );
        assert!(serve_token_ok(&h, None));

        // Wrong secret in the right shape → refused.
        let mut bad = axum::http::HeaderMap::new();
        bad.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer s3cret-valuf".parse().unwrap(),
        );
        assert!(!serve_token_ok(&bad, None));

        // Query form — a browser WebSocket cannot set headers.
        assert!(serve_token_ok(
            &axum::http::HeaderMap::new(),
            Some("token=s3cret-value")
        ));
        // …including alongside other params, and percent-encoded.
        assert!(serve_token_ok(
            &axum::http::HeaderMap::new(),
            Some("x=1&token=s3cret-value&y=2")
        ));
        assert!(!serve_token_ok(
            &axum::http::HeaderMap::new(),
            Some("token=wrong")
        ));
        // A prefix of the real token must not pass — length is compared too.
        assert!(!serve_token_ok(
            &axum::http::HeaderMap::new(),
            Some("token=s3cret")
        ));

        // Whitespace-only is treated as unset, so a stray export cannot
        // silently lock a user out of their own serve.
        std::env::set_var("THCLAWS_SERVE_TOKEN", "   ");
        assert!(serve_token_ok(&axum::http::HeaderMap::new(), None));

        restore(&prev);
    }

    #[test]
    fn resolve_session_handle_single_tenant_returns_default() {
        let state = dummy_state(None);
        let headers = HeaderMap::new();
        let h = resolve_session_handle(&state, &headers).unwrap();
        // Single-tenant returns the same default handle every call.
        assert!(std::sync::Arc::ptr_eq(&h, &state.shared));
    }

    /// Helper: SharedSessionHandle has no Debug impl so `.unwrap_err()`
    /// (which requires Debug on the Ok type) doesn't compile. Use a
    /// match arm to extract the StatusCode without crossing the type
    /// boundary.
    fn expect_status_err(
        result: Result<std::sync::Arc<SharedSessionHandle>, StatusCode>,
    ) -> StatusCode {
        match result {
            Ok(_) => panic!("expected Err(StatusCode), got Ok(handle)"),
            Err(s) => s,
        }
    }

    #[test]
    fn resolve_session_handle_multi_tenant_rejects_missing_headers() {
        let state = dummy_state(Some(multi_tenant_state()));
        let headers = HeaderMap::new();
        assert_eq!(
            expect_status_err(resolve_session_handle(&state, &headers)),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn resolve_session_handle_multi_tenant_rejects_forged_proof() {
        let state = dummy_state(Some(multi_tenant_state()));
        let mut headers = headers_for("alice");
        headers.insert("x-thclaws-user-proof", "00".parse().unwrap());
        assert_eq!(
            expect_status_err(resolve_session_handle(&state, &headers)),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn resolve_session_handle_routes_different_users_to_different_sessions() {
        let mt = multi_tenant_state();
        let state = dummy_state(Some(mt.clone()));
        let alice = resolve_session_handle(&state, &headers_for("alice")).unwrap();
        let bob = resolve_session_handle(&state, &headers_for("bob")).unwrap();
        assert!(
            !std::sync::Arc::ptr_eq(&alice, &bob),
            "different users → different SharedSessionHandle"
        );
        // Same user reuses the same handle.
        let alice2 = resolve_session_handle(&state, &headers_for("alice")).unwrap();
        assert!(
            std::sync::Arc::ptr_eq(&alice, &alice2),
            "same user → same SharedSessionHandle"
        );
        assert_eq!(mt.registry.active_user_count(), 2);
    }

    #[test]
    fn verify_file_asset_for_user_accepts_own_subtree() {
        let mt = multi_tenant_state();
        assert!(verify_file_asset_for_user(
            &headers_for("alice"),
            &mt,
            "output/users/alice/image.png"
        )
        .is_ok());
        assert!(verify_file_asset_for_user(
            &headers_for("alice"),
            &mt,
            ".thclaws/users/alice/storage/sess.json"
        )
        .is_ok());
    }

    #[test]
    fn verify_file_asset_for_user_rejects_other_user_subtree() {
        let mt = multi_tenant_state();
        let err =
            verify_file_asset_for_user(&headers_for("alice"), &mt, "output/users/bob/image.png")
                .unwrap_err();
        assert_eq!(err, StatusCode::FORBIDDEN);
        let err = verify_file_asset_for_user(
            &headers_for("alice"),
            &mt,
            ".thclaws/users/bob/grants.json",
        )
        .unwrap_err();
        assert_eq!(err, StatusCode::FORBIDDEN);
    }

    #[test]
    fn verify_file_asset_for_user_rejects_shared_subtree() {
        let mt = multi_tenant_state();
        for shared_path in [
            "AGENTS.md",
            "output/shared.png",
            ".thclaws/settings.json",
            "kms/products.md",
        ] {
            let err =
                verify_file_asset_for_user(&headers_for("alice"), &mt, shared_path).unwrap_err();
            assert_eq!(err, StatusCode::FORBIDDEN, "{shared_path}");
        }
    }

    #[test]
    fn verify_file_asset_for_user_rejects_missing_hmac() {
        let mt = multi_tenant_state();
        let err = verify_file_asset_for_user(&HeaderMap::new(), &mt, "output/users/alice/x.png")
            .unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn verify_file_asset_for_user_rejects_forged_hmac() {
        let mt = multi_tenant_state();
        let mut headers = headers_for("alice");
        headers.insert("x-thclaws-user-proof", "00".parse().unwrap());
        let err =
            verify_file_asset_for_user(&headers, &mt, "output/users/alice/x.png").unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }
}
