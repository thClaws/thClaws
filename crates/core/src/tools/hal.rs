//! HAL Public API tools — YouTube transcript + Web scrape.
//!
//! HAL (`hal.thaigpt.com/api`) is a hosted service that exposes two
//! research-friendly endpoints behind a single `X-API-Key` header:
//!
//! - `POST /youtube/v1/transcript` — fetch a YouTube video's transcript
//!   (with or without timestamps), with language preference fallback.
//! - `POST /scrape/v1/url` — render a page in a headless browser and
//!   return the text as Markdown, with selector-based wait/cleanup.
//!
//! Both tools declare `requires_env() = &["HAL_API_KEY"]`. When the
//! key isn't set, [`ToolRegistry::tool_defs`] hides them from the
//! model's tool list, so they don't waste tokens or invite failed
//! calls. Live key changes (`api_key_set` / `api_key_clear` followed
//! by `rebuild_agent`) flip the tools in/out on the next turn.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

/// Hard timeout for HAL HTTP requests. The scrape endpoint can be
/// slow for heavy pages with `scroll_to_bottom`; HAL's own default
/// `wait_timeout` is 30s, so we give the round trip a generous 90s
/// before giving up. The agent's per-turn cancel token still wins
/// over this if the user cancels.
pub(crate) const HAL_TIMEOUT: Duration = Duration::from_secs(90);
const HAL_BASE_URL: &str = "https://hal.thaigpt.com/api";

pub(crate) fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(HAL_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Cheap runtime check — does the live process env hold a non-empty
/// `HAL_API_KEY`? Same pattern as `ToolRegistry::tool_is_available`
/// but exposed for cross-tool routing decisions (e.g. WebFetch checks
/// this to decide whether to route through HAL's headless scrape).
///
/// Reads each call — no caching — so a user pasting a key
/// mid-session sees the change on the next tool invocation without
/// needing a worker rebuild.
pub(crate) fn hal_available() -> bool {
    // Gateway active (desktop proxy or cloud pod): HAL is reachable via
    // the gateway even with no local key (the gateway holds it). Direct
    // mode: needs a local key.
    if crate::tools::gateway_active() {
        return true;
    }
    std::env::var("HAL_API_KEY")
        .ok()
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false)
}

/// Convenience wrapper around HAL's `/scrape/v1/url` endpoint. Posts
/// the URL, extracts the rendered `content` field (Markdown) from the
/// JSON envelope, and prepends a `# {title}` header when present so
/// the model gets a single readable blob instead of nested JSON. The
/// caller owns truncation / formatting after this returns.
///
/// Used by `WebFetchTool` to opportunistically upgrade plain HTTP
/// GETs into JS-rendered scrapes when a HAL key is available. Errors
/// propagate from `hal_post` (auth, network, HAL 5xx, target page
/// failure) — caller decides whether to surface or fall back.
pub(crate) async fn scrape_via_hal(client: &reqwest::Client, url: &str) -> Result<String> {
    let body = json!({ "url": url });
    let resp = hal_post(client, "/scrape/v1/url", &body).await?;
    let title = resp
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let content = resp.get("content").and_then(Value::as_str).unwrap_or("");
    Ok(if title.is_empty() {
        content.to_string()
    } else {
        format!("# {title}\n\n{content}")
    })
}

