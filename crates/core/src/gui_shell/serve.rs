//! dev-plan/33 Tier 2 Mode B: serve a single bound GUI Shell over
//! HTTP/WebSocket. Mounted by `server.rs::run_with_engine` when
//! `ServeConfig::gui_shell` is `Some`.
//!
//! Routing surface is deliberately flat — no `/shells/`, no
//! `/gui-shell/<id>/...` (Mode A's internal protocol URLs aren't
//! reachable from the network). Only the bound shell is reachable,
//! and only under the `/t/<token>/` prefix (or `/` when
//! `--gui-shell-no-auth` is set, with the loopback safety guard).
//!
//! ```text
//! GET  /t/<token>/                 → bound shell's index.html (bridge injected)
//! GET  /t/<token>/<rel>            → shell folder asset, MIME-typed, sandbox-checked
//! GET  /t/<token>/__bridge.js      → embedded bridge runtime (mode flag set to "ws")
//! GET  /t/<token>/__ws             → WebSocket — bridge IPC over the same dispatcher Mode A uses
//! *                                → 404 (silent — no auth challenge advertised)
//! ```

use super::{ShellRef, ShellRegistry, ShellToken};
use crate::error::{Error, Result};
use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;

/// Pick the URL prefix used by every Mode B route. With `no_auth` set
/// the prefix is empty (routes mount at `/`); otherwise the token is
/// the gate.
pub fn url_prefix(token: Option<&ShellToken>) -> String {
    match token {
        Some(t) => format!("/t/{}", t.value),
        None => String::new(),
    }
}

/// Build the launch URL printed to stdout when `--serve --gui-shell`
/// starts. Includes the trailing slash because the asset router is
/// prefix-scoped — a URL without the slash 404s.
pub fn launch_url(bind: std::net::SocketAddr, token: Option<&ShellToken>) -> String {
    let scheme = "http"; // Tier 2 doesn't terminate TLS — reverse-proxy responsibility.
    let prefix = url_prefix(token);
    format!("{scheme}://{bind}{prefix}/")
}

/// Resolve the bound shell from the registry — returns an error
/// (with a helpful message) when the id doesn't match any installed
/// shell. Used at launch time so the operator sees the problem
/// before users hit the server.
pub fn resolve_bound_shell(shell_id: &str) -> Result<ShellRef> {
    let registry = ShellRegistry::new();
    registry.resolve(shell_id).ok_or_else(|| {
        let known: Vec<String> = registry.list().into_iter().map(|(_, m)| m.id).collect();
        Error::Tool(format!(
            "--gui-shell '{shell_id}' not found. Installed: [{}]. \
                 Drop a folder in ~/.config/thclaws/gui-shell/<id>/ or \
                 ./.thclaws/gui-shell/<id>/ and retry.",
            known.join(", ")
        ))
    })
}

/// Enforce the loopback safety guard on `--gui-shell-no-auth`.
/// Returns `Err` when the operator combined `no_auth` with a
/// non-loopback bind but forgot the explicit override flag.
pub fn check_no_auth_safety(
    bind: &std::net::SocketAddr,
    no_auth: bool,
    no_auth_allow_public: bool,
) -> Result<()> {
    if !no_auth {
        return Ok(());
    }
    if bind.ip().is_loopback() {
        eprintln!(
            "\x1b[33m[gui-shell] WARNING: --gui-shell-no-auth on loopback. Anyone with shell access on this host can reach the shell.\x1b[0m"
        );
        return Ok(());
    }
    if !no_auth_allow_public {
        return Err(Error::Tool(format!(
            "--gui-shell-no-auth on a non-loopback bind ({}) is refused unless \
             --gui-shell-no-auth-allow-public is also passed. Use that flag only \
             behind your own auth proxy (Cloudflare Access, OAuth2 proxy, mTLS).",
            bind
        )));
    }
    eprintln!(
        "\x1b[31m[gui-shell] DANGER: --gui-shell-no-auth on public bind ({bind}). No token, no auth. Make sure you have an auth proxy in front.\x1b[0m"
    );
    Ok(())
}

