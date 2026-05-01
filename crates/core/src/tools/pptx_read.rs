//! `PptxRead` — extract text from a PowerPoint deck. Like DocxRead,
//! pure Rust: zip the .pptx open, walk each `ppt/slides/slide{N}.xml`
//! file with quick-xml, concatenate `<a:t>` text into per-slide
//! buffers. Slides are emitted in numeric order (slide1, slide2, …)
//! separated by `---` markers, with each slide's title (the first
//! shape's text content) shown as a heading.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde_json::{json, Value};
use std::io::{Cursor, Read};
use zip::ZipArchive;

pub struct PptxReadTool;

#[async_trait]
impl Tool for PptxReadTool {
    fn name(&self) -> &'static str {
        "PptxRead"
    }

    fn description(&self) -> &'static str {
        "Extract text from a PowerPoint deck (.pptx). Returns slides in \
         document order, separated by `---`. Each slide shows its title \
         (first shape) as `# Title`, followed by remaining text-frame \
         content. Pure Rust — no LibreOffice / pandoc required."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the .pptx file."}
            },
            "required": ["path"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_path = req_str(&input, "path")?;
        let validated = crate::sandbox::Sandbox::check(raw_path)?;
        let path_clone = validated.clone();
        tokio::task::spawn_blocking(move || extract_pptx(&path_clone))
            .await
            .map_err(|e| Error::Tool(format!("PPTX worker join failed: {e}")))?
    }
}

pub(crate) fn extract_pptx(path: &std::path::Path) -> Result<String> {
    let bytes =
        std::fs::read(path).map_err(|e| Error::Tool(format!("read {}: {}", path.display(), e)))?;
    let mut zip =
        ZipArchive::new(Cursor::new(bytes)).map_err(|e| Error::Tool(format!("open zip: {e}")))?;

    // Collect slide entry names + numeric indices, sort ascending so
    // `slide10` doesn't sort before `slide2` (lexicographic gotcha).
    let mut slide_entries: Vec<(u32, String)> = Vec::new();
    for i in 0..zip.len() {
        let entry = zip
            .by_index(i)
            .map_err(|e| Error::Tool(format!("read entry {i}: {e}")))?;
        let name = entry.name();
        if let Some(num) = parse_slide_index(name) {
            slide_entries.push((num, name.to_string()));
        }
    }
    slide_entries.sort_by_key(|(n, _)| *n);

    let mut out = String::new();
    for (i, (_, name)) in slide_entries.iter().enumerate() {
        if i > 0 {
            out.push_str("\n---\n");
        }
        let mut xml = String::new();
        zip.by_name(name)
            .map_err(|e| Error::Tool(format!("read {name}: {e}")))?
            .read_to_string(&mut xml)
            .map_err(|e| Error::Tool(format!("decode {name}: {e}")))?;

        let shapes = extract_shapes(&xml)?;
        for (j, shape_text) in shapes.iter().enumerate() {
            if shape_text.trim().is_empty() {
                continue;
            }
            if j == 0 {
                out.push_str("# ");
            }
            out.push_str(shape_text.trim());
            out.push('\n');
        }
    }

    Ok(out.trim_end().to_string() + "\n")
}

/// Parse `ppt/slides/slide{N}.xml` → `Some(N)`. Returns `None` for any
/// other entry name (including slide rels, layouts, etc.).
fn parse_slide_index(name: &str) -> Option<u32> {
    let stem = name
        .strip_prefix("ppt/slides/slide")?
        .strip_suffix(".xml")?;
    stem.parse().ok()
}

/// Walk one `slide*.xml` and return a Vec of per-shape text strings.
/// Each `<p:sp>` (shape) accumulates the text from its descendant
/// `<a:t>` elements, with `<a:br/>` and `</a:p>` flushing newlines so
/// multi-paragraph body shapes preserve line structure.
fn extract_shapes(xml: &str) -> Result<Vec<String>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut shapes: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut depth_in_sp: u32 = 0;
    let mut in_text = false;
    let mut buf = Vec::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::Tool(format!("xml parse @ {}: {}", reader.buffer_position(), e)))?
        {
            Event::Start(e) => {
                let local = e.local_name();
                if local.as_ref() == b"sp" {
                    depth_in_sp += 1;
                    current.clear();
                } else if local.as_ref() == b"t" && depth_in_sp > 0 {
                    in_text = true;
                }
            }
            Event::Empty(e) => {
                if e.local_name().as_ref() == b"br" && depth_in_sp > 0 {
                    current.push('\n');
                }
            }
            Event::End(e) => {
                let local = e.local_name();
                if local.as_ref() == b"sp" && depth_in_sp > 0 {
                    depth_in_sp -= 1;
                    if depth_in_sp == 0 {
                        shapes.push(std::mem::take(&mut current));
                    }
                } else if local.as_ref() == b"t" && in_text {
                    in_text = false;
                } else if local.as_ref() == b"p" && depth_in_sp > 0 {
                    // End of a drawingML paragraph — separate the next
                    // run with a newline so bullets stay on separate lines.
                    if !current.ends_with('\n') {
                        current.push('\n');
                    }
                }
            }
            Event::Text(t) => {
                if in_text {
                    let raw = t
                        .decode()
                        .map_err(|e| Error::Tool(format!("text decode: {e}")))?;
                    let unescaped = quick_xml::escape::unescape(&raw)
                        .map_err(|e| Error::Tool(format!("text unescape: {e}")))?;
                    current.push_str(&unescaped);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(shapes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[tokio::test]
    async fn round_trip_pptx_create_then_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rt.pptx");

        crate::tools::PptxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "content": "# สวัสดี Hello\n\n- bullet หนึ่ง\n- bullet two\n\n# Slide Two\n\n- third\n- fourth"
            }))
            .await
            .unwrap();

        let extracted = PptxReadTool
            .call(json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();

        // Both slides present, separated by ---.
        assert!(
            extracted.contains("---"),
            "missing slide separator: {extracted:?}"
        );
        // Latin survives.
        assert!(extracted.contains("Hello"), "got: {extracted:?}");
        assert!(extracted.contains("Slide Two"), "got: {extracted:?}");
        // Thai survives.
        assert!(
            extracted
                .chars()
                .any(|c| matches!(c, '\u{0E00}'..='\u{0E7F}')),
            "Thai missing: {extracted:?}"
        );
        // Bullet content present.
        assert!(extracted.contains("third"), "got: {extracted:?}");
    }

    #[test]
    fn slide_index_sort_is_numeric() {
        let mut v = vec!["ppt/slides/slide10.xml", "ppt/slides/slide2.xml"];
        let mut idx: Vec<u32> = v.iter().filter_map(|n| parse_slide_index(n)).collect();
        idx.sort();
        assert_eq!(idx, vec![2, 10]);
        // Sanity: lexicographic sort would have been wrong.
        v.sort();
        assert_eq!(v[0], "ppt/slides/slide10.xml"); // lex sorts 10 < 2
    }
}