/// Shared POST helper. Resolves the API key from the live process
/// env (HAL_API_KEY), POSTs `body` to `<base>/<path>`, returns the
/// raw response body on 2xx or maps to `Error::Tool` with the HAL
/// error detail on non-2xx. Network / parse errors map to
/// `Error::Tool` with the underlying message.
async fn hal_post(client: &reqwest::Client, path: &str, body: &Value) -> Result<Value> {
    // Gateway mode: POST to `{gateway}/hal{path}` with the gateway
    // bearer; the gateway injects the real X-API-Key. Direct mode: hit
    // HAL directly with the local key.
    let req = if let Some(gw) = crate::tools::gateway_route() {
        crate::multi_tenant::attach_member(client.post(format!("{}/hal{path}", gw.base)))
            .header("authorization", format!("Bearer {}", gw.token))
    } else if crate::shared::gateway_providers_locked() {
        return Err(Error::Tool(
            "HAL on this deployment goes through its gateway — sign in first".into(),
        ));
    } else {
        let key = std::env::var("HAL_API_KEY").map_err(|_| {
            Error::Tool(
                "HAL_API_KEY not set — paste it in Settings → Providers (HAL Public API)".into(),
            )
        })?;
        if key.trim().is_empty() {
            return Err(Error::Tool("HAL_API_KEY is empty".into()));
        }
        client
            .post(format!("{HAL_BASE_URL}{path}"))
            .header("X-API-Key", key)
    };
    let resp = req
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("HAL request failed: {e}")))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| Error::Tool(format!("HAL response read failed: {e}")))?;
    if !status.is_success() {
        // Try to surface HAL's structured `detail` field when present;
        // otherwise pass the raw body through.
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("detail")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
            })
            .unwrap_or(text);
        return Err(Error::Tool(format!("HAL {status}: {detail}")));
    }
    serde_json::from_str(&text).map_err(|e| Error::Tool(format!("HAL JSON parse failed: {e}")))
}

// ─── YouTubeTranscript ────────────────────────────────────────────────

pub struct YouTubeTranscriptTool {
    client: reqwest::Client,
}

impl YouTubeTranscriptTool {
    pub fn new() -> Self {
        Self {
            client: build_client(),
        }
    }
}

impl Default for YouTubeTranscriptTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for YouTubeTranscriptTool {
    fn name(&self) -> &'static str {
        "YouTubeTranscript"
    }

    fn description(&self) -> &'static str {
        "Fetch a YouTube video's transcript via HAL's public API. \
         Pass either `url` (any standard YouTube URL — youtube.com/watch, youtu.be, \
         /embed/, /v/) or `video_id` (the 11-char ID). Optional `languages` is an \
         ordered preference list (default `[\"en\", \"th\"]`); the first available \
         caption track is returned. Set `with_timestamps: true` to get raw segments \
         with start/duration; default is the joined text with `[m:ss]` timestamps."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Full YouTube URL — accepts watch, youtu.be, embed, /v/ shapes"
                },
                "video_id": {
                    "type": "string",
                    "description": "11-char video ID (alternative to url)"
                },
                "languages": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Preferred caption languages in order. Default: [\"en\", \"th\"]"
                },
                "with_timestamps": {
                    "type": "boolean",
                    "description": "Return raw segments with start/duration. Default: false"
                }
            }
        })
    }

    fn requires_env(&self) -> &'static [&'static str] {
        // HAL_API_KEY is in GATEWAY_SERVED_ENVS, so `tool_is_available`
        // keeps these visible whenever the gateway is active (desktop proxy
        // or cloud pod) even with no local key — `hal_post` then routes
        // through the gateway. Direct mode needs the local key.
        &["HAL_API_KEY"]
    }

    async fn call(&self, input: Value) -> Result<String> {
        let url = input.get("url").and_then(Value::as_str);
        let video_id = input.get("video_id").and_then(Value::as_str);
        if url.is_none() && video_id.is_none() {
            return Err(Error::Tool("either 'url' or 'video_id' is required".into()));
        }
        let mut body = json!({});
        if let Some(u) = url {
            body["url"] = json!(u);
        }
        if let Some(v) = video_id {
            body["video_id"] = json!(v);
        }
        if let Some(langs) = input.get("languages") {
            body["languages"] = langs.clone();
        }
        if let Some(ts) = input.get("with_timestamps").and_then(Value::as_bool) {
            body["with_timestamps"] = json!(ts);
        }
        let resp = hal_post(&self.client, "/youtube/v1/transcript", &body).await?;
        // The schema differs based on with_timestamps; just hand the
        // model the JSON. It's already structured (video_id, title,
        // channel, language, transcript|segments).
        Ok(serde_json::to_string_pretty(&resp).unwrap_or_else(|_| resp.to_string()))
    }
}

// ─── WebScrape ────────────────────────────────────────────────────────

pub struct WebScrapeTool {
    client: reqwest::Client,
}

impl WebScrapeTool {
    pub fn new() -> Self {
        Self {
            client: build_client(),
        }
    }
}

