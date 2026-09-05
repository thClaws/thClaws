//! JSONL file sink. The path may carry strftime tokens (`%Y-%m-%d`) so
//! rotation is by naming — no rotator process, no file handle held open
//! across a date boundary.

use super::sink::AuditSink;
use std::io::Write;
use std::path::PathBuf;

pub struct FileSink {
    template: String,
}

impl FileSink {
    pub fn new(template: String) -> Self {
        Self { template }
    }

    pub fn default_path() -> String {
        let data = crate::util::home_dir()
            .map(|h| h.join(".local/share/thclaws/audit"))
            .unwrap_or_else(|| PathBuf::from("audit"));
        format!("{}/%Y-%m-%d.jsonl", data.display())
    }

    pub fn resolved_path(&self) -> PathBuf {
        let expanded = match self.template.strip_prefix("~/") {
            Some(rest) => crate::util::home_dir()
                .map(|h| h.join(rest).to_string_lossy().into_owned())
                .unwrap_or_else(|| self.template.clone()),
            None => self.template.clone(),
        };
        let rendered = if expanded.contains('%') {
            chrono::Utc::now().format(&expanded).to_string()
        } else {
            expanded
        };
        PathBuf::from(rendered)
    }
}

impl AuditSink for FileSink {
    fn name(&self) -> &'static str {
        "file"
    }

    fn write(&self, line: &str) -> Result<(), String> {
        let path = self.resolved_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        f.write_all(line.as_bytes())
            .and_then(|_| f.write_all(b"\n"))
            .map_err(|e| format!("write {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strftime_path_renders_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let tpl = format!("{}/%Y/audit-%Y-%m-%d.jsonl", dir.path().display());
        let sink = FileSink::new(tpl);
        sink.write(r#"{"v":1}"#).unwrap();
        sink.write(r#"{"v":1}"#).unwrap();
        let p = sink.resolved_path();
        assert!(!p.to_string_lossy().contains('%'));
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), 2);
    }

    #[test]
    fn unwritable_path_is_an_error_not_a_panic() {
        let sink = FileSink::new("/dev/null/nope/%Y.jsonl".into());
        assert!(sink.write("{}").is_err());
    }
}
