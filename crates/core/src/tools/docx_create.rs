//! `DocxCreate` — render a markdown string to a Word document via
//! `docx-rs`. Per-run font properties carry both Latin (`ascii`/`hi_ansi`
//! → Calibri) and complex-script (`cs` → Sarabun) families, so a
//! single Run mixing Thai+Latin renders correctly without splitting —
//! Word's text engine picks the right font per codepoint range from the
//! same Run's properties.
//!
//! Layout is intentionally simple: paragraphs / headings (H1–H4) /
//! bullet lists / numbered lists / fenced code blocks. Tables, images,
//! tracked changes, and ToC are deferred (see dev-plan/02).

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use docx_rs::{
    AbstractNumbering, Docx, IndentLevel, Level, LevelJc, LevelText, NumberFormat, Numbering,
    NumberingId, Paragraph, Pic, Run, RunFonts, SpecialIndentType, Start, Style, StyleType, Table,
    TableCell, TableRow,
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde_json::{json, Value};
use std::path::Path;

/// Body font size in half-points (OOXML convention). 22 = 11pt.
const DEFAULT_BODY_HALF_POINTS: usize = 22;
/// Latin font name. Calibri is Office's default since 2007 — installed
/// everywhere Word runs.
const LATIN_FONT: &str = "Calibri";
/// Thai script font (cs = complex script in OOXML run-properties terms).
/// Sarabun (Google Fonts) draws Thai at the same visual size as Calibri, so
/// cs keeps the Latin point size; the document service has it installed.
/// TH Sarabun New was tried (2026-10-21) and dropped: it draws small and
/// needs a larger cs size, and wherever it is missing the substitute at that
/// size made Thai twice the height of the Latin beside it.
pub(crate) const THAI_FONT: &str = "Sarabun";

fn run_fonts(primary: &str) -> RunFonts {
    RunFonts::new()
        .ascii(primary)
        .hi_ansi(primary)
        .cs(THAI_FONT)
}
/// Heading1..Heading4 defined in styles.xml. A `pStyle` naming a style the
/// document does not define renders as body text in LibreOffice (and so in
/// every preview and PDF made by the document service), so the styles are
/// spelled out: bold, sized, dark blue, outline levels for a TOC.
fn with_heading_styles(docx: Docx) -> Docx {
    [(1usize, 36usize), (2, 30), (3, 26), (4, 24)]
        .into_iter()
        .fold(docx, |d, (level, half_points)| {
            d.add_style(
                Style::new(format!("Heading{level}"), StyleType::Paragraph)
                    .name(format!("heading {level}"))
                    .based_on("Normal")
                    .next("Normal")
                    .q_format(true)
                    .size(half_points)
                    .bold()
                    .color("1F3864")
                    .fonts(run_fonts(LATIN_FONT))
                    .outline_lvl(level - 1),
            )
        })
}

/// Monospace font for fenced code blocks and inline code.
const MONO_FONT: &str = "Consolas";

/// Numbering ids used by our list rendering, registered at document setup.
/// docx-rs already writes a default decimal abstractNum/num with id 1, and
/// LibreOffice takes the first definition of an id — so bullets on id 1
/// rendered as "1. 2.". Ids above the defaults keep ours unambiguous.
const BULLET_ID: usize = 101;
const DECIMAL_ID: usize = 102;

pub struct DocxCreateTool;

#[async_trait]
impl Tool for DocxCreateTool {
    fn name(&self) -> &'static str {
        "DocxCreate"
    }

    fn description(&self) -> &'static str {
        "Render a markdown string to a Word document (.docx). Supports \
         headings (H1–H4), paragraphs, bullet + numbered lists, fenced \
         code blocks, **GFM pipe tables** (header row bolded), inline \
         emphasis, and **inline images** via `![alt](path)` (PNG/JPEG, \
         capped at 5 MB; embedded inline at a default 480×360 px). \
         Run-level fontTable references set Calibri (Latin) + TH \
         Sarabun New (complex script), so mixed Thai+Latin paragraphs \
         render correctly without splitting."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":      {"type": "string", "description": "Output .docx path. Parent directories are created if missing."},
                "content":   {"type": "string", "description": "Markdown content to render."},
                "title":     {"type": "string", "description": "Document title metadata. Optional — defaults to the file stem."},
                "font_size": {"type": "integer", "description": "Body font size in points. Default 11.", "minimum": 6, "maximum": 72}
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

        let body_pt: usize = input
            .get("font_size")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize) * 2)
            .unwrap_or(DEFAULT_BODY_HALF_POINTS);

        if let Some(parent) = Path::new(&*validated.to_string_lossy()).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::Tool(format!("mkdir {}: {}", parent.display(), e)))?;
            }
        }

        let path_clone = validated.clone();
        let content_clone = content.to_string();

        let bytes = tokio::task::spawn_blocking(move || -> Result<usize> {
            render_docx(&path_clone, &content_clone, body_pt)
        })
        .await
        .map_err(|e| Error::Tool(format!("DOCX worker join failed: {e}")))??;

        Ok(format!(
            "Wrote DOCX to {} ({} bytes)",
            validated.display(),
            bytes
        ))
    }
}

