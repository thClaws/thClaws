//! Request guards for the loopback listeners: the always-on `/v1`
//! bridge and a loopback-bound `--serve`.
//!
//! A loopback bind stops other machines, not other web pages: a page the
//! user visits can resolve its own hostname to 127.0.0.1 (DNS rebinding)
//! or fire a cross-site "simple" request at the port. Both carry a Host or
//! Origin that is not loopback, so refusing those closes the browser path
//! while every local client (which sends `Host: 127.0.0.1:<port>`) passes.

use axum::extract::Request;
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::SocketAddr;

/// Hostname part of a `Host` header (`127.0.0.1:18443` → `127.0.0.1`,
/// `[::1]:80` → `[::1]`), lowercased.
fn host_name(host: &str) -> String {
    let host = host.trim();
    let name = if host.starts_with('[') {
        match host.find(']') {
            Some(i) => &host[..=i],
            None => host,
        }
    } else {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    name.to_ascii_lowercase()
}

fn is_loopback_name(name: &str) -> bool {
    matches!(name, "127.0.0.1" | "localhost" | "[::1]")
}

/// `Host` names a loopback address. A request with no Host at all is let
/// through: every browser sends one, so its absence is not the rebinding
/// path.
pub fn host_is_loopback(headers: &HeaderMap) -> bool {
    match headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        None => true,
        Some(h) => is_loopback_name(&host_name(h)),
    }
}

/// `Origin` is absent (a non-browser client, or a top-level navigation) or
/// an http(s) loopback origin. `allow_null` admits `Origin: null`, which
/// sandboxed iframes of our own pages send.
pub fn origin_is_loopback(headers: &HeaderMap, allow_null: bool) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let origin = origin.trim();
    if origin == "null" {
        return allow_null;
    }
    let rest = match origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    {
        Some(r) => r,
        None => return false,
    };
    is_loopback_name(&host_name(rest.split('/').next().unwrap_or("")))
}

fn forbidden(what: &str, path: &str) -> Response {
    eprintln!("\x1b[33m[serve] {path} rejected: non-loopback {what}\x1b[0m");
    (StatusCode::FORBIDDEN, format!("non-loopback {what}")).into_response()
}

/// Layer for the always-on `/v1` bridge: loopback Host, and no browser
/// Origin other than a loopback page's.
pub async fn v1_loopback_guard(req: Request, next: Next) -> Response {
    if !host_is_loopback(req.headers()) {
        return forbidden("Host", req.uri().path());
    }
    if !origin_is_loopback(req.headers(), false) {
        return forbidden("Origin", req.uri().path());
    }
    next.run(req).await
}

/// `Origin: null` is what a sandboxed gui-shell frame sends for its asset
/// GETs, and also what any web page's own sandboxed iframe sends — so it
/// earns nothing beyond a plain read: no WebSocket upgrade, no write.
fn null_origin_allowed(req: &Request) -> bool {
    matches!(
        *req.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    ) && !req.headers().contains_key(header::UPGRADE)
}

/// Layer for a loopback-bound `--serve` / workspace host.
pub async fn serve_loopback_guard(req: Request, next: Next) -> Response {
    if !host_is_loopback(req.headers()) {
        return forbidden("Host", req.uri().path());
    }
    if !origin_is_loopback(req.headers(), null_origin_allowed(&req)) {
        return forbidden("Origin", req.uri().path());
    }
    next.run(req).await
}

/// Hosted runners bind 0.0.0.0 behind the cloud ingress, which authenticates
/// every request (ForwardAuth) before it reaches the pod; the pod port is
/// not reachable otherwise. `KUBERNETES_SERVICE_HOST` is set in every k8s
/// pod, `THCLAWS_HOSTED=1` by the docker provisioner, and
/// `THCLAWS_SERVE_BEHIND_PROXY=1` is the explicit opt-in for anyone else
/// running an authenticating reverse proxy.
fn behind_authenticating_proxy() -> bool {
    let set = |k: &str| std::env::var(k).is_ok_and(|v| !v.trim().is_empty());
    set("KUBERNETES_SERVICE_HOST")
        || std::env::var("THCLAWS_HOSTED").ok().as_deref() == Some("1")
        || std::env::var("THCLAWS_SERVE_BEHIND_PROXY").ok().as_deref() == Some("1")
}

fn serve_token_set() -> bool {
    std::env::var("THCLAWS_SERVE_TOKEN").is_ok_and(|v| !v.trim().is_empty())
}

