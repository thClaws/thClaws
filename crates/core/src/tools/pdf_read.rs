//! `PdfRead` — extract text from a PDF by shelling out to `pdftotext`
//! (poppler-utils). poppler does the heavy lifting (Thai shaping, ligature
//! decomposition, layout-aware extraction); we just wrap it with sandbox
//! checks, page-range parsing, and a clear missing-binary error.
//!
//! When the PDF is a scanned image (no embedded text layer), pdftotext
//! returns empty / mostly-empty output. In that case the multimodal
//! entry point falls through to `pdftoppm` which renders each requested
//! page as a PNG and returns them as image blocks so the model sees the
//! pages visually. Both binaries ship together in poppler-utils, so the
//! fallback adds no new install requirement.
//!
//! Thai gets two extra layers. `-layout` sprinkles spurious spaces inside
//! Thai words (the script has no word boundaries, so every glyph gap reads
//! as a space); `normalize_thai_spacing` repairs that with script-level
//! rules — no per-document word lists. And when a PDF's font carries a
//! broken `ToUnicode` map, pdftotext emits genuinely wrong characters that
//! no post-processing can recover; `thai_looks_garbled` detects the heavy
//! cluster fragmentation that comes with it and routes those pages to the
//! vision path, which reads the rendered glyphs instead of the bad map.
//!
//! Why shell-out instead of a pure-Rust pdf crate: extraction quality
//! across real-world PDFs (tagged structure, form fields, embedded fonts
//! with non-standard cmaps) is dominated by poppler's twenty-plus years
//! of corner-case handling. The Rust crates that exist are good for
//! valid PDFs but break on the long tail. Measured against ground truth
//! (`docs/pdf-extraction-bench.md`): poppler 98.3% / 97.8% Thai 5-gram
//! recall / precision, PDFium 89.9% / 80.5% — and PDFium's damage is a
//! duplicated Thai cluster every ~28 characters that `thai_looks_garbled`
//! does NOT catch, so it would degrade Thai silently.
//!
//! The cost of that choice is that poppler has to be installed, which a
//! desktop user has not done and a Windows user has no instructions for.
//! So when `pdftotext` is missing, text comes from the document service on
//! the gateway (`/pdf/text`, dev-plan/69) and, on open-core only, from the
//! public `pdf.thclaws.cloud` service (dev-plan/66). The file leaves the
//! machine, so those paths are approval-gated — except on an EE install,
//! whose gateway is the organisation's own — and the result says where the
//! text came from. When poppler is present nothing here reaches the network
//! unless the caller asks for `ocr: "tesseract"` or the install is EE.
//!
//! Scans: `ocr: "auto"` OCRs them on the document service (tesseract,
//! tha+eng) and hands pages under `OCR_MIN_CONFIDENCE` to the vision model
//! as images; with no service it is the vision path alone, as before.

use super::slide_render::SlideEndpoint;
use super::{doc_render, req_str, Tool};
use crate::error::{Error, Result};
use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const EXTRACT_TIMEOUT: Duration = Duration::from_secs(60);

/// Vision-OCR fallback constants. Tuned conservatively — the goal is
/// "make scanned PDFs work at all", not "perfectly handle 100-page
/// scans". Users with bigger documents can paginate via the `pages`
/// parameter.
mod fallback {
    use super::Duration;
    /// If pdftotext returns less than this many non-whitespace chars
    /// per requested page on average, treat the PDF as scanned and
    /// fall through to vision OCR. 50 chars ≈ a one-line title;
    /// anything thinner is almost certainly a scanned image.
    pub const MIN_CHARS_PER_PAGE: usize = 50;
    /// Hard cap on pages to render. Twenty 150-DPI A4 PNGs at typical
    /// content density land around 8-15 MB total before base64 — fits
    /// comfortably under most providers' per-request limits.
    pub const MAX_PAGES_TO_RENDER: u32 = 20;
    /// Render resolution. 150 DPI is the sweet spot for OCR quality
    /// vs file size; below that fine print drops out of the model's
    /// recognition; above that the size grows quadratically with
    /// minimal accuracy gain.
    pub const RENDER_DPI: u32 = 150;
    /// Per-page byte cap. Anthropic's documented per-image limit is
    /// 5 MB; matches the Read tool's MAX_IMAGE_BYTES.
    pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
    /// Render-step timeout. pdftoppm at 150 DPI on a 20-page PDF is
    /// usually ~5s on a modern machine, but spinning rust + complex
    /// fonts can push past 30s.
    pub const RENDER_TIMEOUT: Duration = Duration::from_secs(120);

    /// Thai garble detection. A correctly-extracted Thai text layer
    /// orphans essentially no combining marks behind a space; a PDF whose
    /// font has a broken `ToUnicode` map makes pdftotext fragment clusters
    /// and leaves many. Above BOTH thresholds we treat the text layer as
    /// untrustworthy and prefer the vision path (which reads the rendered
    /// glyphs, sidestepping the bad mapping). `MIN_THAI…` keeps short
    /// snippets from tripping the ratio; AND-ing the two avoids firing on
    /// a long clean doc with a stray fragment.
    pub const MIN_THAI_FOR_GARBLE_CHECK: usize = 40;
    pub const GARBLE_ORPHAN_MARKS: usize = 6;
    pub const GARBLE_ORPHAN_RATIO: f32 = 0.04;

    /// Below this many Thai characters there is not enough text to judge how
    /// the extractor is treating the script.
    pub const MIN_THAI_FOR_MODE_CHOICE: usize = 200;
    /// "Mark, space, consonant" per 1000 Thai characters, above which
    /// `-layout` is breaking words rather than preserving columns. Measured
    /// over 24 Thai PDFs: the documents above it gain 1.33x-3.70x average Thai
    /// run length from `-raw`, the ones at 2.6 and below gain exactly 1.00x,
    /// and budget tables sit at 0.0-0.8 so their columns are never traded away.
    pub const SHRED_PER_1K_RETRY: f32 = 10.0;
    /// Mean length of a contiguous Thai run, below which the text layer holds
    /// no words at all — every character stands alone. One PDF in that corpus
    /// reads 1.0 in both modes, and no spacing rule can help it; it belongs on
    /// the vision path.
    pub const MIN_THAI_RUN: f32 = 2.0;
    /// Ceiling on a whole-document OCR (Files-tab "Convert to markdown"),
    /// which pages through the service 20 at a time.
    pub const MAX_OCR_PAGES: u32 = 200;
}

pub struct PdfReadTool;

/// How a scanned page is read. `vision: true` predates this and is an alias
/// for `Vision`.
#[derive(Clone, Copy, PartialEq, Debug)]
enum OcrMode {
    /// Text layer first; a scan goes to OCR on the document service, and a
    /// page OCR is unsure of goes to the vision model.
    Auto,
    Vision,
    Tesseract,
}

fn ocr_mode(input: &Value) -> Result<OcrMode> {
    if input
        .get("vision")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Ok(OcrMode::Vision);
    }
    let raw = input
        .get("ocr")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match raw.as_str() {
        "" | "auto" => Ok(OcrMode::Auto),
        "vision" => Ok(OcrMode::Vision),
        "tesseract" => Ok(OcrMode::Tesseract),
        other => Err(Error::Tool(format!(
            "ocr must be \"auto\", \"vision\" or \"tesseract\", got {other:?}"
        ))),
    }
}

