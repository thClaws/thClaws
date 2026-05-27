//! Session persistence.
//!
//! A session is a saved conversation: metadata + message history stored as
//! append-only JSONL. Sessions live under `~/.local/share/thclaws/sessions/`
//! (XDG data dir convention) as individual `.jsonl` files, one per session id.
//!
//! File format (Claude Code style):
//! - First line: metadata header `{"type":"header","id":...,"model":...,"cwd":...,"created_at":...}`
//! - Subsequent lines: message events `{"type":"user"|"assistant"|"system","content":[...],"timestamp":N}`
//!
//! Design choices:
//! - Session ids are derived from a nanosecond timestamp, so they're unique
//!   and naturally sort chronologically.
//! - `sync()` only appends new messages since `last_saved_count`.
//! - `load()` reads the JSONL and reconstructs the full `Session`.
//! - `SessionStore` is just a directory. No db, no lock file.

use crate::error::{Error, Result};
use crate::types::Message;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// M6.24 BUG M4: serialize concurrent JSONL writes via OS-level
/// advisory file lock. Pre-fix two thClaws processes against the
/// same project session dir could interleave bytes mid-line because
/// POSIX `O_APPEND` is per-write atomic only ≤ PIPE_BUF (~4 KB);
/// tool_use lines with large content easily exceed that. With M6.19
/// H1's per-line skip-with-warning fix in place, the practical
/// impact dropped from "session disappears" to "occasional warning
/// + dropped corrupt line" — but corrupt lines are still corrupt
/// data. Locking eliminates the interleave entirely. Acquire
/// exclusive lock before each write; release at scope end via Drop.
///
/// ## Issue #90: Windows `os error 5` on lock acquisition
///
/// The open call MUST include `.read(true)`. On Windows, `append(true)`
/// alone produces a handle with the access mask
/// `FILE_GENERIC_WRITE & !FILE_WRITE_DATA` — i.e. `FILE_APPEND_DATA +
/// FILE_WRITE_ATTRIBUTES + ...`, no `GENERIC_READ`. The Win32
/// `LockFileEx` API the `fs2` crate calls under the hood requires the
/// file handle to have `GENERIC_READ` or `GENERIC_WRITE` access; an
/// append-only handle fails the access check and returns
/// `ERROR_ACCESS_DENIED` (os error 5). Symptom on Windows: every
/// session save fails with `save failed: config error: session lock:
/// Access is denied. (os error 5)`, JSONL files end up zero bytes,
/// session restore shows empty history. POSIX `flock` doesn't have
/// this requirement, so macOS / Linux work fine with append-only.
/// Adding `.read(true)` fixes Windows without any behavior change on
/// POSIX. See <https://github.com/thClaws/thClaws/issues/90>.
fn append_locked<F>(path: &Path, write: F) -> Result<()>
where
    F: FnOnce(&mut File) -> std::io::Result<()>,
{
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    // `lock_exclusive` blocks until acquired. Cheap on uncontested
    // path (single process). On contention, waits for the other
    // process's append to complete.
    file.lock_exclusive()
        .map_err(|e| Error::Config(format!("session lock: {e}")))?;
    let result = write(&mut file);
    // Best-effort unlock — file's Drop closes the fd which also
    // releases the lock per POSIX flock semantics, so if unlock
    // fails we still don't deadlock the next writer.
    let _ = file.unlock();
    result.map_err(Error::from)
}

/// JSONL header line written once when a session is first created.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionHeader {
    #[serde(rename = "type")]
    kind: String, // always "header"
    id: String,
    model: String,
    cwd: String,
    created_at: u64,
}

/// A single message event line in the JSONL file.
///
/// `provider` + `model` are populated only for `assistant` lines so a
/// reader can attribute which model produced each turn. The session
/// header carries the model the session was created with; assistant
/// lines carry whatever model was active at write time. Today every
/// model switch mints a fresh session, so these are redundant with the
/// header — but the per-line attribution future-proofs scenarios where
/// model switching becomes mid-session (and is cheap to add now while
/// the schema is still under our control).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MessageEvent {
    #[serde(rename = "type")]
    kind: String, // "user", "assistant", "system"
    content: Vec<crate::types::ContentBlock>,
    timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

/// Append-only event for renaming an existing session. Keeps the JSONL
/// format strictly append-only — on load, the latest rename event wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RenameEvent {
    #[serde(rename = "type")]
    kind: String, // always "rename"
    title: String,
    timestamp: u64,
}

/// Append-only snapshot of the active plan (M1+). Each `submit` /
/// `update_step` / `clear` writes one of these. On load, the latest
/// snapshot wins — `null` plan means "active plan was cleared". Keeps
/// the JSONL strictly append-only; older snapshots stay on disk for
/// audit and time-travel restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlanSnapshotEvent {
    #[serde(rename = "type")]
    kind: String, // always "plan_snapshot"
    plan: Option<crate::tools::plan_state::Plan>,
    timestamp: u64,
}

/// Same shape as PlanSnapshotEvent but for `/goal` state. Latest snapshot
/// wins on load — `null` goal means the active goal was cleared (status
/// moved to terminal or `/goal abandon`). Decoupled from PlanSnapshotEvent
/// so a session can carry a plan + goal independently.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GoalSnapshotEvent {
    #[serde(rename = "type")]
    kind: String, // always "goal_snapshot"
    goal: Option<crate::goal_state::GoalState>,
    timestamp: u64,
}

/// Append-only record of the provider's server-side session id. The
/// Anthropic Agent SDK (`anthropic-agent` provider) returns a UUID on
/// the first response frame and uses it to index server-side
/// conversation state; thClaws writes one of these events after every
/// turn that surfaces a new id so the next process / `/load` can pass
/// it back via `--resume <uuid>` and the SDK restores its history.
/// Latest event wins on load — same pattern as `rename`,
/// `plan_snapshot`, and `goal_snapshot`. Other providers never emit
/// this event.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProviderStateEvent {
    #[serde(rename = "type")]
    kind: String, // always "provider_state"
    provider_session_id: Option<String>,
    timestamp: u64,
}

/// Append-only checkpoint marking that the preceding message events
/// have been compacted (via `/compact` or similar). On load, the most
/// recent checkpoint "wins" — its `messages` list is used as the
/// starting history and any `message` events *after* it are appended.
/// Everything before the checkpoint is preserved on disk for audit.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompactionEvent {
    #[serde(rename = "type")]
    kind: String, // always "compaction"
    messages: Vec<CompactedMessage>,
    /// How many message events preceded this checkpoint — informational
    /// only; load logic walks the JSONL sequentially and resets on each
    /// checkpoint, so this isn't strictly required.
    #[serde(default)]
    replaces_count: usize,
    timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompactedMessage {
    role: String,
    content: Vec<crate::types::ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub model: String,
    pub cwd: String,
    pub messages: Vec<Message>,
    /// User-assigned title (set via `/rename`). `None` until the user picks
    /// one — display code should fall back to the session id prefix.
    #[serde(default)]
    pub title: Option<String>,
    /// How many messages have already been persisted to disk.
    #[serde(default)]
    pub last_saved_count: usize,
    /// Active plan (M1+). `None` when no plan-mode work is in flight.
    /// Persisted with the session JSONL so `/load` restores the plan
    /// alongside history — the right-side sidebar comes back populated.
    /// Cleared explicitly via `/plan cancel` or the sidebar Cancel
    /// button (not by `/load` itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<crate::tools::plan_state::Plan>,
    /// Active goal (M6.29 + Phase A sidebar). `None` when no goal is in
    /// flight. Persisted alongside the conversation so `/load` restores
    /// the goal sidebar with elapsed iterations + token consumption
    /// intact. Cleared via `/goal abandon` or by completing the goal
    /// (status moves to terminal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<crate::goal_state::GoalState>,
    /// Provider-side session identifier for resume support. The
    /// Anthropic Agent SDK provider (`anthropic-agent`) maintains its
    /// own server-side conversation indexed by a UUID it returns on
    /// the first response frame; thClaws persists that UUID here so
    /// the next process / `/load` can pass it back via `--resume
    /// <uuid>` and the SDK restores its history server-side. Without
    /// this, every resumed thClaws session became a fresh SDK
    /// conversation that saw only the latest user message — model
    /// appeared to "forget" everything from prior turns.
    /// Append-only via the `provider_state` event (latest wins on
    /// load). Other providers leave it `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.created_at == other.created_at
            && self.updated_at == other.updated_at
            && self.model == other.model
            && self.cwd == other.cwd
            && self.messages == other.messages
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    pub id: String,
    pub updated_at: u64,
    pub model: String,
    pub message_count: usize,
    pub title: Option<String>,
}

impl Session {
    pub fn new(model: impl Into<String>, cwd: impl Into<String>) -> Self {
        let now = now_secs();
        Self {
            id: generate_id(),
            created_at: now,
            updated_at: now,
            model: model.into(),
            cwd: cwd.into(),
            messages: Vec::new(),
            title: None,
            last_saved_count: 0,
            plan: None,
            goal: None,
            provider_session_id: None,
        }
    }

    /// Sync the session with the latest agent history + bump `updated_at`.
    /// Only newly added messages (since `last_saved_count`) will be appended on
    /// the next save.
    pub fn sync(&mut self, messages: Vec<Message>) {
        self.messages = messages;
        self.updated_at = now_secs();
    }

