//! Schedule store + run primitive (Step 1 of the scheduling feature).
//!
//! User-level store at `~/.config/thclaws/schedules.json` lists named
//! recurring jobs. Each job carries its own working directory, prompt,
//! optional model/iteration overrides, and a standard 5-field cron
//! expression. `run_once` fires a single job synchronously by spawning
//! `thclaws --print "<prompt>"` with `current_dir(<cwd>)`. The job's
//! clean output (stdout — the agent's final answer) is auto-saved into
//! the WORKSPACE at `<cwd>/.thclaws/schedule/<id>/<ts>.md` so the user
//! can open the result in the Files tab; stderr (the MCP banner,
//! warnings, errors) goes to a separate diagnostic log under
//! `~/.local/share/thclaws/logs/<id>/<ts>.log`. The exit code + duration
//! are recorded back into the schedule entry's `last_run` / `last_exit`.
//!
//! Step 1 is intentionally **without an in-process scheduler** — the
//! `run` subcommand fires a single job by id, so users can wire it
//! into crontab or launchd themselves and start getting value today.
//! The in-process tick (Step 2) and daemon (Step 3) reuse `run_once`
//! verbatim — only the trigger changes.

use crate::error::{Error, Result};
use chrono::{DateTime, Local, Utc};
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
    /// they `show` the entry. Empty for a one-shot schedule (see
    /// `run_at`), where it is ignored.
    pub cron: String,

    /// One-shot fire time as an RFC 3339 timestamp. When set, this
    /// entry is a one-off: it fires once at/after `run_at`, then
    /// auto-disables (`enabled = false`), and `cron` is ignored.
    /// Mutually exclusive with a non-empty `cron` at add time.
    ///
    /// Catch-up by design: a `run_at` already in the past when the
    /// scheduler ticks (e.g. the daemon was down over the slot) fires
    /// immediately rather than being lost — the one thing a bare cron
    /// expression can't express ("once on May 24 15:30" re-matches the
    /// following year, and a missed minute is gone until then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_at: Option<String>,

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
    /// dev-plan/heartbeat: chain fires into ONE session instead of a fresh
    /// amnesiac run each time. Passed to the spawned job as
    /// `--resume <value>`. Use `"last"` (recommended — resumes the cwd's
    /// most-recent session; the first fire starts it) or an existing
    /// session id. `None` keeps the classic stateless behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_session: Option<String>,

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
        crate::util::home_dir().map(|h| {
            h.join(format!(
                ".config/{}/schedules.json",
                crate::profile::app_dir_name()
            ))
        })
    }

    /// Load from the default user-level path. Returns a fresh empty
    /// store if the file doesn't exist yet.
    pub fn load() -> Result<Self> {
        match Self::default_path() {
            Some(p) => Self::load_from(&p),
            None => Ok(Self::default()),
        }
    }

    /// Earliest upcoming fire across every ENABLED schedule, or `None`
    /// when nothing is pending.
    ///
    /// Cloud runners report this to the control plane on their
    /// keepalive ping. A paused workspace has no process to fire a
    /// job — `pause()` scales the Deployment to 0 — so the reaper uses
    /// this to resume it in time. Disabled entries are skipped: a
    /// schedule the user turned off must not wake a pod.
    pub fn next_fire_across_all(&self, cursor: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.schedules
            .iter()
            .filter(|s| s.enabled)
            .filter_map(|s| next_fire(s, cursor))
            .min()
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
        validate_trigger(&schedule)?;
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

/// Validate a schedule's trigger at add time: exactly one of `cron`
/// or `run_at` must be set. A one-shot (`run_at` set) must carry a
/// parseable RFC 3339 timestamp and an empty `cron`; a recurring
/// entry must carry a valid cron expression and no `run_at`.
pub fn validate_trigger(schedule: &Schedule) -> Result<()> {
    let has_cron = !schedule.cron.trim().is_empty();
    match &schedule.run_at {
        Some(ts) => {
            if has_cron {
                return Err(Error::Config(
                    "a schedule is either recurring (--cron) or one-shot \
                     (--at/--in), not both"
                        .into(),
                ));
            }
            parse_run_at(ts).map(|_| ())
        }
        None => {
            if !has_cron {
                return Err(Error::Config(
                    "a schedule needs a trigger: pass --cron for recurring, \
                     or --at/--in for a one-shot"
                        .into(),
                ));
            }
            validate_cron(&schedule.cron)
        }
    }
}

/// Parse a one-shot `run_at` RFC 3339 timestamp into UTC. Accepts any
/// offset (`Z`, `+07:00`, …) and normalizes to UTC so the tick loop
/// compares against `Utc::now()`.
pub fn parse_run_at(ts: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts.trim())
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            Error::Config(format!(
                "invalid --at timestamp '{ts}': {e} (expected RFC 3339, \
                 e.g. 2026-05-24T15:30:00Z or 2026-05-24T22:30:00+07:00)"
            ))
        })
}

