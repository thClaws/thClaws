//! Schedule store + run primitive (Step 1 of the scheduling feature).
//!
//! User-level store at `~/.config/thclaws/schedules.json` lists named
//! recurring jobs. Each job carries its own working directory, prompt,
//! optional model/iteration overrides, and a standard 5-field cron
//! expression. `run_once` fires a single job synchronously by spawning
//! `thclaws --print "<prompt>"` with `current_dir(<cwd>)`, capturing
//! stdout+stderr to a per-run log file under
//! `~/.local/share/thclaws/logs/<id>/<ts>.log`, and recording the exit
//! code + duration back into the schedule entry's `last_run` /
//! `last_exit` fields.
//!
//! Step 1 is intentionally **without an in-process scheduler** — the
//! `run` subcommand fires a single job by id, so users can wire it
//! into crontab or launchd themselves and start getting value today.
//! The in-process tick (Step 2) and daemon (Step 3) reuse `run_once`
//! verbatim — only the trigger changes.

use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const STORE_VERSION: u32 = 1;

/// One scheduled job.
///
/// The on-disk shape is camelCase; `Option::None` fields are stripped
/// from output via `skip_serializing_if` so a freshly added schedule
/// reads as a small, tidy object rather than a wall of `null`s.
///
/// `Default` is derived so test fixtures + future literals can use
/// `..Default::default()` instead of repeating every field. The
/// defaults aren't a meaningful schedule on their own (empty id /
/// cron / prompt) — production callers always set the required
/// fields explicitly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    /// Stable user-chosen id. Used as the directory name for run logs
    /// and as the lookup key for `schedule run / rm / show`.
    pub id: String,

    /// Standard 5-field POSIX cron expression. Validated on add (the
    /// `cron` crate parses it; an error message names the offending
    /// field). Stored as-is so users see exactly what they typed when
    /// they `show` the entry.
    pub cron: String,

    /// Absolute working directory the spawned `--print` job runs in.
    /// This is what determines which `.thclaws/settings.json`,
    /// sandbox root, memory directory, and project-level MCP config
    /// the job picks up — same as if the user had `cd`'d here and
    /// run `thclaws --print` manually.
    pub cwd: PathBuf,

    /// The prompt handed to `thclaws --print`. Multi-line is fine;
    /// the spawn passes it through as one positional argument.
    pub prompt: String,

    /// Override the model for this job. `None` means "use whatever
    /// the cwd's settings.json picks." Kept as the alias string the
    /// user typed (e.g. `gpt-4o`, `claude-sonnet-4-6`) so the spawned
    /// process resolves it through the same alias path the CLI uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Cap on the agent loop's tool-call iterations for this job.
    /// `None` falls through to the project's `maxIterations` setting
    /// (which itself defaults to 200 in agent.rs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<usize>,

    /// Hard timeout for the spawned subprocess in seconds. `None`
    /// means no timeout — the job runs until the agent loop
    /// terminates naturally. Defaults to 600 (10 min) on add to
    /// avoid runaway jobs swallowing API quota.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,

    /// Disabled jobs stay in the store but the in-process scheduler
    /// (Step 2) and `schedule run` both refuse to fire them. Manual
    /// `--force` override comes later if anyone asks for it.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// When true, the daemon also watches the schedule's `cwd`
    /// recursively for filesystem changes and fires the job on
    /// (debounced) events — in addition to any cron schedule. Same
    /// boundary as the project sandbox: the watch never extends
    /// outside the schedule's workspace. The in-process scheduler
    /// (Step 2) ignores this flag — only the daemon (Step 3) wires
    /// up filesystem watchers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub watch_workspace: bool,

    /// RFC 3339 timestamp of the most recent fire (success or failure).
    /// Set by `run_once`; absent until the first run completes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<String>,

    /// Exit code from the most recent fire. `None` means the run
    /// timed out or failed before producing an exit status (e.g.
    /// the binary couldn't be located).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<i32>,
}

fn default_true() -> bool {
    true
}

/// Top-level on-disk shape of the schedule store. Versioned so
/// later migrations have a hook; bumped through `migrate_to_current`
/// when we add fields that need rewriting (none yet — every field
/// added so far has been Optional and serde-default-friendly).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleStore {
    pub version: u32,
    #[serde(default)]
    pub schedules: Vec<Schedule>,
}

impl Default for ScheduleStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            schedules: Vec::new(),
        }
    }
}

impl ScheduleStore {
    /// Default user-level path: `~/.config/thclaws/schedules.json`.
    /// Returns `None` only on a broken Windows environment with no
    /// usable home dir.
    pub fn default_path() -> Option<PathBuf> {
        crate::util::home_dir().map(|h| h.join(".config/thclaws/schedules.json"))
    }

    /// Load from the default user-level path. Returns a fresh empty
    /// store if the file doesn't exist yet.
    pub fn load() -> Result<Self> {
        match Self::default_path() {
            Some(p) => Self::load_from(&p),
            None => Ok(Self::default()),
        }
    }

    /// Load from a specific path. Used by tests to redirect to a
    /// tempdir, and by callers that want to swap the store location
    /// (e.g. a future per-project overlay).
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(path)?;
        let store: ScheduleStore = serde_json::from_str(&body)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        Ok(store)
    }

    /// Save to the default user-level path. Errors if no home dir
    /// is resolvable.
    pub fn save(&self) -> Result<()> {
        let path = Self::default_path().ok_or_else(|| {
            Error::Config("no home directory found — cannot locate schedule store".into())
        })?;
        self.save_to(&path)
    }

    /// Save to a specific path, creating the parent directory if
    /// needed. Pretty-printed for human editability.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(self)?;
        std::fs::write(path, body)?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Schedule> {
        self.schedules.iter().find(|s| s.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Schedule> {
        self.schedules.iter_mut().find(|s| s.id == id)
    }

    /// Insert a new schedule. Errors if the id already exists — we
    /// don't silently replace; the user calls `rm` then `add` if they
    /// want to overwrite (avoids accidental data loss on a typo).
    pub fn add(&mut self, schedule: Schedule) -> Result<()> {
        if self.get(&schedule.id).is_some() {
            return Err(Error::Config(format!(
                "schedule id '{}' already exists — `thclaws schedule rm {}` first",
                schedule.id, schedule.id
            )));
        }
        validate_cron(&schedule.cron)?;
        self.schedules.push(schedule);
        Ok(())
    }

    /// Returns whether anything was removed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.schedules.len();
        self.schedules.retain(|s| s.id != id);
        before != self.schedules.len()
    }
}

/// Validate a cron expression at add time. Uses the `cron` crate
/// which expects 6-or-7-field input (with seconds and optional
/// year), so we prepend `0` to the user-supplied 5-field POSIX
/// expression before parsing — surfacing a clean error message
/// keyed off the user's input rather than our normalization.
pub fn validate_cron(expr: &str) -> Result<()> {
    let normalized = normalize_cron(expr);
    cron::Schedule::from_str(&normalized)
        .map(|_| ())
        .map_err(|e| Error::Config(format!("invalid cron expression '{expr}': {e}")))
}

