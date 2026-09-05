//! Agent Teams — filesystem-based coordination between multiple thClaws instances.
//!
//! Architecture aligned with Claude Code:
//! - **Mailbox**: single JSON array per agent inbox with `read` tracking + file locking
//! - **Task queue**: filesystem-persisted tasks with claim/complete/dependency tracking
//! - **Protocol messages**: typed structured messages (idle_notification, shutdown, etc.)
//! - **Polling**: 1-second interval for message delivery
//!
//! Layout (workspace v2 — team state is runtime, under `state/`):
//!   .thclaws/state/team/config.json          — team config (members, lead, etc.)
//!   .thclaws/state/team/inboxes/{name}.json  — per-agent inbox (JSON array)
//!   .thclaws/state/team/tasks/{id}.json      — per-task file
//!   .thclaws/state/team/tasks/_hwm           — high water mark for task IDs
//!   .thclaws/state/team/agents/{name}/status.json — heartbeat
//!   .thclaws/state/team/agents/{name}/output.log  — output capture for GUI

use crate::error::{Error, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const POLL_INTERVAL_MS: u64 = 1000;

/// A teammate rewrites its status (heartbeat) every poll (~1s) even when
/// idle, so a status whose `last_heartbeat` is older than this is a
/// crashed / never-booted teammate, not a healthy idle one. Kept
/// comfortably above the poll cadence to avoid racing a momentarily-busy
/// teammate.
pub const STALE_SECS: u64 = 10;

/// M6.34 TEAM1: validate agent / member names before they're used to build
/// filesystem paths (`.thclaws/team/inboxes/<name>.json`,
/// `.thclaws/team/agents/<name>/...`, `.worktrees/<name>`) or git refs
/// (`team/<name>`). Pre-fix `name` flowed straight from
/// model-controlled tool input (`TeamCreate`, `SpawnTeammate`,
/// `SendMessage.to`, GUI `team_send_message.to`) into `format!` /
/// `join`, so `name = "../../sessions/sess-..."` would overwrite
/// arbitrary files in the project tree.
///
/// Rule: 1–64 chars, first char alphanumeric or `_`, subsequent chars
/// alphanumeric / `_` / `-`. Rejects `..`, `.`, leading dash, slashes,
/// backslashes, control chars, NUL, anything else exotic. The 64-char
/// cap is well under FS NAME_MAX (255) and git refname limits.
///
/// Special case: `*` (broadcast) and `lead` are valid agent names too —
/// `lead` matches the regex; `*` is whitelisted at SendMessage's `to`
/// validator and never reaches this function.
pub fn is_valid_agent_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphanumeric() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Set at session startup by `repl::run_repl_with_state`. True when this
/// process is the team lead (team mode is on AND we're not running as a
/// named teammate). Consulted by tools (e.g. BashTool) to apply lead-only
/// guards. Stored as a static rather than env var so spawned teammate
/// child processes don't accidentally inherit the flag.
static IS_TEAM_LEAD: AtomicBool = AtomicBool::new(false);

pub fn set_is_team_lead(b: bool) {
    IS_TEAM_LEAD.store(b, Ordering::Relaxed);
}

pub fn is_team_lead() -> bool {
    IS_TEAM_LEAD.load(Ordering::Relaxed)
}

/// M6.34 TEAM3: lead's canonical team_dir captured at startup. Used by
/// `kill_my_teammates()` to build a `pkill -f` pattern that matches ONLY
/// teammates spawned by this lead session — pre-fix `pkill -f team-agent`
/// matched any process system-wide whose argv contained "team-agent",
/// which killed teammates of OTHER thClaws sessions in other projects.
///
/// Set once at lead startup (`repl::run_repl_with_state` /
/// `shared_session::run_worker`). Static rather than threaded through
/// the EOF handlers because there are multiple EOF sites in repl.rs and
/// passing an Option<String> through the readline / select! plumbing was
/// noisier than the static.
static LEAD_TEAM_DIR: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
    std::sync::OnceLock::new();

fn lead_team_dir_slot() -> &'static std::sync::Mutex<Option<String>> {
    LEAD_TEAM_DIR.get_or_init(|| std::sync::Mutex::new(None))
}

/// `Child` handles of every teammate this lead spawned as a background
/// process (the no-tmux path). `kill_my_teammates()` reaps these
/// directly, which works on every platform — the `pkill -f` fallback
/// only exists on Unix, so before this a mid-task teammate that rejected
/// the graceful shutdown was orphaned on Windows (#202).
static SPAWNED_CHILDREN: std::sync::OnceLock<std::sync::Mutex<Vec<std::process::Child>>> =
    std::sync::OnceLock::new();

fn spawned_children() -> &'static std::sync::Mutex<Vec<std::process::Child>> {
    SPAWNED_CHILDREN.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn register_spawned_child(child: std::process::Child) {
    if let Ok(mut g) = spawned_children().lock() {
        g.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
        g.push(child);
    }
}

/// Hard-kill every registered background teammate still running and
/// reap it. Returns how many were killed.
fn kill_spawned_children() -> usize {
    let Ok(mut g) = spawned_children().lock() else {
        return 0;
    };
    let mut killed = 0;
    for mut c in g.drain(..) {
        if matches!(c.try_wait(), Ok(None)) {
            let _ = c.kill();
            let _ = c.wait();
            killed += 1;
        }
    }
    killed
}

/// Record the lead's absolute team_dir for later teammate cleanup.
/// Idempotent — calling multiple times overwrites (lead may swap
/// projects mid-session via ChangeCwd).
pub fn set_lead_team_dir(team_dir: &Path) {
    // On the first team session the dir often doesn't exist yet, so
    // canonicalize() fails and would store the RAW relative `.thclaws/team`.
    // SpawnTeammate later bakes an ABSOLUTE `--team-dir` into the child
    // argv, so a relative matcher value would never match → kill_my_teammates
    // orphans every teammate. Absolutize via current_dir() join when
    // canonicalize fails, mirroring the spawn path (team.rs ~1386).
    // Must produce the SAME string the spawn path bakes into the child
    // argv — `kill_my_teammates` matches on it. Windows canonicalize
    // returns a verbatim `\\?\` path; both sites go through one helper so
    // they can never disagree.
    let abs = crate::util::absolutize_for_child(team_dir);
    if let Ok(mut g) = lead_team_dir_slot().lock() {
        *g = Some(abs);
    }
}

/// The ABSOLUTE team dir for this session: the value pinned by
/// [`set_lead_team_dir`] (lead startup), else `THCLAWS_TEAM_DIR`, else an
/// absolutized `.thclaws/team`. Long-lived inbox pollers must use this
/// instead of the relative `Mailbox::default_dir()` so a mid-session cwd
/// change (ChangeCwd) doesn't make the lead read a different project's
/// inbox while teammates keep writing to the original absolute dir.
pub fn resolved_team_dir() -> PathBuf {
    if let Some(d) = lead_team_dir_slot().lock().ok().and_then(|g| g.clone()) {
        return PathBuf::from(d);
    }
    if let Ok(d) = std::env::var("THCLAWS_TEAM_DIR") {
        return PathBuf::from(d);
    }
    let rel = Mailbox::default_dir();
    std::env::current_dir().map(|c| c.join(&rel)).unwrap_or(rel)
}

/// Kill every teammate process spawned by THIS lead session. Graceful
/// `ShutdownRequest` first, then hard-kill the `Child` handles we hold
/// from the background spawn path (cross-platform), then on Unix a
/// `pkill -f --team-dir <abs-path>` backstop for teammates we hold no
/// handle for (tmux panes, or a lead that resumed an older team) — the
/// flag pair is specific enough to never hit another session's
/// teammates (M6.34 TEAM3; teamEnabled off ⇒ nothing to match).
/// Best-effort — kill failures are swallowed.
pub fn kill_my_teammates() {
    let dir = lead_team_dir_slot().lock().ok().and_then(|g| g.clone());
    let team_ctx = dir.as_ref().map(|d| {
        let team_dir = std::path::PathBuf::from(d);
        let config = TeamConfig::load(&team_dir.join("config.json")).ok();
        (Mailbox::new(team_dir), config)
    });

    // Graceful first: broadcast a ShutdownRequest so an idle teammate can
    // flush state and self-exit via its poll loop, then give a short grace
    // before the hard fallback. (Previously the handshake was dead
    // code — nothing ever SENT a ShutdownRequest.)
    if let Some((mb, Some(config))) = team_ctx.as_ref() {
        for member in &config.members {
            let msg = TeamMessage::new("lead", &make_shutdown_request("lead"));
            let _ = mb.write_to_mailbox(&member.name, msg);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));
    }

    // Hard fallback 1: the handles we own. A teammate mid-task answers
    // the request with ShutdownRejected and keeps polling; this is the
    // only thing that reaps it on Windows (#202).
    kill_spawned_children();

    // Hard fallback 2 (Unix only — no pkill on Windows): anything still
    // carrying `--team-dir <abs path>` in its cmdline. The `--`
    // end-of-options marker is REQUIRED: the pattern starts with `--`,
    // which pkill's getopt otherwise parses as an unknown long option (no
    // teammates get killed, issue #163 Bug 2).
    #[cfg(unix)]
    if let Some(ref dir) = dir {
        let pattern = format!("--team-dir {}", dir);
        let _ = std::process::Command::new("pkill")
            .args(["-f", "--", &pattern])
            .status();
    }

    // Mark killed teammates stopped and reclaim any tasks they still owned,
    // so a later respawn / resume doesn't see tasks stranded InProgress.
    if let Some((mb, config)) = team_ctx {
        if let Some(config) = config {
            for member in &config.members {
                let _ = mb.write_status(&member.name, "stopped", None);
            }
        }
        let _ = mb.reap_stale_tasks();
    }
}

/// True when (a) a git merge is currently in progress in the repo that
/// `path` lives in, AND (b) `path` itself currently contains conflict
/// markers. Used by Write/Edit to grant the team lead a narrow exception
/// to the "no source-mutation" rule — merge-conflict resolution is the
/// one legitimate lead-author activity. Once the lead resolves and the
/// merge commit is made, .git/MERGE_HEAD disappears and the exception
/// closes automatically.
pub fn lead_resolving_merge_conflict(path: &Path) -> bool {
    // Find the .git directory by walking up from the file's parent.
    // (Path may be a not-yet-existing file — its parent should exist.)
    let start = path.parent().unwrap_or(path);
    let git_dir = match find_git_dir(start) {
        Some(d) => d,
        None => return false,
    };
    if !git_dir.join("MERGE_HEAD").exists() {
        return false;
    }
    // Read the file and check for conflict markers. If the file doesn't
    // exist yet, it can't be a conflicted file — disallow.
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    content.contains("<<<<<<<") && content.contains("=======") && content.contains(">>>>>>>")
}

/// Walk up from `start` to find the `.git` directory (or `.git` file for
/// worktrees, in which case follow the gitdir pointer). Returns the path
/// to the canonical git dir for that repo / worktree.
fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut cur = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    loop {
        let candidate = cur.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            // Worktree: contains "gitdir: /abs/path/to/.git/worktrees/name".
            let s = std::fs::read_to_string(&candidate).ok()?;
            let line = s.lines().next()?;
            let p = line.strip_prefix("gitdir:")?.trim();
            return Some(PathBuf::from(p));
        }
        if !cur.pop() {
            return None;
        }
    }
}

// ── Data structures ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub lead_agent_id: String,
    #[serde(default)]
    pub members: Vec<TeamMember>,
    // Legacy compat: old format used `agents` instead of `members`.
    #[serde(default, skip_serializing)]
    pub agents: Vec<LegacyAgentDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMember {
    pub name: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub tmux_pane_id: Option<String>,
    /// `Some("worktree")` → SpawnTeammate auto-creates a git
    /// worktree at `.worktrees/{name}` on branch `team/{name}`
    /// and launches the teammate there. `None` → teammate runs in the
    /// lead's cwd (shared tree). Previously this was only readable
    /// from `.thclaws/agents/{name}.md` frontmatter; now also settable
    /// declaratively via TeamCreate so ad-hoc teams don't resort to
    /// `git worktree add` shell instructions in the prompt.
    #[serde(default)]
    pub isolation: Option<String>,
}

/// Old format for backward compat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyAgentDef {
    pub name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl TeamConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("team config: {e}")))?;
        let mut config: TeamConfig = serde_json::from_str(&contents)
            .map_err(|e| Error::Config(format!("team config parse: {e}")))?;
        // Migrate legacy format: `agents` → `members`.
        if config.members.is_empty() && !config.agents.is_empty() {
            config.members = config
                .agents
                .iter()
                .map(|a| TeamMember {
                    name: a.name.clone(),
                    prompt: a.instructions.clone(),
                    role: a.role.clone(),
                    color: None,
                    cwd: a.cwd.clone(),
                    is_active: false,
                    tmux_pane_id: None,
                    isolation: None,
                })
                .collect();
        }
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = serde_json::to_string_pretty(self)?;
        with_file_lock(path, || std::fs::write(path, &contents).map_err(Into::into))
    }

    pub fn find_member(&self, name: &str) -> Option<&TeamMember> {
        self.members.iter().find(|m| m.name == name)
    }

    pub fn set_member_active(&mut self, name: &str, active: bool) {
        if let Some(m) = self.members.iter_mut().find(|m| m.name == name) {
            m.is_active = active;
        }
    }
}

