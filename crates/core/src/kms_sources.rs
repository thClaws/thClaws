//! Provenance catalogue for a KMS's `sources/` layer.
//!
//! `sources/` accumulated files from three unrelated writers with three
//! unrelated conventions and no record tying any of them together:
//!
//! - `/kms ingest <file>` → `<alias>.<ext>`, a byte copy, no metadata
//! - `/kms ingest <url>` → `<alias>.md`, raw HTML, origin buried in an
//!   HTML comment on line 1
//! - `/research` + `KmsWriteSource` → `<url-slug>.md` with its own
//!   `type: research-source` frontmatter
//!
//! Nothing could answer "where did this file come from", "is this the
//! same document I already have", or "which pages depend on it".
//! `lint` checked none of it and the graph view could only see sources
//! a page happened to link in the research citation format.
//!
//! This module owns `sources/_catalog.json`: one record per archived
//! file, written by every ingest path, reconcilable against disk, and
//! rendered into the KMS index so both the human and the model can see
//! the provenance trail. Content hashes make re-ingest of an identical
//! document detectable instead of silently duplicating it under a
//! second alias.

use crate::error::{Error, Result};
use crate::kms::KmsRef;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Catalogue filename inside `sources/`. Leading `_` keeps it out of
/// [`crate::kms::list_sources`] — aliases are sanitised with leading
/// underscores trimmed, so no real source can collide.
pub const CATALOG_FILE: &str = "_catalog.json";

/// How a source arrived. Drives the "Origin" rendering and lets lint
/// tell a dead local path from a URL that simply can't be re-fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Copied from a path on this machine.
    File,
    /// Fetched over HTTP(S).
    Url,
    /// Text extracted from a PDF.
    Pdf,
    /// Written by the `/research` pipeline or `KmsWriteSource`.
    Research,
    /// Distilled from a chat session.
    Session,
    /// Present on disk but with no catalogue entry — backfilled by
    /// [`reconcile`] for KMSes that predate the catalogue.
    Unknown,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::File => "file",
            Origin::Url => "url",
            Origin::Pdf => "pdf",
            Origin::Research => "research",
            Origin::Session => "session",
            Origin::Unknown => "unknown",
        }
    }
}

/// One archived source. `file` (name with extension) is the key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRecord {
    /// Filename inside `sources/`, extension included.
    pub file: String,
    /// Human-readable title — the source's own `title:` frontmatter,
    /// its first heading, or the de-slugged stem.
    #[serde(default)]
    pub title: String,
    #[serde(default = "default_origin")]
    pub origin: Origin,
    /// Where it came from: an absolute path, a URL, or a session id.
    #[serde(default)]
    pub origin_ref: String,
    /// `YYYY-MM-DD`.
    #[serde(default)]
    pub ingested: String,
    #[serde(default)]
    pub bytes: u64,
    /// Lowercase hex SHA-256 of the file's bytes as archived. Lets a
    /// re-ingest recognise "same document, different alias".
    #[serde(default)]
    pub sha256: String,
    /// Set when the archived copy was converted rather than copied
    /// verbatim (HTML → Markdown, PDF → text). Records that the
    /// archive is lossy so nobody treats it as the original.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converted_from: Option<String>,
}

fn default_origin() -> Origin {
    Origin::Unknown
}

impl SourceRecord {
    pub fn stem(&self) -> &str {
        self.file
            .rsplit_once('.')
            .map(|(s, _)| s)
            .unwrap_or(&self.file)
    }
}

/// The whole catalogue, keyed by filename.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub entries: BTreeMap<String, SourceRecord>,
}

pub const CATALOG_VERSION: u32 = 1;

fn catalog_path(kref: &KmsRef) -> std::path::PathBuf {
    kref.sources_dir().join(CATALOG_FILE)
}

/// Read the catalogue. A missing or malformed file yields an empty
/// catalogue rather than an error — provenance is additive metadata
/// and must never block a read or a write of the KMS itself.
pub fn load(kref: &KmsRef) -> Catalog {
    let path = catalog_path(kref);
    if let Ok(md) = std::fs::symlink_metadata(&path) {
        if md.file_type().is_symlink() {
            return Catalog::default();
        }
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Catalog>(&raw).ok())
        .unwrap_or_default()
}