#[async_trait]
impl Tool for PdfReadTool {
    fn name(&self) -> &'static str {
        "PdfRead"
    }

    fn description(&self) -> &'static str {
        "Extract text from a PDF file. Optional `pages` parameter accepts \"all\" \
         (default), \"3\" (single page), or \"1-5\" (inclusive range). Text comes \
         from `pdftotext` (poppler-utils) when installed, otherwise from the \
         document service on the thClaws / organisation gateway. **Scanned / \
         image-based PDFs** are handled by `ocr`: \"auto\" (default) OCRs the \
         pages (Thai + English) on the document service and sends pages it is \
         unsure of to you as images; \"vision\" sends every page as an image \
         (same as `vision: true`); \"tesseract\" returns OCR text only. The \
         result says which pages came from OCR (with confidence) and which are \
         images."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":  {"type": "string", "description": "PDF file path."},
                "pages": {"type": "string", "description": "Page range: \"all\", \"N\", or \"M-N\". Default: all."},
                "ocr": {"type": "string", "enum": ["auto", "vision", "tesseract"], "description": "How scanned pages are read. auto (default): OCR, with low-confidence pages sent as images; vision: render every page as an image for you to read; tesseract: OCR text only, skipping the text layer."},
                "vision": {"type": "boolean", "description": "Same as ocr: \"vision\" — render pages to images instead of extracting text. Use when the text layer is garbled, e.g. a broken Thai ToUnicode map that swaps า/ำ. Default false."}
            },
            "required": ["path"]
        })
    }

    /// Reading a local file needs no approval — sending it somewhere does,
    /// except to an EE organisation's own gateway (owner decision,
    /// dev-plan/69). The gate tracks exactly the condition that sends bytes
    /// off the machine.
    fn requires_approval(&self, input: &Value) -> bool {
        upload_destination(input).is_some()
    }

    fn approval_summary(&self, input: &Value) -> Option<String> {
        let (dest, why) = upload_destination(input)?;
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("?");
        Some(format!("{why} — UPLOADS {path} to {dest}"))
    }

    async fn call(&self, input: Value) -> Result<String> {
        let validated = crate::sandbox::Sandbox::check(req_str(&input, "path")?)?;
        let pages_spec = input.get("pages").and_then(|v| v.as_str()).unwrap_or("all");
        let (first, last) = parse_page_range(pages_spec)?;
        let uploads = self.uploads_for(&input);
        let (raw, source) = extract_text_raw(&validated, first, last, uploads).await?;
        Ok(with_note(normalize_thai_spacing(&raw), &source))
    }

    /// Multimodal entry: text first; a scan or a garbled text layer goes to
    /// OCR and/or the vision path per `ocr`.
    async fn call_multimodal(&self, input: Value) -> Result<ToolResultContent> {
        let validated = crate::sandbox::Sandbox::check(req_str(&input, "path")?)?;
        let pages_spec = input.get("pages").and_then(|v| v.as_str()).unwrap_or("all");
        let (first, last) = parse_page_range(pages_spec)?;
        let mode = ocr_mode(&input)?;
        let uploads = self.uploads_for(&input);

        let services = Services::resolve(uploads);
        match mode {
            OcrMode::Vision => {
                return vision_blocks(&validated, first, last, &services, "")
                    .await
                    .map(ToolResultContent::Blocks)
            }
            OcrMode::Tesseract => {
                return read_scanned(&validated, first, last, mode, &services, "").await
            }
            OcrMode::Auto => {}
        }

        let (raw, source) = extract_text_raw(&validated, first, last, uploads).await?;
        // "Looks scanned" averages chars per form-feed page; "looks garbled"
        // counts Thai marks orphaned behind spaces. Either one means the
        // text layer cannot be trusted and the glyphs must be read instead.
        if !text_is_too_sparse(&raw) && !thai_looks_garbled(&raw) {
            return Ok(ToolResultContent::Text(with_note(
                normalize_thai_spacing(&raw),
                &source,
            )));
        }
        read_scanned(&validated, first, last, mode, &services, &raw).await
    }
}

impl PdfReadTool {
    /// An approved call may upload; one that never asked may not.
    fn uploads_for(&self, input: &Value) -> CloudFallback {
        if self.requires_approval(input) {
            CloudFallback::Allowed
        } else {
            CloudFallback::Refused
        }
    }
}

/// Where an approved call would send the file and why; `None` when the call
/// stays local or the destination is the EE organisation's own gateway.
/// With poppler installed an `auto` read stays local — a scan goes to the
/// local vision path, as before — so only an explicit `tesseract` asks.
fn upload_destination(input: &Value) -> Option<(String, &'static str)> {
    if doc_render::uploads_preapproved() {
        return None;
    }
    let service = doc_render::endpoint("pdf/text")
        .ok()
        .map(|ep| doc_render::describe(&ep));
    if poppler_missing() {
        return service
            .or_else(cloud_endpoint)
            .map(|d| (d, "no poppler on this machine"));
    }
    if ocr_mode(input).ok() == Some(OcrMode::Tesseract) {
        return service.map(|d| (d, "OCR runs on the document service"));
    }
    None
}

/// True when the extracted text is so thin it almost certainly came
/// from a PDF without an embedded text layer (scanned image). Splits
/// on form-feed (pdftotext's page boundary marker) and checks the
/// average non-whitespace char count per page against
/// `fallback::MIN_CHARS_PER_PAGE`. A single-page PDF with a one-line
/// title (~30 chars) trips the threshold; that's intentional — vision
/// OCR adds little overhead for a single page and meaningfully
/// improves quality on covers / title slides / posters.
fn text_is_too_sparse(text: &str) -> bool {
    let pages: Vec<&str> = text.split('\u{000C}').collect();
    let page_count = pages.len().max(1);
    let total_meaningful: usize = text.chars().filter(|c| !c.is_whitespace()).count();
    let avg = total_meaningful / page_count;
    avg < fallback::MIN_CHARS_PER_PAGE
}

/// True for Thai vowels/tone marks that must attach to a preceding base
/// character — they can never legitimately start a cluster, so a space
/// in front of one is always a pdftotext fragmentation artifact.
/// Covers U+0E30–U+0E3A (sara/phinthu) and U+0E47–U+0E4E (tone marks +
/// thanthakhat + nikhahit + yamakkan).
fn is_thai_trailing_mark(c: char) -> bool {
    matches!(c, '\u{0E30}'..='\u{0E3A}' | '\u{0E47}'..='\u{0E4E}')
}

/// "Mark, space, consonant" per 1000 Thai characters — the shape `-layout`
/// leaves when it pads a glyph gap inside a Thai word.
fn shred_per_1k(text: &str) -> f32 {
    let chars: Vec<char> = text.chars().collect();
    let thai = chars.iter().filter(|c| is_thai(**c)).count();
    if thai == 0 {
        return 0.0;
    }
    let breaks = chars
        .windows(3)
        .filter(|w| w[1] == ' ' && is_thai_trailing_mark(w[0]) && is_thai_consonant(w[2]))
        .count();
    1000.0 * breaks as f32 / thai as f32
}

/// Mean length of a contiguous run of Thai characters. Thai writes without
/// spaces between words, so a healthy run is many characters long; extraction
/// damage is what cuts it down. No dictionary needed, which matters — the
/// bundled word list is a 229-word placeholder.
fn mean_thai_run(text: &str) -> f32 {
    let (mut runs, mut total, mut cur) = (0usize, 0usize, 0usize);
    for c in text.chars() {
        if is_thai(c) {
            cur += 1;
        } else if cur > 0 {
            runs += 1;
            total += cur;
            cur = 0;
        }
    }
    if cur > 0 {
        runs += 1;
        total += cur;
    }
    if runs == 0 {
        0.0
    } else {
        total as f32 / runs as f32
    }
}

/// Is `-layout` breaking this Thai text apart rather than laying it out?
fn thai_is_shredded(text: &str) -> bool {
    let thai = text.chars().filter(|c| is_thai(*c)).count();
    thai >= fallback::MIN_THAI_FOR_MODE_CHOICE && shred_per_1k(text) > fallback::SHRED_PER_1K_RETRY
}

/// Thai consonants — the class a stray `-layout` space lands in front of.
fn is_thai_consonant(c: char) -> bool {
    ('\u{0E01}'..='\u{0E2E}').contains(&c)
}

/// Any character in the Thai block.
fn is_thai(c: char) -> bool {
    ('\u{0E01}'..='\u{0E5B}').contains(&c)
}

/// Repair Thai clusters that `pdftotext -layout` fragmented by orphaning a
/// combining mark behind a space (e.g. "ผู ้" → "ผู้", "ก ำหนด" → "กำหนด").
/// A Thai vowel/tone mark can never legitimately follow a space — it must
/// attach to the preceding base — so this only ever undoes fragmentation;
/// it can NEVER merge two real words. That safety is deliberate: the
/// normalizer also runs on clean Thai that uses real phrase spaces, and
/// many Thai words end in a vowel, so a rule that also stripped
/// "mark + space + consonant" would wrongly glue "เวลา ทำงาน" into one
/// token. The harder consonant-after-space fragmentation is
/// indistinguishable from a real word break without a segmentation
/// dictionary, so heavily-fragmented documents route to the vision path
/// via `thai_looks_garbled` instead of being force-joined here. This is a
/// script-level rule, not a vocabulary list, so it generalizes to any Thai
/// document; non-Thai text is untouched (the pattern needs Thai on the
/// left and a Thai mark on the right).
pub(crate) fn normalize_thai_spacing(text: &str) -> String {
    use regex::Regex;
    let drop_before_mark =
        Regex::new(r"([\u{0E01}-\u{0E4E}]) +([\u{0E30}-\u{0E3A}\u{0E47}-\u{0E4E}])").unwrap();
    let mut s = text.to_string();
    // Two passes close stacked clusters like "ษ ั ้" that one
    // left-to-right replace_all can't fully collapse.
    for _ in 0..2 {
        s = drop_before_mark.replace_all(&s, "$1$2").into_owned();
    }
    repair_broken_saraam(&s)
}