// ── Mailbox (single-file inbox with `read` tracking) ────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMessage {
    pub id: String,
    pub from: String,
    pub text: String,
    pub timestamp: u64,
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub summary: Option<String>,
    // Legacy compat fields (read but not written).
    #[serde(default, skip_serializing, alias = "content")]
    pub _content: Option<String>,
    #[serde(default, skip_serializing, alias = "to")]
    pub _to: Option<String>,
}

impl TeamMessage {
    pub fn new(from: &str, text: &str) -> Self {
        let summary = text
            .split_whitespace()
            .take(8)
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            from: from.into(),
            text: text.into(),
            timestamp: now_secs(),
            read: false,
            summary: Some(summary),
            _content: None,
            _to: None,
        }
    }

    /// Get the text content, handling legacy `content` field.
    pub fn content(&self) -> &str {
        if self.text.is_empty() {
            self._content.as_deref().unwrap_or("")
        } else {
            &self.text
        }
    }
}

// ── Protocol messages (embedded as JSON in `text` field) ────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ProtocolMessage {
    #[serde(rename = "idle_notification")]
    IdleNotification {
        from: String,
        #[serde(default)]
        idle_reason: Option<String>, // available, interrupted, failed
        #[serde(default)]
        completed_task_id: Option<String>,
        #[serde(default)]
        completed_status: Option<String>, // completed, blocked, failed
        #[serde(default)]
        summary: Option<String>,
    },
    #[serde(rename = "shutdown_request")]
    ShutdownRequest { from: String },
    #[serde(rename = "shutdown_approved")]
    ShutdownApproved { from: String },
    #[serde(rename = "shutdown_rejected")]
    ShutdownRejected { from: String, reason: String },
    /// Lead → teammate: cancel the current turn cooperatively (the only way
    /// to interrupt a headless/background teammate, which never receives
    /// SIGINT).
    #[serde(rename = "abort_turn")]
    AbortTurn { from: String },
}

pub fn parse_protocol_message(text: &str) -> Option<ProtocolMessage> {
    serde_json::from_str(text).ok()
}

pub fn make_idle_notification(
    from: &str,
    task_id: Option<&str>,
    status: Option<&str>,
    summary: Option<&str>,
) -> String {
    make_status_notification(from, "available", task_id, status, summary)
}

/// Like [`make_idle_notification`] but lets the caller set `idle_reason`.
/// The idle helper hardcodes "available", which hides give-up/failure
/// states from the lead's coordination logic, so failure/blocked paths use
/// this directly.
pub fn make_status_notification(
    from: &str,
    idle_reason: &str,
    task_id: Option<&str>,
    status: Option<&str>,
    summary: Option<&str>,
) -> String {
    serde_json::to_string(&ProtocolMessage::IdleNotification {
        from: from.into(),
        idle_reason: Some(idle_reason.into()),
        completed_task_id: task_id.map(String::from),
        completed_status: status.map(String::from),
        summary: summary.map(String::from),
    })
    .unwrap_or_default()
}

/// Teammate → lead: this turn failed (provider/config error). idle_reason
/// and completed_status both say "failed" so the lead doesn't treat it as a
/// clean finish.
pub fn make_failure_notification(from: &str, summary: &str) -> String {
    make_status_notification(from, "failed", None, Some("failed"), Some(summary))
}

/// Teammate → lead: gave up mid-task (e.g. hit max_iterations / time budget).
/// The task is left for the lead to re-drive.
pub fn make_blocked_notification(from: &str, task_id: Option<&str>, summary: &str) -> String {
    make_status_notification(from, "blocked", task_id, Some("blocked"), Some(summary))
}

pub fn make_shutdown_request(from: &str) -> String {
    serde_json::to_string(&ProtocolMessage::ShutdownRequest { from: from.into() })
        .unwrap_or_default()
}

// ── Task queue ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamTask {
    pub id: String,
    pub subject: String,
    pub description: String,
    #[serde(default)]
    pub owner: Option<String>,
    pub status: TaskStatus,
    #[serde(default)]
    pub blocks: Vec<String>,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

pub struct TaskQueue {
    tasks_dir: PathBuf,
}

impl TaskQueue {
    pub fn new(tasks_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&tasks_dir);
        Self { tasks_dir }
    }

    fn hwm_path(&self) -> PathBuf {
        self.tasks_dir.join("_hwm")
    }

    fn task_path(&self, id: &str) -> PathBuf {
        self.tasks_dir.join(format!("{id}.json"))
    }

    fn next_id(&self) -> Result<String> {
        let hwm_path = self.hwm_path();
        with_file_lock(&hwm_path, || {
            let current: u64 = std::fs::read_to_string(&hwm_path)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            let next = current + 1;
            std::fs::write(&hwm_path, next.to_string())?;
            Ok(next.to_string())
        })
    }

    pub fn create(
        &self,
        subject: &str,
        description: &str,
        blocked_by: &[String],
        owner: Option<&str>,
    ) -> Result<TeamTask> {
        let id = self.next_id()?;
        let now = now_secs();
        let task = TeamTask {
            id: id.clone(),
            subject: subject.into(),
            description: description.into(),
            owner: owner.map(String::from),
            status: TaskStatus::Pending,
            blocks: vec![],
            blocked_by: blocked_by.to_vec(),
            created_at: now,
            updated_at: now,
        };
        let path = self.task_path(&id);
        let contents = serde_json::to_string_pretty(&task)?;
        // Take the exclusive lock readers acquire (shared) so a concurrent
        // get()/list() sees either no file or a fully-written one, never a
        // partial. ids are unique via the _hwm lock, so this only guards
        // against partial-read, not collisions.
        with_file_lock(&path, || {
            atomic_write(&path, &contents)?;
            Ok(())
        })?;
        Ok(task)
    }

    pub fn get(&self, id: &str) -> Result<Option<TeamTask>> {
        let path = self.task_path(id);
        if !path.exists() {
            return Ok(None);
        }
        // M6.34 TEAM6: shared lock so we don't observe a task file
        // mid-rewrite from claim() / complete() / release().
        with_file_lock_shared(&path, || {
            let contents = std::fs::read_to_string(&path)?;
            Ok(serde_json::from_str(&contents).ok())
        })
    }

    pub fn claim(&self, task_id: &str, agent_id: &str) -> Result<TeamTask> {
        // Busy check: agent can't claim if they already own an in_progress task.
        let existing = self.list(Some(TaskStatus::InProgress))?;
        let busy_tasks: Vec<String> = existing
            .iter()
            .filter(|t| t.owner.as_deref() == Some(agent_id))
            .map(|t| t.id.clone())
            .collect();
        if !busy_tasks.is_empty() {
            return Err(Error::Tool(format!(
                "agent '{}' is busy with task(s): {}. Complete them first.",
                agent_id,
                busy_tasks.join(", ")
            )));
        }

        // Resolve blocked_by deps BEFORE taking this task's exclusive lock.
        // get() acquires a SHARED lock on the dep's `.lock`; doing that
        // inside the exclusive lock below would deadlock the thread against
        // its own lock if a dep resolves to the same lock file (a
        // self-referential or cyclic blocked_by). flock is per-fd, so a
        // second acquire from the same process still blocks.
        let prelim = self
            .get(task_id)?
            .ok_or_else(|| Error::Tool(format!("task {task_id} not found")))?;
        for dep_id in &prelim.blocked_by {
            if let Some(dep) = self.get(dep_id)? {
                if dep.status != TaskStatus::Completed {
                    return Err(Error::Tool(format!(
                        "task {} blocked by task {} (status: {:?})",
                        task_id, dep_id, dep.status
                    )));
                }
            }
        }

        let path = self.task_path(task_id);
        with_file_lock(&path, || {
            let contents = std::fs::read_to_string(&path)
                .map_err(|_| Error::Tool(format!("task {task_id} not found")))?;
            let mut task: TeamTask = serde_json::from_str(&contents)?;

            if task.status != TaskStatus::Pending {
                return Err(Error::Tool(format!(
                    "task {} is {:?}, not pending",
                    task_id, task.status
                )));
            }
            // A pre-assigned owner (set at create time by the lead) reserves the
            // task for that agent. Other agents must not claim it.
            if let Some(reserved) = task.owner.as_deref() {
                if reserved != agent_id {
                    return Err(Error::Tool(format!(
                        "task {} is reserved for {}",
                        task_id, reserved
                    )));
                }
            }
            // blocked_by deps were validated above the lock (they only
            // progress toward Completed, so no re-check is needed here).

            task.owner = Some(agent_id.into());
            task.status = TaskStatus::InProgress;
            task.updated_at = now_secs();
            atomic_write(&path, &serde_json::to_string_pretty(&task)?)?;
            Ok(task)
        })
    }

    pub fn complete(&self, task_id: &str, agent_id: &str) -> Result<TeamTask> {
        let path = self.task_path(task_id);
        with_file_lock(&path, || {
            let contents = std::fs::read_to_string(&path)
                .map_err(|_| Error::Tool(format!("task {task_id} not found")))?;
            let mut task: TeamTask = serde_json::from_str(&contents)?;

            if task.owner.as_deref() != Some(agent_id) {
                return Err(Error::Tool(format!(
                    "task {} owned by {:?}, not {}",
                    task_id, task.owner, agent_id
                )));
            }
            task.status = TaskStatus::Completed;
            task.updated_at = now_secs();
            atomic_write(&path, &serde_json::to_string_pretty(&task)?)?;
            Ok(task)
        })
    }

    /// Release a task back to pending.
    pub fn release(&self, task_id: &str) -> Result<()> {
        let path = self.task_path(task_id);
        with_file_lock(&path, || {
            let contents = std::fs::read_to_string(&path)
                .map_err(|_| Error::Tool(format!("task {task_id} not found")))?;
            let mut task: TeamTask = serde_json::from_str(&contents)?;
            task.owner = None;
            task.status = TaskStatus::Pending;
            task.updated_at = now_secs();
            atomic_write(&path, &serde_json::to_string_pretty(&task)?)?;
            Ok(())
        })
    }

    pub fn list(&self, filter: Option<TaskStatus>) -> Result<Vec<TeamTask>> {
        let mut tasks = Vec::new();
        if !self.tasks_dir.exists() {
            return Ok(tasks);
        }
        for entry in std::fs::read_dir(&self.tasks_dir)?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // M6.34 TEAM6: shared lock per task file so we don't read a
            // task that's being rewritten by a concurrent claim/complete.
            // Errors swallowed (continue) to preserve original best-effort
            // behavior — one corrupt task shouldn't fail the whole list.
            let parsed: Option<TeamTask> = with_file_lock_shared(&path, || {
                let contents = std::fs::read_to_string(&path)?;
                Ok(serde_json::from_str::<TeamTask>(&contents).ok())
            })
            .ok()
            .flatten();
            if let Some(task) = parsed {
                if filter.is_none() || filter == Some(task.status) {
                    tasks.push(task);
                }
            }
        }
        tasks.sort_by_key(|t| t.id.parse::<u64>().unwrap_or(0));
        Ok(tasks)
    }

    /// Find and claim the first available pending task for an agent.
    pub fn claim_next(&self, agent_id: &str) -> Result<Option<TeamTask>> {
        let pending = self.list(Some(TaskStatus::Pending))?;
        for task in pending {
            // Skip tasks the lead pre-assigned to a different agent; only
            // consider unowned tasks or ones reserved for this agent.
            if let Some(reserved) = task.owner.as_deref() {
                if reserved != agent_id {
                    continue;
                }
            }
            match self.claim(&task.id, agent_id) {
                Ok(claimed) => return Ok(Some(claimed)),
                // Not always a race: the agent may be busy with its own
                // InProgress task, the task may be reserved, or blocked by an
                // incomplete dep. Log the reason so a stuck team is debuggable
                // instead of silently looking idle.
                Err(e) => {
                    eprintln!(
                        "[team] claim_next skip task {} for {agent_id}: {e}",
                        task.id
                    );
                    continue;
                }
            }
        }
        Ok(None)
    }
}

// ── Mailbox ─────────────────────────────────────────────────────────

/// Atomically write `data` to `path` via a temp file + rename in the same
/// directory, so a process killed mid-write can never leave a truncated
/// file (the previous direct `std::fs::write` could). Callers hold the
/// file lock, so the temp name doesn't collide.
fn atomic_write(path: &Path, data: &str) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Parse an inbox JSON WITHOUT ever clobbering it: empty/whitespace → empty
/// vec; an unparseable non-empty file is moved aside to `.corrupt` and an
/// error is returned, so a caller about to rewrite the inbox aborts BEFORE
/// the overwrite — preserving the (recoverable) messages instead of the old
/// `unwrap_or_default()` which silently wiped every unread message.
fn parse_inbox(contents: &str, path: &Path) -> Result<Vec<TeamMessage>> {
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }
    match serde_json::from_str(contents) {
        Ok(v) => Ok(v),
        Err(e) => {
            let _ = std::fs::rename(path, path.with_extension("corrupt"));
            Err(Error::Tool(format!(
                "inbox {} unparseable ({e}) — moved aside to .corrupt for recovery",
                path.display()
            )))
        }
    }
}

