//! `PdfCreate` — render markdown to a typographically sound PDF with
//! embedded Noto Sans (Regular/Bold/Italic) + Noto Sans Thai
//! (Regular/Bold) so Thai/Latin text renders without any system-font
//! dependency.
//!
//! v2 renderer (the v1 "glyph-naive width estimation" is gone):
//! - **Real glyph metrics** from the embedded fonts via `ttf-parser`
//!   — line breaks land where the text actually ends.
//! - **Thai-aware line breaking**: text is segmented into clusters
//!   (base char + combining marks) and Thai breaks avoid splitting
//!   after lead vowels (เแโใไ) or before ๆ/ฯ/ำ — Thai has no spaces,
//!   so space-only wrapping produced overflowing lines in v1.
//! - **OpenType shaping** via rustybuzz (HarfBuzz): each same-font run
//!   is shaped through `shape_run`, applying the font's GSUB (Thai
//!   mark variants for tall/descender consonants) and GPOS (stacked-
//!   mark raising — tone marks above upper vowels), so Thai combining
//!   marks land at their correct anchors instead of font-default
//!   positions. GPOS-offset glyphs are emitted individually with their
//!   shaped advances/offsets rather than via printpdf's run-level text
//!   API.
//! - **Styled runs**: headings + `**bold**` use the Bold faces,
//!   `*italic*` uses Noto Sans Italic (Thai has no italic — falls
//!   back to regular), inline `code` renders dimmed + slightly small.
//! - **Real tables**: bordered grid, bold shaded header row, measured
//!   column widths, wrapped cell text.
//! - **Book furniture**: ordered/unordered lists with hanging
//!   indents, blockquote bars, horizontal rules, fenced code blocks
//!   on a shaded background, centered images with italic captions
//!   from alt text, per-page `n / N` footers, PDF outline bookmarks
//!   for H1/H2, optional page break before each H1 (chapters).

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use printpdf::path::{PaintMode, WindingOrder};
use printpdf::{
    Color, Image, ImageTransform, IndirectFontRef, Line, Mm, PdfDocument, PdfDocumentReference,
    PdfLayerIndex, PdfPageIndex, Point, Rect, Rgb,
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde_json::{json, Value};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const LATIN_REG_BYTES: &[u8] = include_bytes!("../../resources/fonts/NotoSans-Regular.ttf");
const LATIN_BOLD_BYTES: &[u8] = include_bytes!("../../resources/fonts/NotoSans-Bold.ttf");
const LATIN_ITAL_BYTES: &[u8] = include_bytes!("../../resources/fonts/NotoSans-Italic.ttf");
const THAI_REG_BYTES: &[u8] = include_bytes!("../../resources/fonts/NotoSansThai-Regular.ttf");
const THAI_BOLD_BYTES: &[u8] = include_bytes!("../../resources/fonts/NotoSansThai-Bold.ttf");
// Serif counterparts — selected via the `font: "serif"` option.
const LATIN_REG_SERIF_BYTES: &[u8] = include_bytes!("../../resources/fonts/NotoSerif-Regular.ttf");
const LATIN_BOLD_SERIF_BYTES: &[u8] = include_bytes!("../../resources/fonts/NotoSerif-Bold.ttf");
const LATIN_ITAL_SERIF_BYTES: &[u8] = include_bytes!("../../resources/fonts/NotoSerif-Italic.ttf");
const THAI_REG_SERIF_BYTES: &[u8] =
    include_bytes!("../../resources/fonts/NotoSerifThai-Regular.ttf");
const THAI_BOLD_SERIF_BYTES: &[u8] = include_bytes!("../../resources/fonts/NotoSerifThai-Bold.ttf");

const PT_TO_MM: f32 = 0.3528;
const DEFAULT_FONT_SIZE_PT: f32 = 11.0;
const MARGIN_MM: f32 = 22.0;
const FOOTER_BAND_MM: f32 = 10.0;
const PARAGRAPH_GAP_MM: f32 = 2.6;
const BODY_LINE_FACTOR: f32 = 1.55;

pub struct PdfCreateTool;

#[async_trait]
impl Tool for PdfCreateTool {
    fn name(&self) -> &'static str {
        "PdfCreate"
    }

    fn description(&self) -> &'static str {
        "Render markdown to a typographically sound PDF. Embedded Noto Sans \
         Regular/Bold/Italic + Noto Sans Thai Regular/Bold (or set \
         `font: \"serif\"` for Noto Serif + Noto Serif Thai) — Thai text \
         wraps at proper cluster boundaries and headings/bold render in \
         real bold. Supports headings H1-H6 (H1/H2 become PDF outline \
         bookmarks), paragraphs, **bold** / *italic* / `code`, ordered + \
         unordered lists with hanging indents, blockquotes, horizontal \
         rules, fenced code blocks on a shaded background, GFM pipe \
         tables (bordered grid, shaded bold header, wrapped cells), and \
         `![caption](path)` images (PNG/JPEG ≤ 5 MB, scaled to page \
         width, centered, alt text rendered as an italic caption). Pages \
         get `n / N` footers. Pass `content` inline for small documents \
         or `content_path` to render a markdown FILE without copying it \
         through the conversation (relative image paths then resolve \
         against that file's directory — use this for book exports). Set \
         `page_break_h1: true` to start every H1 (chapter) on a fresh \
         page. Page size A4 (default), Letter, or Legal."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":      {"type": "string", "description": "Output PDF path. Parent directories are created if missing."},
                "content":   {"type": "string", "description": "Markdown content to render. Provide this OR content_path."},
                "content_path": {"type": "string", "description": "Path to a markdown file to render. Preferred for large documents (books) — the file is read directly, and relative image paths inside it resolve against its directory."},
                "title":     {"type": "string", "description": "PDF document title (metadata). Optional — defaults to the file stem."},
                "font_size": {"type": "integer", "description": "Body font size in points. Default 11.", "minimum": 6, "maximum": 72},
                "font": {"type": "string", "enum": ["sans", "serif"], "description": "Typeface family. 'sans' (default) = Noto Sans + Noto Sans Thai; 'serif' = Noto Serif + Noto Serif Thai. Both embed the fonts with correct Thai cluster shaping."},
                "page_size": {"type": "string", "enum": ["A4", "Letter", "Legal"], "description": "Default A4."},
                "page_break_h1": {"type": "boolean", "description": "Start every H1 on a new page (book chapters). Default false."},
                "outline_depth": {"type": "integer", "enum": [0, 1, 2], "description": "PDF sidebar bookmarks: 0 = none, 1 = H1 only (default — chapter list), 2 = H1+H2. The PDF outline is flat, so depth 2 gets noisy on long documents."}
            },
            "required": ["path"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_path = req_str(&input, "path")?;
        let validated = crate::sandbox::Sandbox::check_write(raw_path)?;

        let content_path = input
            .get("content_path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let (content, image_base): (String, PathBuf) = if let Some(cp) = content_path {
            let resolved = crate::sandbox::Sandbox::check(cp)?;
            let text = std::fs::read_to_string(&resolved)
                .map_err(|e| Error::Tool(format!("read {cp}: {e}")))?;
            let base = resolved
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            (text, base)
        } else {
            let inline = req_str(&input, "content")?;
            (inline.to_string(), crate::workdir::current_workdir())
        };

        let content = replace_missing_glyphs(&content);
        let title = input
            .get("title")
            .and_then(|v| v.as_str())
            .map(replace_missing_glyphs)
            .unwrap_or_else(|| {
                Path::new(raw_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Document")
                    .to_string()
            });

        let font_size = input
            .get("font_size")
            .and_then(|v| v.as_f64())
            .map(|n| n as f32)
            .unwrap_or(DEFAULT_FONT_SIZE_PT);

        let family = match input.get("font").and_then(|v| v.as_str()) {
            Some("serif") => Family::Serif,
            _ => Family::Sans,
        };

        let (page_w_mm, page_h_mm) = match input.get("page_size").and_then(|v| v.as_str()) {
            Some("Letter") => (215.9, 279.4),
            Some("Legal") => (215.9, 355.6),
            _ => (210.0, 297.0), // A4
        };

        let page_break_h1 = input
            .get("page_break_h1")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let outline_depth = input
            .get("outline_depth")
            .and_then(|v| v.as_u64())
            .map(|n| n.min(2) as u8)
            .unwrap_or(1);

        if let Some(parent) = Path::new(&*validated.to_string_lossy()).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::Tool(format!("mkdir {}: {}", parent.display(), e)))?;
            }
        }

        let path_clone = validated.clone();
        let pages = tokio::task::spawn_blocking(move || -> Result<usize> {
            render_pdf(
                &path_clone,
                &title,
                &content,
                font_size,
                page_w_mm,
                page_h_mm,
                page_break_h1,
                outline_depth,
                &image_base,
                family,
            )
        })
        .await
        .map_err(|e| Error::Tool(format!("PDF worker join failed: {e}")))??;

        Ok(format!(
            "Wrote PDF to {} ({} page{})",
            validated.display(),
            pages,
            if pages == 1 { "" } else { "s" }
        ))
    }
}