    /// Write the JSONL header line for this session if the file is
    /// missing or empty. Idempotent — safe to call repeatedly. Used by
    /// the worker at session-mint time so the header is on disk
    /// BEFORE any `plan_snapshot` event (or other auxiliary write)
    /// can race in and create the file headerless. Pre-fix: a fresh
    /// session's first write was usually `append_plan_snapshot` from
    /// `plan_state::clear()`, which created the file without a header
    /// — `Session::append_to` then saw `path.exists() == true` and
    /// skipped its own header write. The resulting headerless JSONL
    /// failed `load_from` with "missing header line" and the session
    /// silently disappeared from `SessionStore::list`.
    pub fn write_header_if_missing(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let needs_header = !path.exists()
            || std::fs::metadata(path)
                .map(|m| m.len() == 0)
                .unwrap_or(true);
        if !needs_header {
            return Ok(());
        }
        // M6.24 BUG M4: lock the file across the empty-check + write
        // window. Without the lock, two processes could both observe
        // empty and both write the header line — double header. With
        // the lock, the second writer sees the file non-empty after
        // the first releases.
        let header = SessionHeader {
            kind: "header".into(),
            id: self.id.clone(),
            model: self.model.clone(),
            cwd: self.cwd.clone(),
            created_at: self.created_at,
        };
        let line = serde_json::to_string(&header)?;
        append_locked(path, |file| {
            // Re-check under lock — could have been written by another
            // process between our metadata check and the lock acquisition.
            let len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if len > 0 {
                return Ok(());
            }
            writeln!(file, "{}", line)?;
            Ok(())
        })
    }

    /// Append only the new messages (since `last_saved_count`) to the JSONL file.
    /// Writes the header line if the file doesn't exist yet.
    pub fn append_to(&mut self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Pre-build event payloads outside the lock so the critical
        // section is just I/O. M6.24 BUG M4: serialize concurrent
        // appenders (CLI + GUI in same project, two GUIs, etc.).
        let header_line = if !path.exists() {
            let header = SessionHeader {
                kind: "header".into(),
                id: self.id.clone(),
                model: self.model.clone(),
                cwd: self.cwd.clone(),
                created_at: self.created_at,
            };
            Some(serde_json::to_string(&header)?)
        } else {
            None
        };

        let new_messages = &self.messages[self.last_saved_count..];
        let now = now_secs();
        // Provider name is derived from the session's active model.
        // Cached once outside the loop since `self.model` is stable for
        // every message in this batch (model swaps mint a fresh session).
        let provider_name =
            crate::providers::ProviderKind::detect(&self.model).map(|k| k.name().to_string());
        let mut event_lines: Vec<String> = Vec::with_capacity(new_messages.len());
        for msg in new_messages {
            let role_str = match msg.role {
                crate::types::Role::User => "user",
                crate::types::Role::Assistant => "assistant",
                crate::types::Role::System => "system",
            };
            let is_assistant = matches!(msg.role, crate::types::Role::Assistant);
            let event = MessageEvent {
                kind: role_str.into(),
                content: msg.content.clone(),
                timestamp: now,
                provider: if is_assistant {
                    provider_name.clone()
                } else {
                    None
                },
                model: if is_assistant {
                    Some(self.model.clone())
                } else {
                    None
                },
            };
            event_lines.push(serde_json::to_string(&event)?);
        }

        append_locked(path, |file| {
            // Re-check under lock for header write — another process may
            // have created + headered the file between our `path.exists()`
            // check and the lock acquisition.
            if header_line.is_some() {
                let len = file.metadata().map(|m| m.len()).unwrap_or(0);
                if len == 0 {
                    if let Some(ref h) = header_line {
                        writeln!(file, "{}", h)?;
                    }
                }
            }
            for line in &event_lines {
                writeln!(file, "{}", line)?;
            }
            Ok(())
        })?;

        self.last_saved_count = self.messages.len();
        Ok(())
    }

    /// Legacy save method — now delegates to append_to for compatibility.
    pub fn save_to(&mut self, path: &Path) -> Result<()> {
        self.append_to(path)
    }

    /// Append a plan snapshot to the JSONL. Called from the GUI worker
    /// after every `plan_state` mutation so a `/load` restores the
    /// most recent plan along with the conversation history. M1+.
    pub fn append_plan_snapshot_to(
        &mut self,
        path: &Path,
        plan: Option<&crate::tools::plan_state::Plan>,
    ) -> Result<()> {
        append_plan_snapshot(path, plan)?;
        self.plan = plan.cloned();
        self.updated_at = now_secs();
        Ok(())
    }

    /// Append a goal snapshot to the JSONL. Same contract as
    /// `append_plan_snapshot_to` — fires after every `goal_state`
    /// mutation so a `/load` restores the goal sidebar (objective,
    /// elapsed iterations, token consumption, status) intact.
    pub fn append_goal_snapshot_to(
        &mut self,
        path: &Path,
        goal: Option<&crate::goal_state::GoalState>,
    ) -> Result<()> {
        append_goal_snapshot(path, goal)?;
        self.goal = goal.cloned();
        self.updated_at = now_secs();
        Ok(())
    }

