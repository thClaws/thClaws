//! `EpubCreate` — render markdown to a reflowable EPUB 3 e-book.
//!
//! Unlike `PdfCreate`, an EPUB does no fixed layout of its own: the
//! reading system reflows the content, so this tool's job is to turn
//! markdown into clean XHTML + CSS and package it as a spec-compliant
//! EPUB container. That means no glyph metrics or shaping here — but it
//! still optionally embeds Noto Sans + Noto Sans Thai via `@font-face`
//! so Thai renders correctly even on readers that ship no Thai font.
//!
//! What it produces:
//! - **Chapter splitting**: the markdown is split into separate XHTML
//!   documents at headings of `chapter_split` level (1 = each H1, the
//!   default; 2 = H1+H2; 0 = one document). Each becomes a spine item
//!   and a navigation entry, so e-readers get a real chapter list.
//! - **Markdown → XHTML** via `pulldown-cmark` with GFM tables,
//!   strikethrough, task lists and footnotes enabled.
//! - **Images**: `![alt](path)` references are copied into the
//!   container (`OEBPS/images/`), de-duplicated, added to the manifest,
//!   and their `src` rewritten to the in-container path.
//! - **Cover**: an optional `cover` image becomes the EPUB cover (a
//!   `cover.xhtml` page first in the spine + `cover-image` metadata).
//! - **Navigation**: an EPUB 3 `nav.xhtml` plus an EPUB 2 `toc.ncx`
//!   fallback so old readers still get a table of contents.
//! - **Packaging**: `mimetype` is written first and STORED
//!   (uncompressed) per the OCF spec; everything else is deflated.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use pulldown_cmark::{CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;

// Embedded fonts (shared with PdfCreate's `resources/fonts/`). Embedded
// so an EPUB authored with Thai text is self-contained — the reader
// doesn't need a system Thai font.
const FONT_LATIN_REG: &[u8] = include_bytes!("../../resources/fonts/NotoSans-Regular.ttf");
const FONT_LATIN_BOLD: &[u8] = include_bytes!("../../resources/fonts/NotoSans-Bold.ttf");
const FONT_LATIN_ITAL: &[u8] = include_bytes!("../../resources/fonts/NotoSans-Italic.ttf");
const FONT_THAI_REG: &[u8] = include_bytes!("../../resources/fonts/NotoSansThai-Regular.ttf");
const FONT_THAI_BOLD: &[u8] = include_bytes!("../../resources/fonts/NotoSansThai-Bold.ttf");
// Serif counterparts — selected via the `font: "serif"` option.
const FONT_LATIN_REG_SERIF: &[u8] = include_bytes!("../../resources/fonts/NotoSerif-Regular.ttf");
const FONT_LATIN_BOLD_SERIF: &[u8] = include_bytes!("../../resources/fonts/NotoSerif-Bold.ttf");
const FONT_LATIN_ITAL_SERIF: &[u8] = include_bytes!("../../resources/fonts/NotoSerif-Italic.ttf");
const FONT_THAI_REG_SERIF: &[u8] =
    include_bytes!("../../resources/fonts/NotoSerifThai-Regular.ttf");
const FONT_THAI_BOLD_SERIF: &[u8] = include_bytes!("../../resources/fonts/NotoSerifThai-Bold.ttf");

const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

pub struct EpubCreateTool;

