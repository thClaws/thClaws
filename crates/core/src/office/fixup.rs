//! Thai-rendering fixups for OOXML files saved by the ArtCraft apps
//! (GridCraft / DeckCraft / WordCraft), applied before a user opens them in
//! Office. Port of the spike's `craft_fixup.py` (dev-plan/70 §3, T1–T4):
//!
//! - xlsx: shared strings move into the cells as `inlineStr` — Excel drops the
//!   leading vowel (เงิน → งิน) when it reads GridCraft's sharedStrings.
//! - pptx: `latin` + `cs` font on every run property (PowerPoint draws Thai with
//!   `cs`), and line spacing on every slide paragraph so marks clear the line above.
//! - docx: line spacing, and hidden spelling/grammar marks (text is tagged en-US
//!   and Word for Mac has no Thai proofer).
//!
//! Regex-based like the original; every transform is idempotent.

use crate::error::{Error, Result};
use regex::bytes::{Captures, Regex};
use std::io::{Read, Write};
use std::path::Path;

pub struct FixupOptions {
    pub font: String,
    pub leading_pct: u32,
}

impl Default for FixupOptions {
    fn default() -> Self {
        Self {
            font: "Sarabun".into(),
            leading_pct: 125,
        }
    }
}

type Fix = Box<dyn Fn(&[u8], &str) -> Result<Vec<u8>>>;

#[derive(Clone, Copy)]
enum Kind {
    Xlsx,
    Pptx,
    Docx,
}

/// Rewrites the file in place (write to a temp file in the same dir, then rename). Returns Ok(true) if anything changed, Ok(false) if it was already fixed or the extension is not xlsx/pptx/docx.
pub fn fixup_file(path: &Path, opts: &FixupOptions) -> Result<bool> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let kind = match ext.as_deref() {
        Some("xlsx") => Kind::Xlsx,
        Some("pptx") => Kind::Pptx,
        Some("docx") => Kind::Docx,
        _ => return Ok(false),
    };

    let file = std::fs::File::open(path)?;
    let mut zin = zip::ZipArchive::new(file).map_err(|e| zip_err(path, e))?;
    let mut parts: Vec<(String, bool, Vec<u8>)> = Vec::with_capacity(zin.len());
    for i in 0..zin.len() {
        let mut f = zin.by_index(i).map_err(|e| zip_err(path, e))?;
        let mut buf = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut buf)?;
        parts.push((f.name().to_string(), f.is_dir(), buf));
    }

    let fix: Fix = match kind {
        Kind::Xlsx => {
            let Some((_, _, sst_xml)) = parts.iter().find(|(n, _, _)| n == "xl/sharedStrings.xml")
            else {
                return Ok(false);
            };
            let si = Regex::new(r"(?s)<si>(.*?)</si>").unwrap();
            let sst: Vec<Vec<u8>> = si.captures_iter(sst_xml).map(|c| c[1].to_vec()).collect();
            let x = Xlsx::new(sst);
            Box::new(move |b, n| x.fix(b, n))
        }
        Kind::Pptx => {
            let p = Pptx::new(&opts.font, opts.leading_pct);
            Box::new(move |b, n| Ok(p.fix(b, n)))
        }
        Kind::Docx => {
            let d = Docx::new(opts.leading_pct);
            Box::new(move |b, n| Ok(d.fix(b, n)))
        }
    };
    let drop = |n: &str| matches!(kind, Kind::Xlsx) && n == "xl/sharedStrings.xml";

    let mut changed = false;
    let mut out: Vec<(String, bool, Vec<u8>)> = Vec::with_capacity(parts.len());
    for (name, is_dir, data) in parts {
        if drop(&name) {
            changed = true;
            continue;
        }
        let fixed = if is_dir {
            data
        } else {
            let f = fix(&data, &name)?;
            changed |= f != data;
            f
        };
        out.push((name, is_dir, fixed));
    }
    if !changed {
        return Ok(false);
    }

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = tempfile::NamedTempFile::new_in(dir)?;
    {
        let mut zw = zip::ZipWriter::new(std::io::BufWriter::new(tmp.as_file()));
        let o = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, is_dir, data) in &out {
            if *is_dir {
                zw.add_directory(name.as_str(), o)
                    .map_err(|e| zip_err(path, e))?;
            } else {
                zw.start_file(name.as_str(), o)
                    .map_err(|e| zip_err(path, e))?;
                zw.write_all(data)?;
            }
        }
        zw.finish().map_err(|e| zip_err(path, e))?.flush()?;
    }
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(tmp.path(), meta.permissions());
    }
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(true)
}

