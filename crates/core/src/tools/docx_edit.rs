//! `DocxEdit` — in-place edit of a Word document. Two operations for v1:
//!
//! - `find_replace` — replace literal occurrences of a string in body
//!   text. Substring matching is per-run (each `<w:t>` text element)
//!   because Word splits text across runs when styling changes mid-
//!   paragraph; a naïve cross-run match would miss those (rare in
//!   docs we author, common in human-authored docs).
//! - `append_paragraph` — add a new paragraph at the end of the body
//!   with the same RunFonts (Calibri + Noto Sans Thai) DocxCreate uses.
//!
//! The file's other XML parts (styles, numbering, headers, etc.) are
//! passed through verbatim. Only `word/document.xml` is mutated.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;
use serde_json::{json, Value};
use std::io::{Cursor, Read, Write};
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

const LATIN_FONT: &str = "Calibri";
const THAI_FONT: &str = "Noto Sans Thai";

pub struct DocxEditTool;

#[async_trait]
impl Tool for DocxEditTool {
    fn name(&self) -> &'static str {
        "DocxEdit"
    }

    fn description(&self) -> &'static str {
        "Edit a Word document (.docx) in place. Supported ops: \
         `find_replace` (per-run substring replace in body text) and \
         `append_paragraph` (add a new paragraph at end of body). \
         Other OOXML parts (styles, numbering, headers/footers) are \
         passed through unchanged. Note: cross-run substrings won't \
         match — Word splits text on style changes; if you authored \
         the doc with DocxCreate, paragraphs are single-run so this \
         is a non-issue."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string", "description": "Path to the .docx to edit (overwritten in place)."},
                "op":      {"type": "string", "enum": ["find_replace", "append_paragraph"], "description": "Operation to perform."},
                "find":    {"type": "string", "description": "Substring to match (find_replace only)."},
                "replace": {"type": "string", "description": "Replacement string (find_replace only)."},
                "text":    {"type": "string", "description": "Paragraph text (append_paragraph only)."}
            },
            "required": ["path", "op"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_path = req_str(&input, "path")?;
        let validated = crate::sandbox::Sandbox::check_write(raw_path)?;
        let op = req_str(&input, "op")?.to_string();

        let edit = match op.as_str() {
            "find_replace" => Edit::FindReplace {
                find: req_str(&input, "find")?.to_string(),
                replace: req_str(&input, "replace")?.to_string(),
            },
            "append_paragraph" => Edit::AppendParagraph {
                text: req_str(&input, "text")?.to_string(),
            },
            other => {
                return Err(Error::Tool(format!(
                    "unknown op {other:?}; expected find_replace or append_paragraph"
                )))
            }
        };

        let path_clone = validated.clone();
        let summary = tokio::task::spawn_blocking(move || apply_edit(&path_clone, &edit))
            .await
            .map_err(|e| Error::Tool(format!("DOCX edit worker: {e}")))??;
        Ok(summary)
    }
}

enum Edit {
    FindReplace { find: String, replace: String },
    AppendParagraph { text: String },
}

fn apply_edit(path: &std::path::Path, edit: &Edit) -> Result<String> {
    let bytes =
        std::fs::read(path).map_err(|e| Error::Tool(format!("read {}: {}", path.display(), e)))?;
    let mut zip =
        ZipArchive::new(Cursor::new(bytes)).map_err(|e| Error::Tool(format!("open zip: {e}")))?;

    let mut doc_xml = String::new();
    zip.by_name("word/document.xml")
        .map_err(|e| Error::Tool(format!("locate word/document.xml: {e}")))?
        .read_to_string(&mut doc_xml)
        .map_err(|e| Error::Tool(format!("read document.xml: {e}")))?;

    let (new_xml, summary) = match edit {
        Edit::FindReplace { find, replace } => {
            let (xml, count) = find_replace(&doc_xml, find, replace)?;
            (
                xml,
                format!(
                    "Replaced {count} occurrence(s) of {find:?} in {}",
                    path.display()
                ),
            )
        }
        Edit::AppendParagraph { text } => {
            let xml = append_paragraph(&doc_xml, text)?;
            (xml, format!("Appended paragraph to {}", path.display()))
        }
    };

    // Repack: copy all entries, replacing word/document.xml with our new bytes.
    let tmp_buf: Vec<u8> =
        Vec::with_capacity(zip.by_index(0).map(|e| e.size()).unwrap_or(0) as usize * 2);
    let mut writer = ZipWriter::new(Cursor::new(tmp_buf));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| Error::Tool(format!("read entry {i}: {e}")))?;
        let name = entry.name().to_string();
        writer
            .start_file(&name, opts)
            .map_err(|e| Error::Tool(format!("zip start_file {name}: {e}")))?;

        if name == "word/document.xml" {
            writer
                .write_all(new_xml.as_bytes())
                .map_err(|e| Error::Tool(format!("zip write {name}: {e}")))?;
        } else {
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry
                .read_to_end(&mut buf)
                .map_err(|e| Error::Tool(format!("read entry body {name}: {e}")))?;
            writer
                .write_all(&buf)
                .map_err(|e| Error::Tool(format!("zip write {name}: {e}")))?;
        }
    }
    let final_buf = writer
        .finish()
        .map_err(|e| Error::Tool(format!("zip finalize: {e}")))?
        .into_inner();

    std::fs::write(path, final_buf)
        .map_err(|e| Error::Tool(format!("write {}: {}", path.display(), e)))?;

    Ok(summary)
}

