//! `FetchImages` — download every remote image a markdown file references and
//! rewrite the links to the local copies. The deterministic half of web/content
//! extraction: the model writes `<slug>.md` with images left as their original
//! URLs, then calls this to localize them (dedupe by content, extension from the
//! content-type, atomic link rewrite) — no hallucinated filenames, no silent
//! skips. Gated behind the `content-extractor` subagent (which allow-lists it).

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

const GATE: &str = "content-extractor";
const DEFAULT_TIMEOUT: u64 = 20;
const DEFAULT_MAX_MB: u64 = 25;

pub struct FetchImagesTool {
    client: reqwest::Client,
}

impl FetchImagesTool {
    pub fn new() -> Self {
        let client = crate::net_guard::guarded_client_builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT))
            .build()
            .expect("http client");
        Self { client }
    }
}

impl Default for FetchImagesTool {
    fn default() -> Self {
        Self::new()
    }
}

fn content_type_ext(ct: &str) -> Option<&'static str> {
    match ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/svg+xml" => Some("svg"),
        "image/avif" => Some("avif"),
        "image/bmp" => Some("bmp"),
        "image/x-icon" | "image/vnd.microsoft.icon" => Some("ico"),
        "image/tiff" => Some("tiff"),
        _ => None,
    }
}

fn ext_for(abs_url: &str, content_type: Option<&str>) -> String {
    if let Some(e) = content_type.and_then(content_type_ext) {
        return e.to_string();
    }
    // Fall back to the URL path's extension.
    if let Ok(u) = url::Url::parse(abs_url) {
        if let Some(seg) = u.path_segments().and_then(|s| s.last()) {
            if let Some((_, ext)) = seg.rsplit_once('.') {
                let ext = ext.to_ascii_lowercase();
                if !ext.is_empty()
                    && ext.len() <= 5
                    && ext.chars().all(|c| c.is_ascii_alphanumeric())
                {
                    return ext;
                }
            }
        }
    }
    "img".to_string()
}

fn slugify(text: &str, fallback: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in text.to_ascii_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let out = out.trim_matches('-');
    let out = if out.is_empty() { fallback } else { out };
    out.chars().take(48).collect()
}

fn is_local(url: &str) -> bool {
    url.starts_with("images/") || url.starts_with("./images/")
}

