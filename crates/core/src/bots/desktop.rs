//! dev-plan/59 §7.6 step 3: the desktop window's link to a bot.
//!
//! The desktop serves its page over `thclaws://` and talks to the engine
//! through wry's IPC bridge. Both are proven — the window renders that way
//! every day, and two attempts at replacing them with a loopback HTTP origin
//! produced a black screen. So the bridge stays exactly where it is and only
//! its far end moves: frames the page sends go to a bot's `--serve` instead
//! of to an in-process engine, and the bot's frames come back the way the
//! in-process engine's did.

use futures::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TgMessage;

/// One window-to-bot link. Dropping it closes the socket.
pub struct BotBridge {
    tx: mpsc::UnboundedSender<String>,
    pub slug: String,
}

impl BotBridge {
    /// Hand one IPC frame to the bot. Frames sent before the socket is up are
    /// queued by the channel, so the page's opening `frontend_ready` is not
    /// lost to the connect race.
    pub fn send(&self, frame: String) {
        let _ = self.tx.send(frame);
    }
}

/// Open the link. `on_frame` is called for every frame the bot sends, on a
/// runtime thread — the caller is expected to hand it to the UI thread.
pub fn connect(
    addr: SocketAddr,
    token: String,
    slug: String,
    on_frame: impl Fn(String) + Send + 'static,
) -> BotBridge {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let bridge_slug = slug.clone();
    tokio::spawn(async move {
        // The link outlives any one socket. A bot that crashes is restarted
        // by the supervisor, and the host's relay then closes this side —
        // without this loop the window went dead until the user happened to
        // switch bots. The host waits for the bot to be ready before it
        // relays, so reconnecting to the host is enough.
        let mut backoff_ms: u64 = 250;
        loop {
            let sock = match open_socket(addr, &token, &slug).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("\x1b[33m[host] agent '{slug}': {e} — retrying\x1b[0m");
                    on_frame(status_frame(&slug, "connecting"));
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(5_000);
                    if rx.is_closed() {
                        return;
                    }
                    continue;
                }
            };
            // Backoff is NOT reset on connect: the host accepts the upgrade
            // before it knows whether the bot will take the session, so a
            // bot that refuses (a 404 from its router) looked like a healthy
            // connect every time, and the loop spun with no pause until the
            // machine ran out of local ports. Only a session that lived is
            // proof the link is good.
            let opened = std::time::Instant::now();
            let mut got_frame = false;
            // The page's existing "reconnecting…" banner listens for these.
            on_frame(status_frame(&slug, "connected"));
            let (mut sink, mut stream) = sock.split();
            let dropped = loop {
                tokio::select! {
                    out = rx.recv() => match out {
                        Some(frame) => {
                            if sink.send(TgMessage::text(frame)).await.is_err() {
                                break false;
                            }
                        }
                        None => break true, // the window dropped the bridge
                    },
                    inbound = stream.next() => match inbound {
                        Some(Ok(TgMessage::Text(t))) => {
                            got_frame = true;
                            on_frame(tag(&slug, t.as_str()));
                        }
                        Some(Ok(TgMessage::Close(_))) | None => break false,
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            eprintln!("\x1b[33m[host] agent '{slug}' socket: {e}\x1b[0m");
                            break false;
                        }
                    },
                }
            };
            let _ = sink.close().await;
            if dropped {
                return;
            }
            on_frame(status_frame(&slug, "disconnected"));
            let healthy = got_frame || opened.elapsed() >= std::time::Duration::from_secs(5);
            if healthy {
                backoff_ms = 250;
            }
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            if !healthy {
                backoff_ms = (backoff_ms * 2).min(5_000);
            }
            if rx.is_closed() {
                return;
            }
        }
    });
    BotBridge {
        tx,
        slug: bridge_slug,
    }
}

async fn open_socket(
    addr: SocketAddr,
    token: &str,
    slug: &str,
) -> std::result::Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
> {
    let url = format!("ws://{addr}/ws?bot={}", urlencoding::encode(slug));
    let mut req = url
        .into_client_request()
        .map_err(|e| format!("bad agent url: {e}"))?;
    // Header form rather than `?token=`: this end is a Rust client, so the
    // token never has to appear in a URL that could be logged.
    let value = format!("Bearer {token}")
        .parse()
        .map_err(|_| "agent token is not a valid header value".to_string())?;
    req.headers_mut().insert("authorization", value);
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(s, _)| s)
        .map_err(|e| format!("cannot reach the host: {e}"))
}

