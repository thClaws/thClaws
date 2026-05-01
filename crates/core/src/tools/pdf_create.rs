//! `PdfCreate` — render a markdown string to a PDF file with embedded
//! Noto Sans + Noto Sans Thai fonts so Thai text renders without a
//! system-font dependency. Per-codepoint font selection (Thai block
//! U+0E00–U+0E7F → Thai font, else Latin font) handled at the run level
//! via printpdf's `use_text`, which takes a font reference per call.
//!
//! Layout is intentionally simple: paragraphs / headings (H1–H4) /
//! bullet lists / fenced code blocks. Width estimation is glyph-naive
//! (a per-script multiplier on font size) — good enough for reports
//! and notes; precise typography would require glyph metrics from the
//! font's hmtx table, which is a future enhancement.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use printpdf::{
    IndirectFontRef, Mm, PdfDocument, PdfDocumentReference, PdfLayerIndex, PdfPageIndex,
};
use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use serde_json::{json, Value};
use std::io::BufWriter;
use std::path::Path;

const NOTO_SANS_BYTES: &[u8] = include_bytes!("../../resources/fonts/NotoSans-Regular.ttf");
const NOTO_SANS_THAI_BYTES: &[u8] =
    include_bytes!("../../resources/fonts/NotoSansThai-Regular.ttf");

const PT_TO_MM: f32 = 0.3528;
const DEFAULT_FONT_SIZE_PT: f32 = 11.0;
const MARGIN_MM: f32 = 20.0;
const PARAGRAPH_GAP_MM: f32 = 3.0;

pub struct PdfCreateTool;