pub struct Mailbox {
    pub team_dir: PathBuf,
}

impl Mailbox {
    pub fn new(team_dir: PathBuf) -> Self {
        Self { team_dir }
    }

    pub fn default_dir() -> PathBuf {
        PathBuf::from(".thclaws/state/team")
    }

    fn inboxes_dir(&self) -> PathBuf {
        self.team_dir.join("inboxes")
    }

    fn inbox_path(&self, agent: &str) -> PathBuf {
        self.inboxes_dir().join(format!("{agent}.json"))
    }

    fn agents_dir(&self) -> PathBuf {
        self.team_dir.join("agents")
    }

    fn status_path(&self, agent: &str) -> PathBuf {
        self.agents_dir().join(agent).join("status.json")
    }

    pub fn output_log_path(&self, agent: &str) -> PathBuf {
        self.agents_dir().join(agent).join("output.log")
    }

    fn tasks_dir(&self) -> PathBuf {
        self.team_dir.join("tasks")
    }

    /// Initialize directories for an agent.
    pub fn init_agent(&self, agent: &str) -> Result<()> {
        // M6.34 TEAM1: defense-in-depth path-traversal guard. The
        // public tool surfaces (TeamCreate / SpawnTeammate /
        // SendMessage / GUI) validate too; this catches anything
        // that bypasses them.
        if !is_valid_agent_name(agent) {
            return Err(Error::Tool(format!(
                "invalid agent name '{agent}' — must be 1-64 chars, alphanumeric / _ / -, start with alphanumeric or _"
            )));
        }
        std::fs::create_dir_all(self.inboxes_dir())?;
        std::fs::create_dir_all(self.agents_dir().join(agent))?;
        // Create empty inbox if it doesn't exist.
        let inbox = self.inbox_path(agent);
        if !inbox.exists() {
            std::fs::write(&inbox, "[]")?;
        }
        // "spawning" (not "alive") so a teammate that launches but never
        // reaches its poll loop is distinguishable from a booted-and-idle
        // one — the loop's first status write is "idle" (repl.rs).
        self.write_status(agent, "spawning", None)?;
        Ok(())
    }

    /// Read all messages in an agent's inbox.
    pub fn read_mailbox(&self, agent: &str) -> Result<Vec<TeamMessage>> {
        let path = self.inbox_path(agent);
        if !path.exists() {
            return Ok(Vec::new());
        }
        with_file_lock_shared(&path, || {
            let contents = std::fs::read_to_string(&path)?;
            parse_inbox(&contents, &path)
        })
    }

    /// Read only unread messages.
    pub fn read_unread(&self, agent: &str) -> Result<Vec<TeamMessage>> {
        let msgs = self.read_mailbox(agent)?;
        Ok(msgs.into_iter().filter(|m| !m.read).collect())
    }

    /// Write a message to an agent's inbox (exclusive lock, append).
    pub fn write_to_mailbox(&self, agent: &str, msg: TeamMessage) -> Result<()> {
        // M6.34 TEAM1: path-traversal guard. Defends against an
        // agent name like `"../../sessions/sess-abc"` reaching the
        // inbox path constructor.
        if !is_valid_agent_name(agent) {
            return Err(Error::Tool(format!(
                "invalid recipient '{agent}' — must be 1-64 chars, alphanumeric / _ / -"
            )));
        }
        let path = self.inbox_path(agent);
        std::fs::create_dir_all(self.inboxes_dir())?;
        with_file_lock(&path, || {
            let mut msgs: Vec<TeamMessage> = if path.exists() {
                let contents = std::fs::read_to_string(&path)?;
                // parse_inbox returns Err (aborting before the overwrite) on
                // corruption, so existing messages are never wiped.
                parse_inbox(&contents, &path)?
            } else {
                Vec::new()
            };
            msgs.push(msg);
            atomic_write(&path, &serde_json::to_string_pretty(&msgs)?)?;
            Ok(())
        })
    }

    /// Mark specific messages as read.
    pub fn mark_as_read(&self, agent: &str, ids: &[String]) -> Result<()> {
        let path = self.inbox_path(agent);
        with_file_lock(&path, || {
            let contents = std::fs::read_to_string(&path)?;
            let mut msgs: Vec<TeamMessage> = parse_inbox(&contents, &path)?;
            for msg in &mut msgs {
                if ids.contains(&msg.id) {
                    msg.read = true;
                }
            }
            atomic_write(&path, &serde_json::to_string_pretty(&msgs)?)?;
            Ok(())
        })
    }

    /// Write agent status.
    pub fn write_status(&self, agent: &str, status: &str, task: Option<&str>) -> Result<()> {
        // M6.34 TEAM1: path-traversal guard.
        if !is_valid_agent_name(agent) {
            return Err(Error::Tool(format!(
                "invalid agent name '{agent}' — must be 1-64 chars, alphanumeric / _ / -"
            )));
        }
        let s = AgentStatus {
            agent: agent.into(),
            status: status.into(),
            current_task: task.map(String::from),
            last_heartbeat: now_secs(),
        };
        let path = self.status_path(agent);
        std::fs::create_dir_all(path.parent().unwrap())?;
        // M6.34 TEAM6: write under exclusive lock. Pre-fix the heartbeat
        // writer + GUI Team-tab refresher could race — reader observed
        // partial JSON, `serde_json::from_str.ok()` returned None,
        // status appeared as "agent missing" intermittently.
        with_file_lock(&path, || {
            atomic_write(&path, &serde_json::to_string_pretty(&s)?)?;
            Ok(())
        })
    }

    pub fn read_status(&self, agent: &str) -> Option<AgentStatus> {
        let path = self.status_path(agent);
        // M6.34 TEAM6: read under shared lock so we don't observe a
        // status file that's being rewritten.
        if !path.exists() {
            return None;
        }
        with_file_lock_shared(&path, || {
            let contents = std::fs::read_to_string(&path)?;
            Ok(serde_json::from_str::<AgentStatus>(&contents).ok())
        })
        .ok()
        .flatten()
    }

    pub fn all_status(&self) -> Result<Vec<AgentStatus>> {
        let mut statuses = Vec::new();
        let agents_dir = self.agents_dir();
        if !agents_dir.exists() {
            return Ok(statuses);
        }
        for entry in std::fs::read_dir(&agents_dir)?.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(s) = self.read_status(&name) {
                    statuses.push(s);
                }
            }
        }
        Ok(statuses)
    }

    pub fn task_queue(&self) -> TaskQueue {
        TaskQueue::new(self.tasks_dir())
    }

    /// Release any InProgress task whose owner is dead — stopped, crashed
    /// (stale heartbeat), or gone (no status file). Without this a teammate
    /// that claimed a task and then died/erred strands the task InProgress
    /// forever: no other teammate can claim it (claim's busy-check), and the
    /// work is invisible. Returns how many were reclaimed.
    pub fn reap_stale_tasks(&self) -> Result<usize> {
        let tq = self.task_queue();
        let in_progress = tq.list(Some(TaskStatus::InProgress))?;
        let mut released = 0;
        for task in in_progress {
            let Some(owner) = task.owner.as_deref() else {
                continue;
            };
            let dead = match self.read_status(owner) {
                Some(s) => s.status == "stopped" || s.is_stale(),
                None => true, // no status file → owner never existed / gone
            };
            if dead && tq.release(&task.id).is_ok() {
                eprintln!(
                    "[team] reaped stale task {} (owner '{owner}' is dead) → back to Pending",
                    task.id
                );
                released += 1;
            }
        }
        Ok(released)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatus {
    pub agent: String,
    pub status: String,
    pub current_task: Option<String>,
    pub last_heartbeat: u64,
}

impl AgentStatus {
    /// True when this teammate hasn't heartbeat within [`STALE_SECS`] — i.e.
    /// it crashed, was killed, or never entered its poll loop. A live idle
    /// teammate rewrites its heartbeat every poll, so it never goes stale.
    pub fn is_stale(&self) -> bool {
        now_secs().saturating_sub(self.last_heartbeat) > STALE_SECS
    }

    /// True when the process launched but never reached its poll loop
    /// (`init_agent` stamps "spawning"; the loop's first write is "idle").
    pub fn never_booted(&self) -> bool {
        self.status == "spawning"
    }
}

// ── File locking ────────────────────────────────────────────────────

fn with_file_lock<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock_path = path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let lock_file = std::fs::File::create(&lock_path)
        .map_err(|e| Error::Tool(format!("lock create {}: {e}", lock_path.display())))?;
    lock_file
        .lock_exclusive()
        .map_err(|e| Error::Tool(format!("lock acquire: {e}")))?;
    let result = f();
    let _ = lock_file.unlock();
    result
}

fn with_file_lock_shared<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock_path = path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let lock_file = std::fs::File::create(&lock_path)
        .map_err(|e| Error::Tool(format!("lock create {}: {e}", lock_path.display())))?;
    lock_file
        .lock_shared()
        .map_err(|e| Error::Tool(format!("shared lock: {e}")))?;
    let result = f();
    let _ = lock_file.unlock();
    result
}

/// M6.34 TEAM2: POSIX shell single-quote escape for values interpolated
/// into a shell string. Wraps the value in `'…'` and escapes embedded
/// single quotes via the standard `'\''` trick.
///
/// Used in `SpawnTeammate` for `name` / `team_dir` / `effective_cwd` —
/// previously interpolated unquoted, so a name like `foo; rm -rf $HOME ;`
/// would inject shell statements. TEAM1's `is_valid_agent_name` already
/// rejects shell metachars in agent names, but `team_dir` /
/// `effective_cwd` come from filesystem paths which can legally contain
/// spaces / quotes — so escape is necessary defense for those.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Render the teammate launch as a POSIX shell string — `cd <dir> && VAR=…
/// <bin> <args…>`. Only tmux needs this: `tmux split-window` takes a
/// command STRING, not an argv, so there is no way to hand it structured
/// arguments. The background path spawns `bin` directly instead (no shell,
/// nothing to quote) — see [`TeamTools::spawn_teammate`].
///
/// tmux does not exist on Windows, so this stays POSIX-only by
/// construction; that is what makes single-quote escaping safe here.
fn spawn_shell_string(
    bin: &str,
    args: &[String],
    envs: &[(String, String)],
    cwd: Option<&str>,
) -> String {
    let mut out = String::new();
    if let Some(dir) = cwd {
        // Escape the cwd so spaces / quotes in macOS user dirs don't break
        // the cd. Unescaped, `cd /Users/Some One/proj && …` splits into
        // `cd /Users/Some` (wrong dir) + `One/proj` (404).
        out.push_str(&format!("cd {} && ", shell_escape(dir)));
    }
    for (k, v) in envs {
        out.push_str(&format!("{k}={} ", shell_escape(v)));
    }
    out.push_str(&shell_escape(bin));
    for a in args {
        out.push(' ');
        out.push_str(&shell_escape(a));
    }
    out
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── tmux integration ────────────────────────────────────────────────

pub fn has_tmux() -> bool {
    // The tmux launch is a POSIX shell string by construction
    // (`spawn_shell_string`). A tmux.exe from MSYS2/Cygwin on PATH would
    // hand that string to whichever shell it finds — Windows always takes
    // the direct-spawn path instead.
    if cfg!(windows) {
        return false;
    }
    std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn is_inside_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

// ── Team tools ──────────────────────────────────────────────────────

use crate::tools::{Tool, ToolRegistry};
use async_trait::async_trait;
use serde_json::{json, Value};

// ── SendMessage ─────────────────────────────────────────────────────

pub struct SendMessageTool {
    mailbox: Arc<Mailbox>,
    my_name: String,
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &'static str {
        "SendMessage"
    }
    fn description(&self) -> &'static str {
        "Send a message to another agent in the team. Use `to: \"<name>\"` for \
         a specific teammate, or `to: \"*\"` to broadcast to all teammates. \
         Just writing text in your response is NOT visible to others — you MUST \
         use this tool to communicate."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "Recipient name, or \"*\" to broadcast to all"},
                "text": {"type": "string", "description": "Message text"},
                "summary": {"type": "string", "description": "5-10 word preview of the message"},
                "content": {"type": "string", "description": "(legacy alias for text)"}
            },
            "required": ["to"]
        })
    }
    async fn call(&self, input: Value) -> Result<String> {
        let to = crate::tools::req_str(&input, "to")?;
        let text = input
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| input.get("content").and_then(Value::as_str))
            .ok_or_else(|| Error::Tool("missing 'text' field".into()))?;

        // M6.34 TEAM1: validate the recipient before any path
        // construction. `*` is the broadcast sentinel (handled below);
        // every other value must look like a real agent name.
        if to != "*" && !is_valid_agent_name(to) {
            return Err(Error::Tool(format!(
                "invalid recipient '{to}' — must be `*` (broadcast) or a 1-64 char alphanumeric/_/- agent name"
            )));
        }

        // Reject sending to agents that can't receive: stopped, never
        // booted ("spawning" with no heartbeat yet), or crashed (stale
        // heartbeat). Without this a message to a dead teammate "succeeds"
        // (the file write works) and is never read — the lead waits forever.
        if to != "*" && to != "lead" {
            if let Some(status) = self.mailbox.read_status(to) {
                if status.status == "stopped" {
                    return Err(Error::Tool(format!(
                        "teammate '{to}' is stopped — cannot receive messages. \
                         Use SpawnTeammate to respawn it first."
                    )));
                }
                if status.never_booted() {
                    return Err(Error::Tool(format!(
                        "teammate '{to}' launched but never entered its loop \
                         (status=spawning) — it likely failed to start. \
                         Respawn it with SpawnTeammate."
                    )));
                }
                if status.is_stale() {
                    return Err(Error::Tool(format!(
                        "teammate '{to}' is unresponsive — last heartbeat {}s ago \
                         (status={}); it likely crashed. Respawn it with SpawnTeammate.",
                        now_secs().saturating_sub(status.last_heartbeat),
                        status.status
                    )));
                }
            }
        }

        // Broadcast to all team members.
        if to == "*" {
            let config_path = self.mailbox.team_dir.join("config.json");
            let config = TeamConfig::load(&config_path)
                .map_err(|e| Error::Tool(format!("read team config: {e}")))?;
            let mut recipients = Vec::new();
            for member in &config.members {
                if member.name != self.my_name {
                    let msg = TeamMessage::new(&self.my_name, text);
                    self.mailbox.write_to_mailbox(&member.name, msg)?;
                    recipients.push(member.name.as_str());
                }
            }
            // Also send to lead if sender is not lead.
            if self.my_name != "lead" {
                let msg = TeamMessage::new(&self.my_name, text);
                let _ = self.mailbox.write_to_mailbox("lead", msg);
                recipients.push("lead");
            }
            Ok(format!("Broadcast sent to: {}", recipients.join(", ")))
        } else {
            let msg = TeamMessage::new(&self.my_name, text);
            self.mailbox.write_to_mailbox(to, msg)?;
            Ok(format!("Message sent to '{to}'"))
        }
    }
}

