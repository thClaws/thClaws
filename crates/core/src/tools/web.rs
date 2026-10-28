use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

/// Hard timeout for the plain-HTTP WebFetch path. M6.23 BUG WT1:
/// pre-fix the client had no timeout, so a hanging server / slow DNS
/// would stall the agent indefinitely (and the agent's cancel token
/// wasn't observed at the `.send().await` boundary). 30s is generous
/// for normal pages while preventing pathological hangs.
const WEB_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

pub struct WebFetchTool {
    /// Client for the plain HTTP-GET path. 30s timeout — page server
    /// is on the hot path.
    client: reqwest::Client,
    /// Separate client for the HAL scrape path. HAL's `scroll_to_bottom`
    /// + JS render can legitimately run 60s+, so the round-trip
    /// timeout is bumped to 90s (same as `hal::HAL_TIMEOUT`).
    hal_client: reqwest::Client,
}

impl WebFetchTool {
    const BASE_DESCRIPTION: &'static str =
        "Fetch a URL and return text. When `HAL_API_KEY` is set, runs \
         **both** a HAL headless-browser scrape **and** a plain HTTP \
         GET in parallel, returning a single combined response with \
         each section clearly labelled — the HAL section gives you \
         clean Markdown from JS-rendered content, the plain GET \
         section gives you the raw response body (preserves JSON, \
         headers-style content, and anything browser-rendering would \
         distort). This dual view lets you pick the right slice per \
         URL: HAL for SPA / docs / news / blog content; plain GET for \
         JSON APIs / xml / robots.txt / sitemap-style payloads. \
         If only one of the two paths succeeds, the result still \
         comes back with a `[note: ...]` line explaining which one \
         dropped. When `HAL_API_KEY` is absent, this is just a plain \
         HTTP GET. Set `prefer_raw: true` to skip HAL entirely and \
         get only the plain GET section (faster, half the tokens) — \
         useful when you know the URL is a JSON endpoint or similar. \
         `max_bytes` (default 100 KB) caps each section independently.";

    pub fn new() -> Self {
        let client = crate::net_guard::guarded_client_builder()
            .timeout(WEB_FETCH_TIMEOUT)
            .user_agent(crate::version::WEB_USER_AGENT)
            .build()
            .expect("http client");
        // Reuse hal's builder so the timeout / TLS settings stay in
        // lockstep with the dedicated WebScrape tool.
        let hal_client = crate::tools::hal::build_client();
        Self { client, hal_client }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "WebFetch"
    }

    fn description(&self) -> &'static str {
        static ON_BOX: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        crate::aiserver::web_tool_description(Self::BASE_DESCRIPTION, &ON_BOX)
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "The URL to fetch"},
                "max_bytes": {
                    "type": "integer",
                    "description": "Max response size in bytes (default 102400). Applied to both HAL and plain-GET paths."
                },
                "prefer_raw": {
                    "type": "boolean",
                    "description": "Force a plain HTTP GET even if HAL_API_KEY is set. Use for JSON APIs, files, or anything where browser-rendered Markdown would be wrong. Default: false."
                }
            },
            "required": ["url"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let url = req_str(&input, "url")?;
        crate::net_guard::guard(url)
            .await
            .map_err(|e| Error::Tool(format!("WebFetch: {e}")))?;
        let max_bytes = input
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(102_400) as usize;
        let prefer_raw = input
            .get("prefer_raw")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // No HAL key, or model opted out — single plain GET, done.
        if prefer_raw || !crate::tools::hal::hal_available() {
            return plain_get(&self.client, url, max_bytes).await;
        }

        // Combined path. Two independent requests fired in parallel
        // via `tokio::join!` — HAL's headless-browser scrape AND a
        // plain HTTP GET. The model receives both labelled views in
        // one response and decides which slice answers its question.
        // Pre-fix this was a fallback-style routing (HAL primary,
        // plain on failure) — but that hid the plain payload on the
        // happy path, which was the wrong choice for URLs where the
        // raw body matters (JSON APIs, sitemaps, anything HAL renders
        // through a browser tab that mangles the structure). The
        // combined view trades wall-clock + tokens for a complete
        // picture; `prefer_raw: true` is the opt-out for callers
        // that don't need the HAL section.
        let (hal_result, plain_result) = tokio::join!(
            crate::tools::hal::scrape_via_hal(&self.hal_client, url),
            plain_get(&self.client, url, max_bytes),
        );

        match (hal_result, plain_result) {
            (Err(hal_err), Err(plain_err)) => Err(Error::Tool(format!(
                "fetch {url} failed on both paths — HAL: {hal_err}; plain GET: {plain_err}"
            ))),
            (Ok(hal_body), Err(plain_err)) => {
                let hal_section = truncate_for_bytes(&hal_body, max_bytes);
                Ok(format!(
                    "[via HAL scrape — JS-rendered + extracted to Markdown]\n\n\
                     {hal_section}\n\n\
                     ---\n\n\
                     [note: plain HTTP GET also attempted but failed: {plain_err}]"
                ))
            }
            (Err(hal_err), Ok(plain_body)) => Ok(format!(
                "[note: HAL scrape failed: {hal_err}; returning plain GET only]\n\n\
                 [via plain HTTP GET — raw response body]\n\n\
                 {plain_body}"
            )),
            (Ok(hal_body), Ok(plain_body)) => {
                let hal_section = truncate_for_bytes(&hal_body, max_bytes);
                Ok(format!(
                    "[via HAL scrape — JS-rendered + extracted to Markdown]\n\n\
                     {hal_section}\n\n\
                     ---\n\n\
                     [via plain HTTP GET — raw response body]\n\n\
                     {plain_body}"
                ))
            }
        }
    }
}