#[async_trait]
impl Tool for EpubCreateTool {
    fn name(&self) -> &'static str {
        "EpubCreate"
    }

    fn description(&self) -> &'static str {
        "Render markdown to a reflowable EPUB 3 e-book. The reader handles \
         layout, so text reflows to any screen. Splits the markdown into \
         chapters at headings (each H1 by default → its own spine item + \
         navigation entry), converts markdown to XHTML (headings, \
         **bold**/*italic*/`code`, ordered+unordered lists, blockquotes, \
         horizontal rules, fenced code blocks, GFM tables, strikethrough, \
         task lists, footnotes), embeds `![alt](path)` images into the \
         container, and builds an EPUB 3 nav document plus a toc.ncx \
         fallback for older readers. Embeds Noto Sans + Noto Sans Thai \
         (via @font-face) by default — or set `font: \"serif\"` for Noto \
         Serif + Noto Serif Thai — so Thai renders correctly even on \
         readers with no Thai font. Pass `content` inline for small books \
         or `content_path` to render a markdown FILE without copying it \
         through the conversation (relative image paths then resolve \
         against that file's directory — use this for book exports). \
         Optional `cover` image, `author`, and `language` (BCP-47, e.g. \
         \"th\") metadata."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":         {"type": "string", "description": "Output EPUB path. Parent directories are created if missing."},
                "content":      {"type": "string", "description": "Markdown content to render. Provide this OR content_path."},
                "content_path": {"type": "string", "description": "Path to a markdown file to render. Preferred for large documents (books) — the file is read directly, and relative image paths inside it resolve against its directory."},
                "title":        {"type": "string", "description": "Book title (metadata). Optional — defaults to the file stem."},
                "author":       {"type": "string", "description": "Author / creator metadata. Optional."},
                "language":     {"type": "string", "description": "BCP-47 language code (e.g. \"en\", \"th\"). Default \"en\"."},
                "cover":        {"type": "string", "description": "Path to a cover image (PNG/JPEG). Optional — becomes the EPUB cover."},
                "chapter_split": {"type": "integer", "enum": [0, 1, 2], "description": "Split into chapter files at this heading level: 0 = single document, 1 = split at each H1 (default), 2 = split at H1+H2."},
                "font":         {"type": "string", "enum": ["sans", "serif"], "description": "Typeface family for the embedded fonts. 'sans' (default) = Noto Sans + Noto Sans Thai; 'serif' = Noto Serif + Noto Serif Thai. Only applies when embed_fonts is true."},
                "embed_fonts":  {"type": "boolean", "description": "Embed the chosen font family (see `font`) so Thai renders without a system font. Default true."}
            },
            "required": ["path"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_path = req_str(&input, "path")?;
        let out_path = crate::sandbox::Sandbox::check_write(raw_path)?;

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

        let title = input
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| {
                Path::new(raw_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Untitled")
                    .to_string()
            });

        let author = input
            .get("author")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);

        let language = input
            .get("language")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("en")
            .to_string();

        let chapter_split = input
            .get("chapter_split")
            .and_then(|v| v.as_u64())
            .map(|n| n.min(2) as u8)
            .unwrap_or(1);

        let embed_fonts = input
            .get("embed_fonts")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let serif = input.get("font").and_then(|v| v.as_str()) == Some("serif");

        // Resolve the cover path (if any) up front so a bad path fails
        // before we start writing.
        let cover_path = match input.get("cover").and_then(|v| v.as_str()) {
            Some(c) if !c.trim().is_empty() => {
                Some(crate::sandbox::Sandbox::check(c.trim())?.to_path_buf())
            }
            _ => None,
        };

        if let Some(parent) = Path::new(&*out_path.to_string_lossy()).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::Tool(format!("mkdir {}: {}", parent.display(), e)))?;
            }
        }

        let spec = EpubSpec {
            out_path: out_path.to_path_buf(),
            title,
            author,
            language,
            cover_path,
            chapter_split,
            embed_fonts,
            serif,
            content,
            image_base,
        };

        let (n_chapters, n_images) =
            crate::workdir::spawn_blocking_scoped(move || render_epub(spec))
                .await
                .map_err(|e| Error::Tool(format!("EPUB worker join failed: {e}")))??;

        Ok(format!(
            "Wrote EPUB to {} ({} chapter{}, {} image{})",
            out_path.display(),
            n_chapters,
            if n_chapters == 1 { "" } else { "s" },
            n_images,
            if n_images == 1 { "" } else { "s" },
        ))
    }
}

struct EpubSpec {
    out_path: PathBuf,
    title: String,
    author: Option<String>,
    language: String,
    cover_path: Option<PathBuf>,
    chapter_split: u8,
    embed_fonts: bool,
    serif: bool,
    content: String,
    image_base: PathBuf,
}

/// One chapter: a heading-derived title + the rendered XHTML body.
struct Chapter {
    title: String,
    body: String,
}

/// An image to embed: its in-container path + bytes + media type.
struct ImageAsset {
    epub_path: String, // e.g. "images/img001.png"
    media_type: String,
    bytes: Vec<u8>,
}

