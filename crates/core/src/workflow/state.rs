use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Append-only JSONL log for one workflow run. Each event is one line,
/// flushed after the write so a Ctrl-C or crash leaves the file in a
/// recoverable shape (no half-written records). Mirrors the
/// `.thclaws/sessions/<id>.jsonl` convention; per the
/// [JSONL-stays-user-readable rule] the format is plain `cat`-friendly.
pub(crate) struct WorkflowLogger {
    id: String,
    writer: BufWriter<File>,
    next_worker: u32,
}

impl WorkflowLogger {
    /// Open `<cwd>/.thclaws/workflows/<id>/state.jsonl` for append,
    /// creating the parent directory if needed.
    pub fn new(id: String, cwd: &Path) -> std::io::Result<Self> {
        let dir = Self::dir(cwd, &id);
        fs::create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("state.jsonl"))?;
        Ok(Self {
            id,
            writer: BufWriter::new(file),
            next_worker: 0,
        })
    }

    pub fn dir(cwd: &Path, id: &str) -> PathBuf {
        cwd.join(".thclaws").join("workflows").join(id)
    }

    /// Number of workers spawned during this run — read by the REPL
    /// after `spawn_blocking` to print the closing summary line.
    pub fn worker_count(&self) -> u32 {
        self.next_worker
    }

    /// Stage K: seed the worker counter when opening a logger for a
    /// resumed run, so new worker_start events continue the numbering
    /// past the workers that already exist in state.jsonl.
    pub fn set_next_worker_id(&mut self, next: u32) {
        self.next_worker = next;
    }

    pub fn start(&mut self, prompt: &str, script: &str) -> std::io::Result<()> {
        self.write_event(json!({
            "ts": now_iso(),
            "kind": "start",
            "id": &self.id,
            "prompt": prompt,
            "script_sha": script_sha(script),
            "script_chars": script.chars().count(),
        }))
    }

    /// Record the start of a worker subagent call. Returns an
    /// auto-incrementing worker id the caller threads through to
    /// `worker_done` / `worker_error`.
    pub fn worker_start(&mut self, prompt: &str) -> std::io::Result<u32> {
        let worker_id = self.next_worker;
        self.next_worker += 1;
        self.write_event(json!({
            "ts": now_iso(),
            "kind": "worker_start",
            "id": &self.id,
            "worker": format!("w{worker_id}"),
            "prompt": prompt,
        }))?;
        Ok(worker_id)
    }

    pub fn worker_done(&mut self, worker_id: u32, output: &str) -> std::io::Result<()> {
        self.write_event(json!({
            "ts": now_iso(),
            "kind": "worker_done",
            "id": &self.id,
            "worker": format!("w{worker_id}"),
            "output": output,
        }))
    }

    pub fn worker_error(&mut self, worker_id: u32, err: &str) -> std::io::Result<()> {
        self.write_event(json!({
            "ts": now_iso(),
            "kind": "worker_error",
            "id": &self.id,
            "worker": format!("w{worker_id}"),
            "error": err,
        }))
    }

    /// Stage H: a worker attempt failed (hard error or schema
    /// violation) and the runtime is about to retry. The terminal
    /// `worker_done` / `worker_error` event still fires once the
    /// retries settle.
    pub fn worker_retry(
        &mut self,
        worker_id: u32,
        attempt: u32,
        prior_error: &str,
    ) -> std::io::Result<()> {
        self.write_event(json!({
            "ts": now_iso(),
            "kind": "worker_retry",
            "id": &self.id,
            "worker": format!("w{worker_id}"),
            "attempt": attempt,
            "prior_error": prior_error,
        }))
    }

    /// Stage M: script granted the worker explicit KMS write access.
    /// Only emitted when the grant is non-empty so default workflows
    /// (no grants, KMS writes denied) stay quiet in the log.
    pub fn worker_caps(&mut self, worker_id: u32, kms_write: &[String]) -> std::io::Result<()> {
        self.write_event(json!({
            "ts": now_iso(),
            "kind": "worker_caps",
            "id": &self.id,
            "worker": format!("w{worker_id}"),
            "kms_write": kms_write,
        }))
    }

    pub fn done(&mut self, result: &str) -> std::io::Result<()> {
        self.write_event(json!({
            "ts": now_iso(),
            "kind": "done",
            "id": &self.id,
            "result": result,
        }))
    }

    pub fn error(&mut self, err: &str) -> std::io::Result<()> {
        self.write_event(json!({
            "ts": now_iso(),
            "kind": "error",
            "id": &self.id,
            "error": err,
        }))
    }

    fn write_event(&mut self, ev: Value) -> std::io::Result<()> {
        writeln!(self.writer, "{ev}")?;
        self.writer.flush()
    }
}

