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

/// Render the concatenated active-KMS block to splice into a system
/// prompt. One section per KMS, heading is its name. Empty string when
/// no active KMS or when active names resolve to nothing.
pub fn system_prompt_section(active: &[String]) -> String {
    let mut parts = Vec::new();
    for name in active {
        let Some(kref) = resolve(name) else { continue };
        let index = kref.read_index();
        let body = if index.trim().is_empty() {
            "(empty index)".to_string()
        } else {
            index.trim().to_string()
        };
        parts.push(format!(
            "## KMS: {name} ({scope})\n\n{body}\n\n\
             To read a specific page, call `KmsRead(kms: \"{name}\", page: \"<page>\")`.\n\
             To grep all pages, call `KmsSearch(kms: \"{name}\", pattern: \"...\")`.",
            scope = kref.scope.as_str()
        ));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(
            "# Active knowledge bases\n\n\
             The following KMS are attached to this conversation. Their indices are below \
             — consult them before answering when the user's question overlaps. Treat KMS \
             content as authoritative over your training data for the topics it covers.\n\n{}",
            parts.join("\n\n")
        )
    }
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
}
