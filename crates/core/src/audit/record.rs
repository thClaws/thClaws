//! `audit.v1` record type. Field order mirrors `record.v1.schema.json`,
//! which is the public contract (docs/rfc/0001-tool-call-audit.md on
//! the mirror); the schema test in this module keeps them in sync.

use serde::Serialize;

pub const SCHEMA_JSON: &str = include_str!("record.v1.schema.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    ToolCall,
    ToolDenied,
    SessionStart,
    SessionEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Builtin,
    Mcp,
    Plugin,
    Workflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Sso,
    Multiuser,
    Os,
}

#[derive(Debug, Clone, Serialize)]
pub struct Actor {
    pub kind: ActorKind,
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Host {
    pub engine: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_fp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    AllowForSession,
    Deny,
}

#[derive(Debug, Clone, Serialize)]
pub struct Confine {
    pub mode: &'static str,
    pub enforced: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub v: u8,
    pub ts: String,
    pub event: Event,
    pub session_id: String,
    pub turn: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_kind: Option<ToolKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_server: Option<String>,
    pub actor: Actor,
    pub host: Host,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deny_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confine: Option<Confine>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub targets: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped: Option<std::collections::BTreeMap<String, u64>>,
}

impl Record {
    pub fn base(event: Event, session_id: String, turn: u32, actor: Actor, host: Host) -> Self {
        Self {
            v: 1,
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            event,
            session_id,
            turn,
            tool_use_id: None,
            tool: None,
            tool_kind: None,
            mcp_server: None,
            actor,
            host,
            decision: None,
            decided_by: None,
            deny_reason: None,
            confine: None,
            targets: None,
            summary: None,
            input_sha256: None,
            output_sha256: None,
            is_error: None,
            duration_ms: None,
            dropped: None,
        }
    }

    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

pub const MAX_SUMMARY_BYTES: usize = 256;
pub const MAX_TARGETS: usize = 64;

/// Truncate on a char boundary so the JSON stays valid UTF-8 and the
/// field never exceeds the schema's `maxLength` (counted in chars, so a
/// byte cap is always at least as strict).
pub fn clamp_summary(s: &str) -> String {
    let s = s.lines().next().unwrap_or("").trim();
    if s.len() <= MAX_SUMMARY_BYTES {
        return s.to_string();
    }
    let mut end = MAX_SUMMARY_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validator() -> jsonschema::Validator {
        let schema: serde_json::Value = serde_json::from_str(SCHEMA_JSON).unwrap();
        jsonschema::validator_for(&schema).unwrap()
    }

    fn actor() -> Actor {
        Actor {
            kind: ActorKind::Os,
            id: "jimmy".into(),
        }
    }
    fn host() -> Host {
        Host {
            engine: "0.0.0".into(),
            policy_fp: Some("9f3c1a2b".into()),
            machine: Some("m-4d1e".into()),
        }
    }

    #[test]
    fn tool_call_record_validates() {
        let mut r = Record::base(Event::ToolCall, "sess-1".into(), 3, actor(), host());
        r.tool_use_id = Some("toolu_1".into());
        r.tool = Some("Bash".into());
        r.tool_kind = Some(ToolKind::Builtin);
        r.decision = Some(Decision::Allow);
        r.decided_by = Some("repl");
        r.confine = Some(Confine {
            mode: "workspace",
            enforced: true,
        });
        r.summary = Some("git status".into());
        r.input_sha256 = Some(sha256_hex(b"{}"));
        r.output_sha256 = Some(sha256_hex(b"ok"));
        r.is_error = Some(false);
        r.duration_ms = Some(42);
        let v: serde_json::Value = serde_json::from_str(&r.to_line()).unwrap();
        assert!(validator().validate(&v).is_ok(), "{}", r.to_line());
        // Key order is part of the contract.
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(&keys[..5], &["v", "ts", "event", "session_id", "turn"]);
    }

    #[test]
    fn tool_call_without_output_digest_is_rejected() {
        let mut r = Record::base(Event::ToolCall, "s".into(), 1, actor(), host());
        r.tool_use_id = Some("t".into());
        r.tool = Some("Bash".into());
        r.tool_kind = Some(ToolKind::Builtin);
        r.decision = Some(Decision::Allow);
        r.decided_by = Some("auto");
        r.input_sha256 = Some(sha256_hex(b"{}"));
        let v: serde_json::Value = serde_json::from_str(&r.to_line()).unwrap();
        assert!(validator().validate(&v).is_err());
    }

    #[test]
    fn denied_record_validates_and_requires_deny() {
        let mut r = Record::base(Event::ToolDenied, "s".into(), 1, actor(), host());
        r.tool_use_id = Some("t".into());
        r.tool = Some("Write".into());
        r.tool_kind = Some(ToolKind::Builtin);
        r.decision = Some(Decision::Deny);
        r.decided_by = Some("hook");
        r.deny_reason = Some("blocked".into());
        r.input_sha256 = Some(sha256_hex(b"{}"));
        let v: serde_json::Value = serde_json::from_str(&r.to_line()).unwrap();
        assert!(validator().validate(&v).is_ok());
        r.decision = Some(Decision::Allow);
        let v: serde_json::Value = serde_json::from_str(&r.to_line()).unwrap();
        assert!(validator().validate(&v).is_err());
    }

    #[test]
    fn session_end_carries_dropped_counts() {
        let mut r = Record::base(Event::SessionEnd, "s".into(), 9, actor(), host());
        r.dropped = Some([("file".to_string(), 2u64)].into_iter().collect());
        let v: serde_json::Value = serde_json::from_str(&r.to_line()).unwrap();
        assert!(validator().validate(&v).is_ok());
    }

    #[test]
    fn clamp_summary_first_line_and_char_boundary() {
        assert_eq!(clamp_summary("git status\nrm -rf /"), "git status");
        let thai = "ก".repeat(200); // 3 bytes each → 600 bytes
        let c = clamp_summary(&thai);
        assert!(c.len() <= MAX_SUMMARY_BYTES);
        assert!(c.chars().all(|ch| ch == 'ก'));
    }
}