/// Nanosecond-hex workflow id — naturally unique and chronologically
/// sortable. Matches the `sess-{nanos:x}` convention from session.rs.
pub(crate) fn generate_workflow_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("wf-{nanos:x}")
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn script_sha(script: &str) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    script.hash(&mut h);
    format!("{:x}", h.finish())
}

pub(crate) type LoggerHandle = Arc<Mutex<WorkflowLogger>>;

thread_local! {
    /// Set by the REPL workflow handler immediately before invoking
    /// `WorkflowSandbox::run` (inside `spawn_blocking`). Host-side
    /// `thclaws.subagent` retrieves it to wrap each worker call with
    /// `worker_start` / `worker_done` events.
    static WORKFLOW_LOGGER: RefCell<Option<LoggerHandle>> = const { RefCell::new(None) };
}

pub(crate) fn set_logger(logger: Option<LoggerHandle>) {
    WORKFLOW_LOGGER.with(|c| *c.borrow_mut() = logger);
}

/// Invoke `f` with a mutable borrow of the thread-local logger if one
/// is set. Returns `None` when no logger is active (the sandbox is
/// being used outside a workflow run, e.g. in unit tests).
pub(crate) fn with_logger<R>(f: impl FnOnce(&mut WorkflowLogger) -> R) -> Option<R> {
    WORKFLOW_LOGGER.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|l| l.lock().ok().map(|mut g| f(&mut g)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn workflow_id_unique_and_timestamp_ordered() {
        let a = generate_workflow_id();
        std::thread::sleep(std::time::Duration::from_nanos(1));
        let b = generate_workflow_id();
        assert!(a.starts_with("wf-"));
        assert!(b.starts_with("wf-"));
        assert_ne!(a, b);
        assert!(b > a, "ids should be timestamp-ordered: a={a} b={b}");
    }

    #[test]
    fn writes_lifecycle_events_to_state_jsonl() {
        let tmp = tempdir().unwrap();
        let id = "wf-test-1".to_string();
        let mut logger = WorkflowLogger::new(id.clone(), tmp.path()).unwrap();

        logger.start("the goal", "let x = 1; x").unwrap();
        let w = logger.worker_start("alpha").unwrap();
        logger.worker_done(w, "task[alpha]").unwrap();
        logger.done("task[alpha]").unwrap();
        drop(logger);

        let path = tmp
            .path()
            .join(".thclaws")
            .join("workflows")
            .join(&id)
            .join("state.jsonl");
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 4, "got: {body}");

        let kinds: Vec<String> = lines
            .iter()
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                v["kind"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(kinds, vec!["start", "worker_start", "worker_done", "done"]);

        for line in &lines {
            let v: Value = serde_json::from_str(line).unwrap();
            assert!(v.get("ts").is_some(), "missing ts: {line}");
            assert!(v.get("id").is_some(), "missing id: {line}");
        }
    }

    #[test]
    fn worker_ids_are_distinct_and_threaded_through() {
        let tmp = tempdir().unwrap();
        let id = "wf-ids".to_string();
        let mut logger = WorkflowLogger::new(id.clone(), tmp.path()).unwrap();

        let w0 = logger.worker_start("a").unwrap();
        let w1 = logger.worker_start("b").unwrap();
        let w2 = logger.worker_start("c").unwrap();
        logger.worker_done(w1, "B").unwrap();
        logger.worker_done(w0, "A").unwrap();
        logger.worker_done(w2, "C").unwrap();
        drop(logger);

        let path = tmp
            .path()
            .join(".thclaws")
            .join("workflows")
            .join(&id)
            .join("state.jsonl");
        let body = std::fs::read_to_string(&path).unwrap();
        let workers: Vec<String> = body
            .lines()
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                v["worker"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(workers, vec!["w0", "w1", "w2", "w1", "w0", "w2"]);
    }

    #[test]
    fn flush_after_each_event_makes_log_readable_without_drop() {
        let tmp = tempdir().unwrap();
        let id = "wf-flush".to_string();
        let mut logger = WorkflowLogger::new(id.clone(), tmp.path()).unwrap();
        logger.start("p", "s").unwrap();
        let _w = logger.worker_start("x").unwrap();
        let path = tmp
            .path()
            .join(".thclaws")
            .join("workflows")
            .join(&id)
            .join("state.jsonl");
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(body.ends_with('\n'));
        assert!(lines[0].contains("\"start\""));
        assert!(lines[1].contains("\"worker_start\""));
    }
}
