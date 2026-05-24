use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "Edit"
    }

    fn description(&self) -> &'static str {
        "Replace exactly one occurrence of `old_string` with `new_string` in a file. \
         Errors if `old_string` is not found or appears more than once, unless \
         `replace_all: true`."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":        {"type": "string"},
                "old_string":  {"type": "string"},
                "new_string":  {"type": "string"},
                "replace_all": {"type": "boolean", "default": false}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_path = req_str(&input, "path")?;
        let path = crate::sandbox::Sandbox::check_write(raw_path)?;
        // Same rationale as Write: lead is a coordinator, never the author.
        // Narrow exception for active merge-conflict resolution — see
        // write.rs for the full reasoning.
        if crate::team::is_team_lead() && !crate::team::lead_resolving_merge_conflict(&path) {
            return Err(Error::Tool(format!(
                "team lead may not edit source files (path: {raw_path}). Lead is a COORDINATOR — delegate every code change to the responsible teammate via SendMessage. (Exception: when a git merge is in progress and this file has `<<<<<<<` markers, you may edit the conflict markers out. That doesn't apply here.)"
            )));
        }
        let old = req_str(&input, "old_string")?;
        let new = req_str(&input, "new_string")?;
        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // An empty old_string matches between every character, so with
        // replace_all it would inject new_string throughout the file and
        // corrupt it. Reject it, matching docx_edit's find_replace guard.
        if old.is_empty() {
            return Err(Error::Tool("old_string must not be empty".into()));
        }

        if old == new {
            return Err(Error::Tool(
                "old_string and new_string are identical".into(),
            ));
        }

        let contents = std::fs::read_to_string(&path)
            .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?;

        let count = contents.matches(old).count();
        if count == 0 {
            return Err(Error::Tool(format!(
                "old_string not found in {}",
                path.display()
            )));
        }
        if !replace_all && count > 1 {
            return Err(Error::Tool(format!(
                "old_string appears {count} times in {}; use replace_all or add more context",
                path.display()
            )));
        }

        let updated = if replace_all {
            contents.replace(old, new)
        } else {
            contents.replacen(old, new, 1)
        };

        std::fs::write(&path, &updated)
            .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;

        let replaced = if replace_all { count } else { 1 };
        Ok(format!(
            "Replaced {replaced} occurrence(s) in {}",
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn edit(path: &std::path::Path, old: &str, new: &str) -> Result<String> {
        EditTool
            .call(json!({
                "path": path.to_string_lossy(),
                "old_string": old,
                "new_string": new,
            }))
            .await
    }

    #[tokio::test]
    async fn edits_single_occurrence() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "hello world").unwrap();

        let msg = edit(&path, "world", "rust").await.unwrap();
        assert!(msg.contains("Replaced 1"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello rust");
    }

    #[tokio::test]
    async fn refuses_multiple_without_replace_all() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a a a").unwrap();
        let err = edit(&path, "a", "b").await.unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("3 times"));
        // file untouched
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a a a");
    }

    #[tokio::test]
    async fn replace_all_replaces_everything() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a a a").unwrap();
        let msg = EditTool
            .call(json!({
                "path": path.to_string_lossy(),
                "old_string": "a",
                "new_string": "b",
                "replace_all": true
            }))
            .await
            .unwrap();
        assert!(msg.contains("Replaced 3"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "b b b");
    }

    #[tokio::test]
    async fn missing_old_string_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "hello").unwrap();
        let err = edit(&path, "nope", "x").await.unwrap_err();
        assert!(format!("{err}").contains("not found"));
    }

    #[tokio::test]
    async fn identical_old_and_new_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "x").unwrap();
        let err = edit(&path, "x", "x").await.unwrap_err();
        assert!(format!("{err}").contains("identical"));
    }

    #[tokio::test]
    async fn empty_old_string_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "abc").unwrap();
        // Even with replace_all, an empty old_string must be rejected
        // rather than injecting new_string between every character.
        let err = EditTool
            .call(json!({
                "path": path.to_string_lossy(),
                "old_string": "",
                "new_string": "X",
                "replace_all": true
            }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("must not be empty"));
        // file untouched
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "abc");
    }
}