/// Plain HTTP-GET path: GET, status-check, read text, truncate.
/// Shared between the "HAL unavailable" and "HAL failed" code paths.
async fn plain_get(client: &reqwest::Client, url: &str, max_bytes: usize) -> Result<String> {
    let resp = client
        .get(url)
        .header("user-agent", "thclaws/0.1")
        .send()
        .await
        .map_err(|e| Error::Tool(format!("fetch {url}: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(Error::Tool(format!("fetch {url}: HTTP {status}")));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| Error::Tool(format!("read body {url}: {e}")))?;

    Ok(truncate_for_bytes(&strip_html_noise(&text), max_bytes))
}

/// Drop the parts of an HTML body that cannot help answer a question, so the
/// byte budget is spent on content instead of markup.
///
/// Measured on two real `WebFetch` results that spilled to disk during a
/// browse session: one 95,925-byte page was 44% `<svg>` and 32% `<script>`,
/// leaving 7.9% actual text; another at 102,555 bytes was 58% `<style>`.
/// Both blew the 50 KB truncation limit and made the model read icon path
/// coordinates instead of the article. Stripped, both fit whole.
///
/// Only HTML is touched. The plain-GET section exists precisely so JSON
/// APIs, sitemaps and XML come back verbatim, and a `<script>`-shaped run of
/// bytes inside those is content, not noise.
fn strip_html_noise(body: &str) -> String {
    if !looks_like_html(body) {
        return body.to_string();
    }
    static RE: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    let res = RE.get_or_init(|| {
        [
            r"(?is)<script\b[^>]*>.*?</script\s*>",
            r"(?is)<style\b[^>]*>.*?</style\s*>",
            r"(?is)<svg\b[^>]*>.*?</svg\s*>",
            r"(?is)<noscript\b[^>]*>.*?</noscript\s*>",
            r"(?s)<!--.*?-->",
        ]
        .iter()
        .filter_map(|p| regex::Regex::new(p).ok())
        .collect()
    });
    let mut out = body.to_string();
    for re in res {
        out = re.replace_all(&out, "").into_owned();
    }
    let saved = body.len().saturating_sub(out.len());
    // Say so rather than silently handing back something labelled "raw
    // response body" — a reader comparing byte counts should not have to
    // guess where the difference went.
    if saved > 0 {
        out.push_str(&format!(
            "\n\n[stripped {} KB of script/style/svg/comment markup from this HTML \
             so the budget goes to content]",
            saved / 1024
        ));
    }
    out
}

/// Cheap sniff for an HTML body. Looks only at the head of the document so a
/// large JSON payload that happens to embed markup later is left alone.
fn looks_like_html(body: &str) -> bool {
    let head: String = body.chars().take(600).collect::<String>().to_lowercase();
    let head = head.trim_start();
    head.starts_with("<!doctype html")
        || head.starts_with("<html")
        || (head.contains("<html") && head.contains('>'))
}

/// Byte-bounded truncation that respects UTF-8 char boundaries —
/// `&text[..cut]` panics if `cut` lands inside a multibyte char.
fn truncate_for_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n... [truncated at {} bytes, {} total]",
        &text[..cut],
        max_bytes,
        text.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Crate-wide env lock alias. These tests mutate `HAL_API_KEY`,
    /// which the prompt builder's `services_prompt_section()` reads —
    /// a local lock here wouldn't coordinate with the prompt-builder
    /// test and the HAL bullet would flip between its two refresh
    /// calls.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::kms::test_env_lock()
    }

    #[test]
    fn description_mentions_combined_hal_behavior() {
        let t = WebFetchTool::new();
        let desc = t.description();
        assert!(
            desc.contains("HAL_API_KEY"),
            "description should hint at HAL behavior so the model knows"
        );
        // Combined-mode language — model needs to understand both
        // sections come back, not "either HAL or plain GET".
        assert!(
            desc.contains("both") || desc.contains("parallel"),
            "description must surface the combined fetch behavior so the model isn't surprised by two labelled sections in the response: {desc}"
        );
        assert!(
            desc.contains("prefer_raw"),
            "description should mention the opt-out param"
        );
    }

    #[test]
    fn schema_advertises_prefer_raw() {
        let t = WebFetchTool::new();
        let schema = t.input_schema();
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("schema has properties");
        assert!(props.contains_key("url"));
        assert!(props.contains_key("max_bytes"));
        assert!(
            props.contains_key("prefer_raw"),
            "prefer_raw must be in the public schema or the model can't opt out of HAL"
        );
    }

    #[test]
    fn requires_approval_stays_true() {
        // WebFetch hits the network — keep the approval gate intact
        // even after the HAL routing was added. Don't silently relax it.
        let t = WebFetchTool::new();
        assert!(t.requires_approval(&json!({"url": "http://x"})));
    }

    /// Two real WebFetch results that spilled during a browse session were
    /// 92% and 81% markup — svg icon paths, stylesheets, inline scripts —
    /// and the model spent its 50 KB budget reading path coordinates while
    /// the article itself was 7.5 KB. Strip what cannot answer a question.
    #[test]
    fn html_noise_is_stripped_before_the_budget_is_spent() {
        let html = format!(
            "<!doctype html><html><head><style>{}</style>\
             <script>{}</script></head><body><svg><path d=\"{}\"/></svg>\
             <p>the answer is 42</p><!-- {} --></body></html>",
            "x".repeat(3000),
            "y".repeat(3000),
            "1.5 2.5 ".repeat(400),
            "z".repeat(500),
        );
        let out = super::strip_html_noise(&html);
        assert!(out.contains("the answer is 42"), "content must survive");
        for junk in ["<style", "<script", "<svg", "<path", "<!--"] {
            assert!(
                !out.contains(junk),
                "{junk} should have been stripped: {out:.200}"
            );
        }
        assert!(
            out.len() < html.len() / 4,
            "expected a large cut, got {}",
            out.len()
        );
        assert!(out.contains("stripped"), "must disclose that it stripped");
    }

    /// The plain-GET section exists so JSON APIs and sitemaps come back
    /// verbatim. A body that is not HTML must pass through byte-for-byte,
    /// even when it contains markup-shaped text.
    #[test]
    fn non_html_bodies_pass_through_untouched() {
        for body in [
            r#"{"note":"<script>not really a script</script>","n":1}"#,
            "<?xml version=\"1.0\"?><rss><item><title>hi</title></item></rss>",
            "plain text, no markup at all",
        ] {
            assert_eq!(
                super::strip_html_noise(body),
                body,
                "must not touch {body:.40}"
            );
        }
    }

    #[test]
    fn truncate_respects_utf8_boundary() {
        // "héllo" is 6 bytes (é = 2 bytes); cutting at 2 lands inside
        // the multibyte char and would panic on `&s[..2]` without the
        // char-boundary loop.
        let text = "héllo world";
        let out = truncate_for_bytes(text, 2);
        // Must not have panicked. The exact prefix should be "h" since
        // 2 is mid-é, so the loop walks back to 1.
        assert!(out.starts_with("h"));
        assert!(out.contains("[truncated"));
    }

    #[test]
    fn truncate_passthrough_when_within_budget() {
        let text = "small payload";
        let out = truncate_for_bytes(text, 1024);
        assert_eq!(out, text);
    }

    #[test]
    fn hal_unavailable_when_env_missing() {
        let _g = env_lock();
        let prev = std::env::var("HAL_API_KEY").ok();
        std::env::remove_var("HAL_API_KEY");
        assert!(!crate::tools::hal::hal_available());
        if let Some(p) = prev {
            std::env::set_var("HAL_API_KEY", p);
        }
    }

    #[test]
    fn hal_unavailable_when_env_blank() {
        let _g = env_lock();
        let prev = std::env::var("HAL_API_KEY").ok();
        std::env::set_var("HAL_API_KEY", "   ");
        assert!(
            !crate::tools::hal::hal_available(),
            "whitespace-only key should not count as available"
        );
        match prev {
            Some(p) => std::env::set_var("HAL_API_KEY", p),
            None => std::env::remove_var("HAL_API_KEY"),
        }
    }

    #[test]
    fn hal_available_when_env_set() {
        let _g = env_lock();
        let prev = std::env::var("HAL_API_KEY").ok();
        std::env::set_var("HAL_API_KEY", "test-key");
        assert!(crate::tools::hal::hal_available());
        match prev {
            Some(p) => std::env::set_var("HAL_API_KEY", p),
            None => std::env::remove_var("HAL_API_KEY"),
        }
    }
}