impl Default for WebScrapeTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebScrapeTool {
    fn name(&self) -> &'static str {
        "WebScrape"
    }

    fn description(&self) -> &'static str {
        "Render a web page in a headless browser via HAL's public API and \
         return its content as Markdown. `WebFetch` already runs HAL scrape \
         (combined with a plain GET) on every call when `HAL_API_KEY` is \
         set — reach for this tool directly only when you need the advanced \
         HAL parameters: `wait_for` (CSS selector to wait for before scraping), \
         `scroll_to_bottom` (load lazy content), `remove_selectors` (strip nav / \
         ads / cookie banners), `output_format` (markdown / html_markdown / \
         json). For ordinary page reads, prefer `WebFetch` so the model also \
         gets the raw plain-GET payload alongside HAL's rendered output."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "URL to scrape"},
                "wait_for": {
                    "type": "string",
                    "description": "CSS selector to wait for before scraping"
                },
                "wait_timeout": {
                    "type": "integer",
                    "description": "Timeout (ms) for wait_for. Default: 30000"
                },
                "scroll_to_bottom": {
                    "type": "boolean",
                    "description": "Scroll the page to load lazy content. Default: false"
                },
                "remove_selectors": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "CSS selectors to strip (e.g. [\"nav\", \".ads\"])"
                },
                "output_format": {
                    "type": "string",
                    "enum": ["markdown", "html_markdown", "json"],
                    "description": "Output shape. Default: markdown"
                }
            },
            "required": ["url"]
        })
    }

    fn requires_env(&self) -> &'static [&'static str] {
        // HAL_API_KEY is in GATEWAY_SERVED_ENVS, so `tool_is_available`
        // keeps these visible whenever the gateway is active (desktop proxy
        // or cloud pod) even with no local key — `hal_post` then routes
        // through the gateway. Direct mode needs the local key.
        &["HAL_API_KEY"]
    }

    async fn call(&self, input: Value) -> Result<String> {
        let url = req_str(&input, "url")?;
        crate::net_guard::guard(url)
            .await
            .map_err(|e| Error::Tool(format!("WebScrape: {e}")))?;
        let mut body = json!({"url": url});
        for field in [
            "wait_for",
            "wait_timeout",
            "scroll_to_bottom",
            "remove_selectors",
            "output_format",
        ] {
            if let Some(v) = input.get(field) {
                body[field] = v.clone();
            }
        }
        let resp = hal_post(&self.client, "/scrape/v1/url", &body).await?;
        // Hand the model the structured JSON: title, content,
        // metadata, scraped_at. Keeps the metadata accessible without
        // a second tool call.
        Ok(serde_json::to_string_pretty(&resp).unwrap_or_else(|_| resp.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_declares_hal_env() {
        let t = YouTubeTranscriptTool::new();
        assert_eq!(t.requires_env(), &["HAL_API_KEY"]);
        assert_eq!(t.name(), "YouTubeTranscript");
    }

    #[test]
    fn webscrape_declares_hal_env() {
        let t = WebScrapeTool::new();
        assert_eq!(t.requires_env(), &["HAL_API_KEY"]);
        assert_eq!(t.name(), "WebScrape");
    }

    #[tokio::test]
    async fn youtube_rejects_missing_url_and_video_id() {
        let prev = std::env::var("HAL_API_KEY").ok();
        std::env::set_var("HAL_API_KEY", "test-key");
        let t = YouTubeTranscriptTool::new();
        let err = t.call(json!({})).await.unwrap_err();
        assert!(
            format!("{err}").contains("'url' or 'video_id'"),
            "expected url-or-video_id error, got: {err}"
        );
        match prev {
            Some(v) => std::env::set_var("HAL_API_KEY", v),
            None => std::env::remove_var("HAL_API_KEY"),
        }
    }

    #[tokio::test]
    async fn webscrape_rejects_missing_url() {
        let prev = std::env::var("HAL_API_KEY").ok();
        std::env::set_var("HAL_API_KEY", "test-key");
        let t = WebScrapeTool::new();
        let err = t.call(json!({})).await.unwrap_err();
        assert!(
            format!("{err}").contains("url"),
            "expected url-required error, got: {err}"
        );
        match prev {
            Some(v) => std::env::set_var("HAL_API_KEY", v),
            None => std::env::remove_var("HAL_API_KEY"),
        }
    }
}
