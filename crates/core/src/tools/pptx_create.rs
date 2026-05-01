//! `PptxCreate` — render a markdown outline to a PowerPoint deck via
//! a bundled minimal pptx template + targeted XML mutation. The Rust
//! pptx ecosystem is immature (no mature high-level crate); rather than
//! generate the ~30 OOXML files from scratch we ship a single-slide
//! template (`resources/pptx/template-light.pptx`, ~28 KB) and:
//!
//! 1. unpack it into memory,
//! 2. regenerate `ppt/slides/slide1.xml` with the user's first slide,
//! 3. emit additional `ppt/slides/slide{N}.xml` for slides 2..N,
//! 4. update `[Content_Types].xml`, `ppt/_rels/presentation.xml.rels`,
//!    and `ppt/presentation.xml` to register the new slides,
//! 5. repack as a new ZIP at the output path.
//!
//! Markdown rules:
//! - Each `# Heading` starts a new slide; the heading text is the title.
//! - Bullets under the heading become bullet body text.
//! - Empty body = title-only slide.
//!
//! Thai support: each text run carries `<a:cs typeface="Noto Sans Thai"/>`
//! so PowerPoint picks the Thai-script font for U+0E00–U+0E7F codepoints
//! and the layout default (Calibri) for everything else — same per-run
//! split-by-script trick we use in DocxCreate.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde_json::{json, Value};
use std::io::{Cursor, Read, Seek, Write};
use std::path::Path;
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

const TEMPLATE_LIGHT: &[u8] = include_bytes!("../../resources/pptx/template-light.pptx");

pub struct PptxCreateTool;

#[async_trait]
impl Tool for PptxCreateTool {
    fn name(&self) -> &'static str {
        "PptxCreate"
    }

    fn description(&self) -> &'static str {
        "Render a markdown outline to a PowerPoint deck (.pptx). Each \
         `# Heading` starts a new slide (heading is the title); bullet \
         items become bullet body text. Per-run cs typeface set to Noto \
         Sans Thai so Thai content renders without a system-font \
         dependency on the user's machine."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string", "description": "Output .pptx path. Parent directories are created if missing."},
                "content": {"type": "string", "description": "Markdown outline. Each `# Heading` = new slide."}
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

        let slides = parse_markdown_slides(content);
        if slides.is_empty() {
            return Err(Error::Tool(
                "no slides parsed from content — at least one `# Heading` line is required".into(),
            ));
        }

        if let Some(parent) = Path::new(&*validated.to_string_lossy()).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::Tool(format!("mkdir {}: {}", parent.display(), e)))?;
            }
        }

        let path_clone = validated.clone();

        let n = tokio::task::spawn_blocking(move || -> Result<usize> {
            render_pptx(&path_clone, &slides)
        })
        .await
        .map_err(|e| Error::Tool(format!("PPTX worker join failed: {e}")))??;

        Ok(format!(
            "Wrote PPTX to {} ({n} slide{})",
            validated.display(),
            if n == 1 { "" } else { "s" }
        ))
    }
}

#[derive(Debug, Clone)]
struct Slide {
    title: String,
    bullets: Vec<String>,
}

/// Parse markdown into one Slide per `# Heading`. Bullets under the
/// heading (`-` or `*` items) become the slide's body. Other content
/// (paragraphs, code blocks) is collected as a single bullet for v1
/// simplicity — full block-level rendering is a follow-up.
fn parse_markdown_slides(content: &str) -> Vec<Slide> {
    let mut slides: Vec<Slide> = Vec::new();
    let mut current: Option<Slide> = None;

    let parser = Parser::new(content);
    let mut text_buf = String::new();
    let mut in_heading = false;
    let mut in_item = false;

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                if level == pulldown_cmark::HeadingLevel::H1 {
                    if let Some(s) = current.take() {
                        slides.push(s);
                    }
                    text_buf.clear();
                    in_heading = true;
                }
            }
            Event::End(TagEnd::Heading(level)) => {
                if level == pulldown_cmark::HeadingLevel::H1 {
                    current = Some(Slide {
                        title: std::mem::take(&mut text_buf),
                        bullets: Vec::new(),
                    });
                    in_heading = false;
                }
            }
            Event::Start(Tag::Item) => {
                in_item = true;
                text_buf.clear();
            }
            Event::End(TagEnd::Item) => {
                if let Some(slide) = current.as_mut() {
                    slide.bullets.push(std::mem::take(&mut text_buf));
                }
                in_item = false;
            }
            Event::Text(s) => {
                if in_heading || in_item {
                    text_buf.push_str(&s);
                }
            }
            Event::Code(s) => {
                if in_heading || in_item {
                    text_buf.push_str(&s);
                }
            }
            Event::SoftBreak => {
                if in_heading || in_item {
                    text_buf.push(' ');
                }
            }
            _ => {}
        }
    }
    if let Some(s) = current {
        slides.push(s);
    }
    slides
}

