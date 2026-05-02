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
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MessageEvent {
    #[serde(rename = "type")]
    kind: String, // "user", "assistant", "system"
    content: Vec<crate::types::ContentBlock>,
    timestamp: u64,
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
        }
    }

    /// Sync the session with the latest agent history + bump `updated_at`.
    /// Only newly added messages (since `last_saved_count`) will be appended on
    /// the next save.
    pub fn sync(&mut self, messages: Vec<Message>) {
        self.messages = messages;
        self.updated_at = now_secs();
    }

    /// Append only the new messages (since `last_saved_count`) to the JSONL file.
    /// Writes the header line if the file doesn't exist yet.
    pub fn append_to(&mut self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file_exists = path.exists();
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;

        // Write header if this is a new file.
        if !file_exists {
            let header = SessionHeader {
                kind: "header".into(),
                id: self.id.clone(),
                model: self.model.clone(),
                cwd: self.cwd.clone(),
                created_at: self.created_at,
            };
            let line = serde_json::to_string(&header)?;
            writeln!(file, "{}", line)?;
        }

        // Append only unsaved messages.
        let new_messages = &self.messages[self.last_saved_count..];
        let now = now_secs();
        for msg in new_messages {
            let role_str = match msg.role {
                crate::types::Role::User => "user",
                crate::types::Role::Assistant => "assistant",
                crate::types::Role::System => "system",
            };
            let event = MessageEvent {
                kind: role_str.into(),
                content: msg.content.clone(),
                timestamp: now,
            };
            let line = serde_json::to_string(&event)?;
            writeln!(file, "{}", line)?;
        }

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

    /// Load a session from a JSONL file. Reads the header + all message events.
    pub fn load_from(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);

        let mut header: Option<SessionHeader> = None;
        let mut messages = Vec::new();
        let mut last_timestamp = 0u64;
        let mut title: Option<String> = None;
        let mut plan: Option<crate::tools::plan_state::Plan> = None;

        for (line_num, line_result) in reader.lines().enumerate() {
            let line = line_result?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let val: serde_json::Value = serde_json::from_str(line).map_err(|e| {
                Error::Config(format!(
                    "session parse ({}:{}): {e}",
                    path.display(),
                    line_num + 1
                ))
            })?;

            let kind = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

            if kind == "header" {
                let h: SessionHeader = serde_json::from_value(val).map_err(|e| {
                    Error::Config(format!("session header parse ({}): {e}", path.display()))
                })?;
                header = Some(h);
            } else if kind == "rename" {
                // Latest rename wins.
                let ev: RenameEvent = serde_json::from_value(val).map_err(|e| {
                    Error::Config(format!(
                        "session rename parse ({}:{}): {e}",
                        path.display(),
                        line_num + 1
                    ))
                })?;
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
                let ev: PlanSnapshotEvent = serde_json::from_value(val).map_err(|e| {
                    Error::Config(format!(
                        "session plan_snapshot parse ({}:{}): {e}",
                        path.display(),
                        line_num + 1
                    ))
                })?;
                if ev.timestamp > last_timestamp {
                    last_timestamp = ev.timestamp;
                }
                plan = ev.plan;
            } else if kind == "compaction" {
                // Replay checkpoint: everything accumulated so far is
                // archived-on-disk but gets replaced in memory by the
                // checkpoint's messages. Later `message` events in
                // the same file still append after this point.
                let ev: CompactionEvent = serde_json::from_value(val).map_err(|e| {
                    Error::Config(format!(
                        "session compaction parse ({}:{}): {e}",
                        path.display(),
                        line_num + 1
                    ))
                })?;
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
                            return Err(Error::Config(format!(
                                "session compaction ({}:{}): unknown role '{other}'",
                                path.display(),
                                line_num + 1
                            )))
                        }
                    };
                    messages.push(Message {
                        role,
                        content: cm.content,
                    });
                }
            } else {
                // Message event line
                let event: MessageEvent = serde_json::from_value(val).map_err(|e| {
                    Error::Config(format!(
                        "session event parse ({}:{}): {e}",
                        path.display(),
                        line_num + 1
                    ))
                })?;

                let role = match event.kind.as_str() {
                    "user" => crate::types::Role::User,
                    "assistant" => crate::types::Role::Assistant,
                    "system" => crate::types::Role::System,
                    other => {
                        return Err(Error::Config(format!(
                            "session parse ({}:{}): unknown message type '{other}'",
                            path.display(),
                            line_num + 1
                        )))
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

        let h = header.ok_or_else(|| {
            Error::Config(format!(
                "session parse ({}): missing header line",
                path.display()
            ))
        })?;

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
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
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
        writeln!(file, "{}", line)?;
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
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let trimmed = title.trim();
        let event = RenameEvent {
            kind: "rename".into(),
            title: trimmed.to_string(),
            timestamp: now_secs(),
        };
        let line = serde_json::to_string(&event)?;
        writeln!(file, "{}", line)?;
        self.title = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        self.updated_at = event.timestamp;
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
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let event = PlanSnapshotEvent {
        kind: "plan_snapshot".into(),
        plan: plan.cloned(),
        timestamp: now_secs(),
    };
    let line = serde_json::to_string(&event)?;
    writeln!(file, "{}", line)?;
    Ok(())
}

/// Directory-backed store for sessions.
#[derive(Debug, Clone)]
pub struct SessionStore {
    pub root: PathBuf,
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
        self.root.join(format!("{id}.jsonl"))
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
    pub fn list(&self) -> Result<Vec<SessionMeta>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root)?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(s) = Session::load_from(&path) {
                out.push(SessionMeta {
                    id: s.id,
                    updated_at: s.updated_at,
                    model: s.model,
                    message_count: s.messages.len(),
                    title: s.title,
                });
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(out)
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
    pub fn delete(&self, id: &str) -> Result<()> {
        Self::validate_id(id)?;
        let path = self.path_for(id);
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| Error::Config(format!("failed to delete session '{id}': {e}")))?;
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
    fn load_errors_cleanly_on_malformed_json() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let path = store.path_for("sess-bad");
        std::fs::write(&path, "{not-valid").unwrap();
        let err = store.load("sess-bad").unwrap_err();
        assert!(format!("{err}").contains("session parse"));
    }

    #[test]
    fn load_errors_on_missing_file() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        assert!(store.load("nope").is_err());
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
    fn load_errors_on_missing_header() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let path = store.path_for("sess-no-header");
        std::fs::write(&path, r#"{"type":"user","content":[],"timestamp":1}"#).unwrap();
        let err = store.load("sess-no-header").unwrap_err();
        assert!(format!("{err}").contains("missing header"));
    }
}