/// Repair a known broken-`ToUnicode` corruption: some PDF fonts map sara-am
/// (ำ, U+0E33) to bare sara-aa (า, U+0E32), turning e.g. "ทำงาน" into the
/// non-word "ทางาน". Unlike the spacing rule this is a guess (า is a real,
/// extremely common letter), so the table is deliberately limited to whole
/// forms that are NOT valid Thai in their า spelling — real า words (ด้าน,
/// หน้า, ภาพ, เป้า, ทางการ, ประจาน) are never listed, so the substring replace
/// can't damage them. Substring-based, not token-based, because Thai writes
/// without inter-word spaces (the corrupted form appears glued inside a run,
/// e.g. "การทางานของ"). Only covers the high-frequency words; broken-cmap PDFs
/// outside this set still need the vision-OCR path for perfect ำ.
const SARAAM_REPAIRS: &[(&str, &str)] = &[
    ("ทางาน", "ทำงาน"),
    ("ทาความ", "ทำความ"),
    ("จากัด", "จำกัด"),
    ("จานวน", "จำนวน"),
    ("จาเป็น", "จำเป็น"),
    ("จาพวก", "จำพวก"),
    ("กาหนด", "กำหนด"),
    ("กาลัง", "กำลัง"),
    ("กาไร", "กำไร"),
    ("กากับ", "กำกับ"),
    ("สาคัญ", "สำคัญ"),
    ("สาเร็จ", "สำเร็จ"),
    ("สาหรับ", "สำหรับ"),
    ("สานัก", "สำนัก"),
    ("สารวจ", "สำรวจ"),
    ("สาเนา", "สำเนา"),
    ("ดาเนิน", "ดำเนิน"),
    ("ดารง", "ดำรง"),
    ("คานวณ", "คำนวณ"),
    ("คาสั่ง", "คำสั่ง"),
    ("ชาระ", "ชำระ"),
    ("นาเสนอ", "นำเสนอ"),
    ("ลาดับ", "ลำดับ"),
    ("ตาแหน่ง", "ตำแหน่ง"),
    ("อานวย", "อำนวย"),
    ("บารุง", "บำรุง"),
];

fn repair_broken_saraam(text: &str) -> String {
    let mut s = text.to_string();
    for (bad, good) in SARAAM_REPAIRS {
        if s.contains(bad) {
            s = s.replace(bad, good);
        }
    }
    s
}

/// True when extracted Thai shows heavy cluster fragmentation — many
/// combining marks orphaned behind a space, which a correct text layer
/// never produces. It signals a broken font / `ToUnicode` map whose
/// character mapping (not just spacing) can't be trusted, so the caller
/// should prefer the vision path over the text. Computed on the *raw*
/// pdftotext output, before `normalize_thai_spacing` hides the evidence.
fn thai_looks_garbled(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let thai_total = chars.iter().filter(|c| is_thai(**c)).count();
    if thai_total < fallback::MIN_THAI_FOR_GARBLE_CHECK {
        return false;
    }
    let orphan_marks = chars
        .windows(3)
        .filter(|w| w[1] == ' ' && is_thai(w[0]) && is_thai_trailing_mark(w[2]))
        .count();
    if orphan_marks >= fallback::GARBLE_ORPHAN_MARKS
        && (orphan_marks as f32) / (thai_total as f32) > fallback::GARBLE_ORPHAN_RATIO
    {
        return true;
    }
    // Second signal: a text layer that holds no words. Thai runs together, so
    // a mean run near one character means every character came out isolated —
    // no spacing rule can reassemble that, and one PDF in the survey corpus
    // reads 1.0 in BOTH extraction modes while orphan marks stay at zero, so
    // the check above never saw it.
    mean_thai_run(text) < fallback::MIN_THAI_RUN
}

/// Extracted text with the Thai marks put back together, and a note when it
/// did not come from this machine. Never asks, so it never uploads without
/// consent: only an EE install's own document service may take the file
/// (dev-plan/69 B6 — KMS ingest used to reach pdf.thclaws.cloud unasked).
///
/// `pub(crate)` so callers outside the tool can ask for a whole
/// document with `(path, None, None)`. `kms::ingest_pdf` grew its own
/// `pdftotext` call instead and did not carry the normalisation with
/// it, which is how an ingested Thai paper kept the vowel/tone
/// fragmentation `-layout` introduces while the same PDF read through
/// this tool came out clean.
pub(crate) async fn extract_text(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
) -> Result<String> {
    extract_text_inner(validated, first, last, CloudFallback::Refused).await
}

async fn extract_text_inner(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
    cloud: CloudFallback,
) -> Result<String> {
    let (raw, source) = extract_text_raw(validated, first, last, cloud).await?;
    Ok(with_note(normalize_thai_spacing(&raw), &source))
}

/// The Files tab's "Convert to markdown": the text layer when it is usable,
/// otherwise the whole document OCR'd — on an EE install only, where the
/// document service needs no consent. No model runs here, so a page OCR is
/// unsure of is kept and flagged rather than sent to vision.
pub(crate) async fn markdown_text(validated: &std::path::Path) -> Result<String> {
    let (raw, _) = extract_text_raw(validated, None, None, CloudFallback::Refused).await?;
    if !text_is_too_sparse(&raw) && !thai_looks_garbled(&raw) {
        return Ok(normalize_thai_spacing(&raw));
    }
    let Some(ep) = doc_render_endpoint("ocr", CloudFallback::Refused) else {
        return Err(Error::Tool(
            "no usable text layer in this PDF — it is probably scanned. Ask the agent to \
             read it instead: PdfRead OCRs scanned pages"
                .into(),
        ));
    };
    let mut out = String::new();
    let mut first = 1u32;
    loop {
        let last = first + fallback::MAX_PAGES_TO_RENDER - 1;
        let got = doc_render::ocr(&ep, validated, Some(first), Some(last)).await?;
        for p in &got.pages {
            let conf = p.confidence.unwrap_or(0.0);
            let text = normalize_thai_spacing(&doc_render::join_ocr_thai_spaces(&p.text));
            if conf < doc_render::OCR_MIN_CONFIDENCE || doc_render::ocr_looks_damaged(&text) {
                out.push_str(&format!(
                    "<!-- page {}: OCR confidence {conf:.0}, check it against the scan -->\n",
                    p.n
                ));
            } else {
                out.push_str(&format!("<!-- page {}: OCR -->\n", p.n));
            }
            out.push_str(text.trim());
            out.push_str("\n\n");
        }
        if last >= got.page_count || last >= fallback::MAX_OCR_PAGES {
            break;
        }
        first = last + 1;
    }
    Ok(out)
}

/// Where the text came from, so the result can say so.
#[derive(Debug)]
enum Source {
    Local,
    DocRender(String),
    PublicService,
}

fn with_note(mut text: String, source: &Source) -> String {
    match source {
        Source::Local => {}
        Source::DocRender(dest) => text.push_str(&format!(
            "\n\n[extracted by {dest}: poppler is not installed on this machine, so the \
             file was sent there to read its text.]"
        )),
        Source::PublicService => text.push_str(CLOUD_NOTE),
    }
    text
}

/// Whether this call may put the file on the document service: when the
/// user approved the upload, or on an EE install where the service is the
/// organisation's own.
fn doc_render_endpoint(path: &str, uploads: CloudFallback) -> Option<SlideEndpoint> {
    if may_upload(uploads, doc_render::uploads_preapproved()) {
        doc_render::endpoint(path).ok()
    } else {
        None
    }
}

fn may_upload(uploads: CloudFallback, preapproved: bool) -> bool {
    uploads == CloudFallback::Allowed || preapproved
}

// ── No-poppler fallbacks: the document service, then the public one ──

/// The public, keyless endpoint. Overridable, so an enterprise can run
/// `thclaws-cloud/pdf-text/` inside its own network and keep the files there.
const DEFAULT_PDF_TEXT_API: &str = "https://pdf.thclaws.cloud/extract";

/// Matches the service's own body cap: refuse locally rather than upload
/// 30 MB to be told no.
const CLOUD_MAX_BYTES: u64 = 25 * 1024 * 1024;

/// Appended to text the service produced. The approval prompt is the consent;
/// this is so the result itself says where it came from — and how to stop it.
const CLOUD_NOTE: &str = "\n\n[extracted by pdf.thclaws.cloud: poppler is not installed \
     on this machine, so this file was uploaded to the public thClaws extraction service. \
     Install poppler-utils to keep PDFs local, or set \"pdfCloudFallback\": false to \
     disable the fallback.]";

const POPPLER_INSTALL: &str = "install poppler-utils: `brew install poppler` (macOS), \
     `apt install poppler-utils` (Debian/Ubuntu), `winget install oschwartz10612.Poppler` \
     or `scoop install poppler` (Windows)";

/// `Some(url)` when the fallback may be used, `None` when it is switched off.
fn cloud_endpoint() -> Option<String> {
    let enabled = crate::config::AppConfig::load()
        .map(|c| c.pdf_cloud_fallback)
        .unwrap_or(true);
    if !enabled {
        return None;
    }
    Some(
        std::env::var("THCLAWS_PDF_TEXT_API")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_PDF_TEXT_API.to_string()),
    )
}