fn normalize_cron(expr: &str) -> String {
    let trimmed = expr.trim();
    let field_count = trimmed.split_whitespace().count();
    if field_count == 5 {
        // Prepend a `0` seconds field so 5-field POSIX cron parses
        // through the 6+ field `cron::Schedule`.
        format!("0 {trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Outcome of one fire. `exit_code = None` means the process was
/// killed by the timeout enforcer or never produced a status.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub log_path: PathBuf,
    pub exit_code: Option<i32>,
    pub duration: Duration,
    pub timed_out: bool,
}

/// Fire a single schedule by id, using the default user-level store.
/// Convenience wrapper around [`run_once_with`].
pub fn run_once(id: &str, binary_path: &Path) -> Result<RunOutcome> {
    run_once_with(id, binary_path, None)
}

/// Fire a single schedule by id. Loads the store from `store_path`
/// (or the default `~/.config/thclaws/schedules.json` when `None`),
/// finds the entry, spawns `thclaws --print "<prompt>"` with
/// `current_dir(<cwd>)`, captures stdout+stderr into a timestamped
/// log file under `~/.local/share/thclaws/logs/<id>/`, waits for
/// completion (with optional timeout), updates `last_run` +
/// `last_exit` in the store, and returns the outcome.
///
/// The `store_path` parameter exists for tests so they can hit a
/// tempdir without polluting the user's real schedules file. It
/// also opens the door to a future per-project overlay store.
///
/// The `binary_path` argument is the `thclaws` executable to spawn
/// for the job. In production this is `std::env::current_exe()`
/// so a scheduled run uses the same binary build that registered
/// the schedule.
pub fn run_once_with(
    id: &str,
    binary_path: &Path,
    store_path: Option<&Path>,
) -> Result<RunOutcome> {
    let mut store = match store_path {
        Some(p) => ScheduleStore::load_from(p)?,
        None => ScheduleStore::load()?,
    };
    let schedule = store
        .get(id)
        .ok_or_else(|| Error::Config(format!("no schedule with id '{id}'")))?
        .clone();

    if !schedule.enabled {
        return Err(Error::Config(format!(
            "schedule '{id}' is disabled — enable it before running"
        )));
    }
    if !schedule.cwd.exists() {
        return Err(Error::Config(format!(
            "schedule '{id}' cwd does not exist: {}",
            schedule.cwd.display()
        )));
    }

    let outcome = spawn_job(&schedule, binary_path)?;

    if let Some(entry) = store.get_mut(id) {
        entry.last_run = Some(Utc::now().to_rfc3339());
        entry.last_exit = outcome.exit_code;
    }
    match store_path {
        Some(p) => store.save_to(p)?,
        None => store.save()?,
    }

    Ok(outcome)
}

fn spawn_job(schedule: &Schedule, binary_path: &Path) -> Result<RunOutcome> {
    let log_dir = log_dir_for(&schedule.id)?;
    std::fs::create_dir_all(&log_dir)?;
    // Filesystem-safe timestamp: `2026-05-06T13-42-09Z` — colons in
    // RFC 3339 break Windows filenames, so we substitute `-`.
    let ts = Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
    let log_path = log_dir.join(format!("{ts}.log"));
    let log_file = std::fs::File::create(&log_path)?;
    // stderr piped into the same file as stdout so the run log is
    // a single linear trace; matches what a user would see in the
    // terminal.
    let log_file_for_err = log_file.try_clone()?;

    let mut cmd = Command::new(binary_path);
    cmd.arg("--print")
        .arg(&schedule.prompt)
        .current_dir(&schedule.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_for_err));
    if let Some(ref m) = schedule.model {
        cmd.arg("--model").arg(m);
    }
    if let Some(n) = schedule.max_iterations {
        cmd.arg("--max-iterations").arg(n.to_string());
    }
    // Tag the spawned process so any child can tell it was launched
    // by the scheduler (e.g. for hook authors who want to skip
    // certain interactive behavior).
    cmd.env("THCLAWS_SCHEDULE_ID", &schedule.id);

    // `CREATE_NO_WINDOW` on Windows so each scheduled fire doesn't
    // flash a console window — same trick the GUI binary uses for
    // its own startup. No-op on Unix.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }

    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Tool(format!("spawn '{}': {e}", binary_path.display())))?;

    let timeout = schedule.timeout_secs.map(Duration::from_secs);
    let (exit_code, timed_out) = match timeout {
        None => (
            child
                .wait()
                .map_err(|e| Error::Tool(format!("wait: {e}")))?
                .code(),
            false,
        ),
        Some(d) => wait_with_timeout(&mut child, d)?,
    };

    Ok(RunOutcome {
        log_path,
        exit_code,
        duration: started.elapsed(),
        timed_out,
    })
}

/// Block on a child up to `timeout`. If the timeout elapses, kill
/// the child and report `timed_out = true`. Polls every 100 ms —
/// scheduled jobs are minute-grained at best, so the polling
/// granularity doesn't matter.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<(Option<i32>, bool)> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok((status.code(), false)),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok((None, true));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(Error::Tool(format!("try_wait: {e}"))),
        }
    }
}

/// `~/.local/share/thclaws/logs/<id>/`. Mirrors XDG Base Directory
/// on Linux; on macOS we keep the same path rather than splitting
/// to `~/Library/Logs` so users have one consistent place to look
/// across both OSes.
pub fn log_dir_for(id: &str) -> Result<PathBuf> {
    let home = crate::util::home_dir().ok_or_else(|| {
        Error::Config("no home directory found — cannot place schedule logs".into())
    })?;
    Ok(home.join(".local/share/thclaws/logs").join(id))
}

// ─── Step 2: in-process scheduler ────────────────────────────────────
//
// Long-running tokio task that polls the schedule store every
// `TICK_INTERVAL` and fires due jobs by calling `run_once` on a
// blocking-pool thread. State (per-schedule cursor, currently-running
// set) lives inside the task's `InProcessScheduler` struct.
//
// Cursor semantics: each schedule has an in-memory "last considered"
// timestamp. On first sight it's seeded from the entry's `lastRun`
// field if present (to honor manual edits that force a catch-up),
// otherwise from `now` (skip catch-up — the safer default after a
// week-long laptop sleep). On each tick we ask the cron parser for
// the first fire after the cursor; if that's `<= now` the job fires
// and the cursor advances to that fire time.
//
// Concurrent fires: if a schedule is still running when its next
// fire comes due we skip — like `cron`'s `--no-overlap`. v1 doesn't
// queue missed fires.

/// Default tick cadence. 30s is fine for minute-granular cron
/// (every cron field is at least 1 minute wide), and short enough
/// that schedule edits take effect quickly.
const TICK_INTERVAL: Duration = Duration::from_secs(30);

/// Parse `Schedule.last_run` (RFC 3339) back to a `DateTime<Utc>`.
/// Returns None when the field is absent or unparseable.
pub fn parse_last_run(schedule: &Schedule) -> Option<DateTime<Utc>> {
    schedule
        .last_run
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// First cron fire strictly after `after`. Returns `None` if the
/// expression is invalid or has no upcoming fire (rare — e.g. a
/// `cron` expression pinned to a specific year that's already past).
pub fn compute_next_fire(cron_expr: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let normalized = normalize_cron(cron_expr);
    let schedule = cron::Schedule::from_str(&normalized).ok()?;
    schedule.after(&after).next()
}

/// First N cron fires strictly after `after`. Returns an empty Vec
/// if the expression is invalid. Used by the schedule-add modal's
/// live preview to show users the next few times their cron will
/// trigger before they commit the entry.
pub fn compute_next_n_fires(cron_expr: &str, after: DateTime<Utc>, n: usize) -> Vec<DateTime<Utc>> {
    let normalized = normalize_cron(cron_expr);
    let Ok(schedule) = cron::Schedule::from_str(&normalized) else {
        return Vec::new();
    };
    schedule.after(&after).take(n).collect()
}

/// In-process scheduler state. Owns the per-schedule cursor map and
/// the currently-running set; both live for the lifetime of the
/// task so no persistence is needed (cursors re-seed from `lastRun`
/// on next process start).
pub struct InProcessScheduler {
    cursors: HashMap<String, DateTime<Utc>>,
    running: Arc<Mutex<HashSet<String>>>,
    binary: PathBuf,
    /// `None` means the default user-level store. Tests pass `Some`
    /// to redirect to a tempdir.
    store_path: Option<PathBuf>,
}

impl InProcessScheduler {
    pub fn new(binary: PathBuf) -> Self {
        Self {
            cursors: HashMap::new(),
            running: Arc::new(Mutex::new(HashSet::new())),
            binary,
            store_path: None,
        }
    }

    /// Build a scheduler that reads + writes a specific store path
    /// instead of the user-level default. For tests and for any
    /// future "scheduler bound to a project-local store" plumbing.
    pub fn with_store_path(binary: PathBuf, store_path: PathBuf) -> Self {
        Self {
            cursors: HashMap::new(),
            running: Arc::new(Mutex::new(HashSet::new())),
            binary,
            store_path: Some(store_path),
        }
    }

    /// One pass over the store. Reads schedules.json, advances
    /// cursors, fires due-and-not-currently-running jobs on the
    /// blocking pool. Errors (bad JSON, missing home dir) are
    /// swallowed silently — a transient hiccup shouldn't kill the
    /// scheduler. They're surfaced when the user runs the CLI by
    /// hand.
    ///
    /// Returns the spawn-blocking JoinHandles for fires this tick.
    /// Production callers drop them (fire-and-forget); tests
    /// `await` them to drain before asserting on store state.
    pub fn tick(&mut self, now: DateTime<Utc>) -> Vec<(String, tokio::task::JoinHandle<()>)> {
        let store_result = match self.store_path.as_ref() {
            Some(p) => ScheduleStore::load_from(p),
            None => ScheduleStore::load(),
        };
        let store = match store_result {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let mut fired = Vec::new();
        for schedule in store.schedules.iter().filter(|s| s.enabled) {
            // Seed cursor on first sight: prefer the on-disk lastRun
            // (so manual JSON edits forcing a catch-up work), fall
            // back to `now` (skip-catch-up default).
            let cursor = *self
                .cursors
                .entry(schedule.id.clone())
                .or_insert_with(|| parse_last_run(schedule).unwrap_or(now));

            let Some(next) = compute_next_fire(&schedule.cron, cursor) else {
                continue;
            };
            if next > now {
                continue;
            }

            // Skip-overlap guard.
            {
                let mut r = self.running.lock().expect("running set lock");
                if r.contains(&schedule.id) {
                    continue;
                }
                r.insert(schedule.id.clone());
            }

            // Advance the cursor before firing — even if the spawn
            // fails, we don't want to retry-storm the same fire on
            // the next tick.
            self.cursors.insert(schedule.id.clone(), next);

            // Fire on the blocking pool so `wait_with_timeout`'s
            // `std::thread::sleep` doesn't park a tokio worker.
            let id_for_task = schedule.id.clone();
            let binary = self.binary.clone();
            let running = self.running.clone();
            let store_path = self.store_path.clone();
            let handle = tokio::task::spawn_blocking(move || {
                match run_once_with(&id_for_task, &binary, store_path.as_deref()) {
                    Ok(outcome) => {
                        let exit = outcome
                            .exit_code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "(timeout)".to_string());
                        eprintln!(
                            "\x1b[36m[schedule] '{id_for_task}' fired \
                             — exit={exit} duration={}.{:03}s log={}\x1b[0m",
                            outcome.duration.as_secs(),
                            outcome.duration.subsec_millis(),
                            outcome.log_path.display(),
                        );
                    }
                    Err(e) => {
                        eprintln!("\x1b[31m[schedule] '{id_for_task}' failed: {e}\x1b[0m");
                    }
                }
                if let Ok(mut r) = running.lock() {
                    r.remove(&id_for_task);
                }
            });
            fired.push((schedule.id.clone(), handle));
        }
        fired
    }
}

// ─── Step 3: native daemon (PID file, signal handler, install) ───────
//
// `thclaws daemon` is a long-running foreground process that spawns
// the same in-process scheduler used by the GUI/CLI surfaces. The PID
// file at `~/.local/state/thclaws/scheduler.pid` lets `schedule
// status` answer "is the daemon up?" without IPC, and prevents two
// daemons from running concurrently against the same store.
//
// Process model: foreground process under launchd (macOS) or
// systemd --user (Linux). KeepAlive=true on launchd, Restart=on-failure
// on systemd → either supervisor restarts the process if it crashes.
// We don't fork-detach ourselves; the supervisor wants us in the
// foreground so it can track us by PID.

/// State of the scheduler daemon: did we find a PID file, and if so,
/// is the process still alive?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonStatus {
    /// Daemon is running with this PID.
    Running(u32),
    /// PID file exists but the process is gone (crashed or killed
    /// without graceful shutdown).
    Stale(u32),
    /// No PID file; daemon was never started or was cleanly stopped.
    NotRunning,
}

