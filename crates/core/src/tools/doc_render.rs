//! Client for the document endpoints of the `slide-render` service
//! (dev-plan/69): `/convert`, `/pdf/text`, `/pdf/pages`, `/ocr` and
//! `/html-to-pdf`. Endpoint resolution is RenderSlides': `SLIDE_RENDER_API`
//! when set, otherwise `<gateway>/slide-render/<path>` with the gateway key.
//!
//! Every call uploads the file. On a gateway-locked (EE) install that
//! gateway is the organisation's own, so the upload needs no prompt (owner
//! decision 2026-10-21); everywhere else the caller must have asked first.

use super::slide_render::{resolve_slide_endpoint_for, SlideEndpoint};
use crate::error::{Error, Result};
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

/// Below this mean word confidence an OCR'd page goes to the vision model.
pub(crate) const OCR_MIN_CONFIDENCE: f64 = 70.0;

/// Same ceiling as the service's multipart limit.
const MAX_UPLOAD_BYTES: u64 = 200 * 1024 * 1024;

const TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) fn endpoint(path: &str) -> Result<SlideEndpoint> {
    resolve_slide_endpoint_for(path)
}

pub(crate) fn uploads_preapproved() -> bool {
    crate::shared::gateway_providers_locked()
}

/// Where the file goes, in words the approval prompt and the result can use.
pub(crate) fn describe(ep: &SlideEndpoint) -> String {
    let host = reqwest::Url::parse(&ep.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| ep.url.clone());
    if !ep.via_gateway {
        format!("the document service at {host}")
    } else if uploads_preapproved() {
        format!("your organisation's document service ({host})")
    } else {
        format!("the thClaws document service ({host})")
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct PageText {
    pub n: u32,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub confidence: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PagePng {
    pub n: u32,
    pub png_base64: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Pages<T> {
    pub page_count: u32,
    pub pages: Vec<T>,
}

pub(crate) fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "tif" | "tiff" => "image/tiff",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "css" => "text/css",
        "html" | "htm" => "text/html",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "rtf" => "application/rtf",
        "doc" => "application/msword",
        "xls" => "application/vnd.ms-excel",
        "ppt" => "application/vnd.ms-powerpoint",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        _ => "application/octet-stream",
    }
}

fn file_part(path: &Path) -> Result<Part> {
    let size = std::fs::metadata(path)
        .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?
        .len();
    if size > MAX_UPLOAD_BYTES {
        return Err(Error::Tool(format!(
            "{} is {:.1} MB — over the {:.0} MB document-service limit",
            path.display(),
            size as f64 / 1_048_576.0,
            MAX_UPLOAD_BYTES as f64 / 1_048_576.0
        )));
    }
    let bytes =
        std::fs::read(path).map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "input".into());
    let mime = mime_for(&name);
    Part::bytes(bytes)
        .file_name(name)
        .mime_str(mime)
        .map_err(|e| Error::Tool(format!("multipart: {e}")))
}

fn with_range(mut form: Form, first: Option<u32>, last: Option<u32>) -> Form {
    if let Some(f) = first {
        form = form.text("first", f.to_string());
    }
    if let Some(l) = last {
        form = form.text("last", l.to_string());
    }
    form
}

/// POST a multipart form and return the body. A non-2xx surfaces the
/// service's own `{"error"}` sentence rather than a bare status.
pub(crate) async fn post(ep: &SlideEndpoint, form: Form) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| Error::Tool(format!("http client: {e}")))?;
    let mut rb = crate::multi_tenant::attach_member(client.post(&ep.url));
    if let Some(ref token) = ep.bearer {
        rb = rb.bearer_auth(token);
    }
    let resp = rb
        .multipart(form)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("document service unreachable ({}): {e}", ep.url)))?;
    let status = resp.status();
    let body = resp
        .bytes()
        .await
        .map_err(|e| Error::Tool(format!("read document-service response: {e}")))?;
    if !status.is_success() {
        let detail = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| String::from_utf8_lossy(&body).chars().take(300).collect());
        return Err(Error::Tool(format!(
            "document service http {status}: {detail}"
        )));
    }
    Ok(body.to_vec())
}

fn parse_json<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T> {
    serde_json::from_slice(body)
        .map_err(|e| Error::Tool(format!("document service returned unexpected JSON: {e}")))
}

