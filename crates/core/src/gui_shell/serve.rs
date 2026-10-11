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
        // Keep the per-shell token in the URL from leaking when the shell
        // links to an external page (Risk 14 in dev-plan/33) — but only to
        // external pages.
        //
        // `same-origin`, not `no-referrer`: under a workspace host the shell's
        // own `style.css` / `main.js` are same-origin requests carrying no
        // `?bot=` of their own, and `supervisor_forward` reads the Referer to
        // learn which agent they belong to. `no-referrer` withheld it there
        // too, so every sub-asset was routed to whichever agent happened to be
        // first and 404'd — the shell rendered unstyled, with no error to
        // explain it. Desktop never saw this: `thclaws://` serves assets
        // directly and never reaches the supervisor.
        .header(
            header::REFERRER_POLICY,
            HeaderValue::from_static("same-origin"),
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
/// Read a file-asset honoring an optional HTTP `Range` header. Returns the
/// body plus `Some((start, end, total))` when a range applied (→ 206). Only
/// the requested slice is read from disk, and open-ended ranges (`bytes=N-`)
/// are capped at 8 MB per response — media players see the shorter
/// Content-Range and keep requesting, which is how progressive buffering
/// works. Without this, a shell's <video> pulled whole chapter mp4s per
/// request and playback stuttered.
pub fn read_asset_maybe_range(
    path: &std::path::Path,
    range: Option<&str>,
) -> std::io::Result<(Vec<u8>, Option<(u64, u64, u64)>)> {
    use std::io::{Read, Seek, SeekFrom};
    const OPEN_ENDED_CHUNK: u64 = 8 * 1024 * 1024;
    let total = std::fs::metadata(path)?.len();
    let parsed = range
        .and_then(|h| h.strip_prefix("bytes="))
        .and_then(|spec| {
            let (s, e) = spec.split_once('-')?;
            let start: u64 = s.trim().parse().ok()?;
            let end: u64 = match e.trim() {
                "" => (start + OPEN_ENDED_CHUNK - 1).min(total.saturating_sub(1)),
                v => v.parse::<u64>().ok()?.min(total.saturating_sub(1)),
            };
            (start <= end && start < total).then_some((start, end))
        });
    match parsed {
        Some((start, end)) => {
            let mut f = std::fs::File::open(path)?;
            f.seek(SeekFrom::Start(start))?;
            let mut buf = vec![0u8; (end - start + 1) as usize];
            f.read_exact(&mut buf)?;
            Ok((buf, Some((start, end, total))))
        }
        None => Ok((std::fs::read(path)?, None)),
    }
}

pub fn serve_project_asset(
    workspace: &std::path::Path,
    rel: &str,
    range: Option<&str>,
) -> Response<Body> {
    let decoded = match urlencoding::decode(rel) {
        Ok(s) => s.into_owned(),
        Err(_) => rel.to_string(),
    };
    // Two callers produce two URL shapes:
    //   gui-shells (image-batch, video-studio, speech-studio) build
    //     workspace-relative paths like `images/<slug>/<file>.png` —
    //     join with cwd → /workspace/images/...
    //   FilesView's assetUrl builds ABSOLUTE paths like
    //     /workspace/speech/<file>.wav — axum's Path extractor strips
    //     the leading `/`, so without the absolute-first attempt we'd
    //     re-join with cwd and look for /workspace/workspace/... → 404.
    // Try absolute first (re-add the slash the route capture peeled off);
    // fall back to workspace-relative. `check_in` enforces sandbox
    // containment for both — security unchanged.
    let resolved = {
        let abs_candidate = if decoded.starts_with('/') {
            decoded.clone()
        } else {
            format!("/{decoded}")
        };
        crate::sandbox::Sandbox::check_in(workspace, &abs_candidate)
            .or_else(|_| crate::sandbox::Sandbox::check_in(workspace, &decoded))
    };
    let resolved = match resolved {
        Ok(p) => p,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::from("forbidden"))
                .expect("build 403");
        }
    };
    let (bytes, range_meta) = match read_asset_maybe_range(&resolved, range) {
        Ok(v) => v,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("not found"))
                .expect("build 404");
        }
    };
    let mime = mime_for_path(&resolved);
    let mut rb = Response::builder()
        .header(header::CONTENT_TYPE, HeaderValue::from_str(mime).unwrap())
        .header(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"))
        .header(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
    // A hosted workspace on a path-scheme install (`<tenant>/u/<h>/<s>/`)
    // shares its origin with the web app, whose JWT sits in localStorage.
    // Workspace HTML/SVG is whatever the agent or a fetched page wrote, so
    // it gets an opaque origin wherever it is opened — top-level tab or
    // iframe. Not PDFs: a sandboxed document can't host the PDF viewer.
    if is_scriptable_document(mime) {
        rb = rb.header(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(WORKSPACE_DOC_CSP),
        );
    }
    if let Some((start, end, total)) = range_meta {
        rb = rb.status(StatusCode::PARTIAL_CONTENT).header(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")).unwrap(),
        );
    }
    rb.body(Body::from(bytes))
        .expect("build project-asset response")
}

pub const WORKSPACE_DOC_CSP: &str =
    "sandbox allow-scripts allow-popups allow-forms allow-downloads allow-modals";

fn is_scriptable_document(mime: &str) -> bool {
    mime.starts_with("text/html") || mime.starts_with("image/svg+xml")
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
        // Inline-renderable in the browser's viewer — without this,
        // octet-stream makes the Files-tab PDF iframe download instead
        // of displaying.
        "pdf" => "application/pdf",
        // Audio
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" | "aac" => "audio/mp4",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "flac" => "audio/flac",
        "weba" => "audio/webm",
        // Video
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "ogv" => "video/ogg",
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

/// Serve a shell's index.html for the cloud `--serve` mount (Mode C):
/// React parent loads the shell in an iframe, the bridge runs in
/// postMessage mode (Mode A) talking to the parent — NOT to a
/// per-shell WebSocket. So we inline the bridge runtime into the HTML
/// and skip the Mode B WS-URL injection entirely (no relative-path
/// games for `/__bridge.js` when the workspace lives under a traefik
/// strip-prefix).
pub fn serve_shell_index_inline(shell: &ShellRef) -> Response<Body> {
    let (bytes, _mime) = match shell.read_asset("index.html") {
        Ok(pair) => pair,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(format!("shell entry not readable: {e}")))
                .expect("build 500");
        }
    };
    let injected = inject_inline_bridge_with_id(&bytes, &shell.manifest().id);
    Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store, must-revalidate"),
        )
        // See the mode-B response above: `same-origin` still withholds the
        // per-shell token from external pages, while leaving the Referer that
        // routes this shell's own sub-assets to the right agent.
        .header(
            header::REFERRER_POLICY,
            HeaderValue::from_static("same-origin"),
        )
        .body(Body::from(injected))
        .expect("build mode-c index response")
}