fn render_epub(spec: EpubSpec) -> Result<(usize, usize)> {
    // 1. Parse markdown → events, rewriting image URLs to in-container
    //    paths and collecting the image bytes to embed.
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);

    // Repair LLM-miscounted table delimiter rows so GFM tables aren't silently
    // demoted to paragraphs (see tools::md_tables).
    let repaired = super::md_tables::repair_table_delimiters(&spec.content);
    let events: Vec<Event> = Parser::new_ext(&repaired, opts).collect();

    let mut images: Vec<ImageAsset> = Vec::new();
    let mut seen: HashMap<String, String> = HashMap::new(); // src → epub_path
    let rewritten = rewrite_images(events, &spec.image_base, &mut images, &mut seen);

    // 2. Split into chapters at the configured heading level.
    let chapters = split_chapters(rewritten, spec.chapter_split, &spec.title);
    let n_chapters = chapters.len();

    // 3. Build all the container parts.
    let uuid = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    let modified = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let cover_asset = match &spec.cover_path {
        Some(p) => Some(load_cover(p)?),
        None => None,
    };

    // 4. Write the ZIP container.
    let file = std::fs::File::create(&spec.out_path)
        .map_err(|e| Error::Tool(format!("create {}: {e}", spec.out_path.display())))?;
    let mut zip = zip::ZipWriter::new(BufWriter::new(file));
    let stored = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let deflated =
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let zerr = |e: zip::result::ZipError| Error::Tool(format!("zip: {e}"));
    let werr = |e: std::io::Error| Error::Tool(format!("zip write: {e}"));

    // mimetype MUST be first and uncompressed (OCF §4).
    zip.start_file("mimetype", stored).map_err(zerr)?;
    zip.write_all(b"application/epub+zip").map_err(werr)?;

    zip.start_file("META-INF/container.xml", deflated)
        .map_err(zerr)?;
    zip.write_all(CONTAINER_XML.as_bytes()).map_err(werr)?;

    // Chapter documents.
    let chapter_files: Vec<String> = (0..n_chapters)
        .map(|i| format!("ch{:03}.xhtml", i + 1))
        .collect();
    for (i, ch) in chapters.iter().enumerate() {
        let xhtml = wrap_xhtml(&ch.title, &ch.body, &spec.language);
        zip.start_file(format!("OEBPS/{}", chapter_files[i]), deflated)
            .map_err(zerr)?;
        zip.write_all(xhtml.as_bytes()).map_err(werr)?;
    }

    // Cover page + image.
    if let Some(cover) = &cover_asset {
        zip.start_file(format!("OEBPS/{}", cover.epub_path), deflated)
            .map_err(zerr)?;
        zip.write_all(&cover.bytes).map_err(werr)?;
        let cover_page = cover_xhtml(&cover.epub_path, &spec.title, &spec.language);
        zip.start_file("OEBPS/cover.xhtml", deflated)
            .map_err(zerr)?;
        zip.write_all(cover_page.as_bytes()).map_err(werr)?;
    }

    // Body images.
    for img in &images {
        zip.start_file(format!("OEBPS/{}", img.epub_path), deflated)
            .map_err(zerr)?;
        zip.write_all(&img.bytes).map_err(werr)?;
    }

    // Stylesheet.
    zip.start_file("OEBPS/style.css", deflated).map_err(zerr)?;
    zip.write_all(stylesheet(spec.embed_fonts, spec.serif).as_bytes())
        .map_err(werr)?;

    // Embedded fonts.
    if spec.embed_fonts {
        for (name, bytes) in font_files(spec.serif) {
            zip.start_file(format!("OEBPS/fonts/{name}"), deflated)
                .map_err(zerr)?;
            zip.write_all(bytes).map_err(werr)?;
        }
    }

    // Navigation (EPUB 3) + NCX (EPUB 2 fallback).
    let nav = nav_xhtml(&chapters, &chapter_files, &spec.language);
    zip.start_file("OEBPS/nav.xhtml", deflated).map_err(zerr)?;
    zip.write_all(nav.as_bytes()).map_err(werr)?;

    let ncx = toc_ncx(&chapters, &chapter_files, &uuid, &spec.title);
    zip.start_file("OEBPS/toc.ncx", deflated).map_err(zerr)?;
    zip.write_all(ncx.as_bytes()).map_err(werr)?;

    // Package document (manifest + spine + metadata).
    let opf = content_opf(
        &spec,
        &uuid,
        &modified,
        &chapter_files,
        &images,
        &cover_asset,
    );
    zip.start_file("OEBPS/content.opf", deflated)
        .map_err(zerr)?;
    zip.write_all(opf.as_bytes()).map_err(werr)?;

    zip.finish().map_err(zerr)?;

    Ok((n_chapters, images.len()))
}