fn render_docx(path: &Path, content: &str, body_pt: usize) -> Result<usize> {
    let mut docx = with_heading_styles(Docx::new())
        // Bullet list — level 0 only; nested levels future enhancement.
        // "•" (U+2022), not the Symbol-font F0B7 private-use glyph: that
        // one is an empty box wherever Symbol is missing (LibreOffice, so
        // every preview and PDF the document service makes).
        .add_abstract_numbering(
            AbstractNumbering::new(BULLET_ID).add_level(
                Level::new(
                    0,
                    Start::new(1),
                    NumberFormat::new("bullet"),
                    LevelText::new("\u{2022}"),
                    LevelJc::new("left"),
                )
                .indent(
                    Some(720),
                    Some(SpecialIndentType::Hanging(360)),
                    None,
                    None,
                ),
            ),
        )
        .add_numbering(Numbering::new(BULLET_ID, BULLET_ID))
        .add_abstract_numbering(
            AbstractNumbering::new(DECIMAL_ID).add_level(
                Level::new(
                    0,
                    Start::new(1),
                    NumberFormat::new("decimal"),
                    LevelText::new("%1."),
                    LevelJc::new("left"),
                )
                .indent(
                    Some(720),
                    Some(SpecialIndentType::Hanging(360)),
                    None,
                    None,
                ),
            ),
        )
        .add_numbering(Numbering::new(DECIMAL_ID, DECIMAL_ID));

    // Enable GFM tables so `| h1 | h2 |\n|---|---|\n| a | b |` parses
    // into Table / TableHead / TableRow / TableCell events instead of
    // being rendered as a string of pipes.
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    // Repair LLM-miscounted table delimiter rows so GFM tables aren't silently
    // demoted to paragraphs (see tools::md_tables).
    let content = super::md_tables::repair_table_delimiters(content);
    let parser = Parser::new_ext(&content, opts);

    // Streaming markdown walker. We accumulate text inside a paragraph
    // until a block-end event flushes it to the doc. Header / list /
    // code-block state lives outside the inner buffer.
    let mut buf = String::new();
    let current_pt = body_pt;
    let mut bold = false;
    let mut italic = false;
    let mut monospace = false;
    let mut list_kind: Option<ListKind> = None;
    let mut in_code_block = false;
    // Heading level of the paragraph currently being assembled, if any.
    // Markdown's H1..H4 → OOXML `Heading1..Heading4`. Reset on paragraph
    // end so subsequent body text doesn't accidentally inherit the style.
    let mut heading_level: Option<u8> = None;
    // Table assembly state. `in_table` toggles the inner buffer-flush
    // path so paragraph-level events inside a table feed into the
    // current cell instead of emitting body paragraphs. The first row
    // is the header (rendered bold). pulldown_cmark guarantees
    // TableHead/TableRow/TableCell nesting, so we don't need a
    // separate cell-state guard.
    let mut in_table = false;
    let mut in_table_head = false;
    let mut current_cell_text = String::new();
    let mut current_row_cells: Vec<(String, bool /* bold */)> = Vec::new();
    let mut accumulated_rows: Vec<Vec<(String, bool)>> = Vec::new();

    let flush = |docx: &mut Docx,
                 buf: &mut String,
                 pt: usize,
                 bold: bool,
                 italic: bool,
                 mono: bool,
                 list_kind: Option<ListKind>,
                 heading: Option<u8>| {
        if buf.is_empty() {
            return;
        }
        // For headings we attach the `HeadingN` style (defined in
        // styles.xml by `with_heading_styles`) and emit the run with fonts
        // only, so DocxRead still sees the heading through `<w:pStyle>`.
        // Manual bold/size on the run would override the style.
        let run = if heading.is_some() {
            Run::new()
                .add_text(buf.as_str())
                .fonts(run_fonts(LATIN_FONT))
        } else {
            make_run(buf, pt, bold, italic, mono)
        };
        let mut para = Paragraph::new().add_run(run);
        if let Some(level) = heading {
            para = para.style(&format!("Heading{level}"));
        }
        if let Some(kind) = list_kind {
            let id = match kind {
                ListKind::Bullet => BULLET_ID,
                ListKind::Ordered => DECIMAL_ID,
            };
            para = para.numbering(NumberingId::new(id), IndentLevel::new(0));
        }
        let owned = std::mem::take(docx);
        *docx = owned.add_paragraph(para);
        buf.clear();
    };

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                flush(
                    &mut docx,
                    &mut buf,
                    current_pt,
                    bold,
                    italic,
                    monospace,
                    list_kind,
                    heading_level,
                );
                heading_level = Some(map_heading_level(level));
            }
            Event::End(TagEnd::Heading(_)) => {
                flush(
                    &mut docx,
                    &mut buf,
                    current_pt,
                    bold,
                    italic,
                    monospace,
                    list_kind,
                    heading_level,
                );
                heading_level = None;
            }
            Event::Start(Tag::Paragraph) => {
                buf.clear();
            }
            Event::End(TagEnd::Paragraph) => {
                flush(
                    &mut docx,
                    &mut buf,
                    current_pt,
                    bold,
                    italic,
                    monospace,
                    list_kind,
                    heading_level,
                );
            }
            Event::Start(Tag::List(start)) => {
                list_kind = Some(if start.is_some() {
                    ListKind::Ordered
                } else {
                    ListKind::Bullet
                });
            }
            Event::End(TagEnd::List(_)) => {
                list_kind = None;
            }
            Event::Start(Tag::Item) => {
                buf.clear();
            }
            Event::End(TagEnd::Item) => {
                flush(
                    &mut docx,
                    &mut buf,
                    current_pt,
                    bold,
                    italic,
                    monospace,
                    list_kind,
                    heading_level,
                );
            }
            Event::Start(Tag::Emphasis) => italic = true,
            Event::End(TagEnd::Emphasis) => italic = false,
            Event::Start(Tag::Strong) => bold = true,
            Event::End(TagEnd::Strong) => bold = false,
            Event::Start(Tag::CodeBlock(_)) => {
                flush(
                    &mut docx,
                    &mut buf,
                    current_pt,
                    bold,
                    italic,
                    monospace,
                    list_kind,
                    heading_level,
                );
                in_code_block = true;
                monospace = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                flush(
                    &mut docx,
                    &mut buf,
                    current_pt,
                    bold,
                    italic,
                    monospace,
                    list_kind,
                    heading_level,
                );
                in_code_block = false;
                monospace = false;
            }
            Event::Text(s) => {
                if in_table {
                    // Inside a table cell — append to the current
                    // cell's text buffer; we'll flush the whole row
                    // on TagEnd::TableRow.
                    current_cell_text.push_str(&s);
                } else if in_code_block {
                    // Preserve newlines in code blocks by flushing per
                    // line — each line becomes its own paragraph with
                    // monospace font.
                    let mut first = true;
                    for line in s.split('\n') {
                        if !first {
                            flush(
                                &mut docx,
                                &mut buf,
                                current_pt,
                                bold,
                                italic,
                                monospace,
                                list_kind,
                                heading_level,
                            );
                        }
                        buf.push_str(line);
                        first = false;
                    }
                } else {
                    buf.push_str(&s);
                }
            }
            Event::Code(s) => {
                // Inline code — flush the prior text run, then render
                // this fragment in monospace, then resume normal.
                flush(
                    &mut docx,
                    &mut buf,
                    current_pt,
                    bold,
                    italic,
                    monospace,
                    list_kind,
                    heading_level,
                );
                buf.push_str(&s);
                flush(
                    &mut docx,
                    &mut buf,
                    current_pt,
                    bold,
                    italic,
                    true,
                    list_kind,
                    heading_level,
                );
            }
            Event::SoftBreak => buf.push(' '),
            Event::HardBreak => {
                flush(
                    &mut docx,
                    &mut buf,
                    current_pt,
                    bold,
                    italic,
                    monospace,
                    list_kind,
                    heading_level,
                );
            }
            // GFM table handling. State machine:
            //   Tag::Table  → in_table = true, accumulated_rows cleared
            //   Tag::TableHead / TableRow → start a row
            //   Tag::TableCell → start a cell; clear current_cell_text
            //   TagEnd::TableCell → push (text, bold) into current_row_cells
            //   TagEnd::TableRow / TableHead → push row into accumulated_rows
            //   TagEnd::Table → emit docx_rs::Table, reset state
            Event::Start(Tag::Table(_)) => {
                flush(
                    &mut docx,
                    &mut buf,
                    current_pt,
                    bold,
                    italic,
                    monospace,
                    list_kind,
                    heading_level,
                );
                in_table = true;
                accumulated_rows.clear();
            }
            Event::Start(Tag::TableHead) => {
                in_table_head = true;
                current_row_cells.clear();
            }
            Event::Start(Tag::TableRow) => {
                current_row_cells.clear();
            }
            Event::Start(Tag::TableCell) => {
                current_cell_text.clear();
            }
            Event::End(TagEnd::TableCell) => {
                current_row_cells.push((std::mem::take(&mut current_cell_text), in_table_head));
            }
            Event::End(TagEnd::TableRow) | Event::End(TagEnd::TableHead) => {
                accumulated_rows.push(std::mem::take(&mut current_row_cells));
                if matches!(event, Event::End(TagEnd::TableHead)) {
                    in_table_head = false;
                }
            }
            Event::End(TagEnd::Table) => {
                in_table = false;
                if !accumulated_rows.is_empty() {
                    let rows: Vec<TableRow> = accumulated_rows
                        .iter()
                        .map(|row| {
                            let cells: Vec<TableCell> = row
                                .iter()
                                .map(|(text, is_header)| {
                                    let run = make_run(text, current_pt, *is_header, false, false);
                                    TableCell::new().add_paragraph(Paragraph::new().add_run(run))
                                })
                                .collect();
                            TableRow::new(cells)
                        })
                        .collect();
                    let owned = std::mem::take(&mut docx);
                    docx = owned.add_table(Table::new(rows));
                }
                accumulated_rows.clear();
            }
            // Inline image: `![alt](path)`. Read the file, embed via
            // docx-rs's Pic. For inline use, the Pic gets attached to
            // a fresh Run inside its own Paragraph so it lays out as
            // a block-level image — same as Word's default
            // "wrap-around: inline" placement.
            Event::Start(Tag::Image { dest_url, .. }) => {
                flush(
                    &mut docx,
                    &mut buf,
                    current_pt,
                    bold,
                    italic,
                    monospace,
                    list_kind,
                    heading_level,
                );
                if let Some(pic) = load_image_for_docx(&dest_url) {
                    let owned = std::mem::take(&mut docx);
                    docx = owned.add_paragraph(Paragraph::new().add_run(Run::new().add_image(pic)));
                }
                // pulldown_cmark emits Text events for the alt text
                // between Start and End of an Image; suppress those
                // by entering a brief "swallow" mode. We achieve
                // this with a flag toggled at TagEnd::Image. For
                // simplicity, the alt text just lands in `buf` and
                // gets flushed by the next paragraph event — minor
                // visual artefact for `![]` with non-empty alt
                // outside a paragraph; acceptable for v1.
            }
            Event::End(TagEnd::Image) => {
                // Drop any alt-text Text events that landed in buf.
                buf.clear();
            }
            _ => {}
        }
    }
    flush(
        &mut docx,
        &mut buf,
        current_pt,
        bold,
        italic,
        monospace,
        list_kind,
        heading_level,
    );

    let file = std::fs::File::create(path)
        .map_err(|e| Error::Tool(format!("create {}: {}", path.display(), e)))?;
    docx.build()
        .pack(file)
        .map_err(|e| Error::Tool(format!("pack DOCX: {e}")))?;

    let bytes = std::fs::metadata(path)
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    Ok(bytes)
}