// ── CheckInbox ──────────────────────────────────────────────────────

pub struct CheckInboxTool {
    mailbox: Arc<Mailbox>,
    my_name: String,
}

#[async_trait]
impl Tool for CheckInboxTool {
    fn name(&self) -> &'static str {
        "CheckInbox"
    }
    fn description(&self) -> &'static str {
        "Check your inbox for unread messages from other agents. Returns all \
         unread messages and marks them as read."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn call(&self, _input: Value) -> Result<String> {
        let messages = self.mailbox.read_unread(&self.my_name)?;
        if messages.is_empty() {
            return Ok("No new messages.".into());
        }
        let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
        let mut out = Vec::new();
        for msg in &messages {
            out.push(format!("From: {}\n{}", msg.from, msg.content()));
        }
        self.mailbox.mark_as_read(&self.my_name, &ids)?;
        Ok(out.join("\n\n---\n\n"))
    }
}

// ── TeamStatus ──────────────────────────────────────────────────────

pub struct TeamStatusTool {
    mailbox: Arc<Mailbox>,
}

#[async_trait]
impl Tool for TeamStatusTool {
    fn name(&self) -> &'static str {
        "TeamStatus"
    }
    fn description(&self) -> &'static str {
        "Check the status of all agents and the task queue."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn call(&self, _input: Value) -> Result<String> {
        let mut parts = Vec::new();

        // Agent statuses.
        let statuses = self.mailbox.all_status()?;
        if statuses.is_empty() {
            parts.push("No agents running.".to_string());
        } else {
            parts.push("## Agents".to_string());
            for s in &statuses {
                let task = s.current_task.as_deref().unwrap_or("-");
                parts.push(format!("  {} — {} (task: {})", s.agent, s.status, task));
            }
        }

        // Task queue.
        let tq = self.mailbox.task_queue();
        let tasks = tq.list(None)?;
        if !tasks.is_empty() {
            let pending = tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Pending)
                .count();
            let in_progress = tasks
                .iter()
                .filter(|t| t.status == TaskStatus::InProgress)
                .count();
            let completed = tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Completed)
                .count();
            parts.push(format!(
                "\n## Tasks ({} total: {} pending, {} in progress, {} completed)",
                tasks.len(),
                pending,
                in_progress,
                completed
            ));
            for t in &tasks {
                let owner = t.owner.as_deref().unwrap_or("-");
                parts.push(format!(
                    "  [{}] {:?} — {} (owner: {})",
                    t.id, t.status, t.subject, owner
                ));
            }
        }

        Ok(parts.join("\n"))
    }
}

// ── TeamCreate ──────────────────────────────────────────────────────

pub struct TeamCreateTool {
    mailbox: Arc<Mailbox>,
}

#[async_trait]
impl Tool for TeamCreateTool {
    fn name(&self) -> &'static str {
        "TeamCreate"
    }
    fn description(&self) -> &'static str {
        "Create an agent team for parallel work (thClaws native — writes \
         `.thclaws/state/team/config.json` in the current project root; NOT the SDK's \
         server-side teams feature). Define agent names, roles, and optionally \
         `isolation: \"worktree\"` per member — if set, SpawnTeammate auto-\
         creates a git worktree at `.worktrees/{name}` on branch \
         `team/{name}` and launches the teammate there. DO NOT write \
         `git worktree add …` instructions into teammate prompts — it's \
         declarative via the isolation field, not a shell command. After \
         creating, use SpawnTeammate to start each agent and TeamTaskCreate \
         to add tasks to a shared queue teammates can claim."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Team name"},
                "description": {"type": "string", "description": "What this team does"},
                "agents": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "role": {"type": "string"},
                            "prompt": {"type": "string", "description": "Instructions for this agent. Do NOT include `git worktree add` or `cd ../…` — if you want worktree isolation, set `isolation: \"worktree\"` on this member instead."},
                            "isolation": {
                                "type": "string",
                                "enum": ["worktree", "none"],
                                "description": "If \"worktree\", SpawnTeammate auto-creates `.worktrees/{name}` on branch `team/{name}` and runs the teammate there. Default: none (shared cwd)."
                            }
                        },
                        "required": ["name"]
                    }
                }
            },
            "required": ["name", "agents"]
        })
    }
    fn requires_approval(&self, _: &Value) -> bool {
        true
    }
    async fn call(&self, input: Value) -> Result<String> {
        let name = crate::tools::req_str(&input, "name")?;
        let description = input.get("description").and_then(Value::as_str);
        let agents = input
            .get("agents")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Tool("missing agents".into()))?;

        let mut members = Vec::new();
        let mut flagged_prompts: Vec<String> = Vec::new();
        for a in agents {
            let agent_name = a
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Tool("agent missing name".into()))?;
            // M6.34 TEAM1: reject path-traversal / shell-metachar names
            // BEFORE we start writing files. Pre-fix `name = "../foo"`
            // would have status / inbox / output_log files land outside
            // `.thclaws/team/`.
            if !is_valid_agent_name(agent_name) {
                return Err(Error::Tool(format!(
                    "invalid agent name '{agent_name}' — must be 1-64 chars, alphanumeric / _ / -, start with alphanumeric or _. Reserved: agent names also become git branch names (`team/<name>`) and worktree dirs (`.worktrees/<name>`), so the same restriction applies."
                )));
            }
            self.mailbox.init_agent(agent_name)?;
            let prompt_text = a
                .get("prompt")
                .or(a.get("instructions"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // Catch the common hallucination where the lead writes
            // `git worktree add …` into the teammate prompt. We don't
            // strip it (the model might have other reasons for
            // mentioning git), but flag each occurrence so the
            // response nudges the lead toward the isolation field.
            if prompt_text.to_lowercase().contains("git worktree add")
                || prompt_text.contains("worktree add")
            {
                flagged_prompts.push(agent_name.to_string());
            }
            // Normalise the isolation field: accept "worktree" (any
            // case) and "none"/absent; treat other values as None so
            // a typo doesn't silently enable isolation.
            let isolation = a
                .get("isolation")
                .and_then(Value::as_str)
                .map(str::to_lowercase)
                .and_then(|v| match v.as_str() {
                    "worktree" => Some("worktree".to_string()),
                    _ => None,
                });
            members.push(TeamMember {
                name: agent_name.into(),
                prompt: prompt_text,
                role: a.get("role").and_then(Value::as_str).unwrap_or("").into(),
                color: None,
                cwd: None,
                is_active: false,
                tmux_pane_id: None,
                isolation,
            });
        }

        let config = TeamConfig {
            name: name.to_string(),
            description: description.map(String::from),
            created_at: now_secs(),
            lead_agent_id: "lead".into(),
            members: members.clone(),
            agents: vec![],
        };
        config.save(&self.mailbox.team_dir.join("config.json"))?;

        // Initialize task queue.
        let _ = std::fs::create_dir_all(self.mailbox.tasks_dir());

        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        let iso_names: Vec<&str> = members
            .iter()
            .filter(|m| m.isolation.as_deref() == Some("worktree"))
            .map(|m| m.name.as_str())
            .collect();
        let mut msg = format!(
            "Team '{}' created with agents: {}.\n\
             Use SpawnTeammate to start each, TeamTaskCreate to add tasks.\n",
            name,
            names.join(", "),
        );
        if !iso_names.is_empty() {
            msg.push_str(&format!(
                "Worktree isolation requested for: {}. SpawnTeammate will \
                 create `.worktrees/<name>` on branch `team/<name>` \
                 automatically — do NOT run `git worktree add` yourself.\n",
                iso_names.join(", "),
            ));
        }
        msg.push_str(
            "\nIMPORTANT: You are now the team LEAD. Your role is to COORDINATE, not implement.\n\
             - Do NOT use Bash/Write/Edit to build code — delegate to teammates via SendMessage.\n\
             - Use TeamTaskCreate to queue work, SendMessage to assign and coordinate.\n\
             - Use Read/Glob/Grep only for review and verification.\n\
             - If something fails, message the responsible teammate to fix it.\n\
             - Worktree isolation is a DECLARATIVE setting (`isolation: \"worktree\"` \
               in the TeamCreate agents array). Do NOT write `git worktree add …` or \
               `cd ../{name}` into teammate prompts — SpawnTeammate handles it.",
        );
        if !flagged_prompts.is_empty() {
            msg.push_str(&format!(
                "\n\n⚠ The prompt for {} contains `git worktree add …` — that's a \
                 manual shell instruction the teammate will try to execute, \
                 usually landing the worktree outside `.worktrees/`. \
                 Consider re-running TeamCreate with the isolation field set \
                 instead.",
                flagged_prompts.join(", "),
            ));
        }
        Ok(msg)
    }
}

// ── SpawnTeammate ───────────────────────────────────────────────────

pub struct SpawnTeammateTool {
    mailbox: Arc<Mailbox>,
    my_name: String,
}

impl SpawnTeammateTool {
    /// Last ~15 lines of a teammate's output.log, for surfacing a startup
    /// failure (bad cwd, 401, binary abort) back to the lead.
    fn output_tail(&self, name: &str) -> String {
        let log = std::fs::read_to_string(self.mailbox.output_log_path(name)).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        let start = lines.len().saturating_sub(15);
        lines[start..].join("\n")
    }
}

