//! KMS read, search, write, and append tools — pair with [`crate::kms`]
//! to let the model both consult AND maintain wiki pages without
//! embeddings.
//!
//! All tools resolve the `kms` argument via `kms::resolve`, which
//! prefers a project-scope KMS over a user-scope one on name collision.
//!
//! M6.25 BUG #1: `KmsWrite` + `KmsAppend` deliberately bypass
//! `Sandbox::check_write` to land inside the KMS root (project-scope
//! `.thclaws/kms/.../pages/...` is otherwise blocked by the sandbox).
//! Path safety is enforced at a finer grain via `kms::writable_page_path`,
//! which validates the page name and canonicalizes it inside the
//! resolved KMS pages dir. Same intentional carve-out pattern as
//! TodoWrite's `.thclaws/todos.md` write.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

pub struct KmsReadTool;

#[async_trait]
impl Tool for KmsReadTool {
    fn name(&self) -> &'static str {
        "KmsRead"
    }

    fn description(&self) -> &'static str {
        "Read a single page from an attached knowledge base. Use after \
         spotting a relevant entry in the KMS index that the user's \
         question touches on."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kms":  {"type": "string", "description": "KMS name (from the active list)"},
                "page": {"type": "string", "description": "Page name (with or without .md)"}
            },
            "required": ["kms", "page"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let kms_name = req_str(&input, "kms")?;
        let page = req_str(&input, "page")?;
        let Some(kref) = crate::kms::resolve(kms_name) else {
            return Err(Error::Tool(format!(
                "no KMS named '{kms_name}' (check /kms list)"
            )));
        };
        let path = kref.page_path(page)?;
        let body = std::fs::read_to_string(&path)
            .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?;

        // Freshness signal — surface staleness inline so the model
        // hedges or re-verifies before citing facts that may have
        // drifted. The page itself is unchanged; only the tool
        // response prepends the warning. Addresses the LLM-Wiki
        // critique "error persistence — pages are treated as fresh
        // forever even when sources have moved on". `verified:`
        // frontmatter is only stamped by callers that actually
        // verified (research pipeline today; future /kms verify
        // command). Pages without `verified:` get a softer
        // "no verification record" hint rather than a date-based
        // alarm so existing user-curated content isn't shouted at.
        let warning = staleness_warning(&body);
        Ok(match warning {
            Some(w) => format!("{w}\n\n{body}"),
            None => body,
        })
    }
}

/// Inspect a page's frontmatter and return a one-line `[note: …]`
/// banner if it looks stale or unverified. `verified: YYYY-MM-DD`
/// older than [`STALE_DAYS_THRESHOLD`] → date-based warning; missing
/// `verified:` → softer "no verification record" hint. None means
/// the page is fresh enough that no banner is needed.
fn staleness_warning(body: &str) -> Option<String> {
    const STALE_DAYS_THRESHOLD: i64 = 90;
    let (fm, _) = crate::kms::parse_frontmatter(body);
    // Pages with no frontmatter at all (legacy / partial) → don't
    // shout; the model can see the missing frontmatter itself.
    if fm.is_empty() {
        return None;
    }
    match fm.get("verified") {
        None => Some(
            "[note: this page has no `verified:` frontmatter — provenance is best-effort, treat factual claims with caution]"
                .to_string(),
        ),
        Some(date_str) => {
            let today = crate::usage::today_str();
            let days = days_between_ymd(date_str, &today)?;
            if days > STALE_DAYS_THRESHOLD {
                Some(format!(
                    "[note: this page was last verified {days} days ago — sources may have drifted; re-verify before citing as current fact]"
                ))
            } else {
                None
            }
        }
    }
}

/// Days between two `YYYY-MM-DD` strings (lhs older → positive
/// result). Returns `None` on parse failure so the caller skips the
/// warning rather than surfacing a misleading number.
fn days_between_ymd(older: &str, newer: &str) -> Option<i64> {
    let parse = |s: &str| -> Option<(i32, u32, u32)> {
        let mut parts = s.trim().splitn(3, '-');
        let y: i32 = parts.next()?.parse().ok()?;
        let m: u32 = parts.next()?.parse().ok()?;
        let d: u32 = parts.next()?.parse().ok()?;
        Some((y, m, d))
    };
    let (oy, om, od) = parse(older)?;
    let (ny, nm, nd) = parse(newer)?;
    // Cheap day-count without pulling chrono into the tool: treat
    // every month as 30 days, every year as 365. Off by a couple
    // days at the boundary — fine for an "is this page stale?"
    // banner that triggers at 90-day granularity.
    let days_older = (oy as i64) * 365 + (om as i64) * 30 + (od as i64);
    let days_newer = (ny as i64) * 365 + (nm as i64) * 30 + (nd as i64);
    Some(days_newer - days_older)
}

pub struct KmsSearchTool;