/// Inject the bridge runtime as an inline `<script>...</script>` at
/// the start of `<head>`. No mode globals set → bridge defaults to
/// Mode A (postMessage), matching the iframe-in-React-parent pattern
/// UIView uses. Sets `window.__thclaws_shell_id` so the bridge skips
/// URL-parsing (which would fail in cloud — the iframe path is
/// `/u/<handle>/<slug>/gui-shell/<id>/...`, traefik strips the prefix
/// before the engine sees it but the browser's `location.pathname`
/// includes everything, so `parts[0] === "gui-shell"` is false).
pub fn inject_inline_bridge_with_id(html: &[u8], shell_id: &str) -> Vec<u8> {
    let bridge = super::BRIDGE_RUNTIME;
    let id_json = serde_json::to_string(shell_id)
        .unwrap_or_else(|_| "\"\"".into())
        .replace("</", "<\\/");
    let bridge_safe = bridge.replace("</", "<\\/");
    let chrome = super::shared_chrome_head();
    let injection = format!(
        "<script>window.__thclaws_shell_id={id_json};</script><script>{bridge_safe}</script>{chrome}"
    );
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
        serde_json::to_string(ws_url).unwrap_or_else(|_| "\"\"".into()).replace("</", "<\\/"),
        serde_json::to_string(shell_id).unwrap_or_else(|_| "\"\"".into()).replace("</", "<\\/"),
        serde_json::to_string(session_id).unwrap_or_else(|_| "\"\"".into()).replace("</", "<\\/"),
    );
    let bridge_src = format!(
        "<script src=\"{}/__bridge.js\"></script>",
        // Strip trailing /__ws so the same prefix used for the WS URL
        // also resolves the bridge asset.
        ws_url.strip_suffix("/__ws").unwrap_or(ws_url)
    );
    let chrome = super::shared_chrome_head();
    let injection = format!("{marker}{bridge_src}{chrome}");

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

    /// The shell's own `style.css` and `main.js` are same-origin requests
    /// that carry no `?bot=`, and under a workspace host
    /// `server::supervisor_forward` reads the Referer to learn which agent
    /// they belong to. `no-referrer` — which this header was once set to,
    /// to keep the per-shell token out of external links — withheld it from
    /// those requests too, so every sub-asset was routed to whichever agent
    /// came first and 404'd: the shell rendered unstyled with nothing in the
    /// log to say why.
    ///
    /// `same-origin` keeps the original protection (nothing is sent
    /// cross-origin) and restores the routing. Do not "harden" this back.
    #[test]
    fn the_shell_index_keeps_its_same_origin_referer() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("index.html"), b"<html></html>").unwrap();
        let manifest: crate::gui_shell::manifest::ShellManifest = serde_json::from_str(
            r#"{"id":"deskx","name":"Desk X","version":"1.0.0",
                "description":"t","entry":"index.html"}"#,
        )
        .expect("manifest parses");
        let shell = ShellRef::OnDisk {
            manifest,
            root: tmp.path().to_path_buf(),
            source: crate::gui_shell::registry::ShellSource::Project,
        };
        let resp = serve_shell_index_inline(&shell);
        let policy = resp
            .headers()
            .get(header::REFERRER_POLICY)
            .expect("shell index sets a referrer policy");
        assert_eq!(policy, "same-origin");
    }
    use std::net::SocketAddr;

    fn dummy_token(val: &str) -> ShellToken {
        ShellToken {
            value: val.into(),
            created_at: 0,
            ttl_secs: None,
        }
    }

    #[test]
    fn workspace_html_and_svg_get_an_opaque_origin() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("a.html", "<script>1</script>"),
            ("A.HTM", "x"),
            ("b.svg", "<svg/>"),
            ("c.png", "x"),
            ("d.pdf", "x"),
        ] {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        let csp = |rel: &str| {
            serve_project_asset(dir.path(), rel, None)
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .map(|v| v.to_str().unwrap().to_string())
        };
        for rel in ["a.html", "A.HTM", "b.svg"] {
            let v = csp(rel).unwrap_or_else(|| panic!("{rel} has no CSP"));
            assert!(v.starts_with("sandbox "), "{rel}: {v}");
            assert!(!v.contains("allow-same-origin"), "{rel}: {v}");
        }
        assert_eq!(csp("c.png"), None);
        assert_eq!(csp("d.pdf"), None);
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

    // Regression for PR #157 (@JonusNattapong). JSON escaping doesn't
    // touch `</`, but the HTML tokenizer scans for the literal byte
    // sequence `</script>` regardless of JS-level escaping. A
    // shell-manifest id containing `</script>` (a malicious gui-shell
    // bundle could plant one) would close the injected `<script>` tag
    // prematurely and break out. The fix: post-JSON `.replace("</",
    // "<\\/")` everywhere the value enters a `<script>` body. `<\/`
    // is invisible to the HTML tokenizer, valid JSON, and equal to
    // `</` in JS at runtime.
    #[test]
    fn inject_inline_bridge_escapes_script_breakout_in_shell_id() {
        let html = b"<html><head></head><body></body></html>";
        let evil = "shell</script><script>alert('xss')</script>";
        let out = inject_inline_bridge_with_id(html, evil);
        let out_s = std::str::from_utf8(&out).expect("utf8");
        // Find the injection block — between the first `<script>` we
        // emitted and the closing `</script>` of the bridge runtime.
        // The injected shell-id script must contain NO literal
        // `</script>` between its opening `<script>` and its own
        // closer; otherwise the HTML parser sees an early close.
        let first_open = out_s
            .find("<script>window.__thclaws_shell_id=")
            .expect("marker");
        let first_close = out_s[first_open..]
            .find("</script>")
            .expect("close")
            .saturating_add(first_open);
        let inner = &out_s[first_open + "<script>".len()..first_close];
        assert!(
            !inner.contains("</script>"),
            "shell-id script body contains an early </script>: {inner:?}"
        );
        // And the escaped form must be present — sanity check that the
        // replacement actually happened, not that we accidentally
        // stripped the attack string entirely.
        assert!(
            inner.contains("<\\/script>"),
            "expected `<\\/script>` in escaped body, got {inner:?}"
        );
    }

    #[test]
    fn inject_mode_b_head_escapes_script_breakout_in_all_values() {
        let html = b"<html><head></head><body></body></html>";
        let evil_url = "ws://x/</script><script>1</script>";
        let evil_id = "id</script>";
        let evil_session = "sess</script>";
        let out = inject_mode_b_head_with(html, evil_url, evil_id, evil_session);
        let out_s = std::str::from_utf8(&out).expect("utf8");
        let marker_open = out_s
            .find("<script>window.__thclaws_shell_mode=")
            .expect("marker");
        let marker_close = out_s[marker_open..].find("</script>").expect("close") + marker_open;
        let inner = &out_s[marker_open + "<script>".len()..marker_close];
        assert!(
            !inner.contains("</script>"),
            "mode-b marker body contains an early </script>: {inner:?}"
        );
    }

    #[test]
    fn inline_inject_includes_shared_theme_and_chrome() {
        let html = b"<html><head></head><body></body></html>";
        let out = inject_inline_bridge_with_id(html, "demo");
        let out_s = std::str::from_utf8(&out).expect("utf8");
        // Shared theme tokens + the <thc-header> component runtime ride
        // along with the bridge, so studios don't ship their own.
        assert!(out_s.contains("--accent"), "theme tokens missing");
        assert!(
            out_s.contains("customElements.define(\"thc-header\""),
            "thc-header runtime missing"
        );
        // The chrome block must not break out of its own tags: the only
        // closers present are the single wrapper `</style>` + `</script>`.
        let chrome = super::super::shared_chrome_head();
        assert_eq!(chrome.matches("</style>").count(), 1, "early </style>");
        assert_eq!(chrome.matches("</script>").count(), 1, "early </script>");
    }

    #[test]
    fn mode_b_inject_includes_shared_theme_and_chrome() {
        let html = b"<html><head></head><body></body></html>";
        let out = inject_mode_b_head_with(html, "/t/x/__ws", "demo", "sess");
        let out_s = std::str::from_utf8(&out).expect("utf8");
        assert!(out_s.contains("--accent"), "theme tokens missing");
        assert!(
            out_s.contains("customElements.define(\"thc-header\""),
            "thc-header runtime missing"
        );
    }
}
