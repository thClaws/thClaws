//! OpenAI-compatible HTTP API surface for `--serve` mode.
//!
//! Additive to the existing webapp routes — see [`crate::server`] for
//! the canonical router and trust model. Endpoints under `/v1/*`:
//!
//! - `GET  /v1/models`            list available model ids
//! - `POST /v1/chat/completions`  single-turn or streaming chat (S3+S4)
//! - `POST /v1/messages`          the same, in Anthropic Messages shape
//!
//! Anything any tool that speaks the OpenAI Chat Completions API can
//! drive thClaws this way — LiteLLM, openai-python SDK, Cursor's custom
//! provider, aider, n8n, etc. See `dev-plan/19-thclaws-openai-compat.md`
//! for the rationale + full scope. `/v1/messages` is the same agent
//! behind the Anthropic Messages wire shape, for the `anthropic` SDKs
//! and anything pointed at a custom `ANTHROPIC_BASE_URL`.

use axum::extract::{DefaultBodyLimit, FromRequestParts};
use axum::http::{request::Parts, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;

pub mod agent;
pub mod artifacts;
pub mod callback;
pub mod chat;
pub mod deploy;
pub mod errors;
pub mod guard;
pub mod info;
pub mod messages;
pub mod models;
pub mod oauth_callback;

/// Build the `/v1/*` sub-router. Mounted into the main server router
/// by [`crate::server::run_with_engine`].
///
/// Routes:
/// - `GET  /v1/models`            — OpenAI-compatible model listing.
/// - `POST /v1/chat/completions`  — OpenAI-compatible chat (sync + SSE + x_callback).
/// - `POST /v1/messages`          — Anthropic Messages-compatible chat (sync + SSE).
/// - `POST /agent/run`            — thClaws-native agent endpoint with
///   per-request `workspace_dir` for skill / MCP / plugin scoping
///   (see `dev-plan/25-thclaws-as-agent.md`).
/// - `GET  /v1/agent/info`        — read-only capability snapshot for
///   orchestrators that treat this daemon as a sovereign freelancer
///   (see `dev-plan/26-thclaws-pod-as-freelancer.md`).
pub fn router() -> Router {
    // Deploy endpoints accept up to 100 MB tarballs (matches the
    // dev-plan/28 bundle quota). Default axum body limit is 2 MB —
    // raise it just on the deploy routes so unrelated endpoints stay
    // protected from giant payloads.
    const DEPLOY_BODY_LIMIT_BYTES: usize = 100 * 1024 * 1024;
    let deploy_routes = Router::new()
        .route("/v1/deploy", post(deploy::deploy_files))
        .route("/v1/deploy/files", post(deploy::deploy_files))
        .route("/v1/deploy/manifest", post(deploy::deploy_manifest))
        .layer(DefaultBodyLimit::max(DEPLOY_BODY_LIMIT_BYTES));

    loopback_router()
        .route("/v1/restart", post(deploy::restart))
        .merge(deploy_routes)
}

/// The routes the always-on desktop bridge serves: everything except
/// `/v1/deploy*` and `/v1/restart`, which replace a pod's `.thclaws/` and
/// kill the process — orchestrator verbs for a `--serve` pod, not
/// anything a local MCP server needs from a desktop session.
fn loopback_router() -> Router {
    Router::new()
        .route("/v1/models", get(models::list_models))
        .route("/v1/chat/completions", post(chat::chat_completions))
        .route("/v1/messages", post(messages::messages))
        .route("/agent/run", post(agent::agent_run))
        // Session artifacts (job-artifacts): frozen, hash-fixed outputs of a
        // run + input placement for external orchestrators. Bearer-gated
        // like the rest of /v1 — the whole point.
        .route("/v1/sessions/{sid}/artifacts", get(artifacts::get_manifest))
        .route(
            "/v1/sessions/{sid}/artifacts/{aid}",
            get(artifacts::get_artifact),
        )
        .route(
            "/v1/inputs",
            post(artifacts::post_inputs)
                .layer(DefaultBodyLimit::max(artifacts::INPUTS_BODY_LIMIT_BYTES)),
        )
        .route("/v1/agent/info", get(info::get_info))
        // OAuth callback is intentionally public — the provider's
        // redirect won't carry the Bearer token. State-nonce match
        // is the auth, not a header. See oauth_callback.rs.
        .route("/v1/oauth/callback", get(oauth_callback::oauth_callback))
}

/// Bearer-token extractor enforcing [`auth_token`] policy. Returned by
/// every `/v1/*` handler that needs an authenticated request.
///
/// Three modes:
///   - `THCLAWS_API_TOKEN` unset → API disabled, returns 404. (Disabled
///     paths still register but reject before the handler runs.)
///   - `THCLAWS_API_TOKEN=disable-auth` → no header required. Refused
///     unless the listener is loopback-bound (checked at server start,
///     not per-request).
///   - `THCLAWS_API_TOKEN=<value>` → request must carry
///     `Authorization: Bearer <value>` with constant-time match.
pub struct AuthOk;

impl<S: Send + Sync> FromRequestParts<S> for AuthOk {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let expected = match auth_token() {
            AuthMode::Disabled => {
                return Err((StatusCode::NOT_FOUND, "api disabled").into_response());
            }
            AuthMode::Bypass => return Ok(AuthOk),
            AuthMode::Token(t) => t,
        };
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();
        let provided = header.strip_prefix("Bearer ").unwrap_or("");
        if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
            Ok(AuthOk)
        } else {
            Err((
                StatusCode::UNAUTHORIZED,
                Json(errors::OpenAiError::invalid_api_key()),
            )
                .into_response())
        }
    }
}