/// Serve the bound shell's `index.html` with the bridge script
/// injected at `<head>` start AND a Mode B marker that flips the
/// bridge runtime into WebSocket transport.
///
/// `ws_url` is the path the bridge's WS client will connect to (e.g.
/// `/t/<token>/__ws`). It's relative — the browser resolves it
/// against the served origin, so the same handler works whether the
/// reverse proxy is at `localhost:8080` or `https://my-host/`.
///
/// The shell id and session id are also injected as globals so the
/// bridge knows both identifiers without parsing them from the URL
/// (Mode B URLs are `/t/<token>/`, which doesn't carry either).
pub fn serve_shell_index(shell: &ShellRef, ws_url: &str) -> Response<Body> {
    let (bytes, _mime) = match shell.read_asset("index.html") {
        Ok(pair) => pair,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(format!("shell entry not readable: {e}")))
                .expect("build 500");
        }
    };
    let session_id = "serve";
    let injected = inject_mode_b_head_with(&bytes, ws_url, &shell.manifest().id, session_id);
    Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store, must-revalidate"),
        )
        // Strip Referer so the per-shell token in the URL doesn't
        // leak when the shell links to an external page (Risk 14 in
        // dev-plan/33).
        .header(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        )
        .body(Body::from(injected))
        .expect("build mode-b index response")
}

/// Serve a file from the shell's project root (the cwd at serve-start
/// time, set by `server::run` when a shell is bound). Used for files
/// the agent produced — generated images, outputs, sidecar JSON the
/// shell renders via `<img src="…/file-asset/output/abc.png">`. Path
/// is validated via `Sandbox::check_in` rooted at the current
/// workspace.
pub fn serve_project_asset(workspace: &std::path::Path, rel: &str) -> Response<Body> {
    let decoded = match urlencoding::decode(rel) {
        Ok(s) => s.into_owned(),
        Err(_) => rel.to_string(),
    };
    let resolved = match crate::sandbox::Sandbox::check_in(workspace, &decoded) {
        Ok(p) => p,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::from("forbidden"))
                .expect("build 403");
        }
    };
    let bytes = match std::fs::read(&resolved) {
        Ok(b) => b,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("not found"))
                .expect("build 404");
        }
    };
    let mime = mime_for_path(&resolved);
    Response::builder()
        .header(header::CONTENT_TYPE, HeaderValue::from_str(mime).unwrap())
        .header(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        )
        .body(Body::from(bytes))
        .expect("build project-asset response")
}

fn mime_for_path(path: &std::path::Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Serve a non-HTML asset from the shell folder. Uses the same
/// `Sandbox::check_in` path-validation Mode A's protocol handler
/// uses — single source of truth for shell-folder path safety.
pub fn serve_shell_asset(shell: &ShellRef, rel: &str) -> Response<Body> {
    let (bytes, mime) = match shell.read_asset(rel) {
        Ok(pair) => pair,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("not found"))
                .expect("build 404");
        }
    };
    Response::builder()
        .header(header::CONTENT_TYPE, HeaderValue::from_str(mime).unwrap())
        .header(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        )
        .body(Body::from(bytes))
        .expect("build asset response")
}

/// Serve the bridge runtime. Identical bytes to what Mode A's protocol
/// handler returns; the Mode B HTML head injection sets the transport
/// flag so the same bridge file behaves differently at runtime.
pub fn serve_bridge_runtime() -> Response<Body> {
    Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/javascript; charset=utf-8"),
        )
        .header(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))
        .body(Body::from(super::BRIDGE_RUNTIME.as_bytes()))
        .expect("build bridge response")
}

