//! `PaidFetch`: an HTTP request that may pay for itself via x402 (testnet).
//! A 402 is quoted, authorised by a mandate or a human, and only then
//! signed and retried — through `payments::pay_with`, the one path money
//! moves on. The wallet key stays inside thClaws.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use crate::payments::{self, x402, PaymentRequest};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

const MAX_BYTES: usize = 100_000;

pub struct PaidFetchTool {
    client: reqwest::Client,
}

impl PaidFetchTool {
    pub fn new() -> Self {
        Self {
            client: crate::net_guard::guarded_client_builder()
                .timeout(Duration::from_secs(60))
                .user_agent(crate::version::WEB_USER_AGENT)
                .build()
                .expect("http client"),
        }
    }
}

impl Default for PaidFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

/// A paid image API answers with `image_base64`; keep the picture as a file in
/// the workspace and give the model its path instead of megabytes of base64.
fn save_images(s: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(s) else {
        return s.to_string();
    };
    let Some(b64) = v.get("image_base64").and_then(Value::as_str) else {
        return s.to_string();
    };
    use base64::Engine as _;
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
        return s.to_string();
    };
    let ext = match v.get("mime_type").and_then(Value::as_str) {
        Some("image/jpeg") => "jpg",
        Some("image/webp") => "webp",
        _ => "png",
    };
    let name = format!(
        "paid-image-{}.{ext}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    );
    // Agents share the workspace's files: under a v4 host the agent runs in
    // `.thclaws/bots/<name>/`, so save at the workspace root above it.
    let cwd = std::env::current_dir().unwrap_or_default();
    let root = cwd
        .ancestors()
        .find(|a| a.join(".thclaws").join("bots").is_dir() && cwd.starts_with(a.join(".thclaws")))
        .map(std::path::Path::to_path_buf)
        .unwrap_or(cwd);
    let path = root.join(&name);
    match std::fs::write(&path, &bytes) {
        Ok(()) => v["image_base64"] = json!(format!("[saved to {name}, {} bytes]", bytes.len())),
        Err(e) => v["image_base64"] = json!(format!("[not saved: {e}]")),
    }
    v.to_string()
}

fn clip(s: &str) -> String {
    let s = &save_images(s);
    if s.len() <= MAX_BYTES {
        return s.to_string();
    }
    let mut end = MAX_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated at {MAX_BYTES} bytes]", &s[..end])
}

#[async_trait]
impl Tool for PaidFetchTool {
    fn name(&self) -> &'static str {
        "PaidFetch"
    }

    fn description(&self) -> &'static str {
        "HTTP request to an API that may charge per call (x402 or MPP). \
         If the server answers 402 Payment Required, the price is checked against \
         the user's payment mandate or shown to the user for approval; only then is \
         the payment signed and the request retried. Returns the response body and, \
         when paid, a 💳 receipt line (amount, payee, budget left). A denial is final \
         for that call: tell the user, don't retry with another tool."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "The URL to call"},
                "method": {"type": "string", "enum": ["GET", "POST"], "description": "Default GET"},
                "body": {"type": "string", "description": "Request body (POST), usually JSON"},
                "content_type": {"type": "string", "description": "Default application/json when a body is given"},
                "purpose": {"type": "string", "description": "One line: what this purchase is for (shown to the user)"}
            },
            "required": ["url"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    fn approval_summary(&self, input: &Value) -> Option<String> {
        let url = input.get("url").and_then(Value::as_str)?;
        Some(format!("{url} — may ask to pay"))
    }

    async fn call(&self, input: Value) -> Result<String> {
        let url = req_str(&input, "url")?;
        crate::net_guard::guard(url)
            .await
            .map_err(|e| Error::Tool(format!("PaidFetch: {e}")))?;
        let method = match input.get("method").and_then(Value::as_str).unwrap_or("GET") {
            m if m.eq_ignore_ascii_case("POST") => reqwest::Method::POST,
            _ => reqwest::Method::GET,
        };
        let body = input
            .get("body")
            .and_then(Value::as_str)
            .map(str::to_string);
        let content_type = input
            .get("content_type")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| body.as_ref().map(|_| "application/json".to_string()));
        let purpose = input.get("purpose").and_then(Value::as_str);
        fetch(&self.client, method, url, body, content_type, purpose, None)
            .await
            .map_err(Error::Tool)
    }
}

pub(crate) async fn fetch(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    body: Option<String>,
    content_type: Option<String>,
    purpose: Option<&str>,
    key: Option<k256::ecdsa::SigningKey>,
) -> std::result::Result<String, String> {
    let mut rb = client.request(method.clone(), url);
    if let Some(b) = &body {
        rb = rb.body(b.clone());
        if let Some(ct) = &content_type {
            rb = rb.header(reqwest::header::CONTENT_TYPE, ct);
        }
    }
    let resp = rb
        .send()
        .await
        .map_err(|e| format!("PaidFetch {url}: {e}"))?;
    let status = resp.status().as_u16();
    if status != 402 {
        let text = resp.text().await.unwrap_or_default();
        return Ok(format!("HTTP {status}\n\n{}", clip(&text)));
    }
    if let Some(ch) = resp
        .headers()
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(payments::mpp::parse_challenge)
    {
        let adapter =
            payments::mpp::MppAdapter::new(client.clone(), method, url, body, content_type, ch);
        let req = PaymentRequest {
            resource: Some(url.to_string()),
            challenge: adapter.challenge.request.clone(),
            hint: json!({"purpose": purpose}),
        };
        let paid = payments::pay_with(&adapter, &req).await?;
        let (status, text) = adapter
            .response
            .lock()
            .unwrap()
            .take()
            .unwrap_or((200, String::new()));
        return Ok(format!(
            "{}\n\nHTTP {status}\n\n{}",
            paid.line(),
            clip(&text)
        ));
    }
    let header = resp
        .headers()
        .get("PAYMENT-REQUIRED")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let text = resp.text().await.unwrap_or_default();
    let (version, requirement) = x402::parse_challenge(header.as_deref(), &text)?;
    let key = match key {
        Some(k) => k,
        None => x402::load_wallet()?,
    };
    let mut adapter = x402::X402Adapter::new(key, client.clone(), method, url, body, content_type);
    adapter.set_requirement(version, &requirement);
    let mut challenge = requirement;
    if let Some(p) = purpose {
        if challenge["description"].as_str().is_none_or(str::is_empty) {
            challenge["description"] = json!(p);
        }
    }
    let req = PaymentRequest {
        resource: Some(url.to_string()),
        challenge,
        hint: json!({"purpose": purpose}),
    };
    let paid = payments::pay_with(&adapter, &req).await?;
    let (status, text) = adapter
        .response
        .lock()
        .unwrap()
        .take()
        .unwrap_or((200, String::new()));
    Ok(format!(
        "{}\n\nHTTP {status}\n\n{}",
        paid.line(),
        clip(&text)
    ))
}
