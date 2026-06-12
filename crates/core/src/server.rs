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
use crate::event_render::{
    render_chat_dispatches, render_gui_shell_dispatch, render_terminal_ansi,
    terminal_data_envelope, terminal_history_replaced_envelope, TerminalRenderState,
};
use crate::ipc::{handle_ipc, IpcContext, PendingAsks};
use crate::providers::provider_has_credentials;
use crate::session::SessionStore;
use crate::shared_session::{SharedSessionHandle, ShellInput, ViewEvent};
use crate::uploads::{
    ensure_target_dir, ensure_uploads_dir, render_upload_message, unique_path, UploadedFile,
    UPLOADS_DIRNAME, UPLOAD_MAX_BYTES, UPLOAD_MAX_FILES,
};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Multipart, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
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
}

impl std::fmt::Debug for MultiTenantMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never log the HMAC secret — accidental leak risk.
        f.debug_struct("MultiTenantMode")
            .field("hmac_secret", &"<redacted>")
            .field("max_users", &self.max_users)
            .field("idle_timeout", &self.idle_timeout)
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
}

/// State derived from [`MultiTenantMode`] at server bootstrap. Held
/// inside [`ServeState`] when multi-tenant mode is enabled; absent
/// otherwise (single-tenant path unchanged).
#[derive(Clone)]
struct MultiTenantState {
    registry: crate::multi_tenant::UserSessionRegistry,
    hmac_secret: Arc<Vec<u8>>,
}

