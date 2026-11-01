//! Relay one browser WebSocket to the focused bot's own `--serve`.
//!
//! Proxying is the design, not a fallback: a cloud runner exposes a single
//! `containerPort: 8443` with its probes on it, so a child on another port is
//! not reachable from outside the pod at all. The host is the only ingress.
//!
//! Frames pass through verbatim. The host does not parse the IPC protocol —
//! with one bot there is nothing to route, and when there are several the
//! rule is default-forward: a frame the host does not recognise is an agent
//! frame, which is the safe direction to guess.

use super::supervisor::Bot;
use axum::extract::ws::{Message as AxMessage, WebSocket};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TgMessage;

/// A browser that connects while the bot is still starting waits rather than
/// failing — a cold `--serve` start is slower than a page load, and the first
/// connection after `--supervisor` comes up is exactly that case.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(70);

/// WS close codes are u16; 1011 is "internal error", which is what a bot that
/// will not start is from the browser's point of view.
const CLOSE_INTERNAL: u16 = 1011;

/// `origin` is the browser's, carried to the agent so it can tell a browser
/// tab from a bare client (see `server::browser_client`).
pub async fn relay(browser: WebSocket, bot: Arc<Bot>, origin: Option<axum::http::HeaderValue>) {
    let (addr, token) = match bot.wait_ready(CONNECT_TIMEOUT).await {
        Ok(v) => v,
        Err(e) => {
            close_with(browser, &e.to_string()).await;
            return;
        }
    };

    let mut req = match format!("ws://{addr}/ws").into_client_request() {
        Ok(r) => r,
        Err(e) => {
            close_with(browser, &format!("bad agent url: {e}")).await;
            return;
        }
    };
    // Header form rather than `?token=`: this end is a Rust client, so the
    // token never has to appear in a URL that could be logged.
    match format!("Bearer {token}").parse() {
        Ok(v) => {
            req.headers_mut()
                .insert(axum::http::header::AUTHORIZATION, v);
        }
        Err(_) => {
            close_with(browser, "agent token is not a valid header value").await;
            return;
        }
    }
    // The host already checked the browser's Origin against its own bind. A
    // hosted host's Origin is a public name the child's loopback guard would
    // refuse, so the child is told only that a browser is on the other end.
    if origin.is_some_and(|o| o.as_bytes() != b"null") {
        if let Ok(v) = format!("http://{addr}").parse() {
            req.headers_mut().insert(axum::http::header::ORIGIN, v);
        }
    }

    let child = match tokio_tungstenite::connect_async(req).await {
        Ok((sock, _)) => sock,
        Err(e) => {
            close_with(browser, &format!("cannot reach agent '{}': {e}", bot.slug)).await;
            return;
        }
    };

    let (mut child_tx, mut child_rx) = child.split();
    let (mut browser_tx, mut browser_rx) = browser.split();

    // Ping/pong is each hop's own business — forwarding them would answer on
    // behalf of a peer that may already be gone.
    let upstream = async {
        while let Some(Ok(msg)) = browser_rx.next().await {
            let out = match msg {
                AxMessage::Text(t) => TgMessage::text(t.as_str()),
                AxMessage::Binary(b) => TgMessage::binary(b),
                AxMessage::Close(_) => break,
                AxMessage::Ping(_) | AxMessage::Pong(_) => continue,
            };
            if child_tx.send(out).await.is_err() {
                break;
            }
        }
        let _ = child_tx.close().await;
    };

    let downstream = async {
        while let Some(Ok(msg)) = child_rx.next().await {
            let out = match msg {
                TgMessage::Text(t) => AxMessage::Text(t.as_str().into()),
                TgMessage::Binary(b) => AxMessage::Binary(b),
                TgMessage::Close(_) => break,
                _ => continue,
            };
            if browser_tx.send(out).await.is_err() {
                break;
            }
        }
        // The child going away — a crash, or the supervisor restarting it —
        // closes the browser socket too. The frontend already reconnects with
        // backoff and re-sends `frontend_ready`, so it re-initialises against
        // the restarted child instead of sitting on a half-dead session.
        let _ = browser_tx.close().await;
    };

    tokio::select! {
        _ = upstream => {}
        _ = downstream => {}
    }
}

async fn close_with(mut browser: WebSocket, reason: &str) {
    eprintln!("\x1b[33m[bots] proxy: {reason}\x1b[0m");
    let _ = browser
        .send(AxMessage::Close(Some(axum::extract::ws::CloseFrame {
            code: CLOSE_INTERNAL,
            // Close reasons are capped at 123 bytes by the protocol.
            reason: clip_reason(reason).into(),
        })))
        .await;
}

