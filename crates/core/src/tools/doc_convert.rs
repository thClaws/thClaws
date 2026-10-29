//! `DocConvert` — convert an office document between formats on the
//! document service (dev-plan/69). Word / Excel / PowerPoint, OpenDocument
//! and the legacy .doc/.xls/.ppt go through LibreOffice (`/convert`); an
//! HTML page to PDF goes through Chromium with Thai fonts (`/html-to-pdf`).
//! Nothing has to be installed on the desktop, which is the point: a clean
//! Windows PC has no LibreOffice.
//!
//! Writes beside the source (or to `out`) and never overwrites.

use super::{doc_render, req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const TARGETS: &[&str] = &[
    "pdf", "docx", "xlsx", "pptx", "odt", "ods", "odp", "html", "txt", "csv",
];

const INPUTS: &[&str] = &[
    "doc", "docx", "odt", "rtf", "txt", "html", "htm", "xls", "xlsx", "ods", "csv", "ppt", "pptx",
    "odp",
];

const MAX_ASSETS: usize = 120;
const MAX_ASSET_BYTES: u64 = 25 * 1024 * 1024;

pub struct DocConvertTool;

#[async_trait]
impl Tool for DocConvertTool {
    fn name(&self) -> &'static str {
        "DocConvert"
    }

    fn description(&self) -> &'static str {
        "Convert a document to another format on the thClaws document service \
         (LibreOffice + Chromium, Thai fonts) — nothing to install locally. `path`: \
         .doc/.docx/.odt/.rtf/.txt/.html, .xls/.xlsx/.ods/.csv or .ppt/.pptx/.odp. \
         `to`: pdf, docx, xlsx, pptx, odt, ods, odp, html, txt or csv. An .html \
         file to pdf is typeset by Chromium, with images/CSS beside it sent along. \
         Writes `<stem>.<to>` beside the source, or `out`; refuses to overwrite. \
         Use it to make a PDF of a Word/Excel/PowerPoint file, or to open an old \
         .doc/.xls/.ppt: convert to docx/xlsx/pptx, then read it with \
         DocxRead/XlsxRead/PptxRead."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Source document."},
                "to":   {"type": "string", "enum": TARGETS, "description": "Target format."},
                "out":  {"type": "string", "description": "Output path. Default: the source path with the new extension."}
            },
            "required": ["path", "to"]
        })
    }

    /// Always, like the other file-creating tools: it writes a file, and plan
    /// mode must refuse it. Auto mode — and every EE multi-user workspace —
    /// runs it without a prompt.
    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    fn approval_summary(&self, input: &Value) -> Option<String> {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("?");
        let to = input.get("to").and_then(|v| v.as_str()).unwrap_or("?");
        let dest = doc_render::endpoint("convert")
            .map(|ep| doc_render::describe(&ep))
            .unwrap_or_else(|_| "the document service".into());
        Some(format!("UPLOADS {path} to {dest} to convert it to .{to}"))
    }

    async fn call(&self, input: Value) -> Result<String> {
        let path = req_str(&input, "path")?;
        let to = req_str(&input, "to")?;
        let out = input.get("out").and_then(|v| v.as_str());
        let done = convert_file(path, to, out).await?;
        Ok(format!(
            "Converted {path} → {} ({} KB, on {})",
            done.out.display(),
            (done.bytes + 512) / 1024,
            done.via
        ))
    }
}

#[derive(Debug)]
pub(crate) struct Converted {
    pub out: PathBuf,
    pub bytes: usize,
    pub via: String,
}

/// The conversion behind both the tool and the Files tab's "Convert to…".
/// The caller owns consent: the tool through its approval gate, the Files
/// tab by asking first on an install where the service is not the
/// organisation's own.
pub(crate) async fn convert_file(
    src_raw: &str,
    to: &str,
    out_raw: Option<&str>,
) -> Result<Converted> {
    convert_with(src_raw, to, out_raw, doc_render::endpoint).await
}

async fn convert_with(
    src_raw: &str,
    to: &str,
    out_raw: Option<&str>,
    endpoint: impl Fn(&str) -> Result<super::slide_render::SlideEndpoint>,
) -> Result<Converted> {
    let src = crate::sandbox::Sandbox::check(src_raw)?;
    let to = to.trim().trim_start_matches('.').to_ascii_lowercase();
    if !TARGETS.contains(&to.as_str()) {
        return Err(Error::Tool(format!(
            "cannot convert to .{to}; targets are {}",
            TARGETS.join(", ")
        )));
    }
    let ext = extension(&src);
    if !INPUTS.contains(&ext.as_str()) {
        return Err(Error::Tool(format!(
            "cannot convert a .{ext} file; inputs are {}{}",
            INPUTS.join(", "),
            if ext == "pdf" {
                ". To get text out of a PDF use PdfRead"
            } else {
                ""
            }
        )));
    }
    if ext == to || (ext == "htm" && to == "html") {
        return Err(Error::Tool(format!(
            "{} is already a .{to} file",
            src.display()
        )));
    }
    let default_out;
    let out_raw = match out_raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(o) => o,
        None => {
            default_out = Path::new(src_raw)
                .with_extension(&to)
                .to_string_lossy()
                .into_owned();
            &default_out
        }
    };
    let out = crate::sandbox::Sandbox::check_write(out_raw)?;
    if out.exists() {
        return Err(Error::Tool(format!(
            "{} already exists — pass `out` with another name",
            out.display()
        )));
    }

    let (bytes, via) = if to == "pdf" && matches!(ext.as_str(), "html" | "htm") {
        let ep = endpoint("html-to-pdf")?;
        let assets = html_assets(&src)?;
        (
            doc_render::html_to_pdf(&ep, &src, assets).await?,
            doc_render::describe(&ep),
        )
    } else {
        let ep = endpoint("convert")?;
        (
            doc_render::convert(&ep, &src, &to).await?,
            doc_render::describe(&ep),
        )
    };
    if bytes.is_empty() {
        return Err(Error::Tool(
            "the document service returned an empty file".into(),
        ));
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Tool(format!("mkdir {}: {e}", parent.display())))?;
    }
    std::fs::write(&out, &bytes)
        .map_err(|e| Error::Tool(format!("write {}: {e}", out.display())))?;
    Ok(Converted {
        out,
        bytes: bytes.len(),
        via,
    })
}