    /// Load only the metadata (id, model, title, message count, last
    /// activity timestamp) from a JSONL file. Streams the file
    /// line-by-line WITHOUT keeping message bodies in memory — used by
    /// `SessionStore::list()` to render the sidebar without paying for
    /// full deserialization of every session's history.
    ///
    /// M6.24 BUG M3: pre-fix `SessionStore::list()` called `load_from`
    /// for every session, which deserialized every message body into
    /// `Vec<Message>` just to count them and grab the last timestamp.
    /// For a project with hundreds of sessions of multi-MB JSONL each,
    /// the sidebar refresh read + parsed hundreds of MB on every
    /// `SessionListRefresh`. Streaming meta-only avoids the body
    /// deserialization entirely; we just keep a running count + the
    /// most recent message timestamp.
    ///
    /// Same per-line skip-with-warning behavior as `load_from` (corrupt
    /// lines logged + skipped). Same headerless-file salvage path.
    pub fn load_meta_from(path: &Path) -> Result<SessionMeta> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);

        let mut header: Option<SessionHeader> = None;
        let mut last_timestamp = 0u64;
        let mut title: Option<String> = None;
        let mut message_count = 0usize;
        let mut skipped = 0usize;

        for line_result in reader.lines() {
            let line = match line_result {
                Ok(l) => l,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            // Parse only the discriminator + the few fields we care
            // about. Avoid `from_value::<MessageEvent>` because that
            // would deserialize the full content vec.
            let val: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let kind = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

            match kind {
                "header" => {
                    if let Ok(h) = serde_json::from_value::<SessionHeader>(val) {
                        header = Some(h);
                    } else {
                        skipped += 1;
                    }
                }
                "rename" => {
                    if let Some(ts) = val.get("timestamp").and_then(|v| v.as_u64()) {
                        if ts > last_timestamp {
                            last_timestamp = ts;
                        }
                    }
                    let t = val
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    title = if t.is_empty() {
                        None
                    } else {
                        Some(t.to_string())
                    };
                }
                "compaction" => {
                    if let Some(ts) = val.get("timestamp").and_then(|v| v.as_u64()) {
                        if ts > last_timestamp {
                            last_timestamp = ts;
                        }
                    }
                    // Reset count to whatever the checkpoint contains —
                    // matches load_from's behavior.
                    message_count = val
                        .get("messages")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                }
                "plan_snapshot" | "goal_snapshot" => {
                    // Per M6.16.1: do NOT bump last_timestamp from
                    // snapshot events (restore-on-load fires the
                    // broadcaster which writes a fresh snapshot —
                    // not user activity). Same rule for goal_snapshot.
                }
                "provider_state" => {
                    // Provider-side session id capture (Agent SDK
                    // resume support). Doesn't carry a message body
                    // or activity timestamp — meta scan ignores it
                    // entirely. The full load_from path consumes it
                    // for the provider_session_id field on Session.
                    // Pre-fix, the unrecognised `_` arm flagged
                    // every AgentSdk session's file as "1 corrupt
                    // line" on every sidebar refresh (May 2026 user
                    // report).
                }
                "user" | "assistant" | "system" => {
                    if let Some(ts) = val.get("timestamp").and_then(|v| v.as_u64()) {
                        if ts > last_timestamp {
                            last_timestamp = ts;
                        }
                    }
                    message_count += 1;
                }
                _ => {
                    skipped += 1;
                }
            }
        }

        if skipped > 0 {
            eprintln!(
                "\x1b[33m[session] {}: meta scan skipped {skipped} corrupt line(s)\x1b[0m",
                path.display()
            );
        }

        // Headerless-file salvage path — mirrors load_from.
        let h = header.unwrap_or_else(|| {
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            let created_at = std::fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            SessionHeader {
                kind: "header".into(),
                id,
                model: "unknown".into(),
                cwd: String::new(),
                created_at,
            }
        });

        Ok(SessionMeta {
            id: h.id,
            updated_at: if last_timestamp > 0 {
                last_timestamp
            } else {
                h.created_at
            },
            model: h.model,
            message_count,
            title,
        })
    }

    /// Load a session from a JSONL file. Reads the header + all message events.
    ///
    /// M6.19 BUG H1: per-line errors (malformed JSON, invalid UTF-8,
    /// unknown role, mid-write fragments from disk-full / kill -9 /
    /// cross-process race) are now logged + skipped instead of failing
    /// the entire load. Pre-fix a single corrupt line silently dropped
    /// the whole session from `SessionStore::list()` (which catches the
    /// load Err and skips), making sessions invisible in the sidebar
    /// with no surface to the user. Skip-with-warning preserves every
    /// recoverable message; the warning goes to stderr so it's
    /// debuggable but doesn't block the user.
    pub fn load_from(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);

        let mut header: Option<SessionHeader> = None;
        let mut messages = Vec::new();
        let mut last_timestamp = 0u64;
        let mut title: Option<String> = None;
        let mut plan: Option<crate::tools::plan_state::Plan> = None;
        let mut goal: Option<crate::goal_state::GoalState> = None;
        // Latest-wins for provider session id, same pattern as title /
        // plan_snapshot / goal_snapshot. `None` after replay means the
        // provider doesn't persist server-side state (anything other
        // than `anthropic-agent` today).
        let mut provider_session_id: Option<String> = None;
        let mut skipped: usize = 0;

        for (line_num, line_result) in reader.lines().enumerate() {
            let line = match line_result {
                Ok(l) => l,
                Err(e) => {
                    // Invalid UTF-8 or other I/O error mid-stream.
                    // Skip the line and keep going.
                    eprintln!(
                        "\x1b[33m[session] {}:{}: skipping corrupt line ({e})\x1b[0m",
                        path.display(),
                        line_num + 1
                    );
                    skipped += 1;
                    continue;
                }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let val: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!(
                        "\x1b[33m[session] {}:{}: skipping malformed JSON ({e})\x1b[0m",
                        path.display(),
                        line_num + 1
                    );
                    skipped += 1;
                    continue;
                }
            };

            let kind = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

            if kind == "header" {
                match serde_json::from_value::<SessionHeader>(val) {
                    Ok(h) => header = Some(h),
                    Err(e) => {
                        eprintln!(
                            "\x1b[33m[session] {}:{}: skipping malformed header ({e})\x1b[0m",
                            path.display(),
                            line_num + 1
                        );
                        skipped += 1;
                    }
                }
            } else if kind == "rename" {
                // Latest rename wins.
                let ev: RenameEvent = match serde_json::from_value(val) {
                    Ok(ev) => ev,
                    Err(e) => {
                        eprintln!(
                            "\x1b[33m[session] {}:{}: skipping malformed rename ({e})\x1b[0m",
                            path.display(),
                            line_num + 1
                        );
                        skipped += 1;
                        continue;
                    }
                };
                if ev.timestamp > last_timestamp {
                    last_timestamp = ev.timestamp;
                }
                let trimmed = ev.title.trim();
                title = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            } else if kind == "plan_snapshot" {
                // Latest snapshot wins. `null` plan means the active
                // plan was cleared (M1+).
                //
                // Deliberately do NOT bump `last_timestamp` from a
                // plan_snapshot event. Loading a session triggers
                // `plan_state::restore_from_session`, which fires the
                // broadcaster and writes a plan_snapshot with the
                // current wall-clock time — purely a state-restoration
                // artifact, not user activity. Without this guard the
                // sidebar's "most-recently-used" sort would jump the
                // just-clicked session to the top, masking the actual
                // recency ordering. Sort recency now tracks real
                // message / rename / compaction events only.
                let ev: PlanSnapshotEvent = match serde_json::from_value(val) {
                    Ok(ev) => ev,
                    Err(e) => {
                        eprintln!(
                            "\x1b[33m[session] {}:{}: skipping malformed plan_snapshot ({e})\x1b[0m",
                            path.display(),
                            line_num + 1
                        );
                        skipped += 1;
                        continue;
                    }
                };
                plan = ev.plan;
            } else if kind == "goal_snapshot" {
                // Latest goal_snapshot wins. `null` goal means the
                // active goal was cleared (terminal status reached or
                // user-initiated /goal abandon). Same recency-protection
                // rule as plan_snapshot — restore-on-load fires its own
                // broadcaster which writes a fresh snapshot, so don't
                // bump last_timestamp from these events.
                let ev: GoalSnapshotEvent = match serde_json::from_value(val) {
                    Ok(ev) => ev,
                    Err(e) => {
                        eprintln!(
                            "\x1b[33m[session] {}:{}: skipping malformed goal_snapshot ({e})\x1b[0m",
                            path.display(),
                            line_num + 1
                        );
                        skipped += 1;
                        continue;
                    }
                };
                goal = ev.goal;
            } else if kind == "provider_state" {
                // Latest provider_state wins. Carries the provider's
                // server-side session id (`anthropic-agent` SDK only
                // for now) so the next `/load` can rehydrate the
                // provider's `--resume <uuid>` path. Don't bump
                // last_timestamp — these events are
                // state-restoration artifacts, not user activity.
                let ev: ProviderStateEvent = match serde_json::from_value(val) {
                    Ok(ev) => ev,
                    Err(e) => {
                        eprintln!(
                            "\x1b[33m[session] {}:{}: skipping malformed provider_state ({e})\x1b[0m",
                            path.display(),
                            line_num + 1
                        );
                        skipped += 1;
                        continue;
                    }
                };
                provider_session_id = ev.provider_session_id;
            } else if kind == "compaction" {
                // Replay checkpoint: everything accumulated so far is
                // archived-on-disk but gets replaced in memory by the
                // checkpoint's messages. Later `message` events in
                // the same file still append after this point.
                let ev: CompactionEvent = match serde_json::from_value(val) {
                    Ok(ev) => ev,
                    Err(e) => {
                        eprintln!(
                            "\x1b[33m[session] {}:{}: skipping malformed compaction ({e})\x1b[0m",
                            path.display(),
                            line_num + 1
                        );
                        skipped += 1;
                        continue;
                    }
                };
                if ev.timestamp > last_timestamp {
                    last_timestamp = ev.timestamp;
                }
                messages.clear();
                for cm in ev.messages {
                    let role = match cm.role.as_str() {
                        "user" => crate::types::Role::User,
                        "assistant" => crate::types::Role::Assistant,
                        "system" => crate::types::Role::System,
                        other => {
                            eprintln!(
                                "\x1b[33m[session] {}:{}: dropping compaction message with unknown role '{other}'\x1b[0m",
                                path.display(),
                                line_num + 1
                            );
                            skipped += 1;
                            continue;
                        }
                    };
                    messages.push(Message {
                        role,
                        content: cm.content,
                    });
                }
            } else {
                // Message event line
                let event: MessageEvent = match serde_json::from_value(val) {
                    Ok(ev) => ev,
                    Err(e) => {
                        eprintln!(
                            "\x1b[33m[session] {}:{}: skipping malformed message event ({e})\x1b[0m",
                            path.display(),
                            line_num + 1
                        );
                        skipped += 1;
                        continue;
                    }
                };

                let role = match event.kind.as_str() {
                    "user" => crate::types::Role::User,
                    "assistant" => crate::types::Role::Assistant,
                    "system" => crate::types::Role::System,
                    other => {
                        eprintln!(
                            "\x1b[33m[session] {}:{}: skipping message with unknown role '{other}'\x1b[0m",
                            path.display(),
                            line_num + 1
                        );
                        skipped += 1;
                        continue;
                    }
                };

                if event.timestamp > last_timestamp {
                    last_timestamp = event.timestamp;
                }

                messages.push(Message {
                    role,
                    content: event.content,
                });
            }
        }

        if skipped > 0 {
            eprintln!(
                "\x1b[33m[session] {}: loaded with {skipped} corrupt line(s) skipped\x1b[0m",
                path.display()
            );
        }

        // Salvage headerless files (legacy / pre-fix sessions where
        // `append_plan_snapshot` raced ahead of `Session::append_to`
        // and created the file without a header). Infer the id from
        // the filename, model = "unknown", cwd = "", created_at =
        // file mtime so the session still appears in the sidebar and
        // can be loaded. The reader stays strict for in-band errors;
        // only the missing-header case is recovered.
        let h = match header {
            Some(h) => h,
            None => {
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let created_at = std::fs::metadata(path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                SessionHeader {
                    kind: "header".into(),
                    id,
                    model: "unknown".into(),
                    cwd: String::new(),
                    created_at,
                }
            }
        };

        let msg_count = messages.len();
        Ok(Session {
            id: h.id,
            created_at: h.created_at,
            updated_at: if last_timestamp > 0 {
                last_timestamp
            } else {
                h.created_at
            },
            model: h.model,
            cwd: h.cwd,
            messages,
            title,
            last_saved_count: msg_count,
            plan,
            goal,
            provider_session_id,
        })
    }

    /// Write a compaction checkpoint to the JSONL and set the session's
    /// in-memory state so that subsequent `append_to` calls only emit
    /// messages added *after* the checkpoint. The raw message events
    /// that preceded the checkpoint stay on disk (audit trail) but will
    /// be overridden by the checkpoint on load.
    pub fn append_compaction_to(&mut self, path: &Path, compacted: &[Message]) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let compacted_payload: Vec<CompactedMessage> = compacted
            .iter()
            .map(|m| CompactedMessage {
                role: match m.role {
                    crate::types::Role::User => "user".into(),
                    crate::types::Role::Assistant => "assistant".into(),
                    crate::types::Role::System => "system".into(),
                },
                content: m.content.clone(),
            })
            .collect();
        let event = CompactionEvent {
            kind: "compaction".into(),
            messages: compacted_payload,
            replaces_count: self.last_saved_count,
            timestamp: now_secs(),
        };
        let line = serde_json::to_string(&event)?;
        // M6.24 BUG M4: lock the write so a concurrent appender from
        // another process can't interleave bytes mid-line.
        append_locked(path, |file| writeln!(file, "{}", line))?;
        // Drop the in-memory history down to the compacted view so
        // subsequent `append_to` calls start fresh at index 0 and only
        // append new turns produced *after* the checkpoint.
        self.messages = compacted.to_vec();
        self.last_saved_count = self.messages.len();
        self.updated_at = event.timestamp;
        Ok(())
    }

    /// Append a rename event to the session file. Empty / whitespace-only
    /// titles clear the title back to `None`.
    pub fn append_rename_to(&mut self, path: &Path, title: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // M6.19 BUG L1+L5: strip control characters (newlines, tabs,
        // CR, NUL, etc.) from titles. JSON escapes them on write so
        // persistence is fine, but a UI rendering the title raw could
        // break layout (a `\n` in a title would split the sidebar
        // entry across two lines). Convert tabs / newlines to spaces
        // to keep the user's intended segmentation, then drop other
        // control chars entirely. Trim leading/trailing whitespace
        // afterward in case the substitution produced new outer
        // whitespace.
        let sanitized: String = title
            .chars()
            .map(|c| match c {
                '\n' | '\r' | '\t' => ' ',
                c if c.is_control() => '\0',
                c => c,
            })
            .filter(|&c| c != '\0')
            .collect();
        let trimmed = sanitized.trim();
        let event = RenameEvent {
            kind: "rename".into(),
            title: trimmed.to_string(),
            timestamp: now_secs(),
        };
        let line = serde_json::to_string(&event)?;
        // M6.24 BUG M4: lock the write.
        append_locked(path, |file| writeln!(file, "{}", line))?;
        self.title = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        self.updated_at = event.timestamp;
        Ok(())
    }

    /// Append a `provider_state` event capturing the provider's
    /// current server-side session id. Same wire shape as `rename` —
    /// latest event wins on load. Pass `None` to clear (e.g. on
    /// provider switch). Caller should only invoke when the value
    /// has actually changed since the last write; checking
    /// `self.provider_session_id != new` before calling avoids
    /// trivial duplicate events.
    pub fn append_provider_state_to(
        &mut self,
        path: &Path,
        provider_session_id: Option<String>,
    ) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let event = ProviderStateEvent {
            kind: "provider_state".into(),
            provider_session_id: provider_session_id.clone(),
            timestamp: now_secs(),
        };
        let line = serde_json::to_string(&event)?;
        append_locked(path, |file| writeln!(file, "{}", line))?;
        self.provider_session_id = provider_session_id;
        // Do NOT bump updated_at — provider_state events are
        // state-restoration plumbing, not user activity. Same rule as
        // plan_snapshot / goal_snapshot recency.
        Ok(())
    }
}

