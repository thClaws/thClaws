use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

/// One-line summary of a workflow run on disk, for `/workflow list`.
pub(crate) struct WorkflowSummary {
    pub id: String,
    pub prompt: String,
    pub status: WorkflowStatus,
    pub workers_started: u32,
    pub workers_done: u32,
    pub workers_error: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkflowStatus {
    /// Log has no terminal `done` / `error` event — either still
    /// running or the process died mid-run.
    Running,
    Done,
    Error,
}

impl WorkflowStatus {
    pub fn icon(&self) -> &'static str {
        match self {
            WorkflowStatus::Done => "✓",
            WorkflowStatus::Error => "✗",
            WorkflowStatus::Running => "⌛",
        }
    }
}

/// Enumerate every workflow under `<cwd>/.thclaws/workflows/`. Each
/// summary comes from a single forward pass over the directory's
/// `state.jsonl`. Newest-first by id (timestamp-hex, so lexicographic
/// = chronological).
pub(crate) fn list_workflows(cwd: &Path) -> std::io::Result<Vec<WorkflowSummary>> {
    let root = cwd.join(".thclaws").join("workflows");
    if !root.exists() {
        return Ok(vec![]);
    }
    let mut out: Vec<WorkflowSummary> = vec![];
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        if let Ok(summary) = read_summary(&path, &id) {
            out.push(summary);
        }
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(out)
}

fn read_summary(workflow_dir: &Path, id: &str) -> std::io::Result<WorkflowSummary> {
    let file = fs::File::open(workflow_dir.join("state.jsonl"))?;
    let reader = BufReader::new(file);
    let mut prompt = String::new();
    let mut status = WorkflowStatus::Running;
    let mut workers_started = 0u32;
    let mut workers_done = 0u32;
    let mut workers_error = 0u32;
    for line in reader.lines().map_while(Result::ok) {
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("kind").and_then(|k| k.as_str()).unwrap_or("") {
            "start" => {
                prompt = v
                    .get("prompt")
                    .and_then(|p| p.as_str())
                    .unwrap_or("")
                    .to_string();
            }
            "worker_start" => workers_started += 1,
            "worker_done" => workers_done += 1,
            "worker_error" => workers_error += 1,
            "done" => status = WorkflowStatus::Done,
            "error" => status = WorkflowStatus::Error,
            _ => {}
        }
    }
    Ok(WorkflowSummary {
        id: id.to_string(),
        prompt,
        status,
        workers_started,
        workers_done,
        workers_error,
    })
}

/// Read every event from a workflow's `state.jsonl`. Returns the raw
/// JSON values so the caller decides how to render them.
pub(crate) fn read_events(cwd: &Path, id: &str) -> std::io::Result<Vec<Value>> {
    let path = cwd
        .join(".thclaws")
        .join("workflows")
        .join(id)
        .join("state.jsonl");
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    Ok(reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect())
}

pub(crate) fn delete_workflow(cwd: &Path, id: &str) -> std::io::Result<()> {
    fs::remove_dir_all(cwd.join(".thclaws").join("workflows").join(id))
}