fn poppler_missing() -> bool {
    !crate::config::command_on_path("pdftotext")
}

/// Whether a caller is in a position to send the file off the machine: the
/// tool path is approval-gated and may, a UI-initiated IPC arm is not and
/// may not — except to an EE organisation's own document service.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum CloudFallback {
    Allowed,
    Refused,
}

/// What to do when `pdftotext` is not installed: the document service on
/// the gateway, then the public service (open-core only), else name every
/// way to install poppler. Split out from `extract_text_raw` so each branch
/// is reachable in a test without emptying `PATH`.
async fn without_poppler(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
    cloud: CloudFallback,
) -> Result<(String, Source)> {
    let mut service_err = String::new();
    if let Some(ep) = doc_render_endpoint("pdf/text", cloud) {
        match doc_render_text(&ep, validated, first, last).await {
            Ok(text) => return Ok((text, Source::DocRender(doc_render::describe(&ep)))),
            Err(e) => service_err = format!(" The document service failed: {e}."),
        }
    }
    if cloud == CloudFallback::Refused {
        return Err(Error::Tool(format!(
            "pdftotext not found — {POPPLER_INSTALL}. Or ask the agent to read the PDF — \
             PdfRead can extract it on the document service, with your approval.{service_err}"
        )));
    }
    match cloud_endpoint() {
        Some(url) => extract_via_cloud(&url, validated, first, last)
            .await
            .map(|t| (t, Source::PublicService)),
        None => Err(Error::Tool(format!(
            "pdftotext not found — {POPPLER_INSTALL}. The no-install fallback to \
             pdf.thclaws.cloud is switched off here (`pdfCloudFallback` / \
             THCLAWS_PDF_CLOUD).{service_err}"
        ))),
    }
}

/// `/pdf/text`, pages joined with form feeds the way `pdftotext` separates
/// them, and the same `-raw` retry for Thai that `-layout` shreds.
async fn doc_render_text(
    ep: &SlideEndpoint,
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
) -> Result<String> {
    let join = |pages: doc_render::Pages<doc_render::PageText>| {
        pages
            .pages
            .into_iter()
            .map(|p| p.text)
            .collect::<Vec<_>>()
            .join("\u{000C}")
    };
    let laid_out = join(doc_render::pdf_text(ep, validated, first, last, "layout").await?);
    if !thai_is_shredded(&laid_out) {
        return Ok(laid_out);
    }
    match doc_render::pdf_text(ep, validated, first, last, "raw").await {
        Ok(raw) => Ok(join(raw)),
        Err(_) => Ok(laid_out),
    }
}

/// POST the file to the extraction service and return its text.
async fn extract_via_cloud(
    url: &str,
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
) -> Result<String> {
    let size = tokio::fs::metadata(validated)
        .await
        .map_err(|e| Error::Tool(format!("read {}: {e}", validated.display())))?
        .len();
    if size > CLOUD_MAX_BYTES {
        return Err(Error::Tool(format!(
            "{} is {:.1} MB; the no-install extraction service caps uploads at 25 MB. \
             Install poppler-utils to read it locally with no limit.",
            validated.display(),
            size as f64 / (1024.0 * 1024.0)
        )));
    }
    let bytes = tokio::fs::read(validated)
        .await
        .map_err(|e| Error::Tool(format!("read {}: {e}", validated.display())))?;
    let name = validated
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "input.pdf".into());

    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(name)
        .mime_str("application/pdf")
        .map_err(|e| Error::Tool(format!("multipart: {e}")))?;
    let mut form = reqwest::multipart::Form::new().part("file", part);
    if let Some(f) = first {
        form = form.text("first", f.to_string());
    }
    if let Some(l) = last {
        form = form.text("last", l.to_string());
    }

    // Generous: the service's own budget is 30 s and the upload is on top.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| Error::Tool(format!("http client: {e}")))?;
    let resp = client.post(url).multipart(form).send().await.map_err(|e| {
        Error::Tool(format!(
            "no poppler locally and the extraction service at {url} is unreachable: {e}. \
                 Install poppler-utils to read PDFs offline."
        ))
    })?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Tool(format!("read response: {e}")))?;
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| body.chars().take(300).collect());
        return Err(Error::Tool(format!("pdf extraction service: {detail}")));
    }
    Ok(body)
}

/// Raw extracted text (no Thai repair yet, so the garble check sees the
/// evidence) and where it came from.
async fn extract_text_raw(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
    cloud: CloudFallback,
) -> Result<(String, Source)> {
    if poppler_missing() {
        return without_poppler(validated, first, last, cloud).await;
    }
    let laid_out = run_pdftotext(validated, first, last, "-layout").await?;
    // `-layout` reproduces the page by padding with spaces, and it puts one at
    // every glyph gap. Thai does not space its words, so those land INSIDE
    // them: `บริษัท` comes out as `บริ ษ ทั`. Measured over 24 Thai PDFs, the
    // density of "mark, space, consonant" separates the shredded documents
    // from the intact ones cleanly, and re-reading a shredded one with `-raw`
    // lengthens the average Thai run by 1.3x-3.7x while changing the Thai
    // character count by 0.00% (docs/pdf-thai-extraction-modes.md).
    if !thai_is_shredded(&laid_out) {
        return Ok((laid_out, Source::Local));
    }
    match run_pdftotext(validated, first, last, "-raw").await {
        Ok(raw) => {
            eprintln!(
                "\x1b[2m[pdf] -layout shredded this Thai text ({:.0} breaks per 1k) — re-read with -raw\x1b[0m",
                shred_per_1k(&laid_out)
            );
            Ok((raw, Source::Local))
        }
        // The second read is an improvement, never a requirement.
        Err(_) => Ok((laid_out, Source::Local)),
    }
}

/// One `pdftotext` run in the given mode.
async fn run_pdftotext(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
    mode: &str,
) -> Result<String> {
    let mut cmd = Command::new("pdftotext");
    cmd.arg(mode);
    if let Some(f) = first {
        cmd.arg("-f").arg(f.to_string());
    }
    if let Some(l) = last {
        cmd.arg("-l").arg(l.to_string());
    }
    cmd.arg(validated.as_os_str()).arg("-"); // stdout
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Tool(format!("pdftotext not found — {POPPLER_INSTALL}"))
        } else {
            Error::Tool(format!("spawn pdftotext: {e}"))
        }
    })?;

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();

    let run = async {
        let stdout_fut = stdout.read_to_end(&mut out_buf);
        let stderr_fut = stderr.read_to_end(&mut err_buf);
        let (a, b) = tokio::join!(stdout_fut, stderr_fut);
        a.map_err(|e| Error::Tool(format!("read stdout: {e}")))?;
        b.map_err(|e| Error::Tool(format!("read stderr: {e}")))?;
        let status = child
            .wait()
            .await
            .map_err(|e| Error::Tool(format!("wait pdftotext: {e}")))?;
        Ok::<_, Error>(status)
    };

    let status = match timeout(EXTRACT_TIMEOUT, run).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(Error::Tool(format!(
                "pdftotext timed out after {}s",
                EXTRACT_TIMEOUT.as_secs()
            )));
        }
    };

    if !status.success() {
        let stderr_str = String::from_utf8_lossy(&err_buf);
        return Err(Error::Tool(format!(
            "pdftotext failed (exit {}): {}",
            status.code().unwrap_or(-1),
            stderr_str.trim()
        )));
    }

    Ok(String::from_utf8_lossy(&out_buf).to_string())
}

// ── Scanned pages: OCR, then vision ──

/// What can read a scan for this call, resolved once.
struct Services {
    ocr: Option<SlideEndpoint>,
    pages: Option<SlideEndpoint>,
    local_render: bool,
}

impl Services {
    fn resolve(uploads: CloudFallback) -> Self {
        Services {
            ocr: doc_render_endpoint("ocr", uploads),
            pages: doc_render_endpoint("pdf/pages", uploads),
            local_render: crate::config::command_on_path("pdftoppm"),
        }
    }
}

/// The page range a render or OCR call covers, capped at
/// `MAX_PAGES_TO_RENDER`, plus `(requested, rendered)` when it was cut.
/// "all" has no known end, so it becomes the first page + cap and the
/// renderer clips to what the PDF has.
fn capped_range(first: Option<u32>, last: Option<u32>) -> (u32, u32, Option<(u32, u32)>) {
    let render_first = first.unwrap_or(1);
    match last {
        Some(l) if l >= render_first => {
            let span = l - render_first + 1;
            if span > fallback::MAX_PAGES_TO_RENDER {
                (
                    render_first,
                    render_first + fallback::MAX_PAGES_TO_RENDER - 1,
                    Some((span, fallback::MAX_PAGES_TO_RENDER)),
                )
            } else {
                (render_first, l, None)
            }
        }
        _ => (
            render_first,
            render_first + fallback::MAX_PAGES_TO_RENDER - 1,
            None,
        ),
    }
}