/// `~/.local/state/thclaws/scheduler.pid`. State dir per the XDG
/// Base Directory spec — it's where supervisord-style daemons
/// usually keep PID files on user-level installs.
pub fn pid_file_path() -> Option<PathBuf> {
    crate::util::home_dir().map(|h| h.join(".local/state/thclaws/scheduler.pid"))
}

/// Read the PID file and check whether the recorded process is
/// still alive. `kill(pid, 0)` on Unix returns success without
/// sending a signal if the process exists and we own it.
pub fn daemon_status() -> DaemonStatus {
    let Some(path) = pid_file_path() else {
        return DaemonStatus::NotRunning;
    };
    let Ok(body) = std::fs::read_to_string(&path) else {
        return DaemonStatus::NotRunning;
    };
    let pid: u32 = match body.trim().parse() {
        Ok(p) => p,
        Err(_) => return DaemonStatus::NotRunning,
    };
    if pid_alive(pid) {
        DaemonStatus::Running(pid)
    } else {
        DaemonStatus::Stale(pid)
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // kill(pid, 0): no-signal liveness probe. Returns 0 if the
    // process exists and we own it; -1 with ESRCH (3) if it's gone;
    // -1 with EPERM (1) if it exists but is owned by another user
    // (still alive for our purposes — we only want "is the process
    // there", not "can I signal it").
    //
    // SAFETY: libc::kill is a single syscall with no Rust-side
    // invariants; we never dereference any pointer.
    unsafe {
        let ret = libc::kill(pid as libc::pid_t, 0);
        if ret == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error().raw_os_error();
        err == Some(libc::EPERM)
    }
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    // Windows daemon support is deferred — Step 3 ships macOS/Linux
    // first. When the Windows path lands, this becomes an
    // OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) probe.
    false
}

/// Atomically write the current PID to the pid file. Returns the
/// file path on success.
fn write_pid_file() -> Result<PathBuf> {
    let path = pid_file_path().ok_or_else(|| {
        Error::Config("no home directory found — cannot place daemon PID file".into())
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pid = std::process::id();
    // Write to a tempfile then rename so a half-written PID file
    // can never be observed by `schedule status`.
    let tmp = path.with_extension("pid.tmp");
    std::fs::write(&tmp, pid.to_string())?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

fn remove_pid_file() {
    if let Some(path) = pid_file_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Run the daemon process: claim the PID file, spawn the scheduler
/// task, block on SIGTERM/SIGINT, then clean up. Refuses to start if
/// another live daemon already owns the PID file. A stale PID file
/// (process gone) is reclaimed automatically — typical after a crash.
///
/// Designed to be called from `#[tokio::main]`. The function returns
/// `Ok(())` on graceful shutdown, `Err` on failure to claim the PID
/// file or start the scheduler.
pub async fn run_daemon() -> Result<()> {
    match daemon_status() {
        DaemonStatus::Running(pid) => {
            return Err(Error::Config(format!(
                "another daemon is already running (pid {pid}); \
                 stop it with `thclaws schedule uninstall` or `kill {pid}` first"
            )));
        }
        DaemonStatus::Stale(pid) => {
            eprintln!("\x1b[33m[daemon] reclaiming stale PID file (last pid {pid})\x1b[0m");
        }
        DaemonStatus::NotRunning => {}
    }

    let pid_path = write_pid_file()?;
    eprintln!(
        "\x1b[36m[daemon] thclaws scheduler started (pid {}, pid file {})\x1b[0m",
        std::process::id(),
        pid_path.display(),
    );

    let binary = std::env::current_exe()
        .map_err(|e| Error::Config(format!("cannot resolve current_exe: {e}")))?;
    let scheduler_handle = spawn_scheduler_task(binary.clone());

    // Filesystem watchers for any schedule with watch_workspace=true.
    // Reconciled every 30s by re-reading the store and rebuilding
    // the manager — drops all old watchers, spawns fresh ones for
    // the current set. Cheap (notify watchers are ~tens of ns to
    // construct) and avoids hand-rolling diff logic.
    let watch_handle = spawn_watch_reconciler(binary);

    // Wait for SIGTERM (launchd/systemd graceful stop) or SIGINT
    // (Ctrl-C from `thclaws daemon` foreground).
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).map_err(Error::from)?;
        let mut int_ = signal(SignalKind::interrupt()).map_err(Error::from)?;
        tokio::select! {
            _ = term.recv() => eprintln!("\x1b[36m[daemon] SIGTERM received — shutting down\x1b[0m"),
            _ = int_.recv() => eprintln!("\x1b[36m[daemon] SIGINT received — shutting down\x1b[0m"),
        }
    }
    #[cfg(not(unix))]
    {
        // Windows: no Unix signals. tokio::signal::ctrl_c covers
        // Ctrl-C and the equivalent of "stop the service" via
        // SetConsoleCtrlHandler. Service stop on a real Windows
        // Service uses a different mechanism (SCM control codes)
        // not covered here — Windows Service support is deferred
        // (see daemon docs).
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("\x1b[36m[daemon] Ctrl-C received — shutting down\x1b[0m");
    }

    scheduler_handle.abort();
    watch_handle.abort();
    remove_pid_file();
    eprintln!("\x1b[36m[daemon] stopped cleanly\x1b[0m");
    Ok(())
}

/// Background task that rebuilds the WatchManager every 30s. Each
/// rebuild re-reads `~/.config/thclaws/schedules.json` and replaces
/// the current manager — its Drop stops the previous watchers, the
/// new one spawns fresh ones. Schedule edits propagate within one
/// tick interval (matches how the cron scheduler picks up edits).
fn spawn_watch_reconciler(binary: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // The `_current` binding holds the active WatchManager.
        // Reassigning it drops the old manager (stops its watchers)
        // before the new one starts, avoiding a brief overlap where
        // duplicate events would fire from both.
        let mut _current: Option<WatchManager> = None;
        loop {
            let store = match ScheduleStore::load() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("\x1b[33m[watch] store reload failed: {e}\x1b[0m");
                    tokio::time::sleep(TICK_INTERVAL).await;
                    continue;
                }
            };
            // Drop the previous manager explicitly so its watchers
            // stop BEFORE the new manager spawns its replacements.
            _current = None;
            match WatchManager::from_store(&store, binary.clone()) {
                Ok(m) => _current = Some(m),
                Err(e) => {
                    eprintln!("\x1b[31m[watch] manager build failed: {e}\x1b[0m");
                }
            }
            tokio::time::sleep(TICK_INTERVAL).await;
        }
    })
}