// ─── Fonts & metrics ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FontId {
    LatinReg = 0,
    LatinBold = 1,
    LatinItal = 2,
    ThaiReg = 3,
    ThaiBold = 4,
}
const FONT_COUNT: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct InlineStyle {
    bold: bool,
    italic: bool,
    code: bool,
}

#[derive(Debug, Clone)]
struct Span {
    text: String,
    style: InlineStyle,
}

/// Characters neither embedded face can draw come out as empty boxes
/// (seen in an agent's report: "7.94 ☐ 13.57" for →, "☐" for ⚠). Swap the
/// common ones for text the fonts carry; drop emoji and the rest.
fn replace_missing_glyphs(text: &str) -> String {
    let latin = ttf_parser::Face::parse(LATIN_REG_BYTES, 0).ok();
    let thai = ttf_parser::Face::parse(THAI_REG_BYTES, 0).ok();
    let has = |c: char| {
        latin.as_ref().is_some_and(|f| f.glyph_index(c).is_some())
            || thai.as_ref().is_some_and(|f| f.glyph_index(c).is_some())
    };
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_whitespace() || c.is_control() || has(c) {
            out.push(c);
            continue;
        }
        let sub = match c {
            '→' | '⟶' | '➜' | '➔' | '➡' | '▶' | '►' => "->",
            '⇒' => "=>",
            '←' | '⟵' | '⬅' | '◀' | '◄' => "<-",
            '↔' | '⟷' => "<->",
            '↑' | '⬆' | '▲' => "^",
            '↓' | '⬇' | '▼' => "v",
            '⚠' | '❗' | '❕' => "(!)",
            '✓' | '✔' | '✅' | '☑' => {
                if has('√') {
                    "√"
                } else {
                    "v"
                }
            }
            '✗' | '✘' | '❌' | '❎' => "x",
            '★' | '⭐' | '☆' => "*",
            '≥' => ">=",
            '≤' => "<=",
            '≠' => "!=",
            _ => "",
        };
        out.push_str(sub);
    }
    out
}

fn font_for(c: char, style: InlineStyle) -> FontId {
    if is_thai(c) {
        if style.bold {
            FontId::ThaiBold
        } else {
            FontId::ThaiReg
        }
    } else if style.bold {
        FontId::LatinBold
    } else if style.italic {
        FontId::LatinItal
    } else {
        FontId::LatinReg
    }
}

struct FaceSet {
    faces: [ttf_parser::Face<'static>; FONT_COUNT],
    /// Shaping faces (rustybuzz vendors its own ttf-parser, so these
    /// are parsed separately from `faces`).
    hb: [rustybuzz::Face<'static>; FONT_COUNT],
}

const SANS_BYTES: [&[u8]; FONT_COUNT] = [
    LATIN_REG_BYTES,
    LATIN_BOLD_BYTES,
    LATIN_ITAL_BYTES,
    THAI_REG_BYTES,
    THAI_BOLD_BYTES,
];
const SERIF_BYTES: [&[u8]; FONT_COUNT] = [
    LATIN_REG_SERIF_BYTES,
    LATIN_BOLD_SERIF_BYTES,
    LATIN_ITAL_SERIF_BYTES,
    THAI_REG_SERIF_BYTES,
    THAI_BOLD_SERIF_BYTES,
];

/// Typeface family for one render. Selected per `PdfCreate` call via the
/// `font` option. Ambient through a thread-local rather than threaded
/// through every shape/measure call site: `render_pdf` runs synchronously
/// on one blocking thread and sets it once at the top, so the value is
/// stable for the whole render. Defaults to `Sans` so direct `shape_run`
/// callers (tests) keep the original behavior.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Sans,
    Serif,
}

thread_local! {
    static RENDER_FAMILY: std::cell::Cell<Family> = const { std::cell::Cell::new(Family::Sans) };
}

fn family_bytes(family: Family) -> &'static [&'static [u8]; FONT_COUNT] {
    match family {
        Family::Sans => &SANS_BYTES,
        Family::Serif => &SERIF_BYTES,
    }
}

fn build_faces(bytes: &'static [&'static [u8]; FONT_COUNT]) -> Option<FaceSet> {
    Some(FaceSet {
        faces: [
            ttf_parser::Face::parse(bytes[0], 0).ok()?,
            ttf_parser::Face::parse(bytes[1], 0).ok()?,
            ttf_parser::Face::parse(bytes[2], 0).ok()?,
            ttf_parser::Face::parse(bytes[3], 0).ok()?,
            ttf_parser::Face::parse(bytes[4], 0).ok()?,
        ],
        hb: [
            rustybuzz::Face::from_slice(bytes[0], 0)?,
            rustybuzz::Face::from_slice(bytes[1], 0)?,
            rustybuzz::Face::from_slice(bytes[2], 0)?,
            rustybuzz::Face::from_slice(bytes[3], 0)?,
            rustybuzz::Face::from_slice(bytes[4], 0)?,
        ],
    })
}

fn faces() -> Option<&'static FaceSet> {
    // Each family's FaceSet is parsed once and cached for the process.
    match RENDER_FAMILY.with(|c| c.get()) {
        Family::Sans => {
            static F: OnceLock<Option<FaceSet>> = OnceLock::new();
            F.get_or_init(|| build_faces(&SANS_BYTES)).as_ref()
        }
        Family::Serif => {
            static F: OnceLock<Option<FaceSet>> = OnceLock::new();
            F.get_or_init(|| build_faces(&SERIF_BYTES)).as_ref()
        }
    }
}

/// One glyph out of the shaper, in mm at the target size.
struct ShapedGlyph {
    gid: u16,
    x_advance: f32,
    x_offset: f32,
    y_offset: f32,
}

/// Shape a same-font run with HarfBuzz (rustybuzz): applies the
/// font's GSUB (Thai mark variants for tall/descender consonants)
/// and GPOS (stacked-mark raising — tone marks above upper vowels).
/// Returns positioned glyphs + the run's shaped advance width.
fn shape_run(text: &str, font: FontId, pt: f32) -> Option<(Vec<ShapedGlyph>, f32)> {
    let fs = faces()?;
    let face = &fs.hb[font as usize];
    let upem = face.units_per_em() as f32;
    let scale = pt * PT_TO_MM / upem;

    let mut buf = rustybuzz::UnicodeBuffer::new();
    buf.push_str(text);
    buf.set_direction(rustybuzz::Direction::LeftToRight);
    let shaped = rustybuzz::shape(face, &[], buf);
    let infos = shaped.glyph_infos();
    let positions = shaped.glyph_positions();

    let mut out = Vec::with_capacity(infos.len());
    let mut width = 0.0_f32;
    for (info, pos) in infos.iter().zip(positions.iter()) {
        let g = ShapedGlyph {
            gid: info.glyph_id as u16,
            x_advance: pos.x_advance as f32 * scale,
            x_offset: pos.x_offset as f32 * scale,
            y_offset: pos.y_offset as f32 * scale,
        };
        width += g.x_advance;
        out.push(g);
    }
    Some((out, width))
}

/// Exact horizontal advance of one char in mm at the given size, from
/// the embedded font's hmtx table. Combining marks report their true
/// (usually zero) advance. Falls back to the v1 width factors when a
/// glyph is missing from every relevant face — keeps layout sane for
/// emoji etc.
fn char_advance_mm(c: char, font: FontId, pt: f32) -> f32 {
    if let Some(fs) = faces() {
        let face = &fs.faces[font as usize];
        if let Some(gid) = face.glyph_index(c) {
            if let Some(adv) = face.glyph_hor_advance(gid) {
                return adv as f32 / face.units_per_em() as f32 * pt * PT_TO_MM;
            }
        }
        // Char missing from the styled face (e.g. Thai char measured
        // against a Latin face): measure in the face that actually
        // carries the script.
        let alt = if is_thai(c) {
            FontId::ThaiReg
        } else {
            FontId::LatinReg
        };
        if alt != font {
            let face = &fs.faces[alt as usize];
            if let Some(gid) = face.glyph_index(c) {
                if let Some(adv) = face.glyph_hor_advance(gid) {
                    return adv as f32 / face.units_per_em() as f32 * pt * PT_TO_MM;
                }
            }
        }
    }
    let factor = if is_thai_combining_mark(c) {
        0.0
    } else if is_thai(c) {
        0.6
    } else if c == ' ' {
        0.28
    } else {
        0.5
    };
    pt * factor * PT_TO_MM
}

fn is_thai(c: char) -> bool {
    matches!(c, '\u{0E00}'..='\u{0E7F}')
}