fn zip_err(path: &Path, e: zip::result::ZipError) -> Error {
    Error::Tool(format!("{}: {e}", path.display()))
}

struct Xlsx {
    sst: Vec<Vec<u8>>,
    sheet: Regex,
    cell: Regex,
    ct: Regex,
    rel: Regex,
}

impl Xlsx {
    fn new(sst: Vec<Vec<u8>>) -> Self {
        Self {
            sst,
            sheet: Regex::new(r"^xl/worksheets/sheet\d+\.xml$").unwrap(),
            cell: Regex::new(r#"<c r="([A-Z]+\d+)"((?: s="\d+")?) t="s"><v>(\d+)</v></c>"#)
                .unwrap(),
            ct: Regex::new(r#"<Override PartName="/xl/sharedStrings.xml"[^>]*/>"#).unwrap(),
            rel: Regex::new(r"<Relationship [^>]*sharedStrings[^>]*/>").unwrap(),
        }
    }

    fn fix(&self, b: &[u8], name: &str) -> Result<Vec<u8>> {
        if self.sheet.is_match(name.as_bytes()) {
            for c in self.cell.captures_iter(b) {
                let i = idx(&c[3]);
                if i.is_none_or(|i| i >= self.sst.len()) {
                    return Err(Error::Tool(format!(
                        "{name}: shared string index out of range"
                    )));
                }
            }
            return Ok(self
                .cell
                .replace_all(b, |c: &Captures| {
                    let mut o = b"<c r=\"".to_vec();
                    o.extend_from_slice(&c[1]);
                    o.push(b'"');
                    o.extend_from_slice(&c[2]);
                    o.extend_from_slice(b" t=\"inlineStr\"><is>");
                    o.extend_from_slice(&self.sst[idx(&c[3]).unwrap()]);
                    o.extend_from_slice(b"</is></c>");
                    o
                })
                .into_owned());
        }
        Ok(match name {
            "[Content_Types].xml" => self.ct.replace_all(b, &b""[..]).into_owned(),
            "xl/_rels/workbook.xml.rels" => self.rel.replace_all(b, &b""[..]).into_owned(),
            _ => b.to_vec(),
        })
    }
}

fn idx(d: &[u8]) -> Option<usize> {
    std::str::from_utf8(d).ok()?.parse().ok()
}

struct Pptx {
    font: Vec<u8>,
    lnspc: Vec<u8>,
    part: Regex,
    old_font: Regex,
    tags: Vec<(&'static [u8], Regex, Regex)>,
    after_font: Regex,
    old_lnspc: Regex,
    ppr_empty: Regex,
    ppr_open: Regex,
    p_open: Regex,
}

impl Pptx {
    fn new(font: &str, leading_pct: u32) -> Self {
        let tags = [&b"a:rPr"[..], b"a:endParaRPr", b"a:defRPr"]
            .into_iter()
            .map(|t| {
                let s = std::str::from_utf8(t).unwrap();
                (
                    t,
                    Regex::new(&format!(r"<{s}((?: [^>]*?)?)/>")).unwrap(),
                    Regex::new(&format!(r"(?s)(<{s}(?: [^>]*)?>)(.*?)(</{s}>)")).unwrap(),
                )
            })
            .collect();
        Self {
            font: format!(r#"<a:latin typeface="{font}"/><a:cs typeface="{font}"/>"#).into_bytes(),
            lnspc: format!(
                r#"<a:lnSpc><a:spcPct val="{}"/></a:lnSpc>"#,
                leading_pct * 1000
            )
            .into_bytes(),
            part: Regex::new(
                r"^ppt/(slides/slide|slideLayouts/slideLayout|slideMasters/slideMaster)\d+\.xml$",
            )
            .unwrap(),
            old_font: Regex::new(r"<a:(?:latin|cs) [^>]*/>").unwrap(),
            tags,
            // schema order: fills/ln come before latin/ea/cs, hyperlinks and extLst after
            after_font: Regex::new(r"<a:(?:ea|sym|hlinkClick|hlinkMouseOver|rtl|extLst)[ />]")
                .unwrap(),
            old_lnspc: Regex::new(r"<a:lnSpc>.*?</a:lnSpc>").unwrap(),
            ppr_empty: Regex::new(r"<a:pPr([^>]*?)/>").unwrap(),
            ppr_open: Regex::new(r"<a:pPr((?: [^>]*)?)>").unwrap(),
            p_open: Regex::new(r"<a:p>").unwrap(),
        }
    }

    fn fix(&self, b: &[u8], name: &str) -> Vec<u8> {
        if !self.part.is_match(name.as_bytes()) {
            return b.to_vec();
        }
        let f = &self.font[..];
        let mut b = self.old_font.replace_all(b, &b""[..]).into_owned();
        for (tag, empty, full) in &self.tags {
            b = empty
                .replace_all(&b, |c: &Captures| {
                    cat(&[b"<", tag, &c[1], b">", f, b"</", tag, b">"])
                })
                .into_owned();
            b = full
                .replace_all(&b, |c: &Captures| {
                    let inner = &c[2];
                    if contains(inner, b"<a:latin") {
                        return c[0].to_vec();
                    }
                    let at = self
                        .after_font
                        .find(inner)
                        .map_or(inner.len(), |m| m.start());
                    cat(&[&c[1], &inner[..at], f, &inner[at..], &c[3]])
                })
                .into_owned();
        }
        if name.starts_with("ppt/slides/") {
            let l = &self.lnspc[..];
            b = self.old_lnspc.replace_all(&b, &b""[..]).into_owned();
            b = self
                .ppr_empty
                .replace_all(&b, |c: &Captures| {
                    cat(&[b"<a:pPr", &c[1], b">", l, b"</a:pPr>"])
                })
                .into_owned();
            b = replace_unless_followed(&self.ppr_open, &b, b"<a:lnSpc>", |c| {
                cat(&[b"<a:pPr", &c[1], b">", l])
            });
            b = replace_unless_followed(&self.p_open, &b, b"<a:pPr", |_| {
                cat(&[b"<a:p><a:pPr>", l, b"</a:pPr>"])
            });
        }
        b
    }
}

/// `re` followed by a negative lookahead for `next`, which the regex crate lacks.
fn replace_unless_followed(
    re: &Regex,
    b: &[u8],
    next: &[u8],
    rep: impl Fn(&Captures) -> Vec<u8>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut last = 0;
    for c in re.captures_iter(b) {
        let m = c.get(0).unwrap();
        if b[m.end()..].starts_with(next) {
            continue;
        }
        out.extend_from_slice(&b[last..m.start()]);
        out.extend_from_slice(&rep(&c));
        last = m.end();
    }
    out.extend_from_slice(&b[last..]);
    out
}

// CT_Settings children that come before hideSpellingErrors; the flags go before the first other child
const SETTINGS_BEFORE: &[&[u8]] = &[
    b"writeProtection",
    b"view",
    b"zoom",
    b"removePersonalInformation",
    b"removeDateAndTime",
    b"doNotDisplayPageBoundaries",
    b"displayBackgroundShape",
    b"printPostScriptOverText",
    b"printFractionalCharacterWidth",
    b"printFormsData",
    b"embedTrueTypeFonts",
    b"embedSystemFonts",
    b"saveSubsetFonts",
    b"saveFormsData",
    b"mirrorMargins",
    b"alignBordersAndEdges",
    b"bordersDoNotSurroundHeader",
    b"bordersDoNotSurroundFooter",
    b"gutterAtTop",
];

struct Docx {
    line: Vec<u8>,
    spacing: Regex,
    child: Regex,
    run: Regex,
    rpr: Regex,
    fonts: Regex,
    tagged: Regex,
    tail: Regex,
    tbl: Regex,
    tbl_after: Regex,
}

impl Docx {
    fn new(leading_pct: u32) -> Self {
        Self {
            line: ((240 * leading_pct as u64 + 50) / 100)
                .to_string()
                .into_bytes(),
            spacing: Regex::new(r#"(<w:spacing [^>]*?w:line=")\d+("[^>]*?w:lineRule="auto")"#)
                .unwrap(),
            child: Regex::new(r"<w:([A-Za-z]+)").unwrap(),
            run: Regex::new(r"(?s)<w:r(?: [^>]*)?>.*?</w:r>").unwrap(),
            rpr: Regex::new(r"(?s)<w:rPr>(.*?)</w:rPr>").unwrap(),
            fonts: Regex::new(r"<w:rFonts((?: [^>]*?)?)\s*/>").unwrap(),
            tagged: Regex::new(r#"<w:cs/>|<w:lang [^>]*/>|\s*w:hint="[^"]*""#).unwrap(),
            tail: Regex::new(r"<w:(?:eastAsianLayout|specVanish|oMath|rPrChange)\b").unwrap(),
            tbl: Regex::new(r"(?s)<w:tblPr/>|<w:tblPr>(.*?)</w:tblPr>").unwrap(),
            tbl_after: Regex::new(r"<w:(?:tblLook|tblCaption|tblDescription)\b").unwrap(),
        }
    }

    /// Word for Mac breaks Thai lines only inside runs marked as Thai
    /// complex script; WordCraft writes them en-US, and every line then
    /// breaks mid-word (Pages and LibreOffice don't care). Owner-verified
    /// 2026-10-23: hint="cs" + <w:cs/> + lang th-TH on each run fixed it.
    fn thai_runs(&self, b: &[u8]) -> Vec<u8> {
        self.run
            .replace_all(b, |c: &Captures| {
                let r = &c[0];
                if !has_thai(r) || self.rpr.find(r).is_some_and(|m| self.is_thai(m.as_bytes())) {
                    return r.to_vec();
                }
                let open = find(r, b">").map_or(r.len(), |e| e + 1);
                match self.rpr.captures(r) {
                    Some(rc) if rc.get(0).unwrap().start() == open => {
                        let m = rc.get(0).unwrap();
                        let fixed = self.thai_rpr(&rc[1]);
                        cat(&[
                            &r[..m.start()],
                            b"<w:rPr>",
                            &fixed,
                            b"</w:rPr>",
                            &r[m.end()..],
                        ])
                    }
                    _ => cat(&[
                        &r[..open],
                        b"<w:rPr>",
                        &self.thai_rpr(b""),
                        b"</w:rPr>",
                        &r[open..],
                    ]),
                }
            })
            .into_owned()
    }

    /// WordCraft tables have no cell margins, so Sarabun's upper vowels and
    /// tone marks touch the borders in Word. Owner picked these 2026-10-23:
    /// 0.15 cm top, 0.1 cm bottom, Word's usual 0.19 cm left and right.
    fn cell_margins(&self, b: &[u8]) -> Vec<u8> {
        const MAR: &[u8] = br#"<w:tblCellMar><w:top w:w="85" w:type="dxa"/><w:left w:w="108" w:type="dxa"/><w:bottom w:w="57" w:type="dxa"/><w:right w:w="108" w:type="dxa"/></w:tblCellMar>"#;
        self.tbl
            .replace_all(b, |c: &Captures| {
                let Some(inner) = c.get(1).map(|m| m.as_bytes()) else {
                    return cat(&[b"<w:tblPr>", MAR, b"</w:tblPr>"]);
                };
                if contains(inner, b"<w:tblCellMar") || contains(inner, b"<w:tblPrChange") {
                    return c[0].to_vec();
                }
                let at = self
                    .tbl_after
                    .find(inner)
                    .map_or(inner.len(), |m| m.start());
                cat(&[b"<w:tblPr>", &inner[..at], MAR, &inner[at..], b"</w:tblPr>"])
            })
            .into_owned()
    }

    fn is_thai(&self, rpr: &[u8]) -> bool {
        contains(rpr, b"<w:cs/>")
            && contains(rpr, br#"w:bidi="th-TH""#)
            && contains(rpr, br#"w:hint="cs""#)
    }

    fn thai_rpr(&self, inner: &[u8]) -> Vec<u8> {
        let inner = self.tagged.replace_all(inner, &b""[..]);
        let inner = if self.fonts.is_match(&inner) {
            self.fonts
                .replace(&inner, |c: &Captures| {
                    cat(&[b"<w:rFonts", &c[1], br#" w:hint="cs"/>"#])
                })
                .into_owned()
        } else {
            cat(&[br#"<w:rFonts w:hint="cs"/>"#, &inner])
        };
        let tags: &[u8] = br#"<w:cs/><w:lang w:val="th-TH" w:bidi="th-TH"/>"#;
        let at = self.tail.find(&inner).map_or(inner.len(), |m| m.start());
        cat(&[&inner[..at], tags, &inner[at..]])
    }

    fn fix(&self, b: &[u8], name: &str) -> Vec<u8> {
        let story = name == "word/document.xml"
            || name == "word/footnotes.xml"
            || name == "word/endnotes.xml"
            || (name.starts_with("word/header") || name.starts_with("word/footer"))
                && name.ends_with(".xml");
        if story && name != "word/document.xml" {
            return self.cell_margins(&self.thai_runs(b));
        }
        if name == "word/styles.xml" || name == "word/document.xml" {
            // multiple-line spacing (lineRule auto, 240 = single); exact/atLeast spacing is left alone
            let mut b = self
                .spacing
                .replace_all(b, |c: &Captures| cat(&[&c[1], &self.line, &c[2]]))
                .into_owned();
            if name == "word/styles.xml" {
                b = self.default_spacing(&b);
            } else {
                b = self.cell_margins(&self.thai_runs(&b));
            }
            return b;
        }
        if name != "word/settings.xml" || contains(b, b"<w:hideSpellingErrors") {
            return b.to_vec();
        }
        let flags = b"<w:hideSpellingErrors/><w:hideGrammaticalErrors/>";
        let Some(body) =
            find(b, b"<w:settings").and_then(|s| find(&b[s..], b">").map(|e| s + e + 1))
        else {
            return b.to_vec();
        };
        let at = self
            .child
            .captures_iter(&b[body..])
            .find(|c| !SETTINGS_BEFORE.contains(&&c[1]))
            .map(|c| body + c.get(0).unwrap().start())
            .or_else(|| find(b, b"</w:settings>"));
        match at {
            Some(at) => cat(&[&b[..at], flags, &b[at..]]),
            None => b.to_vec(),
        }
    }

    /// Gives docDefaults' `w:spacing` a line when it has none.
    fn default_spacing(&self, b: &[u8]) -> Vec<u8> {
        const HEAD: &[u8] = b"<w:pPrDefault><w:pPr>";
        const SP: &[u8] = b"<w:spacing ";
        let mut out = Vec::with_capacity(b.len() + 40);
        let mut pos = 0;
        while let Some(h) = find(&b[pos..], HEAD).map(|h| pos + h) {
            let region_end = find(&b[h..], b"</w:pPr>").map_or(b.len(), |e| h + e);
            let mut s = h + HEAD.len();
            let mut hit = None;
            while let Some(i) = find(&b[s..region_end], SP).map(|i| s + i) {
                let attrs_start = i + SP.len();
                let tag_end = find(&b[attrs_start..], b">").map_or(b.len(), |e| attrs_start + e);
                if !contains(&b[attrs_start..tag_end], b"w:line=") {
                    hit = Some(attrs_start);
                    break;
                }
                s = i + 1;
            }
            match hit {
                Some(at) => {
                    out.extend_from_slice(&b[pos..at]);
                    out.extend_from_slice(b"w:line=\"");
                    out.extend_from_slice(&self.line);
                    out.extend_from_slice(b"\" w:lineRule=\"auto\" ");
                    pos = at;
                }
                None => {
                    out.extend_from_slice(&b[pos..h + 1]);
                    pos = h + 1;
                }
            }
        }
        out.extend_from_slice(&b[pos..]);
        out
    }
}

/// Any character from the Thai block (U+0E00–U+0E7F: E0 B8 xx / E0 B9 xx).
fn has_thai(b: &[u8]) -> bool {
    b.windows(2)
        .any(|w| w[0] == 0xE0 && (w[1] == 0xB8 || w[1] == 0xB9))
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

fn contains(h: &[u8], n: &[u8]) -> bool {
    find(h, n).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn fixture(name: &str) -> (tempfile::TempDir, PathBuf) {
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/office/testdata")
            .join(name);
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join(name);
        std::fs::copy(src, &dst).unwrap();
        (dir, dst)
    }

    fn parts(path: &Path) -> Vec<(String, Vec<u8>)> {
        let mut z = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        (0..z.len())
            .map(|i| {
                let mut f = z.by_index(i).unwrap();
                let mut b = Vec::new();
                f.read_to_end(&mut b).unwrap();
                (f.name().to_string(), b)
            })
            .collect()
    }

    fn part(path: &Path, name: &str) -> String {
        let p = parts(path).into_iter().find(|(n, _)| n == name).unwrap();
        String::from_utf8(p.1).unwrap()
    }

    fn assert_xml_parses(path: &Path) {
        for (name, data) in parts(path) {
            if !(name.ends_with(".xml") || name.ends_with(".rels")) {
                continue;
            }
            let mut r = quick_xml::Reader::from_reader(&data[..]);
            r.config_mut().check_end_names = true;
            let mut buf = Vec::new();
            loop {
                match r.read_event_into(&mut buf) {
                    Ok(quick_xml::events::Event::Eof) => break,
                    Ok(_) => {}
                    Err(e) => panic!("{name}: {e}"),
                }
                buf.clear();
            }
        }
    }

    fn run(name: &str) -> (tempfile::TempDir, PathBuf) {
        let (d, p) = fixture(name);
        let before: Vec<String> = parts(&p).into_iter().map(|(n, _)| n).collect();
        assert!(fixup_file(&p, &FixupOptions::default()).unwrap());
        assert_xml_parses(&p);
        let after: Vec<String> = parts(&p).into_iter().map(|(n, _)| n).collect();
        let kept: Vec<&String> = before
            .iter()
            .filter(|n| *n != "xl/sharedStrings.xml")
            .collect();
        assert_eq!(kept, after.iter().collect::<Vec<_>>());
        assert_eq!(after[0], "[Content_Types].xml");

        let once: BTreeMap<_, _> = parts(&p).into_iter().collect();
        assert!(!fixup_file(&p, &FixupOptions::default()).unwrap());
        let twice: BTreeMap<_, _> = parts(&p).into_iter().collect();
        assert_eq!(once, twice);
        (d, p)
    }

    #[test]
    fn xlsx_inlines_shared_strings() {
        let (_d, p) = run("budget.xlsx");
        assert!(!parts(&p).iter().any(|(n, _)| n == "xl/sharedStrings.xml"));
        assert!(!part(&p, "[Content_Types].xml").contains("sharedStrings"));
        assert!(!part(&p, "xl/_rels/workbook.xml.rels").contains("sharedStrings"));
        let s = part(&p, "xl/worksheets/sheet1.xml");
        assert_eq!(s.matches(r#"t="inlineStr""#).count(), 11);
        assert!(!s.contains(r#"t="s""#));
        let a3 = Regex::new(r#"<c r="A3"[^>]*><is>(.*?)</is></c>"#).unwrap();
        let inner = a3.captures(s.as_bytes()).unwrap()[1].to_vec();
        let text: String = Regex::new(r"(?s)<t[^>]*>(.*?)</t>")
            .unwrap()
            .captures_iter(&inner)
            .map(|c| String::from_utf8(c[1].to_vec()).unwrap())
            .collect();
        assert_eq!(text, "เงินเดือนพนักงาน");
    }

    #[test]
    fn pptx_fonts_and_leading() {
        let (_d, p) = run("deck.pptx");
        let font = r#"<a:latin typeface="Sarabun"/><a:cs typeface="Sarabun"/>"#;
        let rpr = Regex::new(r"(?s)<a:rPr(?: [^>]*)?>(.*?)</a:rPr>|<a:rPr[^>]*/>").unwrap();
        let para = Regex::new(r"(?s)<a:p>(.*?)</a:p>").unwrap();
        let mut slides = 0;
        for (name, data) in parts(&p) {
            if name.starts_with("ppt/slides/slide") && name.ends_with(".xml") {
                slides += 1;
                let mut n = 0;
                for c in rpr.captures_iter(&data) {
                    n += 1;
                    let inner = c.get(1).map_or(&b""[..], |m| m.as_bytes());
                    assert!(contains(inner, font.as_bytes()), "{name}: rPr without font");
                }
                assert!(n > 0, "{name}: no rPr");
                for c in para.captures_iter(&data) {
                    assert!(
                        c[1].starts_with(b"<a:pPr")
                            && contains(&c[1], br#"<a:lnSpc><a:spcPct val="125000"/>"#),
                        "{name}: paragraph without lnSpc"
                    );
                }
            }
        }
        assert_eq!(slides, 3);
        let defrpr = Regex::new(r"(?s)<a:defRPr(?: [^>]*)?>(.*?)</a:defRPr>").unwrap();
        let mut filled = 0;
        for (name, data) in parts(&p) {
            if !name.starts_with("ppt/slide") || !name.ends_with(".xml") {
                continue;
            }
            for c in defrpr.captures_iter(&data) {
                let inner = &c[1];
                if let Some(fill) = find(inner, b"</a:solidFill>") {
                    filled += 1;
                    assert!(
                        find(inner, b"<a:latin").unwrap() > fill,
                        "{name}: latin before fill"
                    );
                }
            }
        }
        assert!(filled > 0, "no defRPr with solidFill in fixture");
    }

    #[test]
    fn docx_leading_and_proofing() {
        let (_d, p) = run("memo.docx");
        let s = part(&p, "word/settings.xml");
        let hide = s
            .find("<w:hideSpellingErrors/><w:hideGrammaticalErrors/>")
            .unwrap();
        assert!(hide < s.find("<w:defaultTabStop").unwrap());
        let sp = Regex::new(r"<w:spacing [^>]*/?>").unwrap();
        let mut n = 0;
        for name in ["word/styles.xml", "word/document.xml"] {
            let x = part(&p, name);
            for m in sp.find_iter(x.as_bytes()) {
                let t = m.as_bytes();
                if contains(t, br#"w:lineRule="auto""#) {
                    n += 1;
                    assert!(
                        contains(t, br#"w:line="300""#),
                        "{name}: {}",
                        String::from_utf8_lossy(t)
                    );
                }
            }
        }
        assert!(n > 0);
        assert!(part(&p, "word/styles.xml").contains("<w:pPrDefault><w:pPr>"));
    }

    #[test]
    fn docx_thai_runs_are_tagged_for_word_mac() {
        let d = Docx::new(125);
        let x = r#"<w:p><w:r><w:rPr><w:rFonts w:ascii="Sarabun" w:cs="Sarabun"/><w:sz w:val="32"/><w:lang w:val="en-US"/></w:rPr><w:t>ทดสอบ AI</w:t></w:r><w:r><w:t>Hello</w:t></w:r><w:r w:rsidR="1"><w:t>ไทย</w:t></w:r></w:p>"#
            .as_bytes();
        let out = String::from_utf8(d.thai_runs(x)).unwrap();
        assert_eq!(
            out,
            r#"<w:p><w:r><w:rPr><w:rFonts w:ascii="Sarabun" w:cs="Sarabun" w:hint="cs"/><w:sz w:val="32"/><w:cs/><w:lang w:val="th-TH" w:bidi="th-TH"/></w:rPr><w:t>ทดสอบ AI</w:t></w:r><w:r><w:t>Hello</w:t></w:r><w:r w:rsidR="1"><w:rPr><w:rFonts w:hint="cs"/><w:cs/><w:lang w:val="th-TH" w:bidi="th-TH"/></w:rPr><w:t>ไทย</w:t></w:r></w:p>"#
        );
        assert_eq!(d.thai_runs(out.as_bytes()), out.as_bytes(), "idempotent");
    }

    #[test]
    fn docx_tables_get_cell_margins() {
        let d = Docx::new(125);
        let x = r#"<w:tbl><w:tblPr><w:tblStyle w:val="TableGrid"/><w:tblW w:w="0" w:type="auto"/><w:tblLook w:val="04A0"/></w:tblPr></w:tbl><w:tbl><w:tblPr/></w:tbl><w:tbl><w:tblPr><w:tblCellMar><w:left w:w="0" w:type="dxa"/></w:tblCellMar></w:tblPr></w:tbl>"#
            .as_bytes();
        let out = String::from_utf8(d.cell_margins(x)).unwrap();
        let mar = r#"<w:tblCellMar><w:top w:w="85" w:type="dxa"/><w:left w:w="108" w:type="dxa"/><w:bottom w:w="57" w:type="dxa"/><w:right w:w="108" w:type="dxa"/></w:tblCellMar>"#;
        assert_eq!(
            out,
            format!(
                r#"<w:tbl><w:tblPr><w:tblStyle w:val="TableGrid"/><w:tblW w:w="0" w:type="auto"/>{mar}<w:tblLook w:val="04A0"/></w:tblPr></w:tbl><w:tbl><w:tblPr>{mar}</w:tblPr></w:tbl><w:tbl><w:tblPr><w:tblCellMar><w:left w:w="0" w:type="dxa"/></w:tblCellMar></w:tblPr></w:tbl>"#
            )
        );
        assert_eq!(d.cell_margins(out.as_bytes()), out.as_bytes(), "idempotent");
    }

    #[test]
    fn non_office_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("notes.txt");
        std::fs::write(&p, "hello").unwrap();
        assert!(!fixup_file(&p, &FixupOptions::default()).unwrap());
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
    }
}