// ─── Daemon install: launchd (macOS) / systemd (Linux) ──────────────
//
// Two responsibilities:
//   1. Generate the supervisor file (plist or unit) at the right
//      well-known path.
//   2. Activate it with the supervisor (`launchctl bootstrap` or
//      `systemctl --user enable --now`).
//
// On macOS we auto-bootstrap because `~/Library/LaunchAgents` is the
// user's own directory and `launchctl bootstrap gui/$UID` requires
// no privileges. On Linux we write the file but print the systemctl
// commands rather than running them — Linux distros vary in how
// systemd-user is configured and we'd rather not assume.

/// Stable launchd label / systemd unit name. Matches the plist
/// filename so users grepping `launchctl list` see the same string
/// they'd see in `~/Library/LaunchAgents/`.
const DAEMON_LABEL: &str = "sh.thclaws.scheduler";

/// Where the supervisor file lives. macOS: a `.plist` under the
/// user's LaunchAgents. Linux: a `.service` under user-systemd.
pub fn supervisor_file_path() -> Option<PathBuf> {
    let home = crate::util::home_dir()?;
    #[cfg(target_os = "macos")]
    {
        Some(home.join(format!("Library/LaunchAgents/{DAEMON_LABEL}.plist")))
    }
    #[cfg(target_os = "linux")]
    {
        Some(home.join(format!(".config/systemd/user/thclaws-scheduler.service")))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = home;
        None
    }
}

/// Daemon log path (stderr+stdout combined) — written into the
/// supervisor config so the platform redirects daemon output here.
fn daemon_log_path() -> Option<PathBuf> {
    crate::util::home_dir().map(|h| h.join(".local/share/thclaws/daemon.log"))
}

/// Build the macOS launchd plist body. Embeds the absolute binary
/// path so re-installs after a binary move/upgrade just work
/// (compared to relying on $PATH which launchd processes don't
/// inherit fully).
#[cfg(target_os = "macos")]
fn build_launchd_plist(binary: &Path, log: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = DAEMON_LABEL,
        bin = binary.display(),
        log = log.display(),
    )
}

/// Build the Linux systemd-user unit body. `Type=simple` because
/// the daemon runs in foreground; `Restart=on-failure` matches
/// launchd's `KeepAlive`.
#[cfg(target_os = "linux")]
fn build_systemd_unit(binary: &Path, log: &Path) -> String {
    format!(
        r#"[Unit]
Description=thClaws scheduler daemon
After=default.target

[Service]
Type=simple
ExecStart={bin} daemon
Restart=on-failure
RestartSec=5
StandardOutput=append:{log}
StandardError=append:{log}

[Install]
WantedBy=default.target
"#,
        bin = binary.display(),
        log = log.display(),
    )
}

/// Outcome of `install_daemon` — what was written, what to run next.
pub struct InstallReport {
    pub supervisor_path: PathBuf,
    /// Next-step commands to print to the user. Empty when the
    /// install code already activated the supervisor (macOS).
    pub next_steps: Vec<String>,
}

/// Install the scheduler daemon as a user-level supervised service
/// on the current platform. macOS: writes a launchd plist and runs
/// `launchctl bootstrap`. Linux: writes a systemd-user unit and
/// prints the `systemctl --user enable --now` next-steps.
pub fn install_daemon() -> Result<InstallReport> {
    let path = supervisor_file_path().ok_or_else(|| {
        Error::Config(format!(
            "daemon install not yet supported on this platform (target_os={})",
            std::env::consts::OS
        ))
    })?;
    let binary = std::env::current_exe()
        .map_err(|e| Error::Config(format!("cannot resolve current_exe: {e}")))?;
    let log = daemon_log_path()
        .ok_or_else(|| Error::Config("no home directory found — cannot place daemon log".into()))?;
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    #[cfg(target_os = "macos")]
    {
        let body = build_launchd_plist(&binary, &log);
        std::fs::write(&path, body)?;

        // Auto-bootstrap. `launchctl bootstrap gui/$UID <plist>`
        // is the modern API; if a previous version is loaded we
        // bootout first so the new plist takes effect.
        let uid = unsafe { libc::getuid() };
        let domain = format!("gui/{uid}");

        // bootout — ignore errors (typically "service not found"
        // on a clean install).
        let _ = std::process::Command::new("launchctl")
            .arg("bootout")
            .arg(&domain)
            .arg(&path)
            .output();

        let bootstrap = std::process::Command::new("launchctl")
            .arg("bootstrap")
            .arg(&domain)
            .arg(&path)
            .output()
            .map_err(|e| Error::Tool(format!("launchctl bootstrap: {e}")))?;
        if !bootstrap.status.success() {
            let stderr = String::from_utf8_lossy(&bootstrap.stderr);
            return Err(Error::Tool(format!(
                "launchctl bootstrap failed: {}",
                stderr.trim()
            )));
        }

        Ok(InstallReport {
            supervisor_path: path,
            next_steps: Vec::new(),
        })
    }
    #[cfg(target_os = "linux")]
    {
        let body = build_systemd_unit(&binary, &log);
        std::fs::write(&path, body)?;
        Ok(InstallReport {
            supervisor_path: path,
            next_steps: vec![
                "systemctl --user daemon-reload".into(),
                "systemctl --user enable --now thclaws-scheduler.service".into(),
            ],
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (binary, log, path);
        Err(Error::Config(format!(
            "daemon install not yet supported on this platform (target_os={})",
            std::env::consts::OS
        )))
    }
}

/// Inverse of `install_daemon`. Stops + removes the supervisor
/// entry. Returns the path that was removed (or that didn't
/// exist). Errors on filesystem failures but tolerates an already-
/// gone supervisor entry.
pub fn uninstall_daemon() -> Result<PathBuf> {
    let path = supervisor_file_path().ok_or_else(|| {
        Error::Config(format!(
            "daemon uninstall not yet supported on this platform (target_os={})",
            std::env::consts::OS
        ))
    })?;

    #[cfg(target_os = "macos")]
    {
        if path.exists() {
            let uid = unsafe { libc::getuid() };
            let domain = format!("gui/{uid}");
            let _ = std::process::Command::new("launchctl")
                .arg("bootout")
                .arg(&domain)
                .arg(&path)
                .output();
            std::fs::remove_file(&path)?;
        }
        Ok(path)
    }
    #[cfg(target_os = "linux")]
    {
        if path.exists() {
            let _ = std::process::Command::new("systemctl")
                .arg("--user")
                .arg("disable")
                .arg("--now")
                .arg("thclaws-scheduler.service")
                .output();
            std::fs::remove_file(&path)?;
        }
        Ok(path)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Ok(path)
    }
}

/// Spawn the in-process scheduler on the current tokio runtime.
/// Runs forever; the task ends when the process exits. Returns the
/// `JoinHandle` mostly so callers can `.abort()` if they want
/// (currently no caller does — Step 3's daemon will).
///
/// `binary` is the executable to spawn for fires — almost always
/// `std::env::current_exe()`. Pass an explicit path in tests so
/// the scheduler points at a fake binary that doesn't actually
/// invoke the agent loop.
pub fn spawn_scheduler_task(binary: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut sched = InProcessScheduler::new(binary);
        eprintln!(
            "\x1b[36m[schedule] in-process scheduler running (tick {}s)\x1b[0m",
            TICK_INTERVAL.as_secs()
        );
        loop {
            tokio::time::sleep(TICK_INTERVAL).await;
            sched.tick(Utc::now());
        }
    })
}

// ─── Daemon-only: filesystem-change trigger ─────────────────────────
//
// When `Schedule.watch_workspace` is true, the daemon spawns a
// debounced filesystem watcher rooted at the schedule's `cwd`. On a
// (debounced + filtered) event, the schedule fires via the same
// `run_once` path the cron and CLI triggers use. The in-process
// scheduler (Step 2) ignores this flag — only `run_daemon` wires up
// watchers, since unattended filesystem-driven fires are the whole
// point of having a daemon.
//
// Key safety properties:
//
// 1. **Hardcoded ignore list** for paths that would cause infinite
//    fire loops if the schedule's own work touches them — most
//    importantly `.thclaws/` (where the spawned `--print` job's
//    session JSONL gets written) and `.git/` (which churns under
//    normal git ops). v1 doesn't read `.gitignore`; users who want
//    finer control can switch to cron-only triggers for now.
//
// 2. **Cooldown** of 60s after a fire starts: filesystem events
//    received during a fire and the cooldown window after it are
//    swallowed. Combined with the skip-overlap guard (already in
//    `running` set), this keeps a poorly-scoped prompt from
//    detonating into a fire-storm.

