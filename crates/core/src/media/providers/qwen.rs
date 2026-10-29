//! Qwen-Image provider (dev-plan/40) — Alibaba DashScope.
//!
//! qwen-image-2.0 / -pro generate via the DashScope multimodal-generation
//! endpoint, which handles BOTH text→image (a single `{text}` content
//! part) and image→image editing (one or more `{image}` parts + a
//! `{text}` instruction — multi-image editing is supported natively).
//!
//! International endpoint `dashscope-intl.aliyuncs.com`, or
//! `<gateway>/dashscope` when only the thClaws Gateway key is present.
//! Auth is `Authorization: Bearer <DASHSCOPE_API_KEY>`. The response
//! carries a signed image URL (PNG, expires 24h) at
//! `output.choices[0].message.content[].image`, which we download.
//!
//! Pricing is PER IMAGE (2.0 $0.035 / -pro $0.075; 3.0 $0.03 / -pro $0.075
//! at the 2K tiles we request), unlike the
//! token-metered chat models — recorded as `price_per_image_usd` in the
//! catalogue. Gateway per-image metering is a follow-up; desktop users
//! with their own DASHSCOPE_API_KEY work today.

use crate::error::{Error, Result};
use crate::media::provider::{ImageModelInfo, ImageProvider, ImageRequest, ImageResult};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

const DASHSCOPE_BASE: &str = "https://dashscope-intl.aliyuncs.com";
const GEN_PATH: &str = "/api/v1/services/aigc/multimodal-generation/generation";

const MODELS: &[ImageModelInfo] = &[
    ImageModelInfo {
        id: "qwen-image-2.0",
        aliases: &["qwen", "qwen-image", "qwen-image-2.0"],
        label: "Qwen Image 2.0",
    },
    ImageModelInfo {
        id: "qwen-image-2.0-pro",
        aliases: &["qwen-pro", "qwen-image-2.0-pro"],
        label: "Qwen Image 2.0 Pro",
    },
    ImageModelInfo {
        id: "qwen-image-3.0",
        // qwen-image-plus is gone from Model Studio (404 on the intl
        // region and on SIS's workspace, 2026-10-22); its name lands here.
        aliases: &["qwen-3", "qwen-image-3.0", "qwen-plus", "qwen-image-plus"],
        label: "Qwen Image 3.0",
    },
    ImageModelInfo {
        id: "qwen-image-3.0-pro",
        // …and qwen-image-max's here.
        aliases: &[
            "qwen-3-pro",
            "qwen-image-3.0-pro",
            "qwen-max",
            "qwen-image-max",
        ],
        label: "Qwen Image 3.0 Pro",
    },
    ImageModelInfo {
        id: "wan2.7-image",
        // The editor now: the qwen-image-edit models are gone from Model
        // Studio (404, 2026-10-22), and saved prompts naming them land here.
        aliases: &[
            "wan-image",
            "wan2.7-image",
            "qwen-edit",
            "qwen-image-edit",
            "qwen-image-edit-plus",
            "qwen-edit-max",
            "qwen-image-edit-max",
        ],
        label: "Wan 2.7 Image",
    },
    ImageModelInfo {
        id: "wan2.7-image-pro",
        aliases: &["wan-image-pro", "wan2.7-image-pro"],
        label: "Wan 2.7 Image Pro",
    },
    ImageModelInfo {
        id: "z-image-turbo",
        aliases: &["z-image", "z-image-turbo"],
        label: "Z-Image Turbo (text→image)",
    },
];

/// Request shapes on the multimodal-generation endpoint differ by family
/// (Model Studio API reference, checked live 2026-09-29).
#[derive(Debug, PartialEq)]
enum Family {
    /// qwen-image-2.0/3.0 (+pro): 2K tiles, `n`, `negative_prompt`.
    Qwen,
    /// qwen-image-max/-plus: text→image only, their own five sizes.
    QwenFixed,
    /// qwen-image-edit*: image→image, output follows the input — no `size`.
    QwenEdit,
    /// wan2.7-image(-pro): text→image or editing, no `negative_prompt`.
    Wan,
    /// z-image-turbo: text→image only, `size` + `prompt_extend`.
    ZImage,
}

fn family(model: &str) -> Family {
    if model.starts_with("qwen-image-edit") {
        Family::QwenEdit
    } else if model.starts_with("qwen-image-max") || model.starts_with("qwen-image-plus") {
        Family::QwenFixed
    } else if model.starts_with("wan") {
        Family::Wan
    } else if model.starts_with("z-image") {
        Family::ZImage
    } else {
        Family::Qwen
    }
}

pub struct QwenImageProvider;

