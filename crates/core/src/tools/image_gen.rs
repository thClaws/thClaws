//! `TextToImage` + `ImageToImage` — provider-abstracted image generation
//! (dev-plan/40, Tier 1).
//!
//! These were Gemini-only (`tools/gemini_image.rs`); they now resolve a
//! `model` (+ optional `provider`) to a backend via [`crate::media`] and
//! call through the `ImageProvider` trait. Backends: `gemini`
//! (gemini-3.1-flash-image / -pro-image), `openai` (gpt-image-2), `qwen`
//! (qwen-image-2.0/3.0) and `iapp` (iapp-image-generation).
//!
//! `text` + `font` are iApp-only passthroughs — literal copy the
//! provider TYPESETS rather than the model drawing it. They ride on
//! `ImageRequest` because that is the seam both tools already build; the
//! other providers ignore them.
//!
//! Both tools stay **opt-in**: registered only when
//! `imageToolsEnabled: true` in `.thclaws/settings.json`. They no longer
//! hard-require `GEMINI_API_KEY` via `requires_env` (that would hide them
//! from OpenAI-only / gateway users) — each provider validates its own
//! credentials at call time and returns a clear error.
//!
//! Output: bytes are written to `output/img-<ts>-<sha8>.<ext>` and the
//! tool returns a **text-only** result (the path + a checksum). A
//! generated image is an artifact for the user, not input the model
//! reasons over, so the pixels are never shipped back to the LLM — see
//! [`build_image_result`] for why (token cost + the text-only-model 400).

use crate::error::{Error, Result};
use crate::media::provider::{ImageRequest, InputImage};
use crate::media::{registry, save_image, sniff_ext};
use crate::tools::{req_str, Tool};
use crate::types::ToolResultContent;
use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// String-array param as `Vec<String>`, tolerating the single-string
/// form — models routinely send `"text": "one line"` where the schema
/// asks for an array, and rejecting that would be a pointless retry.
fn opt_lines(input: &Value, key: &str) -> Vec<String> {
    match input.get(key) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .collect(),
        Some(Value::String(s)) if !s.trim().is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn opt(input: &Value, key: &str) -> String {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Build the tool result for a generated image. **Text-only by design.**
///
/// A generated image is an output artifact *for the user* — it's written
/// to disk and the GUI shows it from that path. The model does not need
/// the pixels back to do its job (it just reports "created your image at
/// <path>"), so we never ship the base64 blob into history — not even for
/// vision models. That:
///   - saves the multi-MB token cost of round-tripping the image, and
///   - avoids the hard 400 ("unknown variant `image_url`") that a
///     generated image stuck in history raises on EVERY later turn once
///     the active chat model is text-only (e.g. deepseek via the
///     gateway) — which otherwise dead-ends the whole session.
///
/// A workflow that genuinely needs the model to inspect the result can
/// `Read` the returned path with a vision-capable model.
fn build_image_result(bytes: &[u8], path: &std::path::Path) -> ToolResultContent {
    let d = Sha256::digest(bytes);
    ToolResultContent::Text(format!(
        "Wrote {} ({} bytes, sha256-4={:02x}{:02x}{:02x}{:02x})",
        path.display(),
        bytes.len(),
        d[0],
        d[1],
        d[2],
        d[3],
    ))
}

const MODEL_DESC: &str = "Which image model. Provider is inferred from the model. \
Gemini: `flash` (default; gemini-3.1-flash-image) or `pro` (gemini-3-pro-image). \
OpenAI: `gpt-image-2` (alias `openai`). Qwen: `qwen-image-3.0` (alias `qwen-3`) or \
`qwen-image-3.0-pro` — newest, best at dense layouts and small text; `qwen-image-2.0` \
(alias `qwen`) / `-pro` remain. Strong at multi-image editing + text rendering. \
iApp: `iapp` (iapp-image-generation) — Thai-first; the only backend that typesets \
Thai copy correctly, via the `text` + `font` params. Default: flash.";
const PROVIDER_DESC: &str = "Optional explicit provider (`gemini` | `openai` | `qwen` \
| `iapp`). Usually omit — it's inferred from `model`.";

/// Schema fragment for the iApp-only text overlay. Shared by both tools
/// so the font list can't drift between them; the enum comes straight
/// from the provider's `FONTS` so it can't drift from what the API takes.
fn text_overlay_schema() -> (Value, Value) {
    let text = json!({
        "type": "array",
        "items": { "type": "string" },
        "description": "iApp only — literal lines of copy to TYPESET into the image \
    (max 4 lines, 120 chars each). Use this for Thai text instead of describing it in the \
    prompt: iApp sets it in a real font, while every diffusion model (including the other \
    providers here) mangles Thai script. Ignored by gemini/openai/qwen."
    });
    let font = json!({
        "type": "string",
        "description": "iApp only — typeface for `text`. Default kanit-bold. Modern: \
    kanit(-bold), prompt(-bold), athiti-bold, k2d-bold. Formal/document: sarabun(-bold), \
    notosans. Serif: pridi, taviraj, notoserif. Friendly/rounded: mitr, itim. Technical: \
    chakrapetch-bold. Handwriting/script: mali, sriracha, charm. Decorative: srisakdi.",
        "enum": crate::media::providers::iapp::FONTS,
    });
    (text, font)
}

// ─── TextToImage ─────────────────────────────────────────────────

pub struct TextToImageTool;

#[async_trait]
impl Tool for TextToImageTool {
    fn name(&self) -> &'static str {
        "TextToImage"
    }
    fn description(&self) -> &'static str {
        "Generate a brand-new image from a text prompt. Multi-provider: \
         Gemini (`flash` default, `pro`), OpenAI (`gpt-image-2`), Qwen, or iApp \
         (`iapp` — Thai-first, typesets Thai copy via `text`/`font`). Output is \
         written to `output/img-<ts>-<sha8>.<ext>` and returned inline as an \
         image block. Requires `imageToolsEnabled: true` in \
         `.thclaws/settings.json`, plus the chosen provider's API key in env \
         (`GEMINI_API_KEY`/`GOOGLE_API_KEY` for Gemini, `OPENAI_API_KEY` for \
         OpenAI) — or the thClaws Gateway key. Aspect ratios: 1:1, 3:4, 4:3, \
         9:16, 16:9 (default). Sizes: 512, 1K (default), 2K — mapped to each \
         provider's nearest size/quality."
    }
    fn input_schema(&self) -> Value {
        let (text, font) = text_overlay_schema();
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Description of the image. Be specific about subject, style, composition, colors. A 2–3 sentence brief beats a one-word tag."
                },
                "model": { "type": "string", "description": MODEL_DESC },
                "provider": { "type": "string", "description": PROVIDER_DESC, "enum": ["gemini", "openai", "qwen", "iapp"] },
                "aspect_ratio": {
                    "type": "string",
                    "description": "Output aspect ratio. Default 16:9.",
                    "enum": ["1:1", "3:4", "4:3", "9:16", "16:9"]
                },
                "size": {
                    "type": "string",
                    "description": "Output size tier. Default 1K. Ignored by iApp, whose sizes follow the aspect ratio.",
                    "enum": ["512", "1K", "2K"]
                },
                "text": text,
                "font": font
            },
            "required": ["prompt"]
        })
    }
    fn requires_approval(&self, _input: &Value) -> bool {
        // Costs money + writes a file.
        true
    }
    async fn call(&self, input: Value) -> Result<String> {
        let result = self.call_multimodal(input).await?;
        Ok(result.to_text())
    }
    async fn call_multimodal(&self, input: Value) -> Result<ToolResultContent> {
        let prompt = req_str(&input, "prompt")?.to_string();
        let (provider, model) = registry::resolve(&opt(&input, "provider"), &opt(&input, "model"))?;
        let aspect = opt(&input, "aspect_ratio");
        let size = opt(&input, "size");
        let req = ImageRequest {
            model,
            prompt,
            input_images: Vec::new(),
            aspect_ratio: if aspect.is_empty() {
                "16:9".into()
            } else {
                aspect
            },
            size: if size.is_empty() { "1K".into() } else { size },
            text: opt_lines(&input, "text"),
            font: input
                .get("font")
                .and_then(|v| v.as_str())
                .filter(|f| !f.trim().is_empty())
                .map(|f| f.to_string()),
        };
        let out = provider.generate(&req).await?;
        let path = save_image(&out.bytes, sniff_ext(&out.bytes))?;
        Ok(build_image_result(&out.bytes, &path))
    }
}