#[async_trait]
impl Tool for KmsSearchTool {
    fn name(&self) -> &'static str {
        "KmsSearch"
    }

    fn description(&self) -> &'static str {
        "Search one knowledge base. Two modes, exactly one of which \
         must be provided:\n\
         - `query`: natural-language BM25 search across page title \
         (×4 boost), topic (×2), and body. Returns ranked hits with \
         snippet previews. Optional `tags` / `category` filters narrow \
         the candidate set. Requires the `kms_search_index` feature \
         build; falls back to regex with an advisory when unavailable.\n\
         - `pattern`: regex grep across page bodies, returns matching \
         lines as `page:line:text`. Use for exact-shape lookups (find \
         a specific TODO marker, function name, error code)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kms":      {"type": "string", "description": "KMS name (see /kms list)"},
                "query":    {"type": "string", "description": "Natural-language search query (BM25). Mutually exclusive with `pattern`."},
                "pattern":  {"type": "string", "description": "Regex pattern (line grep). Mutually exclusive with `query`."},
                "tags":     {"type": "array", "items": {"type": "string"}, "description": "Optional: limit `query` results to pages tagged with ANY of these. Ignored for `pattern`."},
                "category": {"type": "string", "description": "Optional: limit `query` results to pages whose frontmatter `category:` matches exactly."},
                "limit":    {"type": "integer", "description": "Max hits for `query` (default 10, capped at 50)."}
            },
            "required": ["kms"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let kms_name = req_str(&input, "kms")?;
        let Some(kref) = crate::kms::resolve(kms_name) else {
            return Err(Error::Tool(format!(
                "no KMS named '{kms_name}' (check /kms list)"
            )));
        };

        let query = input.get("query").and_then(|v| v.as_str()).map(str::trim);
        let pattern = input.get("pattern").and_then(|v| v.as_str()).map(str::trim);
        match (
            query.filter(|s| !s.is_empty()),
            pattern.filter(|s| !s.is_empty()),
        ) {
            (Some(_), Some(_)) => Err(Error::Tool(
                "KmsSearch: `query` and `pattern` are mutually exclusive — \
                 pick one (or read both arg descriptions to decide which)"
                    .into(),
            )),
            (None, None) => Err(Error::Tool(
                "KmsSearch: provide either `query` (BM25 ranked) or `pattern` (regex line grep)"
                    .into(),
            )),
            (Some(q), None) => kms_search_query_path(&kref, kms_name, q, &input),
            (None, Some(p)) => kms_search_pattern_path(&kref, kms_name, p),
        }
    }
}

/// Existing regex line-grep path — byte-identical to pre-Tier-2
/// behaviour for scripts memoising the `page:line:text` format.
fn kms_search_pattern_path(
    kref: &crate::kms::KmsRef,
    kms_name: &str,
    pattern: &str,
) -> Result<String> {
    let re = Regex::new(pattern).map_err(|e| Error::Tool(format!("regex: {e}")))?;
    let pages_dir = kref.pages_dir();
    // Refuse to walk if `pages/` itself is a symlink. Entry-level
    // symlink filtering below can't save us from a `pages -> /etc`
    // symlink because /etc's contents aren't themselves symlinks.
    if let Ok(md) = std::fs::symlink_metadata(&pages_dir) {
        if md.file_type().is_symlink() {
            return Err(Error::Tool(format!(
                "kms '{kms_name}' has a symlinked pages/ directory — refusing to read"
            )));
        }
    }
    let Ok(entries) = std::fs::read_dir(&pages_dir) else {
        return Ok(String::new());
    };
    let mut results: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        // Skip symlinks to prevent `ln -s ~/.ssh/id_rsa pages/leak.md`
        // style exfiltration via grep.
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if !path.extension().map(|e| e == "md").unwrap_or(false) {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let page_name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for (i, line) in contents.lines().enumerate() {
            if re.is_match(line) {
                results.push(format!("{}:{}:{}", page_name, i + 1, line));
            }
        }
    }
    results.sort();
    Ok(results.join("\n"))
}

/// BM25 path — only available when the `kms_search_index` Cargo
/// feature is on. Without the feature, returns a clear error so the
/// model knows to fall back to `pattern:`. With the feature, opens
/// the index (auto-builds on first call if missing per Tier 3),
/// runs the query, formats ranked hits.
#[cfg(feature = "kms_search_index")]
fn kms_search_query_path(
    kref: &crate::kms::KmsRef,
    _kms_name: &str,
    query: &str,
    input: &Value,
) -> Result<String> {
    // Parse optional filters.
    let tags: Vec<String> = input
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let category = input
        .get("category")
        .and_then(|v| v.as_str())
        .map(str::trim);
    let limit = input
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(10);

    // Auto-build-on-stale (Tier 3.A): if the manifest is missing or
    // its index_version doesn't match the current binary, do a full
    // rebuild from disk before serving. Cheap on first-touch
    // (single rebuild per KMS per binary version); transparent on
    // steady state (manifest hit → no rebuild).
    let index_dir = kref.root.join(".index");
    let needs_build = match read_manifest(&index_dir) {
        Some(m) => m.index_version != crate::kms_search_index::INDEX_VERSION,
        None => !index_dir.join("meta.json").exists(),
    };
    let mut advisory = String::new();
    if needs_build {
        match crate::kms_search_index::full_rebuild(&kref.root) {
            Ok(n) => {
                write_manifest(&index_dir);
                advisory = format!("[index rebuilt — {n} page(s) indexed]\n\n");
            }
            Err(e) => {
                return Err(Error::Tool(format!(
                    "KmsSearch: index rebuild failed: {e}\nFall back to `pattern:` or run /kms reindex"
                )));
            }
        }
    }

    let idx = crate::kms_search_index::get_or_open(&kref.root)
        .map_err(|e| Error::Tool(format!("KmsSearch: open index: {e}")))?;
    let cat_ref = category.filter(|s| !s.is_empty());
    let hits = idx
        .search(query, &tags, cat_ref, limit)
        .map_err(|e| Error::Tool(format!("KmsSearch: query: {e}")))?;

    Ok(format_hits(&advisory, &hits))
}

