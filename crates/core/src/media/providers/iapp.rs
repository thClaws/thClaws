//! iApp Image Generation provider (Thai-first).
//!
//! One synchronous POST returns a base64 PNG, so this fits the
//! `ImageProvider` shape directly — no job/poll. It covers both modes:
//! text→image from `prompt`, and image→image by passing the source as
//! `reference_image` (composition is preserved; the prompt drives the
//! restyle).
//!
//! What it has that the other backends don't: **`text` + `font`**. The
//! lines in `req.text` are typeset by iApp with a real Thai face (19 to
//! pick from) instead of being drawn by the diffusion model, which is
//! what stops Thai script coming out as mangled glyphs. The API
//! re-checks the render and retries on its own — a text run takes ~60s
//! against ~10s for a plain one, hence the generous timeout.
//!
//! **BYOK-only for now**: there is no `iapp` segment on the thClaws
//! Gateway, so this resolves `IAPP_API_KEY` directly and errors clearly
//! when it's missing, rather than routing to a gateway path that would
//! 404. Auth is the vendor's own `apikey:` header, not Bearer. Adding a
//! gateway route + per-image metering (billed in iApp Credits, 1.5 IC
//! preview / 3 IC standard — no published USD rate) is the follow-up
//! that would let this move onto `resolve_endpoint` like the others.

use crate::error::{Error, Result};
use crate::media::provider::{ImageModelInfo, ImageProvider, ImageRequest, ImageResult};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

const IAPP_BASE: &str = "https://api.iapp.co.th";
const GEN_PATH: &str = "/v3/store/image-generation/iapp-image-generation/generate";

/// Vendor cap on `reference_image` (6 MB of *decoded* bytes).
const MAX_REFERENCE_BYTES: usize = 6 * 1024 * 1024;

/// The API caps a prompt at 1,500 characters.
const MAX_PROMPT_CHARS: usize = 1500;

/// Text overlay limits: at most 4 lines, 120 characters per line.
const MAX_TEXT_LINES: usize = 4;
const MAX_TEXT_LINE_CHARS: usize = 120;

/// The 19 typefaces iApp will set [`ImageRequest::text`] in. Public so
/// the tool schema can offer them as an enum — the model picking a face
/// that doesn't exist is otherwise a 400 at call time. Order is the
/// vendor's: bold/regular pairs first, then the display + script faces.
pub const FONTS: &[&str] = &[
    "kanit-bold",
    "kanit",
    "prompt-bold",
    "prompt",
    "sarabun-bold",
    "sarabun",
    "mitr",
    "chakrapetch-bold",
    "athiti-bold",
    "k2d-bold",
    "notosans",
    "pridi",
    "taviraj",
    "notoserif",
    "itim",
    "mali",
    "sriracha",
    "charm",
    "srisakdi",
];

const MODELS: &[ImageModelInfo] = &[ImageModelInfo {
    id: "iapp-image-generation",
    aliases: &["iapp", "iapp-image"],
    label: "iApp Image Generation (Thai text)",
}];

pub struct IappImageProvider;