#[async_trait]
impl Tool for PdfCreateTool {
    fn name(&self) -> &'static str {
        "PdfCreate"
    }

    fn description(&self) -> &'static str {
        "Render a markdown string to a PDF file. Embedded Noto Sans + Noto \
         Sans Thai fonts so Thai text renders without a system-font \
         dependency. Supports headings (H1–H4), paragraphs, bullet lists, \
         and fenced code blocks. Page size A4 (default), Letter, or Legal."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":      {"type": "string", "description": "Output PDF path. Parent directories are created if missing."},
                "content":   {"type": "string", "description": "Markdown content to render."},
                "title":     {"type": "string", "description": "PDF document title (metadata). Optional — defaults to the file stem."},
                "font_size": {"type": "integer", "description": "Body font size in points. Default 11.", "minimum": 6, "maximum": 72},
                "page_size": {"type": "string", "enum": ["A4", "Letter", "Legal"], "description": "Default A4."}
            },
            "required": ["path", "content"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_path = req_str(&input, "path")?;
        let validated = crate::sandbox::Sandbox::check_write(raw_path)?;
        let content = req_str(&input, "content")?;

        let title = input
            .get("title")
            .and_then(|v| v.as_str())
            .map(String::from)
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

        let (page_w_mm, page_h_mm) = match input.get("page_size").and_then(|v| v.as_str()) {
            Some("Letter") => (215.9, 279.4),
            Some("Legal") => (215.9, 355.6),
            _ => (210.0, 297.0), // A4
        };

        if let Some(parent) = Path::new(&*validated.to_string_lossy()).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::Tool(format!("mkdir {}: {}", parent.display(), e)))?;
            }
        }

        let path_clone = validated.clone();
        let title_clone = title.clone();
        let content_clone = content.to_string();

        let pages = tokio::task::spawn_blocking(move || -> Result<usize> {
            render_pdf(
                &path_clone,
                &title_clone,
                &content_clone,
                font_size,
                page_w_mm,
                page_h_mm,
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

fn render_pdf(
    path: &Path,
    title: &str,
    content: &str,
    body_pt: f32,
    page_w_mm: f32,
    page_h_mm: f32,
) -> Result<usize> {
    let (doc, first_page, first_layer) =
        PdfDocument::new(title, Mm(page_w_mm), Mm(page_h_mm), "Layer 1");

    let latin_font = doc
        .add_external_font(NOTO_SANS_BYTES)
        .map_err(|e| Error::Tool(format!("embed Noto Sans: {e}")))?;
    let thai_font = doc
        .add_external_font(NOTO_SANS_THAI_BYTES)
        .map_err(|e| Error::Tool(format!("embed Noto Sans Thai: {e}")))?;

    let mut renderer = PdfRenderer {
        doc,
        current_page: first_page,
        current_layer: first_layer,
        latin_font,
        thai_font,
        page_w_mm,
        page_h_mm,
        cursor_y_mm: MARGIN_MM,
        body_pt,
        page_count: 1,
    };

    render_markdown(&mut renderer, content);

    let pages_written = renderer.page_count;
    let file = std::fs::File::create(path)
        .map_err(|e| Error::Tool(format!("create {}: {}", path.display(), e)))?;
    renderer
        .doc
        .save(&mut BufWriter::new(file))
        .map_err(|e| Error::Tool(format!("save PDF: {e}")))?;
    Ok(pages_written)
}

struct PdfRenderer {
    doc: PdfDocumentReference,
    current_page: PdfPageIndex,
    current_layer: PdfLayerIndex,
    latin_font: IndirectFontRef,
    thai_font: IndirectFontRef,
    page_w_mm: f32,
    page_h_mm: f32,
    /// Vertical position, measured DOWN from the top of the page.
    /// printpdf's coordinate origin is bottom-left, so we convert at draw
    /// time. Tracking from the top keeps the layout math obvious.
    cursor_y_mm: f32,
    body_pt: f32,
    page_count: usize,
}

impl PdfRenderer {
    fn new_page(&mut self) {
        let (page, layer) = self
            .doc
            .add_page(Mm(self.page_w_mm), Mm(self.page_h_mm), "Layer 1");
        self.current_page = page;
        self.current_layer = layer;
        self.cursor_y_mm = MARGIN_MM;
        self.page_count += 1;
    }

    fn ensure_room(&mut self, line_height_mm: f32) {
        if self.cursor_y_mm + line_height_mm > self.page_h_mm - MARGIN_MM {
            self.new_page();
        }
    }

    /// Emit one line of text at the current cursor, splitting into mixed-font
    /// runs (Thai vs. Latin). Advances cursor_y by the line height.
    fn render_line(&mut self, text: &str, pt: f32) {
        let line_height_mm = pt * 1.4 * PT_TO_MM;
        self.ensure_room(line_height_mm);

        let baseline_from_top_mm = self.cursor_y_mm + pt * PT_TO_MM;
        let baseline_from_bottom_mm = self.page_h_mm - baseline_from_top_mm;
        let layer = self
            .doc
            .get_page(self.current_page)
            .get_layer(self.current_layer);

        let mut x_mm = MARGIN_MM;
        for (is_thai, run) in split_runs(text) {
            if run.is_empty() {
                continue;
            }
            let font = if is_thai {
                &self.thai_font
            } else {
                &self.latin_font
            };
            layer.use_text(&run, pt, Mm(x_mm), Mm(baseline_from_bottom_mm), font);
            x_mm += run_width_mm(&run, pt);
        }
        self.cursor_y_mm += line_height_mm;
    }

    /// Wrap a logical block of text and emit it line by line. Greedy
    /// algorithm: accumulate chars; on overflow, break at the most recent
    /// space, else mid-character (Thai has no word-spaces).
    fn wrap_and_render(&mut self, text: &str, pt: f32, indent_mm: f32) {
        if text.is_empty() {
            return;
        }
        let max_width_mm = self.page_w_mm - 2.0 * MARGIN_MM - indent_mm;

        let mut line = String::new();
        let mut line_width_mm = 0.0_f32;
        let mut last_space_at: Option<(usize, f32)> = None;

        for c in text.chars() {
            let cw = char_width_mm(c, pt);
            if line_width_mm + cw > max_width_mm && !line.is_empty() {
                let (head, tail, tail_width) =
                    if let Some((idx, w_before_space)) = last_space_at.take() {
                        let head: String = line.chars().take(idx).collect();
                        let tail: String = line.chars().skip(idx + 1).collect();
                        let tail_width = line_width_mm - w_before_space - char_width_mm(' ', pt);
                        (head, tail, tail_width)
                    } else {
                        (line.clone(), String::new(), 0.0)
                    };
                let to_render = if indent_mm > 0.0 {
                    format!(
                        "{}{}",
                        " ".repeat((indent_mm / char_width_mm(' ', pt)) as usize),
                        head
                    )
                } else {
                    head
                };
                self.render_line(&to_render, pt);
                line = tail;
                line_width_mm = tail_width;
            }
            if c == ' ' {
                last_space_at = Some((line.chars().count(), line_width_mm));
            }
            line.push(c);
            line_width_mm += cw;
        }
        if !line.is_empty() {
            let to_render = if indent_mm > 0.0 {
                format!(
                    "{}{}",
                    " ".repeat((indent_mm / char_width_mm(' ', pt)) as usize),
                    line
                )
            } else {
                line
            };
            self.render_line(&to_render, pt);
        }
    }

    fn vertical_gap(&mut self, mm: f32) {
        self.cursor_y_mm += mm;
    }
}

fn render_markdown(r: &mut PdfRenderer, content: &str) {
    let parser = Parser::new(content);

    let mut buf = String::new();
    let mut current_pt = r.body_pt;
    let mut indent_mm = 0.0_f32;
    let mut code_block = false;

    let flush = |r: &mut PdfRenderer, buf: &mut String, pt: f32, indent_mm: f32| {
        if buf.is_empty() {
            return;
        }
        r.wrap_and_render(buf, pt, indent_mm);
        buf.clear();
    };

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                flush(r, &mut buf, current_pt, indent_mm);
                current_pt = match level {
                    HeadingLevel::H1 => r.body_pt * 1.8,
                    HeadingLevel::H2 => r.body_pt * 1.5,
                    HeadingLevel::H3 => r.body_pt * 1.25,
                    _ => r.body_pt * 1.1,
                };
                r.vertical_gap(PARAGRAPH_GAP_MM);
            }
            Event::End(TagEnd::Heading(_)) => {
                flush(r, &mut buf, current_pt, indent_mm);
                r.vertical_gap(PARAGRAPH_GAP_MM * 0.7);
                current_pt = r.body_pt;
            }
            Event::Start(Tag::Paragraph) => {
                buf.clear();
            }
            Event::End(TagEnd::Paragraph) => {
                flush(r, &mut buf, current_pt, indent_mm);
                r.vertical_gap(PARAGRAPH_GAP_MM);
            }
            Event::Start(Tag::List(_)) => {
                indent_mm = 6.0;
            }
            Event::End(TagEnd::List(_)) => {
                indent_mm = 0.0;
                r.vertical_gap(PARAGRAPH_GAP_MM * 0.5);
            }
            Event::Start(Tag::Item) => {
                buf.clear();
                buf.push_str("• ");
            }
            Event::End(TagEnd::Item) => {
                flush(r, &mut buf, current_pt, indent_mm);
            }
            Event::Start(Tag::CodeBlock(_)) => {
                flush(r, &mut buf, current_pt, indent_mm);
                code_block = true;
                current_pt = r.body_pt * 0.92;
                r.vertical_gap(PARAGRAPH_GAP_MM * 0.5);
            }
            Event::End(TagEnd::CodeBlock) => {
                flush(r, &mut buf, current_pt, indent_mm);
                code_block = false;
                current_pt = r.body_pt;
                r.vertical_gap(PARAGRAPH_GAP_MM);
            }
            Event::Text(s) => {
                if code_block {
                    // Code blocks: render each newline as its own line so
                    // formatting is preserved (no greedy wrap inside code).
                    for line in s.split('\n') {
                        r.render_line(line, current_pt);
                    }
                } else {
                    buf.push_str(&s);
                }
            }
            Event::Code(s) => {
                buf.push_str(&s);
            }
            Event::SoftBreak => buf.push(' '),
            Event::HardBreak => {
                flush(r, &mut buf, current_pt, indent_mm);
            }
            _ => {}
        }
    }
    flush(r, &mut buf, current_pt, indent_mm);
}