#[async_trait]
impl Tool for SpawnTeammateTool {
    fn name(&self) -> &'static str {
        "SpawnTeammate"
    }
    fn description(&self) -> &'static str {
        "Spawn a teammate agent process. The teammate runs autonomously, \
         polls its inbox for messages, and can claim tasks from the task queue. \
         In tmux: opens a new pane. Otherwise: background process."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Agent name (from TeamCreate)"},
                "prompt": {"type": "string", "description": "Initial task/instructions"},
                "cwd": {"type": "string", "description": "Working directory"},
                "model": {
                    "type": "string",
                    "description": "Optional model override for this teammate (e.g. 'gpt-4o', 'openrouter/anthropic/claude-sonnet-4-6', 'minimax/MiniMax-M2'). Lets a single team mix providers — e.g. lead on Anthropic, researcher on Gemini, coder on OpenAI. Requires the corresponding API key to be configured (`/api-key set <provider>` or env var); SpawnTeammate pre-flights and refuses if the key is missing. Falls back to the persistent agent-def model (`.thclaws/agents/<name>.md` frontmatter) when not set, then to the lead session's default."
                }
            },
            "required": ["name", "prompt"]
        })
    }
    fn requires_approval(&self, _: &Value) -> bool {
        true
    }
    async fn call(&self, input: Value) -> Result<String> {
        let name = crate::tools::req_str(&input, "name")?;
        let prompt = crate::tools::req_str(&input, "prompt")?;
        let cwd = input.get("cwd").and_then(Value::as_str);
        // Trim + drop empty so an LLM passing `"model": ""` (or only
        // whitespace) is treated as "not specified" and falls through
        // to agent_def / config default rather than tripping the
        // "doesn't map to any known provider" pre-flight error.
        let runtime_model = input
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        // M6.44 / #79: pre-flight auth check for cross-provider spawn.
        // If the runtime model maps to a provider whose API key isn't
        // configured (neither keychain nor env var), bail before any
        // side effects (mailbox init, worktree creation, subprocess
        // launch). The teammate would otherwise spawn, fail at first
        // turn with a 401, and exit silently — frustrating to debug.
        if let Some(ref m) = runtime_model {
            if let Some(kind) = crate::providers::ProviderKind::detect(m) {
                // OK if a local key exists OR the gateway proxy can serve this
                // (featured) model — mirror build_provider so a proxy-only user
                // can spawn a featured-model teammate without a BYOK key.
                let gateway_ok = {
                    let mut cfg = crate::config::AppConfig::load().unwrap_or_default();
                    cfg.model = m.clone();
                    crate::providers::thclaws_gateway::gateway_overlay_for_model(&cfg, kind)
                        .is_some()
                };
                if !kind.has_key_available() && !gateway_ok {
                    let env = kind
                        .api_key_env()
                        .map(|e| format!(" (env var: {e})"))
                        .unwrap_or_default();
                    return Err(Error::Tool(format!(
                        "SpawnTeammate model '{m}' routes to provider '{}'{env} but no API key is configured for that provider. \
                         Run `/api-key set {}` (CLI) or set the env var, then retry.",
                        kind.name(),
                        kind.name()
                    )));
                }
            } else {
                return Err(Error::Tool(format!(
                    "SpawnTeammate model '{m}' doesn't map to any known provider — check the model id (e.g. 'gpt-4o', 'claude-sonnet-4-6', 'minimax/MiniMax-M2')"
                )));
            }
        }

        // M6.34 TEAM1: reject before any side effects (worktree creation,
        // file writes, subprocess spawn). Pre-fix `name = "../foo"` would
        // create a worktree at `<project_parent>/foo/` and a branch
        // `team/../foo` (latter typically rejected by git, but the worktree
        // dir was created first).
        if !is_valid_agent_name(name) {
            return Err(Error::Tool(format!(
                "invalid agent name '{name}' — must be 1-64 chars, alphanumeric / _ / -, start with alphanumeric or _"
            )));
        }
        self.mailbox.init_agent(name)?;

        // Look up agent definition from .thclaws/agents/, agents.json, or
        // any plugin-contributed agent dirs.
        let agent_defs = crate::agent_defs::AgentDefsConfig::load_with_extra(
            &crate::plugins::plugin_agent_dirs(),
        );
        let agent_def = agent_defs.get(name);

        // Load the team config member record now (was loaded later in
        // this function; moved up so the worktree check can also see
        // member.isolation declaratively set by TeamCreate).
        let config_path = self.mailbox.team_dir.join("config.json");
        let config = TeamConfig::load(&config_path).ok();
        let member = config.as_ref().and_then(|c| c.find_member(name)).cloned();

        // Send initial prompt as first inbox message.
        // If agent def has instructions, prepend them.
        let full_prompt = if let Some(def) = agent_def {
            if def.instructions.is_empty() {
                prompt.to_string()
            } else {
                format!(
                    "[Agent role: {}]\n[Instructions: {}]\n\n{}",
                    def.description, def.instructions, prompt
                )
            }
        } else {
            prompt.to_string()
        };
        let msg = TeamMessage::new(&self.my_name, &full_prompt);
        self.mailbox.write_to_mailbox(name, msg)?;

        // Build spawn command.
        let bin = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "thclaws".to_string());
        // Use absolute path so teammates with different cwd still find the team dir.
        let team_dir = crate::util::absolutize_for_child(&self.mailbox.team_dir);
        let needs_cli = bin.ends_with("/thclaws") || bin.ends_with("\\thclaws");

        // Argv as data, not as a shell string. `bin` (from current_exe)
        // and `team_dir` routinely contain spaces — macOS app bundles,
        // `C:\Program Files\thClaws\bin\thclaws.exe` — and quoting them
        // correctly for both /bin/sh and cmd.exe is not possible in one
        // string. Only the tmux path renders these back into a shell
        // string, where `shell_escape` is safe (tmux is POSIX-only).
        //
        // `--cli` is redundant (`--team-agent` already forces CLI mode in
        // app.rs) but kept so the spawn line stays self-explanatory.
        let mut args: Vec<String> = Vec::new();
        if needs_cli {
            args.push("--cli".to_string());
        }
        args.extend([
            "--team-agent".to_string(),
            name.to_string(),
            "--team-dir".to_string(),
            team_dir.clone(),
            "--permission-mode".to_string(),
            "auto".to_string(),
            "--accept-all".to_string(),
        ]);

        // Model override resolution — precedence:
        //   1. SpawnTeammate input `model:` (per-spawn explicit choice,
        //      pre-flighted for auth above) — wins
        //   2. Agent def `model:` frontmatter from .thclaws/agents/<name>.md
        //   3. Lead session config default (no flag)
        if let Some(ref m) = runtime_model {
            args.push("--model".to_string());
            args.push(m.clone());
        } else if let Some(def) = agent_def {
            if let Some(ref model) = def.model {
                if model.contains('-') {
                    // M6.34 TEAM2: escape — model strings from agent_def
                    // can legally contain `/` (e.g.
                    // `anthropic/claude-sonnet-4-6`); future model ids
                    // could carry other shell-significant chars.
                    args.push("--model".to_string());
                    args.push(model.clone());
                } else {
                    // Short alias — resolve provider-aware.
                    let current_provider = crate::config::AppConfig::load()
                        .ok()
                        .and_then(|c| c.detect_provider_kind().ok());
                    if let Some(provider) = current_provider {
                        match crate::providers::ProviderKind::resolve_alias_for_provider(
                            model, provider,
                        ) {
                            Some(resolved) => {
                                eprintln!(
                                    "\x1b[33m[team] resolved agent def alias '{model}' → '{resolved}' (provider: {})\x1b[0m",
                                    provider.name()
                                );
                                args.push("--model".to_string());
                                args.push(resolved.clone());
                            }
                            None => {
                                eprintln!(
                                    "\x1b[33m[team] agent def for '{name}' specifies model alias '{model}', which doesn't map to current provider '{}' — ignoring (teammate will use config default)\x1b[0m",
                                    provider.name()
                                );
                            }
                        }
                    } else {
                        eprintln!(
                            "\x1b[33m[team] agent def for '{name}' uses alias '{model}' but no provider is configured — ignoring\x1b[0m"
                        );
                    }
                }
            }
        }

        // Git worktree isolation: fire if either source says so —
        //   (a) `.thclaws/agents/{name}.md` frontmatter has
        //       `isolation: worktree` (persistent agent defs), or
        //   (b) the team config member has `isolation: "worktree"`
        //       (set declaratively at TeamCreate time for ad-hoc teams).
        let iso_from_member =
            member.as_ref().and_then(|m| m.isolation.as_deref()) == Some("worktree");
        let iso_from_def = agent_def.and_then(|d| d.isolation.as_deref()) == Some("worktree");
        let worktree_path = if iso_from_member || iso_from_def {
            let project_root = std::env::current_dir().unwrap_or_default();
            let wt_dir = project_root.join(format!(".worktrees/{name}"));
            let branch = format!("team/{name}");

            // Ensure project_root is a git repo — otherwise `git worktree add` fails
            // and the teammate silently ends up running in the same (empty) cwd as lead.
            let is_git_repo = std::process::Command::new("git")
                .args(["rev-parse", "--is-inside-work-tree"])
                .current_dir(&project_root)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !is_git_repo {
                eprintln!(
                    "\x1b[33m[team] project is not a git repo — running 'git init' so worktree isolation works\x1b[0m"
                );
                let _ = std::process::Command::new("git")
                    .args(["init", "-q"])
                    .current_dir(&project_root)
                    .output();
            }
            // After (or independent of) `git init`, ensure HEAD exists. Worktree
            // creation via `git worktree add <path> <branch>` requires a HEAD to
            // branch from, and `git branch <branch>` silently no-ops in an unborn
            // repo — so without this, the spawn fails with "invalid reference".
            // This covers two cases the original gate missed:
            //   (1) a pre-existing repo with zero commits (e.g. user wiped history)
            //   (2) any other path where init ran but HEAD didn't get created
            let has_head = std::process::Command::new("git")
                .args(["rev-parse", "--verify", "HEAD"])
                .current_dir(&project_root)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !has_head {
                eprintln!(
                    "\x1b[33m[team] repo has no HEAD — creating empty initial commit so worktree can branch off\x1b[0m"
                );
                let _ = std::process::Command::new("git")
                    .args([
                        "-c",
                        "user.name=thclaws",
                        "-c",
                        "user.email=thclaws@local",
                        "commit",
                        "--allow-empty",
                        "-q",
                        "-m",
                        "init",
                    ])
                    .current_dir(&project_root)
                    .output();
            }

            // Clear any stale `.git/worktrees/<name>` registration left by a
            // crash / manual `rm -rf .worktrees/<name>` / prior run, else
            // `git worktree add` dies with "missing but already registered".
            let _ = std::process::Command::new("git")
                .args(["worktree", "prune"])
                .current_dir(&project_root)
                .output();
            // Create branch from current HEAD if it doesn't exist (no-op if it does).
            let _ = std::process::Command::new("git")
                .args(["branch", &branch])
                .current_dir(&project_root)
                .output();
            // Force-add so a leftover-but-stale dir doesn't block creation.
            let result = std::process::Command::new("git")
                .args(["worktree", "add", "-f", &wt_dir.to_string_lossy(), &branch])
                .current_dir(&project_root)
                .output();
            match result {
                Ok(out) if out.status.success() => {
                    eprintln!(
                        "\x1b[33m[team] created worktree for '{}' at {} (branch: {})\x1b[0m",
                        name,
                        wt_dir.display(),
                        branch
                    );
                    Some(wt_dir.to_string_lossy().to_string())
                }
                // Isolation was explicitly requested: do NOT silently fall back
                // to the lead's cwd — a teammate writing to the lead's tree on
                // the wrong branch is silent data corruption, and the lead's
                // later TeamMerge would find 0 commits ahead. Fail loudly.
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    return Err(Error::Tool(format!(
                        "worktree creation failed for '{name}': {} — refusing to spawn (would write to the lead's tree). Resolve the repo state and respawn.",
                        stderr.trim()
                    )));
                }
                Err(e) => {
                    return Err(Error::Tool(format!(
                        "git worktree add failed for '{name}': {e}"
                    )));
                }
            }
        } else {
            None
        };

        // Get cwd from: worktree > input > team config > project root.
        // (config + member are loaded earlier, above the worktree check.)
        // Non-worktree teammates fall through to project_root so they land
        // in the main repo deterministically rather than inheriting whatever
        // shell cwd the lead happened to have. Without this, a teammate like
        // qa (isolation: null) could drift into .worktrees/<other>/ via a
        // single Bash `cd` and start writing files on the wrong branch.
        // Stripped of any `\\?\` prefix for the same reason as team_dir: a
        // workspace switch can leave the process cwd verbatim on Windows,
        // and it would otherwise leak into the child's cwd and
        // THCLAWS_PROJECT_ROOT.
        let project_root_str = crate::util::strip_verbatim_prefix(
            &std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy(),
        );
        let effective_cwd = worktree_path
            .clone()
            .or_else(|| cwd.map(String::from))
            .or_else(|| member.as_ref().and_then(|m| m.cwd.clone()))
            .or_else(|| Some(project_root_str.clone()))
            .map(|d| crate::util::strip_verbatim_prefix(&d));
        // Child environment. Kept as data (not a `VAR=value …` string
        // prefix) because that prefix form is POSIX-only — cmd.exe has no
        // equivalent, which is half of why Windows spawns never launched
        // (#200). `spawn_shell_string` re-adds the prefix for tmux.
        //
        // THCLAWS_PROJECT_ROOT exposes the original project root so
        // teammates in worktrees know where to write shared docs /
        // artifacts that other teammates should see.
        //
        // EDITOR/VISUAL/GIT_EDITOR stub out interactive editors. Even
        // though BashTool redirects stdin to /dev/null, programs like
        // vi/nano/vim open /dev/tty directly to find a terminal — and a
        // teammate running inside a tmux pane has a real tty available, so
        // vi finds it and waits forever for human input. Pointing them at
        // `true` makes any attempted editor invocation a no-op exit-0, so
        // e.g. `git commit -e` falls back to the message it already has
        // and the agent's bash call returns instead of hanging the team.
        let mut envs: Vec<(String, String)> = vec![
            ("THCLAWS_PROJECT_ROOT".to_string(), project_root_str.clone()),
            ("EDITOR".to_string(), "true".to_string()),
            ("VISUAL".to_string(), "true".to_string()),
            ("GIT_EDITOR".to_string(), "true".to_string()),
            ("GIT_SEQUENCE_EDITOR".to_string(), "true".to_string()),
        ];
        if worktree_path.is_some() {
            envs.push(("THCLAWS_IN_WORKTREE".to_string(), "1".to_string()));
        }
        let agent_cmd = spawn_shell_string(&bin, &args, &envs, effective_cwd.as_deref());

        // Update config: mark member active.
        if let Some(mut cfg) = config {
            cfg.set_member_active(name, true);
            let _ = cfg.save(&config_path);
        }

        // Snapshot before spawning: init_agent stamped status "spawning" (as
        // the lead). The teammate's own poll loop rewrites status to "idle"
        // the instant it boots, so a status still reading "spawning" after a
        // grace period means the process launched but never entered its loop
        // — the classic silent-teammate failure. We verify a real boot below
        // instead of reporting success on launcher exit alone.
        let spawn_t = now_secs();

        // Spawn via tmux or background.
        let mut bg_child: Option<std::process::Child> = None;
        let spawn_desc = if has_tmux() {
            eprintln!("\x1b[33m[team] spawn cmd (tmux): {}\x1b[0m", agent_cmd);
            if is_inside_tmux() {
                let st = std::process::Command::new("tmux")
                    .args(["split-window", "-h", "-d"])
                    .arg(&agent_cmd)
                    .status()
                    .map_err(|e| Error::Tool(format!("tmux split: {e}")))?;
                if !st.success() {
                    return Err(Error::Tool(format!(
                        "tmux split-window failed ({st}) — could not spawn '{name}'"
                    )));
                }
                let _ = std::process::Command::new("tmux")
                    .args(["select-layout", "tiled"])
                    .status();
                "in tmux pane (current session)".to_string()
            } else {
                let session = "thclaws-team";
                let exists = std::process::Command::new("tmux")
                    .args(["has-session", "-t", session])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                let st = if exists {
                    let s = std::process::Command::new("tmux")
                        .args(["split-window", "-h", "-t", session, "-d"])
                        .arg(&agent_cmd)
                        .status()
                        .map_err(|e| Error::Tool(format!("tmux split: {e}")))?;
                    let _ = std::process::Command::new("tmux")
                        .args(["select-layout", "-t", session, "tiled"])
                        .status();
                    s
                } else {
                    std::process::Command::new("tmux")
                        .args(["new-session", "-d", "-s", session, "-n", "team"])
                        .arg(&agent_cmd)
                        .status()
                        .map_err(|e| Error::Tool(format!("tmux new: {e}")))?
                };
                if !st.success() {
                    return Err(Error::Tool(format!(
                        "tmux failed ({st}) — could not spawn '{name}'"
                    )));
                }
                format!("in tmux session '{session}'")
            }
        } else {
            // No tmux — redirect stdout/stderr to the output log so the GUI
            // Team tab can read it.
            let log_path = self.mailbox.output_log_path(name);
            if let Some(parent) = log_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let log_file = std::fs::File::create(&log_path)
                .map_err(|e| Error::Tool(format!("create log: {e}")))?;
            let log_err = log_file
                .try_clone()
                .map_err(|e| Error::Tool(format!("clone log: {e}")))?;
            // Spawn the binary DIRECTLY — no shell. A shell string would
            // have to be quoted, and no quoting works on both /bin/sh and
            // cmd.exe: POSIX `'…'` is literal text to cmd.exe, and the
            // `VAR=value cmd` env prefix has no cmd.exe equivalent at all.
            // That is why every Windows spawn failed with "The filename,
            // directory name, or volume label syntax is incorrect" (#200).
            // Log the real argv, not the tmux shell string — on Windows
            // the two differ, and the reporter of #200 reads these lines.
            eprintln!(
                "\x1b[33m[team] spawn (direct, no shell): {} {} (cwd: {})\x1b[0m",
                bin,
                args.join(" "),
                effective_cwd.as_deref().unwrap_or("<inherit>")
            );
            let mut cmd = crate::util::direct_command(&bin);
            cmd.args(&args);
            for (k, v) in &envs {
                cmd.env(k, v);
            }
            if let Some(ref dir) = effective_cwd {
                cmd.current_dir(dir);
            }
            let child = cmd
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::from(log_file))
                .stderr(std::process::Stdio::from(log_err))
                .spawn()
                .map_err(|e| Error::Tool(format!("spawn: {e}")))?;
            bg_child = Some(child);
            "as background process".to_string()
        };

        // Liveness probe: wait up to BOOT_TIMEOUT for the teammate to write
        // its OWN status (leaving "spawning"). Returns as soon as it boots
        // (usually <1s — the loop writes "idle" before its first turn, so
        // model latency doesn't matter). On a background process that exits
        // immediately, surface the failure with the output.log tail.
        // Windows gets a longer grace period: process creation is slower,
        // and a healthy boot still has to bring up the scheduler, bind the
        // api_v1 loopback port and scan session metadata before the poll
        // loop writes its first status. 8s was tight enough there to
        // report a live teammate as dead (#200).
        const BOOT_TIMEOUT: std::time::Duration =
            std::time::Duration::from_secs(if cfg!(windows) { 25 } else { 8 });
        let deadline = std::time::Instant::now() + BOOT_TIMEOUT;
        let result = loop {
            if let Some(st) = self.mailbox.read_status(name) {
                if st.status != "spawning" && st.last_heartbeat >= spawn_t {
                    break Ok(format!(
                        "Teammate '{name}' spawned {spawn_desc} and is live."
                    ));
                }
            }
            if let Some(child) = bg_child.as_mut() {
                if let Ok(Some(exit)) = child.try_wait() {
                    break Err(Error::Tool(format!(
                        "teammate '{name}' exited immediately ({exit}) — never booted.\noutput.log tail:\n{}",
                        self.output_tail(name)
                    )));
                }
            }
            if std::time::Instant::now() >= deadline {
                break Err(Error::Tool(format!(
                    "teammate '{name}' launched but never entered its poll loop within {}s (status still 'spawning') — likely failed to start.\noutput.log tail:\n{}",
                    BOOT_TIMEOUT.as_secs(),
                    self.output_tail(name)
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        };
        // Keep the handle even on a probe timeout: a slow booter that
        // outlives the deadline is exactly the process that would
        // otherwise be orphaned at lead exit (#202).
        if let Some(child) = bg_child.take() {
            register_spawned_child(child);
        }
        result
    }
}

// ── TeamTaskCreate ──────────────────────────────────────────────────

pub struct TeamTaskCreateTool {
    mailbox: Arc<Mailbox>,
}

#[async_trait]
impl Tool for TeamTaskCreateTool {
    fn name(&self) -> &'static str {
        "TeamTaskCreate"
    }
    fn description(&self) -> &'static str {
        "Add a task to the team's task queue. Set `owner` to reserve the task \
         for a specific teammate (recommended when the work is role-specific); \
         leave it unset to make the task claimable by whoever is free first. \
         Use `blocked_by` to specify dependencies."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subject": {"type": "string", "description": "Short task title"},
                "description": {"type": "string", "description": "Detailed instructions"},
                "owner": {
                    "type": "string",
                    "description": "Teammate name to reserve this task for. Only that teammate will be able to claim it. Omit for first-come-first-served."
                },
                "blocked_by": {
                    "type": "array", "items": {"type": "string"},
                    "description": "Task IDs that must complete first"
                }
            },
            "required": ["subject", "description"]
        })
    }
    async fn call(&self, input: Value) -> Result<String> {
        let subject = crate::tools::req_str(&input, "subject")?;
        let description = crate::tools::req_str(&input, "description")?;
        let blocked_by: Vec<String> = input
            .get("blocked_by")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let owner = input
            .get("owner")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // Validate owner is a real team member so typos don't silently leave
        // the task unclaimable forever.
        if let Some(name) = owner {
            let config_path = self.mailbox.team_dir.join("config.json");
            let config = TeamConfig::load(&config_path)
                .map_err(|e| Error::Tool(format!("cannot validate owner '{name}': {e}")))?;
            if config.find_member(name).is_none() {
                let known: Vec<&str> = config.members.iter().map(|m| m.name.as_str()).collect();
                return Err(Error::Tool(format!(
                    "owner '{name}' is not a team member. Known: {}",
                    known.join(", ")
                )));
            }
        }

        let tq = self.mailbox.task_queue();
        let task = tq.create(subject, description, &blocked_by, owner)?;
        let owner_note = owner
            .map(|n| format!(" (reserved for {n})"))
            .unwrap_or_default();
        Ok(format!(
            "Task #{} created: {}{owner_note}",
            task.id, task.subject
        ))
    }
}