/// Header-level Bearer check for surfaces OUTSIDE `/v1` that opt into the
/// same token policy — Tier 1 of job-artifacts: `THCLAWS_SYNC_REQUIRE_AUTH=1`
/// wraps `/workspace/sync/*` with this so an external orchestrator can use
/// export/push holding only the API token, no tunnel/ForwardAuth needed.
pub fn check_bearer_headers(headers: &axum::http::HeaderMap) -> Result<(), Response> {
    let expected = match auth_token() {
        AuthMode::Disabled => {
            return Err((
                StatusCode::UNAUTHORIZED,
                "sync auth required but THCLAWS_API_TOKEN is unset",
            )
                .into_response());
        }
        AuthMode::Bypass => return Ok(()),
        AuthMode::Token(t) => t,
    };
    let header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    let provided = header.strip_prefix("Bearer ").unwrap_or("");
    if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(errors::OpenAiError::invalid_api_key()),
        )
            .into_response())
    }
}

enum AuthMode {
    /// No `THCLAWS_API_TOKEN` set — `/v1/*` returns 404.
    Disabled,
    /// `THCLAWS_API_TOKEN=disable-auth` literal — accept any request.
    /// Only safe when bound to loopback; enforced at server start.
    Bypass,
    /// `THCLAWS_API_TOKEN=<value>` — require Bearer match.
    Token(String),
}

fn auth_token() -> AuthMode {
    match std::env::var("THCLAWS_API_TOKEN") {
        Err(_) => AuthMode::Disabled,
        Ok(t) if t == "disable-auth" => AuthMode::Bypass,
        Ok(t) if t.is_empty() => AuthMode::Disabled,
        Ok(t) => AuthMode::Token(t),
    }
}

/// Returns `true` when the `THCLAWS_API_TOKEN` env is set to something
/// other than `disable-auth`. Used by the serve startup to enforce that
/// `disable-auth` only runs on loopback binds.
pub fn api_enabled() -> bool {
    !matches!(auth_token(), AuthMode::Disabled)
}

/// Returns `true` when the bypass token is in use.
pub fn auth_is_bypassed() -> bool {
    matches!(auth_token(), AuthMode::Bypass)
}