impl IappImageProvider {
    /// Map the engine's portable aspect tiers onto iApp's five fixed
    /// `WxH` sizes. iApp has no quality/size tier of its own, so
    /// `req.size` (512 / 1K / 2K) has nothing to bind to and is ignored —
    /// every aspect already lands on the one resolution it offers.
    fn size(req: &ImageRequest) -> &'static str {
        match req.aspect_ratio.as_str() {
            "1:1" => "1024x1024",
            "9:16" => "768x1280",
            "3:4" => "1024x1536",
            "4:3" => "1536x1024",
            _ => "1280x768", // 16:9 default
        }
    }

    /// Validate the text overlay against the documented limits. Caught
    /// here so the caller gets the rule back instead of a bare 400 with
    /// the field name.
    fn check_text(text: &[String]) -> Result<()> {
        if text.len() > MAX_TEXT_LINES {
            return Err(Error::Tool(format!(
                "iapp: {} text lines — the API renders at most {MAX_TEXT_LINES}",
                text.len()
            )));
        }
        if let Some((i, line)) = text
            .iter()
            .enumerate()
            .find(|(_, l)| l.chars().count() > MAX_TEXT_LINE_CHARS)
        {
            return Err(Error::Tool(format!(
                "iapp: text line {} is {} chars — the API renders at most {MAX_TEXT_LINE_CHARS} per line",
                i + 1,
                line.chars().count()
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl ImageProvider for IappImageProvider {
    fn id(&self) -> &'static str {
        "iapp"
    }
    fn models(&self) -> &'static [ImageModelInfo] {
        MODELS
    }

    async fn generate(&self, req: &ImageRequest) -> Result<ImageResult> {
        let api_key = crate::media::provider::resolve_native_key(&["IAPP_API_KEY"]).ok_or_else(
            || {
                Error::Tool(
                    "no iApp API key — set IAPP_API_KEY. iApp is BYOK-only (the thClaws Gateway has no `iapp` route yet), so a gateway key does not cover it."
                        .to_string(),
                )
            },
        )?;

        if req.prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(Error::Tool(format!(
                "iapp: prompt is {} chars — the API caps it at {MAX_PROMPT_CHARS}",
                req.prompt.chars().count()
            )));
        }
        Self::check_text(&req.text)?;

        let mut body = json!({
            "prompt": req.prompt,
            "size": Self::size(req),
        });

        if !req.text.is_empty() {
            body["text"] = json!(req.text);
            if let Some(font) = req.font.as_deref().filter(|f| !f.trim().is_empty()) {
                let font = font.trim();
                if !FONTS.contains(&font) {
                    return Err(Error::Tool(format!(
                        "iapp: unknown font {font:?} — pick one of: {}",
                        FONTS.join(", ")
                    )));
                }
                body["font"] = json!(font);
            }
        }

        // Edit mode: iApp takes a single reference image, so a caller
        // that passed several gets the first (the tools only ever send
        // one). Base64 of the raw bytes, capped at the vendor's 6 MB.
        if let Some(src) = req.input_images.first() {
            if src.bytes.len() > MAX_REFERENCE_BYTES {
                return Err(Error::Tool(format!(
                    "iapp: reference image is {:.1} MB — the API caps it at 6 MB",
                    src.bytes.len() as f64 / (1024.0 * 1024.0)
                )));
            }
            body["reference_image"] = json!(B64.encode(&src.bytes));
        }

        // A text render re-checks itself and retries upstream (~60s vs
        // ~10s plain), so the ceiling is well above the plain-path need.
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| Error::Tool(format!("http client: {e}")))?;
        let url = format!("{IAPP_BASE}{GEN_PATH}");
        let resp = crate::multi_tenant::attach_member(client.post(&url))
            .header("apikey", &api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("iapp http: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let b = resp.text().await.unwrap_or_default();
            // 503 is "model still starting" upstream — the one status
            // worth telling the caller to simply try again on.
            let hint = if status.as_u16() == 503 {
                " (model starting — retry in ~60s)"
            } else {
                ""
            };
            return Err(Error::Tool(format!(
                "iapp http {status}{hint}: {}",
                b.chars().take(400).collect::<String>()
            )));
        }

        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("iapp response not json: {e}")))?;

        let b64 = v
            .get("image_base64")
            .and_then(|b| b.as_str())
            .ok_or_else(|| {
                // `image_base64` is megabytes when present, so the raw
                // echo here is safe: this arm only runs without it.
                Error::Tool(format!(
                    "iapp returned no image — raw: {}",
                    v.to_string().chars().take(500).collect::<String>()
                ))
            })?;
        let bytes = B64
            .decode(b64)
            .map_err(|e| Error::Tool(format!("iapp image_base64 not valid base64: {e}")))?;
        Ok(ImageResult { bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(aspect: &str) -> ImageRequest {
        ImageRequest {
            model: "iapp-image-generation".into(),
            prompt: "ร้านกาแฟริมคลอง".into(),
            input_images: Vec::new(),
            aspect_ratio: aspect.into(),
            size: "1K".into(),
            text: Vec::new(),
            font: None,
        }
    }

    /// Every mapped size must be one the API actually accepts — a typo
    /// here is a 400 the user only sees at call time.
    #[test]
    fn aspects_map_onto_the_five_supported_sizes() {
        const SUPPORTED: &[&str] = &[
            "1024x1024",
            "1280x768",
            "768x1280",
            "1536x1024",
            "1024x1536",
        ];
        for aspect in ["1:1", "9:16", "3:4", "4:3", "16:9", ""] {
            let got = IappImageProvider::size(&req(aspect));
            assert!(
                SUPPORTED.contains(&got),
                "{aspect} → {got} is not a supported size"
            );
        }
        assert_eq!(IappImageProvider::size(&req("1:1")), "1024x1024");
        assert_eq!(IappImageProvider::size(&req("9:16")), "768x1280");
        // Unknown / empty aspect falls back to the 16:9 default.
        assert_eq!(IappImageProvider::size(&req("")), "1280x768");
        assert_eq!(IappImageProvider::size(&req("21:9")), "1280x768");
    }

    #[test]
    fn text_overlay_limits_are_reported_before_the_call() {
        assert!(IappImageProvider::check_text(&[]).is_ok());
        assert!(IappImageProvider::check_text(&vec![
            "ยินดีต้อนรับ".to_string();
            4
        ])
        .is_ok());

        let five = vec!["บรรทัด".to_string(); 5];
        let err = IappImageProvider::check_text(&five)
            .unwrap_err()
            .to_string();
        assert!(err.contains("at most 4"), "got: {err}");

        // Thai counts by character, not by UTF-8 byte — 121 Thai chars is
        // ~363 bytes and must trip the limit on chars.
        let long = vec!["ก".repeat(121)];
        let err = IappImageProvider::check_text(&long)
            .unwrap_err()
            .to_string();
        assert!(err.contains("121 chars"), "got: {err}");
    }

    /// The vendor documents 19 faces; the schema offers this list
    /// verbatim, so a drift here silently narrows what the model can ask
    /// for (or offers a face the API will 400 on).
    #[test]
    fn font_list_is_the_documented_nineteen() {
        assert_eq!(FONTS.len(), 19);
        assert_eq!(FONTS[0], "kanit-bold", "the API default must lead the list");
        let mut sorted = FONTS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), FONTS.len(), "duplicate font id");
    }

    #[test]
    fn aliases_resolve() {
        let p = IappImageProvider;
        for raw in ["iapp", "iapp-image", "iapp-image-generation"] {
            assert_eq!(
                p.resolve_model(raw).as_deref(),
                Some("iapp-image-generation")
            );
        }
        assert_eq!(p.resolve_model("flash"), None);
    }
}