/// Stream-rewrite `document.xml`: when we hit a Text event nested
/// inside `<w:t>`, do a substring replace and re-emit. Returns the
/// rewritten XML and the count of replacements made.
fn find_replace(xml: &str, find: &str, replace: &str) -> Result<(String, usize)> {
    if find.is_empty() {
        return Err(Error::Tool("find string must not be empty".into()));
    }

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut out_buf: Vec<u8> = Vec::with_capacity(xml.len());
    let mut writer = Writer::new(&mut out_buf);
    let mut buf = Vec::new();
    let mut in_text = false;
    let mut count = 0;

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::Tool(format!("xml parse: {e}")))?
        {
            Event::Start(e) => {
                if e.local_name().as_ref() == b"t" {
                    in_text = true;
                }
                writer
                    .write_event(Event::Start(e.into_owned()))
                    .map_err(|e| Error::Tool(format!("xml write: {e}")))?;
            }
            Event::End(e) => {
                if e.local_name().as_ref() == b"t" {
                    in_text = false;
                }
                writer
                    .write_event(Event::End(e.into_owned()))
                    .map_err(|e| Error::Tool(format!("xml write: {e}")))?;
            }
            Event::Text(t) => {
                if in_text {
                    let raw = t
                        .decode()
                        .map_err(|e| Error::Tool(format!("text decode: {e}")))?;
                    let unescaped = quick_xml::escape::unescape(&raw)
                        .map_err(|e| Error::Tool(format!("text unescape: {e}")))?;
                    let occurrences = unescaped.matches(find).count();
                    if occurrences > 0 {
                        let replaced = unescaped.replace(find, replace);
                        count += occurrences;
                        // BytesText auto-escapes on write.
                        writer
                            .write_event(Event::Text(BytesText::new(&replaced)))
                            .map_err(|e| Error::Tool(format!("xml write: {e}")))?;
                    } else {
                        writer
                            .write_event(Event::Text(t.into_owned()))
                            .map_err(|e| Error::Tool(format!("xml write: {e}")))?;
                    }
                } else {
                    writer
                        .write_event(Event::Text(t.into_owned()))
                        .map_err(|e| Error::Tool(format!("xml write: {e}")))?;
                }
            }
            Event::Eof => break,
            other => writer
                .write_event(other.into_owned())
                .map_err(|e| Error::Tool(format!("xml write: {e}")))?,
        }
        buf.clear();
    }

    let s = String::from_utf8(out_buf).map_err(|e| Error::Tool(format!("utf-8: {e}")))?;
    Ok((s, count))
}

/// Insert a new paragraph just before `</w:body>`. The paragraph uses
/// the same RunFonts as DocxCreate so Thai content renders with the
/// expected font fallback chain. We construct the XML by string injection
/// rather than full reparse — `</w:body>` is a unique terminator and
/// the rest of the doc stays byte-identical.
fn append_paragraph(xml: &str, text: &str) -> Result<String> {
    let escaped = xml_escape(text);
    let para = format!(
        r#"<w:p><w:r><w:rPr><w:rFonts w:ascii="{LATIN_FONT}" w:hAnsi="{LATIN_FONT}" w:cs="{THAI_FONT}"/></w:rPr><w:t xml:space="preserve">{escaped}</w:t></w:r></w:p>"#
    );

    // OOXML body close tags can appear as `</w:body>` (most common) or
    // include a namespace prefix variant. Match the first occurrence and
    // splice our paragraph just before it.
    if let Some(idx) = xml.rfind("</w:body>") {
        let mut out = String::with_capacity(xml.len() + para.len());
        out.push_str(&xml[..idx]);
        out.push_str(&para);
        out.push_str(&xml[idx..]);
        Ok(out)
    } else {
        Err(Error::Tool(
            "could not locate </w:body> in document.xml — file may not be a valid Word doc".into(),
        ))
    }
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

// quick-xml's `Event::*::into_owned` — silence the unused lint when
// the BytesStart import isn't used after fmt rearranges things.
#[allow(dead_code)]
fn _import_anchor() -> (BytesStart<'static>, BytesEnd<'static>) {
    (BytesStart::new(""), BytesEnd::new(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[tokio::test]
    async fn round_trip_create_edit_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rt.docx");

        crate::tools::DocxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "content": "# Title\n\nOriginal paragraph mentions OLDNAME.\n\n- ข้อหนึ่ง\n- bullet two"
            }))
            .await
            .unwrap();

        // 1. find_replace
        let r1 = DocxEditTool
            .call(json!({
                "path": path.to_string_lossy(),
                "op": "find_replace",
                "find": "OLDNAME",
                "replace": "NEWNAME"
            }))
            .await
            .unwrap();
        assert!(r1.contains("Replaced 1 occurrence"));

        // 2. append_paragraph (Thai content)
        let r2 = DocxEditTool
            .call(json!({
                "path": path.to_string_lossy(),
                "op": "append_paragraph",
                "text": "เพิ่มท้ายเอกสาร appended at end"
            }))
            .await
            .unwrap();
        assert!(r2.contains("Appended paragraph"));

        // 3. read back, verify both edits landed + original Thai survived
        let extracted = crate::tools::DocxReadTool
            .call(json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();
        assert!(
            !extracted.contains("OLDNAME"),
            "old text leaked: {extracted:?}"
        );
        assert!(
            extracted.contains("NEWNAME"),
            "replacement missing: {extracted:?}"
        );
        assert!(
            extracted.contains("ข้อหนึ่ง"),
            "original Thai bullet lost: {extracted:?}"
        );
        assert!(
            extracted.contains("เพิ่มท้ายเอกสาร"),
            "appended Thai missing: {extracted:?}"
        );
    }

    #[test]
    fn empty_find_rejected() {
        let r = find_replace("<w:t>x</w:t>", "", "y");
        assert!(r.is_err());
    }
}