/// Default loopback bind for the always-on `/v1/*` listener. The
/// fixed port is the seam separately-spawned MCP-Apps servers (e.g.
/// `thclaws-gamedev-mcp` running over HTTP transport on its own port)
/// use to reach the user's authenticated provider — they don't share
/// a process with us, so they can't inherit our env. A random port
/// would force the operator to glue ports together by hand every
/// restart; a fixed, documented one means `GamedevAiMove` just works.
pub const LOOPBACK_DEFAULT_PORT: u16 = 18443;

/// Override env for the fixed port — for the rare case of a host that
/// already has 18443 in use, or two thClaws instances on one box.
pub const LOOPBACK_PORT_ENV: &str = "THCLAWS_LOOPBACK_PORT";

/// Override env for the bind interface. Default is `127.0.0.1` which
/// keeps the listener loopback-only. Docker / WSL setups where the
/// out-of-process MCP server runs in a sibling VM need to set this to
/// `0.0.0.0` so connections via `host.docker.internal` (the host's
/// non-loopback IP from the container's POV) land. The bearer is still
/// required; only the loopback Host check is dropped.
pub const LOOPBACK_BIND_ENV: &str = "THCLAWS_LOOPBACK_BIND";

/// Exported to every child process once the bridge is up, beside
/// `THCLAWS_API_TOKEN`, so an MCP server we spawn finds both.
pub const LOOPBACK_URL_ENV: &str = "THCLAWS_LOOPBACK_URL";

/// `~/.config/<app>/loopback.json` (`{url, token}`, mode 0600): how a
/// local client this process did NOT spawn (an HTTP MCP server started by
/// hand, a script) learns the per-launch token.
pub const LOOPBACK_FILE: &str = "loopback.json";

/// The bearer the bridge requires: the operator's own `THCLAWS_API_TOKEN`
/// when one is set, else a fresh random one. `disable-auth` is not honoured
/// here — this listener is always on, so it is never allowed to be open.
fn loopback_token(existing: Option<&str>) -> std::io::Result<String> {
    match existing.map(str::trim) {
        Some(t) if !t.is_empty() && t != "disable-auth" => Ok(t.to_string()),
        _ => {
            let mut bytes = [0u8; 32];
            getrandom::getrandom(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
            Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
        }
    }
}

fn write_loopback_file(url: &str, token: &str) {
    let Some(dir) = crate::profile::config_dir() else {
        return;
    };
    let path = dir.join(LOOPBACK_FILE);
    let body = serde_json::json!({ "url": url, "token": token }).to_string();
    let res = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let _ = std::fs::remove_file(&path);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        use std::io::Write;
        opts.open(&path)?.write_all(body.as_bytes())
    })();
    if let Err(e) = res {
        eprintln!(
            "\x1b[33m[api_v1 loopback] could not write {}: {e}\x1b[0m",
            path.display()
        );
    }
}

