use super::{req_str, Tool};
use crate::error::{Error, Result};
use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};

/// Hard cap on raw image bytes the Read tool will base64-encode and ship
/// to the provider. Anthropic's documented per-image limit is 5 MB on the
/// gateway/Bedrock/Vertex path; going above that wastes tokens and risks a
/// 413. Oversized images are auto-downscaled (see `downscale_for_vision`)
/// rather than rejected; this cap is the final ceiling after that pass.
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Long-edge target for auto-downscaling. 1568px is the standard-tier vision
/// limit — Anthropic (and OpenAI/Gemini) downscale anything larger server-side
/// anyway, so resizing to this here costs no fidelity the model would have
/// seen, caps the per-image visual-token cost (~1568 tokens), and reliably
/// fits the byte cap. High-res tiers (Opus 4.7/4.8) accept up to 2576px, but
/// that's ~3x the tokens for detail most reads don't need.
const TARGET_LONG_EDGE: u32 = 1568;

/// M6.23 BUG RT1: hard cap on text-file size for the read-whole-file
/// path. Pre-fix `std::fs::read_to_string` had no cap, so reading a
/// multi-GB log file could OOM the worker. 100 MB is generous enough
/// for any real source file or document; logs / data dumps that
/// exceed it should be sliced via `offset` + `limit`. Note: even
/// offset+limit currently reads the whole file — a future enhancement
/// would stream-read for the slice case, but the size cap still
/// applies for now.
const MAX_TEXT_BYTES: u64 = 100 * 1024 * 1024;

pub struct ReadTool;

/// Detect a supported image MIME type from the file extension. Returns
/// `None` for non-image extensions, in which case the Read tool falls
/// through to the text branch. The extension only gates whether we
/// take the multimodal branch — the *actual* MIME we send to the
/// provider is determined by `sniff_image_mime` against the bytes,
/// because Anthropic / OpenAI / Gemini all 400 when the declared
/// media_type doesn't match what they decode (file named `.png` but
/// containing JPEG bytes is a real-world failure mode).
fn image_media_type(path: &std::path::Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

/// Infer image MIME type from the first ~12 bytes (magic numbers).
/// Returns `None` if the bytes don't match any supported format —
/// caller falls back to the extension-derived MIME, which is
/// usually-but-not-always correct.
fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    // JPEG: FF D8 FF (any third byte — JFIF/Exif/etc.)
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    // GIF: "GIF87a" or "GIF89a"
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    // WebP: "RIFF" at byte 0, "WEBP" at byte 8 (4-byte length in between)
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

fn encode_image(img: &image::DynamicImage, fmt: image::ImageOutputFormat) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), fmt)
        .map_err(|e| Error::Tool(format!("encode image: {e}")))?;
    Ok(out)
}

