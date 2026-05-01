//! `DocxRead` — extract text from a Word document. Pure Rust: zip the
//! .docx open, walk `word/document.xml` with quick-xml, concatenate
//! `<w:t>` text into per-paragraph buffers. Heading and list semantics
//! are reconstructed from `<w:pStyle>` and `<w:numPr>` so the output
//! reads as markdown-ish (`# Heading`, `- list item`).
//!
//! Why no shell-out: PDF had `pdftotext` everywhere; for OOXML we'd
//! need LibreOffice headless, which is too heavy to require. quick-xml
//! is fast enough and keeps the binary self-contained.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde_json::{json, Value};
use std::io::{Cursor, Read};
use zip::ZipArchive;

pub struct DocxReadTool;

#[async_trait]
impl Tool for DocxReadTool {
    fn name(&self) -> &'static str {
        "DocxRead"
    }

    fn description(&self) -> &'static str {
        "Extract text from a Word document (.docx). Returns the document \
         body as markdown-ish text: headings prefixed with `#`/`##`/etc., \
         list items prefixed with `- `, paragraphs separated by blank \
         lines. Pure Rust — no LibreOffice / pandoc required."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the .docx file."}
            },
            "required": ["path"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_path = req_str(&input, "path")?;
        let validated = crate::sandbox::Sandbox::check(raw_path)?;
        let path_clone = validated.clone();
        tokio::task::spawn_blocking(move || extract_docx(&path_clone))
            .await
            .map_err(|e| Error::Tool(format!("DOCX worker join failed: {e}")))?
    }
}

pub(crate) fn extract_docx(path: &std::path::Path) -> Result<String> {
    let bytes =
        std::fs::read(path).map_err(|e| Error::Tool(format!("read {}: {}", path.display(), e)))?;
    let mut zip =
        ZipArchive::new(Cursor::new(bytes)).map_err(|e| Error::Tool(format!("open zip: {e}")))?;

    let mut doc_xml = String::new();
    zip.by_name("word/document.xml")
        .map_err(|e| Error::Tool(format!("locate word/document.xml: {e}")))?
        .read_to_string(&mut doc_xml)
        .map_err(|e| Error::Tool(format!("read document.xml: {e}")))?;

    walk_document_xml(&doc_xml)
}

/// Walk `word/document.xml` events and emit reconstructed markdown.
/// State machine:
///
/// - `<w:p>` opens a paragraph buffer
/// - inside `<w:pPr>`, `<w:pStyle w:val="...">` records the style
///   (Heading1..Heading9 → `#`/`##`/...) and `<w:numPr>` flips the
///   list flag
/// - `<w:t>` text content goes into the buffer (also `<w:tab/>` → tab,
///   `<w:br/>` → newline)
/// - `</w:p>` flushes the buffer with whatever prefix the style/list
///   flags imply, then resets state
fn walk_document_xml(xml: &str) -> Result<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let decoder = reader.decoder();

    let mut out = String::new();
    let mut buf = Vec::new();

    let mut para_text = String::new();
    let mut heading_level: Option<u8> = None;
    let mut is_list_item = false;
    let mut in_text = false;
    let mut in_ppr = false;

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::Tool(format!("xml parse @ {}: {}", reader.buffer_position(), e)))?
        {
            Event::Start(e) => match e.local_name().as_ref() {
                b"p" => {
                    para_text.clear();
                    heading_level = None;
                    is_list_item = false;
                }
                b"pPr" => in_ppr = true,
                b"t" => in_text = true,
                _ => {}
            },
            Event::Empty(e) => match e.local_name().as_ref() {
                b"pStyle" if in_ppr => {
                    if let Ok(Some(val)) = e.try_get_attribute("w:val") {
                        let v = val.decode_and_unescape_value(decoder).unwrap_or_default();
                        // OOXML built-in heading styles: "Heading1", "Heading2", ...
                        // Tolerate lowercase and "heading 1" (some authors).
                        let norm = v.to_lowercase().replace(' ', "");
                        if let Some(rest) = norm.strip_prefix("heading") {
                            let parsed: std::result::Result<u8, _> = rest.parse();
                            if let Ok(n) = parsed {
                                if (1..=9).contains(&n) {
                                    heading_level = Some(n);
                                }
                            }
                        }
                    }
                }
                b"numPr" if in_ppr => is_list_item = true,
                b"tab" => para_text.push('\t'),
                b"br" => para_text.push('\n'),
                _ => {}
            },
            Event::End(e) => match e.local_name().as_ref() {
                b"pPr" => in_ppr = false,
                b"t" => in_text = false,
                b"p" => {
                    let trimmed = para_text.trim();
                    if !trimmed.is_empty() {
                        let prefix = if let Some(level) = heading_level {
                            format!("{} ", "#".repeat(level as usize))
                        } else if is_list_item {
                            "- ".to_string()
                        } else {
                            String::new()
                        };
                        out.push_str(&prefix);
                        out.push_str(trimmed);
                        out.push('\n');
                        // Add an extra newline (= markdown paragraph
                        // separator) after non-list paragraphs so block
                        // boundaries survive. List items pack tightly.
                        if !is_list_item {
                            out.push('\n');
                        }
                    }
                    para_text.clear();
                }
                _ => {}
            },
            Event::Text(t) => {
                if in_text {
                    let raw = t
                        .decode()
                        .map_err(|e| Error::Tool(format!("text decode: {e}")))?;
                    // Resolve XML entities (&amp; &lt; &gt; &quot; &apos;).
                    // OOXML body text rarely uses them but Word does encode
                    // a literal `<` etc. — better to handle than not.
                    let unescaped = quick_xml::escape::unescape(&raw)
                        .map_err(|e| Error::Tool(format!("text unescape: {e}")))?;
                    para_text.push_str(&unescaped);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(out.trim_end().to_string() + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[tokio::test]
    async fn round_trip_docx_create_then_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rt.docx");

        // Use the sibling Tier-1 create tool to produce a doc.
        crate::tools::DocxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "content": "# หัวข้อ Heading\n\nย่อหน้าแรก first paragraph.\n\n- bullet หนึ่ง\n- bullet two"
            }))
            .await
            .unwrap();

        let extracted = DocxReadTool
            .call(json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();

        // Latin survives.
        assert!(extracted.contains("Heading"), "got: {extracted:?}");
        assert!(extracted.contains("first paragraph"), "got: {extracted:?}");
        // Thai survives.
        assert!(
            extracted
                .chars()
                .any(|c| matches!(c, '\u{0E00}'..='\u{0E7F}')),
            "Thai missing: {extracted:?}"
        );
        // Both items came back (we don't assert the bullet prefix because
        // docx-rs's heading style may not be "Heading1" — we render
        // headings as bold+sized runs without the pStyle attr — so the
        // # prefix isn't expected from our own output. The text content
        // is what matters here.)
        assert!(extracted.contains("bullet"));
    }
}