/// Bind the always-on loopback `/v1/*` listener. Resolves the port
/// from `$THCLAWS_LOOPBACK_PORT` or falls back to
/// [`LOOPBACK_DEFAULT_PORT`].
///
/// Every request needs `Authorization: Bearer <token>`, a per-launch
/// random token (or the operator's `THCLAWS_API_TOKEN`). The token is put
/// in this process's environment before any child is spawned, so MCP
/// servers, agents and tools we start inherit it with
/// [`LOOPBACK_URL_ENV`]; anything else reads [`LOOPBACK_FILE`]. A
/// loopback Host is required and a foreign browser Origin refused, which
/// closes DNS rebinding and cross-site requests.
///
/// Not started when org policy says `allow_serve: false`.
///
/// Run once at startup. Logs the bound URL to stderr; returns it so the
/// caller can stash it for in-process clients. Idempotent — a second
/// call is a no-op and returns the existing URL.
///
/// Errors are surfaced for the caller to log+ignore: a failed bind
/// (port collision) should NOT abort startup, because MCP-Apps widgets
/// that don't need the bridge keep working without it.
pub async fn spawn_loopback() -> std::io::Result<String> {
    use std::sync::OnceLock;
    static LOOPBACK_URL: OnceLock<String> = OnceLock::new();
    if let Some(url) = LOOPBACK_URL.get() {
        return Ok(url.clone());
    }
    if !crate::policy::serve_allowed() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "disabled by org policy (policies.runtime.allow_serve = false)",
        ));
    }

    let port: u16 = std::env::var(LOOPBACK_PORT_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(LOOPBACK_DEFAULT_PORT);
    let bind_host = std::env::var(LOOPBACK_BIND_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let public_bind = !bind_host
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(bind_host.eq_ignore_ascii_case("localhost"));

    let token = loopback_token(std::env::var("THCLAWS_API_TOKEN").ok().as_deref())?;
    std::env::set_var("THCLAWS_API_TOKEN", &token);

    let bind = format!("{bind_host}:{port}");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    // Advertise via 127.0.0.1 in the URL even when bound to 0.0.0.0 —
    // in-process callers always want loopback, and the env var
    // override is purely for in-from-the-container reach.
    let url = format!("http://127.0.0.1:{port}");
    let _ = LOOPBACK_URL.set(url.clone());
    std::env::set_var(LOOPBACK_URL_ENV, &url);
    write_loopback_file(&url, &token);

    let app = if public_bind {
        loopback_router()
    } else {
        loopback_router().layer(axum::middleware::from_fn(guard::v1_loopback_guard))
    };
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("\x1b[33m[api_v1 loopback] serve error: {e}\x1b[0m");
        }
    });

    eprintln!(
        "\x1b[36m[api_v1 loopback] /v1/* available at {url} (bearer required: $THCLAWS_API_TOKEN in child processes, {LOOPBACK_FILE} in the config dir)\x1b[0m"
    );
    Ok(url)
}

/// The token policy, flattened for surfaces that do their own header
/// parsing: `None` = API disabled, `Some(None)` = bypass,
/// `Some(Some(t))` = require `t`. Keeps [`AuthMode`] private while
/// letting `/v1/messages` accept `x-api-key` without duplicating the
/// env-var rules.
fn auth_token_for_messages() -> Option<Option<String>> {
    match auth_token() {
        AuthMode::Disabled => None,
        AuthMode::Bypass => Some(None),
        AuthMode::Token(t) => Some(Some(t)),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// `THCLAWS_API_TOKEN` is process-wide, so the tests that set it must
/// not run at the same time as the tests that read it. Held across each
/// such test in this module and in `info`.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_only_equal_bytes() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hell"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn loopback_token_never_bypasses() {
        assert_eq!(loopback_token(Some("op-secret")).unwrap(), "op-secret");
        for open in [None, Some(""), Some("  "), Some("disable-auth")] {
            let t = loopback_token(open).unwrap();
            assert_eq!(t.len(), 64, "{open:?}");
            assert_ne!(t, "disable-auth");
        }
        assert_ne!(loopback_token(None).unwrap(), loopback_token(None).unwrap());
    }

    #[test]
    fn auth_token_distinguishes_three_modes() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("THCLAWS_API_TOKEN").ok();

        std::env::remove_var("THCLAWS_API_TOKEN");
        assert!(matches!(auth_token(), AuthMode::Disabled));

        std::env::set_var("THCLAWS_API_TOKEN", "");
        assert!(matches!(auth_token(), AuthMode::Disabled));

        std::env::set_var("THCLAWS_API_TOKEN", "disable-auth");
        assert!(matches!(auth_token(), AuthMode::Bypass));

        std::env::set_var("THCLAWS_API_TOKEN", "secret-xyz");
        assert!(matches!(auth_token(), AuthMode::Token(t) if t == "secret-xyz"));

        match prior {
            Some(v) => std::env::set_var("THCLAWS_API_TOKEN", v),
            None => std::env::remove_var("THCLAWS_API_TOKEN"),
        }
    }
}