/// Thai combining marks (vowels above/below, tone marks, thanthakhat).
fn is_thai_combining_mark(c: char) -> bool {
    matches!(c,
        '\u{0E31}' |
        '\u{0E34}'..='\u{0E3A}' |
        '\u{0E47}'..='\u{0E4E}'
    )
}

/// Thai lead vowels render BEFORE their consonant — never break after.
fn is_thai_lead_vowel(c: char) -> bool {
    matches!(c, '\u{0E40}'..='\u{0E44}')
}

/// Never start a line with these: ำ (sara am), ๆ (maiyamok),
/// ฯ (paiyannoi), closing punctuation.
fn no_break_before(c: char) -> bool {
    matches!(
        c,
        '\u{0E33}'
            | '\u{0E46}'
            | '\u{0E2F}'
            | ')'
            | ']'
            | '}'
            | '!'
            | '?'
            | ','
            | '.'
            | ':'
            | ';'
            | '%'
            | '”'
            | '’'
            | '»'
            | '…'
    )
}

/// Never end a line with an opening bracket/quote.
fn no_break_after_open(c: char) -> bool {
    matches!(c, '(' | '[' | '{' | '“' | '‘' | '«')
}

// ─── Clusters & line breaking ─────────────────────────────────────────

#[derive(Debug, Clone)]
struct Cluster {
    text: String,
    style: InlineStyle,
    font: FontId,
    width_mm: f32,
    is_space: bool,
    /// Base (first) char — break decisions look at this.
    base: char,
    /// Byte offset of this cluster's base char in the concatenated
    /// span text — used to map ICU line-break opportunities (byte
    /// indices) onto clusters.
    byte_start: usize,
}

/// Segment styled spans into clusters: one base char plus any Thai
/// combining marks that follow it. A combining mark can never be
/// separated from its base by a line break.
fn clusterize(spans: &[Span], pt: f32) -> Vec<Cluster> {
    let mut out: Vec<Cluster> = Vec::new();
    let mut byte_pos = 0usize;
    for span in spans {
        let eff_pt = if span.style.code { pt * 0.92 } else { pt };
        for raw in span.text.chars() {
            let c = if raw == '\n' || raw == '\t' { ' ' } else { raw };
            let start = byte_pos;
            byte_pos += c.len_utf8();
            if is_thai_combining_mark(c) {
                if let Some(last) = out.last_mut() {
                    if last.style == span.style && !last.is_space {
                        last.width_mm += char_advance_mm(c, last.font, eff_pt);
                        last.text.push(c);
                        continue;
                    }
                }
            }
            let font = font_for(c, span.style);
            out.push(Cluster {
                width_mm: char_advance_mm(c, font, eff_pt),
                text: c.to_string(),
                style: span.style,
                font,
                is_space: c == ' ',
                base: c,
                byte_start: start,
            });
        }
    }
    out
}

/// Byte positions where UAX#14 (with Thai LSTM word segmentation)
/// allows a line break in `text`. The segmenter is expensive to
/// construct — build once per process.
fn line_break_opportunities(text: &str) -> Vec<usize> {
    thread_local! {
        // LineSegmenter holds Rc internally (not Send) — one per thread.
        static SEG: icu_segmenter::LineSegmenter =
            icu_segmenter::LineSegmenter::new_auto();
    }
    SEG.with(|seg| seg.segment_str(text).collect())
}

/// May a line break fall between `prev` and `next`?
fn can_break_between(prev: &Cluster, next: &Cluster) -> bool {
    if prev.is_space {
        return !no_break_before(next.base);
    }
    if next.is_space {
        return false; // break after the space instead
    }
    if no_break_before(next.base) {
        return false;
    }
    if no_break_after_open(prev.base) {
        return false;
    }
    let p_thai = is_thai(prev.base);
    let n_thai = is_thai(next.base);
    if p_thai && n_thai {
        // Thai has no spaces: break between clusters, except after a
        // lead vowel (it belongs to the consonant that follows).
        return !is_thai_lead_vowel(prev.base);
    }
    if p_thai != n_thai {
        // Script boundary (Thai↔Latin) is a legal break point.
        return !is_thai_lead_vowel(prev.base);
    }
    // Latin↔Latin: only after hyphens (spaces handled above).
    prev.base == '-'
}