/// Free-function form of [`Session::append_plan_snapshot_to`] for
/// callers that don't have an owned `&mut Session` handy — typically
/// the GUI's plan-state broadcaster, which fires from a closure that
/// only has the JSONL path. Same wire format as the method; no
/// in-memory state to update. M1+.
pub fn append_plan_snapshot(
    path: &Path,
    plan: Option<&crate::tools::plan_state::Plan>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let event = PlanSnapshotEvent {
        kind: "plan_snapshot".into(),
        plan: plan.cloned(),
        timestamp: now_secs(),
    };
    let line = serde_json::to_string(&event)?;
    // M6.24 BUG M4: lock the write.
    append_locked(path, |file| writeln!(file, "{}", line))
}

/// Module-level companion to [`Session::append_goal_snapshot_to`]. Used
/// by the GUI's `goal_state` broadcaster, which only has the JSONL path
/// in scope — no owned `&mut Session` to update.
pub fn append_goal_snapshot(
    path: &Path,
    goal: Option<&crate::goal_state::GoalState>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let event = GoalSnapshotEvent {
        kind: "goal_snapshot".into(),
        goal: goal.cloned(),
        timestamp: now_secs(),
    };
    let line = serde_json::to_string(&event)?;
    append_locked(path, |file| writeln!(file, "{}", line))
}

/// Directory-backed store for sessions.
#[derive(Debug, Clone)]
pub struct SessionStore {
    pub root: PathBuf,
}

/// Compute the sharded subdirectory path for a session id.
///
/// Extracts the first 8 hex characters after the `sess-` prefix and
/// splits them into four 2-character directory levels. For example:
/// `sess-18b1f3dc2e3e8ed8` → `18/b1/f3/dc/`
///
/// If the id doesn't start with `sess-`, has fewer than 8 hex chars
/// after the prefix, or contains non-hex characters, returns an empty
/// path (no sharding).
fn shard_path_from_id(id: &str) -> PathBuf {
    let hex_prefix = "sess-";
    if let Some(rest) = id.strip_prefix(hex_prefix) {
        // Need at least 8 chars and they must all be valid hex.
        if rest.len() >= 8 {
            let first8: &str = &rest[..8];
            if first8.chars().all(|c| c.is_ascii_hexdigit()) {
                let d1 = &first8[0..2];
                let d2 = &first8[2..4];
                let d3 = &first8[4..6];
                let d4 = &first8[6..8];
                return PathBuf::from(format!("{d1}/{d2}/{d3}/{d4}"));
            }
        }
    }
    PathBuf::new()
}