const WATCH_DEBOUNCE: Duration = Duration::from_secs(2);
const WATCH_COOLDOWN: Duration = Duration::from_secs(60);

/// Path components that should never trigger a filesystem-change
/// fire. Matched as path segment exact-equality (case-sensitive on
/// Linux/macOS, but `.git` etc. are conventional names so this is
/// fine in practice). Match logic: any ancestor of the changed file
/// equals one of these segments → ignore.
const IGNORED_SEGMENTS: &[&str] = &[
    ".thclaws",
    ".git",
    ".DS_Store",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".cache",
];

/// True when the changed path falls inside one of the ignored
/// segments somewhere between `root` and the leaf. Defensive against
/// paths above `root` (notify normalizes these but a misconfigured
/// watcher could surface them) — only ancestors strictly inside
/// `root` are checked.
///
/// Canonicalizes both ends before the strip_prefix check so macOS's
/// FSEvents-style `/var/folders/...` → `/private/var/folders/...`
/// rewrite (and equivalent symlink hops on Linux) doesn't cause a
/// strip_prefix mismatch. Falls through to literal comparison when
/// canonicalize fails (e.g. path no longer exists at check time).
///
/// Root-level events: when the changed path IS the watched root
/// (`rel` empty), we can't tell what actually changed underneath.
/// macOS FSEvents coalesces concurrent writes across multiple
/// subdirectories into a single parent-dir event at this level.
/// We return `true` here so writes happening exclusively under
/// `.git/` / `.thclaws/` don't fire-loop the schedule when FSEvents
/// hides them inside a parent-dir event. Cost: a rare false-negative
/// on a legitimate top-level file edit when FSEvents coalesces.
/// In practice subsequent edits emit finer-grained events, and the
/// schedule's cron trigger (if any) is the secondary safety net.
pub fn is_path_ignored(root: &Path, changed: &Path) -> bool {
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let canon_changed = changed
        .canonicalize()
        .unwrap_or_else(|_| changed.to_path_buf());
    let Ok(rel) = canon_changed.strip_prefix(&canon_root) else {
        return false;
    };
    if rel.as_os_str().is_empty() {
        return true;
    }
    rel.components().any(|c| match c {
        std::path::Component::Normal(seg) => seg
            .to_str()
            .map(|s| IGNORED_SEGMENTS.contains(&s))
            .unwrap_or(false),
        _ => false,
    })
}

/// Spawn one filesystem watcher per `watchWorkspace=true` schedule.
/// Returns a handle bag whose Drop stops every watcher and aborts
/// the dispatcher task. The daemon owns this for the duration of
/// its lifetime.
///
/// Reconciliation on store edits is intentionally simple: the
/// daemon's outer loop replaces the entire `WatchManager` on each
/// store reload (the existing manager Drop-stops every watcher,
/// the new one Spawns fresh ones). With ~10s of editor save bursts
/// and the 30s reload tick, this is plenty.
pub struct WatchManager {
    /// Owned debouncers. Drop = stop watching.
    _debouncers:
        Vec<notify_debouncer_mini::Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>>,
    /// Aborted on Drop so the dispatch task doesn't leak.
    dispatch_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for WatchManager {
    fn drop(&mut self) {
        if let Some(h) = self.dispatch_handle.take() {
            h.abort();
        }
    }
}

impl WatchManager {
    /// Build a watch manager from the current store. Schedules with
    /// `watch_workspace=false` (or `enabled=false`) are skipped.
    /// Schedules whose `cwd` doesn't exist log a warning and skip.
    /// Returns Ok(manager) even when no schedules want watching —
    /// the manager's Drop is a no-op then.
    pub fn from_store(store: &ScheduleStore, binary: PathBuf) -> Result<Self> {
        Self::from_store_with_path(store, binary, None)
    }

    /// Same as [`from_store`] but routes fires through a specific
    /// `store_path` instead of the default user-level store. Used
    /// by tests so a fired job doesn't update the user's real
    /// `~/.config/thclaws/schedules.json`.
    pub fn from_store_with_path(
        store: &ScheduleStore,
        binary: PathBuf,
        store_path: Option<PathBuf>,
    ) -> Result<Self> {
        use notify_debouncer_mini::new_debouncer;
        use notify_debouncer_mini::notify::RecursiveMode;

        // Single shared channel: every watcher reports debounced
        // events into this `tokio::mpsc` and one consumer task
        // dispatches the fire. Simpler than per-watcher tasks and
        // serializes the cooldown bookkeeping.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, PathBuf)>();
        let last_fire: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
        let running: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

        let mut debouncers = Vec::new();
        for sched in store
            .schedules
            .iter()
            .filter(|s| s.enabled && s.watch_workspace)
        {
            let id = sched.id.clone();
            let cwd = sched.cwd.clone();
            if !cwd.exists() {
                eprintln!(
                    "\x1b[33m[watch] '{id}': cwd does not exist, skipping: {}\x1b[0m",
                    cwd.display()
                );
                continue;
            }
            let id_for_handler = id.clone();
            let cwd_for_handler = cwd.clone();
            let tx_for_handler = tx.clone();
            let mut debouncer = match new_debouncer(WATCH_DEBOUNCE, move |result| {
                match result {
                    Ok(events) => {
                        let events: Vec<notify_debouncer_mini::DebouncedEvent> = events;
                        for ev in events {
                            if is_path_ignored(&cwd_for_handler, &ev.path) {
                                continue;
                            }
                            let _ = tx_for_handler.send((id_for_handler.clone(), ev.path.clone()));
                        }
                    }
                    Err(e) => {
                        // notify-debouncer-mini 0.4's callback gives
                        // a single Error (not a Vec) on failure.
                        eprintln!("\x1b[31m[watch] '{id_for_handler}' error: {e}\x1b[0m");
                    }
                }
            }) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("\x1b[31m[watch] '{id}': could not start watcher: {e}\x1b[0m");
                    continue;
                }
            };
            if let Err(e) = debouncer.watcher().watch(&cwd, RecursiveMode::Recursive) {
                eprintln!(
                    "\x1b[31m[watch] '{id}': watch({}) failed: {e}\x1b[0m",
                    cwd.display()
                );
                continue;
            }
            eprintln!(
                "\x1b[36m[watch] '{id}': watching {} (debounce {}s, cooldown {}s)\x1b[0m",
                cwd.display(),
                WATCH_DEBOUNCE.as_secs(),
                WATCH_COOLDOWN.as_secs(),
            );
            debouncers.push(debouncer);
        }

        // Dispatcher task: consumes events, applies cooldown +
        // skip-overlap, and fires `run_once` on the blocking pool
        // for any event that survives both gates.
        let binary_for_task = binary.clone();
        let store_path_for_task = store_path.clone();
        let dispatch_handle = if !debouncers.is_empty() {
            Some(tokio::spawn(async move {
                while let Some((id, path)) = rx.recv().await {
                    // Skip if a fire of this schedule is currently
                    // running.
                    {
                        let r = running.lock().expect("running lock");
                        if r.contains(&id) {
                            continue;
                        }
                    }
                    // Skip if a fire of this schedule completed
                    // recently (cooldown). The agent's own writes
                    // back into cwd would otherwise trigger fires
                    // forever.
                    {
                        let lf = last_fire.lock().expect("last_fire lock");
                        if let Some(t) = lf.get(&id) {
                            if t.elapsed() < WATCH_COOLDOWN {
                                continue;
                            }
                        }
                    }
                    {
                        let mut r = running.lock().expect("running lock");
                        r.insert(id.clone());
                    }
                    {
                        let mut lf = last_fire.lock().expect("last_fire lock");
                        lf.insert(id.clone(), Instant::now());
                    }
                    eprintln!(
                        "\x1b[36m[watch] '{id}': fired (changed: {})\x1b[0m",
                        path.display()
                    );
                    let id_for_blocking = id.clone();
                    let binary = binary_for_task.clone();
                    let running_for_done = running.clone();
                    let store_path_for_blocking = store_path_for_task.clone();
                    tokio::task::spawn_blocking(move || {
                        match run_once_with(
                            &id_for_blocking,
                            &binary,
                            store_path_for_blocking.as_deref(),
                        ) {
                            Ok(o) => {
                                let exit = o
                                    .exit_code
                                    .map(|c| c.to_string())
                                    .unwrap_or_else(|| "(timeout)".into());
                                eprintln!(
                                    "\x1b[36m[watch] '{id_for_blocking}' done — exit={exit} duration={}.{:03}s log={}\x1b[0m",
                                    o.duration.as_secs(),
                                    o.duration.subsec_millis(),
                                    o.log_path.display(),
                                );
                            }
                            Err(e) => {
                                eprintln!("\x1b[31m[watch] '{id_for_blocking}' failed: {e}\x1b[0m");
                            }
                        }
                        if let Ok(mut r) = running_for_done.lock() {
                            r.remove(&id_for_blocking);
                        }
                    });
                }
            }))
        } else {
            // Drop the unused tx so the rx side closes cleanly when
            // the manager goes away — no dispatcher task to spawn.
            drop(tx);
            None
        };