/// Greedy line breaker over clusters. Primary break points come from
/// ICU's UAX#14 line segmenter (`break_set` of byte offsets), which
/// segments Thai at WORD boundaries via its LSTM model — breaking
/// between arbitrary Thai syllable clusters reads as wrong as
/// breaking mid-word in English. When a single segmented word is
/// longer than the line (URLs, very long compounds), the cluster-
/// level Thai rules in [`can_break_between`] are the emergency
/// fallback so text still never overflows. `first_max` / `rest_max`
/// allow a hanging indent. Lines never start or end with a space.
fn break_lines_at(
    clusters: &[Cluster],
    break_set: &std::collections::HashSet<usize>,
    first_max: f32,
    rest_max: f32,
) -> Vec<Vec<Cluster>> {
    let mut lines: Vec<Vec<Cluster>> = Vec::new();
    let mut line: Vec<Cluster> = Vec::new();
    let mut width = 0.0_f32;
    let mut last_word_break: Option<usize> = None; // index INTO line
    let mut last_any_break: Option<usize> = None;

    for cl in clusters {
        let max = if lines.is_empty() {
            first_max
        } else {
            rest_max
        };
        if cl.is_space && line.is_empty() {
            continue; // never lead with a space
        }
        if !line.is_empty() {
            if break_set.contains(&cl.byte_start) && !no_break_before(cl.base) {
                last_word_break = Some(line.len());
            }
            if can_break_between(line.last().unwrap(), cl) {
                last_any_break = Some(line.len());
            }
        }
        if width + cl.width_mm > max && !line.is_empty() {
            let split_at = last_word_break.or(last_any_break).unwrap_or(line.len());
            let mut rest: Vec<Cluster> = line.split_off(split_at);
            while line.last().map(|c| c.is_space).unwrap_or(false) {
                line.pop();
            }
            while rest.first().map(|c| c.is_space).unwrap_or(false) {
                rest.remove(0);
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            width = rest.iter().map(|c| c.width_mm).sum();
            line = rest;
            last_word_break = None;
            last_any_break = None;
            if cl.is_space && line.is_empty() {
                continue;
            }
        }
        width += cl.width_mm;
        line.push(cl.clone());
    }
    while line.last().map(|c| c.is_space).unwrap_or(false) {
        line.pop();
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Convenience wrapper: derive the ICU break set from the clusters'
/// own text, then break. (Callers with pre-clusterized text — the
/// span path — use this; tests can call `break_lines_at` directly.)
fn break_lines(clusters: &[Cluster], first_max: f32, rest_max: f32) -> Vec<Vec<Cluster>> {
    let text: String = clusters.iter().map(|c| c.text.as_str()).collect();
    let breaks: std::collections::HashSet<usize> =
        line_break_opportunities(&text).into_iter().collect();
    break_lines_at(clusters, &breaks, first_max, rest_max)
}

fn spans_plain_text(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

fn measure_spans_mm(spans: &[Span], pt: f32) -> f32 {
    clusterize(spans, pt).iter().map(|c| c.width_mm).sum()
}

// ─── Renderer ─────────────────────────────────────────────────────────

const COLOR_TEXT: (f32, f32, f32) = (0.07, 0.07, 0.07);
const COLOR_DIM: (f32, f32, f32) = (0.40, 0.40, 0.40);
const COLOR_CODE: (f32, f32, f32) = (0.15, 0.15, 0.35);
const COLOR_BORDER: (f32, f32, f32) = (0.65, 0.65, 0.65);
const COLOR_SHADE: (f32, f32, f32) = (0.94, 0.94, 0.94);
const COLOR_QUOTE_BAR: (f32, f32, f32) = (0.75, 0.75, 0.75);

fn rgb(c: (f32, f32, f32)) -> Color {
    Color::Rgb(Rgb::new(c.0, c.1, c.2, None))
}

struct PdfRenderer {
    doc: PdfDocumentReference,
    pages: Vec<PdfPageIndex>,
    current_layer: PdfLayerIndex,
    fonts: [IndirectFontRef; FONT_COUNT],
    page_w_mm: f32,
    page_h_mm: f32,
    /// Vertical position measured DOWN from the top of the page.
    cursor_y_mm: f32,
    body_pt: f32,
    page_break_h1: bool,
    /// 0 = no bookmarks, 1 = H1 only (default), 2 = H1+H2.
    outline_depth: u8,
    image_base: PathBuf,
    emitted_any_block: bool,
}

impl PdfRenderer {
    fn printable_w(&self) -> f32 {
        self.page_w_mm - 2.0 * MARGIN_MM
    }
    fn bottom_limit(&self) -> f32 {
        self.page_h_mm - MARGIN_MM - FOOTER_BAND_MM
    }
    fn current_page(&self) -> PdfPageIndex {
        *self.pages.last().expect("at least one page")
    }
    fn layer(&self) -> printpdf::PdfLayerReference {
        self.doc
            .get_page(self.current_page())
            .get_layer(self.current_layer)
    }

    fn new_page(&mut self) {
        let (page, layer) = self
            .doc
            .add_page(Mm(self.page_w_mm), Mm(self.page_h_mm), "Layer 1");
        self.pages.push(page);
        self.current_layer = layer;
        self.cursor_y_mm = MARGIN_MM;
    }

    fn ensure_room(&mut self, needed_mm: f32) {
        if self.cursor_y_mm + needed_mm > self.bottom_limit() && self.cursor_y_mm > MARGIN_MM {
            self.new_page();
        }
    }

    fn vertical_gap(&mut self, mm: f32) {
        self.cursor_y_mm += mm;
    }

    /// Draw one pre-broken line of clusters at `x_start`, advancing the
    /// cursor by `pt × line_factor`. Consecutive same-font runs are
    /// shaped with HarfBuzz: zero-offset glyphs batch into one
    /// `write_codepoints` text section; GPOS-offset glyphs (raised
    /// tone marks, shifted marks over tall consonants) are placed
    /// absolutely in their own section so vertical offsets — which
    /// PDF's TJ operator can't express — render correctly.
    fn draw_cluster_line(
        &mut self,
        line: &[Cluster],
        x_start_mm: f32,
        pt: f32,
        line_factor: f32,
        color: (f32, f32, f32),
    ) {
        let line_h = pt * line_factor * PT_TO_MM;
        self.ensure_room(line_h);
        let baseline_from_top = self.cursor_y_mm + pt * PT_TO_MM;
        let baseline_y = self.page_h_mm - baseline_from_top;
        let layer = self.layer();

        let mut x = x_start_mm;
        let mut i = 0;
        while i < line.len() {
            let font = line[i].font;
            let code = line[i].style.code;
            let mut text = String::new();
            let mut hmtx_w = 0.0_f32;
            while i < line.len() && line[i].font == font && line[i].style.code == code {
                text.push_str(&line[i].text);
                hmtx_w += line[i].width_mm;
                i += 1;
            }
            let eff_pt = if code { pt * 0.92 } else { pt };
            layer.set_fill_color(rgb(if code { COLOR_CODE } else { color }));

            match shape_run(&text, font, eff_pt) {
                Some((glyphs, shaped_w)) => {
                    let font_ref = &self.fonts[font as usize];
                    // Batch consecutive zero-offset glyphs; emit offset
                    // glyphs (marks) absolutely positioned.
                    let mut pen = x;
                    let mut batch: Vec<u16> = Vec::new();
                    let mut batch_x = pen;
                    let flush = |batch: &mut Vec<u16>, at_x: f32| {
                        if batch.is_empty() {
                            return;
                        }
                        layer.begin_text_section();
                        layer.set_font(font_ref, eff_pt);
                        layer.set_text_cursor(Mm(at_x), Mm(baseline_y));
                        layer.write_codepoints(batch.drain(..));
                        layer.end_text_section();
                    };
                    for g in &glyphs {
                        if g.x_offset.abs() < 0.001 && g.y_offset.abs() < 0.001 {
                            if batch.is_empty() {
                                batch_x = pen;
                            }
                            batch.push(g.gid);
                        } else {
                            flush(&mut batch, batch_x);
                            layer.begin_text_section();
                            layer.set_font(font_ref, eff_pt);
                            layer
                                .set_text_cursor(Mm(pen + g.x_offset), Mm(baseline_y + g.y_offset));
                            layer.write_codepoints(std::iter::once(g.gid));
                            layer.end_text_section();
                        }
                        pen += g.x_advance;
                    }
                    flush(&mut batch, batch_x);
                    let _ = shaped_w;
                    x = pen;
                }
                None => {
                    // Shaper unavailable (corrupt face?) — cmap fallback.
                    layer.use_text(
                        &text,
                        eff_pt,
                        Mm(x),
                        Mm(baseline_y),
                        &self.fonts[font as usize],
                    );
                    x += hmtx_w;
                }
            }
        }
        layer.set_fill_color(rgb(COLOR_TEXT));
        self.cursor_y_mm += line_h;
    }

    /// Wrap styled spans into the printable column and draw them.
    /// `indent` shifts the whole block right; `hang` additionally
    /// shifts continuation lines (list items).
    fn draw_spans(
        &mut self,
        spans: &[Span],
        pt: f32,
        indent_mm: f32,
        hang_mm: f32,
        line_factor: f32,
        color: (f32, f32, f32),
    ) {
        let clusters = clusterize(spans, pt);
        if clusters.is_empty() {
            return;
        }
        let first_max = self.printable_w() - indent_mm;
        let rest_max = self.printable_w() - indent_mm - hang_mm;
        let lines = break_lines(&clusters, first_max, rest_max);
        for (li, line) in lines.iter().enumerate() {
            let x = MARGIN_MM + indent_mm + if li == 0 { 0.0 } else { hang_mm };
            self.draw_cluster_line(line, x, pt, line_factor, color);
        }
    }

    fn draw_heading(&mut self, level: HeadingLevel, spans: &[Span]) {
        let (scale, gap_before, gap_after) = match level {
            HeadingLevel::H1 => (2.0, 9.0, 4.0),
            HeadingLevel::H2 => (1.5, 7.0, 3.0),
            HeadingLevel::H3 => (1.22, 5.0, 2.2),
            _ => (1.05, 4.0, 2.0),
        };
        let pt = self.body_pt * scale;
        if self.page_break_h1 && level == HeadingLevel::H1 && self.emitted_any_block {
            self.new_page();
        }
        // Keep-with-next: heading + ~2 body lines must fit, else break.
        let needed =
            gap_before + pt * 1.3 * PT_TO_MM + 2.0 * self.body_pt * BODY_LINE_FACTOR * PT_TO_MM;
        self.ensure_room(needed);
        self.vertical_gap(gap_before);

        // Headings render bold regardless of inline markers.
        let styled: Vec<Span> = spans
            .iter()
            .map(|s| Span {
                text: s.text.clone(),
                style: InlineStyle {
                    bold: true,
                    ..s.style
                },
            })
            .collect();
        self.draw_spans(&styled, pt, 0.0, 0.0, 1.3, COLOR_TEXT);

        if level == HeadingLevel::H1 {
            // Thin accent rule under chapter titles.
            let y_mm = self.page_h_mm - self.cursor_y_mm - 0.8;
            let layer = self.layer();
            layer.set_outline_color(rgb(COLOR_BORDER));
            layer.set_outline_thickness(0.6);
            layer.add_line(Line {
                points: vec![
                    (Point::new(Mm(MARGIN_MM), Mm(y_mm)), false),
                    (
                        Point::new(Mm(MARGIN_MM + self.printable_w()), Mm(y_mm)),
                        false,
                    ),
                ],
                is_closed: false,
            });
            self.vertical_gap(1.6);
        }
        self.vertical_gap(gap_after);

        let bookmark = match level {
            HeadingLevel::H1 => self.outline_depth >= 1,
            HeadingLevel::H2 => self.outline_depth >= 2,
            _ => false,
        };
        if bookmark {
            let name = spans_plain_text(spans);
            if !name.trim().is_empty() {
                self.doc.add_bookmark(name.trim(), self.current_page());
            }
        }
        self.emitted_any_block = true;
    }

    fn block_paragraph(&mut self, spans: &[Span], quote_depth: usize) {
        if spans.iter().all(|s| s.text.trim().is_empty()) {
            return;
        }
        if quote_depth > 0 {
            let start_y = self.cursor_y_mm;
            let start_page = self.pages.len();
            self.draw_spans(spans, self.body_pt, 6.0, 0.0, BODY_LINE_FACTOR, COLOR_DIM);
            // Quote bar (only when the paragraph stayed on one page —
            // cross-page bars would need per-page segments).
            if self.pages.len() == start_page {
                let layer = self.layer();
                layer.set_fill_color(rgb(COLOR_QUOTE_BAR));
                layer.add_rect(
                    Rect::new(
                        Mm(MARGIN_MM + 1.0),
                        Mm(self.page_h_mm - self.cursor_y_mm + 1.0),
                        Mm(MARGIN_MM + 2.2),
                        Mm(self.page_h_mm - start_y - 1.0),
                    )
                    .with_mode(PaintMode::Fill)
                    .with_winding(WindingOrder::NonZero),
                );
                layer.set_fill_color(rgb(COLOR_TEXT));
            }
        } else {
            self.draw_spans(spans, self.body_pt, 0.0, 0.0, BODY_LINE_FACTOR, COLOR_TEXT);
        }
        self.vertical_gap(PARAGRAPH_GAP_MM);
        self.emitted_any_block = true;
    }

    fn block_list_item(&mut self, marker: &str, spans: &[Span], depth: usize) {
        let indent = 4.0 + (depth.saturating_sub(1) as f32) * 6.0;
        let marker_w = marker
            .chars()
            .map(|c| char_advance_mm(c, FontId::LatinReg, self.body_pt))
            .sum::<f32>()
            + 1.8;
        let line_h = self.body_pt * BODY_LINE_FACTOR * PT_TO_MM;
        self.ensure_room(line_h);
        {
            let baseline_y = Mm(self.page_h_mm - self.cursor_y_mm - self.body_pt * PT_TO_MM);
            let layer = self.layer();
            layer.set_fill_color(rgb(COLOR_TEXT));
            layer.use_text(
                marker,
                self.body_pt,
                Mm(MARGIN_MM + indent),
                baseline_y,
                &self.fonts[FontId::LatinReg as usize],
            );
        }
        // Item text occupies the same first line, shifted right of the
        // marker; wrapped lines align under the text column (hanging
        // indent of zero relative to the text start).
        let saved = self.cursor_y_mm;
        self.draw_spans(
            spans,
            self.body_pt,
            indent + marker_w,
            0.0,
            BODY_LINE_FACTOR,
            COLOR_TEXT,
        );
        if (self.cursor_y_mm - saved).abs() < 0.01 {
            // Empty item text still consumes the marker line.
            self.cursor_y_mm += line_h;
        }
        self.vertical_gap(0.6);
        self.emitted_any_block = true;
    }

    fn block_code(&mut self, text: &str) {
        let pt = self.body_pt * 0.9;
        let line_h = pt * 1.45 * PT_TO_MM;
        let pad = 1.6_f32;
        self.vertical_gap(1.5);
        for raw_line in text.trim_end_matches('\n').split('\n') {
            self.ensure_room(line_h);
            let spans = [Span {
                text: raw_line.to_string(),
                style: InlineStyle {
                    code: true,
                    ..Default::default()
                },
            }];
            let clusters = clusterize(&spans, pt);
            let max_w = self.printable_w() - 2.0 * pad;
            let lines = break_lines(&clusters, max_w, max_w);
            if lines.is_empty() {
                // Blank code line: keep the shaded strip + spacing.
                let top = self.cursor_y_mm;
                let layer = self.layer();
                layer.set_fill_color(rgb(COLOR_SHADE));
                layer.add_rect(
                    Rect::new(
                        Mm(MARGIN_MM),
                        Mm(self.page_h_mm - top - line_h),
                        Mm(MARGIN_MM + self.printable_w()),
                        Mm(self.page_h_mm - top),
                    )
                    .with_mode(PaintMode::Fill)
                    .with_winding(WindingOrder::NonZero),
                );
                layer.set_fill_color(rgb(COLOR_TEXT));
                self.cursor_y_mm += line_h;
                continue;
            }
            for line in lines {
                self.ensure_room(line_h);
                let top = self.cursor_y_mm;
                {
                    let layer = self.layer();
                    layer.set_fill_color(rgb(COLOR_SHADE));
                    layer.add_rect(
                        Rect::new(
                            Mm(MARGIN_MM),
                            Mm(self.page_h_mm - top - line_h),
                            Mm(MARGIN_MM + self.printable_w()),
                            Mm(self.page_h_mm - top),
                        )
                        .with_mode(PaintMode::Fill)
                        .with_winding(WindingOrder::NonZero),
                    );
                    layer.set_fill_color(rgb(COLOR_TEXT));
                }
                self.draw_cluster_line(&line, MARGIN_MM + pad, pt, 1.45, COLOR_CODE);
            }
        }
        self.vertical_gap(PARAGRAPH_GAP_MM + 1.0);
        self.emitted_any_block = true;
    }

    fn block_rule(&mut self) {
        self.ensure_room(6.0);
        self.vertical_gap(2.5);
        let y = Mm(self.page_h_mm - self.cursor_y_mm);
        let layer = self.layer();
        layer.set_outline_color(rgb(COLOR_BORDER));
        layer.set_outline_thickness(0.4);
        layer.add_line(Line {
            points: vec![
                (Point::new(Mm(MARGIN_MM + 20.0), y), false),
                (
                    Point::new(Mm(MARGIN_MM + self.printable_w() - 20.0), y),
                    false,
                ),
            ],
            is_closed: false,
        });
        self.vertical_gap(3.5);
        self.emitted_any_block = true;
    }

    fn block_image(&mut self, rel_path: &str, alt: &str) {
        // Resolve relative to the markdown file's directory (matches the
        // Files-tab preview), with a cwd fallback for root-relative refs.
        let candidates = [self.image_base.join(rel_path), PathBuf::from(rel_path)];
        let mut loaded: Option<image::DynamicImage> = None;
        for cand in &candidates {
            let norm = normalize_path(cand);
            if let Ok(p) = crate::sandbox::Sandbox::check(&norm.to_string_lossy()) {
                if let Ok(bytes) = std::fs::read(&p) {
                    if !bytes.is_empty() && bytes.len() <= 5 * 1024 * 1024 {
                        if let Ok(img) = image::load_from_memory(&bytes) {
                            loaded = Some(img);
                            break;
                        }
                    }
                }
            }
        }
        let Some(dyn_img) = loaded else { return };

        let printable = self.printable_w();
        let w_px = dyn_img.width() as f32;
        let h_px = dyn_img.height() as f32;
        if w_px == 0.0 || h_px == 0.0 {
            return;
        }
        let natural_w = w_px * 0.2646; // 96 DPI
        let natural_h = h_px * 0.2646;
        // Book figures fill the content column: scale to the printable
        // width — UP as well as down — preserving aspect ratio, then
        // cap by the page's printable height (minus caption room) so
        // tall figures never spill past the footer band.
        let mut w = printable;
        let mut h = natural_h * printable / natural_w;
        let max_h = self.page_h_mm - 2.0 * MARGIN_MM - FOOTER_BAND_MM - 14.0;
        if h > max_h {
            w = w * max_h / h;
            h = max_h;
        }
        let caption_h = if alt.trim().is_empty() {
            0.0
        } else {
            9.0 * 1.4 * PT_TO_MM * 2.0 // reserve ~2 caption lines
        };
        self.ensure_room(h + caption_h + 3.0);
        self.vertical_gap(2.0);

        let x = MARGIN_MM + (self.printable_w() - w) / 2.0; // centered
        let y_bottom = self.page_h_mm - self.cursor_y_mm - h;
        let image = Image::from_dynamic_image(&dyn_img);
        // dpi MUST match the 96-dpi base used for natural_w/h above:
        // printpdf defaults to 300 dpi, which shrank every figure to
        // 96/300 ≈ a third of the intended size.
        image.add_to_layer(
            self.layer(),
            ImageTransform {
                translate_x: Some(Mm(x)),
                translate_y: Some(Mm(y_bottom)),
                scale_x: Some(w / natural_w),
                scale_y: Some(h / natural_h),
                dpi: Some(96.0),
                ..Default::default()
            },
        );
        self.cursor_y_mm += h + 1.6;

        if !alt.trim().is_empty() {
            let cap_pt = 9.0;
            let spans = [Span {
                text: alt.trim().to_string(),
                style: InlineStyle {
                    italic: true,
                    ..Default::default()
                },
            }];
            let clusters = clusterize(&spans, cap_pt);
            let cap_w = self.printable_w() - 20.0;
            let lines = break_lines(&clusters, cap_w, cap_w);
            for line in lines {
                let lw: f32 = line.iter().map(|c| c.width_mm).sum();
                let cx = MARGIN_MM + (self.printable_w() - lw) / 2.0;
                self.draw_cluster_line(&line, cx, cap_pt, 1.4, COLOR_DIM);
            }
        }
        self.vertical_gap(PARAGRAPH_GAP_MM + 1.0);
        self.emitted_any_block = true;
    }

    fn block_table(&mut self, rows: &[Vec<Vec<Span>>]) {
        if rows.is_empty() {
            return;
        }
        let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let pt = self.body_pt * 0.92;
        let pad = 1.8_f32;
        let line_h = pt * 1.4 * PT_TO_MM;

        // Natural column widths (longest cell, capped), scaled to fit.
        // Row 0 renders BOLD — measure it bold or headers wrap a char
        // short. +0.6mm slack absorbs float accumulation in the
        // greedy breaker.
        let mut nat = vec![10.0_f32; cols];
        for (ri, row) in rows.iter().enumerate() {
            for (ci, cell) in row.iter().enumerate() {
                let measured = if ri == 0 {
                    let bolded: Vec<Span> = cell
                        .iter()
                        .map(|s| Span {
                            text: s.text.clone(),
                            style: InlineStyle {
                                bold: true,
                                ..s.style
                            },
                        })
                        .collect();
                    measure_spans_mm(&bolded, pt)
                } else {
                    measure_spans_mm(cell, pt)
                };
                let w = measured + 2.0 * pad + 0.6;
                nat[ci] = nat[ci].max(w.min(70.0));
            }
        }
        let total: f32 = nat.iter().sum();
        let scale = if total > self.printable_w() {
            self.printable_w() / total
        } else {
            1.0
        };
        let widths: Vec<f32> = nat.iter().map(|w| (w * scale).max(12.0)).collect();
        let table_w: f32 = widths.iter().sum();

        self.vertical_gap(1.5);
        for (ri, row) in rows.iter().enumerate() {
            let is_header = ri == 0;
            // Wrap every cell; row height = tallest cell.
            let mut cell_lines: Vec<Vec<Vec<Cluster>>> = Vec::with_capacity(cols);
            for ci in 0..cols {
                let bolded: Vec<Span>;
                let spans: &[Span] = match row.get(ci) {
                    Some(cell) if is_header => {
                        bolded = cell
                            .iter()
                            .map(|s| Span {
                                text: s.text.clone(),
                                style: InlineStyle {
                                    bold: true,
                                    ..s.style
                                },
                            })
                            .collect();
                        &bolded
                    }
                    Some(cell) => cell,
                    None => &[],
                };
                let clusters = clusterize(spans, pt);
                let max = widths[ci] - 2.0 * pad;
                cell_lines.push(break_lines(&clusters, max, max));
            }
            let row_lines = cell_lines.iter().map(|c| c.len().max(1)).max().unwrap_or(1);
            let row_h = row_lines as f32 * line_h + 2.0 * pad;
            self.ensure_room(row_h);

            let top = self.cursor_y_mm;
            {
                let layer = self.layer();
                if is_header {
                    layer.set_fill_color(rgb(COLOR_SHADE));
                    layer.add_rect(
                        Rect::new(
                            Mm(MARGIN_MM),
                            Mm(self.page_h_mm - top - row_h),
                            Mm(MARGIN_MM + table_w),
                            Mm(self.page_h_mm - top),
                        )
                        .with_mode(PaintMode::Fill)
                        .with_winding(WindingOrder::NonZero),
                    );
                }
                layer.set_outline_color(rgb(COLOR_BORDER));
                layer.set_outline_thickness(0.25);
                layer.add_rect(
                    Rect::new(
                        Mm(MARGIN_MM),
                        Mm(self.page_h_mm - top - row_h),
                        Mm(MARGIN_MM + table_w),
                        Mm(self.page_h_mm - top),
                    )
                    .with_mode(PaintMode::Stroke)
                    .with_winding(WindingOrder::NonZero),
                );
                let mut x_sep = MARGIN_MM;
                for w in widths.iter().take(cols - 1) {
                    x_sep += w;
                    layer.add_line(Line {
                        points: vec![
                            (Point::new(Mm(x_sep), Mm(self.page_h_mm - top)), false),
                            (
                                Point::new(Mm(x_sep), Mm(self.page_h_mm - top - row_h)),
                                false,
                            ),
                        ],
                        is_closed: false,
                    });
                }
                layer.set_fill_color(rgb(COLOR_TEXT));
            }

            // Cell text — manual cursor control inside the reserved row.
            let mut x = MARGIN_MM;
            for (ci, lines) in cell_lines.iter().enumerate() {
                let saved_cursor = self.cursor_y_mm;
                self.cursor_y_mm = top + pad;
                for line in lines {
                    self.draw_cluster_line(line, x + pad, pt, 1.4, COLOR_TEXT);
                }
                self.cursor_y_mm = saved_cursor;
                x += widths[ci];
            }
            self.cursor_y_mm = top + row_h;
        }
        self.vertical_gap(PARAGRAPH_GAP_MM + 1.0);
        self.emitted_any_block = true;
    }
}

/// Lexically resolve `..` / `.` without touching the filesystem, so
/// `<base>/../images/x.jpg` becomes a sandbox-checkable path.
fn normalize_path(p: &Path) -> PathBuf {
    let mut out: Vec<std::path::Component> = Vec::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => match out.last() {
                Some(std::path::Component::Normal(_)) => {
                    out.pop();
                }
                Some(std::path::Component::RootDir) => {}
                _ => out.push(comp),
            },
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out.iter().collect()
}

fn render_pdf(
    path: &Path,
    title: &str,
    content: &str,
    body_pt: f32,
    page_w_mm: f32,
    page_h_mm: f32,
    page_break_h1: bool,
    outline_depth: u8,
    image_base: &Path,
    family: Family,
) -> Result<usize> {
    // Set the ambient family BEFORE any faces()/shaping call below so
    // metrics, shaping and embedding all use the same typeface.
    RENDER_FAMILY.with(|c| c.set(family));

    let (doc, first_page, first_layer) =
        PdfDocument::new(title, Mm(page_w_mm), Mm(page_h_mm), "Layer 1");

    // Embed the selected family's five faces in FontId order
    // (LatinReg, LatinBold, LatinItal, ThaiReg, ThaiBold).
    let fb = family_bytes(family);
    let fonts = [
        doc.add_external_font(fb[0])
            .map_err(|e| Error::Tool(format!("embed regular: {e}")))?,
        doc.add_external_font(fb[1])
            .map_err(|e| Error::Tool(format!("embed bold: {e}")))?,
        doc.add_external_font(fb[2])
            .map_err(|e| Error::Tool(format!("embed italic: {e}")))?,
        doc.add_external_font(fb[3])
            .map_err(|e| Error::Tool(format!("embed Thai: {e}")))?,
        doc.add_external_font(fb[4])
            .map_err(|e| Error::Tool(format!("embed Thai bold: {e}")))?,
    ];

    let mut r = PdfRenderer {
        doc,
        pages: vec![first_page],
        current_layer: first_layer,
        fonts,
        page_w_mm,
        page_h_mm,
        cursor_y_mm: MARGIN_MM,
        body_pt,
        page_break_h1,
        outline_depth,
        image_base: image_base.to_path_buf(),
        emitted_any_block: false,
    };

    render_markdown(&mut r, content);
    stamp_footers(&mut r);

    let pages_written = r.pages.len();
    let mut raw: Vec<u8> = Vec::new();
    r.doc
        .save(&mut BufWriter::new(&mut raw))
        .map_err(|e| Error::Tool(format!("save PDF: {e}")))?;
    let fixed = fix_outline_title_encoding(raw);
    std::fs::write(path, fixed)
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
    Ok(pages_written)
}

/// printpdf serializes outline `/Title` strings as Literal UTF-8
/// bytes; PDF viewers decode plain strings as PDFDocEncoding, so
/// Thai (or any non-ASCII) chapter names render as mojibake in the
/// sidebar. Re-encode every outline title as UTF-16BE with BOM
/// (ISO 32000-1 §7.9.2.2). Returns the input unchanged if the PDF
/// can't be parsed — a garbled sidebar beats a failed export.
fn fix_outline_title_encoding(bytes: Vec<u8>) -> Vec<u8> {
    let mut doc = match lopdf::Document::load_mem(&bytes) {
        Ok(d) => d,
        Err(_) => return bytes,
    };
    let targets: Vec<(lopdf::ObjectId, Vec<u8>)> = doc
        .objects
        .iter()
        .filter_map(|(id, obj)| {
            let dict = obj.as_dict().ok()?;
            if !dict.has(b"Dest") {
                return None;
            }
            match dict.get(b"Title") {
                Ok(lopdf::Object::String(t, _))
                    if std::str::from_utf8(t).is_ok_and(|s| !s.is_ascii()) =>
                {
                    Some((*id, t.clone()))
                }
                _ => None,
            }
        })
        .collect();
    if targets.is_empty() {
        return bytes;
    }
    for (id, title_bytes) in targets {
        let Ok(text) = String::from_utf8(title_bytes) else {
            continue;
        };
        let mut utf16: Vec<u8> = vec![0xFE, 0xFF];
        for unit in text.encode_utf16() {
            utf16.extend_from_slice(&unit.to_be_bytes());
        }
        if let Ok(obj) = doc.get_object_mut(id) {
            if let Ok(dict) = obj.as_dict_mut() {
                dict.set(
                    "Title",
                    lopdf::Object::String(utf16, lopdf::StringFormat::Hexadecimal),
                );
            }
        }
    }
    let mut out = Vec::new();
    if doc.save_to(&mut out).is_err() {
        return bytes;
    }
    out
}

/// Centered `n / N` footer on every page, drawn after layout so the
/// total is known. Single-page documents skip the footer.
fn stamp_footers(r: &mut PdfRenderer) {
    let total = r.pages.len();
    if total <= 1 {
        return;
    }
    let pt = 9.0_f32;
    for (i, page) in r.pages.clone().into_iter().enumerate() {
        let text = format!("{} / {}", i + 1, total);
        let w: f32 = text
            .chars()
            .map(|c| char_advance_mm(c, FontId::LatinReg, pt))
            .sum();
        let x = (r.page_w_mm - w) / 2.0;
        let y = Mm(MARGIN_MM * 0.5);
        let layer = r.doc.get_page(page).get_layer(r.current_layer);
        layer.set_fill_color(rgb(COLOR_DIM));
        layer.use_text(&text, pt, Mm(x), y, &r.fonts[FontId::LatinReg as usize]);
        layer.set_fill_color(rgb(COLOR_TEXT));
    }
}

// ─── Markdown event loop ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct ListState {
    next_number: Option<u64>, // None = bullet list
}

fn render_markdown(r: &mut PdfRenderer, content: &str) {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    // Repair LLM-miscounted table delimiter rows so GFM tables aren't silently
    // demoted to paragraphs (see tools::md_tables).
    let content = super::md_tables::repair_table_delimiters(content);
    let parser = Parser::new_ext(&content, opts);

    let mut spans: Vec<Span> = Vec::new();
    let mut style = InlineStyle::default();
    let mut heading: Option<HeadingLevel> = None;
    let mut quote_depth = 0usize;
    let mut list_stack: Vec<ListState> = Vec::new();
    let mut item_pending = false;

    let mut code_block = false;
    let mut code_buf = String::new();

    let mut in_table = false;
    let mut table_rows: Vec<Vec<Vec<Span>>> = Vec::new();
    let mut table_row: Vec<Vec<Span>> = Vec::new();
    let mut table_cell: Vec<Span> = Vec::new();

    // (dest path, accumulated alt text) while between Start/End(Image).
    let mut image_capture: Option<(String, String)> = None;

    macro_rules! emit_list_item {
        () => {{
            let depth = list_stack.len();
            let marker = match list_stack.last_mut() {
                Some(ls) => match ls.next_number.as_mut() {
                    Some(n) => {
                        let m = format!("{n}.");
                        *n += 1;
                        m
                    }
                    None => "•".to_string(),
                },
                None => "•".to_string(),
            };
            r.block_list_item(&marker, &spans, depth);
            spans.clear();
            item_pending = false;
        }};
    }

    macro_rules! push_text {
        ($s:expr) => {{
            let s: &str = $s;
            if let Some((_, alt)) = image_capture.as_mut() {
                alt.push_str(s);
            } else if in_table {
                table_cell.push(Span {
                    text: s.to_string(),
                    style,
                });
            } else if code_block {
                code_buf.push_str(s);
            } else {
                spans.push(Span {
                    text: s.to_string(),
                    style,
                });
            }
        }};
    }

    macro_rules! flush_block {
        () => {{
            if !spans.is_empty() {
                match heading {
                    Some(level) => r.draw_heading(level, &spans),
                    None => r.block_paragraph(&spans, quote_depth),
                }
                spans.clear();
            }
        }};
    }

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                flush_block!();
                heading = Some(level);
            }
            Event::End(TagEnd::Heading(_)) => {
                flush_block!();
                heading = None;
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                if item_pending {
                    emit_list_item!();
                } else {
                    flush_block!();
                }
            }
            Event::Start(Tag::BlockQuote) => {
                flush_block!();
                quote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote) => {
                flush_block!();
                quote_depth = quote_depth.saturating_sub(1);
            }
            Event::Start(Tag::List(start)) => {
                flush_block!();
                list_stack.push(ListState { next_number: start });
            }
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
                if list_stack.is_empty() {
                    r.vertical_gap(PARAGRAPH_GAP_MM * 0.8);
                }
            }
            Event::Start(Tag::Item) => {
                spans.clear();
                item_pending = true;
            }
            Event::End(TagEnd::Item) => {
                if item_pending {
                    emit_list_item!();
                }
            }
            Event::Start(Tag::Strong) => style.bold = true,
            Event::End(TagEnd::Strong) => style.bold = false,
            Event::Start(Tag::Emphasis) => style.italic = true,
            Event::End(TagEnd::Emphasis) => style.italic = false,
            Event::Start(Tag::CodeBlock(_)) => {
                flush_block!();
                code_block = true;
                code_buf.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                code_block = false;
                r.block_code(&code_buf);
                code_buf.clear();
            }
            Event::Rule => {
                flush_block!();
                r.block_rule();
            }
            Event::Text(s) => push_text!(&s),
            Event::Code(s) => {
                let saved = style;
                style.code = true;
                push_text!(&s);
                style = saved;
            }
            Event::SoftBreak => push_text!(" "),
            Event::HardBreak => {
                if !in_table && image_capture.is_none() && !code_block {
                    flush_block!();
                }
            }
            // Tables.
            Event::Start(Tag::Table(_)) => {
                flush_block!();
                in_table = true;
                table_rows.clear();
            }
            Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => {
                table_row.clear();
            }
            Event::Start(Tag::TableCell) => {
                table_cell.clear();
            }
            Event::End(TagEnd::TableCell) => {
                table_row.push(std::mem::take(&mut table_cell));
            }
            Event::End(TagEnd::TableRow) | Event::End(TagEnd::TableHead) => {
                table_rows.push(std::mem::take(&mut table_row));
            }
            Event::End(TagEnd::Table) => {
                in_table = false;
                r.block_table(&table_rows);
                table_rows.clear();
            }
            // Images: alt text accumulates between Start/End.
            Event::Start(Tag::Image { dest_url, .. }) => {
                flush_block!();
                image_capture = Some((dest_url.to_string(), String::new()));
            }
            Event::End(TagEnd::Image) => {
                if let Some((dest, alt)) = image_capture.take() {
                    r.block_image(&dest, &alt);
                }
            }
            _ => {}
        }
    }
    flush_block!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn writes_pdf_with_thai_and_latin() {
        let dir = tempdir().unwrap();
        let simple = dir.path().join("hello.pdf");
        let thai = dir.path().join("thai.pdf");

        let msg = PdfCreateTool
            .call(json!({
                "path": simple.to_string_lossy(),
                "content": "# Hello\n\nThis is a **bold** paragraph with *italic* text."
            }))
            .await
            .unwrap();
        assert!(msg.contains("Wrote PDF to"));
        let bytes = std::fs::read(&simple).unwrap();
        assert!(bytes.starts_with(b"%PDF-"), "output should be a PDF");

        let _ = PdfCreateTool
            .call(json!({
                "path": thai.to_string_lossy(),
                "content": "# สวัสดี\n\nนี่คือเอกสารทดสอบ Thai-Latin mixed text กลางย่อหน้า ผำเป็นพืชน้ำขนาดเล็กที่สุดในโลกและมีโปรตีนสูงมากเหมาะแก่การเพาะเลี้ยงเชิงพาณิชย์"
            }))
            .await
            .unwrap();
        assert!(std::fs::metadata(&thai).unwrap().len() > 1000);
    }

    #[tokio::test]
    async fn writes_pdf_with_pipe_table() {
        if !std::process::Command::new("pdftotext")
            .arg("-v")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("skipping: pdftotext not in PATH");
            return;
        }
        let dir = tempdir().unwrap();
        let pdf = dir.path().join("table.pdf");
        PdfCreateTool
            .call(json!({
                "path": pdf.to_string_lossy(),
                "content": "# Expense\n\n| Item | Qty | Price |\n|---|---|---|\n| Coffee | 2 | $7 |\n"
            }))
            .await
            .unwrap();
        let extracted = crate::tools::PdfReadTool
            .call(json!({"path": pdf.to_string_lossy()}))
            .await
            .unwrap();
        assert!(extracted.contains("Item"), "header missing: {extracted}");
        // The shaper applies the ff ligature, which pdftotext may
        // extract as the single U+FB00 char.
        assert!(
            extracted.contains("Coffee") || extracted.contains("Co\u{fb00}ee"),
            "row missing: {extracted}"
        );
    }

    #[tokio::test]
    async fn content_path_renders_file_with_relative_image() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("images")).unwrap();
        std::fs::create_dir_all(dir.path().join("chapters")).unwrap();
        // Generate a small PNG with the image crate (hand-rolled byte
        // arrays rot when encoders change — render one for real).
        let img = image::DynamicImage::new_rgb8(4, 4);
        img.save(dir.path().join("images/fig.png")).unwrap();
        let md = dir.path().join("chapters/ch01.md");
        std::fs::write(
            &md,
            "# บทที่ 1\n\nเนื้อหา\n\n![ภาพที่ 1.1 — ทดสอบ](../images/fig.png)\n",
        )
        .unwrap();
        let pdf = dir.path().join("out.pdf");
        let msg = PdfCreateTool
            .call(json!({
                "path": pdf.to_string_lossy(),
                "content_path": md.to_string_lossy(),
                "page_break_h1": true
            }))
            .await
            .unwrap();
        assert!(msg.contains("Wrote PDF to"), "{msg}");
        assert!(std::fs::metadata(&pdf).unwrap().len() > 1000);
    }

    #[test]
    fn thai_clusters_keep_combining_marks() {
        let spans = [Span {
            text: "ที่นี่".to_string(), // ท + ี + ่ , น + ี + ่
            style: InlineStyle::default(),
        }];
        let clusters = clusterize(&spans, 11.0);
        assert_eq!(clusters.len(), 2, "marks must merge into base clusters");
        assert!(clusters[0].text.starts_with('ท'));
        assert_eq!(clusters[0].text.chars().count(), 3);
    }

    #[test]
    fn no_break_after_thai_lead_vowel() {
        let spans = [Span {
            text: "เกษตร".to_string(),
            style: InlineStyle::default(),
        }];
        let clusters = clusterize(&spans, 11.0);
        // เ | ก — no break allowed between lead vowel and consonant.
        assert!(!can_break_between(&clusters[0], &clusters[1]));
        // ก | ษ — Thai-Thai break is fine.
        assert!(can_break_between(&clusters[1], &clusters[2]));
    }

    #[test]
    fn break_lines_wraps_long_thai_without_overflow() {
        let text = "ผำเป็นพืชน้ำขนาดเล็กที่สุดในโลกมีโปรตีนสูงและเลี้ยงง่าย".repeat(4);
        let spans = [Span {
            text,
            style: InlineStyle::default(),
        }];
        let clusters = clusterize(&spans, 11.0);
        let lines = break_lines(&clusters, 60.0, 60.0);
        assert!(lines.len() > 2, "long Thai must wrap into multiple lines");
        for line in &lines {
            let w: f32 = line.iter().map(|c| c.width_mm).sum();
            assert!(w <= 60.5, "line overflows: {w}mm");
            // No line may start with a combining mark or ำ/ๆ.
            let first = line[0].base;
            assert!(!is_thai_combining_mark(first) && !no_break_before(first));
        }
    }

    #[test]
    fn glyph_metrics_differ_per_char() {
        // Real metrics: 'i' must be narrower than 'W' in the Latin face.
        let wi = char_advance_mm('i', FontId::LatinReg, 12.0);
        let ww = char_advance_mm('W', FontId::LatinReg, 12.0);
        assert!(wi > 0.0 && ww > wi, "i={wi} W={ww}");
        // Thai combining mark has (near-)zero advance.
        assert!(char_advance_mm('\u{0E34}', FontId::ThaiReg, 12.0) < 0.3);
    }

    #[test]
    fn shaper_raises_stacked_thai_tone_marks() {
        // ที่ = ท + ี (upper vowel) + ่ (tone mark). With GPOS the tone
        // mark must be RAISED to sit above the vowel; without shaping
        // both render at the same height and collide.
        // Fonts implement the raise either via GPOS y_offset or — as
        // Noto Sans Thai does — via a GSUB substitution to a raised
        // variant glyph (height baked into the outline). Accept either:
        // what matters is that the STACKED context produces different
        // positioning data than the unstacked one.
        let (stacked, _) = shape_run("ที่", FontId::ThaiReg, 11.0).expect("shaper");
        assert_eq!(stacked.len(), 3, "3 glyphs for ท ี ่");
        let (simple, _) = shape_run("ท่า", FontId::ThaiReg, 11.0).expect("shaper");
        let stacked_mark = &stacked[2];
        let simple_mark = &simple[1];
        let adjusted = stacked_mark.gid != simple_mark.gid
            || stacked_mark.y_offset > simple_mark.y_offset + 0.05;
        assert!(
            adjusted,
            "stacked tone mark must be raised (variant gid or GPOS): \
             stacked gid={} y={}, simple gid={} y={}",
            stacked_mark.gid, stacked_mark.y_offset, simple_mark.gid, simple_mark.y_offset
        );
    }

    #[test]
    fn shaper_handles_tall_consonant_marks() {
        // ป่ — the tone mark over a tall-ascender consonant must shift
        // (GPOS x/y or GSUB variant) so it doesn't collide with the
        // ascender. Assert the shaper produces SOME adjustment vs the
        // mark's default placement over ท.
        let (over_tall, _) = shape_run("ป่า", FontId::ThaiReg, 11.0).expect("shaper");
        let (over_normal, _) = shape_run("ท่า", FontId::ThaiReg, 11.0).expect("shaper");
        let tall_mark = &over_tall[1];
        let norm_mark = &over_normal[1];
        let differs = tall_mark.gid != norm_mark.gid
            || (tall_mark.y_offset - norm_mark.y_offset).abs() > 0.05
            || (tall_mark.x_offset - norm_mark.x_offset).abs() > 0.05;
        assert!(differs, "mark over ป must differ from mark over ท");
    }

    #[test]
    fn icu_segments_thai_at_word_boundaries() {
        // ประเทศไทย = ประเทศ + ไทย. The LSTM segmenter must offer the
        // word boundary (byte 18 = after 6 three-byte chars) and NOT
        // offer mid-word positions like ป|ระเทศ (byte 3).
        let breaks = line_break_opportunities("ประเทศไทย");
        assert!(breaks.contains(&18), "missing word boundary: {breaks:?}");
        assert!(!breaks.contains(&3), "mid-word break offered: {breaks:?}");
        assert!(!breaks.contains(&6), "mid-word break offered: {breaks:?}");
    }

    #[test]
    fn break_lines_prefers_thai_word_boundaries() {
        // A run of short Thai words must wrap BETWEEN words, never
        // inside one, as long as each word fits a line.
        let text = "เกษตรกรไทยเลี้ยงผำในบ่อซีเมนต์หลังบ้านเพื่อขายเป็นรายได้เสริม";
        let spans = [Span {
            text: text.to_string(),
            style: InlineStyle::default(),
        }];
        let clusters = clusterize(&spans, 11.0);
        let word_starts: std::collections::HashSet<usize> =
            line_break_opportunities(text).into_iter().collect();
        let lines = break_lines(&clusters, 35.0, 35.0);
        assert!(lines.len() > 2, "must wrap: got {} line(s)", lines.len());
        for line in &lines[1..] {
            let first_byte = line[0].byte_start;
            assert!(
                word_starts.contains(&first_byte),
                "line starts mid-word at byte {first_byte}"
            );
        }
    }

    #[tokio::test]
    async fn thai_outline_titles_are_utf16() {
        let dir = tempdir().unwrap();
        let pdf = dir.path().join("outline.pdf");
        PdfCreateTool
            .call(json!({
                "path": pdf.to_string_lossy(),
                "content": "# บทที่ 1 — ทดสอบ\n\nเนื้อหา\n\n# Chapter 2\n\nbody\n",
                "page_break_h1": true
            }))
            .await
            .unwrap();
        let doc = lopdf::Document::load(&pdf).unwrap();
        let mut thai_seen = false;
        for (_, obj) in doc.objects.iter() {
            let Ok(dict) = obj.as_dict() else { continue };
            if !dict.has(b"Dest") {
                continue;
            }
            if let Ok(lopdf::Object::String(t, _)) = dict.get(b"Title") {
                if t.starts_with(&[0xFE, 0xFF]) {
                    // Decode UTF-16BE and confirm the Thai survived.
                    let units: Vec<u16> = t[2..]
                        .chunks_exact(2)
                        .map(|c| u16::from_be_bytes([c[0], c[1]]))
                        .collect();
                    let decoded = String::from_utf16(&units).unwrap();
                    if decoded.contains("บทที่") {
                        thai_seen = true;
                    }
                }
            }
        }
        assert!(thai_seen, "Thai outline title must be UTF-16BE with BOM");
    }

    #[test]
    fn normalize_path_resolves_parent_dirs() {
        assert_eq!(
            normalize_path(Path::new("/a/b/../images/x.jpg")),
            PathBuf::from("/a/images/x.jpg")
        );
        assert_eq!(
            normalize_path(Path::new("chapters/../images/x.jpg")),
            PathBuf::from("images/x.jpg")
        );
    }

    #[test]
    fn symbols_the_fonts_lack_are_replaced_not_boxed() {
        let out = replace_missing_glyphs("7.94 → 13.57 ล้าน ⚠ เทียบไม่ได้ ✅ ok 🚀");
        assert!(out.contains("7.94 -> 13.57 ล้าน"), "{out}");
        assert!(out.contains("(!) เทียบไม่ได้"), "{out}");
        assert!(
            !out.contains('🚀') && !out.contains('→') && !out.contains('⚠'),
            "{out}"
        );
        assert_eq!(
            replace_missing_glyphs("ภาษาไทย ABC 1,234 บาท"),
            "ภาษาไทย ABC 1,234 บาท"
        );
    }
}