impl SessionStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Always project-scoped: `./.thclaws/sessions/`. Starting in a blank
    /// directory gives you an empty session list — legacy user-level sessions
    /// at `~/.local/share/thclaws/sessions/` and `~/.claude/sessions/` are
    /// left alone (you can move them into a project's `.thclaws/sessions/` to
    /// import). The dir is created on first save; we don't materialise it
    /// just to list.
    pub fn default_path() -> Option<PathBuf> {
        let cwd = std::env::current_dir().ok()?;
        Some(cwd.join(".thclaws").join("sessions"))
    }

    pub fn path_for(&self, id: &str) -> PathBuf {
        let shard = shard_path_from_id(id);
        if shard.as_os_str().is_empty() {
            // Fallback: no sharding for ids that don't match the pattern.
            self.root.join(format!("{id}.jsonl"))
        } else {
            self.root.join(shard).join(format!("{id}.jsonl"))
        }
    }

    /// Reject session ids that could escape the sessions dir via path
    /// traversal or embed shell / filesystem metacharacters. Session ids
    /// generated by this crate use `sess-{ts}-{rand}`, but the `/load`
    /// command accepts user input verbatim, and legacy sessions
    /// on disk may have been written by third-party tooling.
    fn validate_id(id: &str) -> Result<()> {
        if id.is_empty() {
            return Err(Error::Config("session id is empty".into()));
        }
        // POSIX filename cap is 255 bytes. With our `.jsonl` suffix
        // (6 bytes) that leaves 249 for the id itself. Reject above
        // that so we never produce a filename the filesystem refuses.
        if id.len() > 249 {
            return Err(Error::Config("session id exceeds 249 characters".into()));
        }
        let forbidden_chars = |c: char| matches!(c, '/' | '\\' | '\0') || c.is_control();
        if id.contains("..") || id.chars().any(forbidden_chars) {
            return Err(Error::Config(format!(
                "session id '{id}' contains path separators or control characters"
            )));
        }
        if std::path::Path::new(id).is_absolute() {
            return Err(Error::Config(format!(
                "session id '{id}' is an absolute path"
            )));
        }
        Ok(())
    }

    pub fn save(&self, session: &mut Session) -> Result<PathBuf> {
        Self::validate_id(&session.id)?;
        let path = self.path_for(&session.id);
        session.append_to(&path)?;
        Ok(path)
    }

    pub fn load(&self, id: &str) -> Result<Session> {
        Self::validate_id(id)?;
        Session::load_from(&self.path_for(id))
    }

    /// Resolve a user-supplied identifier to a session id. Tries id match
    /// first (exact filename on disk), then case-insensitive title match.
    /// Exact title matches win over substring; substring matches are only
    /// returned when unambiguous.
    pub fn resolve_id(&self, name_or_id: &str) -> Result<String> {
        let trimmed = name_or_id.trim();
        if trimmed.is_empty() {
            return Err(Error::Config("session name or id is empty".into()));
        }

        // 1. Exact id match — fast path, works even if no title is set.
        //    Treat traversal-looking inputs as "no exact match" rather
        //    than erroring, so `/load my funny name` still falls through
        //    to the title-search branch below; but never let a traversal
        //    string reach the filesystem.
        if Self::validate_id(trimmed).is_ok() && self.path_for(trimmed).exists() {
            return Ok(trimmed.to_string());
        }

        let metas = self.list()?;
        let needle = trimmed.to_lowercase();

        // 2. Id prefix match (covers cases where the user copies a
        // truncated id from the sidebar).
        if trimmed.starts_with("sess-") {
            let by_prefix: Vec<&SessionMeta> =
                metas.iter().filter(|m| m.id.starts_with(trimmed)).collect();
            match by_prefix.len() {
                1 => return Ok(by_prefix[0].id.clone()),
                n if n > 1 => {
                    return Err(Error::Config(format!(
                        "id prefix '{trimmed}' matches {n} sessions — be more specific"
                    )));
                }
                _ => {}
            }
        }

        // 3. Title match (exact, then substring).
        let exact: Vec<&SessionMeta> = metas
            .iter()
            .filter(|m| {
                m.title
                    .as_deref()
                    .map(|t| t.to_lowercase() == needle)
                    .unwrap_or(false)
            })
            .collect();

        match exact.len() {
            1 => return Ok(exact[0].id.clone()),
            n if n > 1 => {
                return Err(Error::Config(format!(
                    "session name '{trimmed}' matches {n} sessions — use the id instead",
                )));
            }
            _ => {}
        }

        let partial: Vec<&SessionMeta> = metas
            .iter()
            .filter(|m| {
                m.title
                    .as_deref()
                    .map(|t| t.to_lowercase().contains(&needle))
                    .unwrap_or(false)
            })
            .collect();

        match partial.len() {
            1 => Ok(partial[0].id.clone()),
            0 => Err(Error::Config(format!("no session matching '{trimmed}'"))),
            n => Err(Error::Config(format!(
                "session name '{trimmed}' matches {n} sessions — be more specific or use the id",
            ))),
        }
    }

    /// Convenience: resolve a name-or-id and load the session.
    pub fn load_by_name_or_id(&self, name_or_id: &str) -> Result<Session> {
        let id = self.resolve_id(name_or_id)?;
        self.load(&id)
    }

    /// List saved sessions, newest first. Returns an empty vec when the
    /// store directory doesn't exist yet.
    ///
    /// M6.24 BUG M3: uses `Session::load_meta_from` (streaming, no
    /// message-body deserialization) instead of `load_from`. For a
    /// project with hundreds of sessions of multi-MB each, this drops
    /// `SessionListRefresh` from "read + parse all message bodies"
    /// (potentially hundreds of MB) to "stream + count" (a few KB of
    /// headers + timestamps).
    ///
    /// Sharded layout (M6.30+): recursively walks the sessions root to
    /// discover `.jsonl` files in both the flat root and nested shard
    /// directories (e.g. `18/b1/f3/dc/sess-*.jsonl`).
    pub fn list(&self) -> Result<Vec<SessionMeta>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        Self::walk_dir_for_jsonl(&self.root, &mut out)?;
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(out)
    }

    /// Recursively walk a directory tree and collect `.jsonl` session
    /// metadata. Follows symlinks = false; only regular files with
    /// `.jsonl` extension are considered.
    fn walk_dir_for_jsonl(root: &Path, out: &mut Vec<SessionMeta>) -> Result<()> {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    if let Ok(meta) = Session::load_meta_from(&path) {
                        out.push(meta);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn latest(&self) -> Result<Option<Session>> {
        let metas = self.list()?;
        match metas.first() {
            Some(m) => Ok(Some(self.load(&m.id)?)),
            None => Ok(None),
        }
    }

    /// Rename a stored session by appending a rename event to its JSONL
    /// file. Pass an empty string to clear the title. Returns the updated
    /// [`Session`].
    pub fn rename(&self, id: &str, title: &str) -> Result<Session> {
        Self::validate_id(id)?;
        let path = self.path_for(id);
        if !path.exists() {
            return Err(Error::Config(format!("session '{id}' not found")));
        }
        let mut session = Session::load_from(&path)?;
        session.append_rename_to(&path, title)?;
        Ok(session)
    }

    /// Delete a session from disk. Returns Ok if removed or already
    /// gone (idempotent), Err if the id is malformed or fs::remove_file
    /// fails for a real reason (permissions, etc.).
    ///
    /// After removing the file, attempts to clean up empty parent
    /// shard directories up to the store root (best-effort — non-fatal
    /// if cleanup fails).
    pub fn delete(&self, id: &str) -> Result<()> {
        Self::validate_id(id)?;
        let path = self.path_for(id);
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| Error::Config(format!("failed to delete session '{id}': {e}")))?;
            // Best-effort cleanup of empty shard directories.
            // Walk up from the file's parent, removing empty dirs
            // until we hit the store root or a non-empty dir.
            if let Some(parent) = path.parent() {
                let mut current = parent;
                while current != self.root && current != self.root.parent().unwrap_or(current) {
                    if std::fs::read_dir(current)
                        .map(|mut d| d.next().is_none())
                        .unwrap_or(false)
                    {
                        let _ = std::fs::remove_dir(current);
                        current = current.parent().unwrap_or(current);
                    } else {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Nanosecond-timestamped id — naturally unique and chronologically sortable.
fn generate_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("sess-{nanos:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, Role};
    use tempfile::tempdir;

    /// Issue #90 regression: pre-fix, `append_locked` opened the
    /// JSONL with `.create(true).append(true)` only. On Windows that
    /// hands `LockFileEx` a handle without `GENERIC_READ`, the
    /// API returns `ERROR_ACCESS_DENIED` (os error 5), and every
    /// session save fails. POSIX `flock` doesn't have that
    /// requirement, so macOS / Linux passed even with the old code
    /// — but the test still pins the contract: two back-to-back
    /// calls succeed AND both writes land in the file. If
    /// `append_locked` ever drops `.read(true)` again, this test
    /// keeps passing on POSIX but the Windows CI job (or any
    /// Windows user) breaks on first save. The body is the same
    /// shape on every OS so the test is cross-platform.
    #[test]
    fn append_locked_acquires_and_releases_for_repeat_writes() {
        let td = tempdir().unwrap();
        let path = td.path().join("session.jsonl");

        append_locked(&path, |f| f.write_all(b"line-1\n")).expect("first append");
        append_locked(&path, |f| f.write_all(b"line-2\n")).expect("second append");

        let body = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(body, "line-1\nline-2\n");
    }

    fn sample_messages() -> Vec<Message> {
        vec![
            Message::user("hello"),
            Message::assistant("hi there"),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
            },
        ]
    }

    #[test]
    fn new_session_has_fresh_timestamps_and_unique_id() {
        let a = Session::new("claude-sonnet-4-5", "/tmp");
        std::thread::sleep(std::time::Duration::from_nanos(1));
        let b = Session::new("claude-sonnet-4-5", "/tmp");
        assert_ne!(a.id, b.id);
        assert!(a.created_at <= b.created_at);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        let mut session = Session::new("claude-sonnet-4-5", "/tmp/proj");
        session.sync(sample_messages());

        let path = store.save(&mut session).unwrap();
        assert!(path.exists());
        assert_eq!(session.last_saved_count, 3);

        let loaded = store.load(&session.id).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.model, session.model);
        assert_eq!(loaded.cwd, session.cwd);
        assert_eq!(loaded.messages, session.messages);
        assert_eq!(loaded.last_saved_count, 3);
    }

    #[test]
    fn append_only_adds_new_messages() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        let mut session = Session::new("claude-sonnet-4-5", "/tmp/proj");

        // First turn: 1 user + 1 assistant message.
        session.sync(vec![Message::user("hello"), Message::assistant("hi")]);
        store.save(&mut session).unwrap();
        assert_eq!(session.last_saved_count, 2);

        // Second turn: add more messages (sync gives full history).
        session.sync(vec![
            Message::user("hello"),
            Message::assistant("hi"),
            Message::user("what's up?"),
            Message::assistant("not much"),
        ]);
        store.save(&mut session).unwrap();
        assert_eq!(session.last_saved_count, 4);

        // Verify the file has header + 4 message lines = 5 lines total.
        let path = store.path_for(&session.id);
        let contents = std::fs::read_to_string(&path).unwrap();
        let line_count = contents.lines().count();
        assert_eq!(line_count, 5); // 1 header + 4 messages

        // Load back and verify all messages round-trip.
        let loaded = store.load(&session.id).unwrap();
        assert_eq!(loaded.messages.len(), 4);
        assert_eq!(loaded.messages[0], Message::user("hello"));
        assert_eq!(loaded.messages[3], Message::assistant("not much"));
    }

    #[test]
    fn jsonl_format_has_correct_line_structure() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        let mut session = Session::new("test-model", "/tmp");
        session.sync(vec![Message::user("ping")]);
        store.save(&mut session).unwrap();

        let path = store.path_for(&session.id);
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();

        // Line 0: header
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["type"], "header");
        assert_eq!(header["model"], "test-model");

        // Line 1: message event
        let event: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(event["type"], "user");
        assert!(event["content"].is_array());
        assert!(event["timestamp"].is_number());
    }

    #[test]
    fn assistant_messages_carry_provider_and_model_attribution() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        let mut session = Session::new("claude-sonnet-4-5", "/tmp");
        session.sync(vec![Message::user("ping"), Message::assistant("pong")]);
        store.save(&mut session).unwrap();

        let contents = std::fs::read_to_string(store.path_for(&session.id)).unwrap();
        let lines: Vec<&str> = contents.lines().collect();

        // User line: no provider/model attribution.
        let user_ev: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(user_ev["type"], "user");
        assert!(user_ev.get("provider").is_none());
        assert!(user_ev.get("model").is_none());

        // Assistant line: carries provider + model from the session.
        let asst_ev: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(asst_ev["type"], "assistant");
        assert_eq!(asst_ev["provider"], "anthropic");
        assert_eq!(asst_ev["model"], "claude-sonnet-4-5");
    }

    #[test]
    fn old_sessions_without_provider_model_still_load() {
        // Backward compat: a JSONL file written before M-now (no
        // provider/model fields on assistant lines) must still load.
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"header","id":"sess-old","model":"gpt-4o","cwd":"/tmp","created_at":1000}
{"type":"user","content":[{"type":"text","text":"hi"}],"timestamp":1001}
{"type":"assistant","content":[{"type":"text","text":"hello"}],"timestamp":1002}
"#,
        )
        .unwrap();

        let loaded = Session::load_from(&path).unwrap();
        assert_eq!(loaded.id, "sess-old");
        assert_eq!(loaded.messages.len(), 2);
    }

    #[test]
    fn list_returns_empty_when_store_missing() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("nonexistent"));
        let metas = store.list().unwrap();
        assert!(metas.is_empty());
    }

    #[test]
    fn list_sorts_newest_first() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        let mut a = Session::new("claude-sonnet-4-5", "/tmp");
        a.updated_at = 100;
        a.id = "sess-aaa".into();
        // Write a valid JSONL manually with specific timestamps.
        let path_a = store.path_for("sess-aaa");
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(&path_a, format!(
            "{}\n{}\n",
            r#"{"type":"header","id":"sess-aaa","model":"claude-sonnet-4-5","cwd":"/tmp","created_at":100}"#,
            r#"{"type":"user","content":[{"type":"text","text":"hi"}],"timestamp":100}"#,
        )).unwrap();

        let path_b = store.path_for("sess-bbb");
        std::fs::write(&path_b, format!(
            "{}\n{}\n",
            r#"{"type":"header","id":"sess-bbb","model":"gpt-4o","cwd":"/tmp","created_at":200}"#,
            r#"{"type":"user","content":[{"type":"text","text":"hi"}],"timestamp":200}"#,
        )).unwrap();

        let path_c = store.path_for("sess-ccc");
        std::fs::write(&path_c, format!(
            "{}\n{}\n",
            r#"{"type":"header","id":"sess-ccc","model":"claude-opus","cwd":"/tmp","created_at":150}"#,
            r#"{"type":"user","content":[{"type":"text","text":"hi"}],"timestamp":150}"#,
        )).unwrap();

        let metas = store.list().unwrap();
        let ids: Vec<&str> = metas.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["sess-bbb", "sess-ccc", "sess-aaa"]);
        assert_eq!(metas[0].model, "gpt-4o");
    }

    /// M6.24 BUG M3: `load_meta_from` must produce the same metadata
    /// as `load_from` -> SessionMeta but WITHOUT keeping message
    /// bodies in memory. Verify equivalence on a representative
    /// session with multiple message events, a rename, and a
    /// compaction.
    #[test]
    fn load_meta_from_matches_load_from_metadata() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess-meta.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"header","id":"sess-meta","model":"claude-sonnet-4-5","cwd":"/tmp","created_at":1000}"#,
                "\n",
                r#"{"type":"user","content":[{"type":"text","text":"q1"}],"timestamp":1100}"#,
                "\n",
                r#"{"type":"assistant","content":[{"type":"text","text":"a1"}],"timestamp":1200}"#,
                "\n",
                r#"{"type":"rename","title":"my session","timestamp":1250}"#,
                "\n",
                r#"{"type":"user","content":[{"type":"text","text":"q2"}],"timestamp":1300}"#,
                "\n",
                r#"{"type":"plan_snapshot","plan":null,"timestamp":99999}"#,
                "\n",
            ),
        )
        .unwrap();

        let full = Session::load_from(&path).unwrap();
        let full_meta = SessionMeta {
            id: full.id.clone(),
            updated_at: full.updated_at,
            model: full.model.clone(),
            message_count: full.messages.len(),
            title: full.title.clone(),
        };

        let streamed = Session::load_meta_from(&path).unwrap();

        assert_eq!(streamed, full_meta, "streamed meta must match full load");
        assert_eq!(streamed.id, "sess-meta");
        assert_eq!(streamed.model, "claude-sonnet-4-5");
        assert_eq!(streamed.message_count, 3);
        assert_eq!(streamed.title.as_deref(), Some("my session"));
        // plan_snapshot's 99999 timestamp must NOT bump updated_at —
        // M6.16.1 fix preserved.
        assert_eq!(streamed.updated_at, 1300);
    }

    /// Regression for the May 2026 user report: every session file
    /// created by an Agent SDK turn writes a `provider_state` event
    /// carrying the upstream session_id. The meta scanner used by
    /// the sidebar refresh fell through to the unknown-event arm and
    /// counted each one as "1 corrupt line(s)", producing a yellow
    /// stderr line per session on every list refresh.
    #[test]
    fn load_meta_from_recognises_provider_state_without_warning() {
        let td = tempdir().unwrap();
        let path = td.path().join("sess-provider-state.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"header","id":"sess-ps","model":"agent/claude-sonnet-4-6","cwd":"/tmp","created_at":1000}"#,
                "\n",
                r#"{"type":"user","content":[{"type":"text","text":"hi"}],"timestamp":1100}"#,
                "\n",
                r#"{"type":"provider_state","provider_session_id":"abcd1234","timestamp":1150}"#,
                "\n",
                r#"{"type":"assistant","content":[{"type":"text","text":"hello"}],"timestamp":1200}"#,
                "\n",
            ),
        )
        .unwrap();

        // No way to assert "no eprintln" directly from a unit test,
        // but we can assert the meta scan parses correctly and the
        // message count doesn't include provider_state.
        let meta = Session::load_meta_from(&path).unwrap();
        assert_eq!(meta.id, "sess-ps");
        assert_eq!(
            meta.message_count, 2,
            "provider_state must NOT count as a message"
        );
        assert_eq!(
            meta.updated_at, 1200,
            "provider_state must NOT bump updated_at"
        );
    }

    /// M6.24 BUG M3: load_meta_from of a compacted session reports
    /// the post-compaction message count, not pre.
    #[test]
    fn load_meta_from_respects_compaction_checkpoint_count() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess-comp.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"header","id":"sess-comp","model":"m","cwd":"/tmp","created_at":1000}"#,
                "\n",
                r#"{"type":"user","content":[{"type":"text","text":"q1"}],"timestamp":1100}"#,
                "\n",
                r#"{"type":"user","content":[{"type":"text","text":"q2"}],"timestamp":1200}"#,
                "\n",
                r#"{"type":"user","content":[{"type":"text","text":"q3"}],"timestamp":1300}"#,
                "\n",
                r#"{"type":"compaction","messages":[{"role":"user","content":[{"type":"text","text":"summary"}]}],"replaces_count":3,"timestamp":1400}"#,
                "\n",
                r#"{"type":"user","content":[{"type":"text","text":"q4"}],"timestamp":1500}"#,
                "\n",
            ),
        )
        .unwrap();

        let meta = Session::load_meta_from(&path).unwrap();
        // 1 from compaction + 1 added after = 2
        assert_eq!(meta.message_count, 2);
        assert_eq!(meta.updated_at, 1500);
    }

    /// M6.24 BUG M4: append_locked must serialize concurrent writers
    /// — the resulting file should be valid JSONL with no interleaved
    /// bytes mid-line. Test by spawning two threads that append
    /// distinct large messages concurrently and verifying every line
    /// parses as valid JSON.
    #[test]
    fn concurrent_appends_dont_corrupt_jsonl() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess-concurrent.jsonl");

        // Pre-write the header so both threads only append message events.
        std::fs::write(
            &path,
            r#"{"type":"header","id":"sess-concurrent","model":"m","cwd":"/tmp","created_at":1}"#
                .to_string()
                + "\n",
        )
        .unwrap();

        // Build two large messages — large enough that a single
        // writeln! exceeds typical PIPE_BUF and would interleave
        // without locking.
        let big_a = "A".repeat(8192);
        let big_b = "B".repeat(8192);

        // Each thread writes 50 messages of its filler text.
        let path_a = path.clone();
        let path_b = path.clone();
        let h_a = std::thread::spawn(move || {
            for i in 0..50 {
                let event = format!(
                    r#"{{"type":"user","content":[{{"type":"text","text":"{big_a}-{i}"}}],"timestamp":2}}"#,
                );
                append_locked(&path_a, |f| writeln!(f, "{}", event)).unwrap();
            }
        });
        let h_b = std::thread::spawn(move || {
            for i in 0..50 {
                let event = format!(
                    r#"{{"type":"assistant","content":[{{"type":"text","text":"{big_b}-{i}"}}],"timestamp":3}}"#,
                );
                append_locked(&path_b, |f| writeln!(f, "{}", event)).unwrap();
            }
        });
        h_a.join().unwrap();
        h_b.join().unwrap();

        // Verify every line is valid JSON. Pre-fix this would fail
        // for some lines because two threads' bytes would interleave.
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        // 1 header + 50 from each thread = 101 lines
        assert_eq!(
            lines.len(),
            101,
            "expected 101 lines (1 header + 100 messages), got {}",
            lines.len()
        );
        for (i, line) in lines.iter().enumerate() {
            let parsed: std::result::Result<serde_json::Value, _> = serde_json::from_str(line);
            assert!(
                parsed.is_ok(),
                "line {} failed to parse as JSON: {:?}",
                i + 1,
                line.chars().take(80).collect::<String>()
            );
        }
    }

    #[test]
    fn latest_returns_most_recent_session() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        std::fs::create_dir_all(dir.path()).unwrap();

        let path_a = store.path_for("sess-a");
        std::fs::write(
            &path_a,
            format!(
                "{}\n{}\n",
                r#"{"type":"header","id":"sess-a","model":"m1","cwd":"/tmp","created_at":50}"#,
                r#"{"type":"user","content":[{"type":"text","text":"hi"}],"timestamp":50}"#,
            ),
        )
        .unwrap();

        let path_b = store.path_for("sess-b");
        std::fs::write(
            &path_b,
            r#"{"type":"header","id":"sess-b","model":"m2","cwd":"/tmp","created_at":999}"#
                .to_string()
                + "\n",
        )
        .unwrap();

        let latest = store.latest().unwrap().unwrap();
        assert_eq!(latest.id, "sess-b");
        assert_eq!(latest.model, "m2");
    }

    #[test]
    fn sync_bumps_updated_at_and_replaces_messages() {
        let mut session = Session::new("m", "/tmp");
        let before = session.updated_at;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        session.sync(sample_messages());
        assert_eq!(session.messages.len(), 3);
        assert!(session.updated_at > before);
    }

    #[test]
    fn save_creates_parent_directories() {
        let dir = tempdir().unwrap();
        let deep = dir.path().join("a/b/c");
        let store = SessionStore::new(deep);
        let mut session = Session::new("m", "/tmp");
        store.save(&mut session).unwrap();
        assert!(store.path_for(&session.id).exists());
    }

    #[test]
    fn load_skips_malformed_lines_and_keeps_recoverable_session() {
        // M6.19 BUG H1: pre-fix, a single malformed line failed the
        // entire load and the session disappeared from `list()`'s
        // silent-skip catcher. Now the malformed line is skipped
        // with a stderr warning and the rest of the session is
        // preserved. With NO valid lines, the salvage path
        // (file_stem → id, mtime → created_at) returns a usable
        // "unknown-model" placeholder rather than an error.
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let path = store.path_for("sess-mixed");
        // Mix of valid header + valid message + malformed line + another
        // valid message. The malformed line should be dropped; the rest
        // should round-trip.
        let body = concat!(
            r#"{"type":"header","id":"sess-mixed","model":"m","cwd":"/tmp","created_at":100}"#,
            "\n",
            r#"{"type":"user","content":[{"type":"text","text":"first"}],"timestamp":200}"#,
            "\n",
            "{not-valid-json",
            "\n",
            r#"{"type":"assistant","content":[{"type":"text","text":"second"}],"timestamp":201}"#,
            "\n",
        );
        std::fs::write(&path, body).unwrap();
        let s = store
            .load("sess-mixed")
            .expect("partial load should succeed");
        assert_eq!(s.id, "sess-mixed");
        assert_eq!(s.messages.len(), 2, "valid messages preserved");
    }

    #[test]
    fn load_salvages_pure_garbage_via_filename() {
        // Pure-garbage JSONL with no recoverable lines — salvage path
        // gives a placeholder session so the file at least appears in
        // the sidebar (where the user can decide to delete it).
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let path = store.path_for("sess-bad");
        std::fs::write(&path, "{not-valid\nmore garbage\n").unwrap();
        let s = store.load("sess-bad").expect("salvage path should succeed");
        assert_eq!(s.id, "sess-bad");
        assert_eq!(s.model, "unknown");
        assert!(s.messages.is_empty());
    }

    #[test]
    fn load_errors_on_missing_file() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        assert!(store.load("nope").is_err());
    }

    #[test]
    fn rename_strips_control_characters_from_title() {
        // M6.19 BUG L1+L5: titles with embedded newlines / tabs / CR
        // / NUL would JSON-escape on persistence (so the file stays
        // valid), but a UI rendering the title raw would break
        // layout. Sanitize at write time: convert tabs / newlines
        // to spaces (preserve segmentation), strip other control
        // chars entirely, then trim outer whitespace.
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let mut session = Session::new("m", "/tmp");
        session.sync(vec![Message::user("hello")]);
        store.save(&mut session).unwrap();

        let path = store.path_for(&session.id);
        session
            .append_rename_to(&path, "  before\nafter\twith\rcontrol\x01here  ")
            .unwrap();

        // Newlines / tabs / CR collapse to spaces; \x01 (control) is
        // stripped entirely (no replacement char); outer whitespace
        // trimmed.
        assert_eq!(
            session.title.as_deref(),
            Some("before after with controlhere")
        );

        // Roundtrip through load_from to confirm persistence.
        let reloaded = store.load(&session.id).unwrap();
        assert_eq!(
            reloaded.title.as_deref(),
            Some("before after with controlhere")
        );
    }

    #[test]
    fn rename_appends_event_and_persists() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        let mut session = Session::new("m", "/tmp");
        session.sync(vec![Message::user("hello")]);
        store.save(&mut session).unwrap();
        let id = session.id.clone();

        let updated = store.rename(&id, "my chat").unwrap();
        assert_eq!(updated.title.as_deref(), Some("my chat"));

        // Reload and confirm title persisted.
        let reloaded = store.load(&id).unwrap();
        assert_eq!(reloaded.title.as_deref(), Some("my chat"));

        // List reports the title too.
        let metas = store.list().unwrap();
        assert_eq!(metas[0].title.as_deref(), Some("my chat"));

        // Rename again — latest wins.
        store.rename(&id, "renamed").unwrap();
        let reloaded2 = store.load(&id).unwrap();
        assert_eq!(reloaded2.title.as_deref(), Some("renamed"));

        // Empty string clears the title.
        store.rename(&id, "").unwrap();
        let cleared = store.load(&id).unwrap();
        assert_eq!(cleared.title, None);
    }

    #[test]
    fn provider_state_round_trips_through_jsonl() {
        // The whole point of the field: write it once, kill the
        // process, load the session back, and find the same value
        // ready to feed back into the SDK as `--resume <uuid>`. Pre-
        // fix this round-trip didn't exist at all (the value lived
        // only in `Arc<Mutex<>>` on the provider instance) — that's
        // why resumed sessions appeared to forget previous turns.
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        let mut session = Session::new("anthropic-agent/sonnet", "/tmp");
        session.sync(vec![Message::user("hi")]);
        store.save(&mut session).unwrap();
        assert!(session.provider_session_id.is_none());

        let path = store.path_for(&session.id);
        session
            .append_provider_state_to(&path, Some("uuid-abc-123".into()))
            .unwrap();
        assert_eq!(session.provider_session_id.as_deref(), Some("uuid-abc-123"));

        // Reload from disk — value must survive the process boundary.
        let reloaded = store.load(&session.id).unwrap();
        assert_eq!(
            reloaded.provider_session_id.as_deref(),
            Some("uuid-abc-123"),
            "session JSONL must persist the provider-side session id so resume can rehydrate it"
        );
    }

    #[test]
    fn provider_state_latest_wins_on_load() {
        // Same latest-wins semantics as `rename`. Multiple turns =
        // multiple events; loader collapses to the last value seen.
        // (In practice the SDK keeps reusing the same UUID, so most
        // turns won't change it — `save_history` skips the append
        // when nothing changed. But the schema has to handle rotation
        // for the case where the SDK does mint a new id.)
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        let mut session = Session::new("anthropic-agent/sonnet", "/tmp");
        session.sync(vec![Message::user("hi")]);
        store.save(&mut session).unwrap();
        let path = store.path_for(&session.id);

        session
            .append_provider_state_to(&path, Some("first".into()))
            .unwrap();
        session
            .append_provider_state_to(&path, Some("second".into()))
            .unwrap();

        let reloaded = store.load(&session.id).unwrap();
        assert_eq!(reloaded.provider_session_id.as_deref(), Some("second"));
    }

    #[test]
    fn provider_state_none_clears_persisted_id() {
        // `None` is meaningful — it represents an explicit clear
        // (e.g. provider switched away from anthropic-agent, or the
        // user reset). Latest wins, so a trailing None overrides the
        // earlier `Some`.
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        let mut session = Session::new("anthropic-agent/sonnet", "/tmp");
        session.sync(vec![Message::user("hi")]);
        store.save(&mut session).unwrap();
        let path = store.path_for(&session.id);

        session
            .append_provider_state_to(&path, Some("uuid".into()))
            .unwrap();
        session.append_provider_state_to(&path, None).unwrap();

        let reloaded = store.load(&session.id).unwrap();
        assert!(reloaded.provider_session_id.is_none());
    }

    #[test]
    fn provider_state_absent_in_legacy_sessions() {
        // Backwards compat: sessions written by older builds have no
        // `provider_state` event. `provider_session_id` must default
        // to `None` so existing JSONL files keep loading without a
        // forced migration.
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        let mut session = Session::new("anthropic-agent/sonnet", "/tmp");
        session.sync(vec![Message::user("hi")]);
        store.save(&mut session).unwrap();

        let reloaded = store.load(&session.id).unwrap();
        assert!(
            reloaded.provider_session_id.is_none(),
            "fresh session without a provider_state event must load with None"
        );
    }

    #[test]
    fn compaction_checkpoint_replays_on_load() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        // Start a session with 6 messages (3 turns).
        let mut s = Session::new("m", "/tmp");
        for i in 0..6 {
            let role = if i % 2 == 0 {
                crate::types::Role::User
            } else {
                crate::types::Role::Assistant
            };
            s.messages.push(Message {
                role,
                content: vec![crate::types::ContentBlock::Text {
                    text: format!("msg-{i}"),
                }],
            });
        }
        store.save(&mut s).unwrap();

        // Write a compaction checkpoint collapsing the first 4 into 1 summary.
        let path = store.path_for(&s.id);
        let compacted = vec![
            Message {
                role: crate::types::Role::User,
                content: vec![crate::types::ContentBlock::Text {
                    text: "[summary] first two turns".into(),
                }],
            },
            s.messages[4].clone(),
            s.messages[5].clone(),
        ];
        s.append_compaction_to(&path, &compacted).unwrap();

        // Add one fresh message post-checkpoint and save.
        s.messages.push(Message {
            role: crate::types::Role::User,
            content: vec![crate::types::ContentBlock::Text {
                text: "msg-6".into(),
            }],
        });
        store.save(&mut s).unwrap();

        // Load: checkpoint + msg-6, not the original 7.
        let loaded = store.load(&s.id).unwrap();
        assert_eq!(loaded.messages.len(), 4);
        match &loaded.messages[0].content[0] {
            crate::types::ContentBlock::Text { text } => {
                assert!(text.contains("[summary]"));
            }
            _ => panic!("expected summary text"),
        }
        match &loaded.messages[3].content[0] {
            crate::types::ContentBlock::Text { text } => {
                assert_eq!(text, "msg-6");
            }
            _ => panic!("expected msg-6"),
        }
    }

    #[test]
    fn resolve_id_prefers_exact_id_then_title() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        // Two sessions with explicit ids so the id-prefix case isn't
        // tripped by back-to-back nanosecond id collisions.
        let mut a = Session::new("m", "/tmp");
        a.id = "sess-aaaaaaaa11111111".into();
        a.sync(vec![Message::user("a")]);
        store.save(&mut a).unwrap();
        let id_a = a.id.clone();

        let mut b = Session::new("m", "/tmp");
        b.id = "sess-bbbbbbbb22222222".into();
        b.sync(vec![Message::user("b")]);
        store.save(&mut b).unwrap();
        let id_b = b.id.clone();
        store.rename(&id_b, "My Chat").unwrap();

        // Exact id wins.
        assert_eq!(store.resolve_id(&id_a).unwrap(), id_a);
        // Exact title (case-insensitive).
        assert_eq!(store.resolve_id("my chat").unwrap(), id_b);
        assert_eq!(store.resolve_id("MY CHAT").unwrap(), id_b);
        // Substring match (unambiguous).
        assert_eq!(store.resolve_id("chat").unwrap(), id_b);
        // Unknown.
        assert!(store.resolve_id("nonexistent").is_err());
        // Empty.
        assert!(store.resolve_id("   ").is_err());
        // Id prefix (covers truncated id copy from the sidebar).
        assert_eq!(store.resolve_id("sess-aaaa").unwrap(), id_a);
    }

    #[test]
    fn resolve_id_errors_on_ambiguous_title() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        let mut a = Session::new("m", "/tmp");
        a.sync(vec![Message::user("a")]);
        store.save(&mut a).unwrap();
        store.rename(&a.id, "shared").unwrap();

        let mut b = Session::new("m", "/tmp");
        std::thread::sleep(std::time::Duration::from_millis(5));
        b.sync(vec![Message::user("b")]);
        store.save(&mut b).unwrap();
        store.rename(&b.id, "shared").unwrap();

        let err = store.resolve_id("shared").unwrap_err();
        assert!(format!("{err}").contains("matches 2 sessions"));
    }

    #[test]
    fn rename_errors_on_unknown_session() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        assert!(store.rename("sess-nonexistent", "x").is_err());
    }

    #[test]
    fn load_salvages_headerless_files() {
        // Pre-fix `append_plan_snapshot` raced ahead of `Session::append_to`
        // when minting a fresh session, leaving a headerless JSONL on
        // disk. The strict reader rejected such files with "missing
        // header line" and `SessionStore::list` silently dropped them
        // — sidebar showed "No saved sessions" even when files were
        // present. Salvage path: infer id from the filename, model =
        // "unknown", cwd = "", created_at = mtime, so the file at
        // least appears in the sidebar and can be loaded.
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let path = store.path_for("sess-no-header");
        std::fs::write(&path, r#"{"type":"user","content":[],"timestamp":1}"#).unwrap();
        let s = store.load("sess-no-header").expect("salvage headerless");
        assert_eq!(s.id, "sess-no-header");
        assert_eq!(s.model, "unknown");
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn write_header_if_missing_writes_on_empty_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess-test.jsonl");
        // Empty file simulates the `append_plan_snapshot` race condition
        // — file exists but no header has been written yet. Wait, in
        // the real race, the plan_snapshot line IS written. So check
        // both: empty file gets a header, non-empty file does not.
        std::fs::write(&path, b"").unwrap();
        let session = Session::new("test-model", "/test/cwd");
        session.write_header_if_missing(&path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains(r#""type":"header""#));
        assert!(contents.contains(r#""model":"test-model""#));
    }

    #[test]
    fn plan_snapshot_does_not_bump_updated_at() {
        // Sidebar sorts by `updated_at`. Loading a session triggers
        // `plan_state::restore_from_session`, which fires the
        // broadcaster's `append_plan_snapshot` with the current wall-
        // clock time. If that timestamp bumped `updated_at`, every
        // session click would jump that session to the top of the
        // sidebar — masking real recency. Pin: plan_snapshot
        // timestamps are ignored by the activity-recency calc.
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess-test.jsonl");
        // Header (created_at = 100), one user message at t=200, then
        // a much-later plan_snapshot at t=999_999 (simulating a
        // session click bumping the snapshot to "now").
        let lines = [
            r#"{"type":"header","id":"sess-test","model":"m","cwd":"","created_at":100}"#,
            r#"{"type":"user","content":[],"timestamp":200}"#,
            r#"{"type":"plan_snapshot","plan":null,"timestamp":999999}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let session = Session::load_from(&path).unwrap();
        // updated_at must come from the message timestamp (200), NOT
        // from the plan_snapshot (999_999).
        assert_eq!(
            session.updated_at, 200,
            "plan_snapshot should not bump updated_at; got {}",
            session.updated_at
        );
    }

    #[test]
    fn write_header_if_missing_idempotent_on_populated_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sess-test.jsonl");
        std::fs::write(&path, "existing line\n").unwrap();
        let session = Session::new("test-model", "/test/cwd");
        session.write_header_if_missing(&path).unwrap();
        // No header injected because the file already has content.
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "existing line\n");
    }

    // ── Sharded directory layout tests ──────────────────────────────

    #[test]
    fn shard_path_from_id_splits_hex_correctly() {
        // Normal hex id: first 8 chars after "sess-" become 4 dirs.
        assert_eq!(
            shard_path_from_id("sess-18b1f3dc2e3e8ed8")
                .to_str()
                .unwrap(),
            "18/b1/f3/dc"
        );
        // Shorter but still 8 hex chars.
        assert_eq!(
            shard_path_from_id("sess-aabbccdd").to_str().unwrap(),
            "aa/bb/cc/dd"
        );
        // Non-hex chars → no sharding (empty path).
        assert!(shard_path_from_id("sess-no-header").as_os_str().is_empty());
        // Too short (< 8 chars after prefix) → no sharding.
        assert!(shard_path_from_id("sess-abc").as_os_str().is_empty());
        // No "sess-" prefix → no sharding.
        assert!(shard_path_from_id("my-session").as_os_str().is_empty());
        // Mixed case hex → still valid.
        assert_eq!(
            shard_path_from_id("sess-AaBbCcDd1234").to_str().unwrap(),
            "Aa/Bb/Cc/Dd"
        );
    }

    #[test]
    fn save_and_load_roundtrip_uses_sharded_path() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        let mut session = Session::new("claude-sonnet-4-5", "/tmp/proj");
        // Force a known hex id for predictability.
        session.id = "sess-1234567890abcdef".into();
        session.sync(vec![Message::user("hello")]);

        store.save(&mut session).unwrap();

        // Verify file lands in the sharded directory.
        let expected = dir.path().join("12/34/56/78/sess-1234567890abcdef.jsonl");
        assert!(expected.exists(), "file should be at sharded path");

        // Round-trip through store.load.
        let loaded = store.load(&session.id).unwrap();
        assert_eq!(loaded.messages.len(), 1);
    }

    #[test]
    fn list_discovers_sharded_sessions() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        // Manually create two sessions in different shard dirs.
        let path_a = dir.path().join("aa/bb/cc/dd/sess-aabbccdd00000001.jsonl");
        std::fs::create_dir_all(path_a.parent().unwrap()).unwrap();
        std::fs::write(
            &path_a,
            concat!(
                r#"{"type":"header","id":"sess-aabbccdd00000001","model":"m1","cwd":"/a","created_at":100}"#,
                "\n",
                r#"{"type":"user","content":[{"type":"text","text":"hi"}],"timestamp":100}"#,
                "\n",
            ),
        )
        .unwrap();

        let path_b = dir.path().join("11/22/33/44/sess-1122334400000002.jsonl");
        std::fs::create_dir_all(path_b.parent().unwrap()).unwrap();
        std::fs::write(
            &path_b,
            concat!(
                r#"{"type":"header","id":"sess-1122334400000002","model":"m2","cwd":"/b","created_at":200}"#,
                "\n",
                r#"{"type":"user","content":[{"type":"text","text":"hi"}],"timestamp":200}"#,
                "\n",
            ),
        )
        .unwrap();

        let metas = store.list().unwrap();
        let ids: Vec<&str> = metas.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["sess-1122334400000002", "sess-aabbccdd00000001"]);
    }

    #[test]
    fn delete_removes_file_and_cleans_empty_shard_dirs() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());

        // Create a session via the store (uses sharded path).
        let mut session = Session::new("m", "/tmp");
        session.id = "sess-deadbeef00000001".into();
        session.sync(vec![Message::user("hi")]);
        store.save(&mut session).unwrap();

        let shard_dir = dir.path().join("de/ad/be/ef");
        assert!(shard_dir.join("sess-deadbeef00000001.jsonl").exists());

        store.delete(&session.id).unwrap();

        // File is gone.
        assert!(!shard_dir.join("sess-deadbeef00000001.jsonl").exists());
        // Empty shard directories are cleaned up.
        assert!(!shard_dir.exists());
        assert!(!dir.path().join("de/ad/be").exists());
        assert!(!dir.path().join("de/ad").exists());
        assert!(!dir.path().join("de").exists());
        // Store root still exists.
        assert!(dir.path().exists());
    }
}