/// Read pages whose text layer is unusable. `auto`: OCR on the document
/// service, with each page under `OCR_MIN_CONFIDENCE` sent to the vision
/// model instead; no service → vision for everything, as before.
/// `tesseract`: OCR text only. `layer` is whatever text extraction found,
/// shown when there is no way to read the glyphs at all.
async fn read_scanned(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
    mode: OcrMode,
    services: &Services,
    layer: &str,
) -> Result<ToolResultContent> {
    let Some(ep) = services.ocr.as_ref() else {
        if mode == OcrMode::Tesseract {
            return Err(Error::Tool(
                "OCR runs on the document service, which is not reachable from here — sign in \
                 to the thClaws Gateway or set SLIDE_RENDER_API. `ocr: \"vision\"` reads the \
                 pages as images instead."
                    .into(),
            ));
        }
        return vision_or_explain(validated, first, last, services, layer, "").await;
    };
    let (ocr_first, ocr_last, truncated) = capped_range(first, last);
    let got = match doc_render::ocr(ep, validated, Some(ocr_first), Some(ocr_last)).await {
        Ok(got) => got,
        Err(e) if mode == OcrMode::Auto => {
            let why = format!("OCR failed ({e}), so the pages are read as images instead. ");
            return vision_or_explain(validated, first, last, services, layer, &why).await;
        }
        Err(e) => return Err(e),
    };

    let mut text = String::new();
    let mut by_ocr: Vec<String> = Vec::new();
    let mut damaged: Vec<String> = Vec::new();
    let mut pages_read: Vec<u32> = Vec::new();
    for p in got.pages {
        let conf = p.confidence.unwrap_or(0.0);
        let page_text = normalize_thai_spacing(&doc_render::join_ocr_thai_spaces(&p.text));
        let bad = conf < doc_render::OCR_MIN_CONFIDENCE
            || page_text.trim().is_empty()
            || doc_render::ocr_looks_damaged(&page_text);
        if bad {
            damaged.push(p.n.to_string());
        }
        pages_read.push(p.n);
        by_ocr.push(format!("{} ({conf:.0})", p.n));
        let label = if mode == OcrMode::Auto {
            if bad {
                "OCR draft, unreliable"
            } else {
                "OCR draft"
            }
        } else {
            "OCR"
        };
        text.push_str(&format!(
            "[page {} — {label}, confidence {conf:.0}]\n{}\n\n",
            p.n,
            page_text.trim()
        ));
    }

    let mut summary = format!(
        "Scanned PDF, read by OCR (tesseract, Thai + English) on {}.",
        doc_render::describe(ep)
    );
    if !by_ocr.is_empty() {
        summary.push_str(&format!(
            " Pages from OCR (confidence 0-100): {}.",
            by_ocr.join(", ")
        ));
    }
    if let Some((requested, done)) = truncated {
        summary.push_str(&format!(
            " Only the first {done} of the {requested} requested pages were read; call again \
             with a narrower `pages` range for the rest."
        ));
    } else if last.is_none() && got.page_count > ocr_last {
        summary.push_str(&format!(
            " The PDF has {} pages; pages {}-{} were read. Call again with `pages` for the rest.",
            got.page_count, ocr_first, ocr_last
        ));
    }

    // `tesseract` asked for text only. `auto` hands the model every page
    // image with the OCR text as a draft to check: Thai tesseract output is
    // rarely clean enough to use as is (owner, 2026-10-21), and its
    // confidence does not say when it isn't.
    if mode != OcrMode::Auto || pages_read.is_empty() {
        return Ok(ToolResultContent::Text(format!("{summary}\n\n{text}")));
    }
    let lo = *pages_read.iter().min().unwrap_or(&ocr_first);
    let hi = *pages_read.iter().max().unwrap_or(&ocr_last);
    match page_images(validated, Some(lo), Some(hi), services).await {
        Ok((images, via)) => {
            summary.push_str(&format!(
                " Each page's image is attached (rendered by {via}). The OCR text is a DRAFT: \
                 read every page from its image, use the draft only as a hint, and correct \
                 misread words, numbers, missing vowels and tone marks, and junk lines. The \
                 image is authoritative."
            ));
            if !damaged.is_empty() {
                summary.push_str(&format!(
                    " The draft is visibly damaged on page(s) {}.",
                    damaged.join(", ")
                ));
            }
            let mut blocks = vec![ToolResultBlock::Text {
                text: format!("{summary}\n\n{text}"),
            }];
            blocks.extend(image_blocks(images).0);
            Ok(ToolResultContent::Blocks(blocks))
        }
        Err(e) => {
            summary.push_str(&format!(
                " The pages could not be rendered as images ({e}), so only the OCR text \
                 follows; it may contain errors."
            ));
            if !damaged.is_empty() {
                summary.push_str(&format!(
                    " It is visibly damaged on page(s) {}.",
                    damaged.join(", ")
                ));
            }
            Ok(ToolResultContent::Text(format!("{summary}\n\n{text}")))
        }
    }
}

/// The vision path when it can run; otherwise the text layer with a note
/// naming what would make the scan readable.
async fn vision_or_explain(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
    services: &Services,
    layer: &str,
    why: &str,
) -> Result<ToolResultContent> {
    if !services.local_render && services.pages.is_none() {
        return Ok(ToolResultContent::Text(format!(
            "{}\n\n[{why}this PDF's text layer is empty or unreliable — it is probably a scan. \
             Reading it needs OCR or page images, and neither is available here: \
             {POPPLER_INSTALL} to render pages locally, or sign in to the thClaws Gateway \
             so the document service can OCR them.]",
            normalize_thai_spacing(layer)
        )));
    }
    vision_blocks(validated, first, last, services, why)
        .await
        .map(ToolResultContent::Blocks)
}

/// Every requested page as an image block, plus a summary text block.
async fn vision_blocks(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
    services: &Services,
    why: &str,
) -> Result<Vec<ToolResultBlock>> {
    let (_, _, truncated) = capped_range(first, last);
    let (pages, via) = page_images(validated, first, last, services).await?;
    let (mut blocks, total_bytes) = image_blocks(pages);
    let mut summary = format!(
        "{why}PDF read by vision: rendered {} page(s) at {} DPI via {via} — total {} KB \
         before base64.",
        blocks
            .iter()
            .filter(|b| matches!(b, ToolResultBlock::Image { .. }))
            .count(),
        fallback::RENDER_DPI,
        (total_bytes + 512) / 1024
    );
    if let Some((requested, capped)) = truncated {
        summary.push_str(&format!(
            " Truncated: requested {} pages but only the first {} were rendered \
             (cap: MAX_PAGES_TO_RENDER). Re-invoke with a narrower `pages` range \
             to see the rest.",
            requested, capped
        ));
    }
    blocks.push(ToolResultBlock::Text { text: summary });
    Ok(blocks)
}

/// PNGs for the (capped) range with their page numbers, and what rendered
/// them: local `pdftoppm`, else the document service's `/pdf/pages`.
async fn page_images(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
    services: &Services,
) -> Result<(Vec<(u32, Vec<u8>)>, String)> {
    let (render_first, render_last, _) = capped_range(first, last);
    if !services.local_render {
        if let Some(ep) = services.pages.as_ref() {
            let got = doc_render::pdf_pages(
                ep,
                validated,
                Some(render_first),
                Some(render_last),
                fallback::RENDER_DPI,
            )
            .await?;
            let pages = got
                .pages
                .into_iter()
                .map(|p| {
                    base64::engine::general_purpose::STANDARD
                        .decode(p.png_base64.as_bytes())
                        .map(|b| (p.n, b))
                        .map_err(|e| Error::Tool(format!("page {} image: {e}", p.n)))
                })
                .collect::<Result<Vec<_>>>()?;
            return Ok((pages, doc_render::describe(ep)));
        }
    }
    let pages = render_local(validated, render_first, render_last).await?;
    Ok((pages, "pdftoppm".into()))
}

fn image_blocks(pages: Vec<(u32, Vec<u8>)>) -> (Vec<ToolResultBlock>, usize) {
    let mut blocks = Vec::with_capacity(pages.len());
    let mut total_bytes = 0usize;
    for (n, bytes) in pages {
        if bytes.len() > fallback::MAX_IMAGE_BYTES {
            blocks.push(ToolResultBlock::Text {
                text: format!(
                    "(page {n} skipped — rendered PNG is {} bytes, over the {}-byte cap; \
                     try a narrower `pages` range or downscale via the source PDF.)",
                    bytes.len(),
                    fallback::MAX_IMAGE_BYTES
                ),
            });
            continue;
        }
        total_bytes += bytes.len();
        blocks.push(ToolResultBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            },
        });
    }
    (blocks, total_bytes)
}