/// Stamp a bot's frame with where it came from. The page's wry transport has
/// no per-socket routing the way the browser's does, so a frame still in
/// flight from the bot being switched away from would otherwise land in the
/// new bot's tree as if it were its own. Inserted textually — every frame is
/// a JSON object from serde, and parsing each one just to add a key would be
/// the only cost on this path.
pub(crate) fn tag(slug: &str, frame: &str) -> String {
    match frame.strip_prefix('{') {
        Some(rest) => format!(
            "{{\"_bot\":{},{rest}",
            serde_json::Value::String(slug.to_string())
        ),
        None => frame.to_string(),
    }
}

fn status_frame(slug: &str, status: &str) -> String {
    serde_json::json!({ "type": "ws_status", "status": status, "_bot": slug }).to_string()
}

/// One host mutation asked for from the window — install, remove, restart —
/// answered as a frame the page can act on, followed by a fresh list.
pub async fn action_frame(
    addr: SocketAddr,
    token: &str,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> String {
    let url = format!("http://{addr}{path}");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_default();
    let mut req = client
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::POST),
            &url,
        )
        .header("Authorization", format!("Bearer {token}"));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let (ok, error, log) = match req.send().await {
        Ok(r) => {
            let status_ok = r.status().is_success();
            let v = r
                .json::<serde_json::Value>()
                .await
                .unwrap_or(serde_json::Value::Null);
            let ok = status_ok && v.get("ok").and_then(|b| b.as_bool()).unwrap_or(status_ok);
            let error = v
                .get("error")
                .and_then(|e| e.as_str())
                .map(str::to_string)
                .unwrap_or_default();
            let log = v.get("log").cloned().unwrap_or(serde_json::json!([]));
            (ok, error, log)
        }
        Err(e) => (
            false,
            format!("host unreachable: {e}"),
            serde_json::json!([]),
        ),
    };
    serde_json::json!({ "type": "bots_action_result", "ok": ok, "error": error, "log": log })
        .to_string()
}

/// The window's own answer to `bots_list`: what the host says it is
/// supervising, plus which bot this window is currently attached to.
pub async fn list_frame(addr: SocketAddr, token: &str, active: &str) -> String {
    let url = format!("http://{addr}/bots");
    let body = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_default()
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .ok();
    let bots = match body {
        Some(r) => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("bots").cloned())
            .unwrap_or_else(|| serde_json::json!([])),
        None => serde_json::json!([]),
    };
    serde_json::json!({ "type": "bots_list_result", "bots": bots, "active": active }).to_string()
}

/// The host's `/bots/templates`, as a `bots_templates_result` frame.
pub async fn templates_frame(addr: SocketAddr, token: &str) -> String {
    let url = format!("http://{addr}/bots/templates");
    let v = match reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_default()
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
    {
        Ok(r) => r
            .json::<serde_json::Value>()
            .await
            .unwrap_or_else(|e| serde_json::json!({ "ok": false, "error": e.to_string() })),
        Err(e) => serde_json::json!({ "ok": false, "error": format!("host unreachable: {e}") }),
    };
    let mut v = v;
    v["type"] = serde_json::json!("bots_templates_result");
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::tag;

    /// Every frame a bot sends is stamped with the bot it came from, so the
    /// page can drop one still in flight from a bot it just switched away
    /// from. The stamp is inserted textually into serde's JSON object.
    #[test]
    fn frames_are_stamped_with_their_bot() {
        let out = tag("research", r#"{"type":"chat_text_delta","text":"hi"}"#);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["_bot"], "research");
        assert_eq!(v["type"], "chat_text_delta");
        assert_eq!(v["text"], "hi");
        // A slug is quoted as JSON, not pasted raw.
        let odd = tag("a\"b", r#"{"type":"x"}"#);
        let v: serde_json::Value = serde_json::from_str(&odd).unwrap();
        assert_eq!(v["_bot"], "a\"b");
        // Not an object: left alone rather than broken.
        assert_eq!(tag("main", "[1,2]"), "[1,2]");
    }
}