fn clip_reason(reason: &str) -> String {
    let mut out = String::new();
    for c in reason.chars() {
        if out.len() + c.len_utf8() > 120 {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ws::WebSocketUpgrade;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;

    const TOKEN: &str = "child-secret";

    /// Stands in for a bot's own `--serve`: refuses the socket without the
    /// per-child bearer, and echoes what it is sent so the test can tell a
    /// relayed frame from a dropped one.
    async fn child_ws(
        ws: WebSocketUpgrade,
        headers: axum::http::HeaderMap,
    ) -> axum::response::Response {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if presented != Some(&format!("Bearer {TOKEN}")[..]) {
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
        ws.on_upgrade(|mut sock| async move {
            while let Some(Ok(msg)) = sock.recv().await {
                if let AxMessage::Text(t) = msg {
                    if sock
                        .send(AxMessage::Text(format!("echo:{t}").into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        })
    }

    async fn serve(app: Router) -> std::net::SocketAddr {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        addr
    }

    async fn host_for(bot: Arc<Bot>) -> std::net::SocketAddr {
        serve(Router::new().route(
            "/ws",
            get(move |ws: WebSocketUpgrade| {
                let bot = bot.clone();
                async move { ws.on_upgrade(move |sock| relay(sock, bot, None)) }
            }),
        ))
        .await
    }

    /// The whole point of Step 3: a browser talks to the host and the bot's
    /// own process answers, over a socket the browser never sees.
    #[tokio::test]
    async fn a_frame_reaches_the_bot_and_its_reply_comes_back() {
        let child = serve(Router::new().route("/ws", get(child_ws))).await;
        let host = host_for(Bot::fake_ready("main", child, TOKEN)).await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{host}/ws"))
            .await
            .expect("browser connects to the host");
        ws.send(TgMessage::text(r#"{"type":"frontend_ready"}"#))
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("bot replied in time")
            .expect("stream open")
            .expect("frame");
        assert_eq!(
            reply.into_text().unwrap().as_str(),
            r#"echo:{"type":"frontend_ready"}"#
        );
    }

    /// The child's bearer is the host's to present. A bot whose token the
    /// host got wrong must fail closed, not fall back to an open socket.
    #[tokio::test]
    async fn a_wrong_child_token_does_not_open_a_socket() {
        let child = serve(Router::new().route("/ws", get(child_ws))).await;
        let host = host_for(Bot::fake_ready("main", child, "wrong")).await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{host}/ws"))
            .await
            .unwrap();
        // The host accepted the browser, then closed it once the child
        // refused — no frame is ever relayed.
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("host answered")
            .expect("stream open")
            .expect("frame");
        match msg {
            TgMessage::Close(Some(f)) => {
                assert_eq!(u16::from(f.code), CLOSE_INTERNAL);
                assert!(f.reason.contains("cannot reach agent"), "{}", f.reason);
            }
            other => panic!("expected a close frame, got {other:?}"),
        }
    }

    /// A browser that connects to a parked bot is told why instead of being
    /// left on a socket that will never carry anything.
    #[tokio::test]
    async fn a_browser_on_a_parked_bot_is_told() {
        let bot = Bot::fake_crash_looped("main", "settings.json is not valid JSON");
        let host = host_for(bot).await;

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{host}/ws"))
            .await
            .unwrap();
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("host answered")
            .expect("stream open")
            .expect("frame");
        match msg {
            TgMessage::Close(Some(f)) => {
                assert!(f.reason.contains("not valid JSON"), "{}", f.reason);
            }
            other => panic!("expected a close frame, got {other:?}"),
        }
    }
}

/// Forward one HTTP request to a bot's own `--serve`, attaching its bearer.
///
/// Used for the routes the host deliberately does not serve itself
/// (`/file-asset`, `/upload`): they read and write inside a bot's tree, which
/// is the bot's business, and the browser never learns a child's token.
pub async fn forward_http(
    addr: &std::net::SocketAddr,
    token: &str,
    req: axum::extract::Request,
) -> std::result::Result<axum::response::Response, String> {
    forward(addr, Some(token), req).await
}

/// Forward with the CALLER's `Authorization` rather than the agent's own
/// token. `/v1/*` is checked against `THCLAWS_API_TOKEN`, which every agent
/// inherits from the host, so the caller's bearer is the one that proves
/// anything there.
pub async fn forward_http_as_caller(
    addr: &std::net::SocketAddr,
    req: axum::extract::Request,
) -> std::result::Result<axum::response::Response, String> {
    forward(addr, None, req).await
}

async fn forward(
    addr: &std::net::SocketAddr,
    token: Option<&str>,
    req: axum::extract::Request,
) -> std::result::Result<axum::response::Response, String> {
    let (parts, body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let url = format!("http://{addr}{path}");

    // The request body is buffered: the agent's own route limits bound it,
    // and a retry-free loopback hop gains nothing from streaming it.
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|e| format!("read request body: {e}"))?;

    let mut out = forward_client().request(parts.method.clone(), &url);
    if let Some(token) = token {
        out = out.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    for (name, value) in parts.headers.iter() {
        // `host` names the host, not the child; `authorization` is ours to
        // set unless the caller's is the one being carried; `content-length`
        // is recomputed from the buffered body.
        if matches!(name.as_str(), "host" | "content-length" | "connection")
            || (token.is_some() && name == axum::http::header::AUTHORIZATION)
        {
            continue;
        }
        out = out.header(name, value);
    }
    let res = out
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("forward {}: {e}", parts.uri))?;

    let status = res.status();
    let headers = res.headers().clone();
    // Streamed, not buffered: `/v1/chat/completions` with `stream: true` is
    // server-sent events, and a buffered relay would hold every token until
    // the turn ended.
    let body = axum::body::Body::from_stream(res.bytes_stream());
    let mut builder = axum::response::Response::builder().status(status);
    for (name, value) in headers.iter() {
        if matches!(name.as_str(), "connection" | "transfer-encoding") {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(body)
        .map_err(|e| format!("build response: {e}"))
}

fn forward_client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            // A machine-wide HTTP_PROXY must not intercept a loopback hop.
            .no_proxy()
            .build()
            .unwrap_or_default()
    })
}