pub(crate) async fn pdf_text(
    ep: &SlideEndpoint,
    path: &Path,
    first: Option<u32>,
    last: Option<u32>,
    mode: &str,
) -> Result<Pages<PageText>> {
    let form = with_range(Form::new().part("file", file_part(path)?), first, last)
        .text("mode", mode.to_string());
    parse_json(&post(ep, form).await?)
}

pub(crate) async fn pdf_pages(
    ep: &SlideEndpoint,
    path: &Path,
    first: Option<u32>,
    last: Option<u32>,
    dpi: u32,
) -> Result<Pages<PagePng>> {
    let form = with_range(Form::new().part("file", file_part(path)?), first, last)
        .text("dpi", dpi.to_string());
    parse_json(&post(ep, form).await?)
}

pub(crate) async fn ocr(
    ep: &SlideEndpoint,
    path: &Path,
    first: Option<u32>,
    last: Option<u32>,
) -> Result<Pages<PageText>> {
    let form =
        with_range(Form::new().part("file", file_part(path)?), first, last).text("lang", "tha+eng");
    parse_json(&post(ep, form).await?)
}

pub(crate) async fn convert(ep: &SlideEndpoint, path: &Path, to: &str) -> Result<Vec<u8>> {
    let form = Form::new()
        .part("file", file_part(path)?)
        .text("to", to.to_string());
    post(ep, form).await
}

pub(crate) async fn html_to_pdf(
    ep: &SlideEndpoint,
    path: &Path,
    assets: Vec<(String, Vec<u8>)>,
) -> Result<Vec<u8>> {
    let mut form = Form::new().part("file", file_part(path)?);
    for (name, bytes) in assets {
        let mime = mime_for(&name);
        let part = Part::bytes(bytes)
            .file_name(name)
            .mime_str(mime)
            .map_err(|e| Error::Tool(format!("multipart: {e}")))?;
        form = form.part("assets[]", part);
    }
    post(ep, form).await
}

/// Some tesseract runs separate Thai words — often single characters — with
/// spaces the script never writes; others keep the document's own phrase
/// breaks. Join only when the page is clearly tokenised (a space at least
/// every eight Thai characters), and then only single spaces between two Thai
/// characters; a wider gap is layout and stays. Measured 2026-10-21: the
/// Debian `tha` model kept real breaks ("ส่วนราชการ ฝ่ายบริหาร"), and an
/// unconditional join glued them together.
pub(crate) fn join_ocr_thai_spaces(text: &str) -> String {
    use regex::Regex;
    let gap = Regex::new(r"([\u{0E01}-\u{0E5B}]) ([\u{0E01}-\u{0E5B}])").unwrap();
    let thai = text.chars().filter(|c| is_thai(*c)).count();
    if gap.find_iter(text).count() * 8 < thai {
        return text.to_string();
    }
    let mut s = text.to_string();
    // Each match consumes its right-hand character, so "ก ข ค" needs a
    // second pass for the "ข ค" the first one stepped over.
    for _ in 0..2 {
        s = gap.replace_all(&s, "$1$2").into_owned();
    }
    s
}

fn is_thai(c: char) -> bool {
    ('\u{0E01}'..='\u{0E5B}').contains(&c)
}

/// Above-/below-base vowels and tone marks: no Thai word starts with one.
fn is_thai_dependent(c: char) -> bool {
    matches!(c, '\u{0E31}' | '\u{0E34}'..='\u{0E3A}' | '\u{0E47}'..='\u{0E4E}')
}