/// Stage K: extract the contiguous prefix of workers that completed
/// successfully (worker_done event), keyed by their numeric index.
/// A gap stops the chain — if w0 and w2 are done but w1 isn't, we
/// only replay w0 and treat the script's second call as fresh, since
/// w1 corresponds to it.
pub(crate) fn read_completed_workers(
    cwd: &Path,
    id: &str,
) -> std::io::Result<Vec<(String, String)>> {
    use std::collections::{BTreeMap, HashMap};

    let events = read_events(cwd, id)?;
    let mut starts: HashMap<String, String> = HashMap::new();
    let mut completed: BTreeMap<u32, (String, String)> = BTreeMap::new();
    for ev in events {
        let kind = ev.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        let worker = ev.get("worker").and_then(|w| w.as_str()).unwrap_or("");
        match kind {
            "worker_start" => {
                if let Some(prompt) = ev.get("prompt").and_then(|p| p.as_str()) {
                    starts.insert(worker.to_string(), prompt.to_string());
                }
            }
            "worker_done" => {
                if let Some(prompt) = starts.get(worker) {
                    let output = ev
                        .get("output")
                        .and_then(|o| o.as_str())
                        .unwrap_or("")
                        .to_string();
                    if let Ok(idx) = worker.trim_start_matches('w').parse::<u32>() {
                        completed.insert(idx, (prompt.clone(), output));
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    let mut i = 0u32;
    while let Some(pair) = completed.get(&i) {
        out.push(pair.clone());
        i += 1;
    }
    Ok(out)
}

/// Read the script.js file that was persisted at workflow start.
pub(crate) fn read_workflow_script(cwd: &Path, id: &str) -> std::io::Result<String> {
    fs::read_to_string(
        cwd.join(".thclaws")
            .join("workflows")
            .join(id)
            .join("script.js"),
    )
}

/// Persist the approved script next to state.jsonl so a later resume
/// can replay against the same source.
pub(crate) fn write_workflow_script(cwd: &Path, id: &str, script: &str) -> std::io::Result<()> {
    let dir = cwd.join(".thclaws").join("workflows").join(id);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("script.js"), script)
}

/// Resolve a user-supplied id-or-prefix to the full workflow id.
/// Exact match wins; otherwise a unique starts-with match wins.
/// Errors when there's no match or more than one starts-with hit.
pub(crate) fn resolve_id_prefix(cwd: &Path, prefix: &str) -> std::io::Result<String> {
    let root = cwd.join(".thclaws").join("workflows");
    if !root.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no .thclaws/workflows/ directory in cwd",
        ));
    }
    let mut matches: Vec<String> = vec![];
    let mut exact = None;
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == prefix {
            exact = Some(name);
            break;
        }
        if name.starts_with(prefix) {
            matches.push(name);
        }
    }
    if let Some(name) = exact {
        return Ok(name);
    }
    match matches.len() {
        0 => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no workflow matching '{prefix}'"),
        )),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("'{prefix}' matches {n} workflows — be more specific"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::state::WorkflowLogger;
    use tempfile::tempdir;

    fn run_one_workflow(cwd: &Path, id: &str, terminal: &str) {
        let mut l = WorkflowLogger::new(id.to_string(), cwd).unwrap();
        l.start("the goal", "let x = 1; x").unwrap();
        let w = l.worker_start("alpha").unwrap();
        l.worker_done(w, "alpha-out").unwrap();
        match terminal {
            "done" => l.done("final").unwrap(),
            "error" => l.error("boom").unwrap(),
            _ => {}
        }
    }

    #[test]
    fn list_empty_dir_returns_empty() {
        let tmp = tempdir().unwrap();
        let workflows = list_workflows(tmp.path()).unwrap();
        assert!(workflows.is_empty());
    }

    #[test]
    fn list_returns_newest_first() {
        let tmp = tempdir().unwrap();
        run_one_workflow(tmp.path(), "wf-aaaa", "done");
        run_one_workflow(tmp.path(), "wf-bbbb", "done");
        run_one_workflow(tmp.path(), "wf-cccc", "done");
        let workflows = list_workflows(tmp.path()).unwrap();
        let ids: Vec<&str> = workflows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, vec!["wf-cccc", "wf-bbbb", "wf-aaaa"]);
    }

    #[test]
    fn summary_status_from_terminal_event() {
        let tmp = tempdir().unwrap();
        run_one_workflow(tmp.path(), "wf-done", "done");
        run_one_workflow(tmp.path(), "wf-err", "error");
        run_one_workflow(tmp.path(), "wf-run", "none");
        let workflows = list_workflows(tmp.path()).unwrap();
        let by_id = |id: &str| {
            workflows
                .iter()
                .find(|w| w.id == id)
                .expect("workflow missing")
        };
        assert_eq!(by_id("wf-done").status, WorkflowStatus::Done);
        assert_eq!(by_id("wf-err").status, WorkflowStatus::Error);
        assert_eq!(by_id("wf-run").status, WorkflowStatus::Running);
    }

    #[test]
    fn summary_counts_workers() {
        let tmp = tempdir().unwrap();
        let mut l = WorkflowLogger::new("wf-multi".into(), tmp.path()).unwrap();
        l.start("goal", "src").unwrap();
        for prompt in ["a", "b", "c"] {
            let w = l.worker_start(prompt).unwrap();
            l.worker_done(w, prompt).unwrap();
        }
        let w = l.worker_start("d").unwrap();
        l.worker_error(w, "fail").unwrap();
        l.done("ok").unwrap();
        drop(l);

        let workflows = list_workflows(tmp.path()).unwrap();
        let s = &workflows[0];
        assert_eq!(s.workers_started, 4);
        assert_eq!(s.workers_done, 3);
        assert_eq!(s.workers_error, 1);
    }

    #[test]
    fn resolve_id_prefix_exact_match() {
        let tmp = tempdir().unwrap();
        run_one_workflow(tmp.path(), "wf-abc123", "done");
        let id = resolve_id_prefix(tmp.path(), "wf-abc123").unwrap();
        assert_eq!(id, "wf-abc123");
    }

    #[test]
    fn resolve_id_prefix_unique_starts_with() {
        let tmp = tempdir().unwrap();
        run_one_workflow(tmp.path(), "wf-abc123", "done");
        run_one_workflow(tmp.path(), "wf-xyz789", "done");
        let id = resolve_id_prefix(tmp.path(), "wf-abc").unwrap();
        assert_eq!(id, "wf-abc123");
    }

    #[test]
    fn resolve_id_prefix_ambiguous_errors() {
        let tmp = tempdir().unwrap();
        run_one_workflow(tmp.path(), "wf-abc111", "done");
        run_one_workflow(tmp.path(), "wf-abc222", "done");
        let err = resolve_id_prefix(tmp.path(), "wf-abc").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(err.to_string().contains("matches 2 workflows"));
    }

    #[test]
    fn resolve_id_prefix_no_match_errors() {
        let tmp = tempdir().unwrap();
        run_one_workflow(tmp.path(), "wf-abc", "done");
        let err = resolve_id_prefix(tmp.path(), "wf-zzz").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn read_events_returns_all_lines() {
        let tmp = tempdir().unwrap();
        run_one_workflow(tmp.path(), "wf-read", "done");
        let events = read_events(tmp.path(), "wf-read").unwrap();
        assert_eq!(events.len(), 4); // start + worker_start + worker_done + done
        assert_eq!(events[0]["kind"], "start");
        assert_eq!(events[3]["kind"], "done");
    }

    #[test]
    fn delete_workflow_removes_dir() {
        let tmp = tempdir().unwrap();
        run_one_workflow(tmp.path(), "wf-rm-me", "done");
        let dir = tmp
            .path()
            .join(".thclaws")
            .join("workflows")
            .join("wf-rm-me");
        assert!(dir.exists());
        delete_workflow(tmp.path(), "wf-rm-me").unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn read_completed_workers_returns_contiguous_prefix() {
        let tmp = tempdir().unwrap();
        let id = "wf-cache".to_string();
        let mut l = WorkflowLogger::new(id.clone(), tmp.path()).unwrap();
        l.start("goal", "src").unwrap();
        let w0 = l.worker_start("alpha").unwrap();
        l.worker_done(w0, "A").unwrap();
        let w1 = l.worker_start("beta").unwrap();
        l.worker_done(w1, "B").unwrap();
        let _w2 = l.worker_start("gamma").unwrap();
        drop(l);

        let cache = read_completed_workers(tmp.path(), &id).unwrap();
        assert_eq!(
            cache,
            vec![("alpha".into(), "A".into()), ("beta".into(), "B".into())]
        );
    }

    #[test]
    fn read_completed_workers_stops_at_gap() {
        let tmp = tempdir().unwrap();
        let id = "wf-gap".to_string();
        let mut l = WorkflowLogger::new(id.clone(), tmp.path()).unwrap();
        l.start("goal", "src").unwrap();
        let w0 = l.worker_start("alpha").unwrap();
        l.worker_done(w0, "A").unwrap();
        let w1 = l.worker_start("beta").unwrap();
        l.worker_error(w1, "boom").unwrap();
        let w2 = l.worker_start("gamma").unwrap();
        l.worker_done(w2, "G").unwrap();
        drop(l);

        let cache = read_completed_workers(tmp.path(), &id).unwrap();
        assert_eq!(cache, vec![("alpha".into(), "A".into())]);
    }

    #[test]
    fn read_completed_workers_empty_when_nothing_finished() {
        let tmp = tempdir().unwrap();
        let id = "wf-none".to_string();
        let mut l = WorkflowLogger::new(id.clone(), tmp.path()).unwrap();
        l.start("g", "s").unwrap();
        let _ = l.worker_start("a").unwrap();
        drop(l);
        let cache = read_completed_workers(tmp.path(), &id).unwrap();
        assert!(cache.is_empty());
    }

    #[test]
    fn script_round_trip() {
        let tmp = tempdir().unwrap();
        let id = "wf-script";
        let body = "const x = 1;\nx;";
        write_workflow_script(tmp.path(), id, body).unwrap();
        let read_back = read_workflow_script(tmp.path(), id).unwrap();
        assert_eq!(read_back, body);
    }
}