/// Render `render_first..=render_last` to PNG with `pdftoppm`.
async fn render_local(
    validated: &std::path::Path,
    render_first: u32,
    render_last: u32,
) -> Result<Vec<(u32, Vec<u8>)>> {
    let tmp = tempfile::tempdir().map_err(|e| Error::Tool(format!("tempdir: {e}")))?;
    let prefix = tmp.path().join("page");
    let prefix_str = prefix.to_string_lossy().into_owned();

    let mut cmd = Command::new("pdftoppm");
    cmd.arg("-png")
        .arg("-r")
        .arg(fallback::RENDER_DPI.to_string())
        .arg("-f")
        .arg(render_first.to_string())
        .arg("-l")
        .arg(render_last.to_string())
        .arg(validated.as_os_str())
        .arg(&prefix_str);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Tool(format!(
                "pdftoppm not found — {POPPLER_INSTALL} for the vision path. Or sign in \
                 to the thClaws Gateway so the document service can render the pages."
            ))
        } else {
            Error::Tool(format!("spawn pdftoppm: {e}"))
        }
    })?;

    let mut stderr = child.stderr.take().unwrap();
    let mut err_buf = Vec::new();

    let run = async {
        stderr
            .read_to_end(&mut err_buf)
            .await
            .map_err(|e| Error::Tool(format!("read stderr: {e}")))?;
        let status = child
            .wait()
            .await
            .map_err(|e| Error::Tool(format!("wait pdftoppm: {e}")))?;
        Ok::<_, Error>(status)
    };

    let status = match timeout(fallback::RENDER_TIMEOUT, run).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(Error::Tool(format!(
                "pdftoppm timed out after {}s",
                fallback::RENDER_TIMEOUT.as_secs()
            )));
        }
    };
    if !status.success() {
        let stderr_str = String::from_utf8_lossy(&err_buf);
        return Err(Error::Tool(format!(
            "pdftoppm failed (exit {}): {}",
            status.code().unwrap_or(-1),
            stderr_str.trim()
        )));
    }

    // pdftoppm's filename pattern is `<prefix>-<N>.png` where N is
    // 1-indexed and zero-padded to the digits needed for the LAST
    // page. e.g. 100 pages → `page-001.png`; 9 pages → `page-1.png`.
    // Walking the directory and sorting by name gives us pages in
    // the right order regardless of the padding width.
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(tmp.path())
        .map_err(|e| Error::Tool(format!("read tmp dir: {e}")))?
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("png"))
        .collect();
    entries.sort();
    entries
        .iter()
        .enumerate()
        .map(|(idx, path)| {
            std::fs::read(path)
                .map(|b| (render_first + idx as u32, b))
                .map_err(|e| Error::Tool(format!("read png: {e}")))
        })
        .collect()
}