/// Tesseract's word confidence stays high on noisy Thai scans (93 on a page
/// with stray vowels and junk lines), so it cannot gate the vision fallback
/// alone. Signs the text is damaged: words starting with a dependent vowel
/// or tone mark, and short junk lines left by specks and stamps.
pub(crate) fn ocr_looks_damaged(text: &str) -> bool {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return false;
    }
    let stray = text
        .split_whitespace()
        .filter(|w| w.chars().next().is_some_and(is_thai_dependent))
        .count();
    let junk = lines
        .iter()
        .filter(|l| {
            let n = l.chars().filter(|c| !c.is_whitespace()).count();
            n <= 4
                && !l
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
        })
        .count();
    let bad = stray + junk;
    bad >= 3 && bad * 10 >= lines.len()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::extract::Multipart;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post as axum_post;
    use std::collections::BTreeMap;

    /// What a stand-in doc-render saw: fields by name (file → its filename).
    pub(crate) type Seen =
        std::sync::Arc<std::sync::Mutex<Vec<(String, BTreeMap<String, String>)>>>;

    async fn fields(mut mp: Multipart) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        while let Ok(Some(f)) = mp.next_field().await {
            let name = f.name().unwrap_or("").to_string();
            let value = match f.file_name() {
                Some(n) => n.to_string(),
                None => f.text().await.unwrap_or_default(),
            };
            out.entry(name)
                .and_modify(|v: &mut String| {
                    v.push(',');
                    v.push_str(&value)
                })
                .or_insert(value);
        }
        out
    }

    /// A local stand-in for doc-render that answers every contract endpoint
    /// with fixed data. Page 2 of `/ocr` is low-confidence on purpose.
    pub(crate) async fn mock_doc_render() -> (String, Seen) {
        let seen: Seen = Default::default();
        let record = |path: &'static str, seen: Seen| {
            move |headers: HeaderMap, mp: Multipart| {
                let seen = seen.clone();
                async move {
                    let mut f = fields(mp).await;
                    if let Some(a) = headers.get("authorization") {
                        f.insert("auth".into(), a.to_str().unwrap_or("").into());
                    }
                    seen.lock().unwrap().push((path.to_string(), f.clone()));
                    let file = f.get("file").cloned().unwrap_or_default();
                    if file.ends_with(".bad") {
                        return (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            r#"{"error":"cannot read this file"}"#.as_bytes().to_vec(),
                        );
                    }
                    let png = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        b"\x89PNG\r\n\x1a\nfake",
                    );
                    let body = match path {
                        "/pdf/text" => serde_json::json!({"page_count": 3, "pages": [
                            {"n": 1, "text": "รายงานการประชุม ครั้งที่ 1/2569 ผู้เข้าร่วมประชุม ทั้งหมด สิบสองคน"},
                            {"n": 2, "text": "มติที่ประชุม อนุมัติงบประมาณ 6,000,000 บาท สำหรับค่าใบอนุญาตซอฟต์แวร์"}
                        ]})
                        .to_string()
                        .into_bytes(),
                        "/pdf/pages" => {
                            let first: u32 = f.get("first").and_then(|v| v.parse().ok()).unwrap_or(1);
                            let last: u32 = f.get("last").and_then(|v| v.parse().ok()).unwrap_or(first);
                            let pages: Vec<_> = (first..=last.min(3))
                                .map(|n| serde_json::json!({"n": n, "png_base64": png}))
                                .collect();
                            serde_json::json!({"page_count": 3, "pages": pages})
                                .to_string()
                                .into_bytes()
                        }
                        "/ocr" => serde_json::json!({"page_count": 3, "pages": [
                            {"n": 1, "text": "บันทึก ข้อ ความ เรื่อง ขอ อนุมัติ", "confidence": 91.5},
                            {"n": 2, "text": "ล า ย มือ", "confidence": 41.0},
                            {"n": 3, "text": "งบ ประมาณ 6,000,000 บาท", "confidence": 88.0}
                        ]})
                        .to_string()
                        .into_bytes(),
                        _ => format!("converted:{file}:{}", f.get("to").cloned().unwrap_or_default())
                            .into_bytes(),
                    };
                    (StatusCode::OK, body)
                }
            }
        };
        let app = axum::Router::new()
            .route("/convert", axum_post(record("/convert", seen.clone())))
            .route("/pdf/text", axum_post(record("/pdf/text", seen.clone())))
            .route("/pdf/pages", axum_post(record("/pdf/pages", seen.clone())))
            .route("/ocr", axum_post(record("/ocr", seen.clone())))
            .route(
                "/html-to-pdf",
                axum_post(record("/html-to-pdf", seen.clone())),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), seen)
    }

    pub(crate) fn ep(base: &str, path: &str) -> SlideEndpoint {
        SlideEndpoint {
            url: format!("{base}/{path}"),
            bearer: Some("gw_test".into()),
            via_gateway: true,
        }
    }

    #[tokio::test]
    async fn every_endpoint_speaks_the_contract() {
        let (base, seen) = mock_doc_render().await;
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("รายงาน.pdf");
        std::fs::write(&pdf, b"%PDF-1.7 fake").unwrap();

        let text = pdf_text(&ep(&base, "pdf/text"), &pdf, Some(1), Some(2), "layout")
            .await
            .unwrap();
        assert_eq!(text.page_count, 3);
        assert_eq!(text.pages.len(), 2);

        let pages = pdf_pages(&ep(&base, "pdf/pages"), &pdf, Some(2), Some(3), 150)
            .await
            .unwrap();
        assert_eq!(pages.pages.iter().map(|p| p.n).collect::<Vec<_>>(), [2, 3]);

        let o = ocr(&ep(&base, "ocr"), &pdf, None, None).await.unwrap();
        assert_eq!(o.pages[1].confidence, Some(41.0));

        let out = convert(&ep(&base, "convert"), &pdf, "docx").await.unwrap();
        assert_eq!(out, "converted:รายงาน.pdf:docx".as_bytes());

        let html = dir.path().join("r.html");
        std::fs::write(&html, "<img src='a.png'>").unwrap();
        html_to_pdf(
            &ep(&base, "html-to-pdf"),
            &html,
            vec![
                ("a.png".into(), b"png".to_vec()),
                ("s.css".into(), b"p{}".to_vec()),
            ],
        )
        .await
        .unwrap();

        let seen = seen.lock().unwrap();
        let by = |p: &str| seen.iter().find(|(path, _)| path == p).unwrap().1.clone();
        let t = by("/pdf/text");
        assert_eq!(t["file"], "รายงาน.pdf");
        assert_eq!((t["first"].as_str(), t["last"].as_str()), ("1", "2"));
        assert_eq!(t["mode"], "layout");
        assert_eq!(t["auth"], "Bearer gw_test");
        assert_eq!(by("/pdf/pages")["dpi"], "150");
        assert_eq!(by("/ocr")["lang"], "tha+eng");
        assert_eq!(by("/convert")["to"], "docx");
        assert_eq!(by("/html-to-pdf")["assets[]"], "a.png,s.css");
    }

    #[tokio::test]
    async fn a_refusal_reaches_the_caller_in_the_service_words() {
        let (base, _) = mock_doc_render().await;
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("x.bad");
        std::fs::write(&bad, b"nope").unwrap();
        let err = convert(&ep(&base, "convert"), &bad, "pdf")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cannot read this file"), "{msg}");
        assert!(msg.contains("422"), "{msg}");
    }

    #[test]
    fn ocr_thai_spaces_are_joined_but_wide_gaps_and_latin_stay() {
        assert_eq!(
            join_ocr_thai_spaces("ส่วนราชการ ฝ่ายบริหารทรัพยากรบุคคล โทร. 1234"),
            "ส่วนราชการ ฝ่ายบริหารทรัพยากรบุคคล โทร. 1234",
            "real phrase breaks on an untokenised page stay"
        );
        assert_eq!(join_ocr_thai_spaces("บันทึก ข้อ ความ"), "บันทึกข้อความ");
        assert_eq!(join_ocr_thai_spaces("ล า ย มือ"), "ลายมือ");
        assert_eq!(
            join_ocr_thai_spaces("งบ ประมาณ 6,000,000 บาท"),
            "งบ ประมาณ 6,000,000 บาท",
            "one gap in eleven Thai characters is a phrase break, not tokenising"
        );
        assert_eq!(
            join_ocr_thai_spaces("ง บ ป ระ มาณ 6,000,000 บาท"),
            "งบประมาณ 6,000,000 บาท"
        );
        assert_eq!(join_ocr_thai_spaces("ชื่อ  นามสกุล"), "ชื่อ  นามสกุล");
        assert_eq!(join_ocr_thai_spaces("Hello world"), "Hello world");
    }

    #[test]
    fn damaged_ocr_is_spotted() {
        let noisy = "aw ๕-    9\nบนทกขอความ\nส่วนราชการ ฝ่ายบริหารทรัพยากรบุคคล\n๑7\n\
                     เรือง ขออนุมัติ โดอบรมการใช้งานปัญญาประ ะดิษฐ์\nad\nเรียน กรรมการผู้จัดการ\n\
                     ด้วยฝ่ายบริหารทรัพยากรบุคคลมีแผนพัฒนาทักษะดิ ะดติจิทัลของ\nขบ";
        assert!(ocr_looks_damaged(noisy));
        let clean = "บันทึกข้อความ\nส่วนราชการ ฝ่ายบริหารทรัพยากรบุคคล โทร. 1234\n\
                     ที่ บค 045/2569 วันที่ 20 ตุลาคม 2569\n2\nเรียน กรรมการผู้จัดการ";
        assert!(!ocr_looks_damaged(clean));
        assert!(!ocr_looks_damaged(""));
    }
}