/// Spin up the server. Spawns the worker, builds the Axum router,
/// blocks until the listener returns (Ctrl-C / panic / shutdown).
pub async fn run(config: ServeConfig) -> crate::error::Result<()> {
    // M6.36 SERVE6 hint: keychain access doesn't make sense on a
    // headless server (no user session, often no Secret Service
    // running). Skip the keychain probe by default; users put API
    // keys in `.thclaws/.env` instead. CLI flag override TBD.
    if std::env::var_os("THCLAWS_DISABLE_KEYCHAIN").is_none() {
        std::env::set_var("THCLAWS_DISABLE_KEYCHAIN", "1");
    }

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

    let (approver, _approval_rx) = crate::permissions::GuiApprover::new();
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

    run_with_engine(config, approver, shared, pending_asks, ask_broadcast).await
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
) -> crate::error::Result<()> {
    let workspace = match config.workspace.clone() {
        Some(p) => p,
        None => std::env::current_dir()
            .map_err(|e| crate::error::Error::Tool(format!("workspace cwd unavailable: {e}")))?,
    };
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
            });
        // Sweep every 30s — fine for 30m default idle_timeout, will
        // need re-tuning if Tier 3 wants sub-minute sessions.
        let _evictor = registry.spawn_evictor(std::time::Duration::from_secs(30));
        eprintln!(
            "\x1b[36m[serve] multi-tenant on — max_users={}, idle_timeout={:?}\x1b[0m",
            cfg.max_users, cfg.idle_timeout
        );
        MultiTenantState {
            registry,
            hmac_secret: Arc::new(cfg.hmac_secret.clone()),
        }
    });
    let ws_connections = Arc::new(AtomicUsize::new(0));
    let state = ServeState {
        shared,
        approver,
        pending_asks,
        ask_broadcast,
        workspace: Arc::new(workspace),
        multi_tenant: multi_tenant_state,
        ws_connections: ws_connections.clone(),
    };

    // Cloud heartbeat: when running inside a thclaws.cloud workspace
    // pod, periodically POST to `/api/hosted/workspaces/<id>/keepalive`
    // while at least one browser tab is connected. The provisioner
    // injects THCLAWS_CLOUD_URL / THCLAWS_CLOUD_TOKEN / THCLAWS_WORKSPACE_ID
    // at provision time. Outside cloud (any env var missing), this is
    // a no-op — local CLI / desktop GUI runs don't need it.
    spawn_cloud_heartbeat(ws_connections);

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
        // Mode C (cloud serve / browser SSH-tunnel): the React webapp
        // hosts gui-shells in iframes. Browser has no `thclaws://`
        // protocol handler, so expose the shell folders over HTTP
        // under `/gui-shell/<id>/...`. The bridge runs in postMessage
        // mode (inline-injected) — no per-shell WS.
        Router::new()
            .route("/", get(serve_index))
            .route("/healthz", get(serve_health))
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
            .with_state(state)
            .merge(crate::api_v1::router())
    };

    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .map_err(|e| crate::error::Error::Tool(format!("bind {}: {e}", config.bind)))?;
    if config.gui_shell.is_none() {
        eprintln!(
            "\x1b[36m[serve] thClaws listening on http://{}\x1b[0m",
            config.bind
        );
        eprintln!("\x1b[36m[serve] open the URL above in your browser (over an SSH tunnel for remote access)\x1b[0m");
    }
    axum::serve(listener, app)
        .await
        .map_err(|e| crate::error::Error::Tool(format!("serve: {e}")))?;
    Ok(())
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
                        crate::gui_shell::serve::serve_project_asset(workspace.as_ref(), &rel)
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
fn spawn_cloud_heartbeat(connections: Arc<AtomicUsize>) {
    let url = match std::env::var("THCLAWS_CLOUD_URL") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return,
    };
    let token = match std::env::var("THCLAWS_CLOUD_TOKEN") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return,
    };
    let workspace_id = match std::env::var("THCLAWS_WORKSPACE_ID") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return,
    };
    let endpoint = format!(
        "{}/api/hosted/workspaces/{}/keepalive",
        url.trim_end_matches('/'),
        workspace_id
    );
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
            if !connected && !busy {
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
                .json(&serde_json::json!({ "busy": busy }))
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

async fn serve_health() -> impl IntoResponse {
    "ok"
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
async fn serve_file_asset(axum::extract::Path(rel): axum::extract::Path<String>) -> Response {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::gui_shell::serve::serve_project_asset(&cwd, &rel)
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
) -> impl IntoResponse {
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
    let user_id = crate::multi_tenant::verify_user_header(
        user_id_h,
        ts_h,
        proof_h,
        &mt.hmac_secret,
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
    let user_id = crate::multi_tenant::verify_user_header(
        user_id_h,
        ts_h,
        proof_h,
        &mt.hmac_secret,
        now_secs,
    )
    .map_err(|e| {
        eprintln!("\x1b[33m[serve] HMAC rejected: {e}\x1b[0m");
        StatusCode::UNAUTHORIZED
    })?;
    let session = mt.registry.get_or_spawn(&user_id);
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
        on_quit: Arc::new(|| {
            eprintln!(
                "\x1b[36m[serve] frontend requested app_close — closing WS connection\x1b[0m"
            );
        }),
        on_send_initial_state: Arc::new(move || {
            let payload = build_initial_state_payload();
            let _ = initial_dispatch(payload);
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
                // silence).
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            }
        }
    });

    // M6.36 SERVE3: subscribe to the broadcast and translate every
    // ViewEvent into chat-shaped + terminal-shaped envelopes, identical
    // to gui::spawn_event_translator's path. Both translators feed the
    // same outbound channel so the writer task serializes WS writes.
    // dev-plan/35 Tier 1: subscribe to the RESOLVED handle (per-user
    // in multi-tenant; default in single-tenant). Critical for
    // isolation — without this, every user's translator would
    // subscribe to the default handle and see everyone's events.
    let mut events_rx = shared.subscribe();
    let event_tx = out_tx.clone();
    let event_forwarder = tokio::spawn(async move {
        let mut term_state = TerminalRenderState::default();
        loop {
            match events_rx.recv().await {
                Ok(ev) => {
                    // QuitRequested is a worker-side signal that the
                    // user typed `/quit` — we close the WS so the
                    // browser sees the disconnect and can decide what
                    // to do next (today: nothing; future: snapshot
                    // re-fetch on reconnect handles state).
                    if matches!(ev, ViewEvent::QuitRequested) {
                        break;
                    }
                    for dispatch in render_chat_dispatches(&ev) {
                        if event_tx.send(dispatch).is_err() {
                            return;
                        }
                    }
                    // dev-plan/33 Tier 2 Mode B: emit gui_shell_event
                    // envelopes so a shell's bridge runtime can consume
                    // streamed text/done/error events over the same WS.
                    // The browser-side bridge filters by `event` and
                    // ignores chat_*/terminal_* envelopes meant for
                    // the React frontend (which isn't loaded in Mode B
                    // anyway, but staying symmetric keeps the gui+serve
                    // combo path working too).
                    if let Some(dispatch) = render_gui_shell_dispatch(&ev) {
                        if event_tx.send(dispatch).is_err() {
                            return;
                        }
                    }
                    if let Some(ansi) = render_terminal_ansi(&mut term_state, &ev) {
                        let envelope = if matches!(ev, ViewEvent::HistoryReplaced(_)) {
                            terminal_history_replaced_envelope(&ansi)
                        } else {
                            terminal_data_envelope(&ansi)
                        };
                        if event_tx.send(envelope).is_err() {
                            return;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Slow consumer dropped events; ignore and resume
                    // — Phase 1A reconnect-with-snapshot-replay will
                    // re-sync state on next ws drop.
                    continue;
                }
                Err(_) => break,
            }
        }
    });

    // Outbound writer task — serializes every payload to the WS sink.
    let writer = tokio::spawn(async move {
        while let Some(payload) = out_rx.recv().await {
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
/// Auto-fallback model: if the saved model's provider has no
/// credentials but another provider does, switch + persist so the
/// "ready" indicator in the sidebar is accurate after the user adds
/// a key.
fn build_initial_state_payload() -> String {
    let mut config = AppConfig::load().unwrap_or_default();
    if let Some(new_model) = crate::providers::auto_fallback_model(&config) {
        let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
        project.set_model(&new_model);
        let _ = project.save();
        config = AppConfig::load().unwrap_or_default();
    }
    let provider_name = config.detect_provider().unwrap_or("unknown");
    let provider_ready = provider_has_credentials(&config);
    // Consult the live MCP_TOOL_COUNTS cache (populated by the
    // McpReady worker event) so reconnect-after-startup ships real
    // counts instead of the hardcoded zeros that surfaced as issue #86.
    let mcp_servers = crate::gui::build_mcp_servers_payload(&config);
    let sessions: Vec<serde_json::Value> = SessionStore::default_path()
        .map(SessionStore::new)
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
    // #95(c): on WS open the frontend's mount-time `team_enabled_get`
    // request can be dropped if the socket is still CONNECTING — the
    // wsSend guard logs and discards (frontend/src/hooks/useIPC.ts:114).
    // Ship the flag here so every (re)connect-driven initial_state
    // carries it and the Team tab heals automatically without the user
    // having to open Settings to incidentally refire the get.
    let team_enabled = crate::config::ProjectConfig::load()
        .and_then(|c| c.team_enabled)
        .unwrap_or(false);
    serde_json::json!({
        "type": "initial_state",
        "provider": provider_name,
        "model": config.model,
        "provider_ready": provider_ready,
        "mcp_servers": mcp_servers,
        "sessions": sessions,
        "kmss": kmss,
        "team_enabled": team_enabled,
        "version": crate::version::VERSION,
    })
    .to_string()
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
        let scope = match kref.scope {
            crate::kms::KmsScope::Project => "project",
            crate::kms::KmsScope::User => "user",
        };
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

        // Bind to an OS-assigned port so concurrent test runs don't
        // collide. We pre-bind a TcpListener to discover the port,
        // then drop the listener and let server::run rebind. Tiny
        // window for race; in practice fine for unit tests.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let cfg = ServeConfig {
            bind: addr,
            ..Default::default()
        };
        let server_handle = tokio::spawn(async move {
            let _ = run(cfg).await;
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
            serde_json::json!({"type": "shell_input", "text": "/help"}).to_string(),
        ))
        .await
        .expect("ws send shell_input");

        // Drain frames for up to 3s collecting `type` values; assert
        // the canonical sequence shows up.
        let mut seen: Vec<String> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), ws.next()).await {
                Ok(Some(Ok(WsMessage::Text(text)))) => {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text.as_str()) {
                        if let Some(t) = parsed.get("type").and_then(|v| v.as_str()) {
                            seen.push(t.to_string());
                            if t == "chat_done" {
                                break;
                            }
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

        // Clean shutdown.
        let _ = ws.send(WsMessage::Close(None)).await;
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
        assert_eq!(json["files"][0]["path"], "uploads/photo.jpg");
        assert_eq!(json["files"][0]["size"], 16);

        assert!(td.path().join("uploads").join("photo.jpg").exists());

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
        assert_eq!(json2["files"][0]["path"], "uploads/photo_1.jpg");
        assert!(td.path().join("uploads").join("photo_1.jpg").exists());

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
            });
        MultiTenantState {
            registry,
            hmac_secret: std::sync::Arc::new(TEST_SECRET.to_vec()),
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