/// Parse a `pages` string into (first, last) page numbers (1-indexed,
/// inclusive). `None` for either side means "no bound". Examples:
/// - `"all"` → (None, None)
/// - `"3"` → (Some(3), Some(3))
/// - `"1-5"` → (Some(1), Some(5))
fn parse_page_range(spec: &str) -> Result<(Option<u32>, Option<u32>)> {
    let s = spec.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("all") {
        return Ok((None, None));
    }
    if let Some((a, b)) = s.split_once('-') {
        let first: u32 = a
            .trim()
            .parse()
            .map_err(|_| Error::Tool(format!("invalid page range start: {a:?}")))?;
        let last: u32 = b
            .trim()
            .parse()
            .map_err(|_| Error::Tool(format!("invalid page range end: {b:?}")))?;
        if first == 0 || last < first {
            return Err(Error::Tool(format!(
                "invalid page range: {first}-{last} (pages are 1-indexed; end must be >= start)"
            )));
        }
        return Ok((Some(first), Some(last)));
    }
    let n: u32 = s
        .parse()
        .map_err(|_| Error::Tool(format!("invalid page spec: {spec:?}")))?;
    if n == 0 {
        return Err(Error::Tool("page numbers are 1-indexed, got 0".into()));
    }
    Ok((Some(n), Some(n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_page_range_all() {
        assert_eq!(parse_page_range("all").unwrap(), (None, None));
        assert_eq!(parse_page_range("ALL").unwrap(), (None, None));
        assert_eq!(parse_page_range("").unwrap(), (None, None));
    }

    #[test]
    fn parse_page_range_single() {
        assert_eq!(parse_page_range("3").unwrap(), (Some(3), Some(3)));
    }

    #[test]
    fn parse_page_range_span() {
        assert_eq!(parse_page_range("1-5").unwrap(), (Some(1), Some(5)));
        assert_eq!(parse_page_range(" 2 - 7 ").unwrap(), (Some(2), Some(7)));
    }

    #[test]
    fn parse_page_range_rejects_bad_input() {
        assert!(parse_page_range("0").is_err());
        assert!(parse_page_range("abc").is_err());
        assert!(parse_page_range("5-3").is_err());
        assert!(parse_page_range("1-").is_err());
    }

    /// The sparseness heuristic must:
    /// - Treat empty extracted text as scanned (no chars at all)
    /// - Treat a single-line title as scanned (way under the threshold)
    /// - Pass dense paragraphs through as text-PDF
    /// - Average across pages (form-feed separated)
    #[test]
    fn text_sparseness_matches_intent() {
        // Empty → scanned.
        assert!(text_is_too_sparse(""));
        assert!(text_is_too_sparse("   \n\n  "));

        // One-line title across one page → still under 50 chars
        // non-whitespace → treated as scanned. Conservative on
        // purpose; vision OCR adds little cost for a single page.
        assert!(text_is_too_sparse("Cover page"));

        // Dense single-page paragraph → not scanned.
        let dense = "A".repeat(500);
        assert!(!text_is_too_sparse(&dense));

        // Two pages, one dense + one empty → average dilutes but
        // overall still well above threshold.
        let mixed = format!("{}\u{000C}", "A".repeat(500));
        assert!(!text_is_too_sparse(&mixed));

        // Five pages of title-only content → very sparse, should
        // trip even though chars-per-page math hits exactly the
        // boundary. 5 pages × 10 chars = 50 chars total, divided
        // by 5 pages = 10 chars/page average → below the 50
        // threshold.
        let titles = (0..5)
            .map(|_| "Cover page")
            .collect::<Vec<_>>()
            .join("\u{000C}");
        assert!(text_is_too_sparse(&titles));
    }

    fn pdftotext_available() -> bool {
        std::process::Command::new("pdftotext")
            .arg("-v")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// End-to-end: PdfCreateTool writes a Thai+Latin PDF to a tempfile,
    /// PdfReadTool extracts it via pdftotext, and we assert that both
    /// scripts survive the round-trip. Skipped if poppler-utils isn't
    /// installed (CI macOS runners need `brew install poppler` in the
    /// workflow setup; ubuntu uses `apt install poppler-utils`).
    #[tokio::test]
    async fn round_trips_thai_latin_via_pdftotext() {
        if !pdftotext_available() {
            eprintln!("skipping: pdftotext not in PATH");
            return;
        }
        use crate::tools::PdfCreateTool;
        use serde_json::json;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let pdf = dir.path().join("rt.pdf");
        let _ = PdfCreateTool
            .call(json!({
                "path": pdf.to_string_lossy(),
                "content": "# Hello สวัสดี\n\nMixed paragraph with English and ภาษาไทย together."
            }))
            .await
            .unwrap();

        let extracted = PdfReadTool
            .call(json!({"path": pdf.to_string_lossy()}))
            .await
            .unwrap();

        assert!(
            extracted.contains("Hello"),
            "Latin should survive round-trip, got: {extracted:?}"
        );
        assert!(
            extracted
                .chars()
                .any(|c| matches!(c, '\u{0E00}'..='\u{0E7F}')),
            "Thai should survive round-trip, got: {extracted:?}"
        );
    }

    /// Multimodal entry: a normal text PDF returns Text, not Blocks.
    /// Regression guard so the fallback doesn't accidentally fire on
    /// every invocation.
    #[tokio::test]
    async fn call_multimodal_returns_text_for_text_pdf() {
        if !pdftotext_available() {
            eprintln!("skipping: pdftotext not in PATH");
            return;
        }
        use crate::tools::PdfCreateTool;
        use serde_json::json;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let pdf = dir.path().join("text.pdf");
        // Long enough body that the per-page average comfortably
        // clears MIN_CHARS_PER_PAGE.
        let body = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, \
                    sed do eiusmod tempor incididunt ut labore et dolore magna \
                    aliqua. Ut enim ad minim veniam, quis nostrud exercitation \
                    ullamco laboris nisi ut aliquip ex ea commodo consequat.";
        PdfCreateTool
            .call(json!({"path": pdf.to_string_lossy(), "content": format!("# Doc\n\n{body}")}))
            .await
            .unwrap();

        let result = PdfReadTool
            .call_multimodal(json!({"path": pdf.to_string_lossy()}))
            .await
            .unwrap();
        match result {
            ToolResultContent::Text(t) => {
                assert!(t.contains("Lorem"), "expected text body, got: {t:?}");
            }
            ToolResultContent::Blocks(_) => {
                panic!("text PDF should not trigger image fallback")
            }
        }
    }

    #[test]
    fn normalize_thai_reattaches_orphaned_marks() {
        // pdftotext -layout orphans combining marks behind spaces;
        // re-attaching them is unambiguous (a mark can't start a word).
        assert_eq!(normalize_thai_spacing("ผู ้"), "ผู้"); // orphaned tone over vowel
        assert_eq!(normalize_thai_spacing("ษ ัท"), "ษัท"); // orphaned mai-han-akat
        assert_eq!(normalize_thai_spacing("ก ำหนด"), "กำหนด"); // orphaned sara-am
        assert_eq!(normalize_thai_spacing("ปฏิบ ัติ"), "ปฏิบัติ");
        assert_eq!(normalize_thai_spacing("ษ ั ้"), "ษั้"); // stacked marks, two passes
    }

    #[test]
    fn normalize_thai_never_merges_real_words() {
        // Non-Thai untouched.
        assert_eq!(normalize_thai_spacing("Hello world"), "Hello world");
        // A real phrase space — the next word starts with a consonant, not
        // an orphaned mark — must survive, even though "เวลา" ends in a
        // vowel. This is the case a "mark + space + consonant" rule would
        // wrongly merge.
        assert_eq!(normalize_thai_spacing("เวลา ทำงาน"), "เวลา ทำงาน");
        assert_eq!(normalize_thai_spacing("คน รถ"), "คน รถ");
    }

    #[test]
    fn repairs_known_saraam_cmap_corruption() {
        // ำ→า corruption in non-word forms is repaired, even glued mid-run
        // (substring) and after the space-join collapses a fragment.
        assert_eq!(normalize_thai_spacing("การทางานของบริษัท"), "การทำงานของบริษัท");
        assert_eq!(normalize_thai_spacing("กาหนดเวลา"), "กำหนดเวลา");
        assert_eq!(normalize_thai_spacing("จานวนวันลา"), "จำนวนวันลา");
        assert_eq!(normalize_thai_spacing("ก าหนด"), "กำหนด"); // space-join then repair
                                                               // Real า words must NEVER be touched (never in the table), incl. ones
                                                               // that share a prefix with a corrupted form (ประจาน vs ประจำ).
        for w in ["ด้าน", "หน้าที่", "ภาพ", "เป้าหมาย", "ทางการ", "ประจาน", "ราคา"]
        {
            assert_eq!(
                normalize_thai_spacing(w),
                w,
                "must not corrupt real word {w}"
            );
        }
    }

    #[test]
    fn garbled_detector_flags_fragmented_thai_not_clean() {
        // Clean Thai prose: no marks orphaned behind spaces → trustworthy.
        let clean = "พนักงานทุกคนมีสิทธิได้รับค่าจ้างตามที่กฎหมายกำหนดไว้อย่างเป็นธรรมเสมอ";
        assert!(!thai_looks_garbled(clean));

        // Heavily fragmented Thai (≥6 marks orphaned behind spaces) → the
        // text layer is untrustworthy, so route to the vision path.
        let garbled = "บริ ษ ัท ผู ้ ปฏิบ ัติ หน้ า ค่ าจ้าง ก ำหนด ท ำงาน จ ำเป็น \
                       สิ ทธิ พนักงานทุกคนในองค์กร";
        assert!(thai_looks_garbled(garbled));
    }

    /// The numbers behind SHRED_PER_1K_RETRY, taken from the survey corpus
    /// (docs/pdf-thai-extraction-modes.md): the worst document reads 36.3
    /// breaks per 1k Thai characters under `-layout`, budget tables read
    /// 0.0-0.8, and Samkok reads 2.3-2.6 with nothing to gain from `-raw`.
    #[test]
    fn shredded_thai_is_told_from_merely_spaced_thai() {
        // Real `-layout` output: the spaces fall inside the words.
        let shredded = "บริ ษทั ฯ จาแนกประเภทของพนักงานไว้ดงั นี้ พนักงานที่บริ ษทั ฯ                         ตกลงจ้างโดยกำหนดค่าจ้างเป็ นรายเดือน ปฏิบตัิงานเป็ นระยะเวลา                         โดยผูบ้ งั คับบัญชาจะประเมินการทดลองงานจาก ผลการปฏิบตัิงาน                         หากผลการประเมินไม่ผา่ นตามมาตรฐาน บริ ษทั ฯ จะเลิ กจ้าง"
            .repeat(2);
        assert!(shred_per_1k(&shredded) > fallback::SHRED_PER_1K_RETRY);
        assert!(
            thai_is_shredded(&shredded),
            "this is the document that needs -raw"
        );

        // The same prose intact: Thai spaces between phrases, never inside a
        // word. Must NOT trigger a second extraction.
        let clean = "พนักงานทุกคนมีสิทธิได้รับค่าจ้างตามที่กฎหมายกำหนดไว้อย่างเป็นธรรม                      บริษัทจำแนกประเภทของพนักงานไว้ดังนี้ พนักงานรายเดือนและพนักงานรายวัน                      ผู้บังคับบัญชาจะประเมินผลการปฏิบัติงานตามมาตรฐานที่บริษัทกำหนด"
            .repeat(2);
        assert!(
            shred_per_1k(&clean) <= fallback::SHRED_PER_1K_RETRY,
            "{}",
            shred_per_1k(&clean)
        );
        assert!(!thai_is_shredded(&clean));

        // Too little Thai to judge — never spend a second extraction on it.
        assert!(!thai_is_shredded("บริ ษทั ฯ"));
    }

    /// A text layer with no words in it reaches the vision path. The orphan-
    /// mark check cannot see this case: the characters are isolated, so no
    /// mark ever sits behind a space.
    #[test]
    fn a_text_layer_of_isolated_characters_is_garbled() {
        let isolated: String = "ข้อบังคับเกี่ยวกับการทำงานของพนักงานบริษัท"
            .chars()
            .map(|c| format!("{c} "))
            .collect::<Vec<_>>()
            .join("")
            .repeat(3);
        assert!(mean_thai_run(&isolated) < fallback::MIN_THAI_RUN);
        assert!(
            thai_looks_garbled(&isolated),
            "isolated characters must route to vision"
        );

        // Ordinary Thai prose runs long and stays on the text path.
        let prose = "พนักงานทุกคนมีสิทธิได้รับค่าจ้างตามที่กฎหมายกำหนดไว้อย่างเป็นธรรมเสมอ".repeat(2);
        assert!(mean_thai_run(&prose) > 10.0, "{}", mean_thai_run(&prose));
        assert!(!thai_looks_garbled(&prose));
    }

    /// dev-plan/66: the no-poppler fallback, end to end against a stand-in
    /// for `thclaws-cloud/pdf-text/` — multipart out, page range carried,
    /// text back, and a service error surfaced as the service worded it
    /// rather than as a status code.
    #[tokio::test]
    async fn cloud_fallback_posts_the_file_and_returns_its_text() {
        use axum::extract::Multipart;
        use axum::routing::post;

        async fn extract(mut mp: Multipart) -> (axum::http::StatusCode, String) {
            let (mut name, mut bytes, mut first) = (String::new(), 0usize, String::new());
            while let Ok(Some(field)) = mp.next_field().await {
                match field.name().unwrap_or("").to_string().as_str() {
                    "file" => {
                        name = field.file_name().unwrap_or("").to_string();
                        bytes = field.bytes().await.map(|b| b.len()).unwrap_or(0);
                    }
                    "first" => first = field.text().await.unwrap_or_default(),
                    _ => {}
                }
            }
            if name.ends_with(".bad") {
                return (
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    r#"{"error":"not a readable PDF"}"#.to_string(),
                );
            }
            (
                axum::http::StatusCode::OK,
                format!("ได้รับ {name} ({bytes} bytes) first={first}"),
            )
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = axum::Router::new().route("/extract", post(extract));
            let _ = axum::serve(listener, app).await;
        });
        let url = format!("http://{addr}/extract");

        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("สัญญา.pdf");
        std::fs::write(&pdf, vec![b'x'; 1234]).unwrap();
        let got = extract_via_cloud(&url, &pdf, Some(2), None).await.unwrap();
        assert!(got.contains("สัญญา.pdf"), "filename carried: {got}");
        assert!(got.contains("1234 bytes"), "body carried: {got}");
        assert!(got.contains("first=2"), "page range carried: {got}");

        // A refusal must reach the user as the service's own sentence.
        let bad = dir.path().join("x.bad");
        std::fs::write(&bad, b"nope").unwrap();
        let err = extract_via_cloud(&url, &bad, None, None)
            .await
            .expect_err("422 must be an error");
        assert!(
            err.to_string().contains("not a readable PDF"),
            "service wording must survive: {err}"
        );
    }

    /// The switch has to actually switch: with it off there is no endpoint,
    /// so `extract_text_raw` reports what to install instead of uploading.
    #[test]
    fn cloud_fallback_can_be_switched_off() {
        // Cargo runs lib tests in parallel and these are process-global.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("THCLAWS_PDF_CLOUD", "0");
        assert!(
            cloud_endpoint().is_none(),
            "THCLAWS_PDF_CLOUD=0 disables it"
        );
        std::env::set_var("THCLAWS_PDF_CLOUD", "1");
        std::env::set_var("THCLAWS_PDF_TEXT_API", "https://example.invalid/x");
        assert_eq!(
            cloud_endpoint().as_deref(),
            Some("https://example.invalid/x"),
            "the endpoint is overridable for a self-hosted copy"
        );
        std::env::remove_var("THCLAWS_PDF_TEXT_API");
        std::env::remove_var("THCLAWS_PDF_CLOUD");
    }

    /// With the fallback off, a machine without poppler must be told how to
    /// install it — on every platform, including the one the old message left
    /// out entirely.
    #[tokio::test]
    async fn no_poppler_and_no_fallback_names_every_install_route() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("THCLAWS_PDF_CLOUD", "0");
        let err = without_poppler(
            std::path::Path::new("/tmp/none.pdf"),
            None,
            None,
            CloudFallback::Allowed,
        )
        .await
        .expect_err("no local binary and no fallback is an error");
        std::env::remove_var("THCLAWS_PDF_CLOUD");
        let msg = err.to_string();
        for hint in [
            "brew install poppler",
            "apt install poppler-utils",
            "winget",
            "scoop",
        ] {
            assert!(msg.contains(hint), "missing {hint} in: {msg}");
        }
    }

    #[test]
    fn ocr_mode_parses_and_vision_flag_is_an_alias() {
        assert_eq!(ocr_mode(&json!({})).unwrap(), OcrMode::Auto);
        assert_eq!(
            ocr_mode(&json!({"ocr": "Tesseract"})).unwrap(),
            OcrMode::Tesseract
        );
        assert_eq!(
            ocr_mode(&json!({"ocr": "vision"})).unwrap(),
            OcrMode::Vision
        );
        assert_eq!(ocr_mode(&json!({"vision": true})).unwrap(), OcrMode::Vision);
        assert!(ocr_mode(&json!({"ocr": "magic"})).is_err());
    }

    fn services(base: &str, ocr: bool) -> Services {
        use crate::tools::doc_render::tests::ep;
        Services {
            ocr: ocr.then(|| ep(base, "ocr")),
            pages: Some(ep(base, "pdf/pages")),
            local_render: false,
        }
    }

    /// dev-plan/69 `auto`: OCR, then every page image goes to the model with
    /// the OCR text as a draft to correct; the unsure page (41) is flagged.
    #[tokio::test]
    async fn auto_ocr_attaches_every_page_with_the_ocr_draft() {
        let (base, seen) = crate::tools::doc_render::tests::mock_doc_render().await;
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("scan.pdf");
        std::fs::write(&pdf, b"%PDF-1.4 scan").unwrap();

        let got = read_scanned(&pdf, None, None, OcrMode::Auto, &services(&base, true), "")
            .await
            .unwrap();
        let ToolResultContent::Blocks(blocks) = got else {
            panic!("auto must attach page images");
        };
        let images = blocks
            .iter()
            .filter(|b| matches!(b, ToolResultBlock::Image { .. }))
            .count();
        let ToolResultBlock::Text { text } = &blocks[0] else {
            panic!("summary first");
        };
        assert!(
            text.contains("[page 1 — OCR draft, confidence 92]"),
            "{text}"
        );
        assert!(
            text.contains("[page 2 — OCR draft, unreliable, confidence 41]"),
            "{text}"
        );
        assert!(text.contains("DRAFT"), "{text}");
        assert!(text.contains("บันทึกข้อความเรื่องขออนุมัติ"), "{text}");

        let seen = seen.lock().unwrap();
        let pages_call = seen.iter().find(|(p, _)| p == "/pdf/pages").unwrap();
        let first = pages_call.1["first"].as_str().parse::<usize>().unwrap();
        let last = pages_call.1["last"].as_str().parse::<usize>().unwrap();
        assert_eq!(first, 1, "every OCR'd page is rendered");
        assert_eq!(images, last - first + 1, "one image per page");
    }

    #[tokio::test]
    async fn tesseract_mode_returns_every_page_as_text() {
        let (base, _) = crate::tools::doc_render::tests::mock_doc_render().await;
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("scan.pdf");
        std::fs::write(&pdf, b"%PDF-1.4 scan").unwrap();
        let got = read_scanned(
            &pdf,
            Some(1),
            Some(3),
            OcrMode::Tesseract,
            &services(&base, true),
            "",
        )
        .await
        .unwrap();
        let ToolResultContent::Text(text) = got else {
            panic!("tesseract mode is text only");
        };
        assert!(
            text.contains("[page 2 — OCR, confidence 41]\nลายมือ"),
            "{text}"
        );
        assert!(text.contains("งบ ประมาณ 6,000,000 บาท"), "{text}");
    }

    /// No OCR service (open-core without consent): the vision path, as before.
    #[tokio::test]
    async fn without_an_ocr_service_auto_falls_back_to_vision() {
        let (base, _) = crate::tools::doc_render::tests::mock_doc_render().await;
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("scan.pdf");
        std::fs::write(&pdf, b"%PDF-1.4 scan").unwrap();
        let got = read_scanned(
            &pdf,
            Some(1),
            Some(2),
            OcrMode::Auto,
            &services(&base, false),
            "",
        )
        .await
        .unwrap();
        let ToolResultContent::Blocks(blocks) = got else {
            panic!("vision path returns blocks");
        };
        assert_eq!(
            blocks
                .iter()
                .filter(|b| matches!(b, ToolResultBlock::Image { .. }))
                .count(),
            2
        );

        let nothing = Services {
            ocr: None,
            pages: None,
            local_render: false,
        };
        let got = read_scanned(&pdf, None, None, OcrMode::Auto, &nothing, "x")
            .await
            .unwrap();
        let ToolResultContent::Text(text) = got else {
            panic!("explained as text")
        };
        assert!(
            text.contains("winget install oschwartz10612.Poppler"),
            "{text}"
        );
        let err = read_scanned(&pdf, None, None, OcrMode::Tesseract, &nothing, "")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("document service"), "{err}");
    }

    #[tokio::test]
    async fn document_service_text_is_paged_like_pdftotext() {
        let (base, _) = crate::tools::doc_render::tests::mock_doc_render().await;
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("minutes.pdf");
        std::fs::write(&pdf, b"%PDF-1.7").unwrap();
        let ep = crate::tools::doc_render::tests::ep(&base, "pdf/text");
        let text = doc_render_text(&ep, &pdf, None, None).await.unwrap();
        assert_eq!(text.matches('\u{000C}').count(), 1, "{text:?}");
        assert!(!text_is_too_sparse(&text));
        let noted = with_note(
            text,
            &Source::DocRender(crate::tools::doc_render::describe(&ep)),
        );
        assert!(
            noted.contains("extracted by the thClaws document service (127.0.0.1)")
                || noted.contains("extracted by your organisation's document service (127.0.0.1)"),
            "{noted}"
        );
        assert!(!noted.contains("pdf.thclaws.cloud"), "{noted}");
    }

    /// B6: the consent-less entry points (KMS ingest, the Files tab) upload
    /// only to an EE organisation's own service; open-core needs approval.
    #[test]
    fn consentless_callers_upload_only_on_ee() {
        assert!(
            may_upload(CloudFallback::Refused, true),
            "EE: own gateway, no prompt"
        );
        assert!(
            !may_upload(CloudFallback::Refused, false),
            "open-core: never unasked"
        );
        assert!(
            may_upload(CloudFallback::Allowed, false),
            "approved tool call"
        );
    }

    #[tokio::test]
    async fn consentless_extraction_without_poppler_points_at_pdfread() {
        if doc_render::uploads_preapproved() {
            return;
        }
        let err = without_poppler(
            std::path::Path::new("/tmp/none.pdf"),
            None,
            None,
            CloudFallback::Refused,
        )
        .await
        .expect_err("no poppler and no consent is an error");
        let msg = err.to_string();
        assert!(msg.contains("PdfRead"), "{msg}");
        assert!(msg.contains("winget"), "{msg}");
        assert!(!msg.contains("pdf.thclaws.cloud"), "{msg}");
    }
}