/// Split a string into runs that share a font (Thai vs. Latin/everything
/// else). Each run becomes its own `use_text` call.
fn split_runs(s: &str) -> Vec<(bool, String)> {
    let mut out: Vec<(bool, String)> = Vec::new();
    for c in s.chars() {
        let thai = is_thai(c);
        match out.last_mut() {
            Some(last) if last.0 == thai => last.1.push(c),
            _ => out.push((thai, c.to_string())),
        }
    }
    out
}

fn is_thai(c: char) -> bool {
    matches!(c, '\u{0E00}'..='\u{0E7F}')
}

/// Thai combining marks (vowels above/below, tone marks) carry no
/// horizontal advance — they stack on the preceding consonant. Treating
/// them as zero-width keeps width estimates roughly accurate.
fn is_thai_combining_mark(c: char) -> bool {
    matches!(c,
        '\u{0E31}' |
        '\u{0E34}'..='\u{0E3A}' |
        '\u{0E47}'..='\u{0E4E}'
    )
}

fn char_width_mm(c: char, pt: f32) -> f32 {
    if is_thai_combining_mark(c) {
        return 0.0;
    }
    let factor = if is_thai(c) {
        0.6
    } else if c == ' ' {
        0.28
    } else {
        0.5
    };
    pt * factor * PT_TO_MM
}