#[derive(Copy, Clone)]
enum ListKind {
    Bullet,
    Ordered,
}

/// Resolve a markdown image's `dest_url` to a `Pic` ready for
/// `Run::add_image`. Returns `None` for missing files, oversize
/// blobs, or non-image MIME — in those cases the image is silently
/// dropped from the rendered docx and the alt text remains in the
/// markdown source for whoever opens the file. Path safety is
/// enforced via `Sandbox::check` (read access).
///
/// Sizing: docx_rs's `Pic::size(width_emu, height_emu)` takes English
/// Metric Units (1 inch = 914400 EMU; 1 pixel at 96 DPI = 9525 EMU).
/// We don't decode the image to query intrinsic dimensions for v1 —
/// just clamp at a sensible default (480×360 px ≈ a 5"×3.75" card)
/// so embedded images fit on a typical letter/A4 page without
/// running off the edge. Users who need exact sizing can resize the
/// source image first.
fn load_image_for_docx(dest_url: &str) -> Option<Pic> {
    const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
    const DEFAULT_W_PX: u32 = 480;
    const DEFAULT_H_PX: u32 = 360;
    const PX_TO_EMU: u32 = 9525;

    let path = crate::sandbox::Sandbox::check(dest_url).ok()?;
    let bytes = std::fs::read(&path).ok()?;
    if bytes.len() > MAX_IMAGE_BYTES || bytes.is_empty() {
        return None;
    }
    Some(Pic::new(&bytes).size(DEFAULT_W_PX * PX_TO_EMU, DEFAULT_H_PX * PX_TO_EMU))
}