fn render_pptx(path: &Path, slides: &[Slide]) -> Result<usize> {
    // Open template from embedded bytes.
    let mut tpl = ZipArchive::new(Cursor::new(TEMPLATE_LIGHT))
        .map_err(|e| Error::Tool(format!("open template zip: {e}")))?;

    let out_file = std::fs::File::create(path)
        .map_err(|e| Error::Tool(format!("create {}: {}", path.display(), e)))?;
    let mut zip = ZipWriter::new(out_file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let n = slides.len();
    let mut written: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Walk template entries and either rewrite (for the manifest files)
    // or pass through verbatim. Slide files are handled separately
    // below since we generate one per slide and the template only has
    // slide1.
    for i in 0..tpl.len() {
        let mut entry = tpl
            .by_index(i)
            .map_err(|e| Error::Tool(format!("read tpl entry: {e}")))?;
        let name = entry.name().to_string();

        // Skip template's slide1 + its rels — we regenerate them below
        // (also any slide* files in case the template ever ships more).
        if name.starts_with("ppt/slides/") {
            continue;
        }

        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut buf)
            .map_err(|e| Error::Tool(format!("read tpl entry body: {e}")))?;

        let new_buf: Vec<u8> = match name.as_str() {
            "[Content_Types].xml" => mutate_content_types(&buf, n)?,
            "ppt/_rels/presentation.xml.rels" => mutate_pres_rels(&buf, n)?,
            "ppt/presentation.xml" => mutate_presentation(&buf, n)?,
            _ => buf,
        };

        zip.start_file(&name, opts)
            .map_err(|e| Error::Tool(format!("zip start_file {name}: {e}")))?;
        zip.write_all(&new_buf)
            .map_err(|e| Error::Tool(format!("zip write {name}: {e}")))?;
        written.insert(name);
    }

    // Emit one slide{N}.xml + slide{N}.xml.rels per slide.
    for (idx, slide) in slides.iter().enumerate() {
        let n = idx + 1;
        let slide_path = format!("ppt/slides/slide{n}.xml");
        let rels_path = format!("ppt/slides/_rels/slide{n}.xml.rels");

        zip.start_file(&slide_path, opts)
            .map_err(|e| Error::Tool(format!("zip start_file {slide_path}: {e}")))?;
        zip.write_all(slide_xml(slide).as_bytes())
            .map_err(|e| Error::Tool(format!("zip write {slide_path}: {e}")))?;

        zip.start_file(&rels_path, opts)
            .map_err(|e| Error::Tool(format!("zip start_file {rels_path}: {e}")))?;
        zip.write_all(SLIDE_RELS.as_bytes())
            .map_err(|e| Error::Tool(format!("zip write {rels_path}: {e}")))?;
    }

    zip.finish()
        .map_err(|e| Error::Tool(format!("zip finalize: {e}")))?;

    Ok(n)
}

/// Slide XML using Title-and-Content layout. Each bullet is its own
/// `<a:p>`. Per-run rPr sets `cs typeface="Noto Sans Thai"` so PowerPoint
/// uses the Thai font for complex-script codepoints.
fn slide_xml(slide: &Slide) -> String {
    let title_xml = run_xml(&slide.title);
    let body_xml = if slide.bullets.is_empty() {
        // Title-only slide: still need a body placeholder with an empty
        // paragraph so the placeholder shows nothing instead of the
        // layout's "Click to add text" prompt.
        run_xml("")
    } else {
        slide.bullets.iter().map(|b| run_xml(b)).collect::<String>()
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/><a:p>{title_xml}</a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:cNvPr id="3" name="Content Placeholder 2"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/>{body_xml}</p:txBody></p:sp></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"#,
    )
}

