//! Client-side tool-call audit (Enterprise Edition Phase 5, RFC 0001).
//!
//! Activated only by `policies.audit` in the signed org policy. With the
//! block absent every entry point here is one `Option` check. Records are
//! thin indexes into the session JSONL (digests + a bounded summary,
//! never payloads) and every sink is fail-open: a sink error is counted,
//! never surfaced to the tool call.

pub mod file;
pub mod http;
pub mod record;
pub mod sink;

use record::{Actor, ActorKind, Confine, Decision, Event, Host, Record};
use sink::AuditSink;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

struct Registered {
    sink: Box<dyn AuditSink>,
    dropped: AtomicU64,
}

struct Audit {
    sinks: Vec<Registered>,
    include_summary: bool,
    correlate_gateway: bool,
    host: Host,
}

static AUDIT: OnceLock<Option<Audit>> = OnceLock::new();
static SESSION: Mutex<Option<String>> = Mutex::new(None);
static TURN: AtomicU32 = AtomicU32::new(0);

fn audit() -> Option<&'static Audit> {
    AUDIT.get().and_then(|a| a.as_ref())
}

pub fn enabled() -> bool {
    audit().is_some()
}

/// Build sinks from the active policy. Call once at startup, after
/// `policy::load_or_refuse()`. Idempotent; a second call is a no-op.
pub fn init() {
    let _ = AUDIT.get_or_init(build);
}

fn build() -> Option<Audit> {
    let active = crate::policy::active()?;
    let p = active.policy.policies.audit.as_ref()?;
    if !p.enabled {
        return None;
    }
    let sinks = p
        .sinks
        .iter()
        .map(|s| {
            let sink: Box<dyn AuditSink> = match s {
                crate::policy::AuditSinkConfig::File { path } => Box::new(file::FileSink::new(
                    path.clone().unwrap_or_else(file::FileSink::default_path),
                )),
                crate::policy::AuditSinkConfig::Http {
                    url,
                    auth_header_template,
                    batch,
                    flush_secs,
                } => Box::new(http::HttpSink::new(
                    url.clone(),
                    auth_header_template.clone(),
                    *batch,
                    *flush_secs,
                )),
            };
            Registered {
                sink,
                dropped: AtomicU64::new(0),
            }
        })
        .collect();
    let policy_fp = std::fs::read(&active.source_path)
        .ok()
        .map(|b| record::sha256_hex(&b)[..8].to_string());
    Some(Audit {
        sinks,
        include_summary: p.include_summary,
        correlate_gateway: p.correlate_gateway,
        host: Host {
            engine: env!("CARGO_PKG_VERSION").to_string(),
            policy_fp,
            machine: Some(machine_id()),
        },
    })
}

/// Stable per machine+user, never reversible to either: sha256 of the
/// home dir path and login name.
fn machine_id() -> String {
    let home = crate::util::home_dir()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_default();
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    format!(
        "m-{}",
        &record::sha256_hex(format!("{home}\n{user}").as_bytes())[..8]
    )
}

fn actor() -> Actor {
    if let Some(id) = crate::multi_tenant::member::current_member_id() {
        return Actor {
            kind: ActorKind::Multiuser,
            id,
        };
    }
    if let Some(sso) = crate::policy::active().and_then(|a| a.policy.policies.sso.as_ref()) {
        if sso.enabled {
            if let Some(s) = crate::sso::current_session(sso) {
                if let Some(id) = s.email.clone().or_else(|| s.sub.clone()) {
                    return Actor {
                        kind: ActorKind::Sso,
                        id,
                    };
                }
            }
        }
    }
    Actor {
        kind: ActorKind::Os,
        id: std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown".into()),
    }
}

fn emit(a: &Audit, rec: &Record) {
    let line = rec.to_line();
    for r in &a.sinks {
        if let Err(e) = r.sink.write(&line) {
            let n = r.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 {
                eprintln!(
                    "[audit] {} sink dropped a record: {e} (fail-open; counted)",
                    r.sink.name()
                );
            }
        }
    }
}

fn base(a: &Audit, event: Event) -> Record {
    let sid = SESSION
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default();
    Record::base(
        event,
        sid,
        TURN.load(Ordering::Relaxed),
        actor(),
        a.host.clone(),
    )
}

pub fn dropped_counts() -> BTreeMap<String, u64> {
    let mut m = BTreeMap::new();
    if let Some(a) = audit() {
        for r in &a.sinks {
            let n = r.dropped.load(Ordering::Relaxed) + r.sink.dropped_extra();
            *m.entry(r.sink.name().to_string()).or_insert(0) += n;
        }
    }
    m
}

/// A session became current. Closes the previous one (if different)
/// with `session_end`, resets the turn counter, opens with `session_start`.
pub fn set_session(id: &str) {
    let Some(a) = audit() else { return };
    let prev = match SESSION.lock() {
        Ok(mut g) => {
            if g.as_deref() == Some(id) {
                return;
            }
            std::mem::replace(&mut *g, Some(id.to_string()))
        }
        Err(_) => return,
    };
    if let Some(prev) = prev {
        let mut rec = base(a, Event::SessionEnd);
        rec.session_id = prev;
        rec.dropped = Some(dropped_counts());
        emit(a, &rec);
    }
    TURN.store(0, Ordering::Relaxed);
    emit(a, &base(a, Event::SessionStart));
}

