//! OpenAI-compatible HTTP API surface for `--serve` mode.
//!
//! Additive to the existing webapp routes — see [`crate::server`] for
//! the canonical router and trust model. Endpoints under `/v1/*`:
//!
//! - `GET  /v1/models`            list available model ids
//! - `POST /v1/chat/completions`  single-turn or streaming chat (S3+S4)
//!
//! Anything any tool that speaks the OpenAI Chat Completions API can
//! drive thClaws this way — LiteLLM, openai-python SDK, Cursor's custom
//! provider, aider, n8n, etc. See `dev-plan/19-thclaws-openai-compat.md`
//! for the rationale + full scope.

use axum::extract::FromRequestParts;
use axum::http::{request::Parts, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;

pub mod callback;
pub mod chat;
pub mod errors;
pub mod models;

/// Build the `/v1/*` sub-router. Mounted into the main server router
/// by [`crate::server::run_with_engine`].
pub fn router() -> Router {
    Router::new()
        .route("/v1/models", get(models::list_models))
        .route("/v1/chat/completions", post(chat::chat_completions))
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

/// Bind the always-on loopback `/v1/*` listener. Resolves the port
/// from `$THCLAWS_LOOPBACK_PORT` or falls back to
/// [`LOOPBACK_DEFAULT_PORT`]. Auth is forced to `disable-auth` because
/// the listener is loopback-only — the safety boundary is the bind
/// address, not a bearer token an out-of-process MCP server would need
/// to discover from us anyway.
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

    let port: u16 = std::env::var(LOOPBACK_PORT_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(LOOPBACK_DEFAULT_PORT);

    // Force disable-auth so out-of-process clients on the same host
    // (HTTP-transport MCP servers, host scripts) don't need to learn a
    // per-process bearer token to reach our /v1 surface. Safety is
    // anchored to the loopback bind, not the token.
    if std::env::var("THCLAWS_API_TOKEN")
        .map(|v| v != "disable-auth")
        .unwrap_or(true)
    {
        std::env::set_var("THCLAWS_API_TOKEN", "disable-auth");
    }

    let bind = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let url = format!("http://{bind}");
    let _ = LOOPBACK_URL.set(url.clone());

    let app = axum::Router::new().merge(router());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("\x1b[33m[api_v1 loopback] serve error: {e}\x1b[0m");
        }
    });

    eprintln!(
        "\x1b[36m[api_v1 loopback] /v1/* available at {url} for out-of-process MCP servers\x1b[0m"
    );
    Ok(url)
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
    fn auth_token_distinguishes_three_modes() {
        // Order matters — env is process-wide and these tests can't run
        // in parallel without locks. Reading + restoring around each
        // assertion keeps the suite passable when run as a unit.
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