/// Collect image URLs (markdown `![](url)` + stray `<img src>`), de-duplicated,
/// first-seen order.
fn collect_refs(md: &str) -> Vec<String> {
    let md_img =
        regex::Regex::new(r#"!\[[^\]]*\]\(([^)\s]+)(?:\s+(?:"[^"]*"|'[^']*'|\([^)]*\)))?\)"#)
            .unwrap();
    let html_img = regex::Regex::new(r#"(?i)<img\b[^>]*?\bsrc=["']([^"']+)["']"#).unwrap();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for caps in md_img.captures_iter(md) {
        let u = caps[1].to_string();
        if seen.insert(u.clone()) {
            out.push(u);
        }
    }
    for caps in html_img.captures_iter(md) {
        let u = caps[1].to_string();
        if seen.insert(u.clone()) {
            out.push(u);
        }
    }
    out
}

/// Swap every mapped image URL for its local path — markdown `![](url)` (title
/// preserved) and stray `<img src>`. Unmapped and already-local links untouched.
fn rewrite_links(md: &str, mapping: &HashMap<String, String>) -> String {
    let md_img =
        regex::Regex::new(r#"(!\[[^\]]*\]\()([^)\s]+)((?:\s+(?:"[^"]*"|'[^']*'|\([^)]*\)))?\))"#)
            .unwrap();
    let step1 = md_img.replace_all(md, |caps: &regex::Captures| match mapping.get(&caps[2]) {
        Some(local) => format!("{}{}{}", &caps[1], local, &caps[3]),
        None => caps[0].to_string(),
    });
    let html_img = regex::Regex::new(r#"(?i)(<img\b[^>]*?\bsrc=["'])([^"']+)(["'])"#).unwrap();
    html_img
        .replace_all(&step1, |caps: &regex::Captures| {
            match mapping.get(&caps[2]) {
                Some(local) => format!("{}{}{}", &caps[1], local, &caps[3]),
                None => caps[0].to_string(),
            }
        })
        .into_owned()
}

#[async_trait]
impl Tool for FetchImagesTool {
    fn name(&self) -> &'static str {
        "FetchImages"
    }

    fn description(&self) -> &'static str {
        "Download every remote image referenced by a markdown file and rewrite \
         the links to the local copies — the reliable half of content extraction. \
         Point it at a markdown file you've written with images still as their \
         original URLs; it downloads each (deduping identical images by content, \
         picking the extension from the HTTP content-type), saves them under an \
         `images/` folder next to the file, and edits the file IN PLACE to point \
         at the local paths. Relative image links (`/media/x.png`) need `base_url` \
         (the page the markdown came from) to resolve. Re-runnable: links already \
         rewritten to `images/...` are left untouched. Returns a JSON summary \
         {found, downloaded, failed, failed_urls}."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "markdown_path": {
                    "type": "string",
                    "description": "Path to the markdown file to process. Edited in place; images are saved to a sibling images/ folder."
                },
                "base_url": {
                    "type": "string",
                    "description": "Page URL the markdown was extracted from. Required only if the file has relative image links; absolute http(s) images need it not."
                },
                "timeout_secs": {"type": "integer", "description": "Per-image download timeout (default 20)."},
                "max_mb": {"type": "integer", "description": "Skip any single image larger than this many MB (default 25)."}
            },
            "required": ["markdown_path"]
        })
    }

    fn requires_gate(&self) -> Option<&'static str> {
        Some(GATE)
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        // Sandbox the path — FetchImages reads AND rewrites the file in
        // place and writes a sibling `images/` dir. In multiuser mode
        // approval is auto-granted, so without this a subagent could
        // read/rewrite any absolute or co-tenant path. `check_write`
        // confines it to the per-user workspace root (rejects absolute /
        // `..` / symlink-escape / another member's subtree).
        let md_path = crate::sandbox::Sandbox::check_write(req_str(&input, "markdown_path")?)?;
        let base_url = input
            .get("base_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let timeout = Duration::from_secs(
            input
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT),
        );
        let max_bytes = input
            .get("max_mb")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_MB) as usize
            * 1024
            * 1024;

        let md = std::fs::read_to_string(&md_path).map_err(|e| {
            Error::Tool(format!(
                "FetchImages: can't read {}: {e}",
                md_path.display()
            ))
        })?;

        let refs = collect_refs(&md);
        let remote: Vec<String> = refs
            .into_iter()
            .filter(|u| !u.starts_with("data:") && !is_local(u))
            .collect();

        // Relative links (no scheme) can only resolve with a base_url.
        let relative: Vec<&String> = remote
            .iter()
            .filter(|u| url::Url::parse(u).is_err())
            .collect();
        if !relative.is_empty() && base_url.is_empty() {
            let sample: Vec<&str> = relative.iter().take(8).map(|s| s.as_str()).collect();
            return Err(Error::Tool(format!(
                "FetchImages: file has relative image links but no base_url to resolve them:\n  {}",
                sample.join("\n  ")
            )));
        }

        // Always a sibling `images/` next to the markdown file — the rewritten
        // links are hardcoded `images/<file>`, so the save dir must match.
        let img_dir = md_path.parent().unwrap_or(Path::new(".")).join("images");
        std::fs::create_dir_all(&img_dir)
            .map_err(|e| Error::Tool(format!("FetchImages: mkdir {}: {e}", img_dir.display())))?;

        let base = if base_url.is_empty() {
            None
        } else {
            url::Url::parse(&base_url).ok()
        };

        let mut mapping: HashMap<String, String> = HashMap::new(); // original url -> images/<file>
        let mut by_hash: HashMap<String, String> = HashMap::new(); // content hash -> local path
        let mut failed: Vec<(String, String)> = Vec::new();
        let mut idx = 0usize;

        for url in &remote {
            let abs = match (&base, url::Url::parse(url)) {
                (_, Ok(u)) => u,
                (Some(b), Err(_)) => match b.join(url) {
                    Ok(u) => u,
                    Err(e) => {
                        failed.push((url.clone(), format!("bad url: {e}")));
                        continue;
                    }
                },
                (None, Err(e)) => {
                    failed.push((url.clone(), format!("bad url: {e}")));
                    continue;
                }
            };

            // SSRF guard — a source page's image URL is untrusted; refuse
            // private / loopback / link-local (metadata) targets.
            if let Err(e) = crate::net_guard::guard(abs.as_str()).await {
                failed.push((url.clone(), e));
                continue;
            }

            let resp = match self
                .client
                .get(abs.clone())
                .timeout(timeout)
                .header("user-agent", "thclaws/0.1 (content-extractor)")
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    failed.push((url.clone(), e.to_string()));
                    continue;
                }
            };
            if !resp.status().is_success() {
                failed.push((url.clone(), format!("HTTP {}", resp.status().as_u16())));
                continue;
            }
            // Reject oversized bodies from the Content-Length header before
            // buffering them into memory (the post-read check below still
            // covers chunked responses that omit the header).
            if let Some(len) = resp.content_length() {
                if len > max_bytes as u64 {
                    failed.push((url.clone(), format!("exceeds max_mb ({len} bytes)")));
                    continue;
                }
            }
            let ct = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let data = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    failed.push((url.clone(), e.to_string()));
                    continue;
                }
            };
            if data.len() > max_bytes {
                failed.push((
                    url.clone(),
                    format!("exceeds max_mb ({} bytes)", data.len()),
                ));
                continue;
            }

            let digest = format!("{:x}", Sha256::digest(&data));
            if let Some(local) = by_hash.get(&digest) {
                mapping.insert(url.clone(), local.clone());
                continue;
            }

            idx += 1;
            let stem = abs
                .path_segments()
                .and_then(|s| s.last())
                .and_then(|seg| seg.rsplit_once('.').map(|(a, _)| a).or(Some(seg)))
                .unwrap_or("");
            let name_hint = slugify(stem, &format!("image-{idx}"));
            let fname = format!(
                "{idx:03}-{name_hint}.{}",
                ext_for(abs.as_str(), ct.as_deref())
            );
            let rel = format!("images/{fname}");
            if let Err(e) = std::fs::write(img_dir.join(&fname), &data) {
                failed.push((url.clone(), format!("write failed: {e}")));
                continue;
            }
            by_hash.insert(digest, rel.clone());
            mapping.insert(url.clone(), rel);
        }

        // Rewrite the markdown in place (markdown images + stray <img src>).
        let new_md = rewrite_links(&md, &mapping);

        if new_md != md {
            std::fs::write(&md_path, &new_md).map_err(|e| {
                Error::Tool(format!("FetchImages: write {}: {e}", md_path.display()))
            })?;
        }

        Ok(json!({
            "found": remote.len(),
            "downloaded": mapping.len(),
            "failed": failed.len(),
            "failed_urls": failed.iter().map(|(u, r)| json!({"url": u, "reason": r})).collect::<Vec<_>>(),
            "images_dir": img_dir.to_string_lossy(),
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_refs_finds_md_and_html_deduped() {
        let md = "![a](https://x/1.png)\n<img src=\"https://x/2.jpg\">\n![again](https://x/1.png)\n![rel](/m/3.webp)";
        let refs = collect_refs(md);
        assert_eq!(
            refs,
            vec!["https://x/1.png", "/m/3.webp", "https://x/2.jpg"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn collect_refs_handles_single_quote_and_paren_titles() {
        // CommonMark allows "…", '…', and (…) image titles — all must be
        // collected (and rewritten), not silently skipped.
        let md = "![a](https://x/1.png 'cap')\n![b](https://x/2.png (cap))\n![c](https://x/3.png \"cap\")";
        assert_eq!(
            collect_refs(md),
            vec!["https://x/1.png", "https://x/2.png", "https://x/3.png"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        let mut m = HashMap::new();
        m.insert(
            "https://x/1.png".to_string(),
            "images/001-1.png".to_string(),
        );
        assert!(rewrite_links(md, &m).contains("![a](images/001-1.png 'cap')"));
    }

    #[test]
    fn ext_from_content_type_then_path() {
        assert_eq!(ext_for("https://x/a", Some("image/png; charset=x")), "png");
        assert_eq!(ext_for("https://x/a.JPG", None), "jpg");
        assert_eq!(ext_for("https://x/noext", None), "img");
    }

    #[test]
    fn is_local_matches_images_dir() {
        assert!(is_local("images/001-x.png"));
        assert!(is_local("./images/x.png"));
        assert!(!is_local("https://x/y.png"));
    }

    #[test]
    fn rewrite_swaps_mapped_keeps_rest() {
        let mut m = HashMap::new();
        m.insert(
            "https://x/1.png".to_string(),
            "images/001-1.png".to_string(),
        );
        let md = "![a](https://x/1.png \"t\")\n![b](https://x/2.png)\n<img src='https://x/1.png'>";
        let out = rewrite_links(md, &m);
        assert!(out.contains("![a](images/001-1.png \"t\")")); // title preserved
        assert!(out.contains("![b](https://x/2.png)")); // unmapped untouched
        assert!(out.contains("<img src='images/001-1.png'>")); // html rewritten
    }
}
