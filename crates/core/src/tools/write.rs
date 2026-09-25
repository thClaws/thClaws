use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

/// Write `content` to `path`, refusing to follow a symlink at the final
/// component.
///
/// The backstop is [PR #222](https://github.com/thClaws/thClaws/pull/222) by
/// @mikemikimike: `Sandbox::check_write` resolves and validates the
/// destination, but between that check and `open(2)` the last component can
/// be replaced with a link pointing anywhere. `O_NOFOLLOW` closes that window
/// in the kernel, where no amount of checking can be raced.
///
/// **`path` must already be the RESOLVED landing**, not the path the caller
/// typed. `O_NOFOLLOW` refuses every final symlink, legitimate ones included
/// — `CLAUDE.md -> AGENTS.md` is a pattern this product actively supports —
/// so applying it to the typed path turns a normal write into
/// `Too many levels of symbolic links (os error 62)`. Measured: the PR as
/// submitted failed `writes_through_a_symlink_that_stays_inside`. Resolving
/// first and opening the landing keeps both properties: links the sandbox
/// already approved still work, and a link swapped in AFTER the check is
/// refused by the kernel.
fn write_no_follow(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)?.write_all(content.as_bytes())
}

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
        // `validated` is already the sandbox-approved landing path.
        // Opening it directly preserves approved symlinks while keeping
        // O_NOFOLLOW as the check/open race backstop.
        write_no_follow(&validated, content)
            .map_err(|e| Error::Tool(format!("write {path}: {e}")))?;
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

    /// A symlink that stays INSIDE the sandbox is a legitimate path — the
    /// `CLAUDE.md -> AGENTS.md` pattern is common — and the sandbox resolves
    /// it and allows the landing. Writing through it must keep working.
    #[cfg(unix)]
    #[tokio::test]
    async fn writes_through_a_symlink_that_stays_inside() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("AGENTS.md");
        std::fs::write(&real, "old").unwrap();
        let link = dir.path().join("CLAUDE.md");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        WriteTool
            .call(json!({ "path": link.to_string_lossy(), "content": "new" }))
            .await
            .expect("an in-sandbox symlink is a legitimate write target");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "and the link must still be a link, not replaced by a file"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn writes_through_an_inside_dangling_symlink() {
        let dir = tempdir().unwrap();
        let output = dir.path().join("output");
        std::fs::create_dir(&output).unwrap();
        let link = output.join("nested.txt");
        let landing = output.join("nested").join("file.txt");
        std::os::unix::fs::symlink(&landing, &link).unwrap();

        WriteTool
            .call(json!({ "path": link.to_string_lossy(), "content": "new" }))
            .await
            .expect("an approved inside dangling symlink is a legitimate write target");
        assert_eq!(std::fs::read_to_string(&landing).unwrap(), "new");
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),);
    }

    /// PR #222's case: a link swapped in after the check must not be
    /// followed. Asserted on the filesystem primitive, because the race it
    /// closes cannot be staged from the tool's own call path.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_swapped_in_after_the_check_is_refused_by_the_kernel() {
        let dir = tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let link = dir.path().join("artifact.txt");
        std::os::unix::fs::symlink(outside.join("artifact.txt"), &link).unwrap();

        let err = write_no_follow(&link, "must not follow").unwrap_err();
        let message = format!("{err}");
        assert!(
            message.contains("symbolic link") || message.contains("Too many levels"),
            "unexpected error: {message}"
        );
        assert!(
            !outside.join("artifact.txt").exists(),
            "nothing may have been written through the link"
        );
    }

    // The tool-level half of this — a dangling link whose landing is outside
    // the workspace — is covered by `sandbox::tests::
    // a_dangling_symlink_cannot_carry_a_write_out_of_the_sandbox`, which can
    // scope a workspace without a process-global fixture.

    #[tokio::test]
    async fn missing_content_errors() {
        let err = WriteTool
            .call(json!({"path": "/tmp/noop"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("content"));
    }
}
