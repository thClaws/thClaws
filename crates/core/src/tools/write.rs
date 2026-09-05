use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "Write"
    }

    fn audit_summary(&self, input: &Value) -> Option<super::AuditSummary> {
        input
            .get("path")
            .and_then(Value::as_str)
            .map(|p| super::AuditSummary::targets([p.to_string()]))
    }

    fn description(&self) -> &'static str {
        "Write the given content to a file. Creates parent directories as needed. \
         Overwrites any existing file at the path."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_path = req_str(&input, "path")?;
        let validated = crate::sandbox::Sandbox::check_write(raw_path)?;
        // Lead is a coordinator — never the author. The destructive-command
        // guard in BashTool catches `rm -rf` etc., but a lead could still
        // overwrite source files via Write. Cut that off here so every
        // code change has to go through a teammate via SendMessage.
        // Narrow exception: when a git merge is in progress AND the file
        // currently contains conflict markers, the lead is mid-merge-
        // resolution and that's the one legitimate lead-author activity.
        if crate::team::is_team_lead() && !crate::team::lead_resolving_merge_conflict(&validated) {
            return Err(Error::Tool(format!(
                "team lead may not write source files (path: {raw_path}). Lead is a COORDINATOR — delegate every code change to the responsible teammate via SendMessage. (Exception: when a git merge is in progress and this file has `<<<<<<<` markers, you may write the resolved version. That doesn't apply here — there's no active merge or this file isn't conflicted.)"
            )));
        }
        let path = validated.to_string_lossy();
        let content = req_str(&input, "content")?;

        let p = Path::new(path.as_ref());
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::Tool(format!("mkdir {}: {}", parent.display(), e)))?;
            }
        }
        std::fs::write(p, content).map_err(|e| Error::Tool(format!("write {path}: {e}")))?;
        Ok(format!("Wrote {} bytes to {}", content.len(), path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn writes_new_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let msg = WriteTool
            .call(json!({
                "path": path.to_string_lossy(),
                "content": "hello"
            }))
            .await
            .unwrap();
        assert!(msg.contains("Wrote 5 bytes"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ow.txt");
        std::fs::write(&path, "old").unwrap();

        WriteTool
            .call(json!({
                "path": path.to_string_lossy(),
                "content": "new"
            }))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[tokio::test]
    async fn creates_parent_directories() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a/b/c/nested.txt");
        WriteTool
            .call(json!({
                "path": path.to_string_lossy(),
                "content": "x"
            }))
            .await
            .unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn missing_content_errors() {
        let err = WriteTool
            .call(json!({"path": "/tmp/noop"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("content"));
    }
}
