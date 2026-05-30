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
        "Grep across all pages in one knowledge base. Returns matching \
         lines as `page:line:text`. Use to locate a fact before reading \
         a whole page."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kms":     {"type": "string", "description": "KMS name"},
                "pattern": {"type": "string", "description": "Regex pattern"}
            },
            "required": ["kms", "pattern"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let kms_name = req_str(&input, "kms")?;
        let pattern = req_str(&input, "pattern")?;
        let Some(kref) = crate::kms::resolve(kms_name) else {
            return Err(Error::Tool(format!(
                "no KMS named '{kms_name}' (check /kms list)"
            )));
        };
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