// ─── ImageToImage ────────────────────────────────────────────────

pub struct ImageToImageTool;

#[async_trait]
impl Tool for ImageToImageTool {
    fn name(&self) -> &'static str {
        "ImageToImage"
    }
    fn description(&self) -> &'static str {
        "Edit or transform an existing image using a text prompt (edit mode). \
         Multi-provider: Gemini (`flash`/`pro`), OpenAI (`gpt-image-2`), iApp \
         (`iapp`, Thai text), or Qwen \
         (`qwen-image-3.0`/`-pro`, newest; `qwen-image-2.0`/`-pro` also available — \
         strong at edits + text). Pass \
         `input_path` (a path under the workspace) + a `prompt` describing the \
         change; the result is written to `output/img-<ts>-<sha8>.<ext>`. Use for \
         background removal, style transfer, adding/removing elements, lighting \
         changes, text overlays. Same gating + keys as TextToImage. Tip: edits \
         with a strict 'keep everything else identical' clause preserve \
         composition best."
    }
    fn input_schema(&self) -> Value {
        let (text, font) = text_overlay_schema();
        json!({
            "type": "object",
            "properties": {
                "input_path": {
                    "type": "string",
                    "description": "Path to the source image inside the workspace. Supported: PNG, JPEG, WebP."
                },
                "prompt": {
                    "type": "string",
                    "description": "What to change. Be explicit about what should STAY the same — vague prompts produce drift."
                },
                "model": { "type": "string", "description": MODEL_DESC },
                "provider": { "type": "string", "description": PROVIDER_DESC, "enum": ["gemini", "openai", "qwen", "iapp"] },
                "aspect_ratio": { "type": "string", "enum": ["1:1", "3:4", "4:3", "9:16", "16:9"] },
                "size": { "type": "string", "enum": ["512", "1K", "2K"] },
                "text": text,
                "font": font
            },
            "required": ["input_path", "prompt"]
        })
    }
    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }
    async fn call(&self, input: Value) -> Result<String> {
        let result = self.call_multimodal(input).await?;
        Ok(result.to_text())
    }
    async fn call_multimodal(&self, input: Value) -> Result<ToolResultContent> {
        let input_path_raw = req_str(&input, "input_path")?;
        let prompt = req_str(&input, "prompt")?.to_string();
        let (provider, model) = registry::resolve(&opt(&input, "provider"), &opt(&input, "model"))?;
        let aspect = opt(&input, "aspect_ratio");
        let size = opt(&input, "size");

        // Sandbox-check the input path so the agent can't smuggle an
        // arbitrary system file into a provider's context.
        let abs = crate::sandbox::Sandbox::check(input_path_raw)
            .map_err(|e| Error::Tool(format!("input_path: {e}")))?;
        let in_bytes =
            std::fs::read(&abs).map_err(|e| Error::Tool(format!("read {}: {e}", abs.display())))?;
        let in_mime = match abs
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            _ => "image/png",
        }
        .to_string();

        let req = ImageRequest {
            model,
            prompt,
            input_images: vec![InputImage {
                bytes: in_bytes,
                mime: in_mime,
            }],
            aspect_ratio: if aspect.is_empty() {
                "16:9".into()
            } else {
                aspect
            },
            size: if size.is_empty() { "1K".into() } else { size },
            text: opt_lines(&input, "text"),
            font: input
                .get("font")
                .and_then(|v| v.as_str())
                .filter(|f| !f.trim().is_empty())
                .map(|f| f.to_string()),
        };
        let out = provider.generate(&req).await?;
        let out_path = save_image(&out.bytes, sniff_ext(&out.bytes))?;
        Ok(build_image_result(&out.bytes, &out_path))
    }
}