#[cfg(not(feature = "kms_search_index"))]
fn kms_search_query_path(
    _kref: &crate::kms::KmsRef,
    _kms_name: &str,
    _query: &str,
    _input: &Value,
) -> Result<String> {
    Err(Error::Tool(
        "KmsSearch `query:` requires the kms_search_index feature; \
         this binary was built without it. Use `pattern:` for regex \
         search instead. (Operators: build with `--features \
         kms_search_index` to enable BM25 search.)"
            .into(),
    ))
}

/// Operator-facing `/kms search` entry point — invoked by both the
/// CLI REPL handler and the GUI / `--serve` slash dispatcher.
/// `name` is a single KMS name OR the wildcard `*` (fan out across
/// every visible KMS per `kms::list_all`). Results from each KMS
/// are grouped under a `── KMS: <name> ──` header so attribution
/// stays unambiguous when `*` is used.
///
/// `is_pattern: true` routes through the regex line-grep path
/// (same surface as the model-callable tool's `pattern:`);
/// `false` uses BM25 `query:`. The format mirrors the tool output
/// the model sees — no separate "operator format" to maintain.
pub fn run_slash_search(name: &str, query: &str, is_pattern: bool) -> String {
    // Wildcard expansion: project + user scope visible KMSes, in
    // discovery order (project first so on-name-collision the
    // project entry runs first). `list_all` may return duplicates
    // when the same name exists in both scopes; dedupe by
    // (scope-tagged) root path so we don't search the same
    // directory twice.
    let kmses: Vec<crate::kms::KmsRef> = if name == "*" {
        let mut seen = std::collections::HashSet::new();
        crate::kms::list_all()
            .into_iter()
            .filter(|k| seen.insert(k.root.clone()))
            .collect()
    } else {
        match crate::kms::resolve(name) {
            Some(k) => vec![k],
            None => {
                return format!(
                    "no KMS named '{name}' (use `/kms list` to see what's visible, \
                     or `*` to search every KMS)"
                );
            }
        }
    };

    if kmses.is_empty() {
        return "no KMSes visible — create one with `/kms new <name>` first".to_string();
    }

    let mut out = String::new();
    let multi = kmses.len() > 1;
    for (idx, kref) in kmses.iter().enumerate() {
        if multi {
            if idx > 0 {
                out.push_str("\n");
            }
            out.push_str(&format!("── KMS: {} ──\n", kref.name));
        }
        let result = if is_pattern {
            // The pattern path takes a kms_name arg only for its
            // error message text; pass the resolved name.
            match kms_search_pattern_path(kref, &kref.name, query) {
                Ok(s) if s.is_empty() => "(no matches)".to_string(),
                Ok(s) => s,
                Err(e) => format!("(error: {e})"),
            }
        } else {
            // Re-use the model-callable query path. Construct the
            // same JSON shape the tool sees so format + fallback
            // semantics stay aligned.
            let input = serde_json::json!({
                "kms": kref.name,
                "query": query,
            });
            match kms_search_query_path(kref, &kref.name, query, &input) {
                Ok(s) => s,
                Err(e) => format!("(error: {e})"),
            }
        };
        out.push_str(&result);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[cfg(feature = "kms_search_index")]
fn format_hits(advisory: &str, hits: &[crate::kms_search_index::SearchHit]) -> String {
    if hits.is_empty() {
        return format!(
            "{advisory}(no hits — try `pattern:` for exact-shape lookups, or broaden the query)"
        );
    }
    let mut out = String::new();
    out.push_str(advisory);
    for (i, h) in hits.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("[score {:.2}] page: {}\n", h.score, h.page));
        if let Some(title) = &h.title {
            out.push_str(&format!("  title: {title}\n"));
        }
        if let Some(topic) = &h.topic {
            out.push_str(&format!("  topic: {topic}\n"));
        }
        if !h.snippet_preview.is_empty() {
            out.push_str(&format!("  preview: {}\n", h.snippet_preview));
        }
    }
    out
}

/// Tier 3.A: on-disk manifest distinguishing a current vs stale
/// index. Lives at `<kms_root>/.index/manifest.json`. Read on every
/// query to decide whether to auto-rebuild; written after every
/// full_rebuild.
#[cfg(feature = "kms_search_index")]
#[derive(serde::Serialize, serde::Deserialize)]
struct IndexManifest {
    index_version: u32,
    /// Unix seconds (i64 for serde compat; never negative in practice).
    last_full_rebuild_at: i64,
}