/// Inject Mode B marker + bridge `<script>` at the start of `<head>`.
/// Sets `window.__thclaws_shell_mode = "ws"`, `..._ws_url`,
/// `..._shell_id`, and `..._shell_session_id` so the bridge knows
/// both the transport and identifiers without parsing them from the
/// URL (Mode B URLs are `/t/<token>/`, no shell id or session id).
pub fn inject_mode_b_head_with(
    html: &[u8],
    ws_url: &str,
    shell_id: &str,
    session_id: &str,
) -> Vec<u8> {
    // Same find-or-create-head logic as Mode A's gui.rs::inject_bridge_script,
    // but with an extra inline <script> before the bridge.
    let marker = format!(
        "<script>window.__thclaws_shell_mode=\"ws\";window.__thclaws_shell_ws_url={};window.__thclaws_shell_id={};window.__thclaws_shell_session_id={};</script>",
        serde_json::to_string(ws_url).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(shell_id).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(session_id).unwrap_or_else(|_| "\"\"".into()),
    );
    let bridge_src = format!(
        "<script src=\"{}/__bridge.js\"></script>",
        // Strip trailing /__ws so the same prefix used for the WS URL
        // also resolves the bridge asset.
        ws_url.strip_suffix("/__ws").unwrap_or(ws_url)
    );
    let injection = format!("{marker}{bridge_src}");

    let lower = html.to_ascii_lowercase();
    if let Some(idx) = find_subslice(&lower, b"<head>") {
        let insert_at = idx + b"<head>".len();
        let mut out = Vec::with_capacity(html.len() + injection.len());
        out.extend_from_slice(&html[..insert_at]);
        out.extend_from_slice(injection.as_bytes());
        out.extend_from_slice(&html[insert_at..]);
        out
    } else if let Some(idx) = find_subslice(&lower, b"<head ") {
        let after_open = html[idx..]
            .iter()
            .position(|&b| b == b'>')
            .map(|p| idx + p + 1)
            .unwrap_or(idx);
        let mut out = Vec::with_capacity(html.len() + injection.len());
        out.extend_from_slice(&html[..after_open]);
        out.extend_from_slice(injection.as_bytes());
        out.extend_from_slice(&html[after_open..]);
        out
    } else {
        let mut out = Vec::with_capacity(html.len() + injection.len() + b"<head></head>".len());
        out.extend_from_slice(b"<head>");
        out.extend_from_slice(injection.as_bytes());
        out.extend_from_slice(b"</head>");
        out.extend_from_slice(html);
        out
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn dummy_token(val: &str) -> ShellToken {
        ShellToken {
            value: val.into(),
            created_at: 0,
            ttl_secs: None,
        }
    }

    #[test]
    fn url_prefix_strips_when_no_token() {
        assert_eq!(url_prefix(None), "");
        let t = dummy_token("abc123");
        assert_eq!(url_prefix(Some(&t)), "/t/abc123");
    }

    #[test]
    fn launch_url_includes_trailing_slash() {
        let bind: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let t = dummy_token("tok");
        let url = launch_url(bind, Some(&t));
        assert_eq!(url, "http://127.0.0.1:8080/t/tok/");
        assert!(url.ends_with('/'));
    }

    #[test]
    fn launch_url_no_auth_form() {
        let bind: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let url = launch_url(bind, None);
        assert_eq!(url, "http://127.0.0.1:8080/");
    }

    #[test]
    fn no_auth_safety_passes_on_loopback() {
        let bind: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert!(check_no_auth_safety(&bind, true, false).is_ok());
    }

    #[test]
    fn no_auth_safety_refuses_public_without_override() {
        let bind: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let err = check_no_auth_safety(&bind, true, false).unwrap_err();
        assert!(format!("{err}").contains("non-loopback"));
    }

    #[test]
    fn no_auth_safety_allows_public_with_override() {
        let bind: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        assert!(check_no_auth_safety(&bind, true, true).is_ok());
    }

    #[test]
    fn no_auth_safety_noop_when_no_auth_unset() {
        let bind: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        assert!(check_no_auth_safety(&bind, false, false).is_ok());
    }

    #[test]
    fn resolve_bound_shell_finds_session_explorer() {
        // Session Explorer is always present as a built-in.
        let s = resolve_bound_shell("session-explorer").unwrap();
        assert_eq!(s.manifest().id, "session-explorer");
    }

    #[test]
    fn resolve_bound_shell_lists_known_on_miss() {
        let err = resolve_bound_shell("does-not-exist").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("session-explorer"), "lists installed: {msg}");
        assert!(msg.contains("does-not-exist"));
    }
}