fn store(kref: &KmsRef, cat: &Catalog) -> Result<()> {
    let dir = kref.sources_dir();
    std::fs::create_dir_all(&dir).map_err(|e| Error::Tool(format!("ensure sources dir: {e}")))?;
    let path = catalog_path(kref);
    let json = serde_json::to_string_pretty(cat)
        .map_err(|e| Error::Tool(format!("serialize source catalog: {e}")))?;
    std::fs::write(&path, json.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
    Ok(())
}

/// Insert or replace one record.
pub fn upsert(kref: &KmsRef, rec: SourceRecord) -> Result<()> {
    let mut cat = load(kref);
    cat.version = CATALOG_VERSION;
    cat.entries.insert(rec.file.clone(), rec);
    store(kref, &cat)
}

/// Drop a record (the file was deleted).
pub fn forget(kref: &KmsRef, file: &str) -> Result<()> {
    let mut cat = load(kref);
    if cat.entries.remove(file).is_some() {
        cat.version = CATALOG_VERSION;
        store(kref, &cat)?;
    }
    Ok(())
}

/// Look up by content hash — the "have I already archived this exact
/// document?" check an ingest runs before writing a second copy under
/// a different alias.
pub fn find_by_hash(kref: &KmsRef, sha256: &str) -> Option<SourceRecord> {
    if sha256.is_empty() {
        return None;
    }
    load(kref)
        .entries
        .into_values()
        .find(|r| r.sha256 == sha256)
}

/// SHA-256 of a file's bytes, lowercase hex. Empty string on read
/// failure — a missing hash degrades dedup, it doesn't break ingest.
pub fn hash_file(path: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let mut h = Sha256::new();
    h.update(&bytes);
    format!("{:x}", h.finalize())
}

/// What [`reconcile`] changed.
#[derive(Debug, Default)]
pub struct ReconcileReport {
    /// Files on disk that had no record — backfilled as `unknown`.
    pub backfilled: Vec<String>,
    /// Records whose file is gone — dropped.
    pub dropped: Vec<String>,
    /// Records whose size/hash no longer matches the file — refreshed.
    pub refreshed: Vec<String>,
}

impl ReconcileReport {
    pub fn total(&self) -> usize {
        self.backfilled.len() + self.dropped.len() + self.refreshed.len()
    }
}

/// Bring the catalogue in line with what is actually on disk. Safe to
/// run repeatedly; called by `/kms lint` and after bulk operations
/// (merge, OKF import) that move source files around without going
/// through an ingest path.
pub fn reconcile(kref: &KmsRef) -> Result<ReconcileReport> {
    let mut cat = load(kref);
    let mut report = ReconcileReport::default();
    let on_disk = crate::kms::list_sources(kref);
    let disk_names: std::collections::HashSet<String> =
        on_disk.iter().map(|s| s.file_name()).collect();

    for src in &on_disk {
        let file = src.file_name();
        let path = kref.sources_dir().join(&file);
        match cat.entries.get_mut(&file) {
            Some(rec) => {
                if rec.bytes != src.bytes || rec.sha256.is_empty() {
                    rec.bytes = src.bytes;
                    rec.sha256 = hash_file(&path);
                    report.refreshed.push(file.clone());
                }
            }
            None => {
                cat.entries.insert(
                    file.clone(),
                    SourceRecord {
                        title: derive_title(&path, &src.stem),
                        origin: Origin::Unknown,
                        origin_ref: String::new(),
                        ingested: crate::usage::today_str(),
                        bytes: src.bytes,
                        sha256: hash_file(&path),
                        converted_from: None,
                        file: file.clone(),
                    },
                );
                report.backfilled.push(file);
            }
        }
    }

    let stale: Vec<String> = cat
        .entries
        .keys()
        .filter(|k| !disk_names.contains(*k))
        .cloned()
        .collect();
    for k in stale {
        cat.entries.remove(&k);
        report.dropped.push(k);
    }

    if report.total() > 0 || cat.version != CATALOG_VERSION {
        cat.version = CATALOG_VERSION;
        store(kref, &cat)?;
    }
    Ok(report)
}

/// Title for a source with no catalogue record: its `title:`
/// frontmatter, else its first ATX heading, else the de-slugged stem.
pub fn derive_title(path: &std::path::Path, stem: &str) -> String {
    if let Ok(raw) = std::fs::read_to_string(path) {
        let (fm, body) = crate::kms::parse_frontmatter(&raw);
        if let Some(t) = fm.get("title").map(|s| s.trim().trim_matches('"')) {
            if !t.is_empty() {
                return t.to_string();
            }
        }
        for line in body.lines().take(40) {
            if let Some(h) = line.trim().strip_prefix("# ") {
                let h = h.trim();
                if !h.is_empty() {
                    return h.chars().take(120).collect();
                }
            }
        }
    }
    stem.replace(['-', '_'], " ")
}

/// Which pages cite a source. A page counts as citing it when its
/// frontmatter `sources:` names the stem, or its body links the file
/// relatively (`](../sources/<file>)`) — covering both the ingest
/// convention and the research-citation convention that previously had
/// no common reader.
pub fn citing_pages(kref: &KmsRef, file: &str) -> Vec<String> {
    citation_map(kref).remove(file).unwrap_or_default()
}

/// Every source → the pages citing it, in ONE pass over `pages/`.
///
/// The per-source variant re-reads the whole page directory for each
/// archived file, which the index renderer calls once per source — so
/// a KMS with 200 pages and 100 sources did 20,000 file reads on every
/// single page write. Index rendering goes through this instead.
pub fn citation_map(kref: &KmsRef) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // stem → file, so a `sources: spec` declaration resolves to
    // `spec.md` without knowing the extension.
    let by_stem: BTreeMap<String, String> = crate::kms::list_sources(kref)
        .into_iter()
        .map(|s| (s.stem.clone(), s.file_name()))
        .collect();
    let by_file: std::collections::HashSet<String> = by_stem.values().cloned().collect();

    let Ok(entries) = std::fs::read_dir(kref.pages_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(page_stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (fm, body) = crate::kms::parse_frontmatter(&raw);
        let hit = |file: &str, out: &mut BTreeMap<String, Vec<String>>| {
            let bucket = out.entry(file.to_string()).or_default();
            if !bucket.iter().any(|p| p == page_stem) {
                bucket.push(page_stem.to_string());
            }
        };
        // Declared provenance (`sources:` frontmatter, the ingest
        // convention).
        if let Some(v) = fm.get("sources") {
            for token in v
                .split(|c: char| c == ',' || c.is_whitespace())
                .map(|s| s.trim().trim_matches('"'))
                .filter(|s| !s.is_empty())
            {
                if by_file.contains(token) {
                    hit(token, &mut out);
                } else if let Some(file) = by_stem.get(token) {
                    let file = file.clone();
                    hit(&file, &mut out);
                }
            }
        }
        // Relative links (`](../sources/<file>)`, the research
        // citation convention). Both are checked because the two
        // writers never shared one.
        for target in body.match_indices("](../sources/").filter_map(|(i, m)| {
            let rest = &body[i + m.len()..];
            rest.find(')').map(|e| rest[..e].to_string())
        }) {
            let cleaned = target
                .split(['#', '?'])
                .next()
                .unwrap_or(&target)
                .to_string();
            if by_file.contains(&cleaned) {
                hit(&cleaned, &mut out);
            } else if let Some(file) = by_stem.get(cleaned.trim_end_matches(".md")) {
                let file = file.clone();
                hit(&file, &mut out);
            }
        }
    }
    for v in out.values_mut() {
        v.sort();
    }
    out
}

/// Render the `## Sources` block spliced into the KMS index. One line
/// per archived file: title, the local path, where it came from, and
/// which pages stand on it — so an orphaned archive is visible at a
/// glance instead of needing a lint run to discover.
pub fn render_index_block(kref: &KmsRef, max_entries: usize) -> String {
    let cat = load(kref);
    let on_disk = crate::kms::list_sources(kref);
    if on_disk.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n## Sources\n\n");
    out.push_str(
        "Raw archived material behind the pages above. Read one with \
         `KmsRead(kind: \"source\", page: \"<file>\")`; search inside them with \
         `KmsSearch(scope: \"sources\")`.\n\n",
    );
    let citations = citation_map(kref);
    let mut shown = 0usize;
    for src in &on_disk {
        if shown >= max_entries {
            out.push_str(&format!(
                "\n_… {} more source(s) not listed_\n",
                on_disk.len() - shown
            ));
            break;
        }
        let file = src.file_name();
        let rec = cat.entries.get(&file);
        let title = rec
            .map(|r| r.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| src.stem.replace(['-', '_'], " "));
        let origin = rec
            .map(|r| match r.origin {
                Origin::Url => format!(" · from {}", r.origin_ref),
                Origin::Unknown if r.origin_ref.is_empty() => String::new(),
                // Local paths are shown tail-first: the full path is on
                // the page's Provenance section and in this catalogue,
                // and a wall of absolute paths makes the index (which
                // also goes into the system prompt) unreadable.
                _ => format!(" · {} {}", r.origin.as_str(), short_path(&r.origin_ref)),
            })
            .unwrap_or_default();
        let empty = Vec::new();
        let citers = citations.get(&file).unwrap_or(&empty);
        let cited = if citers.is_empty() {
            " · **uncited**".to_string()
        } else {
            format!(" · cited by {}", citers.join(", "))
        };
        out.push_str(&format!(
            "- [{title}](sources/{file}) — {}{origin}{cited}\n",
            human_bytes(src.bytes)
        ));
        shown += 1;
    }
    out
}

/// Last two path components, prefixed with `…/` when truncated.
fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.rsplit('/').filter(|s| !s.is_empty()).collect();
    match parts.len() {
        0 => p.to_string(),
        1 => parts[0].to_string(),
        2 => format!("{}/{}", parts[1], parts[0]),
        _ => format!("…/{}/{}", parts[1], parts[0]),
    }
}