/// Parse a relative `--in` duration like `15m`, `2h`, `90s`, `1d`
/// into a `chrono::Duration`. A bare integer is treated as seconds.
/// Used to turn `--in <dur>` into an absolute `run_at = now + dur`.
pub fn parse_relative_duration(s: &str) -> Result<chrono::Duration> {
    let s = s.trim();
    let bad = || {
        Error::Config(format!(
            "invalid --in duration '{s}': use forms like 15m, 2h, 90s, 1d"
        ))
    };
    let (num, unit_secs) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1_i64),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3_600),
        Some('d') => (&s[..s.len() - 1], 86_400),
        Some(c) if c.is_ascii_digit() => (s, 1),
        _ => return Err(bad()),
    };
    let value: i64 = num.trim().parse().map_err(|_| bad())?;
    if value <= 0 {
        return Err(Error::Config(format!(
            "invalid --in duration '{s}': must be a positive amount of time"
        )));
    }
    value
        .checked_mul(unit_secs)
        .map(chrono::Duration::seconds)
        .ok_or_else(bad)
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
    /// The run's clean output (the agent's final answer) — captured
    /// stdout, written into the workspace at
    /// `<cwd>/.thclaws/schedule/<id>/<ts>.md` so the user can open it in
    /// the Files tab without digging into `~/.local/share`.
    pub result_path: PathBuf,
    /// Diagnostics for this run (captured stderr: MCP banner, warnings,
    /// errors) under `~/.local/share/thclaws/logs/<id>/<ts>.log`.
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
        // One-shots fire exactly once: disable after the first run so
        // the tick loop's `enabled` filter never re-fires it.
        if entry.run_at.is_some() {
            entry.enabled = false;
        }
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

    // The RESULT (stdout = the agent's final answer; print mode already
    // suppresses ANSI/thinking when not a TTY) is auto-saved into the
    // WORKSPACE so the user can view it in the Files tab regardless of
    // what the prompt was — no need to ask the agent to "save to a file".
    // Diagnostics (stderr: the MCP banner, warnings, errors) go to the
    // separate log under ~/.local/share so the result file stays clean.
    let result_dir = schedule
        .cwd
        .join(".thclaws")
        .join("state")
        .join("schedule")
        .join(&schedule.id);
    std::fs::create_dir_all(&result_dir)?;
    let result_path = result_dir.join(format!("{ts}.md"));
    let result_file = std::fs::File::create(&result_path)?;

    let mut cmd = Command::new(binary_path);
    cmd.arg("--print")
        .arg(&schedule.prompt)
        .current_dir(&schedule.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(result_file))
        .stderr(Stdio::from(log_file));
    if let Some(ref m) = schedule.model {
        cmd.arg("--model").arg(m);
    }
    // Heartbeat: continue the same session across fires. Print mode now
    // persists sessions + honors --resume, so history accumulates.
    if let Some(ref sid) = schedule.resume_session {
        cmd.arg("--resume").arg(sid);
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
        result_path,
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

/// Newest diagnostic log for a schedule, if any. Each fire writes one
/// timestamped `<ts>.log` (its stderr — where the actual error is), so the
/// lexicographically-largest name is the most recent fire's log. Used by
/// `schedule status` to point the user at the error behind an `err`.
pub fn latest_log(id: &str) -> Option<PathBuf> {
    let dir = log_dir_for(id).ok()?;
    std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .max_by(|a, b| a.file_name().cmp(&b.file_name()))
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

/// Format a last-run timestamp for display in the local timezone.
/// Returns `never` when the field is absent and preserves the raw
/// string if parsing fails.
pub fn display_last_run(last_run: Option<&str>) -> String {
    last_run
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|| last_run.unwrap_or("never").to_string())
}

/// First cron fire strictly after `after`. Returns `None` if the
/// expression is invalid or has no upcoming fire (rare — e.g. a
/// `cron` expression pinned to a specific year that's already past).
pub fn compute_next_fire(cron_expr: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let normalized = normalize_cron(cron_expr);
    let schedule = cron::Schedule::from_str(&normalized).ok()?;
    schedule
        .after(&after.with_timezone(&Local))
        .next()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Next time `schedule` is due relative to `cursor`, unifying the
/// recurring and one-shot cases for the tick loop.
///
/// - One-shot (`run_at` set): returns the `run_at` instant until the
///   job has fired (`last_run` recorded), then `None`. The `cursor`
///   is ignored — a one-shot's due time is absolute, so a `run_at`
///   in the past returns immediately (caught up) rather than being
///   skipped past.
/// - Recurring: delegates to `compute_next_fire(cron, cursor)`.
pub fn next_fire(schedule: &Schedule, cursor: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if let Some(run_at) = &schedule.run_at {
        if schedule.last_run.is_some() {
            return None; // one-shot already fired
        }
        return parse_run_at(run_at).ok();
    }
    compute_next_fire(&schedule.cron, cursor)
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
    schedule
        .after(&after.with_timezone(&Local))
        .take(n)
        .map(|dt| dt.with_timezone(&Utc))
        .collect()
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
    /// Ids already warned about for a missing cwd, so the warning is
    /// once per process rather than once per tick.
    warned_missing_cwd: HashSet<String>,
}

impl InProcessScheduler {
    pub fn new(binary: PathBuf) -> Self {
        Self {
            cursors: HashMap::new(),
            running: Arc::new(Mutex::new(HashSet::new())),
            binary,
            store_path: None,
            warned_missing_cwd: HashSet::new(),
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
            warned_missing_cwd: HashSet::new(),
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

            let Some(next) = next_fire(schedule, cursor) else {
                continue;
            };
            if next > now {
                continue;
            }

            // A schedule whose cwd is gone can never run, and `run_once_with`
            // rejects it before recording the attempt — so `lastRun` stays
            // frozen and the backlog is replayed in full by every process that
            // starts, one failed fire per tick, growing by one per day. Park
            // the cursor at `now` instead: no walk, and nothing to replay.
            if !schedule.cwd.exists() {
                if self.warned_missing_cwd.insert(schedule.id.clone()) {
                    eprintln!(
                        "\x1b[33m[schedule] '{}': cwd does not exist, skipping: {}\x1b[0m",
                        schedule.id,
                        schedule.cwd.display()
                    );
                }
                self.cursors.insert(schedule.id.clone(), now);
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
                             — exit={exit} duration={}.{:03}s → result={} (log={})\x1b[0m",
                            outcome.duration.as_secs(),
                            outcome.duration.subsec_millis(),
                            outcome.result_path.display(),
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
/// dev-plan/60 G1: whether this process ticks the in-process scheduler.
///
/// The store is user-level (`$HOME/.config/thclaws/schedules.json`) and holds
/// every workspace's schedules, so exactly one process per HOME should tick
/// it. Under a workspace host that process is the host. Its agents run with
/// `HOME=<agent>/.home`, where the store is empty — which is how a workspace
/// that became multi-agent silently stopped firing its schedules — and a
/// second ticker over the real store would fire each schedule twice.
pub fn in_process_scheduler_wanted(
    print: bool,
    no_scheduler: bool,
    supervised_child: bool,
) -> bool {
    !print && !no_scheduler && !supervised_child
}

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
    /// Closing this drops the debouncers on the watch thread, which is
    /// what stops watching. The `Vec` used to live here; it moved one
    /// thread over so a slow `watch()` delays that thread rather than
    /// whoever built the manager.
    _stop: Option<std::sync::mpsc::Sender<()>>,
    /// Fires once every watcher has been bound. Nothing in the engine
    /// waits on it — a schedule that starts watching a second late is
    /// no worse than one that starts a second late for any other reason
    /// — but a test that writes a file and expects a fire has to.
    #[allow(dead_code)] // read only through wait_until_armed, i.e. by tests
    armed: Option<std::sync::mpsc::Receiver<()>>,
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
    /// Block until every watcher is bound, or `timeout` passes.
    ///
    /// Returns whether they armed in time. Nothing in the engine needs
    /// this — watching a moment late is harmless — but a test that
    /// writes a file and expects a fire is racing the bind, and on a
    /// dev binary that bind has been measured at 40-50 seconds.
    #[allow(dead_code)] // tests only; kept out of #[cfg(test)] so it compiles in both
    pub(crate) fn wait_until_armed(&self, timeout: std::time::Duration) -> bool {
        match &self.armed {
            None => true, // nothing to arm
            Some(rx) => rx.recv_timeout(timeout).is_ok(),
        }
    }

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
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(String, PathBuf)>();
        let last_fire: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
        let running: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

        // Which directories we intend to watch. Collected before any
        // watcher is built so the decision to start a dispatcher does
        // not depend on work that now happens on another thread.
        let mut targets: Vec<(String, PathBuf)> = Vec::new();
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
            targets.push((id, cwd));
        }

        // Binding the watchers happens on its own thread, and that is
        // load-bearing rather than tidiness.
        //
        // `notify`'s macOS backend starts a CFRunLoop thread and
        // `watch()` blocks on a channel until that thread reports
        // itself ready; when the run loop never gets going, `watch()`
        // neither returns nor errors. Called inline — as it was — that
        // parks whoever is building the manager. The same shape parked
        // the settings watcher and took the whole `--lib` suite with
        // it; here it parked `watch_manager_fires_on_file_change` and
        // `watch_manager_ignores_internal_thclaws_writes` forever,
        // while `watch_manager_skips_watch_workspace_false` — the one
        // that never reaches this code — passed in milliseconds.
        //
        // The thread owns the debouncers, because dropping them is what
        // stops watching. `stop_tx` lives in the manager: when the
        // manager is dropped the channel closes, the recv below
        // returns, and the debouncers go with it — the same lifetime
        // the `Vec` field used to give, moved one thread over. If
        // `watch()` never returns, this thread is what waits instead of
        // the caller.
        let (stop_tx, armed_rx) = if targets.is_empty() {
            (None, None)
        } else {
            let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
            // Fires once every watcher is bound. Measured 2026-09-22:
            // binding takes 10ms in a small binary and 40-50s in this
            // crate's unsigned 200MB macOS test binary (40.5s and 49.5s
            // on an idle machine, re-measured after a load artefact was
            // ruled out) — the system appears to vet the image before
            // it will register an FSEvents stream. That is why the
            // inline version looked like a hang: it was one, for the
            // better part of a minute per watcher, on the caller's
            // thread.
            //
            // Nothing in the engine waits on this. A schedule that
            // starts watching a moment late is no worse than one that
            // starts late for any other reason, and blocking startup on
            // a system service is how the old shape earned its
            // reputation. Tests that write a file and expect a fire do
            // have to wait, and [`WatchManager::wait_until_armed`] is
            // for them.
            let (armed_tx, armed_rx) = std::sync::mpsc::channel::<()>();
            // A clone, so the original `tx` stays here to be dropped by
            // the no-dispatcher branch below — the compiler cannot see
            // that the two are mutually exclusive.
            let tx_for_thread = tx.clone();
            let spawned = std::thread::Builder::new()
                .name("watch-setup".into())
                .spawn(move || {
                    let mut debouncers = Vec::new();
                    for (id, cwd) in targets {
                        let id_for_handler = id.clone();
                        let cwd_for_handler = cwd.clone();
                        let tx_for_handler = tx_for_thread.clone();
                        let mut debouncer = match new_debouncer(WATCH_DEBOUNCE, move |result| {
                            match result {
                                Ok(events) => {
                                    let events: Vec<notify_debouncer_mini::DebouncedEvent> = events;
                                    for ev in events {
                                        if is_path_ignored(&cwd_for_handler, &ev.path) {
                                            continue;
                                        }
                                        let _ = tx_for_handler
                                            .send((id_for_handler.clone(), ev.path.clone()));
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
                                eprintln!(
                                    "\x1b[31m[watch] '{id}': could not start watcher: {e}\x1b[0m"
                                );
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
                    let _ = armed_tx.send(());
                    // Hold them until the manager goes away.
                    let _ = stop_rx.recv();
                    drop(debouncers);
                });
            match spawned {
                Ok(_) => (Some(stop_tx), Some(armed_rx)),
                Err(e) => {
                    eprintln!("\x1b[33m[watch] could not start watch thread: {e}\x1b[0m");
                    (None, None)
                }
            }
        };

        // Dispatcher task: consumes events, applies cooldown +
        // skip-overlap, and fires `run_once` on the blocking pool
        // for any event that survives both gates.
        let dispatch_handle = if stop_tx.is_some() {
            Some(spawn_dispatcher(
                rx,
                binary.clone(),
                store_path.clone(),
                last_fire,
                running,
            ))
        } else {
            // Drop the unused tx so the rx side closes cleanly when
            // the manager goes away — no dispatcher task to spawn.
            drop(tx);
            None
        };

        Ok(Self {
            _stop: stop_tx,
            armed: armed_rx,
            dispatch_handle,
        })
    }
}

/// Consume debounced watch events and fire the schedules they name.
///
/// Its own function so it can be driven without a filesystem. What it
/// decides — cooldown, skip-while-running, and that a fire actually runs
/// the binary — is ordinary logic worth testing on every machine; that
/// macOS will hand this process an FSEvents stream is not, and mixing
/// the two is how the only tests of this logic came to depend on a
/// system service that takes 46 seconds here and 10ms elsewhere.
fn spawn_dispatcher(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<(String, PathBuf)>,
    binary_for_task: PathBuf,
    store_path_for_task: Option<PathBuf>,
    last_fire: Arc<Mutex<HashMap<String, Instant>>>,
    running: Arc<Mutex<HashSet<String>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
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
                                "\x1b[36m[watch] '{id_for_blocking}' done — exit={exit} duration={}.{:03}s → result={} (log={})\x1b[0m",
                                o.duration.as_secs(),
                                o.duration.subsec_millis(),
                                o.result_path.display(),
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

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
    fn the_scheduler_runs_in_the_host_not_in_its_agents() {
        // A desktop window, a plain --serve, and a workspace host all tick it.
        assert!(in_process_scheduler_wanted(false, false, false));
        // An agent under a host does not: its HOME is its own folder, and the
        // host already ticks the real store.
        assert!(!in_process_scheduler_wanted(false, false, true));
        // -p and --no-scheduler never did.
        assert!(!in_process_scheduler_wanted(true, false, false));
        assert!(!in_process_scheduler_wanted(false, true, false));
    }

    #[test]
    fn next_fire_across_all_skips_disabled_and_takes_the_earliest() {
        // Every assertion compares against `next_fire()` outputs rather
        // than wall-clock numbers, and no assertion assumes which cron
        // lands first. Cron is evaluated in LOCAL time, so "which of
        // these two times comes first" genuinely differs by machine —
        // an earlier version of this test pinned an hour, and a later
        // one pinned an ordering, and BOTH passed in Bangkok and failed
        // on CI in UTC.
        let mk = |id: &str, cron: &str, enabled: bool| {
            let mut s = Schedule::default();
            s.id = id.into();
            s.cron = cron.into();
            s.enabled = enabled;
            s
        };
        let cursor = Utc.with_ymd_and_hms(2026, 9, 10, 1, 0, 0).unwrap();

        let mut store = ScheduleStore::default();
        assert!(
            store.next_fire_across_all(cursor).is_none(),
            "an empty store must report nothing pending, not a wake-up"
        );

        // Disabled only: nothing pending. A schedule the user switched
        // off must never wake a paused workspace.
        store.schedules.push(mk("off", "30 0 * * *", false));
        assert!(
            store.next_fire_across_all(cursor).is_none(),
            "a disabled schedule must not count as pending"
        );

        // Add two enabled ones. The answer is the earlier of their own
        // computed fires — whichever that turns out to be here.
        let a = mk("a", "0 2 * * *", true);
        let b = mk("b", "0 3 * * *", true);
        let a_at = next_fire(&a, cursor).unwrap();
        let b_at = next_fire(&b, cursor).unwrap();
        store.schedules.push(a);
        store.schedules.push(b);
        assert_eq!(
            store.next_fire_across_all(cursor),
            Some(a_at.min(b_at)),
            "earliest ENABLED schedule wins"
        );

        // Enabling the third must move the answer only if it is
        // genuinely earlier — stated as a min() over all three so it
        // holds in any zone.
        let off_at = next_fire(&mk("off", "30 0 * * *", true), cursor).unwrap();
        store.schedules[0].enabled = true;
        assert_eq!(
            store.next_fire_across_all(cursor),
            Some(a_at.min(b_at).min(off_at)),
            "enabling a schedule brings it into consideration"
        );
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

    // ─── one-shot / relative-delay schedules ───

    #[test]
    fn validate_trigger_enforces_exactly_one() {
        let cron_only = Schedule {
            cron: "* * * * *".into(),
            ..Default::default()
        };
        assert!(validate_trigger(&cron_only).is_ok());

        let one_shot = Schedule {
            cron: String::new(),
            run_at: Some("2026-05-24T15:30:00Z".into()),
            ..Default::default()
        };
        assert!(validate_trigger(&one_shot).is_ok());

        // Both set → rejected.
        let both = Schedule {
            cron: "* * * * *".into(),
            run_at: Some("2026-05-24T15:30:00Z".into()),
            ..Default::default()
        };
        assert!(validate_trigger(&both).is_err());

        // Neither set → rejected.
        let neither = Schedule {
            cron: String::new(),
            ..Default::default()
        };
        assert!(validate_trigger(&neither).is_err());

        // run_at set but unparseable → rejected.
        let bad_at = Schedule {
            run_at: Some("tomorrow morning".into()),
            ..Default::default()
        };
        assert!(validate_trigger(&bad_at).is_err());
    }

    #[test]
    fn parse_run_at_accepts_rfc3339_offsets() {
        let z = parse_run_at("2026-05-24T15:30:00Z").unwrap();
        let off = parse_run_at("2026-05-24T22:30:00+07:00").unwrap();
        // Both denote the same instant, normalized to UTC.
        assert_eq!(z, off);
        assert!(parse_run_at("not a time").is_err());
    }

    #[test]
    fn parse_relative_duration_units() {
        use chrono::Duration;
        assert_eq!(
            parse_relative_duration("90s").unwrap(),
            Duration::seconds(90)
        );
        assert_eq!(
            parse_relative_duration("15m").unwrap(),
            Duration::minutes(15)
        );
        assert_eq!(parse_relative_duration("2h").unwrap(), Duration::hours(2));
        assert_eq!(parse_relative_duration("1d").unwrap(), Duration::days(1));
        // Bare integer = seconds.
        assert_eq!(
            parse_relative_duration("45").unwrap(),
            Duration::seconds(45)
        );
        // Junk / non-positive rejected.
        assert!(parse_relative_duration("soon").is_err());
        assert!(parse_relative_duration("0m").is_err());
        assert!(parse_relative_duration("-5m").is_err());
    }

    #[test]
    fn next_fire_one_shot_fires_once_and_catches_up() {
        let now = Utc::now();
        // A run_at already in the past is still "due" — next_fire
        // returns it so the tick loop (next <= now) fires immediately
        // rather than losing the slot (the catch-up property).
        let past = (now - chrono::Duration::hours(1)).to_rfc3339();
        let pending = Schedule {
            run_at: Some(past.clone()),
            ..Default::default()
        };
        let next = next_fire(&pending, now).expect("pending one-shot is due");
        assert!(next <= now, "past run_at should be due now");

        // Once it has fired (last_run set), it never fires again.
        let fired = Schedule {
            run_at: Some(past),
            last_run: Some(now.to_rfc3339()),
            ..Default::default()
        };
        assert!(next_fire(&fired, now).is_none());
    }

    #[test]
    fn next_fire_recurring_delegates_to_cron() {
        let now = Utc::now();
        let recurring = Schedule {
            cron: "* * * * *".into(),
            ..Default::default()
        };
        // Delegates to compute_next_fire — a once-a-minute cron always
        // has a next fire after `now`.
        assert!(next_fire(&recurring, now).is_some());
    }

    #[test]
    fn one_shot_serde_omits_field_when_absent_and_old_files_parse() {
        // A recurring schedule serializes without a runAt key.
        let recurring = Schedule {
            id: "r".into(),
            cron: "* * * * *".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&recurring).unwrap();
        assert!(!json.contains("runAt"), "absent run_at must be skipped");

        // An old schedules.json with no runAt field still deserializes.
        let legacy = r#"{"id":"r","cron":"* * * * *","cwd":"/tmp","prompt":"hi","enabled":true}"#;
        let back: Schedule = serde_json::from_str(legacy).unwrap();
        assert!(back.run_at.is_none());

        // A one-shot round-trips with a camelCase runAt key.
        let one_shot = Schedule {
            id: "o".into(),
            run_at: Some("2026-05-24T15:30:00Z".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&one_shot).unwrap();
        assert!(json.contains("\"runAt\":\"2026-05-24T15:30:00Z\""));
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
                resume_session: None,
                max_iterations: Some(20),
                timeout_secs: Some(60),
                enabled: true,
                watch_workspace: false,
                last_run: None,
                last_exit: None,
                run_at: None,
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
            resume_session: None,
            max_iterations: None,
            timeout_secs: None,
            enabled: true,
            watch_workspace: false,
            last_run: None,
            last_exit: None,
            run_at: None,
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
            resume_session: None,
            max_iterations: None,
            timeout_secs: None,
            enabled: true,
            watch_workspace: false,
            last_run: None,
            last_exit: None,
            run_at: None,
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
                resume_session: None,
                max_iterations: None,
                timeout_secs: None,
                enabled: true,
                watch_workspace: false,
                last_run: None,
                last_exit: None,
                run_at: None,
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
        // `log_dir_for` reads `$HOME` to place the log file. Concurrent
        // tests in tools/{memory,kms} + research/* + kms.rs flip HOME
        // to tempdirs they then drop on guard release — racing those
        // would leave `outcome.log_path` pointing at a deleted dir.
        let _env = crate::kms::test_env_lock();
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
            resume_session: None,
            max_iterations: None,
            timeout_secs: Some(5),
            enabled: true,
            watch_workspace: false,
            last_run: None,
            last_exit: None,
            run_at: None,
        };
        let outcome = spawn_job(&schedule, &fake).unwrap();
        assert_eq!(outcome.exit_code, Some(7));
        assert!(!outcome.timed_out);
        // The fake echoes pwd + the prompt to STDOUT, which is now the
        // auto-saved RESULT file in the workspace (.thclaws/schedule/...).
        let result = std::fs::read_to_string(&outcome.result_path).unwrap();
        assert!(
            outcome
                .result_path
                .starts_with(work.path().join(".thclaws").join("state").join("schedule")),
            "result must be saved in the workspace; got: {}",
            outcome.result_path.display()
        );
        // pwd in the spawned shell should match `work.path()` after
        // canonicalization (macOS adds `/private` to `/var/folders/...`).
        let canonical = work.path().canonicalize().unwrap();
        assert!(
            result.contains(canonical.to_string_lossy().trim_end_matches('/'))
                || result.contains(work.path().to_string_lossy().trim_end_matches('/')),
            "result should contain cwd; got: {result}"
        );
        assert!(result.contains("prompt-was: hello there"));

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
    /// The dispatcher fires the schedule an event names.
    ///
    /// No filesystem: the event is pushed straight into the channel the
    /// watcher callback would have used. Everything this asserts — that
    /// a fire runs the binary, records `last_run` and `last_exit`, and
    /// saves the store — is ours, and now runs on every machine.
    #[cfg(unix)]
    /// Poll the store until the schedule records a run, up to `secs`.
    /// A flat sleep is a coin toss on a loaded machine; this waits only
    /// as long as it has to and fails loudly if the run never lands.
    /// Poll a closure until it yields a value. Bounded so a genuine failure
    /// still ends the test, but the bound is a backstop, not the mechanism —
    /// what is being waited on is an observable the dispatcher itself sets,
    /// not a side effect that has to travel through a spawned process and the
    /// filesystem first.
    async fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> Option<T> {
        for _ in 0..600 {
            if let Some(v) = probe() {
                return Some(v);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }

    async fn wait_for_last_run(path: &Path, id: &str, secs: u64) -> Option<String> {
        for _ in 0..(secs * 10) {
            if let Ok(store) = ScheduleStore::load_from(path) {
                if let Some(run) = store.get(id).and_then(|s| s.last_run.clone()) {
                    return Some(run);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }

    #[cfg(unix)]
    #[test]
    fn a_watch_event_fires_the_schedule() {
        let bin_dir = tempfile::tempdir().unwrap();
        let fake = bin_dir.path().join("fake-thclaws");
        write_fake_executable(&fake, "#!/bin/sh\nexit 0");

        let work = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store_path = store_dir.path().join("schedules.json");
        let id = format!("disp-{}", uuid::Uuid::new_v4());

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
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(String, PathBuf)>();
            let _h = spawn_dispatcher(
                rx,
                fake.clone(),
                Some(store_path.clone()),
                Arc::new(Mutex::new(HashMap::new())),
                Arc::new(Mutex::new(HashSet::new())),
            );
            tx.send((id.clone(), work.path().join("notes/hello.txt")))
                .unwrap();
            assert!(
                wait_for_last_run(&store_path, &id, 30).await.is_some(),
                "the event should have fired the schedule"
            );
        });

        let after = ScheduleStore::load_from(&store_path).unwrap();
        let s = after.get(&id).expect("schedule present");
        assert!(s.last_run.is_some(), "the event should have fired it");
        assert_eq!(s.last_exit, Some(0), "fake binary exits 0");
        if let Ok(d) = log_dir_for(&id) {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// A second event inside the cooldown does not fire again.
    ///
    /// The agent's own writes back into `cwd` are what this protects
    /// against: without it a fire triggers the next one forever.
    #[cfg(unix)]
    #[test]
    fn a_second_event_inside_the_cooldown_is_dropped() {
        let bin_dir = tempfile::tempdir().unwrap();
        let fake = bin_dir.path().join("fake-thclaws");
        write_fake_executable(&fake, "#!/bin/sh\nexit 0");

        let work = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store_path = store_dir.path().join("schedules.json");
        let id = format!("cool-{}", uuid::Uuid::new_v4());
        // A second schedule used only as a marker. The channel is FIFO and the
        // dispatcher is one loop, so once the marker has been stamped, the
        // event before it has certainly been through the cooldown decision —
        // a happens-before, where the old test used a sleep.
        let marker = format!("mark-{}", uuid::Uuid::new_v4());

        let mut store = ScheduleStore::default();
        for sid in [&id, &marker] {
            store
                .add(Schedule {
                    id: sid.clone(),
                    cron: "0 0 1 1 *".into(),
                    cwd: work.path().to_path_buf(),
                    prompt: "p".into(),
                    timeout_secs: Some(5),
                    enabled: true,
                    watch_workspace: true,
                    ..Default::default()
                })
                .unwrap();
        }
        store.save_to(&store_path).unwrap();

        // The cooldown lives in this map, so assert on it rather than on
        // `last_run` in the store — that only appears after a spawned child
        // has run and written the file, and waiting for it lost the race on a
        // loaded CI runner and failed the whole suite on the v0.135.0 commit.
        // Nothing about "a second event inside the cooldown is dropped" needs
        // the child to finish; `a_watch_event_fires_the_schedule` covers that.
        let last_fire: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
        let stamp = |who: &str| -> Option<Instant> {
            last_fire.lock().expect("last_fire lock").get(who).copied()
        };

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(String, PathBuf)>();
            let _h = spawn_dispatcher(
                rx,
                fake.clone(),
                Some(store_path.clone()),
                last_fire.clone(),
                Arc::new(Mutex::new(HashSet::new())),
            );

            tx.send((id.clone(), work.path().join("a.txt"))).unwrap();
            let first = wait_for(|| stamp(&id))
                .await
                .expect("the first event should have fired");

            // The second event, then the marker behind it.
            tx.send((id.clone(), work.path().join("b.txt"))).unwrap();
            tx.send((marker.clone(), work.path().join("m.txt")))
                .unwrap();
            wait_for(|| stamp(&marker))
                .await
                .expect("the marker event should have fired");

            assert_eq!(
                stamp(&id),
                Some(first),
                "a second event inside the cooldown must not re-arm it"
            );
        });

        for sid in [&id, &marker] {
            if let Ok(d) = log_dir_for(sid) {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    }

    /// Real FSEvents/inotify, end to end. `#[ignore]` because
    /// registering a stream is instant on Linux and measured at 46
    /// SECONDS in this crate's unsigned macOS dev binary — the
    /// system appears to vet the image first — which made this the
    /// test that hung the suite for ten days. What it covers that
    /// the deterministic tests cannot is only that the OS delivers
    /// the event at all. Run it deliberately:
    ///   cargo test --lib -- --ignored watch_manager_fires_on_file_change
    #[cfg(unix)]
    #[test]
    #[ignore = "real FSEvents: ~45s to bind in this binary; run with --ignored"]
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

            // Wait for the bind rather than assume it. The old fixed
            // 300ms was a coin flip this binary always lost — the write
            // landed before anything was watching. Registering an
            // FSEvents stream is instant on Linux (CI, inotify) and
            // slow enough here to be the reason this test is
            // `#[ignore]`d; the elapsed time is printed because it is
            // the one number anyone re-running this wants.
            let t0 = Instant::now();
            let armed =
                tokio::task::block_in_place(|| _manager.wait_until_armed(Duration::from_secs(180)));
            eprintln!(
                "[test] watcher bind: armed={armed} after {:.1}s",
                t0.elapsed().as_secs_f32()
            );
            assert!(armed, "the watcher never bound; nothing below can pass");

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
    /// Real FSEvents/inotify, end to end. `#[ignore]` because
    /// registering a stream is instant on Linux and measured at 46
    /// SECONDS in this crate's unsigned macOS dev binary — the
    /// system appears to vet the image first — which made this the
    /// test that hung the suite for ten days. What it covers that
    /// the deterministic tests cannot is only that the OS delivers
    /// the event at all. Run it deliberately:
    ///   cargo test --lib -- --ignored watch_manager_ignores_internal_thclaws_writes
    #[cfg(unix)]
    #[test]
    #[ignore = "real FSEvents: ~45s to bind in this binary; run with --ignored"]
    fn watch_manager_ignores_internal_thclaws_writes() {
        let bin_dir = tempfile::tempdir().unwrap();
        let fake = bin_dir.path().join("fake-thclaws");
        write_fake_executable(&fake, "#!/bin/sh\nexit 0");

        let work = tempfile::tempdir().unwrap();
        // Pre-create the .thclaws directory so the file write below
        // is observed as a child of an existing dir (some platforms
        // emit different events for create-dir vs create-in-dir).
        let thclaws_dir = work.path().join(".thclaws").join("state").join("sessions");
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
            // Registering an FSEvents stream is instant on Linux (CI,
            // inotify) and slow enough here to be the reason this test
            // is `#[ignore]`d.
            let t0 = Instant::now();
            let armed =
                tokio::task::block_in_place(|| _manager.wait_until_armed(Duration::from_secs(180)));
            eprintln!(
                "[test] watcher bind: armed={armed} after {:.1}s",
                t0.elapsed().as_secs_f32()
            );
            assert!(armed, "the watcher never bound; nothing below can pass");

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
        assert!(fires.windows(2).all(|pair| pair[0] < pair[1]));
        for fire in fires {
            assert_eq!(
                fire.with_timezone(&Local).format("%H:%M").to_string(),
                "09:00"
            );
        }
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
        // 8:30 every day in local time.
        let after = DateTime::parse_from_rfc3339("2026-05-06T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = compute_next_fire("30 8 * * *", after).unwrap();
        assert_eq!(
            next.with_timezone(&Local).format("%H:%M").to_string(),
            "08:30"
        );
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
            resume_session: None,
            max_iterations: None,
            timeout_secs: None,
            enabled: true,
            watch_workspace: false,
            last_run: None,
            last_exit: None,
            run_at: None,
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
            resume_session: None,
            max_iterations: None,
            timeout_secs: None,
            enabled: true,
            watch_workspace: false,
            last_run: Some("2026-05-06T12:34:56Z".into()),
            last_exit: None,
            run_at: None,
        };
        let parsed = parse_last_run(&s).unwrap();
        assert_eq!(
            parsed.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "2026-05-06T12:34:56Z"
        );
    }

    #[test]
    fn display_last_run_formats_local_time_and_preserves_instant() {
        let displayed = display_last_run(Some("2026-05-06T12:34:56Z"));
        let parsed = DateTime::parse_from_rfc3339(&displayed).unwrap();
        assert_eq!(
            parsed.with_timezone(&Utc).to_rfc3339(),
            "2026-05-06T12:34:56+00:00"
        );
    }

    #[test]
    fn display_last_run_handles_absent_and_invalid_values() {
        assert_eq!(display_last_run(None), "never");
        assert_eq!(display_last_run(Some("bad-timestamp")), "bad-timestamp");
    }

    /// A schedule whose cwd is gone must be skipped outright, never
    /// walked: `run_once_with` rejects it before recording the attempt,
    /// so its `lastRun` never advances and the whole backlog replays on
    /// every start. Uses an explicit store path so the test never
    /// touches `~/.config/thclaws/schedules.json`.
    #[cfg(unix)]
    #[test]
    fn tick_skips_a_schedule_whose_cwd_is_gone() {
        let bin_dir = tempfile::tempdir().unwrap();
        let fake = bin_dir.path().join("fake-thclaws");
        write_fake_executable(&fake, "#!/bin/sh\nexit 0");

        let store_dir = tempfile::tempdir().unwrap();
        let store_path = store_dir.path().join("schedules.json");

        // A cwd that existed when the schedule was added and is gone now.
        let gone = tempfile::tempdir().unwrap();
        let gone_path = gone.path().to_path_buf();
        drop(gone);

        // A week of missed daily fires. `run_once_with` rejects each one on
        // the cwd check before it can stamp lastRun, so without the skip the
        // cursor walks the backlog a fire per tick — and every restart walks
        // it again from the same frozen timestamp.
        let week_ago = (Utc::now() - chrono::Duration::days(7)).to_rfc3339();
        let id = format!("gone-{}", uuid::Uuid::new_v4());
        let mut store = ScheduleStore::default();
        store
            .add(Schedule {
                id: id.clone(),
                cron: "0 6 * * *".into(),
                cwd: gone_path,
                prompt: "p".into(),
                model: None,
                resume_session: None,
                max_iterations: None,
                timeout_secs: Some(5),
                enabled: true,
                watch_workspace: false,
                last_run: Some(week_ago),
                last_exit: None,
                run_at: None,
            })
            .unwrap();
        store.save_to(&store_path).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut sched = InProcessScheduler::with_store_path(fake.clone(), store_path.clone());
            for _ in 0..3 {
                assert!(
                    sched.tick(Utc::now()).is_empty(),
                    "a schedule whose cwd is gone must never fire"
                );
            }
        });

        // And it stayed untouched on disk: no attempt was recorded.
        let after = ScheduleStore::load_from(&store_path).unwrap();
        assert_eq!(after.get(&id).unwrap().last_exit, None);
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
                resume_session: None,
                max_iterations: None,
                timeout_secs: Some(5),
                enabled: true,
                watch_workspace: false,
                last_run: Some(two_min_ago.clone()),
                last_exit: None,
                run_at: None,
            })
            .unwrap();
        store
            .add(Schedule {
                id: id_fresh.clone(),
                cron: "* * * * *".into(),
                cwd: work.path().to_path_buf(),
                prompt: "p".into(),
                model: None,
                resume_session: None,
                max_iterations: None,
                timeout_secs: Some(5),
                enabled: true,
                watch_workspace: false,
                last_run: None,
                last_exit: None,
                run_at: None,
            })
            .unwrap();
        store
            .add(Schedule {
                id: id_off.clone(),
                cron: "* * * * *".into(),
                cwd: work.path().to_path_buf(),
                prompt: "p".into(),
                model: None,
                resume_session: None,
                max_iterations: None,
                timeout_secs: Some(5),
                enabled: false,
                watch_workspace: false,
                last_run: Some(two_min_ago.clone()),
                last_exit: None,
                run_at: None,
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
            resume_session: None,
            max_iterations: None,
            timeout_secs: Some(1),
            enabled: true,
            watch_workspace: false,
            last_run: None,
            last_exit: None,
            run_at: None,
        };
        let outcome = spawn_job(&schedule, &fake).unwrap();
        // Say why, if this ever fails. It was reported failing once
        // inside a loaded full-suite run and could not be reproduced
        // afterwards — 24 runs, idle and under a load average of ten on
        // eight cores, all passed. A bare `assert!` told us nothing
        // about which of the two plausible causes it was, so these carry
        // the outcome: a `timed_out: false` with an exit code means the
        // child could not be started (the script is `sleep 10` against a
        // one-second deadline, so it cannot have finished in time),
        // while a slow deadline would show up in the duration.
        assert!(
            outcome.timed_out,
            "expected the deadline to fire; got exit_code={:?} after {:?} \
             — a non-None exit code here means the child could not be started, \
             not that the timeout was too tight",
            outcome.exit_code, outcome.duration
        );
        assert_eq!(
            outcome.exit_code, None,
            "a killed child reports no exit code; duration was {:?}",
            outcome.duration
        );

        if let Ok(d) = log_dir_for(&schedule.id) {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