/// DashScope's accepted range is 0..=2^31-1.
fn random_seed() -> u32 {
    let mut b = [0u8; 4];
    let _ = getrandom::getrandom(&mut b);
    u32::from_le_bytes(b) & 0x7fff_ffff
}

impl QwenImageProvider {
    /// Map the engine's portable aspect tiers onto DashScope `W*H` size
    /// strings. One table for every model in the family.
    ///
    /// The 3.x model card says dimensions run 512-2048 per side, so these
    /// 2688-wide tiles look out of range — they are not. Checked against the
    /// live API (2026-08-25): `qwen-image-3.0` and `-3.0-pro` both return
    /// exactly 2688x1536 and 1536x2688 when asked. Sizing 3.x down to 2048
    /// would only hand the user a smaller picture for the same money, since
    /// billing is by tier, not by pixel: everything over 2,250,000 px is
    /// "2K", and every tile here clears that line. So the aspect choice
    /// never silently changes what an image costs, on either series.
    fn size(req: &ImageRequest) -> &'static str {
        match req.aspect_ratio.as_str() {
            "1:1" => "2048*2048",
            "9:16" => "1536*2688",
            "3:4" => "1728*2368",
            "4:3" => "2368*1728",
            _ => "2688*1536", // 16:9 default
        }
    }

    /// qwen-image-max/-plus accept exactly these five.
    fn fixed_size(req: &ImageRequest) -> &'static str {
        match req.aspect_ratio.as_str() {
            "1:1" => "1328*1328",
            "9:16" => "928*1664",
            "3:4" => "1104*1472",
            "4:3" => "1472*1104",
            _ => "1664*928",
        }
    }

    /// z-image-turbo tops out at 2048*2048 total pixels.
    fn z_size(req: &ImageRequest) -> &'static str {
        match req.aspect_ratio.as_str() {
            "1:1" => "1536*1536",
            "9:16" => "1152*2048",
            "3:4" => "1296*1728",
            "4:3" => "1728*1296",
            _ => "2048*1152",
        }
    }

    fn parameters(req: &ImageRequest) -> Result<Value> {
        let editing = !req.input_images.is_empty();
        let fam = family(&req.model);
        match fam {
            Family::QwenFixed | Family::ZImage if editing => {
                return Err(Error::Tool(format!(
                    "{} is text→image only — use wan2.7-image to edit an image",
                    req.model
                )))
            }
            Family::QwenEdit if !editing => {
                return Err(Error::Tool(format!(
                "{} edits an image — give it an input image, or use qwen-image-3.0 for text→image",
                req.model
            )))
            }
            _ => {}
        }
        Ok(match fam {
            Family::Qwen => json!({
                "size": Self::size(req), "n": 1, "negative_prompt": "", "watermark": false
            }),
            Family::QwenFixed => json!({
                "size": Self::fixed_size(req), "n": 1, "negative_prompt": "", "watermark": false
            }),
            Family::QwenEdit => json!({ "n": 1, "negative_prompt": "", "watermark": false }),
            Family::Wan if editing => json!({ "n": 1, "watermark": false }),
            Family::Wan => json!({ "size": Self::size(req), "n": 1, "watermark": false }),
            Family::ZImage => json!({ "size": Self::z_size(req), "prompt_extend": false }),
        })
    }
}