pub fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::{self, KmsScope};

    fn temp_kms(name: &str) -> (tempfile::TempDir, KmsRef) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(name);
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::create_dir_all(root.join("sources")).unwrap();
        let kref = KmsRef {
            name: name.to_string(),
            scope: KmsScope::Project,
            root,
        };
        (dir, kref)
    }

    fn write_source(kref: &KmsRef, file: &str, body: &str) {
        std::fs::write(kref.sources_dir().join(file), body).unwrap();
    }

    #[test]
    fn catalog_roundtrips() {
        let (_d, k) = temp_kms("t");
        write_source(&k, "spec.md", "# Spec\n\nbody");
        upsert(
            &k,
            SourceRecord {
                file: "spec.md".into(),
                title: "Spec".into(),
                origin: Origin::Url,
                origin_ref: "https://x.test/spec".into(),
                ingested: "2026-08-30".into(),
                bytes: 14,
                sha256: "abc".into(),
                converted_from: Some("text/html".into()),
            },
        )
        .unwrap();
        let cat = load(&k);
        let rec = cat.entries.get("spec.md").expect("record stored");
        assert_eq!(rec.origin, Origin::Url);
        assert_eq!(rec.origin_ref, "https://x.test/spec");
        assert_eq!(rec.converted_from.as_deref(), Some("text/html"));
    }

    #[test]
    fn catalog_survives_corrupt_file() {
        let (_d, k) = temp_kms("t");
        std::fs::write(k.sources_dir().join(CATALOG_FILE), b"{not json").unwrap();
        assert!(load(&k).entries.is_empty());
    }

    #[test]
    fn reconcile_backfills_and_drops() {
        let (_d, k) = temp_kms("t");
        write_source(&k, "orphan.txt", "hello");
        upsert(
            &k,
            SourceRecord {
                file: "vanished.md".into(),
                title: "Gone".into(),
                origin: Origin::File,
                origin_ref: "/tmp/gone.md".into(),
                ingested: "2026-01-01".into(),
                bytes: 1,
                sha256: "x".into(),
                converted_from: None,
            },
        )
        .unwrap();

        let r = reconcile(&k).unwrap();
        assert_eq!(r.backfilled, vec!["orphan.txt".to_string()]);
        assert_eq!(r.dropped, vec!["vanished.md".to_string()]);
        let cat = load(&k);
        assert!(cat.entries.contains_key("orphan.txt"));
        assert!(!cat.entries.contains_key("vanished.md"));
        assert!(!cat.entries["orphan.txt"].sha256.is_empty());
    }

    #[test]
    fn reconcile_is_idempotent() {
        let (_d, k) = temp_kms("t");
        write_source(&k, "a.md", "# A");
        reconcile(&k).unwrap();
        let second = reconcile(&k).unwrap();
        assert_eq!(second.total(), 0, "{second:?}");
    }

    #[test]
    fn catalog_file_is_not_listed_as_a_source() {
        let (_d, k) = temp_kms("t");
        write_source(&k, "real.md", "# Real");
        upsert(
            &k,
            SourceRecord {
                file: "real.md".into(),
                title: "Real".into(),
                origin: Origin::File,
                origin_ref: "/tmp/real.md".into(),
                ingested: "2026-08-30".into(),
                bytes: 6,
                sha256: String::new(),
                converted_from: None,
            },
        )
        .unwrap();
        let listed: Vec<String> = kms::list_sources(&k)
            .into_iter()
            .map(|s| s.file_name())
            .collect();
        assert_eq!(listed, vec!["real.md".to_string()]);
    }

    #[test]
    fn citing_pages_sees_both_conventions() {
        let (_d, k) = temp_kms("t");
        write_source(&k, "spec.md", "# Spec");
        std::fs::write(
            k.pages_dir().join("via-frontmatter.md"),
            "---\nsources: spec\n---\n\n# P\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("via-link.md"),
            "# Q\n\nSee [the spec](../sources/spec.md).\n",
        )
        .unwrap();
        std::fs::write(k.pages_dir().join("unrelated.md"), "# R\n").unwrap();

        let citers = citing_pages(&k, "spec.md");
        assert_eq!(citers, vec!["via-frontmatter", "via-link"]);
    }

    #[test]
    fn find_by_hash_detects_duplicate_content() {
        let (_d, k) = temp_kms("t");
        write_source(&k, "a.md", "identical bytes");
        reconcile(&k).unwrap();
        let h = hash_file(&k.sources_dir().join("a.md"));
        assert!(!h.is_empty());
        assert_eq!(find_by_hash(&k, &h).unwrap().file, "a.md");
        assert!(find_by_hash(&k, "deadbeef").is_none());
        assert!(find_by_hash(&k, "").is_none());
    }

    #[test]
    fn index_block_flags_uncited_sources() {
        let (_d, k) = temp_kms("t");
        write_source(&k, "lonely.md", "# Lonely\n\nnobody links me");
        reconcile(&k).unwrap();
        let block = render_index_block(&k, 50);
        assert!(block.contains("## Sources"), "{block}");
        assert!(block.contains("lonely.md"), "{block}");
        assert!(block.contains("**uncited**"), "{block}");
    }

    #[test]
    fn index_block_empty_without_sources() {
        let (_d, k) = temp_kms("t");
        assert!(render_index_block(&k, 50).is_empty());
    }

    #[test]
    fn derive_title_prefers_frontmatter_then_heading() {
        let (_d, k) = temp_kms("t");
        write_source(
            &k,
            "fm.md",
            "---\ntitle: From Frontmatter\n---\n\n# Ignored\n",
        );
        write_source(&k, "h.md", "# From Heading\n\nbody");
        write_source(&k, "plain-name.txt", "no markers here");
        let d = k.sources_dir();
        assert_eq!(derive_title(&d.join("fm.md"), "fm"), "From Frontmatter");
        assert_eq!(derive_title(&d.join("h.md"), "h"), "From Heading");
        assert_eq!(
            derive_title(&d.join("plain-name.txt"), "plain-name"),
            "plain name"
        );
    }
}