        Ok(Self {
            _debouncers: debouncers,
            dispatch_handle,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a `#!/bin/sh` script to `path` and chmod it executable.
    /// Crucially, the write fd is dropped before chmod+spawn — Linux
    /// returns `ETXTBSY` ("Text file busy") if you try to `exec()` a
    /// file that any process still holds open for writing. macOS
    /// doesn't enforce this restriction so the bug only surfaces on
    /// `ubuntu-latest` CI, where every spawn-based schedule test was
    /// flaking. Centralising the pattern here means new tests can't
    /// reintroduce the same race.
    #[cfg(unix)]
    fn write_fake_executable(path: &std::path::Path, script: &str) {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        {
            let mut f = std::fs::File::create(path).unwrap();
            writeln!(f, "{}", script).unwrap();
            // f drops at end of this scope, releasing the write fd
            // before chmod+spawn below.
        }
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn cron_validation_accepts_valid_5_field() {
        assert!(validate_cron("*/5 * * * *").is_ok());
        assert!(validate_cron("0 9 * * MON-FRI").is_ok());
        assert!(validate_cron("0 0 1 * *").is_ok());
    }

    #[test]
    fn cron_validation_rejects_garbage() {
        assert!(validate_cron("not a cron").is_err());
        assert!(validate_cron("* * *").is_err()); // too few fields
        assert!(validate_cron("99 * * * *").is_err()); // out of range
    }

    #[test]
    fn schedule_store_roundtrip() {
        let mut store = ScheduleStore::default();
        store
            .add(Schedule {
                id: "morning".into(),
                cron: "30 8 * * *".into(),
                cwd: std::env::temp_dir(),
                prompt: "hello".into(),
                model: Some("gpt-4o".into()),
                max_iterations: Some(20),
                timeout_secs: Some(60),
                enabled: true,
                watch_workspace: false,
                last_run: None,
                last_exit: None,
            })
            .unwrap();
        let json = serde_json::to_string(&store).unwrap();
        let back: ScheduleStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schedules.len(), 1);
        assert_eq!(back.schedules[0].id, "morning");
        assert_eq!(back.schedules[0].model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn add_rejects_duplicate_id() {
        let mut store = ScheduleStore::default();
        let mk = |id: &str| Schedule {
            id: id.into(),
            cron: "* * * * *".into(),
            cwd: std::env::temp_dir(),
            prompt: "x".into(),
            model: None,
            max_iterations: None,
            timeout_secs: None,
            enabled: true,
            watch_workspace: false,
            last_run: None,
            last_exit: None,
        };
        store.add(mk("dup")).unwrap();
        assert!(store.add(mk("dup")).is_err());
    }

    #[test]
    fn add_validates_cron_at_insert_time() {
        let mut store = ScheduleStore::default();
        let bad = Schedule {
            id: "bad".into(),
            cron: "definitely not cron".into(),
            cwd: std::env::temp_dir(),
            prompt: "x".into(),
            model: None,
            max_iterations: None,
            timeout_secs: None,
            enabled: true,
            watch_workspace: false,
            last_run: None,
            last_exit: None,
        };
        assert!(store.add(bad).is_err());
        assert_eq!(store.schedules.len(), 0);
    }

    #[test]
    fn remove_returns_whether_removed() {
        let mut store = ScheduleStore::default();
        store
            .add(Schedule {
                id: "x".into(),
                cron: "* * * * *".into(),
                cwd: std::env::temp_dir(),
                prompt: "p".into(),
                model: None,
                max_iterations: None,
                timeout_secs: None,
                enabled: true,
                watch_workspace: false,
                last_run: None,
                last_exit: None,
            })
            .unwrap();
        assert!(store.remove("x"));
        assert!(!store.remove("x"));
    }

    /// End-to-end spawn test: feed `spawn_job` a fake "binary" that's
    /// actually `/bin/sh -c "echo ok && exit 0"`. We can't pass args
    /// to a `Command::new(binary_path)` style spawn directly — so
    /// instead we drop a tiny shell-script binary in a tempdir,
    /// chmod +x, and use that as the binary. This validates: log
    /// file gets created, exit code is captured, cwd is honored.
    #[cfg(unix)]
    #[test]
    fn spawn_job_captures_exit_and_writes_log() {
        let tmp = tempfile::tempdir().unwrap();
        // Fake binary: ignores --print + the prompt, just echoes its
        // cwd to stdout and exits 0. This proves cwd was honored.
        let fake = tmp.path().join("fake-thclaws");
        write_fake_executable(&fake, "#!/bin/sh\npwd; echo prompt-was: \"$2\"\nexit 7");

        // Use the tempdir as cwd; assert log captures pwd output.
        let work = tempfile::tempdir().unwrap();
        let schedule = Schedule {
            id: format!("test-{}", uuid::Uuid::new_v4()),
            cron: "* * * * *".into(),
            cwd: work.path().to_path_buf(),
            prompt: "hello there".into(),
            model: None,
            max_iterations: None,
            timeout_secs: Some(5),
            enabled: true,
            watch_workspace: false,
            last_run: None,
            last_exit: None,
        };
        let outcome = spawn_job(&schedule, &fake).unwrap();
        assert_eq!(outcome.exit_code, Some(7));
        assert!(!outcome.timed_out);
        let log = std::fs::read_to_string(&outcome.log_path).unwrap();
        // pwd in the spawned shell should match `work.path()` after
        // canonicalization (macOS adds `/private` to `/var/folders/...`).
        let canonical = work.path().canonicalize().unwrap();
        assert!(
            log.contains(canonical.to_string_lossy().trim_end_matches('/'))
                || log.contains(work.path().to_string_lossy().trim_end_matches('/')),
            "log should contain cwd; got: {log}"
        );
        assert!(log.contains("prompt-was: hello there"));

        // Cleanup the schedule's log directory we just created under
        // the real ~/.local/share/thclaws/logs/<id>/.
        if let Ok(d) = log_dir_for(&schedule.id) {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn is_path_ignored_skips_thclaws_dir() {
        let root = std::path::Path::new("/work/proj");
        let inside = std::path::Path::new("/work/proj/.thclaws/sessions/x.jsonl");
        assert!(is_path_ignored(root, inside));
    }

    #[test]
    fn is_path_ignored_skips_git_node_modules() {
        let root = std::path::Path::new("/work/proj");
        for child in [
            "/work/proj/.git/refs/HEAD",
            "/work/proj/node_modules/foo/index.js",
            "/work/proj/target/debug/build.log",
            "/work/proj/dist/main.js",
            "/work/proj/.DS_Store",
        ] {
            assert!(
                is_path_ignored(root, std::path::Path::new(child)),
                "expected ignored: {child}"
            );
        }
    }

    #[test]
    fn is_path_ignored_allows_normal_files() {
        let root = std::path::Path::new("/work/proj");
        for child in [
            "/work/proj/src/main.rs",
            "/work/proj/README.md",
            "/work/proj/docs/notes.md",
            "/work/proj/sub/.thclawsrc",
        ] {
            assert!(
                !is_path_ignored(root, std::path::Path::new(child)),
                "expected allowed: {child}"
            );
        }
    }

    #[test]
    fn is_path_ignored_treats_root_event_as_ignored() {
        // FSEvents on macOS coalesces concurrent writes spanning
        // multiple subdirectories into a single parent-dir event at
        // the watched root level. We can't tell from the bare root
        // path which subtree changed, so the safer call is to skip
        // and rely on subsequent finer-grained events. This catches
        // the common production hazard: concurrent `.git/` index
        // updates plus a real source-file edit get coalesced; the
        // root-only event would otherwise pass the ignore filter
        // and fire-loop the schedule.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert!(is_path_ignored(root, root));
    }

    #[test]
    fn is_path_ignored_outside_root_returns_false() {
        let root = std::path::Path::new("/work/proj");
        // Path outside the watched root: nothing to do — return
        // false (not ignored) so the caller can decide what to do.
        let outside = std::path::Path::new("/elsewhere/.thclaws/file");
        assert!(!is_path_ignored(root, outside));
    }

    /// Real end-to-end: build a WatchManager pointing at a tempdir,
    /// touch a file inside it, observe the schedule fire (last_run
    /// gets set in the tempdir-local store). Uses a fake binary that
    /// just exits 0 so the run is fast — the test asserts the
    /// dispatch chain (notify → debouncer → channel → cooldown →
    /// run_once_with → store update) end-to-end.
    ///
    /// Slow by necessity: WATCH_DEBOUNCE is 2s. We wait 4s after
    /// touching the file to give the debouncer + dispatcher +
    /// spawn_blocking + child process time to land.
    #[cfg(unix)]
    #[test]
    fn watch_manager_fires_on_file_change() {
        // Fake binary: a shell script that exits 0 fast.
        let bin_dir = tempfile::tempdir().unwrap();
        let fake = bin_dir.path().join("fake-thclaws");
        write_fake_executable(&fake, "#!/bin/sh\nexit 0");

        // Tempdir for the watched workspace + store.
        let work = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store_path = store_dir.path().join("schedules.json");
        let id = format!("watch-{}", uuid::Uuid::new_v4());

        let mut store = ScheduleStore::default();
        store
            .add(Schedule {
                id: id.clone(),
                cron: "0 0 1 1 *".into(), // far-future cron — never fires by cron
                cwd: work.path().to_path_buf(),
                prompt: "p".into(),
                timeout_secs: Some(5),
                enabled: true,
                watch_workspace: true,
                ..Default::default()
            })
            .unwrap();
        store.save_to(&store_path).unwrap();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _manager =
                WatchManager::from_store_with_path(&store, fake.clone(), Some(store_path.clone()))
                    .expect("manager build");

            // Give the watcher a beat to bind before mutating.
            tokio::time::sleep(Duration::from_millis(300)).await;

            // Touch a file inside a subdirectory, not at the root.
            // macOS FSEvents may coalesce a single root-level write
            // into a parent-dir event whose path equals the watched
            // root — `is_path_ignored` (correctly) treats those as
            // "can't tell what changed → skip." Writing into `notes/`
            // ensures the reported path is at least one segment deep
            // so the test doesn't depend on FSEvents granularity.
            let sub = work.path().join("notes");
            std::fs::create_dir_all(&sub).unwrap();
            let triggered = sub.join("hello.txt");
            std::fs::write(&triggered, b"hi").unwrap();

            // Wait for: debounce window (2s) + dispatcher + child
            // exit + store save. 4s is safe with margin.
            tokio::time::sleep(Duration::from_secs(4)).await;
        });

        // Reload store from disk and verify the schedule fired.
        let after = ScheduleStore::load_from(&store_path).unwrap();
        let s = after.get(&id).expect("schedule present");
        assert!(
            s.last_run.is_some(),
            "watch trigger should have fired the schedule (last_run is None)"
        );
        assert_eq!(s.last_exit, Some(0), "fake binary exits 0");

        // Cleanup the per-id log dir so the user's real log tree
        // doesn't accumulate test debris.
        if let Ok(d) = log_dir_for(&id) {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// Files under ignored segments (e.g. `.thclaws/`) inside the
    /// watched workspace must NOT trigger fires. Otherwise the
    /// schedule's own session JSONL writes would re-fire forever.
    #[cfg(unix)]
    #[test]
    fn watch_manager_ignores_internal_thclaws_writes() {
        let bin_dir = tempfile::tempdir().unwrap();
        let fake = bin_dir.path().join("fake-thclaws");
        write_fake_executable(&fake, "#!/bin/sh\nexit 0");

        let work = tempfile::tempdir().unwrap();
        // Pre-create the .thclaws directory so the file write below
        // is observed as a child of an existing dir (some platforms
        // emit different events for create-dir vs create-in-dir).
        let thclaws_dir = work.path().join(".thclaws").join("sessions");
        std::fs::create_dir_all(&thclaws_dir).unwrap();

        let store_dir = tempfile::tempdir().unwrap();
        let store_path = store_dir.path().join("schedules.json");
        let id = format!("watch-ignore-{}", uuid::Uuid::new_v4());

        let mut store = ScheduleStore::default();
        store
            .add(Schedule {
                id: id.clone(),
                cron: "0 0 1 1 *".into(),
                cwd: work.path().to_path_buf(),
                prompt: "p".into(),
                timeout_secs: Some(5),
                enabled: true,
                watch_workspace: true,
                ..Default::default()
            })
            .unwrap();
        store.save_to(&store_path).unwrap();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _manager =
                WatchManager::from_store_with_path(&store, fake.clone(), Some(store_path.clone()))
                    .expect("manager build");
            tokio::time::sleep(Duration::from_millis(300)).await;

            // Write only into ignored directories: .thclaws and .git.
            std::fs::write(thclaws_dir.join("noise.jsonl"), b"x").unwrap();
            std::fs::create_dir_all(work.path().join(".git")).unwrap();
            std::fs::write(work.path().join(".git/HEAD"), b"ref").unwrap();

            tokio::time::sleep(Duration::from_secs(4)).await;
        });

        let after = ScheduleStore::load_from(&store_path).unwrap();
        let s = after.get(&id).expect("schedule present");
        assert!(
            s.last_run.is_none(),
            "ignored-segment writes must not trigger a fire (last_run = {:?})",
            s.last_run,
        );

        if let Ok(d) = log_dir_for(&id) {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// Schedules with `watch_workspace=false` must not get a watcher
    /// even when other schedules in the same store do. Defensive
    /// against a future refactor that drops the per-schedule filter.
    #[cfg(unix)]
    #[test]
    fn watch_manager_skips_watch_workspace_false() {
        let bin_dir = tempfile::tempdir().unwrap();
        let fake = bin_dir.path().join("fake-thclaws");
        write_fake_executable(&fake, "#!/bin/sh\nexit 0");

        let work = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store_path = store_dir.path().join("schedules.json");
        let id = format!("no-watch-{}", uuid::Uuid::new_v4());

        let mut store = ScheduleStore::default();
        store
            .add(Schedule {
                id: id.clone(),
                cron: "0 0 1 1 *".into(),
                cwd: work.path().to_path_buf(),
                prompt: "p".into(),
                timeout_secs: Some(5),
                enabled: true,
                watch_workspace: false, // <- the property under test
                ..Default::default()
            })
            .unwrap();
        store.save_to(&store_path).unwrap();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _manager =
                WatchManager::from_store_with_path(&store, fake.clone(), Some(store_path.clone()))
                    .expect("manager build");
            tokio::time::sleep(Duration::from_millis(300)).await;
            std::fs::write(work.path().join("hello.txt"), b"hi").unwrap();
            tokio::time::sleep(Duration::from_secs(4)).await;
        });

        let after = ScheduleStore::load_from(&store_path).unwrap();
        let s = after.get(&id).expect("schedule present");
        assert!(
            s.last_run.is_none(),
            "watch_workspace=false must skip the watcher (last_run = {:?})",
            s.last_run,
        );
    }

    #[test]
    fn schedule_serde_omits_watch_workspace_when_false() {
        // skip_serializing_if on the field keeps the JSON tidy: a
        // schedule that doesn't use the watch trigger doesn't carry
        // a noisy `"watchWorkspace": false` key.
        let mut s = Schedule {
            id: "x".into(),
            cron: "* * * * *".into(),
            cwd: std::env::temp_dir(),
            prompt: "p".into(),
            ..Default::default()
        };
        s.enabled = true;
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("watchWorkspace"), "got: {json}");
        s.watch_workspace = true;
        let json2 = serde_json::to_string(&s).unwrap();
        assert!(json2.contains("\"watchWorkspace\":true"), "got: {json2}");
    }

    #[test]
    fn compute_next_n_fires_returns_n() {
        let after = DateTime::parse_from_rfc3339("2026-05-06T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let fires = compute_next_n_fires("0 9 * * *", after, 3);
        assert_eq!(fires.len(), 3);
        assert_eq!(
            fires[0].format("%Y-%m-%dT%H:%M").to_string(),
            "2026-05-06T09:00"
        );
        assert_eq!(
            fires[1].format("%Y-%m-%dT%H:%M").to_string(),
            "2026-05-07T09:00"
        );
        assert_eq!(
            fires[2].format("%Y-%m-%dT%H:%M").to_string(),
            "2026-05-08T09:00"
        );
    }

    #[test]
    fn compute_next_n_fires_returns_empty_for_invalid_cron() {
        let after = Utc::now();
        assert!(compute_next_n_fires("nope", after, 3).is_empty());
    }

    #[test]
    fn compute_next_n_fires_zero_returns_empty() {
        let after = Utc::now();
        assert!(compute_next_n_fires("* * * * *", after, 0).is_empty());
    }

    #[test]
    fn compute_next_fire_handles_minute_cron() {
        // 8:30 every day. After 2026-05-06T08:00:00Z next fire is 08:30 same day.
        let after = DateTime::parse_from_rfc3339("2026-05-06T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = compute_next_fire("30 8 * * *", after).unwrap();
        assert_eq!(next.format("%H:%M").to_string(), "08:30");
        assert_eq!(next.format("%Y-%m-%d").to_string(), "2026-05-06");
    }

    #[test]
    fn compute_next_fire_returns_none_for_invalid() {
        let after = Utc::now();
        assert!(compute_next_fire("nope", after).is_none());
    }

    #[test]
    fn parse_last_run_returns_none_when_absent() {
        let s = Schedule {
            id: "x".into(),
            cron: "* * * * *".into(),
            cwd: std::env::temp_dir(),
            prompt: "p".into(),
            model: None,
            max_iterations: None,
            timeout_secs: None,
            enabled: true,
            watch_workspace: false,
            last_run: None,
            last_exit: None,
        };
        assert!(parse_last_run(&s).is_none());
    }

    #[test]
    fn parse_last_run_parses_rfc3339() {
        let s = Schedule {
            id: "x".into(),
            cron: "* * * * *".into(),
            cwd: std::env::temp_dir(),
            prompt: "p".into(),
            model: None,
            max_iterations: None,
            timeout_secs: None,
            enabled: true,
            watch_workspace: false,
            last_run: Some("2026-05-06T12:34:56Z".into()),
            last_exit: None,
        };
        let parsed = parse_last_run(&s).unwrap();
        assert_eq!(
            parsed.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "2026-05-06T12:34:56Z"
        );
    }

    /// Tick logic end-to-end: covers catch-up skipping (fresh
    /// schedule with no `lastRun`), firing on a due cron, cursor
    /// advancement (re-tick at same `now` doesn't re-fire), and
    /// disabled-schedule skipping. Uses an explicit store path so
    /// the test never touches `~/.config/thclaws/schedules.json`.
    #[cfg(unix)]
    #[test]
    fn tick_lifecycle_end_to_end() {
        // Fake binary: exits 0 fast.
        let bin_dir = tempfile::tempdir().unwrap();
        let fake = bin_dir.path().join("fake-thclaws");
        write_fake_executable(&fake, "#!/bin/sh\nexit 0");

        let store_dir = tempfile::tempdir().unwrap();
        let store_path = store_dir.path().join("schedules.json");
        let work = tempfile::tempdir().unwrap();
        let unique = uuid::Uuid::new_v4();
        let id_due = format!("due-{unique}");
        let id_fresh = format!("fresh-{unique}");
        let id_off = format!("off-{unique}");

        let two_min_ago = (Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
        let mut store = ScheduleStore::default();
        store
            .add(Schedule {
                id: id_due.clone(),
                cron: "* * * * *".into(),
                cwd: work.path().to_path_buf(),
                prompt: "p".into(),
                model: None,
                max_iterations: None,
                timeout_secs: Some(5),
                enabled: true,
                watch_workspace: false,
                last_run: Some(two_min_ago.clone()),
                last_exit: None,
            })
            .unwrap();
        store
            .add(Schedule {
                id: id_fresh.clone(),
                cron: "* * * * *".into(),
                cwd: work.path().to_path_buf(),
                prompt: "p".into(),
                model: None,
                max_iterations: None,
                timeout_secs: Some(5),
                enabled: true,
                watch_workspace: false,
                last_run: None,
                last_exit: None,
            })
            .unwrap();
        store
            .add(Schedule {
                id: id_off.clone(),
                cron: "* * * * *".into(),
                cwd: work.path().to_path_buf(),
                prompt: "p".into(),
                model: None,
                max_iterations: None,
                timeout_secs: Some(5),
                enabled: false,
                watch_workspace: false,
                last_run: Some(two_min_ago.clone()),
                last_exit: None,
            })
            .unwrap();
        store.save_to(&store_path).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut sched = InProcessScheduler::with_store_path(fake.clone(), store_path.clone());
            let now = Utc::now();
            let fired = sched.tick(now);
            // Only the due-and-enabled schedule fires; fresh
            // (skip-catch-up) and off (disabled) stay quiet.
            let ids: Vec<String> = fired.iter().map(|(id, _)| id.clone()).collect();
            assert_eq!(ids, vec![id_due.clone()]);

            // Drain the spawn_blocking task before asserting on
            // store state.
            for (_, handle) in fired {
                handle.await.unwrap();
            }

            // Cursor advances by exactly one cron period per tick.
            // For an every-minute schedule with a `lastRun` from
            // two minutes ago, a follow-up tick at the *same* `now`
            // will fire the next missed minute. Loop until no
            // further fires happen, then assert the cursor is
            // strictly past `now`.
            let mut total_fires = 1;
            loop {
                let more = sched.tick(now);
                if more.is_empty() {
                    break;
                }
                total_fires += more.len();
                for (_, handle) in more {
                    handle.await.unwrap();
                }
                assert!(total_fires < 10, "scheduler must not loop forever");
            }
        });

        // Verify the spawn updated the right store (lastRun set on
        // due, untouched on fresh and off).
        let after = ScheduleStore::load_from(&store_path).unwrap();
        let due = after.get(&id_due).unwrap();
        assert!(due.last_run.is_some(), "due schedule should have lastRun");
        assert_eq!(due.last_exit, Some(0));
        let fresh = after.get(&id_fresh).unwrap();
        assert!(
            fresh.last_run.is_none(),
            "fresh schedule should not have fired"
        );
        let off = after.get(&id_off).unwrap();
        assert_eq!(
            off.last_run.as_deref(),
            Some(two_min_ago.as_str()),
            "disabled schedule should be untouched"
        );

        for id in [&id_due, &id_fresh, &id_off] {
            if let Ok(d) = log_dir_for(id) {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn pid_alive_detects_self() {
        let me = std::process::id();
        assert!(pid_alive(me), "current process should be alive");
    }

    #[cfg(unix)]
    #[test]
    fn pid_alive_rejects_dead_pid() {
        // PID 1 is always alive (init), but a very high PID very
        // likely isn't allocated. The test relies on PID > pid_max
        // being unallocated; on Linux the default pid_max is
        // ~32768, on macOS ~99998. 0xFFFF_FFE0 is virtually
        // guaranteed to be unallocated.
        assert!(!pid_alive(u32::MAX - 32));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_plist_has_required_keys() {
        let plist = build_launchd_plist(
            std::path::Path::new("/usr/local/bin/thclaws"),
            std::path::Path::new("/Users/x/.local/share/thclaws/daemon.log"),
        );
        assert!(plist.contains("<string>sh.thclaws.scheduler</string>"));
        assert!(plist.contains("<string>/usr/local/bin/thclaws</string>"));
        assert!(plist.contains("<string>daemon</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(plist.contains("<key>KeepAlive</key>\n    <true/>"));
        assert!(plist.contains("<string>/Users/x/.local/share/thclaws/daemon.log</string>"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_unit_has_required_keys() {
        let unit = build_systemd_unit(
            std::path::Path::new("/usr/local/bin/thclaws"),
            std::path::Path::new("/home/x/.local/share/thclaws/daemon.log"),
        );
        assert!(unit.contains("ExecStart=/usr/local/bin/thclaws daemon"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("/home/x/.local/share/thclaws/daemon.log"));
    }

    /// Timeout enforcement: a job that sleeps longer than its
    /// `timeout_secs` should be killed and reported `timed_out=true`
    /// with no exit code.
    #[cfg(unix)]
    #[test]
    fn spawn_job_enforces_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("sleeper");
        write_fake_executable(&fake, "#!/bin/sh\nsleep 10\nexit 0");

        let work = tempfile::tempdir().unwrap();
        let schedule = Schedule {
            id: format!("timeout-test-{}", uuid::Uuid::new_v4()),
            cron: "* * * * *".into(),
            cwd: work.path().to_path_buf(),
            prompt: "p".into(),
            model: None,
            max_iterations: None,
            timeout_secs: Some(1),
            enabled: true,
            watch_workspace: false,
            last_run: None,
            last_exit: None,
        };
        let outcome = spawn_job(&schedule, &fake).unwrap();
        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, None);

        if let Ok(d) = log_dir_for(&schedule.id) {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