// ── TeamTaskList ────────────────────────────────────────────────────

pub struct TeamTaskListTool {
    mailbox: Arc<Mailbox>,
}

#[async_trait]
impl Tool for TeamTaskListTool {
    fn name(&self) -> &'static str {
        "TeamTaskList"
    }
    fn description(&self) -> &'static str {
        "List tasks in the team's task queue. Optionally filter by status."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
            }
        })
    }
    async fn call(&self, input: Value) -> Result<String> {
        let filter = input
            .get("status")
            .and_then(Value::as_str)
            .and_then(|s| match s {
                "pending" => Some(TaskStatus::Pending),
                "in_progress" => Some(TaskStatus::InProgress),
                "completed" => Some(TaskStatus::Completed),
                _ => None,
            });
        let tq = self.mailbox.task_queue();
        let tasks = tq.list(filter)?;
        if tasks.is_empty() {
            return Ok("No tasks.".into());
        }
        let lines: Vec<String> = tasks
            .iter()
            .map(|t| {
                let owner = t.owner.as_deref().unwrap_or("-");
                format!(
                    "[{}] {:?} — {} (owner: {})",
                    t.id, t.status, t.subject, owner
                )
            })
            .collect();
        Ok(lines.join("\n"))
    }
}

// ── TeamTaskClaim ───────────────────────────────────────────────────

pub struct TeamTaskClaimTool {
    mailbox: Arc<Mailbox>,
    my_name: String,
}

#[async_trait]
impl Tool for TeamTaskClaimTool {
    fn name(&self) -> &'static str {
        "TeamTaskClaim"
    }
    fn description(&self) -> &'static str {
        "Claim a pending task from the task queue. Only unclaimed, unblocked tasks \
         can be claimed. Use TeamTaskList to see available tasks."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Task ID to claim"}
            },
            "required": ["task_id"]
        })
    }
    async fn call(&self, input: Value) -> Result<String> {
        let task_id = crate::tools::req_str(&input, "task_id")?;
        let tq = self.mailbox.task_queue();
        let task = tq.claim(task_id, &self.my_name)?;
        Ok(format!(
            "Claimed task #{}: {}\n\n{}",
            task.id, task.subject, task.description
        ))
    }
}

// ── TeamTaskComplete ────────────────────────────────────────────────

pub struct TeamTaskCompleteTool {
    mailbox: Arc<Mailbox>,
    my_name: String,
}

#[async_trait]
impl Tool for TeamTaskCompleteTool {
    fn name(&self) -> &'static str {
        "TeamTaskComplete"
    }
    fn description(&self) -> &'static str {
        "Mark a task as completed. Sends an idle notification to the lead so \
         more work can be assigned."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Task ID to complete"},
                "summary": {"type": "string", "description": "Brief summary of what was done"}
            },
            "required": ["task_id"]
        })
    }
    async fn call(&self, input: Value) -> Result<String> {
        let task_id = crate::tools::req_str(&input, "task_id")?;
        let summary = input.get("summary").and_then(Value::as_str);
        let tq = self.mailbox.task_queue();
        let task = tq.complete(task_id, &self.my_name)?;

        // Send idle notification to lead.
        let idle_msg =
            make_idle_notification(&self.my_name, Some(&task.id), Some("completed"), summary);
        let notification = TeamMessage::new(&self.my_name, &idle_msg);
        self.mailbox.write_to_mailbox("lead", notification)?;

        Ok(format!("Task #{} completed.", task.id))
    }
}

// ── TeamMerge (lead-only) ───────────────────────────────────────────

pub struct TeamMergeTool {
    mailbox: Arc<Mailbox>,
}