#[cfg(feature = "kms_search_index")]
fn read_manifest(index_dir: &std::path::Path) -> Option<IndexManifest> {
    let path = index_dir.join("manifest.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

#[cfg(feature = "kms_search_index")]
fn write_manifest(index_dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(index_dir);
    let manifest = IndexManifest {
        index_version: crate::kms_search_index::INDEX_VERSION,
        last_full_rebuild_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };
    if let Ok(json) = serde_json::to_string_pretty(&manifest) {
        let _ = std::fs::write(index_dir.join("manifest.json"), json);
    }
}

/// M6.25 BUG #1: write a KMS page. Create-or-replace; if the content
/// includes YAML frontmatter (`---\n...\n---\n`), it's preserved and
/// `updated:` is bumped to today. New pages get `created:` stamped.
/// Updates the index.md bullet and appends a `## [date] wrote | alias`
/// log entry. Bypasses `Sandbox::check_write` for the KMS pages dir
/// (validated by `kms::writable_page_path`).
pub struct KmsWriteTool;

#[async_trait]
impl Tool for KmsWriteTool {
    fn name(&self) -> &'static str {
        "KmsWrite"
    }

    fn description(&self) -> &'static str {
        "Create or replace a page in an attached knowledge base. Content \
         MUST start with YAML frontmatter:\n\
         \n\
         ```\n\
         ---\n\
         title: Human-readable page title\n\
         topic: One-line description of what this page covers\n\
         sources: [\"https://example.com/article\", \"session-XYZ\"]   # REQUIRED — provenance for every page\n\
         category: optional\n\
         tags: [optional, free-form]\n\
         ---\n\
         \n\
         Body content goes here…\n\
         ```\n\
         \n\
         `sources:` is required — pages without provenance are hard to \
         re-verify later. Valid values: external URLs, `session-<id>` for \
         facts learned in conversation, `memory` for stable user-supplied \
         knowledge, or `[]` (empty list) for opinion/convention pages \
         that have no external source (still better than omitting the \
         field — it's an explicit acknowledgement). Without `sources:` \
         the write succeeds but the response includes a warning, and \
         `KmsRead` later prepends a `[note: this page has no \
         verification record]` banner.\n\
         \n\
         `created:` / `updated:` are auto-stamped. The tool injects a \
         canonical `# {title}\\nDescription: {topic}\\n---` block before \
         the body so every page has a uniform header — DO NOT include \
         that block yourself (it will be added automatically). If you \
         intentionally want a different leading heading, write your own \
         `# heading` as the body's first line and the tool will respect \
         it. Missing `title:` falls back to the page filename; missing \
         `topic:` skips the Description line."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kms":     {"type": "string", "description": "KMS name (from the active list)"},
                "page":    {"type": "string", "description": "Page name (with or without .md). No path separators."},
                "content": {"type": "string", "description": "Full page content. Include YAML frontmatter with `title:`, `topic:`, AND `sources:` at the top; the body follows below. The tool auto-injects `# {title}\\nDescription: {topic}\\n---` before the body."}
            },
            "required": ["kms", "page", "content"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let kms_name = req_str(&input, "kms")?;
        let page = req_str(&input, "page")?;
        let content = req_str(&input, "content")?;
        // dev-plan/32 Stage M: gate KMS writes inside workflow
        // subagent calls. Outside `/workflow run` this is a no-op
        // and the call proceeds as before.
        crate::workflow::check_kms_write_capability(kms_name)?;
        let Some(kref) = crate::kms::resolve(kms_name) else {
            return Err(Error::Tool(format!(
                "no KMS named '{kms_name}' (check /kms list)"
            )));
        };
        // Pre-flight provenance check: pages without `sources:` in
        // frontmatter still write (soft enforcement keeps the tool
        // usable for legacy / quick captures), but the response
        // carries a warning so the model notices on the spot rather
        // than waiting for a future `KmsRead` to surface the gap.
        // The `KmsRead` staleness banner is the second layer of the
        // same enforcement.
        let provenance_warning = check_provenance(content);
        let path = crate::kms::write_page(&kref, page, content)?;
        let base = format!("wrote {} ({} bytes)", path.display(), content.len());
        Ok(match provenance_warning {
            Some(w) => format!("{base}\nwarning: {w}"),
            None => base,
        })
    }
}

/// Inspect content's frontmatter for the `sources:` key. Returns a
/// one-line warning when missing/empty so the KmsWrite caller can
/// notice immediately. Frontmatter-free pages are exempt (legacy /
/// freeform — separate concern; the `KmsRead` banner handles them).
fn check_provenance(content: &str) -> Option<String> {
    let (fm, _) = crate::kms::parse_frontmatter(content);
    if fm.is_empty() {
        return None;
    }
    match fm.get("sources").map(String::as_str).map(str::trim) {
        None => Some(
            "no `sources:` frontmatter — add a URL list (or `[]` for \
             opinion/convention pages, or `session-<id>` / `memory` for \
             in-conversation provenance) so the page is auditable later"
                .to_string(),
        ),
        Some("") => Some(
            "`sources:` is present but empty — set explicit values \
             (URLs / `session-<id>` / `memory` / `[]`) so the field's \
             intent isn't ambiguous"
                .to_string(),
        ),
        Some(_) => None,
    }
}

/// M6.25 BUG #1: append to a KMS page. If the page exists with
/// frontmatter, only the body grows and `updated:` bumps. If no
/// frontmatter, plain append. If the page doesn't exist, creates it
/// with the given content (no frontmatter — model can rewrite via
/// KmsWrite to add metadata).
pub struct KmsAppendTool;

