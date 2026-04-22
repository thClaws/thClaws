use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use globset::Glob;
use ignore::WalkBuilder;
use serde_json::{json, Value};

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "Glob"
    }

    fn description(&self) -> &'static str {
        "Match files against a specific shell glob pattern (e.g. `src/**/*.rs`). \
         Use this only when you already know a pattern you want to match; for \
         general directory listing use `Ls` instead. Respects `.gitignore` \
         inside git repositories. Returns absolute paths, one per line, sorted."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob like '**/*.rs' or 'src/main.*'"},
                "path":    {"type": "string", "description": "Base directory for relative patterns"}
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let pattern = req_str(&input, "pattern")?;
        let raw_base = input.get("path").and_then(Value::as_str).unwrap_or(".");
        let base_path = crate::sandbox::Sandbox::check(raw_base)?;

        let matcher = Glob::new(pattern)
            .map_err(|e| Error::Tool(format!("glob syntax: {e}")))?
            .compile_matcher();
        let mut paths: Vec<String> = Vec::new();
        for entry in WalkBuilder::new(&base_path).build().flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            let rel = path.strip_prefix(&base_path).unwrap_or(path);
            if matcher.is_match(rel) {
                paths.push(path.to_string_lossy().into_owned());
            }
        }
        paths.sort();
        Ok(paths.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn setup_tree() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/util")).unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/util/helper.rs"), "").unwrap();
        std::fs::write(dir.path().join("tests/integration.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        dir
    }

    #[tokio::test]
    async fn matches_recursive_rust_files() {
        let dir = setup_tree();
        let out = GlobTool
            .call(json!({
                "pattern": "**/*.rs",
                "path": dir.path().to_string_lossy(),
            }))
            .await
            .unwrap();
        let normalized = out.replace('\\', "/");
        let lines: Vec<&str> = normalized.lines().collect();
        assert_eq!(lines.len(), 4, "expected 4 .rs files, got: {out}");
        assert!(lines.iter().any(|l| l.ends_with("src/main.rs")));
        assert!(lines.iter().any(|l| l.ends_with("src/lib.rs")));
        assert!(lines.iter().any(|l| l.ends_with("src/util/helper.rs")));
        assert!(lines.iter().any(|l| l.ends_with("tests/integration.rs")));
    }

    #[tokio::test]
    async fn matches_specific_pattern() {
        let dir = setup_tree();
        let out = GlobTool
            .call(json!({
                "pattern": "src/main.rs",
                "path": dir.path().to_string_lossy(),
            }))
            .await
            .unwrap();
        let normalized = out.replace('\\', "/");
        assert!(normalized.ends_with("src/main.rs"), "got: {out}");
    }

    #[tokio::test]
    async fn empty_result_for_no_matches() {
        let dir = setup_tree();
        let out = GlobTool
            .call(json!({
                "pattern": "**/*.nonsense",
                "path": dir.path().to_string_lossy(),
            }))
            .await
            .unwrap();
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn results_are_sorted() {
        let dir = setup_tree();
        let out = GlobTool
            .call(json!({
                "pattern": "**/*.rs",
                "path": dir.path().to_string_lossy(),
            }))
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        let mut sorted = lines.clone();
        sorted.sort();
        assert_eq!(lines, sorted);
    }

    #[tokio::test]
    async fn missing_pattern_errors() {
        let err = GlobTool.call(json!({})).await.unwrap_err();
        assert!(format!("{err}").contains("pattern"));
    }

    #[tokio::test]
    async fn respects_gitignore() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("visible.rs"), "").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "").unwrap();

        let out = GlobTool
            .call(json!({
                "pattern": "**/*.rs",
                "path": dir.path().to_string_lossy(),
            }))
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines.len(),
            1,
            "expected gitignore to hide ignored.rs, got: {out}"
        );
        assert!(lines[0].ends_with("visible.rs"));
    }
}