#[async_trait]
impl Tool for TeamMergeTool {
    fn name(&self) -> &'static str {
        "TeamMerge"
    }
    fn description(&self) -> &'static str {
        "Lead-only. Merge teammate worktree branches (`team/<name>`) into a target branch. \
         Reports commit counts, conflicts, and optionally cleans up merged worktrees + branches. \
         Use when backend / frontend / other teammates have pushed work on their worktree branches \
         and you need to deliver the aggregated result back to a shared branch."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "into": {
                    "type": "string",
                    "description": "Target branch to merge into. Default: the repo's current branch."
                },
                "only": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional allow-list of teammate names. If omitted, every `team/*` branch with commits ahead of the target is considered."
                },
                "cleanup": {
                    "type": "boolean",
                    "description": "After a successful merge, remove the worktree at .worktrees/<name> and delete the merged branch. Default: false."
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "Only report what would be merged; don't actually merge. Default: false."
                }
            }
        })
    }
    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let project_root = std::env::current_dir().map_err(|e| Error::Tool(format!("cwd: {e}")))?;

        // Resolve target branch.
        let into = if let Some(b) = input.get("into").and_then(Value::as_str) {
            b.to_string()
        } else {
            let out = std::process::Command::new("git")
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .current_dir(&project_root)
                .output()
                .map_err(|e| Error::Tool(format!("git: {e}")))?;
            if !out.status.success() {
                return Err(Error::Tool("not a git repository (no HEAD)".into()));
            }
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        let only_filter: Option<Vec<String>> =
            input.get("only").and_then(Value::as_array).map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            });
        let cleanup = input
            .get("cleanup")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let dry_run = input
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // List team/* branches.
        let branches_out = std::process::Command::new("git")
            .args([
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/team/",
            ])
            .current_dir(&project_root)
            .output()
            .map_err(|e| Error::Tool(format!("git for-each-ref: {e}")))?;
        let raw_branches = String::from_utf8_lossy(&branches_out.stdout);
        let mut candidates: Vec<(String, String)> = raw_branches
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| {
                let name = l.strip_prefix("team/")?.to_string();
                Some((name, l.to_string()))
            })
            .collect();
        if let Some(ref allow) = only_filter {
            candidates.retain(|(n, _)| allow.iter().any(|a| a == n));
        }
        if candidates.is_empty() {
            return Ok(format!("No team/* branches found to merge into '{into}'."));
        }

        // A dirty lead working tree (or untracked-file collisions) makes
        // `git merge` fail with ZERO conflicted files — which the failure
        // path below would otherwise misreport as a teammate conflict and
        // bounce to a teammate who has nothing to fix. Refuse up front.
        if !dry_run {
            let status_out = std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&project_root)
                .output()
                .map_err(|e| Error::Tool(format!("git status: {e}")))?;
            if !status_out.stdout.is_empty() {
                return Err(Error::Tool(format!(
                    "lead working tree is dirty or has untracked collisions — \
                     commit or stash before TeamMerge:\n{}",
                    String::from_utf8_lossy(&status_out.stdout).trim()
                )));
            }
        }

        // For each candidate: count commits ahead, status, optionally merge.
        let mut report = Vec::new();
        report.push(format!("Merge target: {into}"));
        if dry_run {
            report.push("(dry run — no changes made)".into());
        }

        for (name, branch) in &candidates {
            let ahead_out = std::process::Command::new("git")
                .args(["rev-list", "--count", &format!("{into}..{branch}")])
                .current_dir(&project_root)
                .output();
            let ahead: u32 = match ahead_out {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse()
                    .unwrap_or(0),
                _ => 0,
            };
            if ahead == 0 {
                report.push(format!("  {name} ({branch}): 0 commits ahead — skipped"));
                if cleanup && !dry_run {
                    // 0-ahead usually means the teammate is STILL WORKING and
                    // hasn't committed yet — removing its worktree would yank
                    // the cwd out from under a live process and lose
                    // uncommitted work. Only clean up a stopped teammate, and
                    // never --force (preserve a dirty worktree).
                    let stopped = self
                        .mailbox
                        .read_status(name)
                        .map(|s| s.status == "stopped")
                        .unwrap_or(false);
                    if stopped {
                        let _ = std::process::Command::new("git")
                            .args(["worktree", "remove", &format!(".worktrees/{name}")])
                            .current_dir(&project_root)
                            .output();
                        let _ = std::process::Command::new("git")
                            .args(["branch", "-D", branch])
                            .current_dir(&project_root)
                            .output();
                        report.push("    cleaned up worktree + branch".to_string());
                    } else {
                        report.push(format!(
                            "    {name}: teammate still active — not cleaning up worktree"
                        ));
                    }
                }
                continue;
            }

            if dry_run {
                report.push(format!(
                    "  {name} ({branch}): {ahead} commit(s) ahead — would merge"
                ));
                continue;
            }

            let merge_out = std::process::Command::new("git")
                .args(["merge", "--no-ff", "--no-edit", branch])
                .current_dir(&project_root)
                .output()
                .map_err(|e| Error::Tool(format!("git merge: {e}")))?;
            if merge_out.status.success() {
                report.push(format!("  {name} ({branch}): merged {ahead} commit(s) ✓"));
                if cleanup {
                    let wt_remove = std::process::Command::new("git")
                        .args([
                            "worktree",
                            "remove",
                            "--force",
                            &format!(".worktrees/{name}"),
                        ])
                        .current_dir(&project_root)
                        .output();
                    let br_delete = std::process::Command::new("git")
                        .args(["branch", "-d", branch])
                        .current_dir(&project_root)
                        .output();
                    let wt_ok = wt_remove
                        .as_ref()
                        .map(|o| o.status.success())
                        .unwrap_or(false);
                    let br_ok = br_delete
                        .as_ref()
                        .map(|o| o.status.success())
                        .unwrap_or(false);
                    report.push(format!(
                        "    cleanup: worktree {} branch {}",
                        if wt_ok { "removed" } else { "kept" },
                        if br_ok { "deleted" } else { "kept" },
                    ));
                }
            } else {
                let stderr = String::from_utf8_lossy(&merge_out.stderr);
                // Collect conflicted files before aborting.
                let diff_out = std::process::Command::new("git")
                    .args(["diff", "--name-only", "--diff-filter=U"])
                    .current_dir(&project_root)
                    .output();
                let conflicts = diff_out
                    .map(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .lines()
                            .map(String::from)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                // Only abort if a merge actually started, else `git merge
                // --abort` errors spuriously and masks the real cause.
                if project_root.join(".git/MERGE_HEAD").exists() {
                    let _ = std::process::Command::new("git")
                        .args(["merge", "--abort"])
                        .current_dir(&project_root)
                        .output();
                }
                report.push(format!(
                    "  {name} ({branch}): merge FAILED, aborted. stderr: {}",
                    stderr.trim()
                ));
                if conflicts.is_empty() {
                    // No conflicted files → NOT a teammate-fixable content
                    // conflict (dirty tree, untracked-overwrite, --no-ff
                    // refusal). Surface as a lead-side error; do NOT bounce it
                    // to a teammate who has nothing to fix.
                    report.push(format!(
                        "    merge could not start — lead-side git error, not a teammate \
                         conflict: {}",
                        stderr.trim()
                    ));
                } else {
                    report.push(format!("    conflicts in: {}", conflicts.join(", ")));
                    report.push(format!(
                        "    delegate a fix to '{name}' (their worktree still has the changes), \
                         then re-run TeamMerge."
                    ));
                }
                // Stop on first failure so the lead deals with it before continuing.
                break;
            }
        }

        Ok(report.join("\n"))
    }
}

// ── Register all team tools ─────────────────────────────────────────