fn run_width_mm(text: &str, pt: f32) -> f32 {
    text.chars().map(|c| char_width_mm(c, pt)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Single test driving the full path so we don't race Sandbox::init,
    /// which is process-global and can't be set per-test in parallel.
    /// Skipping Sandbox setup is fine because `Sandbox::check_write` is a
    /// no-op when the sandbox isn't initialized — matches WriteTool tests.
    #[tokio::test]
    async fn writes_pdf_with_thai_and_latin() {
        let dir = tempdir().unwrap();
        let simple = dir.path().join("hello.pdf");
        let thai = dir.path().join("thai.pdf");

        let msg = PdfCreateTool
            .call(json!({
                "path": simple.to_string_lossy(),
                "content": "# Hello\n\nThis is a paragraph."
            }))
            .await
            .unwrap();
        assert!(msg.contains("Wrote PDF to"));
        let bytes = std::fs::read(&simple).unwrap();
        assert!(bytes.starts_with(b"%PDF-"), "output should be a PDF");

        let _ = PdfCreateTool
            .call(json!({
                "path": thai.to_string_lossy(),
                "content": "# สวัสดี\n\nนี่คือเอกสารทดสอบ Thai-Latin mixed text กลางย่อหน้า"
            }))
            .await
            .unwrap();
        assert!(std::fs::metadata(&thai).unwrap().len() > 1000);
    }

    #[test]
    fn run_split_mixes() {
        let runs = split_runs("Hello สวัสดี world");
        assert_eq!(runs.len(), 3);
        assert!(!runs[0].0);
        assert!(runs[1].0);
        assert!(!runs[2].0);
    }

    #[test]
    fn combining_marks_zero_width() {
        // 'อ' has width; 'ิ' (U+0E34) is a combining mark — zero width.
        assert!(char_width_mm('อ', 12.0) > 0.0);
        assert_eq!(char_width_mm('\u{0E34}', 12.0), 0.0);
    }
}
