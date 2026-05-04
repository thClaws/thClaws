//! Knowledge Management System (KMS) — Karpathy-style LLM wikis.
//!
//! A KMS is a directory of markdown pages plus an `index.md` table of
//! contents and a `log.md` change history. Two scopes:
//!
//! - **User**: `~/.config/thclaws/kms/<name>/`
//! - **Project**: `.thclaws/kms/<name>/`
//!
//! Users mark any subset of KMS as "active" in `.thclaws/settings.json`'s
//! `kms.active` array. When a chat turn runs, each active KMS's
//! `index.md` is concatenated into the system prompt, and the
//! `KmsRead` / `KmsSearch` tools let the model pull in specific pages
//! on demand. No embeddings, no vector store — just grep + read, per
//! Karpathy's pattern.
//!
//! Layout of a KMS directory:
//!
//! ```text
//! <kms_root>/
//!   index.md     — table of contents, one line per page (model reads this)
//!   log.md       — append-only change log (human and model write here)
//!   SCHEMA.md    — optional: shape rules for pages (not enforced in code)
//!   pages/       — individual wiki pages, one per topic
//!   sources/     — raw source material (URLs, PDFs, notes) — optional
//! ```

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KmsScope {
    User,
    Project,
}

impl KmsScope {
    pub fn as_str(self) -> &'static str {
        match self {
            KmsScope::User => "user",
            KmsScope::Project => "project",
        }
    }
}

/// A KMS instance — its scope, name, and root directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmsRef {
    pub name: String,
    pub scope: KmsScope,
    pub root: PathBuf,
}

impl KmsRef {
    pub fn index_path(&self) -> PathBuf {
        self.root.join("index.md")
    }

    pub fn log_path(&self) -> PathBuf {
        self.root.join("log.md")
    }

    pub fn pages_dir(&self) -> PathBuf {
        self.root.join("pages")
    }

    pub fn schema_path(&self) -> PathBuf {
        self.root.join("SCHEMA.md")
    }

    /// Read `index.md`. Returns `""` (not an error) when the file is absent,
    /// OR when the path is a symlink (refused to prevent a cloned KMS
    /// with `index.md -> /etc/passwd` from exfiltrating through the
    /// system prompt). A fresh KMS with no entries yet is a valid state.
    pub fn read_index(&self) -> String {
        let path = self.index_path();
        if let Ok(md) = std::fs::symlink_metadata(&path) {
            if md.file_type().is_symlink() {
                return String::new();
            }
        }
        std::fs::read_to_string(&path).unwrap_or_default()
    }

    /// Resolve a page name to a file path inside `pages/`. `.md` is added
    /// if missing. Returns an error if the resolved path escapes the KMS
    /// directory via `..`, an absolute path, path separators, null bytes,
    /// or symlink trickery (e.g. `pages/` itself symlinked outside, or a
    /// page file symlinked to `/etc/passwd`).
    pub fn page_path(&self, page: &str) -> Result<PathBuf> {
        // Reject obviously-bad names before touching the filesystem.
        if page.is_empty()
            || page.contains("..")
            || page.contains('/')
            || page.contains('\\')
            || page.contains('\0')
            || page.chars().any(|c| c.is_control())
            || Path::new(page).is_absolute()
        {
            return Err(Error::Tool(format!(
                "invalid page name '{page}' — no '..', path separators, or control chars"
            )));
        }
        let name = if page.ends_with(".md") {
            page.to_string()
        } else {
            format!("{page}.md")
        };
        let candidate = self.pages_dir().join(&name);

        // Canonicalize the scope root and require the candidate to resolve
        // *within* this specific KMS directory under it. This defeats
        // symlink bypasses: if `pages/` or the page file itself is a
        // symlink pointing outside, the canonical candidate escapes the
        // KMS root and we reject.
        let canon_candidate = std::fs::canonicalize(&candidate).map_err(|e| {
            Error::Tool(format!(
                "cannot resolve page path '{}': {e}",
                candidate.display()
            ))
        })?;
        let canon_scope = scope_root(self.scope)
            .and_then(|p| std::fs::canonicalize(&p).ok())
            .ok_or_else(|| Error::Tool("kms scope root not resolvable".into()))?;
        let canon_kms_root = canon_scope.join(&self.name);
        if !canon_candidate.starts_with(&canon_kms_root) {
            return Err(Error::Tool(format!(
                "page '{page}' resolves outside the KMS directory — symlink escape rejected"
            )));
        }
        // Also require it's a regular file, not a directory.
        let meta = std::fs::metadata(&canon_candidate)
            .map_err(|e| Error::Tool(format!("cannot stat page '{page}': {e}")))?;
        if !meta.is_file() {
            return Err(Error::Tool(format!("page '{page}' is not a regular file")));
        }
        Ok(candidate)
    }
}

fn user_root() -> Option<PathBuf> {
    crate::util::home_dir().map(|h| h.join(".config/thclaws/kms"))
}

fn project_root() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".thclaws/kms")
}

fn scope_root(scope: KmsScope) -> Option<PathBuf> {
    match scope {
        KmsScope::User => user_root(),
        KmsScope::Project => Some(project_root()),
    }
}