pub fn register_team_tools(registry: &mut ToolRegistry, my_name: &str) -> Arc<Mailbox> {
    // Honour THCLAWS_TEAM_DIR so teammates running in a git worktree write
    // inbox/task/status files back to the shared project team dir instead of
    // a stray `.thclaws/team/` inside their worktree cwd.
    let team_dir = std::env::var("THCLAWS_TEAM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Mailbox::default_dir());
    let mailbox = Arc::new(Mailbox::new(team_dir));
    let name = my_name.to_string();

    registry.register(Arc::new(TeamCreateTool {
        mailbox: mailbox.clone(),
    }));
    registry.register(Arc::new(SpawnTeammateTool {
        mailbox: mailbox.clone(),
        my_name: name.clone(),
    }));
    registry.register(Arc::new(SendMessageTool {
        mailbox: mailbox.clone(),
        my_name: name.clone(),
    }));
    registry.register(Arc::new(CheckInboxTool {
        mailbox: mailbox.clone(),
        my_name: name.clone(),
    }));
    registry.register(Arc::new(TeamStatusTool {
        mailbox: mailbox.clone(),
    }));
    registry.register(Arc::new(TeamTaskCreateTool {
        mailbox: mailbox.clone(),
    }));
    registry.register(Arc::new(TeamTaskListTool {
        mailbox: mailbox.clone(),
    }));
    registry.register(Arc::new(TeamTaskClaimTool {
        mailbox: mailbox.clone(),
        my_name: name.clone(),
    }));
    registry.register(Arc::new(TeamTaskCompleteTool {
        mailbox: mailbox.clone(),
        my_name: name.clone(),
    }));
    // TeamMerge is lead-only: teammates should never merge each other's branches.
    if name == "lead" {
        registry.register(Arc::new(TeamMergeTool {
            mailbox: mailbox.clone(),
        }));
    }
    mailbox
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// #202: a registered background teammate that is still running gets
    /// hard-killed and reaped by the handle, no `pkill` involved; one
    /// that already exited is pruned without being counted.
    #[test]
    fn kill_spawned_children_reaps_live_handles_only() {
        fn sleeper(secs: &str) -> std::process::Child {
            let mut c = if cfg!(windows) {
                let mut c = std::process::Command::new("cmd");
                c.args(["/C", &format!("ping -n {} 127.0.0.1 >nul", secs)]);
                c
            } else {
                let mut c = std::process::Command::new("sleep");
                c.arg(secs);
                c
            };
            c.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        }
        let _ = kill_spawned_children();

        let mut done = sleeper("0");
        let _ = done.wait();
        register_spawned_child(done);
        let live = sleeper("30");
        let live_id = live.id();
        register_spawned_child(live);
        assert_eq!(spawned_children().lock().unwrap().len(), 1);

        assert_eq!(kill_spawned_children(), 1);
        assert!(spawned_children().lock().unwrap().is_empty());
        #[cfg(unix)]
        {
            let alive = std::process::Command::new("kill")
                .args(["-0", &live_id.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(!alive, "pid {live_id} should be dead after kill");
        }
        #[cfg(windows)]
        let _ = live_id;
    }

    /// The narrow "lead may resolve a merge conflict" exception is only
    /// active when BOTH (a) `.git/MERGE_HEAD` exists, AND (b) the target
    /// file currently contains `<<<<<<<` / `=======` / `>>>>>>>` markers.
    /// Both conditions are easy for an LLM to spoof in isolation, so we
    /// require both to coincide.
    #[test]
    fn lead_resolving_merge_conflict_requires_both_signals() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let conflicted = repo.join("package.json");
        std::fs::write(
            &conflicted,
            "<<<<<<< HEAD\n  a\n=======\n  b\n>>>>>>> branch\n",
        )
        .unwrap();
        let clean = repo.join("README.md");
        std::fs::write(&clean, "no markers here\n").unwrap();

        // No MERGE_HEAD yet → exception inactive even though file has markers.
        assert!(!lead_resolving_merge_conflict(&conflicted));

        // Add MERGE_HEAD → conflicted file unlocks; clean file stays locked.
        std::fs::write(repo.join(".git/MERGE_HEAD"), "deadbeef\n").unwrap();
        assert!(lead_resolving_merge_conflict(&conflicted));
        assert!(!lead_resolving_merge_conflict(&clean));

        // Non-existent file (e.g. lead trying to Write a fresh file
        // mid-merge) → no markers → no exception.
        assert!(!lead_resolving_merge_conflict(&repo.join("new-file.txt")));

        // Remove MERGE_HEAD → exception closes.
        std::fs::remove_file(repo.join(".git/MERGE_HEAD")).unwrap();
        assert!(!lead_resolving_merge_conflict(&conflicted));
    }

    #[test]
    fn mailbox_write_and_read() {
        let dir = tempdir().unwrap();
        let mb = Mailbox::new(dir.path().to_path_buf());
        mb.init_agent("alice").unwrap();

        let msg = TeamMessage::new("bob", "do the thing");
        mb.write_to_mailbox("alice", msg).unwrap();

        let msgs = mb.read_mailbox("alice").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from, "bob");
        assert_eq!(msgs[0].content(), "do the thing");
        assert!(!msgs[0].read);
    }

    #[test]
    fn read_unread_and_mark() {
        let dir = tempdir().unwrap();
        let mb = Mailbox::new(dir.path().to_path_buf());
        mb.init_agent("alice").unwrap();

        mb.write_to_mailbox("alice", TeamMessage::new("bob", "msg1"))
            .unwrap();
        mb.write_to_mailbox("alice", TeamMessage::new("bob", "msg2"))
            .unwrap();

        let unread = mb.read_unread("alice").unwrap();
        assert_eq!(unread.len(), 2);

        mb.mark_as_read("alice", &[unread[0].id.clone()]).unwrap();

        let unread2 = mb.read_unread("alice").unwrap();
        assert_eq!(unread2.len(), 1);
        assert_eq!(unread2[0].content(), "msg2");

        // All messages still in mailbox.
        let all = mb.read_mailbox("alice").unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn task_queue_create_and_claim() {
        let dir = tempdir().unwrap();
        let tq = TaskQueue::new(dir.path().join("tasks"));

        let t1 = tq
            .create("build API", "Create REST endpoints", &[], None)
            .unwrap();
        assert_eq!(t1.id, "1");
        assert_eq!(t1.status, TaskStatus::Pending);

        let t2 = tq
            .create("build UI", "Create React app", &[t1.id.clone()], None)
            .unwrap();
        assert_eq!(t2.id, "2");

        // Can't claim t2 (blocked by t1).
        assert!(tq.claim("2", "frontend").is_err());

        // Claim t1.
        let claimed = tq.claim("1", "backend").unwrap();
        assert_eq!(claimed.status, TaskStatus::InProgress);
        assert_eq!(claimed.owner.as_deref(), Some("backend"));

        // Can't claim t1 again.
        assert!(tq.claim("1", "another").is_err());

        // Complete t1.
        let done = tq.complete("1", "backend").unwrap();
        assert_eq!(done.status, TaskStatus::Completed);

        // Now t2 is unblocked.
        let claimed2 = tq.claim("2", "frontend").unwrap();
        assert_eq!(claimed2.status, TaskStatus::InProgress);
    }

    #[test]
    fn task_queue_claim_next() {
        let dir = tempdir().unwrap();
        let tq = TaskQueue::new(dir.path().join("tasks"));

        tq.create("task A", "do A", &[], None).unwrap();
        tq.create("task B", "do B", &[], None).unwrap();

        let next = tq.claim_next("worker1").unwrap();
        assert!(next.is_some());
        assert_eq!(next.unwrap().id, "1");

        let next2 = tq.claim_next("worker2").unwrap();
        assert!(next2.is_some());
        assert_eq!(next2.unwrap().id, "2");

        let next3 = tq.claim_next("worker3").unwrap();
        assert!(next3.is_none()); // all claimed
    }

    #[test]
    fn task_queue_preassigned_owner() {
        let dir = tempdir().unwrap();
        let tq = TaskQueue::new(dir.path().join("tasks"));

        // Lead pre-assigns task #1 to backend; task #2 is unowned.
        let t1 = tq
            .create("API work", "write spec", &[], Some("backend"))
            .unwrap();
        assert_eq!(t1.owner.as_deref(), Some("backend"));
        tq.create("chore", "anything", &[], None).unwrap();

        // QA must not auto-claim the reserved task; it gets the unowned one.
        let picked = tq.claim_next("qa").unwrap().unwrap();
        assert_eq!(picked.id, "2");
        // Complete that one so QA is no longer busy (claim's busy-check).
        tq.complete("2", "qa").unwrap();

        // QA cannot manually claim the reserved task either.
        let err = tq.claim("1", "qa").unwrap_err();
        assert!(
            format!("{err}").contains("reserved for backend"),
            "unexpected error: {err}"
        );

        // Backend can claim its reserved task.
        let claimed = tq.claim("1", "backend").unwrap();
        assert_eq!(claimed.status, TaskStatus::InProgress);
        assert_eq!(claimed.owner.as_deref(), Some("backend"));
    }

    #[test]
    fn protocol_message_roundtrip() {
        let json =
            make_idle_notification("backend", Some("1"), Some("completed"), Some("built API"));
        let parsed = parse_protocol_message(&json).unwrap();
        match parsed {
            ProtocolMessage::IdleNotification {
                from,
                completed_task_id,
                ..
            } => {
                assert_eq!(from, "backend");
                assert_eq!(completed_task_id.as_deref(), Some("1"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn status_write_and_read() {
        let dir = tempdir().unwrap();
        let mb = Mailbox::new(dir.path().to_path_buf());
        mb.init_agent("alice").unwrap();
        mb.write_status("alice", "working", Some("1")).unwrap();

        let s = mb.read_status("alice").unwrap();
        assert_eq!(s.status, "working");
        assert_eq!(s.current_task.as_deref(), Some("1"));
    }

    #[test]
    fn all_status_lists_agents() {
        let dir = tempdir().unwrap();
        let mb = Mailbox::new(dir.path().to_path_buf());
        mb.init_agent("alice").unwrap();
        mb.init_agent("bob").unwrap();

        let statuses = mb.all_status().unwrap();
        assert_eq!(statuses.len(), 2);
    }

    /// M6.34 TEAM1: agent name validator pins exactly which strings
    /// can become filesystem path / git ref components. Anything
    /// rejected here can never reach `inbox_path` /
    /// `output_log_path` / `worktree_path` / `team/<name>` branch.
    #[test]
    fn is_valid_agent_name_accepts_normal_names() {
        assert!(is_valid_agent_name("lead"));
        assert!(is_valid_agent_name("frontend"));
        assert!(is_valid_agent_name("backend-2"));
        assert!(is_valid_agent_name("qa_team"));
        assert!(is_valid_agent_name("a"));
        assert!(is_valid_agent_name("agent01"));
        assert!(is_valid_agent_name("_internal"));
        assert!(is_valid_agent_name(&"x".repeat(64)));
    }

    #[test]
    fn is_valid_agent_name_rejects_path_traversal() {
        // Pre-fix these would have flowed straight into `format!("{name}.json")`
        // and escaped the inboxes/ directory.
        assert!(!is_valid_agent_name(".."));
        assert!(!is_valid_agent_name("../foo"));
        assert!(!is_valid_agent_name("../../sessions/abc"));
        assert!(!is_valid_agent_name("foo/bar"));
        assert!(!is_valid_agent_name("foo\\bar"));
        assert!(!is_valid_agent_name(".hidden"));
    }

    #[test]
    fn is_valid_agent_name_rejects_shell_metacharacters() {
        // Pre-fix these could inject shell statements via SpawnTeammate's
        // un-escaped `--team-agent {name}` interpolation.
        assert!(!is_valid_agent_name("foo; rm -rf /"));
        assert!(!is_valid_agent_name("foo`whoami`"));
        assert!(!is_valid_agent_name("foo$bar"));
        assert!(!is_valid_agent_name("foo bar"));
        assert!(!is_valid_agent_name("foo\nbar"));
        assert!(!is_valid_agent_name("foo'bar"));
        assert!(!is_valid_agent_name("foo\"bar"));
        assert!(!is_valid_agent_name("foo|bar"));
        assert!(!is_valid_agent_name("foo&bar"));
    }

    #[test]
    fn is_valid_agent_name_rejects_edges() {
        assert!(!is_valid_agent_name(""));
        assert!(!is_valid_agent_name(&"x".repeat(65)));
        // Leading dash isn't allowed because `--team-agent -foo` would
        // be parsed as an option by argparse.
        assert!(!is_valid_agent_name("-foo"));
    }

    /// M6.34 TEAM1 defense-in-depth — Mailbox::init_agent must reject
    /// invalid names at the storage boundary even when the caller
    /// (TeamCreate / SpawnTeammate / SendMessage) skipped validation.
    #[test]
    fn mailbox_init_agent_rejects_invalid_name() {
        let dir = tempdir().unwrap();
        let mb = Mailbox::new(dir.path().to_path_buf());
        let err = mb.init_agent("../escape").unwrap_err();
        assert!(
            format!("{err}").contains("invalid agent name"),
            "expected path-traversal rejection, got: {err}"
        );
        // Verify NO files leaked outside the team_dir.
        let escape_target = dir.path().parent().unwrap().join("escape");
        assert!(
            !escape_target.exists(),
            "init_agent('../escape') must not create anything at {}",
            escape_target.display()
        );
    }

    #[test]
    fn mailbox_write_to_mailbox_rejects_invalid_recipient() {
        let dir = tempdir().unwrap();
        let mb = Mailbox::new(dir.path().to_path_buf());
        mb.init_agent("alice").unwrap();
        let err = mb
            .write_to_mailbox("../passwd", TeamMessage::new("attacker", "drop tables"))
            .unwrap_err();
        assert!(format!("{err}").contains("invalid recipient"));
    }

    /// M6.34 TEAM2: shell-escape helper pins POSIX single-quote
    /// behavior. Critical paths are values that make it into
    /// SpawnTeammate's agent_cmd shell string.
    /// The tmux path is the only one that still needs a shell string, and
    /// tmux is POSIX-only — so every interpolated value must be escaped
    /// there, including paths with spaces.
    #[test]
    fn spawn_shell_string_escapes_bin_args_env_and_cwd() {
        let got = spawn_shell_string(
            "/Applications/My App/thclaws",
            &[
                "--cli".to_string(),
                "--team-agent".to_string(),
                "qa".to_string(),
                "--team-dir".to_string(),
                "/Users/Some One/proj/.thclaws/team".to_string(),
            ],
            &[(
                "THCLAWS_PROJECT_ROOT".to_string(),
                "/Users/Some One/proj".to_string(),
            )],
            Some("/Users/Some One/proj"),
        );
        assert_eq!(
            got,
            "cd '/Users/Some One/proj' && THCLAWS_PROJECT_ROOT='/Users/Some One/proj' \
'/Applications/My App/thclaws' '--cli' '--team-agent' 'qa' '--team-dir' \
'/Users/Some One/proj/.thclaws/team'"
        );
    }

    #[test]
    fn spawn_shell_string_omits_the_cd_when_there_is_no_cwd() {
        let got = spawn_shell_string("thclaws", &["--cli".to_string()], &[], None);
        assert_eq!(got, "'thclaws' '--cli'");
        assert!(!got.contains("cd "));
    }

    #[test]
    fn shell_escape_wraps_in_single_quotes() {
        assert_eq!(shell_escape("simple"), "'simple'");
        assert_eq!(shell_escape("/tmp/path"), "'/tmp/path'");
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn shell_escape_handles_embedded_single_quote() {
        // `it's` becomes `'it'\''s'` — the standard POSIX double-quote
        // dance. Sequence: close quote, escaped quote, reopen quote.
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
        assert_eq!(shell_escape("'"), "''\\'''");
    }

    #[test]
    fn shell_escape_neutralizes_injection_metacharacters() {
        // Pre-fix interpolation of these straight into a shell string
        // would inject statements / subshells / variable expansion.
        // After escape, they're literal arguments to the inner command.
        let evil = "foo; rm -rf $HOME && echo `pwn`";
        let escaped = shell_escape(evil);
        // Must wrap in single quotes — every metachar inside is now literal.
        assert!(escaped.starts_with('\''));
        assert!(escaped.ends_with('\''));
        // No bare metachars outside the quoted region.
        assert!(!escaped.starts_with('"'));
    }

    #[test]
    fn team_config_legacy_migration() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.json");
        let legacy = r#"{
            "name": "test",
            "agents": [
                {"name": "alice", "role": "dev", "instructions": "code stuff"}
            ]
        }"#;
        std::fs::write(&path, legacy).unwrap();

        let config = TeamConfig::load(&path).unwrap();
        assert_eq!(config.members.len(), 1);
        assert_eq!(config.members[0].name, "alice");
        assert_eq!(config.members[0].prompt, "code stuff");
    }

    // F2: a corrupt inbox must NOT be silently wiped on the next write —
    // it errors and is moved aside to .corrupt so the messages survive.
    #[test]
    fn corrupt_inbox_is_preserved_not_clobbered() {
        let dir = tempdir().unwrap();
        let mb = Mailbox::new(dir.path().to_path_buf());
        mb.init_agent("alice").unwrap();
        mb.write_to_mailbox("alice", TeamMessage::new("bob", "keep me"))
            .unwrap();
        let inbox = dir.path().join("inboxes").join("alice.json");
        std::fs::write(&inbox, "{ not valid json").unwrap();
        // The overwrite aborts (Err) instead of wiping.
        assert!(mb
            .write_to_mailbox("alice", TeamMessage::new("bob", "new"))
            .is_err());
        // Corrupt content is moved aside for recovery.
        assert!(dir.path().join("inboxes").join("alice.corrupt").exists());
    }

    // F12: a task owned by a dead (stopped/stale) teammate is reclaimed.
    #[test]
    fn reap_releases_dead_owner_task() {
        let dir = tempdir().unwrap();
        let mb = Mailbox::new(dir.path().to_path_buf());
        mb.init_agent("worker").unwrap();
        let tq = mb.task_queue();
        let t = tq.create("do x", "details", &[], None).unwrap();
        tq.claim(&t.id, "worker").unwrap();
        mb.write_status("worker", "stopped", None).unwrap();
        assert_eq!(mb.reap_stale_tasks().unwrap(), 1);
        let after = tq.get(&t.id).unwrap().unwrap();
        assert_eq!(after.status, TaskStatus::Pending);
        assert!(after.owner.is_none());
    }

    // F6/F17: heartbeat staleness + never-booted detection.
    #[test]
    fn status_staleness_detection() {
        let mk = |status: &str, hb: u64| AgentStatus {
            agent: "a".into(),
            status: status.into(),
            current_task: None,
            last_heartbeat: hb,
        };
        assert!(!mk("idle", now_secs()).is_stale());
        assert!(mk("idle", now_secs().saturating_sub(STALE_SECS + 5)).is_stale());
        assert!(mk("spawning", now_secs()).never_booted());
        assert!(!mk("idle", now_secs()).never_booted());
    }

    // F10: a self-referential blocked_by must not deadlock claim() (it
    // resolves deps before taking the task's own lock). Bounded by the
    // test harness — a hang would time the test out.
    #[test]
    fn claim_with_self_blocked_by_does_not_deadlock() {
        let dir = tempdir().unwrap();
        let mb = Mailbox::new(dir.path().to_path_buf());
        let tq = mb.task_queue();
        let t = tq.create("x", "y", &[], None).unwrap();
        // Hand-write a self-referential blocked_by.
        let mut task = tq.get(&t.id).unwrap().unwrap();
        task.blocked_by = vec![t.id.clone()];
        std::fs::write(
            dir.path().join("tasks").join(format!("{}.json", t.id)),
            serde_json::to_string_pretty(&task).unwrap(),
        )
        .unwrap();
        // Returns (blocked, since the self-dep isn't Completed) — must not hang.
        let _ = tq.claim(&t.id, "worker");
    }
}