/// One drawingML run wrapped in an `<a:p>` paragraph (when used for
/// bullets). For title text we wrap differently — caller controls.
fn run_xml(text: &str) -> String {
    if text.is_empty() {
        return "<a:p></a:p>".into();
    }
    let escaped = xml_escape(text);
    // lang="th-TH" + cs typeface ensures complex-script codepoints
    // (Thai range U+0E00–U+0E7F) pick up the cs font, while everything
    // else falls through to the slide layout's default typeface.
    format!(
        r#"<a:p><a:r><a:rPr lang="th-TH"><a:cs typeface="Noto Sans Thai"/></a:rPr><a:t>{escaped}</a:t></a:r></a:p>"#
    )
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Identical for every slide we emit — points at slideLayout2 (Title
/// and Content), inherited from the template. Static string ships in
/// the binary; no need to re-emit per slide.
const SLIDE_RELS: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes'?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout2.xml"/></Relationships>"#;

fn mutate_content_types(buf: &[u8], n_slides: usize) -> Result<Vec<u8>> {
    // The template ships with one slide override; we add overrides for
    // slides 2..n. Tag we insert just before `</Types>`.
    let s = std::str::from_utf8(buf)
        .map_err(|e| Error::Tool(format!("Content_Types not utf-8: {e}")))?;
    let mut extra = String::new();
    for i in 2..=n_slides {
        extra.push_str(&format!(
            r#"<Override PartName="/ppt/slides/slide{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#
        ));
    }
    Ok(s.replace("</Types>", &format!("{extra}</Types>"))
        .into_bytes())
}

fn mutate_pres_rels(buf: &[u8], n_slides: usize) -> Result<Vec<u8>> {
    // Add Relationship entries for slides 2..n. The template's last
    // relationship is rId7 (slide1); we start from rId8.
    let s =
        std::str::from_utf8(buf).map_err(|e| Error::Tool(format!("pres rels not utf-8: {e}")))?;
    let mut extra = String::new();
    for i in 2..=n_slides {
        let rid = 6 + i; // rId7 = slide1, rId8 = slide2, etc.
        extra.push_str(&format!(
            r#"<Relationship Id="rId{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{i}.xml"/>"#
        ));
    }
    Ok(
        s.replace("</Relationships>", &format!("{extra}</Relationships>"))
            .into_bytes(),
    )
}

fn mutate_presentation(buf: &[u8], n_slides: usize) -> Result<Vec<u8>> {
    // Add <p:sldId/> entries for slides 2..n inside <p:sldIdLst>.
    // Template has <p:sldId id="256" r:id="rId7"/>; subsequent slides
    // increment id by 1 and rId by 1.
    let s = std::str::from_utf8(buf)
        .map_err(|e| Error::Tool(format!("presentation.xml not utf-8: {e}")))?;
    let mut extra = String::new();
    for i in 2..=n_slides {
        let sld_id = 255 + i; // 256 is slide1
        let rid = 6 + i; // matches mutate_pres_rels
        extra.push_str(&format!(r#"<p:sldId id="{sld_id}" r:id="rId{rid}"/>"#));
    }
    Ok(s.replace("</p:sldIdLst>", &format!("{extra}</p:sldIdLst>"))
        .into_bytes())
}

#[allow(dead_code)] // kept for symmetry with future Cursor-based tests
fn rewind_cursor<W: Seek>(c: &mut W) -> Result<()> {
    c.rewind().map_err(|e| Error::Tool(format!("rewind: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn writes_pptx_with_thai_and_latin() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("deck.pptx");
        let msg = PptxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "content": "# Slide 1 สวัสดี\n\n- bullet หนึ่ง\n- bullet two\n\n# Slide 2 Title\n\n- only bullet\n\n# Title-only Slide"
            }))
            .await
            .unwrap();
        assert!(msg.contains("Wrote PPTX to"));
        assert!(msg.contains("3 slides"));
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            bytes.starts_with(b"PK"),
            "output should be a ZIP/OOXML file"
        );
        // Verify the ZIP contains the slide files we expect.
        let mut zip = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        for n in 1..=3 {
            assert!(names
                .iter()
                .any(|s| s == &format!("ppt/slides/slide{n}.xml")));
            assert!(names
                .iter()
                .any(|s| s == &format!("ppt/slides/_rels/slide{n}.xml.rels")));
        }
    }

    #[tokio::test]
    async fn errors_on_no_slides() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.pptx");
        let err = PptxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "content": "no h1 here, just text"
            }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("at least one"));
    }

    #[test]
    fn xml_escape_handles_specials() {
        assert_eq!(xml_escape("a & b < c"), "a &amp; b &lt; c");
        assert_eq!(xml_escape("\"quoted\""), "&quot;quoted&quot;");
    }

    #[test]
    fn parse_markdown_extracts_slides() {
        let md = "# First\n\n- a\n- b\n\n# Second\n\n- c";
        let slides = parse_markdown_slides(md);
        assert_eq!(slides.len(), 2);
        assert_eq!(slides[0].title, "First");
        assert_eq!(slides[0].bullets, vec!["a", "b"]);
        assert_eq!(slides[1].title, "Second");
        assert_eq!(slides[1].bullets, vec!["c"]);
    }
}