#[cfg(test)]
mod tests {
    use crate::media::registry;

    #[test]
    fn default_model_resolves_to_gemini_flash() {
        let (p, m) = registry::resolve("", "").expect("default resolves");
        assert_eq!(p.id(), "gemini");
        assert_eq!(m, "gemini-3.1-flash-image");
    }

    #[test]
    fn aliases_route_to_the_right_provider() {
        assert_eq!(
            registry::resolve("", "pro").unwrap().1,
            "gemini-3-pro-image"
        );
        // The 3.1 id 404s upstream (d5a17692) but stays an alias so
        // existing callers keep working — it must resolve to the real one.
        assert_eq!(
            registry::resolve("", "gemini-3.1-pro-image").unwrap().1,
            "gemini-3-pro-image"
        );
        let (p, m) = registry::resolve("", "gpt-image-2").unwrap();
        assert_eq!(p.id(), "openai");
        assert_eq!(m, "gpt-image-2");
        // bare alias `openai`
        assert_eq!(registry::resolve("", "openai").unwrap().0.id(), "openai");
    }

    #[test]
    fn explicit_provider_overrides_inference() {
        let (p, m) = registry::resolve("openai", "").unwrap();
        assert_eq!(p.id(), "openai");
        assert_eq!(m, "gpt-image-2");
    }

    #[test]
    fn qwen_3_models_resolve() {
        for (input, want) in [
            ("qwen-image-3.0", "qwen-image-3.0"),
            ("qwen-3", "qwen-image-3.0"),
            ("qwen-image-3.0-pro", "qwen-image-3.0-pro"),
            ("qwen-3-pro", "qwen-image-3.0-pro"),
        ] {
            let (p, m) = registry::resolve("", input).unwrap();
            assert_eq!(p.id(), "qwen", "{input} routes to the qwen provider");
            assert_eq!(m, want, "{input}");
        }
        // Adding 3.x must not move the bare `qwen` alias or the provider
        // default off 2.0 — that is a separate decision.
        assert_eq!(registry::resolve("", "qwen").unwrap().1, "qwen-image-2.0");
        assert_eq!(registry::resolve("qwen", "").unwrap().1, "qwen-image-2.0");
    }

    #[test]
    fn qwen_models_resolve() {
        let (p, m) = registry::resolve("", "qwen-image-2.0").unwrap();
        assert_eq!(p.id(), "qwen");
        assert_eq!(m, "qwen-image-2.0");
        assert_eq!(
            registry::resolve("", "qwen-image-2.0-pro").unwrap().1,
            "qwen-image-2.0-pro"
        );
        // bare alias + explicit provider default
        assert_eq!(registry::resolve("", "qwen").unwrap().0.id(), "qwen");
        assert_eq!(registry::resolve("qwen", "").unwrap().1, "qwen-image-2.0");
    }

    #[test]
    fn unknown_model_errors() {
        assert!(registry::resolve("", "midjourney-v9").is_err());
        assert!(registry::resolve("nope", "flash").is_err());
    }
}