fn extension(p: &Path) -> String {
    p.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Files an HTML page pulls in by a plain relative `src`/`href` in its own
/// directory — the images and stylesheet a generated report sits beside.
/// Sent by bare name so the service resolves them the way the page does.
/// Anything with a path, a scheme or a parent hop is left alone.
fn html_assets(html: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let page = String::from_utf8_lossy(
        &std::fs::read(html).map_err(|e| Error::Tool(format!("read {}: {e}", html.display())))?,
    )
    .into_owned();
    let dir = html.parent().unwrap_or(Path::new("."));
    let re = regex::Regex::new(r#"(?i)\b(?:src|href)\s*=\s*["']([^"'#?]+)"#).unwrap();
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for cap in re.captures_iter(&page) {
        let name = cap[1].trim().trim_start_matches("./");
        if name.is_empty()
            || name.contains(['/', '\\', ':'])
            || name.starts_with('.')
            || out.iter().any(|(n, _)| n == name)
        {
            continue;
        }
        let path = dir.join(name);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_ASSET_BYTES {
            continue;
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?;
        out.push((name.to_string(), bytes));
        if out.len() >= MAX_ASSETS {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_assets_are_same_directory_files_that_exist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("logo.png"), b"png").unwrap();
        std::fs::write(dir.path().join("style.css"), b"body{}").unwrap();
        std::fs::create_dir(dir.path().join("img")).unwrap();
        std::fs::write(dir.path().join("img/deep.png"), b"png").unwrap();
        let html = dir.path().join("report.html");
        std::fs::write(
            &html,
            r#"<link rel="stylesheet" href="style.css"><img src='./logo.png'>
               <img src="logo.png"><img src="img/deep.png"><img src="missing.png">
               <a href="https://example.com/x.png">x</a><img src="data:image/png;base64,AA">"#,
        )
        .unwrap();
        let mut names: Vec<String> = html_assets(&html)
            .unwrap()
            .into_iter()
            .map(|a| a.0)
            .collect();
        names.sort();
        assert_eq!(names, ["logo.png", "style.css"]);
    }

    #[tokio::test]
    async fn refuses_bad_targets_inputs_and_overwrites_before_uploading() {
        let dir = tempfile::tempdir().unwrap();
        let docx = dir.path().join("memo.docx");
        std::fs::write(&docx, b"PK").unwrap();
        let p = docx.to_string_lossy().to_string();

        let err = convert_file(&p, "exe", None).await.unwrap_err().to_string();
        assert!(err.contains("cannot convert to .exe"), "{err}");

        let err = convert_file(&p, "docx", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("already a .docx"), "{err}");

        std::fs::write(dir.path().join("memo.pdf"), b"%PDF").unwrap();
        let err = convert_file(&p, "pdf", None).await.unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");

        let pdf = dir.path().join("memo.pdf").to_string_lossy().to_string();
        let err = convert_file(&pdf, "docx", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("PdfRead"), "{err}");
    }

    #[tokio::test]
    async fn converts_beside_the_source_and_routes_html_to_chromium() {
        use crate::tools::doc_render::tests::{ep, mock_doc_render};
        let (base, seen) = mock_doc_render().await;
        let resolve = |path: &str| Ok(ep(&base, path));
        let dir = tempfile::tempdir().unwrap();

        let xls = dir.path().join("งบประมาณ.xls");
        std::fs::write(&xls, b"\xD0\xCF\x11\xE0").unwrap();
        let done = convert_with(&xls.to_string_lossy(), "XLSX", None, resolve)
            .await
            .unwrap();
        // The sandbox canonicalises (/var → /private/var on macOS).
        assert_eq!(
            done.out.file_name().unwrap().to_string_lossy(),
            "งบประมาณ.xlsx"
        );
        assert_eq!(
            std::fs::read(&done.out).unwrap(),
            "converted:งบประมาณ.xls:xlsx".as_bytes()
        );

        let html = dir.path().join("report.html");
        std::fs::write(&html, r#"<img src="chart.png">"#).unwrap();
        std::fs::write(dir.path().join("chart.png"), b"png").unwrap();
        let out = dir.path().join("out/report-final.pdf");
        let done = convert_with(
            &html.to_string_lossy(),
            "pdf",
            Some(&out.to_string_lossy()),
            resolve,
        )
        .await
        .unwrap();
        assert!(done.out.ends_with("out/report-final.pdf"), "{:?}", done.out);

        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "/convert");
        assert_eq!(seen[1].0, "/html-to-pdf");
        assert_eq!(seen[1].1["assets[]"], "chart.png");
    }
}