/// Enumerate KMS directories under one scope. Silently ignores missing
/// roots — fresh installs have neither. Symlinks are intentionally
/// skipped: a user can't turn a KMS directory into a symlink to `/etc`
/// and have thClaws enumerate it.
fn list_in(scope: KmsScope) -> Vec<KmsRef> {
    let Some(root) = scope_root(scope) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        // symlink_metadata → file_type doesn't follow the symlink, so
        // a `ln -s /etc foo` sitting in the kms dir returns is_symlink.
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        out.push(KmsRef {
            name,
            scope,
            root: entry.path(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// List every KMS visible to this process — project entries first, then
/// user. If the same name exists in both scopes, both are returned;
/// callers that need to pick one treat project as higher priority.
pub fn list_all() -> Vec<KmsRef> {
    let mut out = list_in(KmsScope::Project);
    out.extend(list_in(KmsScope::User));
    out
}

/// Find a KMS by name. Project scope wins over user on collision — this
/// matches how project instructions override user instructions elsewhere
/// in thClaws. Returns `None` when no KMS by that name exists, or when
/// the matching directory is a symlink (symlinks are rejected to prevent
/// `ln -s /etc <kms-name>` style exfiltration).
pub fn resolve(name: &str) -> Option<KmsRef> {
    for scope in [KmsScope::Project, KmsScope::User] {
        if let Some(root) = scope_root(scope) {
            let candidate = root.join(name);
            // symlink_metadata doesn't follow the symlink.
            let Ok(meta) = std::fs::symlink_metadata(&candidate) else {
                continue;
            };
            if meta.is_symlink() || !meta.is_dir() {
                continue;
            }
            return Some(KmsRef {
                name: name.to_string(),
                scope,
                root: candidate,
            });
        }
    }
    None
}

/// Create a new KMS. Seeds `index.md`, `log.md`, and `SCHEMA.md` with
/// minimal starter content so the model has something to read on day
/// one. No-op and returns `Ok(existing)` if a KMS by that name already
/// exists at the requested scope.
pub fn create(name: &str, scope: KmsScope) -> Result<KmsRef> {
    if name.is_empty() {
        return Err(Error::Config("kms name must not be empty".into()));
    }
    if name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.contains('\0')
        || name.chars().any(|c| c.is_control())
        || name.starts_with('.')
        || Path::new(name).is_absolute()
    {
        return Err(Error::Config(format!(
            "invalid kms name '{name}' — no path separators, '..', control chars, or leading '.'"
        )));
    }
    let root = scope_root(scope)
        .ok_or_else(|| Error::Config("cannot locate user home directory".into()))?
        .join(name);
    if root.is_dir() {
        return Ok(KmsRef {
            name: name.to_string(),
            scope,
            root,
        });
    }
    std::fs::create_dir_all(root.join("pages"))?;
    std::fs::create_dir_all(root.join("sources"))?;
    let kref = KmsRef {
        name: name.to_string(),
        scope,
        root,
    };
    std::fs::write(
        kref.index_path(),
        format!("# {name}\n\nKnowledge base index — list each page with a one-line summary.\n"),
    )?;
    std::fs::write(
        kref.log_path(),
        "# Change log\n\nAppend-only list of ingests / edits / lints.\n",
    )?;
    std::fs::write(
        kref.schema_path(),
        "# Schema\n\nDescribe the shape of pages in this KMS — required\n\
         sections, naming conventions, cross-link style. Both you and the\n\
         agent read this before editing pages.\n",
    )?;
    Ok(kref)
}

/// Extensions a user can ingest into a KMS. Deliberately narrow: these
/// are the text formats `KmsRead` can hand to the model meaningfully,
/// and that a human would expect to grep with `KmsSearch`. Binary
/// formats (PDF, images, archives) are rejected with a hint to convert
/// them to markdown first — we'd rather make the user choose the
/// conversion than silently store a blob the model can't read.
pub const INGEST_EXTENSIONS: &[&str] = &["md", "markdown", "txt", "rst", "log", "json"];

/// Reserved aliases that collide with the KMS starter files — refuse
/// to ingest into them, otherwise a `/kms ingest notes README.md as index`
/// would clobber the index with no way back except `--force`.
const RESERVED_PAGE_STEMS: &[&str] = &["index", "log", "SCHEMA"];

/// What `ingest()` did. `overwrote == true` means `--force` replaced an
/// existing page; the handler surfaces that to the user so a typo in
/// the alias doesn't silently nuke a page. `cascaded` is the count of
/// dependent pages marked stale (M6.25 BUG #10).
#[derive(Debug)]
pub struct IngestResult {
    pub alias: String,
    pub target: PathBuf,
    pub summary: String,
    pub overwrote: bool,
    pub cascaded: usize,
}

/// M6.25 BUG #2: Ingest now SPLITS raw source from wiki page.
///
/// Pre-fix: `ingest()` copied the source straight into `pages/` and
/// treated it as both layer-1 (raw, immutable) and layer-2 (LLM-
/// authored synthesis). The llm-wiki concept requires those to be
/// distinct.
///
/// Post-fix: copy raw to `sources/<alias>.<ext>`, then write a stub
/// page in `pages/<alias>.md` with frontmatter pointing at the
/// source. The page stub is plain markdown the LLM can later enrich
/// via `KmsWrite`. `--force` re-copies the source AND triggers a
/// cascade: any page whose frontmatter `sources:` includes this
/// alias gets a "stale" marker appended (BUG #10). User then runs
/// `/kms lint` or asks the agent to refresh affected pages.
pub fn ingest(
    kms: &KmsRef,
    source: &Path,
    alias: Option<&str>,
    force: bool,
) -> Result<IngestResult> {
    let meta = std::fs::metadata(source)
        .map_err(|e| Error::Tool(format!("cannot stat source '{}': {e}", source.display())))?;
    if !meta.is_file() {
        return Err(Error::Tool(format!(
            "source '{}' is not a regular file",
            source.display()
        )));
    }

    let ext_raw = source.extension().and_then(|e| e.to_str()).ok_or_else(|| {
        Error::Tool(format!(
            "'{}' has no extension — ingest requires one of: {}",
            source.display(),
            INGEST_EXTENSIONS.join(", "),
        ))
    })?;
    let ext = ext_raw.to_ascii_lowercase();
    if !INGEST_EXTENSIONS.iter().any(|e| *e == ext) {
        return Err(Error::Tool(format!(
            "extension '.{ext}' not supported — allowed: {} (or use the URL/PDF ingest variants)",
            INGEST_EXTENSIONS.join(", "),
        )));
    }

    let raw_alias = match alias {
        Some(a) => a.to_string(),
        None => source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("page")
            .to_string(),
    };
    let alias = sanitize_alias(&raw_alias);
    if alias.is_empty() {
        return Err(Error::Tool(format!(
            "alias '{raw_alias}' sanitises to empty — use [A-Za-z0-9_-] characters"
        )));
    }
    if RESERVED_PAGE_STEMS
        .iter()
        .any(|r| r.eq_ignore_ascii_case(&alias))
    {
        return Err(Error::Tool(format!(
            "alias '{alias}' is reserved — pick another"
        )));
    }

    // Source path lives under sources/, page stub under pages/.
    std::fs::create_dir_all(kms.root.join("sources"))
        .map_err(|e| Error::Tool(format!("ensure sources dir: {e}")))?;
    let source_target = kms.root.join("sources").join(format!("{alias}.{ext}"));
    let page_target = kms.pages_dir().join(format!("{alias}.md"));
    let page_existed = page_target.exists();
    let source_existed = source_target.exists();
    if (page_existed || source_existed) && !force {
        return Err(Error::Tool(format!(
            "alias '{alias}' already exists ({}{}{}) — re-run with --force to overwrite",
            if source_existed { "source" } else { "" },
            if source_existed && page_existed {
                " + "
            } else {
                ""
            },
            if page_existed { "page" } else { "" },
        )));
    }

    std::fs::copy(source, &source_target).map_err(|e| {
        Error::Tool(format!(
            "copy {} → {} failed: {e}",
            source.display(),
            source_target.display()
        ))
    })?;
    let summary = first_summary_line(&source_target);

    // Write the page stub with frontmatter pointing at the source.
    let mut fm = std::collections::BTreeMap::new();
    let today = crate::usage::today_str();
    if !page_existed {
        fm.insert("created".into(), today.clone());
    }
    fm.insert("updated".into(), today.clone());
    fm.insert("category".into(), "uncategorized".into());
    fm.insert("sources".into(), alias.clone());
    let body = format!(
        "# {alias}\n\nStub page — raw source at `sources/{alias}.{ext}`. Summary line: {summary}\n\n\
         _Replace this stub with a curated summary, key takeaways, cross-references to other pages, etc._\n",
    );
    let serialized = write_frontmatter(&fm, &body);
    std::fs::write(&page_target, serialized.as_bytes())
        .map_err(|e| Error::Tool(format!("write page {}: {e}", page_target.display())))?;

    update_index_for_write(kms, &alias, &summary, Some("uncategorized"), page_existed)?;
    append_log_header(
        kms,
        if page_existed {
            "re-ingested"
        } else {
            "ingested"
        },
        &alias,
    )?;

    // BUG #10: cascade on re-ingest. Pages whose frontmatter
    // `sources:` mentions this alias get a stale marker appended so
    // the next reader (human or agent) knows to refresh.
    let cascade_count = if page_existed && force {
        mark_dependent_pages_stale(kms, &alias).unwrap_or(0)
    } else {
        0
    };

    Ok(IngestResult {
        alias,
        target: page_target,
        summary,
        overwrote: page_existed,
        cascaded: cascade_count,
    })
}

/// M6.25 BUG #10: re-ingest cascade. Walk every page; if its
/// frontmatter `sources:` contains the changed alias (comma- or
/// space- separated list), append a stale-marker line at the bottom
/// of the page body (after frontmatter). Returns the count of pages
/// touched.
fn mark_dependent_pages_stale(kref: &KmsRef, changed_alias: &str) -> Result<usize> {
    let pages_dir = kref.pages_dir();
    let entries = match std::fs::read_dir(&pages_dir) {
        Ok(e) => e,
        Err(_) => return Ok(0),
    };
    let today = crate::usage::today_str();
    let mut count = 0usize;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if stem == changed_alias {
            // Don't mark the freshly-written page as stale.
            continue;
        }
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        let (mut fm, body) = parse_frontmatter(&raw);
        let sources_field = match fm.get("sources") {
            Some(s) => s.clone(),
            None => continue,
        };
        let mentions = sources_field
            .split(|c: char| c == ',' || c.is_whitespace())
            .any(|s| s.trim() == changed_alias);
        if !mentions {
            continue;
        }
        fm.insert("updated".into(), today.clone());
        let mut new_body = body;
        if !new_body.ends_with('\n') {
            new_body.push('\n');
        }
        new_body.push_str(&format!(
            "\n> ⚠ STALE: source `{changed_alias}` was re-ingested on {today}. Refresh this page.\n"
        ));
        let serialized = write_frontmatter(&fm, &new_body);
        if std::fs::write(&path, serialized.as_bytes()).is_ok() {
            count += 1;
        }
    }
    Ok(count)
}

/// M6.25 BUG #8: ingest a remote URL by fetching it via the existing
/// WebFetchTool then writing the response body to a temp file and
/// running `ingest()` against it. The HTML→markdown conversion is
/// out of scope — we save the raw response. Pages can be cleaned up
/// by the LLM via KmsWrite.
pub async fn ingest_url(
    kref: &KmsRef,
    url: &str,
    alias: Option<&str>,
    force: bool,
) -> Result<IngestResult> {
    let resolved_alias = alias.map(String::from).unwrap_or_else(|| {
        // Derive an alias from the last path segment.
        url.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("page")
            .split('?')
            .next()
            .unwrap_or("page")
            .to_string()
    });
    let alias_clean = sanitize_alias(&resolved_alias);
    if alias_clean.is_empty() {
        return Err(Error::Tool(format!(
            "could not derive alias from URL '{url}' — pass --alias explicitly"
        )));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Tool(format!("http client: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("fetch {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Tool(format!(
            "fetch {url}: HTTP {}",
            resp.status().as_u16()
        )));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Tool(format!("read body: {e}")))?;

    // Stage to a tempfile with a markdown extension so the existing
    // ingest path accepts it.
    let tmp_dir = std::env::temp_dir();
    let tmp_path = tmp_dir.join(format!("kms-url-{alias_clean}.md"));
    let banner = format!(
        "<!-- fetched from {url} on {} -->\n",
        crate::usage::today_str()
    );
    std::fs::write(&tmp_path, format!("{banner}{body}").as_bytes())
        .map_err(|e| Error::Tool(format!("stage {}: {e}", tmp_path.display())))?;
    let result = ingest(kref, &tmp_path, Some(&alias_clean), force);
    let _ = std::fs::remove_file(&tmp_path);
    result
}

/// M6.25 BUG #8: ingest a PDF by extracting text via pdftotext
/// (the same path PdfReadTool uses). Output is markdown with a
/// short "extracted from PDF" banner. The agent can refine it
/// with KmsWrite.
pub async fn ingest_pdf(
    kref: &KmsRef,
    pdf_path: &Path,
    alias: Option<&str>,
    force: bool,
) -> Result<IngestResult> {
    let resolved_alias = alias.map(String::from).unwrap_or_else(|| {
        pdf_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("pdf-page")
            .to_string()
    });
    let alias_clean = sanitize_alias(&resolved_alias);
    if alias_clean.is_empty() {
        return Err(Error::Tool(format!(
            "alias derived from PDF is empty — pass --alias"
        )));
    }
    // Run pdftotext in a blocking task — same shape PdfReadTool uses.
    let pdf_owned = pdf_path.to_path_buf();
    let extracted = tokio::task::spawn_blocking(move || -> Result<String> {
        let output = std::process::Command::new("pdftotext")
            .args(["-layout", "-enc", "UTF-8"])
            .arg(&pdf_owned)
            .arg("-") // stdout
            .output()
            .map_err(|e| Error::Tool(format!("pdftotext (is poppler installed?): {e}")))?;
        if !output.status.success() {
            return Err(Error::Tool(format!(
                "pdftotext exited {}: {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    })
    .await
    .map_err(|e| Error::Tool(format!("pdftotext join: {e}")))??;

    let tmp_dir = std::env::temp_dir();
    let tmp_path = tmp_dir.join(format!("kms-pdf-{alias_clean}.md"));
    let banner = format!(
        "<!-- extracted from PDF '{}' on {} -->\n",
        pdf_path.display(),
        crate::usage::today_str(),
    );
    std::fs::write(&tmp_path, format!("{banner}{extracted}").as_bytes())
        .map_err(|e| Error::Tool(format!("stage {}: {e}", tmp_path.display())))?;
    let result = ingest(kref, &tmp_path, Some(&alias_clean), force);
    let _ = std::fs::remove_file(&tmp_path);
    result
}

/// Keep only `[A-Za-z0-9_-]`; collapse anything else to `_`. An empty
/// result returns empty so the caller can reject it with a useful
/// message rather than writing a page named "".
///
/// Made `pub` in M6.28 so the `/kms ingest <name> $` rewrite can
/// derive a slug from the active session's title (which may contain
/// spaces / punctuation) without re-implementing the sanitizer.
pub fn sanitize_alias(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    cleaned.trim_matches('_').to_string()
}

/// First non-empty line of the just-copied file, trimmed to 80 chars.
/// Leading markdown `#` / `-` / `*` / `>` markers are stripped so the
/// summary reads as a snippet, not as heading syntax inside the index
/// bullet. Returns "(empty)" for empty files.
fn first_summary_line(target: &Path) -> String {
    let text = match std::fs::read_to_string(target) {
        Ok(t) => t,
        Err(_) => return "(binary or unreadable)".into(),
    };
    for line in text.lines() {
        let stripped = line.trim_start_matches(|c: char| {
            c == '#' || c == '-' || c == '*' || c == '>' || c.is_whitespace()
        });
        let trimmed = stripped.trim();
        if !trimmed.is_empty() {
            let mut s: String = trimmed.chars().take(80).collect();
            if trimmed.chars().count() > 80 {
                s.push('…');
            }
            return s;
        }
    }
    "(empty)".into()
}

// `append_index_entry` + `append_log_entry` removed in M6.25 — the
// new `update_index_for_write` and `append_log_header` (defined
// below in the BUG #1 + #7 sections) replace them with the
// frontmatter-aware index update and the greppable `## [date] verb |
// alias` log format.

/// Render the concatenated active-KMS block to splice into a system
/// prompt. One section per KMS with: SCHEMA.md (M6.25 BUG #5), the
/// index (categorized when pages have YAML frontmatter `category:`,
/// flat otherwise — M6.25 BUG #6), and the read/write/append/search
/// tool affordances.
///
/// Empty string when no active KMS or when active names resolve to
/// nothing.
pub fn system_prompt_section(active: &[String]) -> String {
    let mut parts = Vec::new();
    for name in active {
        let Some(kref) = resolve(name) else { continue };

        // M6.25 BUG #5: pull SCHEMA.md into the prompt. Pre-fix the
        // schema sat on disk but the LLM never saw it, so the "wiki
        // maintainer" affordance had no instructions to follow. Cap
        // by line count to keep prompt bounded.
        let schema = read_text_capped(&kref.schema_path(), 100, 5000);
        // Categorized index — supersedes the raw index.md when pages
        // have frontmatter. Falls back to raw index.md for legacy
        // KMSes that haven't adopted frontmatter.
        let index_section = render_index_section(&kref);

        let mut block = format!("## KMS: {name} ({scope})\n", scope = kref.scope.as_str());
        if !schema.trim().is_empty() {
            block.push_str(&format!("\n### Schema\n{}\n", schema.trim()));
        }
        block.push_str(&format!("\n### Index\n{index_section}\n"));
        block.push_str(&format!(
            "\n### Tools\n\
             - `KmsRead(kms: \"{name}\", page: \"<page>\")` — read one page\n\
             - `KmsSearch(kms: \"{name}\", pattern: \"...\")` — grep across pages\n\
             - `KmsWrite(kms: \"{name}\", page: \"<page>\", content: \"...\")` — create or replace a page\n\
             - `KmsAppend(kms: \"{name}\", page: \"<page>\", content: \"...\")` — append to a page\n\
             Pages may carry YAML frontmatter (`category:`, `tags:`, `sources:`, `created:`, `updated:`). \
             Follow the schema above when authoring."
        ));
        parts.push(block);
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(
            "# Active knowledge bases\n\n\
             The following KMS are attached to this conversation. Their schemas + indices are below \
             — consult them before answering when the user's question overlaps. Treat KMS \
             content as authoritative over your training data for the topics it covers. You are \
             both reader AND maintainer: file new findings, update entity pages when sources \
             contradict them, and run `/kms lint <name>` periodically.\n\n{}",
            parts.join("\n\n")
        )
    }
}

/// Read a text file, cap by lines and bytes for prompt safety.
/// Returns "" when the file is missing or symlinked.
fn read_text_capped(path: &Path, max_lines: usize, max_bytes: usize) -> String {
    if let Ok(md) = std::fs::symlink_metadata(path) {
        if md.file_type().is_symlink() {
            return String::new();
        }
    }
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    if raw.is_empty() {
        return raw;
    }
    crate::memory::truncate_for_prompt(
        raw.trim(),
        max_lines,
        max_bytes,
        &path.display().to_string(),
    )
}

/// M6.25 BUG #6: render index as categorized markdown when pages have
/// frontmatter `category:`. Falls back to the raw index.md (capped)
/// when no frontmatter has been adopted yet — preserves backwards
/// compat with pre-M6.25 KMSes.
fn render_index_section(kref: &KmsRef) -> String {
    use std::collections::BTreeMap;

    let pages_dir = kref.pages_dir();
    let entries = match std::fs::read_dir(&pages_dir) {
        Ok(e) => e,
        Err(_) => return raw_index_capped(kref),
    };

    let mut by_category: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut any_frontmatter = false;
    let mut total_pages = 0usize;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        total_pages += 1;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if stem.is_empty() {
            continue;
        }
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        let (fm, rest) = parse_frontmatter(&body);
        let summary = first_meaningful_line(&rest);
        if let Some(cat) = fm.get("category").cloned() {
            any_frontmatter = true;
            by_category.entry(cat).or_default().push((stem, summary));
        } else {
            by_category
                .entry("uncategorized".into())
                .or_default()
                .push((stem, summary));
        }
    }

    if !any_frontmatter {
        return raw_index_capped(kref);
    }

    let mut out = String::new();
    let mut shown = 0usize;
    let cap = crate::memory::MEMORY_INDEX_MAX_LINES;
    for (cat, mut pages) in by_category {
        pages.sort();
        out.push_str(&format!("\n**{cat}**\n"));
        for (stem, summary) in pages {
            if shown >= cap {
                out.push_str(&format!(
                    "\n_… index truncated at {cap} entries (total: {total_pages})_\n"
                ));
                return out;
            }
            out.push_str(&format!("- [{stem}](pages/{stem}.md) — {summary}\n"));
            shown += 1;
        }
    }
    out
}

fn raw_index_capped(kref: &KmsRef) -> String {
    let index = kref.read_index();
    if index.trim().is_empty() {
        return "(empty index)".into();
    }
    crate::memory::truncate_for_prompt(
        index.trim(),
        crate::memory::MEMORY_INDEX_MAX_LINES,
        crate::memory::MEMORY_INDEX_MAX_BYTES,
        &format!("KMS index `{}`", kref.name),
    )
}

/// First non-empty line of body text, stripped of markdown markers,
/// trimmed to 80 chars. Used for index summaries.
fn first_meaningful_line(body: &str) -> String {
    for line in body.lines() {
        let stripped = line.trim_start_matches(|c: char| {
            c == '#' || c == '-' || c == '*' || c == '>' || c.is_whitespace()
        });
        let trimmed = stripped.trim();
        if !trimmed.is_empty() {
            let mut s: String = trimmed.chars().take(80).collect();
            if trimmed.chars().count() > 80 {
                s.push('…');
            }
            return s;
        }
    }
    "(empty)".into()
}

// ────────────────────────────────────────────────────────────────────────
// M6.25 BUG #9: YAML frontmatter convention for KMS pages.
//
// Tiny, hand-rolled parser — we deliberately don't pull in `serde_yaml`
// for this. Pages either start with `---\n<key>: <value>\n...\n---\n`
// or they don't. Values are flat strings (single line), no nesting,
// no anchors, no multiline. That matches the documented convention
// (`category:`, `tags:`, `sources:`, `created:`, `updated:`) — anything
// fancier should live in the page body, not the metadata.

/// Parse `(frontmatter, body)` from a page. Frontmatter map preserves
/// insertion order via Vec under the hood (BTreeMap is fine — keys
/// are conventional and small). Returns `(empty, original)` when no
/// frontmatter delimiter present.
pub fn parse_frontmatter(s: &str) -> (std::collections::BTreeMap<String, String>, String) {
    let mut map = std::collections::BTreeMap::new();
    let trimmed = s.trim_start_matches('\u{FEFF}');
    let Some(after_open) = trimmed.strip_prefix("---\n") else {
        return (map, s.to_string());
    };
    // Find the closing `---\n` (or `---` at EOF) anchored to start-of-line.
    let close_idx = after_open.find("\n---\n").or_else(|| {
        if after_open.ends_with("\n---") {
            Some(after_open.len() - 4)
        } else {
            None
        }
    });
    let Some(close) = close_idx else {
        return (map, s.to_string());
    };
    let yaml = &after_open[..close];
    let body = if close + 5 <= after_open.len() {
        // skip "\n---\n"
        &after_open[close + 5..]
    } else {
        ""
    };
    for line in yaml.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_string();
            let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
            if !key.is_empty() {
                map.insert(key, val);
            }
        }
    }
    (map, body.to_string())
}

/// Serialize a frontmatter map + body into a page string. Empty map →
/// just the body (no `---` block).
pub fn write_frontmatter(map: &std::collections::BTreeMap<String, String>, body: &str) -> String {
    if map.is_empty() {
        return body.to_string();
    }
    let mut out = String::from("---\n");
    for (k, v) in map {
        // YAML-safe values: if the value contains `:`, `#`, leading
        // whitespace, or quote chars, wrap in double quotes and
        // escape internal double quotes.
        let needs_quote = v.contains(':')
            || v.contains('#')
            || v.starts_with(' ')
            || v.contains('"')
            || v.contains('\n');
        if needs_quote {
            let escaped = v.replace('"', "\\\"");
            out.push_str(&format!("{k}: \"{escaped}\"\n"));
        } else {
            out.push_str(&format!("{k}: {v}\n"));
        }
    }
    out.push_str("---\n");
    out.push_str(body);
    out
}

// ────────────────────────────────────────────────────────────────────────
// M6.25 BUG #1 + #4: write helpers for KMS pages.
//
// `KmsWrite` / `KmsAppend` tools and the `/kms file-answer` slash
// command bypass `Sandbox::check_write` to land inside the KMS root
// (project-scope `.thclaws/kms/.../pages/...` is otherwise blocked).
// Same pattern as TodoWrite's intentional `.thclaws/todos.md` carve-
// out: the path is computed from a validated KMS name + a validated
// page name (no `..`, no path separators, no symlinks, must resolve
// inside the KMS root via `KmsRef::page_path`-style canonicalization).
//
// We don't want the LLM passing an arbitrary file path here.

/// Resolve `page_name` to a writable path inside `kref.pages_dir()`.
/// Differs from `KmsRef::page_path` — that one requires the file to
/// EXIST so canonicalize works. This one is for create-or-replace, so
/// it canonicalizes the parent directory and ensures the candidate
/// resolves under it.
pub fn writable_page_path(kref: &KmsRef, page_name: &str) -> Result<PathBuf> {
    if page_name.is_empty()
        || page_name.contains("..")
        || page_name.contains('/')
        || page_name.contains('\\')
        || page_name.contains('\0')
        || page_name.chars().any(|c| c.is_control())
        || Path::new(page_name).is_absolute()
    {
        return Err(Error::Tool(format!(
            "invalid page name '{page_name}' — no '..', path separators, or control chars"
        )));
    }
    let stem = page_name.trim_end_matches(".md");
    if RESERVED_PAGE_STEMS
        .iter()
        .any(|r| r.eq_ignore_ascii_case(stem))
    {
        return Err(Error::Tool(format!(
            "page name '{page_name}' is reserved — pick another stem"
        )));
    }
    let name = if page_name.ends_with(".md") {
        page_name.to_string()
    } else {
        format!("{page_name}.md")
    };

    let pages_dir = kref.pages_dir();
    std::fs::create_dir_all(&pages_dir)
        .map_err(|e| Error::Tool(format!("ensure pages dir for '{}': {e}", kref.name)))?;
    // Refuse if pages/ itself is a symlink (would let an attacker
    // redirect writes outside the KMS root).
    if let Ok(md) = std::fs::symlink_metadata(&pages_dir) {
        if md.file_type().is_symlink() {
            return Err(Error::Tool(format!(
                "kms '{}' has a symlinked pages/ directory — refusing to write",
                kref.name
            )));
        }
    }
    let canon_pages = std::fs::canonicalize(&pages_dir)
        .map_err(|e| Error::Tool(format!("canonicalize pages dir: {e}")))?;
    let candidate = canon_pages.join(&name);
    // The candidate may not exist yet (create case) — verify the
    // parent canonicalizes inside pages_dir, and that the file
    // (if it exists) is not a symlink to outside.
    if let Ok(canon_existing) = std::fs::canonicalize(&candidate) {
        if !canon_existing.starts_with(&canon_pages) {
            return Err(Error::Tool(format!(
                "page '{page_name}' resolves outside pages/ — symlink escape rejected"
            )));
        }
    }
    Ok(candidate)
}

/// Write (create-or-replace) a page. Bumps `updated:` frontmatter to
/// today, preserves existing other frontmatter when the body itself
/// includes a `---` block. Updates the index.md bullet under the
/// page's category. Appends a log entry.
pub fn write_page(kref: &KmsRef, page_name: &str, content: &str) -> Result<PathBuf> {
    let path = writable_page_path(kref, page_name)?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("page")
        .to_string();
    let existed = path.exists();

    // Merge user-supplied content's frontmatter with auto-stamped
    // `updated:` (and `created:` on new pages). User-supplied keys
    // win on conflict — they explicitly set them.
    let (mut fm, body) = parse_frontmatter(content);
    let today = crate::usage::today_str();
    fm.entry("updated".into()).or_insert_with(|| today.clone());
    if !existed {
        fm.entry("created".into()).or_insert(today.clone());
    }
    let serialized = write_frontmatter(&fm, &body);
    std::fs::write(&path, serialized.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;

    let summary = first_meaningful_line(&body);
    let category = fm.get("category").cloned();
    update_index_for_write(kref, &stem, &summary, category.as_deref(), existed)?;
    append_log_header(kref, if existed { "edited" } else { "wrote" }, &stem)?;
    Ok(path)
}

/// Append a chunk to a page. If the page doesn't exist, create it
/// (no frontmatter — the model can write a full page later via
/// `KmsWrite` to add metadata). Bumps `updated:` if frontmatter
/// already present.
pub fn append_to_page(kref: &KmsRef, page_name: &str, chunk: &str) -> Result<PathBuf> {
    use std::io::Write;
    let path = writable_page_path(kref, page_name)?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("page")
        .to_string();
    let existed = path.exists();
    if existed {
        // Bump updated: in frontmatter if present, leave body alone,
        // append the new chunk after a newline.
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        let (mut fm, body) = parse_frontmatter(&raw);
        if !fm.is_empty() {
            fm.insert("updated".into(), crate::usage::today_str());
            let mut new_body = body;
            if !new_body.ends_with('\n') {
                new_body.push('\n');
            }
            new_body.push_str(chunk);
            let serialized = write_frontmatter(&fm, &new_body);
            std::fs::write(&path, serialized.as_bytes())
                .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
        } else {
            // No frontmatter — straight append.
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .map_err(|e| Error::Tool(format!("open {}: {e}", path.display())))?;
            if !raw.ends_with('\n') {
                writeln!(f).ok();
            }
            f.write_all(chunk.as_bytes())
                .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
        }
    } else {
        // Create with bare body (no frontmatter); subsequent
        // writes can add metadata.
        std::fs::write(&path, chunk.as_bytes())
            .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
        let summary = first_meaningful_line(chunk);
        update_index_for_write(kref, &stem, &summary, None, false)?;
    }
    append_log_header(kref, "appended", &stem)?;
    Ok(path)
}

/// Update index.md to reflect a write. Adds a fresh bullet (or
/// replaces an existing one for the same page). Categorization is a
/// hint — the actual rendering for the system prompt is built from
/// per-page frontmatter at read time, so this is just so the on-disk
/// index.md stays human-readable.
fn update_index_for_write(
    kref: &KmsRef,
    stem: &str,
    summary: &str,
    _category: Option<&str>,
    existed: bool,
) -> Result<()> {
    use std::io::Write;
    let path = kref.index_path();
    let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
    let needle = format!("(pages/{stem}.md)");
    if existed || existing.contains(&needle) {
        existing = existing
            .lines()
            .filter(|l| !l.contains(&needle))
            .collect::<Vec<_>>()
            .join("\n");
        if !existing.ends_with('\n') {
            existing.push('\n');
        }
    }
    if !existing.ends_with('\n') && !existing.is_empty() {
        existing.push('\n');
    }
    existing.push_str(&format!("- [{stem}](pages/{stem}.md) — {summary}\n"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| Error::Tool(format!("open {}: {e}", path.display())))?;
    f.write_all(existing.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
    Ok(())
}

/// M6.25 BUG #7: append a header-style log entry for greppability.
/// `## [YYYY-MM-DD] verb | alias`. Pre-fix `- date verb src → dest`
/// bullets weren't greppable as "give me the last 5 ingests".
fn append_log_header(kref: &KmsRef, verb: &str, alias: &str) -> Result<()> {
    use std::io::Write;
    let path = kref.log_path();
    let line = format!("## [{}] {verb} | {alias}\n", crate::usage::today_str());
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .map_err(|e| Error::Tool(format!("open {}: {e}", path.display())))?;
    f.write_all(line.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────
// M6.25 BUG #3: lint — pure-read health check.

/// What `lint()` found. Each list is a category of issue.
#[derive(Debug, Default)]
pub struct LintReport {
    pub orphan_pages: Vec<String>, // page exists but no inbound link from any other page
    pub broken_links: Vec<(String, String)>, // (page, target) where pages/<target>.md doesn't exist
    pub index_orphans: Vec<String>, // index entry but no underlying file
    pub missing_in_index: Vec<String>, // page file but no index entry
    pub missing_frontmatter: Vec<String>, // page has no `---` block
}

impl LintReport {
    pub fn total_issues(&self) -> usize {
        self.orphan_pages.len()
            + self.broken_links.len()
            + self.index_orphans.len()
            + self.missing_in_index.len()
            + self.missing_frontmatter.len()
    }
}

/// Walk a KMS and report common health issues. Pure-read; doesn't
/// modify the wiki. Inbound-link detection is greedy: any markdown
/// link `[*](pages/<stem>.md)` counts.
pub fn lint(kref: &KmsRef) -> Result<LintReport> {
    use std::collections::HashSet;
    let mut report = LintReport::default();

    let pages_dir = kref.pages_dir();
    let entries = match std::fs::read_dir(&pages_dir) {
        Ok(e) => e,
        Err(_) => return Ok(report),
    };

    let mut all_stems: HashSet<String> = HashSet::new();
    let mut page_bodies: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if stem.is_empty() {
            continue;
        }
        all_stems.insert(stem.clone());
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        page_bodies.push((stem, body));
    }

    // Frontmatter audit + outbound link extraction.
    let link_re = regex::Regex::new(r"\(pages/([^)]+?)\.md\)").unwrap();
    let mut inbound_targets: HashSet<String> = HashSet::new();
    for (stem, body) in &page_bodies {
        let (fm, _rest) = parse_frontmatter(body);
        if fm.is_empty() {
            report.missing_frontmatter.push(stem.clone());
        }
        for cap in link_re.captures_iter(body) {
            let target = cap[1].to_string();
            inbound_targets.insert(target.clone());
            if !all_stems.contains(&target) {
                report.broken_links.push((stem.clone(), target));
            }
        }
    }

    // Orphan pages: exist on disk but no other page links to them.
    for (stem, _) in &page_bodies {
        if !inbound_targets.contains(stem) {
            report.orphan_pages.push(stem.clone());
        }
    }

    // Index <-> filesystem cross-check.
    let index = kref.read_index();
    let index_re = regex::Regex::new(r"\(pages/([^)]+?)\.md\)").unwrap();
    let mut indexed: HashSet<String> = HashSet::new();
    for cap in index_re.captures_iter(&index) {
        indexed.insert(cap[1].to_string());
    }
    for stem in &indexed {
        if !all_stems.contains(stem) {
            report.index_orphans.push(stem.clone());
        }
    }
    for stem in &all_stems {
        if !indexed.contains(stem) {
            report.missing_in_index.push(stem.clone());
        }
    }

    report.orphan_pages.sort();
    report.broken_links.sort();
    report.index_orphans.sort();
    report.missing_in_index.sort();
    report.missing_frontmatter.sort();
    Ok(report)
}

/// Build the `kms_update` envelope the frontend's KMS sidebar
/// consumes. M6.36 SERVE9c — moved from `gui.rs` to an always-on
/// module so the WS transport's `kms_list` IPC arm can call it from
/// `crate::ipc::handle_ipc`. Same JSON shape both transports emit.
pub fn build_update_payload() -> serde_json::Value {
    let active: std::collections::HashSet<String> = crate::config::ProjectConfig::load()
        .and_then(|c| c.kms.map(|k| k.active))
        .unwrap_or_default()
        .into_iter()
        .collect();
    let kmss: Vec<serde_json::Value> = list_all()
        .into_iter()
        .map(|k| {
            serde_json::json!({
                "name": k.name,
                "scope": k.scope.as_str(),
                "active": active.contains(&k.name),
            })
        })
        .collect();
    serde_json::json!({
        "type": "kms_update",
        "kmss": kmss,
    })
}

/// Test-only lock shared by every test in this module *and* in
/// `tools::kms` that mutates the process env (HOME, cwd). Without
/// this, parallel tests race on env — which can also break unrelated
/// tests (bash/grep) whose sandbox resolver reads cwd.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_home: Option<String>,
        prev_userprofile: Option<String>,
        prev_cwd: std::path::PathBuf,
        _home_dir: tempfile::TempDir,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // Restore cwd first — set_current_dir against a dropped
            // tempdir would fail silently otherwise.
            let _ = std::env::set_current_dir(&self.prev_cwd);
            match &self.prev_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
            match &self.prev_userprofile {
                Some(h) => std::env::set_var("USERPROFILE", h),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }

    /// Acquire exclusive access to the process env + cwd for this
    /// test, set HOME (+ USERPROFILE on Windows) to a fresh tempdir,
    /// leave cwd pointing at that tempdir. Dropped at end of test to
    /// restore.
    fn scoped_home() -> EnvGuard {
        let lock = test_env_lock();
        let prev_home = std::env::var("HOME").ok();
        let prev_userprofile = std::env::var("USERPROFILE").ok();
        let prev_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        std::env::set_var("USERPROFILE", dir.path());
        std::env::set_current_dir(dir.path()).unwrap();
        EnvGuard {
            _lock: lock,
            prev_home,
            prev_userprofile,
            prev_cwd,
            _home_dir: dir,
        }
    }

    #[test]
    fn create_seeds_starter_files() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::User).unwrap();
        assert!(k.index_path().exists());
        assert!(k.log_path().exists());
        assert!(k.schema_path().exists());
        assert!(k.pages_dir().is_dir());
    }

    #[test]
    fn create_is_idempotent() {
        let _home = scoped_home();
        let a = create("notes", KmsScope::User).unwrap();
        let b = create("notes", KmsScope::User).unwrap();
        assert_eq!(a.root, b.root);
    }

    #[test]
    fn create_rejects_path_traversal() {
        let _home = scoped_home();
        assert!(create("../evil", KmsScope::User).is_err());
        assert!(create("foo/bar", KmsScope::User).is_err());
    }

    #[test]
    fn resolve_prefers_project_over_user() {
        let _home = scoped_home();
        create("shared", KmsScope::User).unwrap();
        create("shared", KmsScope::Project).unwrap();
        let found = resolve("shared").unwrap();
        assert_eq!(found.scope, KmsScope::Project);
    }

    #[test]
    fn list_all_returns_project_then_user() {
        let _home = scoped_home();
        create("user-only", KmsScope::User).unwrap();
        create("proj-only", KmsScope::Project).unwrap();
        let all = list_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].scope, KmsScope::Project);
        assert_eq!(all[1].scope, KmsScope::User);
    }

    #[test]
    fn system_prompt_section_empty_when_no_active() {
        let _home = scoped_home();
        assert_eq!(system_prompt_section(&[]), "");
    }

    #[test]
    fn system_prompt_section_includes_index_text() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(k.index_path(), "# nb\n- [foo](pages/foo.md) — foo page\n").unwrap();
        let out = system_prompt_section(&["nb".into()]);
        assert!(out.contains("## KMS: nb"));
        assert!(out.contains("foo page"));
        assert!(out.contains("KmsRead"));
    }

    #[test]
    fn system_prompt_section_skips_missing() {
        let _home = scoped_home();
        let out = system_prompt_section(&["does-not-exist".into()]);
        assert_eq!(out, "");
    }

    #[test]
    fn page_path_rejects_traversal() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        assert!(k.page_path("../../etc/passwd").is_err());
        assert!(k.page_path("/etc/passwd").is_err());
        assert!(k.page_path("foo/bar").is_err()); // path separator
        assert!(k.page_path("").is_err()); // empty name
        assert!(k.page_path("foo\0bar").is_err()); // null byte

        // The happy path: create the file first (page_path now requires
        // the file to exist so it can canonicalize + symlink-check).
        std::fs::write(k.pages_dir().join("ok-page.md"), "body").unwrap();
        assert!(k.page_path("ok-page").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn page_path_rejects_symlink_to_outside() {
        use std::os::unix::fs::symlink;
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();

        // Attacker plants a symlink in pages/ to an outside target.
        let target_dir = tempfile::tempdir().unwrap();
        let outside_file = target_dir.path().join("secret.md");
        std::fs::write(&outside_file, "top secret").unwrap();
        let symlink_path = k.pages_dir().join("leaked.md");
        symlink(&outside_file, &symlink_path).unwrap();

        // Despite the file existing (via symlink), page_path rejects
        // because canonical candidate escapes the KMS root.
        let result = k.page_path("leaked");
        assert!(result.is_err(), "expected symlink to be rejected");
        let err_str = format!("{}", result.unwrap_err());
        assert!(
            err_str.contains("symlink escape") || err_str.contains("outside the KMS"),
            "unexpected error: {err_str}"
        );
    }

    /// M6.25 BUG #2: ingest now SPLITS source from page. Raw content
    /// lands in `sources/<alias>.<ext>`; a stub page with frontmatter
    /// lands in `pages/<alias>.md` pointing at it. Verifies the new
    /// shape end-to-end.
    #[test]
    fn ingest_splits_source_from_page() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("intro.md");
        std::fs::write(&src, "# Intro\n\nFirst real line of content.\n").unwrap();

        let result = ingest(&k, &src, None, false).unwrap();
        assert_eq!(result.alias, "intro");
        assert!(!result.overwrote);
        assert!(result.target.exists());
        // The target is the page stub, not the raw source.
        assert!(result.target.ends_with("pages/intro.md"));

        // Raw source lives under sources/ — verbatim.
        let source_copy = k.root.join("sources/intro.md");
        let raw = std::fs::read_to_string(&source_copy).unwrap();
        assert!(raw.contains("First real line"));

        // Page is a stub with frontmatter pointing back at the source.
        let page_body = std::fs::read_to_string(&result.target).unwrap();
        let (fm, body) = parse_frontmatter(&page_body);
        assert_eq!(fm.get("sources").map(String::as_str), Some("intro"));
        assert_eq!(
            fm.get("category").map(String::as_str),
            Some("uncategorized")
        );
        assert!(fm.contains_key("created"));
        assert!(fm.contains_key("updated"));
        assert!(body.contains("Stub page"));
        assert!(body.contains("sources/intro.md"));

        // Index.md now has a bullet pointing at the page.
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(
            index.contains("- [intro](pages/intro.md)"),
            "index missing bullet, got:\n{index}"
        );

        // M6.25 BUG #7: log uses `## [date] verb | alias` header form.
        let log = std::fs::read_to_string(k.log_path()).unwrap();
        assert!(
            log.contains("## [") && log.contains("] ingested | intro"),
            "log missing header-style entry, got:\n{log}"
        );
    }

    #[test]
    fn ingest_collides_without_force() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("page.md");
        std::fs::write(&src, "a").unwrap();

        ingest(&k, &src, Some("topic"), false).unwrap();
        let err = ingest(&k, &src, Some("topic"), false).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("already exists"),
            "expected collision, got: {msg}"
        );

        // --force replaces, and is flagged as overwrote. The raw source
        // copy carries the new bytes; the page stub is regenerated.
        std::fs::write(&src, "b").unwrap();
        let r = ingest(&k, &src, Some("topic"), true).unwrap();
        assert!(r.overwrote);
        let raw = std::fs::read_to_string(k.root.join("sources/topic.md")).unwrap();
        assert_eq!(raw, "b");
    }

    #[test]
    fn ingest_rejects_unknown_extension() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("bin.xyz");
        std::fs::write(&src, "data").unwrap();
        let err = ingest(&k, &src, None, false).unwrap_err();
        assert!(format!("{err}").contains("not supported"));
    }

    #[test]
    fn ingest_rejects_reserved_alias() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("file.md");
        std::fs::write(&src, "x").unwrap();
        let err = ingest(&k, &src, Some("index"), false).unwrap_err();
        assert!(format!("{err}").contains("reserved"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlink_kms_dir() {
        use std::os::unix::fs::symlink;
        let _home = scoped_home();

        // Attacker plants a symlink where a KMS dir should be.
        let target = tempfile::tempdir().unwrap();
        let kms_root = scope_root(KmsScope::User).unwrap();
        std::fs::create_dir_all(&kms_root).unwrap();
        symlink(target.path(), kms_root.join("evil")).unwrap();

        // resolve() should not return a KmsRef for a symlinked dir.
        assert!(
            resolve("evil").is_none(),
            "symlinked KMS dir should be rejected"
        );
    }

    // ─── M6.25: frontmatter (BUG #9) ──────────────────────────────────────

    #[test]
    fn parse_frontmatter_extracts_keys_and_strips_block() {
        let s = "---\ncategory: research\ntags: ai\nsources: paper-x\n---\n# Body\n\nHello.\n";
        let (fm, body) = parse_frontmatter(s);
        assert_eq!(fm.get("category").map(String::as_str), Some("research"));
        assert_eq!(fm.get("tags").map(String::as_str), Some("ai"));
        assert_eq!(fm.get("sources").map(String::as_str), Some("paper-x"));
        assert_eq!(body, "# Body\n\nHello.\n");
    }

    #[test]
    fn parse_frontmatter_no_block_returns_empty_and_original() {
        let s = "# No frontmatter\n\nHello.\n";
        let (fm, body) = parse_frontmatter(s);
        assert!(fm.is_empty());
        assert_eq!(body, s);
    }

    #[test]
    fn write_frontmatter_round_trips() {
        let mut fm = std::collections::BTreeMap::new();
        fm.insert("category".into(), "research".into());
        fm.insert("note".into(), "has: colon".into()); // forces quoting
        let serialized = write_frontmatter(&fm, "body text\n");
        let (parsed, body) = parse_frontmatter(&serialized);
        assert_eq!(parsed.get("category").map(String::as_str), Some("research"));
        assert_eq!(parsed.get("note").map(String::as_str), Some("has: colon"));
        assert_eq!(body, "body text\n");
    }

    // ─── M6.25: write_page + append_to_page (BUG #1) ──────────────────────

    #[test]
    fn write_page_creates_with_stamps_and_index_bullet() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let path = write_page(&k, "topic", "# Topic\n\nBody.\n").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let (fm, body) = parse_frontmatter(&raw);
        assert!(fm.contains_key("created"), "created stamp missing");
        assert!(fm.contains_key("updated"), "updated stamp missing");
        assert!(body.contains("Body."));
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(index.contains("- [topic](pages/topic.md)"));
        let log = std::fs::read_to_string(k.log_path()).unwrap();
        assert!(log.contains("] wrote | topic"));
    }

    #[test]
    fn write_page_replace_preserves_created_bumps_updated() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let path = write_page(&k, "topic", "v1").unwrap();
        let raw1 = std::fs::read_to_string(&path).unwrap();
        let (fm1, _) = parse_frontmatter(&raw1);
        let created = fm1.get("created").cloned().unwrap();

        // Write again with explicit created override that should win.
        let _ = write_page(&k, "topic", "---\ncreated: 1999-01-01\n---\nv2").unwrap();
        let raw2 = std::fs::read_to_string(&path).unwrap();
        let (fm2, body2) = parse_frontmatter(&raw2);
        // User-supplied frontmatter wins on conflict.
        assert_eq!(fm2.get("created").map(String::as_str), Some("1999-01-01"));
        // updated still gets a stamp.
        assert!(fm2.contains_key("updated"));
        assert_eq!(body2, "v2");
        // Index has exactly one entry for `topic` (no duplicates).
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        let count = index.matches("(pages/topic.md)").count();
        assert_eq!(count, 1, "expected one entry, got {count}\n{index}");
        // Sanity: original `created` was today, the override moved it.
        assert_ne!(created, "1999-01-01");
    }

    #[test]
    fn append_to_page_creates_then_appends_with_frontmatter_bump() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        // First call creates with bare body (no frontmatter).
        append_to_page(&k, "log-page", "first chunk\n").unwrap();
        // Now write a frontmatter version then append more.
        write_page(&k, "log-page", "---\ncategory: log\n---\noriginal\n").unwrap();
        append_to_page(&k, "log-page", "second chunk\n").unwrap();
        let path = k.pages_dir().join("log-page.md");
        let raw = std::fs::read_to_string(&path).unwrap();
        let (fm, body) = parse_frontmatter(&raw);
        assert_eq!(fm.get("category").map(String::as_str), Some("log"));
        assert!(fm.contains_key("updated"));
        assert!(body.contains("original"));
        assert!(body.contains("second chunk"));
    }

    #[test]
    fn writable_page_path_rejects_traversal_and_reserved() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        assert!(writable_page_path(&k, "../etc/passwd").is_err());
        assert!(writable_page_path(&k, "foo/bar").is_err());
        assert!(writable_page_path(&k, "").is_err());
        assert!(writable_page_path(&k, "index").is_err()); // reserved
        assert!(writable_page_path(&k, "log").is_err());
        assert!(writable_page_path(&k, "SCHEMA").is_err());
        assert!(writable_page_path(&k, "ok-page").is_ok());
    }

    // ─── M6.25: lint (BUG #3) ─────────────────────────────────────────────

    #[test]
    fn lint_finds_orphans_broken_links_and_missing_frontmatter() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // Page A links to non-existent target → broken link.
        // Page B has no inbound links → orphan.
        // Page C has no frontmatter → flagged.
        std::fs::write(
            k.pages_dir().join("a.md"),
            "---\ncategory: x\n---\nLink: [nope](pages/missing.md)\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("b.md"),
            "---\ncategory: y\n---\nIsland.\n",
        )
        .unwrap();
        std::fs::write(k.pages_dir().join("c.md"), "no frontmatter here\n").unwrap();

        let report = lint(&k).unwrap();
        assert!(report
            .broken_links
            .iter()
            .any(|(p, t)| p == "a" && t == "missing"));
        assert!(report.orphan_pages.contains(&"b".to_string()));
        assert!(report.missing_frontmatter.contains(&"c".to_string()));
        assert!(report.total_issues() >= 3);
    }

    #[test]
    fn lint_clean_kms_reports_no_issues() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("a.md"),
            "---\ncategory: x\n---\nLink to [b](pages/b.md)\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("b.md"),
            "---\ncategory: x\n---\nLink to [a](pages/a.md)\n",
        )
        .unwrap();
        std::fs::write(k.index_path(), "- [a](pages/a.md)\n- [b](pages/b.md)\n").unwrap();
        let report = lint(&k).unwrap();
        assert_eq!(report.total_issues(), 0, "{report:?}");
    }

    // ─── M6.25: SCHEMA injection in system prompt (BUG #5) ────────────────

    #[test]
    fn system_prompt_includes_schema_when_present() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(
            k.schema_path(),
            "Pages must have category: in frontmatter.\n",
        )
        .unwrap();
        let out = system_prompt_section(&["nb".into()]);
        assert!(out.contains("### Schema"));
        assert!(out.contains("Pages must have category"));
        assert!(out.contains("KmsWrite")); // tool affordance listed
        assert!(out.contains("KmsAppend"));
    }

    // ─── M6.25: categorized index (BUG #6) ────────────────────────────────

    #[test]
    fn system_prompt_categorizes_index_by_frontmatter() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(
            k.pages_dir().join("paper-a.md"),
            "---\ncategory: research\n---\n# Paper A\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("api-x.md"),
            "---\ncategory: api\n---\n# API X\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("paper-b.md"),
            "---\ncategory: research\n---\n# Paper B\n",
        )
        .unwrap();
        let out = system_prompt_section(&["nb".into()]);
        assert!(
            out.contains("**research**"),
            "missing research section: {out}"
        );
        assert!(out.contains("**api**"), "missing api section: {out}");
        assert!(out.contains("paper-a"));
        assert!(out.contains("paper-b"));
        assert!(out.contains("api-x"));
    }

    // ─── M6.25: re-ingest cascade (BUG #10) ───────────────────────────────

    #[test]
    fn reingest_marks_dependent_pages_stale() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // Ingest source `topic`.
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("topic.md");
        std::fs::write(&src, "v1").unwrap();
        ingest(&k, &src, Some("topic"), false).unwrap();

        // Write a derived page that mentions `topic` in `sources:`.
        write_page(
            &k,
            "summary",
            "---\ncategory: synthesis\nsources: topic\n---\n# Summary\n",
        )
        .unwrap();

        // Re-ingest topic with --force → cascade fires.
        std::fs::write(&src, "v2").unwrap();
        let r = ingest(&k, &src, Some("topic"), true).unwrap();
        assert_eq!(r.cascaded, 1, "expected 1 dependent page marked stale");

        let derived = std::fs::read_to_string(k.pages_dir().join("summary.md")).unwrap();
        assert!(derived.contains("STALE"), "stale marker missing: {derived}");
        assert!(derived.contains("source `topic`"));
    }
}