/// Called at the top of every agent turn. Only the main agent advances
/// the counter; subagent / side-channel turns are attributed to the user
/// turn that spawned them.
pub fn begin_turn(main_agent: bool) {
    if main_agent && enabled() {
        TURN.fetch_add(1, Ordering::Relaxed);
    }
}

/// Process exit: close the session and flush buffered sinks.
pub fn shutdown() {
    let Some(a) = audit() else { return };
    let sid = SESSION.lock().ok().and_then(|mut g| g.take());
    if let Some(sid) = sid {
        let mut rec = base(a, Event::SessionEnd);
        rec.session_id = sid;
        rec.dropped = Some(dropped_counts());
        emit(a, &rec);
    }
    for r in &a.sinks {
        r.sink.flush();
    }
}

/// Outbound headers for provider requests when `correlate_gateway` is on.
pub fn correlation_headers() -> Vec<(&'static str, String)> {
    let Some(a) = audit() else { return Vec::new() };
    if !a.correlate_gateway {
        return Vec::new();
    }
    let sid = SESSION
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default();
    vec![
        ("x-thclaws-session", sid),
        ("x-thclaws-turn", TURN.load(Ordering::Relaxed).to_string()),
    ]
}

/// One line for `/policy status`.
pub fn status_line() -> String {
    match audit() {
        None => "audit: off".into(),
        Some(a) => {
            let sinks: Vec<String> = a
                .sinks
                .iter()
                .map(|r| {
                    format!(
                        "{}(dropped {})",
                        r.sink.name(),
                        r.dropped.load(Ordering::Relaxed) + r.sink.dropped_extra()
                    )
                })
                .collect();
            format!(
                "audit: on — sinks {} · turn {} · correlate_gateway={}",
                sinks.join(", "),
                TURN.load(Ordering::Relaxed),
                a.correlate_gateway
            )
        }
    }
}

/// Everything the dispatch site knows about one tool call.
pub struct ToolCall<'a> {
    pub tool_use_id: &'a str,
    pub tool: &'a dyn crate::tools::Tool,
    pub input: &'a serde_json::Value,
    pub decision: Decision,
    pub decided_by: &'static str,
    /// Materialized result text as pushed into history (post-truncation).
    pub output: &'a str,
    pub is_error: bool,
    pub duration_ms: u64,
}

fn fill_tool_fields(
    a: &Audit,
    rec: &mut Record,
    tool_use_id: &str,
    tool: &dyn crate::tools::Tool,
    input: &serde_json::Value,
) {
    rec.tool_use_id = Some(tool_use_id.to_string());
    rec.tool = Some(tool.name().to_string());
    rec.tool_kind = Some(tool.audit_kind());
    rec.mcp_server = tool.audit_mcp_server().map(str::to_string);
    rec.input_sha256 = Some(record::sha256_hex(
        serde_json::to_string(input).unwrap_or_default().as_bytes(),
    ));
    if tool.name() == "Bash" {
        let (mode, enforced) = crate::confine::enforcement_state();
        rec.confine = Some(Confine {
            mode: mode.as_str(),
            enforced,
        });
    }
    if let Some(s) = tool.audit_summary(input) {
        if !s.targets.is_empty() {
            let mut t = s.targets;
            t.truncate(record::MAX_TARGETS);
            rec.targets = Some(t);
        }
        if a.include_summary {
            rec.summary = s
                .summary
                .as_deref()
                .map(record::clamp_summary)
                .filter(|s| !s.is_empty());
        }
    }
}

pub fn record_tool_call(call: ToolCall<'_>) {
    let Some(a) = audit() else { return };
    let mut rec = base(a, Event::ToolCall);
    fill_tool_fields(a, &mut rec, call.tool_use_id, call.tool, call.input);
    rec.decision = Some(call.decision);
    rec.decided_by = Some(call.decided_by);
    rec.output_sha256 = Some(record::sha256_hex(call.output.as_bytes()));
    rec.is_error = Some(call.is_error);
    rec.duration_ms = Some(call.duration_ms);
    emit(a, &rec);
}

pub fn record_denied(
    tool_use_id: &str,
    tool: &dyn crate::tools::Tool,
    input: &serde_json::Value,
    decided_by: &'static str,
    reason: &str,
) {
    let Some(a) = audit() else { return };
    let mut rec = base(a, Event::ToolDenied);
    fill_tool_fields(a, &mut rec, tool_use_id, tool, input);
    rec.decision = Some(Decision::Deny);
    rec.decided_by = Some(decided_by);
    rec.deny_reason = Some(record::clamp_summary(reason));
    emit(a, &rec);
}

pub fn decision_of(d: &crate::permissions::ApprovalDecision) -> Decision {
    match d {
        crate::permissions::ApprovalDecision::Allow => Decision::Allow,
        crate::permissions::ApprovalDecision::AllowForSession => Decision::AllowForSession,
        crate::permissions::ApprovalDecision::Deny => Decision::Deny,
    }
}

pub use record::ToolKind as AuditToolKind;