/// Why a `--serve` on `bind` must not start, if it must not. `own_auth` is
/// a mode that authenticates every request itself (multiuser HMAC, a
/// gui-shell's own token).
pub fn public_bind_refusal(bind: &SocketAddr, own_auth: bool) -> Option<String> {
    public_bind_refusal_with(
        bind,
        own_auth,
        serve_token_set(),
        behind_authenticating_proxy(),
    )
}

fn public_bind_refusal_with(
    bind: &SocketAddr,
    own_auth: bool,
    token_set: bool,
    behind_proxy: bool,
) -> Option<String> {
    if bind.ip().is_loopback() || own_auth || token_set || behind_proxy {
        return None;
    }
    Some(format!(
        "refusing to serve on {bind} without authentication: anyone who can reach this \
         address would get full control of the agent and the workspace.\n  \
         Set THCLAWS_SERVE_TOKEN to a long random secret (e.g. `export \
         THCLAWS_SERVE_TOKEN=$(openssl rand -hex 32)`) and open the URL with \
         `?token=<secret>`, or bind to 127.0.0.1 and use an SSH tunnel.\n  \
         Behind a reverse proxy that authenticates every request, set \
         THCLAWS_SERVE_BEHIND_PROXY=1."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(host: Option<&str>, origin: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = host {
            h.insert(header::HOST, HeaderValue::from_str(v).unwrap());
        }
        if let Some(v) = origin {
            h.insert(header::ORIGIN, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn null_origin_reads_only() {
        // Re-audit N3: any page's sandboxed iframe sends `Origin: null`.
        let req = |method: &str, upgrade: bool| {
            let mut b = axum::http::Request::builder()
                .method(method)
                .uri("/ws")
                .header(header::ORIGIN, "null");
            if upgrade {
                b = b.header(header::UPGRADE, "websocket");
            }
            b.body(axum::body::Body::empty()).unwrap()
        };
        assert!(null_origin_allowed(&req("GET", false)));
        assert!(!null_origin_allowed(&req("GET", true)));
        assert!(!null_origin_allowed(&req("POST", false)));
        assert!(!null_origin_allowed(&req("PUT", false)));
    }

    #[test]
    fn guard_host_accepts_loopback_only() {
        for ok in [
            "127.0.0.1:18443",
            "127.0.0.1",
            "localhost:8443",
            "LOCALHOST",
            "[::1]:18443",
            "[::1]",
        ] {
            assert!(host_is_loopback(&headers(Some(ok), None)), "{ok}");
        }
        for bad in [
            "evil.example:18443",
            "127.0.0.1.evil.example",
            "localhost.evil.example:18443",
            "10.0.0.5:8443",
            "[::2]:18443",
        ] {
            assert!(!host_is_loopback(&headers(Some(bad), None)), "{bad}");
        }
        assert!(host_is_loopback(&headers(None, None)));
    }

    #[test]
    fn guard_origin_rejects_foreign_pages() {
        assert!(origin_is_loopback(&headers(None, None), false));
        assert!(origin_is_loopback(
            &headers(None, Some("http://127.0.0.1:5173")),
            false
        ));
        assert!(origin_is_loopback(
            &headers(None, Some("http://localhost")),
            false
        ));
        assert!(!origin_is_loopback(
            &headers(None, Some("https://evil.example")),
            false
        ));
        assert!(!origin_is_loopback(
            &headers(None, Some("http://127.0.0.1.evil.example")),
            false
        ));
        assert!(!origin_is_loopback(
            &headers(None, Some("thclaws://localhost")),
            false
        ));
        assert!(!origin_is_loopback(&headers(None, Some("null")), false));
        assert!(origin_is_loopback(&headers(None, Some("null")), true));
    }

    #[test]
    fn guard_public_bind_needs_auth() {
        let lan: SocketAddr = "0.0.0.0:8443".parse().unwrap();
        let lo: SocketAddr = "127.0.0.1:8443".parse().unwrap();
        assert!(public_bind_refusal_with(&lan, false, false, false).is_some());
        assert!(public_bind_refusal_with(&lo, false, false, false).is_none());
        assert!(public_bind_refusal_with(&lan, true, false, false).is_none());
        assert!(public_bind_refusal_with(&lan, false, true, false).is_none());
        assert!(public_bind_refusal_with(&lan, false, false, true).is_none());
        let msg = public_bind_refusal_with(&lan, false, false, false).unwrap();
        assert!(msg.contains("THCLAWS_SERVE_TOKEN"));
    }
}