#[async_trait]
impl Tool for KmsAppendTool {
    fn name(&self) -> &'static str {
        "KmsAppend"
    }

    fn description(&self) -> &'static str {
        "Append content to a page in an attached knowledge base. \
         Faster than KmsWrite for incremental updates (logs, journal \
         entries, accumulating notes). Bumps `updated:` if the page \
         already has frontmatter."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kms":     {"type": "string", "description": "KMS name"},
                "page":    {"type": "string", "description": "Page name (with or without .md)"},
                "content": {"type": "string", "description": "Text chunk to append"}
            },
            "required": ["kms", "page", "content"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let kms_name = req_str(&input, "kms")?;
        let page = req_str(&input, "page")?;
        let content = req_str(&input, "content")?;
        // dev-plan/32 Stage M: gate KMS appends inside workflow
        // subagent calls.
        crate::workflow::check_kms_write_capability(kms_name)?;
        let Some(kref) = crate::kms::resolve(kms_name) else {
            return Err(Error::Tool(format!(
                "no KMS named '{kms_name}' (check /kms list)"
            )));
        };
        let path = crate::kms::append_to_page(&kref, page, content)?;
        Ok(format!(
            "appended {} bytes to {}",
            content.len(),
            path.display()
        ))
    }
}

/// Delete a single page from a KMS. Removes the file, strips its
/// bullet from `index.md`, and appends a `deleted | <stem>` log line.
/// Used during consolidation (`/dream`) to retire duplicates or stale
/// entries — gated on approval since it's destructive.
pub struct KmsDeleteTool;

#[async_trait]
impl Tool for KmsDeleteTool {
    fn name(&self) -> &'static str {
        "KmsDelete"
    }

    fn description(&self) -> &'static str {
        "Delete a single page from an attached knowledge base. \
         Removes the file, prunes the index.md bullet, and logs the \
         removal. Use during consolidation to retire duplicates or \
         stale entries — never as a casual cleanup."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kms":  {"type": "string", "description": "KMS name (from the active list)"},
                "page": {"type": "string", "description": "Page name (with or without .md). No path separators."}
            },
            "required": ["kms", "page"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let kms_name = req_str(&input, "kms")?;
        let page = req_str(&input, "page")?;
        // dev-plan/32 Stage M: gate KMS deletes inside workflow
        // subagent calls.
        crate::workflow::check_kms_write_capability(kms_name)?;
        let Some(kref) = crate::kms::resolve(kms_name) else {
            return Err(Error::Tool(format!(
                "no KMS named '{kms_name}' (check /kms list)"
            )));
        };
        let path = crate::kms::delete_page(&kref, page)?;
        Ok(format!("deleted {}", path.display()))
    }
}

/// Ensure a named knowledge base exists at the requested scope.
/// Idempotent: returns the existing KMS if already present, otherwise
/// seeds the directory tree (`pages/`, `sources/`, `index.md`,
/// `log.md`, `SCHEMA.md`, `manifest.json`).
///
/// Primary motivation: /dream's Pass 4 writes its summary page into a
/// dedicated `dreams` KMS so audit logs do not contaminate the user's
/// real knowledge vaults. The dispatch path auto-creates `dreams`
/// before spawning the dream agent, but giving the agent the tool to
/// re-create on its own provides defense-in-depth — if the binary
/// running the dispatch is stale (no pre-create call) or the disk
/// state changed between dispatch and Pass 4, the agent can still
/// recover by calling KmsCreate itself instead of looping on
/// "no KMS named 'dreams'" errors.
///
/// Auto-approved (no Ask gate) for the same reason `SessionRename` is:
/// the operation is name-validated, idempotent, and scoped to a
/// known config directory. Worst case the user ends up with an empty
/// KMS they can delete by `rm -rf .thclaws/kms/<name>` — recoverable.
pub struct KmsCreateTool;