/// Rewrite every image `dest_url` to an in-container path and collect
/// the image bytes. Missing/oversize/unreadable images keep their
/// original URL (so the book still builds) and are not embedded.
fn rewrite_images<'a>(
    events: Vec<Event<'a>>,
    image_base: &Path,
    images: &mut Vec<ImageAsset>,
    seen: &mut HashMap<String, String>,
) -> Vec<Event<'a>> {
    events
        .into_iter()
        .map(|ev| match ev {
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let new_url = match resolve_image(&dest_url, image_base, images, seen) {
                    Some(p) => CowStr::Boxed(p.into_boxed_str()),
                    None => dest_url,
                };
                Event::Start(Tag::Image {
                    link_type,
                    dest_url: new_url,
                    title,
                    id,
                })
            }
            other => other,
        })
        .collect()
}

/// Resolve one image source to an in-container path, reading + caching
/// its bytes. Returns `None` (leave URL as-is) for remote URLs, missing
/// files, oversize files, or read errors.
fn resolve_image(
    src: &str,
    image_base: &Path,
    images: &mut Vec<ImageAsset>,
    seen: &mut HashMap<String, String>,
) -> Option<String> {
    if src.starts_with("http://") || src.starts_with("https://") || src.starts_with("data:") {
        return None;
    }
    if let Some(existing) = seen.get(src) {
        return Some(existing.clone());
    }
    let candidates = [image_base.join(src), PathBuf::from(src)];
    let found = candidates.iter().find(|p| p.is_file())?;
    let meta = std::fs::metadata(found).ok()?;
    if meta.len() == 0 || meta.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let bytes = std::fs::read(found).ok()?;
    let (media_type, ext) = media_type_for(found, &bytes);
    let epub_path = format!("images/img{:03}.{ext}", images.len() + 1);
    images.push(ImageAsset {
        epub_path: epub_path.clone(),
        media_type,
        bytes,
    });
    seen.insert(src.to_string(), epub_path.clone());
    Some(epub_path)
}

fn load_cover(path: &Path) -> Result<ImageAsset> {
    let bytes = std::fs::read(path)
        .map_err(|e| Error::Tool(format!("read cover {}: {e}", path.display())))?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(Error::Tool(format!(
            "cover {} is empty or larger than 10 MB",
            path.display()
        )));
    }
    let (media_type, ext) = media_type_for(path, &bytes);
    Ok(ImageAsset {
        epub_path: format!("cover.{ext}"),
        media_type,
        bytes,
    })
}

/// Detect media type + canonical extension from magic bytes, falling
/// back to the file extension.
fn media_type_for(path: &Path, bytes: &[u8]) -> (String, String) {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return ("image/png".into(), "png".into());
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return ("image/jpeg".into(), "jpg".into());
    }
    if bytes.starts_with(b"GIF8") {
        return ("image/gif".into(), "gif".into());
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return ("image/webp".into(), "webp".into());
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => ("image/png".into(), "png".into()),
        "jpg" | "jpeg" => ("image/jpeg".into(), "jpg".into()),
        "gif" => ("image/gif".into(), "gif".into()),
        "webp" => ("image/webp".into(), "webp".into()),
        "svg" => ("image/svg+xml".into(), "svg".into()),
        other if !other.is_empty() => (format!("image/{other}"), other.into()),
        _ => ("image/png".into(), "png".into()),
    }
}

/// Split the event stream into chapters at headings of `level` (1 or 2).
/// `level == 0` keeps everything in one chapter titled `book_title`.
fn split_chapters(events: Vec<Event>, level: u8, book_title: &str) -> Vec<Chapter> {
    if level == 0 {
        let body = render_html(&events);
        return vec![Chapter {
            title: book_title.to_string(),
            body,
        }];
    }

    let mut chapters: Vec<Chapter> = Vec::new();
    let mut cur: Vec<Event> = Vec::new();
    let mut cur_title = String::new();
    let mut capturing = false;

    let flush = |events: &mut Vec<Event>, title: &mut String, out: &mut Vec<Chapter>| {
        if events.is_empty() {
            return;
        }
        let t = title.trim().to_string();
        let label = if t.is_empty() {
            format!("Section {}", out.len() + 1)
        } else {
            t
        };
        out.push(Chapter {
            title: label,
            body: render_html(events),
        });
        events.clear();
        title.clear();
    };

    for ev in events {
        match &ev {
            Event::Start(Tag::Heading { level: hl, .. }) if heading_num(*hl) <= level => {
                flush(&mut cur, &mut cur_title, &mut chapters);
                capturing = true;
                cur.push(ev);
            }
            Event::End(TagEnd::Heading(_)) if capturing => {
                capturing = false;
                cur.push(ev);
            }
            Event::Text(t) | Event::Code(t) if capturing => {
                cur_title.push_str(t);
                cur.push(ev);
            }
            _ => cur.push(ev),
        }
    }
    flush(&mut cur, &mut cur_title, &mut chapters);

    if chapters.is_empty() {
        chapters.push(Chapter {
            title: book_title.to_string(),
            body: String::new(),
        });
    }
    chapters
}