/// Downscale an oversized image so it fits the byte cap and the standard
/// vision long-edge limit. Returns `Ok(None)` when the image is already small
/// enough to ship untouched (the common case — no decode, no quality loss).
/// Otherwise decodes, resizes to `TARGET_LONG_EDGE` (preserving aspect), and
/// re-encodes: PNG for lossless sources (screenshots/diagrams stay crisp),
/// falling back to progressively-lower-quality JPEG if PNG can't get under
/// the cap. `mime` is the sniffed input type; the returned mime reflects the
/// re-encoded format (WebP/GIF can't be re-encoded in-place, so they become
/// PNG or JPEG — the caller re-reports the mime, so this stays consistent).
pub(crate) fn downscale_for_vision(
    bytes: &[u8],
    mime: &str,
) -> Result<Option<(Vec<u8>, &'static str)>> {
    // Header-only dimension probe — avoids a full decode when we don't need one.
    let dims = image::io::Reader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok());
    let oversized_dims = dims
        .map(|(w, h)| w.max(h) > TARGET_LONG_EDGE)
        .unwrap_or(false);
    if bytes.len() <= MAX_IMAGE_BYTES && !oversized_dims {
        return Ok(None);
    }

    let img = image::load_from_memory(bytes)
        .map_err(|e| Error::Tool(format!("decode image for downscale: {e}")))?;
    let img = if img.width().max(img.height()) > TARGET_LONG_EDGE {
        // `resize` fits within the box while preserving aspect ratio.
        img.resize(
            TARGET_LONG_EDGE,
            TARGET_LONG_EDGE,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    };

    // Lossless sources: try PNG first; keep it if it's under the cap.
    if mime != "image/jpeg" {
        let png = encode_image(&img, image::ImageOutputFormat::Png)?;
        if png.len() <= MAX_IMAGE_BYTES {
            return Ok(Some((png, "image/png")));
        }
    }
    // JPEG (primary for photos, fallback for big PNGs). JPEG has no alpha.
    let rgb = image::DynamicImage::ImageRgb8(img.to_rgb8());
    for q in [85u8, 70, 55] {
        let jpg = encode_image(&rgb, image::ImageOutputFormat::Jpeg(q))?;
        if jpg.len() <= MAX_IMAGE_BYTES {
            return Ok(Some((jpg, "image/jpeg")));
        }
    }
    Err(Error::Tool(format!(
        "image is still over the {}-byte cap after downscaling to {}px @ JPEG q55 — \
         the source is unusually dense; crop or pre-resize it",
        MAX_IMAGE_BYTES, TARGET_LONG_EDGE
    )))
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "Read"
    }

    fn audit_summary(&self, input: &Value) -> Option<super::AuditSummary> {
        input
            .get("path")
            .and_then(Value::as_str)
            .map(|p| super::AuditSummary::targets([p.to_string()]))
    }

    fn parallelizable(&self) -> bool {
        true
    }

    fn description(&self) -> &'static str {
        "Read the contents of a file. Optional `offset` (1-indexed line) and `limit` \
         (max lines) select a slice; omit for the whole file. \
         Image files (.png/.jpg/.jpeg/.webp/.gif) are returned as inline \
         multimodal content so vision-capable models (Claude, etc.) can \
         see the pixels — the model receives the image alongside a text \
         summary describing size and MIME type."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute path to the file"},
                "offset": {"type": "integer", "description": "Start line (1-indexed) — text files only"},
                "limit":  {"type": "integer", "description": "Max number of lines — text files only"}
            },
            "required": ["path"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_path = req_str(&input, "path")?;
        let path = crate::sandbox::Sandbox::check(raw_path)?;

        // Reject image extensions on the text-only entry point — they
        // would otherwise hit `read_to_string` below and surface as a
        // confusing utf-8 error. Callers that route through
        // `call_multimodal` (the agent loop, in fact) get the proper
        // image branch instead.
        if image_media_type(&path).is_some() {
            return Err(Error::Tool(format!(
                "{} is an image; use call_multimodal or invoke Read via the agent loop",
                path.display()
            )));
        }

        let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize);

        // M6.23 BUG RT1: pre-flight size check so a multi-GB file
        // doesn't OOM the worker via `read_to_string`. The agent's
        // tool-result truncation (TOOL_RESULT_CONTEXT_LIMIT=50KB)
        // catches huge OUTPUT, but the read itself happens before
        // truncation. Cap at 100 MB; require offset+limit for larger
        // files (with the hint in the error message).
        let file_size = std::fs::metadata(&path)
            .map_err(|e| Error::Tool(format!("stat {}: {e}", path.display())))?
            .len();
        if file_size > MAX_TEXT_BYTES {
            return Err(Error::Tool(format!(
                "{} is {} bytes — over the {}-byte cap. Use `offset` + `limit` to read a slice, \
                 or use Bash + `head`/`tail`/`sed` for very large files.",
                path.display(),
                file_size,
                MAX_TEXT_BYTES,
            )));
        }

        let contents = std::fs::read_to_string(&path)
            .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?;

        if offset == 0 && limit.is_none() {
            return Ok(contents);
        }

        let lines: Vec<&str> = contents.lines().collect();
        let start = offset.saturating_sub(1).min(lines.len());
        let end = limit
            .map(|l| start.saturating_add(l))
            .unwrap_or(lines.len())
            .min(lines.len());
        Ok(lines[start..end].join("\n"))
    }

    /// Override the multimodal entry point so an image file returns an
    /// inline base64-encoded image block alongside a text summary
    /// (so providers without multimodal support still get descriptive
    /// metadata via `ToolResultContent::to_text()`).
    async fn call_multimodal(&self, input: Value) -> Result<ToolResultContent> {
        let raw_path = req_str(&input, "path")?;
        let path = crate::sandbox::Sandbox::check(raw_path)?;

        let Some(ext_mime) = image_media_type(&path) else {
            // Non-image extension: defer to the existing text-branch
            // behavior. We don't sniff arbitrary files because users
            // don't expect Read on a `.txt` to return image bytes.
            return self.call(input).await.map(ToolResultContent::Text);
        };

        let bytes = std::fs::read(&path)
            .map_err(|e| Error::Tool(format!("read image {}: {e}", path.display())))?;

        // Trust magic bytes over the file extension. Real-world cards/
        // screenshots get saved with the wrong extension all the time
        // (`.png` containing JPEG, `.jpg` containing PNG). Anthropic /
        // OpenAI / Gemini all 400 when the declared media_type doesn't
        // match what the decoder sees, so we sniff and use the actual
        // format. If the bytes don't match any of the four formats we
        // accept, we error out cleanly here rather than shipping bytes
        // with a guessed MIME — providers would reject them anyway,
        // but with a less actionable error. (`image_media_type` and
        // `sniff_image_mime` cover the same four formats, so a working
        // image with one of these extensions is guaranteed to sniff.)
        let mime = sniff_image_mime(&bytes).ok_or_else(|| {
            // ext_mime is unused on this path but kept in the message
            // for context — tells the user "you said it was X, but
            // the bytes aren't a recognised image".
            Error::Tool(format!(
                "{}: bytes don't match any supported image format \
                 (PNG/JPEG/WebP/GIF) despite extension claiming {}. \
                 File may be corrupted, encrypted, or saved with the \
                 wrong extension.",
                path.display(),
                ext_mime,
            ))
        })?;

        // Auto-downscale anything over the byte cap or the vision long-edge
        // limit; small images pass through untouched (no decode/re-encode).
        let down = downscale_for_vision(&bytes, mime)?;
        let (out_bytes, out_mime): (&[u8], &str) = match &down {
            Some((b, m)) => (b.as_slice(), m),
            None => (bytes.as_slice(), mime),
        };
        let note = match &down {
            Some(_) => format!(
                " · downscaled from {} KB {}",
                (bytes.len() + 512) / 1024,
                mime
            ),
            None => String::new(),
        };

        let data = base64::engine::general_purpose::STANDARD.encode(out_bytes);
        let summary = format!(
            "image: {} · {} KB · {}{}",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("(unnamed)"),
            (out_bytes.len() + 512) / 1024,
            out_mime,
            note
        );

        Ok(ToolResultContent::Blocks(vec![
            ToolResultBlock::Image {
                source: ImageSource::Base64 {
                    media_type: out_mime.to_string(),
                    data,
                },
            },
            ToolResultBlock::Text { text: summary },
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn reads_whole_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, "line1\nline2\nline3\n").unwrap();

        let out = ReadTool
            .call(json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();
        assert_eq!(out, "line1\nline2\nline3\n");
    }

    #[tokio::test]
    async fn reads_slice_with_offset_and_limit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("many.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();

        let out = ReadTool
            .call(json!({
                "path": path.to_string_lossy(),
                "offset": 2,
                "limit": 2
            }))
            .await
            .unwrap();
        assert_eq!(out, "b\nc");
    }

    #[tokio::test]
    async fn missing_path_errors() {
        let err = ReadTool.call(json!({})).await.unwrap_err();
        assert!(format!("{err}").contains("path"));
    }

    #[tokio::test]
    async fn nonexistent_file_errors() {
        let err = ReadTool
            .call(json!({"path": "/nope/does/not/exist.txt"}))
            .await
            .unwrap_err();
        let s = format!("{err}");
        // M6.23 BUG RT1: error may surface from `stat` (the new
        // pre-flight size check) instead of `read`. Either word
        // means we surfaced a clear filesystem-error message.
        assert!(
            s.contains("stat") || s.contains("read"),
            "expected stat/read in error, got: {s}"
        );
    }

    /// M6.23 BUG RT1: oversize-file pre-flight check rejects with a
    /// clear "use offset+limit" hint instead of OOMing on
    /// `read_to_string`. Tested by stubbing the cap (the production
    /// MAX_TEXT_BYTES is 100MB which is impractical to fixture).
    /// Instead verify the error path triggers when metadata reports
    /// a size > MAX_TEXT_BYTES via a check on the helper logic
    /// indirectly.
    #[tokio::test]
    async fn oversize_text_file_errors_before_read() {
        // Build a temp file just over the cap by sparse-write — we
        // don't actually need 100MB of bytes; on most filesystems
        // `set_len` creates a sparse file that reports the right
        // metadata.len() without consuming disk.
        let dir = tempdir().unwrap();
        let path = dir.path().join("huge.txt");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_TEXT_BYTES + 1).unwrap();

        let err = ReadTool
            .call(json!({"path": path.to_string_lossy()}))
            .await
            .unwrap_err();
        let s = format!("{err}");
        assert!(
            s.contains("over the") && s.contains("cap"),
            "expected oversize error, got: {s}"
        );
        assert!(
            s.contains("offset") && s.contains("limit"),
            "error should hint at the offset+limit slice escape, got: {s}"
        );
    }

    #[tokio::test]
    async fn offset_past_end_returns_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tiny.txt");
        std::fs::write(&path, "only-line\n").unwrap();
        let out = ReadTool
            .call(json!({
                "path": path.to_string_lossy(),
                "offset": 100,
                "limit": 10
            }))
            .await
            .unwrap();
        assert_eq!(out, "");
    }

    /// Smallest valid PNG (a single transparent pixel) — embedded as
    /// bytes so the test doesn't depend on a fixture file.
    const ONE_PIXEL_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[tokio::test]
    async fn call_multimodal_returns_image_blocks_for_png() {
        use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};

        let dir = tempdir().unwrap();
        let path = dir.path().join("pixel.png");
        std::fs::write(&path, ONE_PIXEL_PNG).unwrap();

        let out = ReadTool
            .call_multimodal(json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();

        let ToolResultContent::Blocks(blocks) = out else {
            panic!("expected Blocks variant for image, got Text");
        };
        assert_eq!(blocks.len(), 2, "expected image + summary text");

        // First block is the inline image.
        match &blocks[0] {
            ToolResultBlock::Image {
                source: ImageSource::Base64 { media_type, data },
            } => {
                assert_eq!(media_type, "image/png");
                assert!(!data.is_empty(), "base64 data should not be empty");
            }
            other => panic!("expected Image block, got {other:?}"),
        }

        // Second block is the text summary.
        match &blocks[1] {
            ToolResultBlock::Text { text } => {
                assert!(text.contains("pixel.png"), "summary should name the file");
                assert!(text.contains("image/png"), "summary should name the mime");
            }
            other => panic!("expected Text block, got {other:?}"),
        }
    }

    /// Smallest valid JPEG (1×1 pixel, gray) — embedded as bytes
    /// for the wrong-extension regression test below.
    const ONE_PIXEL_JPEG: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06, 0x07, 0x06, 0x05, 0x08, 0x07,
        0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D, 0x0C, 0x0B, 0x0B, 0x0C, 0x19, 0x12,
        0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D, 0x1A, 0x1C, 0x1C, 0x20, 0x24, 0x2E, 0x27,
        0x20, 0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28, 0x37, 0x29, 0x2C, 0x30, 0x31, 0x34, 0x34, 0x34,
        0x1F, 0x27, 0x39, 0x3D, 0x38, 0x32, 0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF, 0xC0, 0x00, 0x0B,
        0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x14, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0xFF, 0xC4, 0x00, 0x14, 0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xDA, 0x00, 0x08, 0x01,
        0x01, 0x00, 0x00, 0x3F, 0x00, 0x37, 0xFF, 0xD9,
    ];

    #[test]
    fn sniff_recognizes_supported_image_formats() {
        assert_eq!(sniff_image_mime(ONE_PIXEL_PNG), Some("image/png"));
        assert_eq!(sniff_image_mime(ONE_PIXEL_JPEG), Some("image/jpeg"));
        assert_eq!(sniff_image_mime(b"GIF89a..."), Some("image/gif"));
        // RIFF + 4-byte length + WEBP
        let mut webp = Vec::from(*b"RIFF");
        webp.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(sniff_image_mime(&webp), Some("image/webp"));

        // Random bytes don't match anything.
        assert_eq!(sniff_image_mime(b"<html>not an image"), None);
        assert_eq!(sniff_image_mime(b""), None);
    }

    #[tokio::test]
    async fn call_multimodal_sniffs_actual_format_when_extension_lies() {
        // Regression for the v0.3.2-dev Anthropic bug: a file named
        // `card.png` containing JPEG bytes (real-world: business
        // cards exported from a tool that wrote .png by default).
        // Anthropic 400s if declared media_type and actual bytes
        // disagree. We must sniff and report the truth.
        use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};

        let dir = tempdir().unwrap();
        let path = dir.path().join("misnamed.png");
        std::fs::write(&path, ONE_PIXEL_JPEG).unwrap();

        let out = ReadTool
            .call_multimodal(json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();

        let ToolResultContent::Blocks(blocks) = out else {
            panic!("expected Blocks variant for image");
        };

        // Image block must report image/jpeg (the truth) regardless
        // of the .png extension.
        match &blocks[0] {
            ToolResultBlock::Image {
                source: ImageSource::Base64 { media_type, .. },
            } => {
                assert_eq!(
                    media_type, "image/jpeg",
                    "sniffed MIME should win over extension"
                );
            }
            other => panic!("expected Image block, got {other:?}"),
        }

        // Sibling text summary should also reflect the sniffed MIME
        // so the model's text-fallback view of the message is
        // internally consistent.
        match &blocks[1] {
            ToolResultBlock::Text { text } => {
                assert!(
                    text.contains("image/jpeg"),
                    "summary should report sniffed MIME, got: {text}"
                );
            }
            other => panic!("expected Text block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn call_text_branch_rejects_image_extension() {
        // The text-only entry point must error rather than attempt
        // utf-8 decoding on PNG bytes — that would surface as a
        // confusing "stream did not contain valid UTF-8" rather than
        // a clear "use call_multimodal" hint.
        let dir = tempdir().unwrap();
        let path = dir.path().join("pixel.png");
        std::fs::write(&path, ONE_PIXEL_PNG).unwrap();

        let err = ReadTool
            .call(json!({"path": path.to_string_lossy()}))
            .await
            .unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("image"), "got: {s}");
    }

    #[tokio::test]
    async fn call_multimodal_downscales_oversized_png() {
        use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};

        let dir = tempdir().unwrap();
        let path = dir.path().join("big.png");
        // 2000x1000 — long edge exceeds the 1568px target, so it must be
        // resized even though the byte size may be under the cap.
        let buf = image::RgbImage::from_fn(2000, 1000, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        image::DynamicImage::ImageRgb8(buf).save(&path).unwrap();

        let out = ReadTool
            .call_multimodal(json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();
        let ToolResultContent::Blocks(blocks) = out else {
            panic!("expected Blocks");
        };
        let ToolResultBlock::Image {
            source: ImageSource::Base64 { data, .. },
        } = &blocks[0]
        else {
            panic!("expected image block first");
        };
        let raw = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap();
        assert!(raw.len() <= MAX_IMAGE_BYTES, "still over byte cap");
        let (w, h) = image::io::Reader::new(std::io::Cursor::new(&raw))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        // Aspect preserved, long edge clamped to the target.
        assert_eq!((w, h), (1568, 784), "expected 2:1 fit to 1568px");

        // Summary block notes the downscale.
        let ToolResultBlock::Text { text } = &blocks[1] else {
            panic!("expected summary text");
        };
        assert!(text.contains("downscaled"), "got: {text}");
    }

    #[tokio::test]
    async fn call_multimodal_passes_small_image_through_unchanged() {
        use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};

        let dir = tempdir().unwrap();
        let path = dir.path().join("tiny.png");
        std::fs::write(&path, ONE_PIXEL_PNG).unwrap();

        let out = ReadTool
            .call_multimodal(json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();
        let ToolResultContent::Blocks(blocks) = out else {
            panic!("expected Blocks");
        };
        let ToolResultBlock::Image {
            source: ImageSource::Base64 { data, media_type },
        } = &blocks[0]
        else {
            panic!("expected image block first");
        };
        assert_eq!(media_type, "image/png");
        // Untouched: base64 equals the original bytes (no decode/re-encode).
        let expected = base64::engine::general_purpose::STANDARD.encode(ONE_PIXEL_PNG);
        assert_eq!(data, &expected);
    }
}