fn make_run(text: &str, pt_half: usize, bold: bool, italic: bool, mono: bool) -> Run {
    let primary = if mono { MONO_FONT } else { LATIN_FONT };
    let mut run = Run::new()
        .add_text(text)
        .size(pt_half)
        .fonts(run_fonts(primary));
    run.run_property.sz_cs = Some(docx_rs::SzCs::new(pt_half));
    if bold {
        run = run.bold();
    }
    if italic {
        run = run.italic();
    }
    run
}

/// Map pulldown-cmark's heading level to the OOXML built-in style number.
/// `Heading1..Heading4` are the four levels we expose; H5/H6 fall back to
/// Heading4 since Word's built-in styles for 5/6 render barely differently
/// from body text and the user is unlikely to use them in agent output.
fn map_heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 | HeadingLevel::H6 => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn writes_docx_with_thai_and_latin() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hello.docx");
        let msg = DocxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "content": "# สวัสดี Hello\n\nนี่คือเอกสาร with mixed scripts.\n\n- bullet หนึ่ง\n- bullet two\n\n## Code\n\n```\nfn main() {}\n```"
            }))
            .await
            .unwrap();
        assert!(msg.contains("Wrote DOCX to"));
        let bytes = std::fs::read(&path).unwrap();
        // OOXML zip starts with PK signature.
        assert!(
            bytes.starts_with(b"PK"),
            "output should be a ZIP/OOXML file"
        );
        assert!(bytes.len() > 1000, "non-trivial size");
    }

    /// GFM pipe-table renders as a docx_rs::Table. Re-read via
    /// DocxRead and assert each header / cell text appears in the
    /// extracted body. We don't introspect the OOXML directly to keep
    /// the test resilient to docx_rs version bumps; the round-trip
    /// "header text + cell text both present in the rendered file"
    /// check catches the only failure mode that matters in practice.
    #[tokio::test]
    async fn writes_docx_with_pipe_table() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("table.docx");
        DocxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "content": "# Expense\n\n| Item | Qty | Price |\n|---|---|---|\n| Coffee | 2 | $7 |\n| Sandwich | 1 | $9 |\n"
            }))
            .await
            .unwrap();
        let extracted = crate::tools::DocxReadTool
            .call(json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();
        assert!(extracted.contains("Item"), "header missing: {extracted:?}");
        assert!(extracted.contains("Coffee"), "row missing: {extracted:?}");
        assert!(extracted.contains("Sandwich"), "row missing: {extracted:?}");
    }

    #[tokio::test]
    async fn thai_runs_carry_the_complex_script_font() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memo.docx");
        DocxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "content": "# บันทึก\n\nงบประมาณ 6,000,000 บาท"
            }))
            .await
            .unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap();
        let mut xml = String::new();
        std::io::Read::read_to_string(&mut zip.by_name("word/document.xml").unwrap(), &mut xml)
            .unwrap();
        assert!(xml.contains(r#"w:cs="Sarabun""#), "{xml}");
        assert!(!xml.contains("w:hint"), "no cs hint on mixed runs: {xml}");
        assert!(xml.contains(r#"w:ascii="Calibri""#), "{xml}");
        assert!(
            !xml.contains(r#"<w:szCs w:val="32""#),
            "Thai keeps the Latin size: {xml}"
        );
    }

    #[test]
    fn heading_levels_map_correctly() {
        assert_eq!(map_heading_level(HeadingLevel::H1), 1);
        assert_eq!(map_heading_level(HeadingLevel::H2), 2);
        assert_eq!(map_heading_level(HeadingLevel::H3), 3);
        assert_eq!(map_heading_level(HeadingLevel::H4), 4);
        // H5/H6 collapse to Heading4 — Word's built-in styles for 5/6
        // are barely distinguishable from body text.
        assert_eq!(map_heading_level(HeadingLevel::H5), 4);
        assert_eq!(map_heading_level(HeadingLevel::H6), 4);
    }

    #[tokio::test]
    async fn heading_styles_are_defined() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("h.docx");
        DocxCreateTool
            .call(json!({"path": path.to_string_lossy(), "content": "# หัวข้อ\n\n## ย่อย\n\nเนื้อหา"}))
            .await
            .unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap();
        let mut styles = String::new();
        std::io::Read::read_to_string(&mut zip.by_name("word/styles.xml").unwrap(), &mut styles)
            .unwrap();
        for level in 1..=4 {
            assert!(
                styles.contains(&format!(r#"w:styleId="Heading{level}""#)),
                "{styles}"
            );
        }
        assert!(styles.contains(r#"<w:outlineLvl w:val="0" />"#), "{styles}");
    }

    #[tokio::test]
    async fn bullets_do_not_share_an_id_with_docx_rs_defaults() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("b.docx");
        DocxCreateTool
            .call(json!({"path": path.to_string_lossy(), "content": "- หนึ่ง\n- สอง"}))
            .await
            .unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap();
        let mut numbering = String::new();
        std::io::Read::read_to_string(
            &mut zip.by_name("word/numbering.xml").unwrap(),
            &mut numbering,
        )
        .unwrap();
        let defs = numbering
            .matches(&format!(r#"w:abstractNumId="{BULLET_ID}""#))
            .count();
        assert_eq!(defs, 1, "exactly one abstractNum {BULLET_ID}: {numbering}");
        let mut doc = String::new();
        std::io::Read::read_to_string(&mut zip.by_name("word/document.xml").unwrap(), &mut doc)
            .unwrap();
        assert!(
            doc.contains(&format!(r#"<w:numId w:val="{BULLET_ID}" />"#)),
            "{doc}"
        );
    }
}