#[async_trait]
impl Tool for KmsCreateTool {
    fn name(&self) -> &'static str {
        "KmsCreate"
    }

    fn description(&self) -> &'static str {
        "Ensure a knowledge base exists. Idempotent: returns the existing \
         KMS if already present, otherwise seeds index.md / log.md / \
         SCHEMA.md / pages/ / sources/. Use sparingly: prefer KmsWrite \
         to an already-existing KMS. /dream's Pass 4 calls this on \
         'dreams' (scope: project) so the audit-log KMS exists before \
         the summary page is written."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name":  {"type": "string", "description": "KMS name. No path separators, no leading dot, no control chars."},
                "scope": {
                    "type": "string",
                    "enum": ["project", "user"],
                    "description": "'project' = ./.thclaws/kms/<name> (per-workspace); 'user' = ~/.config/thclaws/kms/<name> (global). /dream uses 'project'."
                }
            },
            "required": ["name", "scope"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        false
    }

    async fn call(&self, input: Value) -> Result<String> {
        let name = req_str(&input, "name")?;
        // dev-plan/32 Stage M: creating a fresh KMS inside a workflow
        // subagent call requires the new name to be in the granted
        // write list — same gate as Write/Append/Delete.
        crate::workflow::check_kms_write_capability(name)?;
        let scope_str = req_str(&input, "scope")?;
        let scope = match scope_str {
            "project" => crate::kms::KmsScope::Project,
            "user" => crate::kms::KmsScope::User,
            other => {
                return Err(Error::Tool(format!(
                    "invalid scope '{other}' — must be 'project' or 'user'"
                )))
            }
        };
        let kref = crate::kms::create(name, scope)?;
        Ok(format!(
            "ensured KMS '{}' ({}) at {}",
            kref.name,
            scope.as_str(),
            kref.root.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::{create, KmsScope};

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_home: Option<String>,
        prev_userprofile: Option<String>,
        prev_cwd: std::path::PathBuf,
        _home_dir: tempfile::TempDir,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
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

    fn scoped_home() -> EnvGuard {
        let lock = crate::kms::test_env_lock();
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

    #[tokio::test]
    async fn read_returns_page_contents() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(k.pages_dir().join("hello.md"), "hi from kms").unwrap();
        let out = KmsReadTool
            .call(json!({"kms": "nb", "page": "hello"}))
            .await
            .unwrap();
        assert_eq!(out, "hi from kms");
    }

    #[tokio::test]
    async fn read_resolves_missing_extension() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(k.pages_dir().join("x.md"), "x body").unwrap();
        let with = KmsReadTool
            .call(json!({"kms": "nb", "page": "x.md"}))
            .await
            .unwrap();
        let without = KmsReadTool
            .call(json!({"kms": "nb", "page": "x"}))
            .await
            .unwrap();
        assert_eq!(with, without);
    }

    #[tokio::test]
    async fn read_unknown_kms_errors() {
        let _home = scoped_home();
        let err = KmsReadTool
            .call(json!({"kms": "nope", "page": "x"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("no KMS"));
    }

    #[tokio::test]
    async fn search_returns_page_line_matches() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(k.pages_dir().join("a.md"), "alpha\nbeta\nhello world\n").unwrap();
        std::fs::write(k.pages_dir().join("b.md"), "nothing here\n").unwrap();
        let out = KmsSearchTool
            .call(json!({"kms": "nb", "pattern": "hello"}))
            .await
            .unwrap();
        assert_eq!(out, "a:3:hello world");
    }

    #[tokio::test]
    async fn search_returns_empty_for_no_matches() {
        let _home = scoped_home();
        create("nb", KmsScope::User).unwrap();
        let out = KmsSearchTool
            .call(json!({"kms": "nb", "pattern": "absent"}))
            .await
            .unwrap();
        assert_eq!(out, "");
    }

    // ─── dev-plan/36 Tier 2: BM25 `query:` path ────────────────────────────

    #[tokio::test]
    async fn search_rejects_both_query_and_pattern() {
        let _home = scoped_home();
        create("nb", KmsScope::User).unwrap();
        let err = KmsSearchTool
            .call(json!({"kms": "nb", "query": "x", "pattern": "y"}))
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("mutually exclusive"),
            "got: {err}",
        );
    }

    #[tokio::test]
    async fn search_rejects_neither_query_nor_pattern() {
        let _home = scoped_home();
        create("nb", KmsScope::User).unwrap();
        let err = KmsSearchTool.call(json!({"kms": "nb"})).await.unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("provide either"), "got: {s}");
    }

    /// dev-plan/36 Tier 2 + Tier 3.A: feature-on BM25 round-trip
    /// through the tool surface. Validates the auto-build-on-stale
    /// path (no manifest → full_rebuild before serving), the query
    /// path itself (page indexed via write_page hook lights up),
    /// and the human-readable result format.
    #[cfg(feature = "kms_search_index")]
    #[tokio::test]
    async fn search_query_path_returns_ranked_hits() {
        let _home = scoped_home();
        let _k = create("nb", KmsScope::User).unwrap();
        // Use the write tool to populate (exercises the
        // on_page_mutated index hook from Tier 1.D).
        KmsWriteTool
            .call(json!({
                "kms": "nb",
                "page": "auth-flow",
                "content": "---\ntitle: Refresh token rotation\ntopic: auth\n---\n\nThe token refresh rotates on every login.\n"
            }))
            .await
            .unwrap();
        KmsWriteTool
            .call(json!({
                "kms": "nb",
                "page": "unrelated",
                "content": "---\ntitle: Theming\n---\n\nDark mode colour tokens.\n"
            }))
            .await
            .unwrap();
        let out = KmsSearchTool
            .call(json!({"kms": "nb", "query": "token refresh"}))
            .await
            .unwrap();
        // auth-flow should rank above unrelated; the human-readable
        // format must include "page: auth-flow" + a score.
        assert!(
            out.contains("page: auth-flow"),
            "missing auth-flow hit: {out}"
        );
        assert!(out.contains("[score "), "missing score format: {out}");
    }

    /// dev-plan/36 follow-up: `/kms search * pattern` fans out
    /// across every visible KMS. Each KMS gets a `── KMS: <name> ──`
    /// header in the output so attribution stays clear.
    #[tokio::test]
    async fn slash_search_wildcard_fans_out_across_kmses() {
        let _home = scoped_home();
        let a = create("alpha", KmsScope::User).unwrap();
        let b = create("beta", KmsScope::User).unwrap();
        std::fs::write(a.pages_dir().join("p1.md"), "needle in alpha").unwrap();
        std::fs::write(b.pages_dir().join("p1.md"), "haystack").unwrap();
        std::fs::write(b.pages_dir().join("p2.md"), "needle in beta").unwrap();

        let out = run_slash_search("*", "needle", /* is_pattern */ true);
        assert!(
            out.contains("── KMS: alpha ──"),
            "missing alpha header: {out}"
        );
        assert!(
            out.contains("── KMS: beta ──"),
            "missing beta header: {out}"
        );
        assert!(
            out.contains("p1:1:needle in alpha"),
            "missing alpha hit: {out}"
        );
        assert!(
            out.contains("p2:1:needle in beta"),
            "missing beta hit: {out}"
        );
    }

    #[tokio::test]
    async fn slash_search_single_kms_omits_header() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::User).unwrap();
        std::fs::write(k.pages_dir().join("p.md"), "find-me here").unwrap();
        let out = run_slash_search("notes", "find-me", /* is_pattern */ true);
        assert!(
            !out.contains("── KMS:"),
            "single-KMS search should not print the multi-KMS header: {out}",
        );
        assert!(out.contains("p:1:find-me here"), "missing hit: {out}");
    }

    #[tokio::test]
    async fn slash_search_unknown_kms_returns_clear_message() {
        let _home = scoped_home();
        let out = run_slash_search("nope", "x", true);
        assert!(out.contains("no KMS named 'nope'"), "got: {out}");
    }

    /// `pattern:` output stays byte-identical to pre-Tier-2 for
    /// scripts that memoised the format. (The same fixture as
    /// `search_returns_page_line_matches` above; re-asserted here
    /// alongside `query:` to make the back-compat contract obvious
    /// in one place.)
    #[tokio::test]
    async fn pattern_path_output_unchanged() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(k.pages_dir().join("p.md"), "one\ntwo\nfind-me\n").unwrap();
        let out = KmsSearchTool
            .call(json!({"kms": "nb", "pattern": "find-me"}))
            .await
            .unwrap();
        assert_eq!(out, "p:3:find-me");
    }

    // ─── M6.25 BUG #1: write/append tools ─────────────────────────────────

    #[tokio::test]
    async fn write_tool_creates_page_with_stamps() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let result = KmsWriteTool
            .call(json!({
                "kms": "nb",
                "page": "topic",
                "content": "# Topic\n\nFresh page.\n"
            }))
            .await
            .unwrap();
        assert!(result.contains("wrote"));
        let raw = std::fs::read_to_string(k.pages_dir().join("topic.md")).unwrap();
        let (fm, body) = crate::kms::parse_frontmatter(&raw);
        assert!(fm.contains_key("created"));
        assert!(fm.contains_key("updated"));
        assert!(body.contains("Fresh page."));
    }

    #[tokio::test]
    async fn write_tool_unknown_kms_errors() {
        let _home = scoped_home();
        let err = KmsWriteTool
            .call(json!({"kms": "nope", "page": "x", "content": "y"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("no KMS"));
    }

    #[tokio::test]
    async fn write_tool_rejects_traversal() {
        let _home = scoped_home();
        create("nb", KmsScope::Project).unwrap();
        let err = KmsWriteTool
            .call(json!({
                "kms": "nb",
                "page": "../escape",
                "content": "evil"
            }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("invalid page name") || format!("{err}").contains(".."));
    }

    #[tokio::test]
    async fn append_tool_creates_then_extends() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // First call creates with bare body.
        KmsAppendTool
            .call(json!({"kms": "nb", "page": "log", "content": "line one\n"}))
            .await
            .unwrap_err(); // "log" is reserved
        KmsAppendTool
            .call(json!({"kms": "nb", "page": "journal", "content": "line one\n"}))
            .await
            .unwrap();
        KmsAppendTool
            .call(json!({"kms": "nb", "page": "journal", "content": "line two\n"}))
            .await
            .unwrap();
        let raw = std::fs::read_to_string(k.pages_dir().join("journal.md")).unwrap();
        assert!(raw.contains("line one"));
        assert!(raw.contains("line two"));
    }

    #[tokio::test]
    async fn write_and_append_require_approval() {
        let _home = scoped_home();
        // Approval defaults are read off the trait; write tools must
        // require approval (they mutate disk) — same posture as Write.
        assert!(KmsWriteTool.requires_approval(&json!({})));
        assert!(KmsAppendTool.requires_approval(&json!({})));
        assert!(KmsDeleteTool.requires_approval(&json!({})));
    }

    #[tokio::test]
    async fn delete_removes_page_and_index_bullet() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        KmsWriteTool
            .call(json!({
                "kms": "nb",
                "page": "doomed",
                "content": "to be deleted\n"
            }))
            .await
            .unwrap();
        let page_path = k.pages_dir().join("doomed.md");
        assert!(page_path.exists());
        let index_before = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(index_before.contains("(pages/doomed.md)"));
        let out = KmsDeleteTool
            .call(json!({"kms": "nb", "page": "doomed"}))
            .await
            .unwrap();
        assert!(out.starts_with("deleted"));
        assert!(!page_path.exists());
        let index_after = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(!index_after.contains("(pages/doomed.md)"));
        let log = std::fs::read_to_string(k.log_path()).unwrap();
        assert!(log.contains("deleted | doomed"));
    }

    #[tokio::test]
    async fn delete_missing_page_errors() {
        let _home = scoped_home();
        let _ = create("nb", KmsScope::Project).unwrap();
        let err = KmsDeleteTool
            .call(json!({"kms": "nb", "page": "ghost"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("not found"));
    }

    #[tokio::test]
    async fn delete_rejects_reserved_names() {
        let _home = scoped_home();
        let _ = create("nb", KmsScope::Project).unwrap();
        // Same path-safety carve-out: index/log/SCHEMA cannot be deleted
        // through the tool.
        assert!(KmsDeleteTool
            .call(json!({"kms": "nb", "page": "index"}))
            .await
            .is_err());
        assert!(KmsDeleteTool
            .call(json!({"kms": "nb", "page": "log"}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn create_tool_seeds_new_kms() {
        let _home = scoped_home();
        let out = KmsCreateTool
            .call(json!({"name": "dreams", "scope": "project"}))
            .await
            .unwrap();
        assert!(out.contains("dreams"), "got: {out}");
        // Resolve picks it up after creation — i.e. the directory exists
        // and is shaped correctly.
        let kref =
            crate::kms::resolve("dreams").expect("KmsCreate should have made dreams resolvable");
        assert!(kref.pages_dir().is_dir());
        assert!(kref.index_path().is_file());
    }

    #[tokio::test]
    async fn create_tool_is_idempotent() {
        let _home = scoped_home();
        let first = KmsCreateTool
            .call(json!({"name": "dreams", "scope": "project"}))
            .await
            .unwrap();
        // Second call must not error and must produce the same path
        // shape — the dream agent calls this on every run, so a
        // collision would defeat the purpose.
        let second = KmsCreateTool
            .call(json!({"name": "dreams", "scope": "project"}))
            .await
            .unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn create_tool_rejects_invalid_scope() {
        let _home = scoped_home();
        let err = KmsCreateTool
            .call(json!({"name": "dreams", "scope": "shared"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("invalid scope"), "got: {err}");
    }

    #[tokio::test]
    async fn create_tool_rejects_path_traversal() {
        let _home = scoped_home();
        let err = KmsCreateTool
            .call(json!({"name": "../escape", "scope": "user"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("invalid kms name"), "got: {err}");
    }

    // ── Provenance + freshness ─────────────────────────────────────

    #[test]
    fn check_provenance_flags_missing_sources_key() {
        let warning = check_provenance("---\ntitle: t\ntopic: p\n---\nbody");
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("no `sources:` frontmatter"));
    }

    #[test]
    fn check_provenance_flags_empty_sources_value() {
        // `sources:` present but blank value (model wrote the key
        // without a list) → soft warning so the model fills it.
        let warning = check_provenance("---\ntitle: t\nsources:\n---\nbody");
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("present but empty"));
    }

    #[test]
    fn check_provenance_accepts_explicit_empty_list() {
        // `sources: []` is the deliberate "opinion / convention,
        // no external source" form — explicit acknowledgement, not
        // an omission. Should NOT warn.
        let warning = check_provenance("---\ntitle: t\nsources: []\n---\nbody");
        assert!(
            warning.is_none(),
            "explicit `[]` is the acknowledged opt-out, must not warn: {warning:?}"
        );
    }

    #[test]
    fn check_provenance_accepts_filled_sources() {
        let warning =
            check_provenance("---\ntitle: t\nsources: [\"https://example.com\"]\n---\nbody");
        assert!(warning.is_none());
    }

    #[test]
    fn check_provenance_ignores_legacy_no_frontmatter_pages() {
        // Pages without any frontmatter (legacy / freeform) aren't
        // shouted at — the KmsRead staleness banner handles them
        // separately. Avoids double-warning.
        let warning = check_provenance("just body, no frontmatter");
        assert!(warning.is_none());
    }

    #[test]
    fn staleness_warning_fires_for_old_verified_date() {
        // `verified:` from years ago → date-based banner.
        let body = "---\ntitle: t\ntopic: p\nverified: 2020-01-01\n---\nbody";
        let warning = staleness_warning(body);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("days ago"));
    }

    #[test]
    fn staleness_warning_silent_for_fresh_page() {
        // `verified: <today>` → no banner. Use a date in the future
        // so this test doesn't bit-rot when today shifts.
        let body = "---\ntitle: t\ntopic: p\nverified: 2099-01-01\n---\nbody";
        assert!(staleness_warning(body).is_none());
    }

    #[test]
    fn staleness_warning_flags_missing_verified_field() {
        // Frontmatter present but no `verified:` → softer hint.
        let body = "---\ntitle: t\ntopic: p\n---\nbody";
        let warning = staleness_warning(body);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("no `verified:` frontmatter"));
    }

    #[test]
    fn staleness_warning_silent_for_no_frontmatter() {
        // Legacy page with bare body — staleness check doesn't fire
        // (the page may have been hand-written; we don't presume
        // staleness without a frontmatter contract).
        assert!(staleness_warning("just body").is_none());
    }
}