fn heading_num(h: HeadingLevel) -> u8 {
    match h {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn render_html(events: &[Event]) -> String {
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, events.iter().cloned());
    out
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn wrap_xhtml(title: &str, body: &str, lang: &str) -> String {
    let l = xml_escape(lang);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" lang="{l}" xml:lang="{l}">
<head>
<meta charset="utf-8"/>
<title>{}</title>
<link rel="stylesheet" type="text/css" href="style.css"/>
</head>
<body>
{}
</body>
</html>
"#,
        xml_escape(title),
        body
    )
}

fn cover_xhtml(cover_img: &str, title: &str, lang: &str) -> String {
    let l = xml_escape(lang);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" lang="{l}" xml:lang="{l}">
<head>
<meta charset="utf-8"/>
<title>{t}</title>
<style>body{{margin:0;padding:0;text-align:center;}} img{{max-width:100%;max-height:100vh;}}</style>
</head>
<body epub:type="cover">
<section epub:type="cover">
<img src="{img}" alt="{t}"/>
</section>
</body>
</html>
"#,
        l = l,
        t = xml_escape(title),
        img = xml_escape(cover_img),
    )
}

fn nav_xhtml(chapters: &[Chapter], files: &[String], lang: &str) -> String {
    let l = xml_escape(lang);
    let mut items = String::new();
    for (ch, f) in chapters.iter().zip(files) {
        items.push_str(&format!(
            "      <li><a href=\"{}\">{}</a></li>\n",
            xml_escape(f),
            xml_escape(&ch.title)
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" lang="{l}" xml:lang="{l}">
<head>
<meta charset="utf-8"/>
<title>Contents</title>
<link rel="stylesheet" type="text/css" href="style.css"/>
</head>
<body>
<nav epub:type="toc" id="toc">
<h1>Contents</h1>
<ol>
{items}</ol>
</nav>
</body>
</html>
"#,
    )
}

fn toc_ncx(chapters: &[Chapter], files: &[String], uuid: &str, title: &str) -> String {
    let mut points = String::new();
    for (i, (ch, f)) in chapters.iter().zip(files).enumerate() {
        points.push_str(&format!(
            r#"    <navPoint id="navpoint-{n}" playOrder="{n}">
      <navLabel><text>{label}</text></navLabel>
      <content src="{src}"/>
    </navPoint>
"#,
            n = i + 1,
            label = xml_escape(&ch.title),
            src = xml_escape(f),
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE ncx PUBLIC "-//NISO//DTD ncx 2005-1//EN" "http://www.daisy.org/z3986/2005/ncx-2005-1.dtd">
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head>
    <meta name="dtb:uid" content="{uid}"/>
    <meta name="dtb:depth" content="1"/>
    <meta name="dtb:totalPageCount" content="0"/>
    <meta name="dtb:maxPageNumber" content="0"/>
  </head>
  <docTitle><text>{title}</text></docTitle>
  <navMap>
{points}  </navMap>
</ncx>
"#,
        uid = xml_escape(uuid),
        title = xml_escape(title),
    )
}

fn content_opf(
    spec: &EpubSpec,
    uuid: &str,
    modified: &str,
    chapter_files: &[String],
    images: &[ImageAsset],
    cover: &Option<ImageAsset>,
) -> String {
    let mut metadata = String::new();
    metadata.push_str(&format!(
        "    <dc:identifier id=\"bookid\">{}</dc:identifier>\n",
        xml_escape(uuid)
    ));
    metadata.push_str(&format!(
        "    <dc:title>{}</dc:title>\n",
        xml_escape(&spec.title)
    ));
    metadata.push_str(&format!(
        "    <dc:language>{}</dc:language>\n",
        xml_escape(&spec.language)
    ));
    if let Some(a) = &spec.author {
        metadata.push_str(&format!("    <dc:creator>{}</dc:creator>\n", xml_escape(a)));
    }
    metadata.push_str(&format!(
        "    <meta property=\"dcterms:modified\">{}</meta>\n",
        xml_escape(modified)
    ));
    if cover.is_some() {
        metadata.push_str("    <meta name=\"cover\" content=\"cover-image\"/>\n");
    }

    let mut manifest = String::new();
    manifest.push_str(
        "    <item id=\"nav\" href=\"nav.xhtml\" properties=\"nav\" media-type=\"application/xhtml+xml\"/>\n",
    );
    manifest.push_str(
        "    <item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n",
    );
    manifest.push_str("    <item id=\"css\" href=\"style.css\" media-type=\"text/css\"/>\n");

    if let Some(c) = cover {
        manifest.push_str(&format!(
            "    <item id=\"cover-image\" href=\"{}\" media-type=\"{}\" properties=\"cover-image\"/>\n",
            xml_escape(&c.epub_path),
            xml_escape(&c.media_type),
        ));
        manifest.push_str(
            "    <item id=\"cover-page\" href=\"cover.xhtml\" media-type=\"application/xhtml+xml\"/>\n",
        );
    }

    for (i, f) in chapter_files.iter().enumerate() {
        manifest.push_str(&format!(
            "    <item id=\"ch{:03}\" href=\"{}\" media-type=\"application/xhtml+xml\"/>\n",
            i + 1,
            xml_escape(f),
        ));
    }
    for (i, img) in images.iter().enumerate() {
        manifest.push_str(&format!(
            "    <item id=\"img{:03}\" href=\"{}\" media-type=\"{}\"/>\n",
            i + 1,
            xml_escape(&img.epub_path),
            xml_escape(&img.media_type),
        ));
    }
    if spec.embed_fonts {
        for (id, name) in font_manifest_ids(spec.serif) {
            manifest.push_str(&format!(
                "    <item id=\"{id}\" href=\"fonts/{name}\" media-type=\"font/ttf\"/>\n",
            ));
        }
    }

    let mut spine = String::new();
    if cover.is_some() {
        spine.push_str("    <itemref idref=\"cover-page\"/>\n");
    }
    spine.push_str("    <itemref idref=\"nav\"/>\n");
    for i in 0..chapter_files.len() {
        spine.push_str(&format!("    <itemref idref=\"ch{:03}\"/>\n", i + 1));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid" xml:lang="{lang}">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
{metadata}  </metadata>
  <manifest>
{manifest}  </manifest>
  <spine toc="ncx">
{spine}  </spine>
</package>
"#,
        lang = xml_escape(&spec.language),
    )
}

fn stylesheet(embed_fonts: bool, serif: bool) -> String {
    let mut css = String::new();
    if embed_fonts {
        css.push_str(if serif {
            r#"@font-face { font-family: "Noto Serif"; font-weight: normal; font-style: normal; src: url("fonts/NotoSerif-Regular.ttf"); }
@font-face { font-family: "Noto Serif"; font-weight: bold; font-style: normal; src: url("fonts/NotoSerif-Bold.ttf"); }
@font-face { font-family: "Noto Serif"; font-weight: normal; font-style: italic; src: url("fonts/NotoSerif-Italic.ttf"); }
@font-face { font-family: "Noto Serif Thai"; font-weight: normal; font-style: normal; src: url("fonts/NotoSerifThai-Regular.ttf"); }
@font-face { font-family: "Noto Serif Thai"; font-weight: bold; font-style: normal; src: url("fonts/NotoSerifThai-Bold.ttf"); }
"#
        } else {
            r#"@font-face { font-family: "Noto Sans"; font-weight: normal; font-style: normal; src: url("fonts/NotoSans-Regular.ttf"); }
@font-face { font-family: "Noto Sans"; font-weight: bold; font-style: normal; src: url("fonts/NotoSans-Bold.ttf"); }
@font-face { font-family: "Noto Sans"; font-weight: normal; font-style: italic; src: url("fonts/NotoSans-Italic.ttf"); }
@font-face { font-family: "Noto Sans Thai"; font-weight: normal; font-style: normal; src: url("fonts/NotoSansThai-Regular.ttf"); }
@font-face { font-family: "Noto Sans Thai"; font-weight: bold; font-style: normal; src: url("fonts/NotoSansThai-Bold.ttf"); }
"#
        });
    }
    let family = match (embed_fonts, serif) {
        (true, true) => "\"Noto Serif\", \"Noto Serif Thai\", serif",
        (true, false) => "\"Noto Sans\", \"Noto Sans Thai\", sans-serif",
        (false, true) => "serif",
        (false, false) => "sans-serif",
    };
    css.push_str(&format!(
        r#"html {{ font-size: 100%; }}
body {{ font-family: {family}; line-height: 1.6; margin: 0 5%; padding: 1em 0; color: #1a1a1a; }}
h1, h2, h3, h4, h5, h6 {{ line-height: 1.25; margin: 1.4em 0 0.5em; font-weight: bold; }}
h1 {{ font-size: 1.8em; }}
h2 {{ font-size: 1.45em; }}
h3 {{ font-size: 1.2em; }}
p {{ margin: 0 0 0.9em; text-align: justify; }}
a {{ color: #0d6efd; text-decoration: none; }}
code {{ font-family: monospace; background: #f0f0f0; padding: 0.1em 0.3em; border-radius: 3px; font-size: 0.9em; }}
pre {{ background: #f5f5f5; padding: 0.8em 1em; border-radius: 5px; overflow-x: auto; white-space: pre-wrap; }}
pre code {{ background: none; padding: 0; }}
blockquote {{ margin: 1em 0; padding: 0.2em 1em; border-left: 4px solid #ccc; color: #555; }}
hr {{ border: none; border-top: 1px solid #ccc; margin: 2em 0; }}
img {{ max-width: 100%; height: auto; }}
figure {{ margin: 1.2em 0; text-align: center; }}
table {{ border-collapse: collapse; width: 100%; margin: 1em 0; }}
th, td {{ border: 1px solid #ccc; padding: 0.4em 0.6em; text-align: left; }}
th {{ background: #f0f0f0; font-weight: bold; }}
ul, ol {{ margin: 0 0 0.9em; padding-left: 1.6em; }}
li {{ margin: 0.2em 0; }}
"#,
    ));
    css
}

fn font_files(serif: bool) -> [(&'static str, &'static [u8]); 5] {
    if serif {
        [
            ("NotoSerif-Regular.ttf", FONT_LATIN_REG_SERIF),
            ("NotoSerif-Bold.ttf", FONT_LATIN_BOLD_SERIF),
            ("NotoSerif-Italic.ttf", FONT_LATIN_ITAL_SERIF),
            ("NotoSerifThai-Regular.ttf", FONT_THAI_REG_SERIF),
            ("NotoSerifThai-Bold.ttf", FONT_THAI_BOLD_SERIF),
        ]
    } else {
        [
            ("NotoSans-Regular.ttf", FONT_LATIN_REG),
            ("NotoSans-Bold.ttf", FONT_LATIN_BOLD),
            ("NotoSans-Italic.ttf", FONT_LATIN_ITAL),
            ("NotoSansThai-Regular.ttf", FONT_THAI_REG),
            ("NotoSansThai-Bold.ttf", FONT_THAI_BOLD),
        ]
    }
}

fn font_manifest_ids(serif: bool) -> [(&'static str, &'static str); 5] {
    if serif {
        [
            ("font-latin-reg", "NotoSerif-Regular.ttf"),
            ("font-latin-bold", "NotoSerif-Bold.ttf"),
            ("font-latin-ital", "NotoSerif-Italic.ttf"),
            ("font-thai-reg", "NotoSerifThai-Regular.ttf"),
            ("font-thai-bold", "NotoSerifThai-Bold.ttf"),
        ]
    } else {
        [
            ("font-latin-reg", "NotoSans-Regular.ttf"),
            ("font-latin-bold", "NotoSans-Bold.ttf"),
            ("font-latin-ital", "NotoSans-Italic.ttf"),
            ("font-thai-reg", "NotoSansThai-Regular.ttf"),
            ("font-thai-bold", "NotoSansThai-Bold.ttf"),
        ]
    }
}

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn read_zip_entry(epub: &Path, name: &str) -> Option<String> {
        let f = std::fs::File::open(epub).ok()?;
        let mut zip = zip::ZipArchive::new(f).ok()?;
        let mut entry = zip.by_name(name).ok()?;
        let mut s = String::new();
        entry.read_to_string(&mut s).ok()?;
        Some(s)
    }

    fn zip_names(epub: &Path) -> Vec<String> {
        let f = std::fs::File::open(epub).unwrap();
        let zip = zip::ZipArchive::new(f).unwrap();
        zip.file_names().map(String::from).collect()
    }

    #[tokio::test]
    async fn basic_epub_is_well_formed() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("book.epub");
        let res = EpubCreateTool
            .call(json!({
                "path": out.to_string_lossy(),
                "title": "Test Book",
                "author": "Ada",
                "content": "# Chapter One\n\nHello **world**.\n\n# Chapter Two\n\nMore text.\n"
            }))
            .await
            .unwrap();
        assert!(res.contains("2 chapters"), "got: {res}");

        let names = zip_names(&out);
        assert!(names.contains(&"mimetype".to_string()));
        assert!(names.contains(&"META-INF/container.xml".to_string()));
        assert!(names.contains(&"OEBPS/content.opf".to_string()));
        assert!(names.contains(&"OEBPS/nav.xhtml".to_string()));
        assert!(names.contains(&"OEBPS/toc.ncx".to_string()));
        assert!(names.contains(&"OEBPS/ch001.xhtml".to_string()));
        assert!(names.contains(&"OEBPS/ch002.xhtml".to_string()));

        let opf = read_zip_entry(&out, "OEBPS/content.opf").unwrap();
        assert!(opf.contains("<dc:title>Test Book</dc:title>"));
        assert!(opf.contains("<dc:creator>Ada</dc:creator>"));
        assert!(opf.contains("dcterms:modified"));

        let nav = read_zip_entry(&out, "OEBPS/nav.xhtml").unwrap();
        assert!(nav.contains("Chapter One"));
        assert!(nav.contains("Chapter Two"));
    }

    #[tokio::test]
    async fn mimetype_is_first_and_stored() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("m.epub");
        EpubCreateTool
            .call(json!({"path": out.to_string_lossy(), "content": "# A\n\ntext"}))
            .await
            .unwrap();
        let f = std::fs::File::open(&out).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        let first = zip.by_index(0).unwrap();
        assert_eq!(first.name(), "mimetype");
        assert_eq!(first.compression(), zip::CompressionMethod::Stored);
    }

    #[tokio::test]
    async fn thai_content_embeds_fonts() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("thai.epub");
        EpubCreateTool
            .call(json!({
                "path": out.to_string_lossy(),
                "language": "th",
                "content": "# บทที่ 1\n\nผำเป็นพืชน้ำขนาดเล็กที่สุดในโลก"
            }))
            .await
            .unwrap();
        let names = zip_names(&out);
        assert!(names.contains(&"OEBPS/fonts/NotoSansThai-Regular.ttf".to_string()));
        let css = read_zip_entry(&out, "OEBPS/style.css").unwrap();
        assert!(css.contains("Noto Sans Thai"));
        let opf = read_zip_entry(&out, "OEBPS/content.opf").unwrap();
        assert!(opf.contains("<dc:language>th</dc:language>"));
        assert!(opf.contains("font-thai-reg"));
    }

    #[tokio::test]
    async fn embeds_relative_image() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("images")).unwrap();
        // 1x1 PNG.
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        std::fs::write(dir.path().join("images/fig.png"), png).unwrap();
        let md = dir.path().join("ch.md");
        std::fs::write(&md, "# Ch\n\n![a figure](images/fig.png)\n").unwrap();
        let out = dir.path().join("img.epub");
        let res = EpubCreateTool
            .call(json!({"path": out.to_string_lossy(), "content_path": md.to_string_lossy()}))
            .await
            .unwrap();
        assert!(res.contains("1 image"), "got: {res}");
        let names = zip_names(&out);
        assert!(names.contains(&"OEBPS/images/img001.png".to_string()));
        let ch = read_zip_entry(&out, "OEBPS/ch001.xhtml").unwrap();
        assert!(ch.contains("images/img001.png"));
        let opf = read_zip_entry(&out, "OEBPS/content.opf").unwrap();
        assert!(opf.contains("media-type=\"image/png\""));
    }

    #[tokio::test]
    async fn chapter_split_zero_is_single_doc() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("single.epub");
        let res = EpubCreateTool
            .call(json!({
                "path": out.to_string_lossy(),
                "chapter_split": 0,
                "content": "# A\n\nx\n\n# B\n\ny"
            }))
            .await
            .unwrap();
        assert!(res.contains("1 chapter"), "got: {res}");
        let names = zip_names(&out);
        assert!(names.contains(&"OEBPS/ch001.xhtml".to_string()));
        assert!(!names.contains(&"OEBPS/ch002.xhtml".to_string()));
    }
}
