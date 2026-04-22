//! KMS read and search tools — pair with [`crate::kms`] to let the
//! model pull in wiki pages and grep across them without embeddings.
//!
//! Both tools resolve the `kms` argument via `kms::resolve`, which
//! prefers a project-scope KMS over a user-scope one on name collision.

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
        std::fs::read_to_string(&path)
            .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))
    }
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
}