#[async_trait]
impl ImageProvider for QwenImageProvider {
    fn id(&self) -> &'static str {
        "qwen"
    }
    fn models(&self) -> &'static [ImageModelInfo] {
        MODELS
    }
    fn resolve_model(&self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        for m in MODELS {
            if raw == m.id || m.aliases.contains(&raw) {
                return Some(m.id.to_string());
            }
        }
        // Forward-compat: accept any future id of these families verbatim.
        if raw.starts_with("qwen-image")
            || (raw.starts_with("wan") && raw.contains("-image"))
            || raw.starts_with("z-image")
        {
            return Some(raw.to_string());
        }
        None
    }

    async fn generate(&self, req: &ImageRequest) -> Result<ImageResult> {
        let mut parameters = Self::parameters(req)?;
        // DashScope returns the identical image for an identical request
        // when no seed is given, so a retry would cost a call and change
        // nothing.
        parameters["seed"] = json!(random_seed());
        let ep = crate::media::provider::resolve_dashscope_endpoint(DASHSCOPE_BASE)?;

        // content: image parts first (data URIs for local bytes), then
        // the text instruction — text2image is just the text part.
        let mut content: Vec<Value> = Vec::new();
        for img in &req.input_images {
            content.push(json!({
                "image": format!("data:{};base64,{}", img.mime, B64.encode(&img.bytes))
            }));
        }
        content.push(json!({ "text": req.prompt }));

        let body = json!({
            "model": req.model,
            "input": { "messages": [ { "role": "user", "content": content } ] },
            "parameters": parameters,
        });
        let url = format!("{}{}", ep.base_url.trim_end_matches('/'), GEN_PATH);
        let client = Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| Error::Tool(format!("http client: {e}")))?;
        let resp = crate::multi_tenant::attach_member(client.post(&url))
            .bearer_auth(&ep.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("qwen http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let b = resp.text().await.unwrap_or_default();
            return Err(Error::Tool(format!(
                "qwen http {status}: {}",
                b.chars().take(400).collect::<String>()
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("qwen response not json: {e}")))?;

        // Walk output.choices[].message.content[] for the first `image`
        // URL (signed OSS URL, PNG).
        let img_url = v
            .pointer("/output/choices")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .filter_map(|choice| {
                choice
                    .pointer("/message/content")
                    .and_then(|c| c.as_array())
            })
            .flatten()
            .find_map(|part| part.get("image").and_then(|i| i.as_str()))
            .ok_or_else(|| {
                let raw = v.to_string();
                Error::Tool(format!(
                    "qwen returned no image — raw: {}",
                    raw.chars().take(500).collect::<String>()
                ))
            })?
            .to_string();

        let img = client
            .get(&img_url)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("qwen image download: {e}")))?;
        if !img.status().is_success() {
            return Err(Error::Tool(format!(
                "qwen image download http {}",
                img.status()
            )));
        }
        let bytes = img
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| Error::Tool(format!("qwen image body: {e}")))?;
        Ok(ImageResult { bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(model: &str, aspect: &str, images: usize) -> ImageRequest {
        ImageRequest {
            model: model.into(),
            prompt: "p".into(),
            input_images: (0..images)
                .map(|_| crate::media::provider::InputImage {
                    bytes: vec![0],
                    mime: "image/png".into(),
                })
                .collect(),
            aspect_ratio: aspect.into(),
            size: String::new(),
            text: Vec::new(),
            font: None,
        }
    }

    #[test]
    fn seeds_differ_and_stay_in_range() {
        let a: Vec<u32> = (0..8).map(|_| random_seed()).collect();
        assert!(a.iter().all(|s| *s <= 0x7fff_ffff));
        assert!(a.windows(2).any(|w| w[0] != w[1]), "{a:?}");
    }

    #[test]
    fn each_family_sends_its_own_parameters() {
        let p = |m: &str, a: &str, n: usize| QwenImageProvider::parameters(&req(m, a, n)).unwrap();
        assert_eq!(p("qwen-image-3.0", "16:9", 0)["size"], "2688*1536");
        assert_eq!(p("qwen-image-max", "9:16", 0)["size"], "928*1664");
        assert_eq!(p("qwen-image-plus", "", 0)["size"], "1664*928");
        assert!(p("qwen-image-edit-plus", "16:9", 1).get("size").is_none());
        assert_eq!(p("wan2.7-image", "1:1", 0)["size"], "2048*2048");
        assert!(p("wan2.7-image", "1:1", 1).get("size").is_none());
        assert!(p("wan2.7-image", "1:1", 0).get("negative_prompt").is_none());
        let z = p("z-image-turbo", "16:9", 0);
        assert_eq!(z["size"], "2048*1152");
        assert_eq!(z["prompt_extend"], false);
        assert!(z.get("n").is_none());
    }

    #[test]
    fn text_only_and_edit_only_models_refuse_the_wrong_input() {
        let err = |m: &str, n: usize| QwenImageProvider::parameters(&req(m, "", n)).is_err();
        assert!(err("qwen-image-max", 1));
        assert!(err("z-image-turbo", 1));
        assert!(err("qwen-image-edit-plus", 0));
        assert!(!err("wan2.7-image", 1));
    }

    #[test]
    fn new_models_resolve() {
        let p = QwenImageProvider;
        assert_eq!(p.resolve_model("z-image").as_deref(), Some("z-image-turbo"));
        assert_eq!(
            p.resolve_model("wan-image").as_deref(),
            Some("wan2.7-image")
        );
        // Retired on Model Studio (404, 2026-10-22): their names land on
        // living models instead of erroring.
        for (gone, now) in [
            ("qwen-edit", "wan2.7-image"),
            ("qwen-image-edit-plus", "wan2.7-image"),
            ("qwen-image-edit-max", "wan2.7-image"),
            ("qwen-image-max", "qwen-image-3.0-pro"),
            ("qwen-image-plus", "qwen-image-3.0"),
        ] {
            assert_eq!(p.resolve_model(gone).as_deref(), Some(now), "{gone}");
        }
        assert_eq!(
            p.resolve_model("wan2.8-image").as_deref(),
            Some("wan2.8-image")
        );
        assert!(p.resolve_model("wan2.7-t2v").is_none());
    }
}
