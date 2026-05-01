//! `PdfRead` — extract text from a PDF by shelling out to `pdftotext`
//! (poppler-utils). poppler does the heavy lifting (Thai shaping, ligature
//! decomposition, layout-aware extraction); we just wrap it with sandbox
//! checks, page-range parsing, and a clear missing-binary error.
//!
//! Why shell-out instead of a pure-Rust pdf crate: extraction quality
//! across real-world PDFs (tagged structure, form fields, embedded fonts
//! with non-standard cmaps) is dominated by poppler's twenty-plus years
//! of corner-case handling. The Rust crates that exist are good for
//! valid PDFs but break on the long tail.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const EXTRACT_TIMEOUT: Duration = Duration::from_secs(60);

pub struct PdfReadTool;

#[async_trait]
impl Tool for PdfReadTool {
    fn name(&self) -> &'static str {
        "PdfRead"
    }

    fn description(&self) -> &'static str {
        "Extract text from a PDF file. Uses `pdftotext` from poppler-utils. \
         Optional `pages` parameter accepts \"all\" (default), \"3\" \
         (single page), or \"1-5\" (inclusive range). Returns extracted \
         text. Requires poppler-utils installed (`brew install poppler` \
         on macOS, `apt install poppler-utils` on Debian/Ubuntu)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":  {"type": "string", "description": "PDF file path."},
                "pages": {"type": "string", "description": "Page range: \"all\", \"N\", or \"M-N\". Default: all."}
            },
            "required": ["path"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_path = req_str(&input, "path")?;
        let validated = crate::sandbox::Sandbox::check(raw_path)?;
        let pages_spec = input.get("pages").and_then(|v| v.as_str()).unwrap_or("all");

        let (first, last) = parse_page_range(pages_spec)?;

        let mut cmd = Command::new("pdftotext");
        cmd.arg("-layout");
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
                Error::Tool(
                    "pdftotext not found — install poppler-utils \
                     (`brew install poppler` on macOS, \
                     `apt install poppler-utils` on Debian/Ubuntu)"
                        .into(),
                )
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

        let text = String::from_utf8_lossy(&out_buf).to_string();
        Ok(text)
    }
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
}
