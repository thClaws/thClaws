//! Shared helpers for tool-call progress labels, duration formatting,
//! heartbeat text, and result summaries. Used by the REPL, team loops,
//! shared-session worker, and event renderer to avoid divergent label logic.

use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;

// ── constants ──────────────────────────────────────────────────────

/// Maximum visible characters for a tool preview inside brackets.
const PREVIEW_CAP: usize = 60;

/// First heartbeat fires after this many seconds.
pub(crate) const HEARTBEAT_FIRST_AFTER: Duration = Duration::from_secs(5);

/// Subsequent heartbeats fire every this many seconds.
pub(crate) const HEARTBEAT_EVERY: Duration = Duration::from_secs(15);

/// "Thinking" heartbeat: fires when no output for this long during a turn.
pub(crate) const THINKING_HEARTBEAT_AFTER: Duration = Duration::from_secs(10);

// ── spinner ───────────────────────────────────────────────────────

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Spinner animation interval — 100ms for smooth braille animation.
pub(crate) const SPINNER_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) fn spinner_frame(tick: u32) -> char {
    SPINNER_FRAMES[(tick as usize) % SPINNER_FRAMES.len()]
}

// ── secret redaction ───────────────────────────────────────────────

struct RedactionPattern {
    regex: Regex,
    replacement: &'static str,
}

static REDACTION_PATTERNS: OnceLock<Vec<RedactionPattern>> = OnceLock::new();

fn redaction_patterns() -> &'static [RedactionPattern] {
    REDACTION_PATTERNS.get_or_init(|| {
        vec![
            RedactionPattern {
                regex: Regex::new(r"(?i)\b(authorization\s*:\s*bearer\s+)[^\s;&]+").unwrap(),
                replacement: "${1}<redacted>",
            },
            RedactionPattern {
                regex: Regex::new(r"(?i)\b(bearer\s+)[^\s;&]+").unwrap(),
                replacement: "${1}<redacted>",
            },
            RedactionPattern {
                regex: Regex::new(
                    r"(?i)(--(?:api-key|api_key|token|password|secret)(?:=|\s+))[^\s;&]+",
                )
                .unwrap(),
                replacement: "${1}<redacted>",
            },
            RedactionPattern {
                regex: Regex::new(
                    r"(?i)(\w*(?:api[_-]?key|apikey|token|password|secret))=([^\s;&]+)",
                )
                .unwrap(),
                replacement: "${1}=<redacted>",
            },
        ]
    })
}

/// Redact known secret patterns from `s`. Patterns are intentionally
/// key-shaped and case-insensitive so common environment and CLI flag
/// variants are covered without matching ordinary words.
fn redact_secrets(s: &str) -> String {
    let mut out = s.to_string();
    for pat in redaction_patterns() {
        out = pat.regex.replace_all(&out, pat.replacement).into_owned();
    }
    out
}

static SENSITIVE_KEY_REGEX: OnceLock<Regex> = OnceLock::new();

fn is_sensitive_key(key: &str) -> bool {
    let re = SENSITIVE_KEY_REGEX.get_or_init(|| {
        Regex::new(r"(?i)^(api[_-]?key|apikey|token|password|secret|authorization|credentials?)$")
            .unwrap()
    });
    re.is_match(key)
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub(crate) fn redact_json_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(redact_secrets(s)),
        Value::Array(items) => Value::Array(items.iter().map(redact_json_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let redacted_v = if is_sensitive_key(k) {
                        match v {
                            Value::String(_) => Value::String("<redacted>".to_string()),
                            _ => redact_json_value(v),
                        }
                    } else {
                        redact_json_value(v)
                    };
                    (redact_secrets(k), redacted_v)
                })
                .collect(),
        ),
        _ => value.clone(),
    }
}

// ── sanitization ───────────────────────────────────────────────────

/// Strip control characters and collapse whitespace into single spaces.
pub(crate) fn sanitize_label_field(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to `cap` visible characters, appending "…" if truncated.
fn truncate(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        let taken: String = s.chars().take(cap).collect();
        format!("{taken}…")
    }
}

fn preview(s: &str, cap: usize) -> String {
    truncate(&sanitize_label_field(&redact_secrets(s)), cap)
}

// ── label building ─────────────────────────────────────────────────

/// Build a compact tool label suitable for inline display.
///
/// Returns something like:
/// - `Bash (cargo test -p thclaws-core)`
/// - `Read (crates/core/src/repl.rs)`
/// - `Grep (ToolCallStart)`
/// - `WebFetch (https://example.com/…)`
/// - `Task (agent=dev-glm)`
/// - fallback: `ToolName`
pub(crate) fn tool_label(name: &str, input: &serde_json::Value) -> String {
    let detail = match name {
        "Bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| preview(c, PREVIEW_CAP)),
        "Read" | "Write" | "Edit" => input
            .get("file_path")
            .or_else(|| input.get("path"))
            .and_then(|v| v.as_str())
            .map(|p| preview(p, PREVIEW_CAP)),
        "Glob" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|p| preview(p, PREVIEW_CAP)),
        "Grep" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|p| preview(p, PREVIEW_CAP)),
        "WebFetch" => input
            .get("url")
            .and_then(|v| v.as_str())
            .map(|u| preview(u, 60)),
        "WebSearch" => input
            .get("query")
            .and_then(|v| v.as_str())
            .map(|q| preview(q, PREVIEW_CAP)),
        "Skill" => input
            .get("skill")
            .or_else(|| input.get("name"))
            .and_then(|v| v.as_str())
            .map(|n| preview(n, PREVIEW_CAP)),
        "ToolSearch" => input
            .get("query")
            .and_then(|v| v.as_str())
            .map(|q| preview(q, PREVIEW_CAP)),
        "Agent" => input
            .get("description")
            .and_then(|v| v.as_str())
            .map(|d| preview(d, PREVIEW_CAP)),
        "Task" => input
            .get("agent")
            .and_then(|v| v.as_str())
            .map(|a| format!("agent={}", preview(a, PREVIEW_CAP))),
        "AskUserQuestion" => input
            .get("question")
            .and_then(|v| v.as_str())
            .map(|q| preview(q, PREVIEW_CAP)),
        _ => None,
    };

    match detail {
        Some(d) if !d.is_empty() => format!("{name} ({d})"),
        _ => name.to_string(),
    }
}

// ── duration formatting ────────────────────────────────────────────

pub(crate) fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    if total_secs < 60 {
        format!("{total_secs}s")
    } else {
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        format!("{mins}m {secs:02}s")
    }
}

// ── formatted output strings ───────────────────────────────────────

/// Heartbeat text for a long-running tool.
///
/// Example: `[tool: Bash (sleep 60)] still running 45s`
pub(crate) fn format_tool_heartbeat(label: &str, elapsed: Duration) -> String {
    format!("[tool: {label}] still running {}", format_duration(elapsed))
}

/// Inline spinner line for an active tool — overwrites current line via `\r`.
pub(crate) fn format_tool_spinner(label: &str, elapsed: Duration, tick: u32) -> String {
    let frame = spinner_frame(tick);
    let dur = format_duration(elapsed);
    format!("\r\x1b[2K\x1b[2m  {frame} {label:<50} {dur}\x1b[0m")
}

/// Final completion line — clears spinner and writes ✓/✗ with newline.
pub(crate) fn format_tool_done(label: &str, elapsed: Duration, is_error: bool) -> String {
    let icon = if is_error { '✗' } else { '✓' };
    let dur = format_duration(elapsed);
    format!("\r\x1b[2K\x1b[2m  {icon} {label:<50} {dur}\x1b[0m\n")
}

/// Inline thinking spinner — overwrites current line with a brightness
/// wave that sweeps left-to-right across the phrase and timer.
pub(crate) fn format_thinking_spinner(elapsed: Duration, tick: u32) -> String {
    let frame = spinner_frame(tick);
    let dur = format_duration(elapsed);
    let secs = elapsed.as_secs();
    let phrase = if secs < 15 {
        "Thinking"
    } else if secs < 45 {
        "Working"
    } else {
        "Still working"
    };
    let text = format!("{phrase} ({dur})");
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let gap = 6;
    let wave_pos = (tick as usize) % (len + gap);
    let spinner_color = match (tick / 3) % 4 {
        0 => "\x1b[2;36m",
        1 => "\x1b[36m",
        2 => "\x1b[96m",
        _ => "\x1b[36m",
    };
    let mut out = format!("\r\x1b[2K{spinner_color}  {frame}\x1b[0m ");
    for (i, ch) in chars.iter().enumerate() {
        let dist = wave_pos.abs_diff(i);
        let color = if dist == 0 {
            "\x1b[96m"
        } else if dist <= 2 {
            "\x1b[36m"
        } else {
            "\x1b[2;36m"
        };
        out.push_str(color);
        out.push(*ch);
    }
    out.push_str("\x1b[0m");
    out
}

/// Clear thinking line when real output arrives.
pub(crate) fn clear_thinking_line() -> String {
    "\r\x1b[2K".to_string()
}

// ── active tool state ──────────────────────────────────────────────

/// Tracks a tool call in progress for heartbeat and duration reporting.
#[derive(Debug, Clone)]
pub(crate) struct ActiveToolDisplay {
    pub label: String,
    pub started_at: std::time::Instant,
    pub last_heartbeat_at: std::time::Instant,
}

impl ActiveToolDisplay {
    pub fn new(label: String) -> Self {
        let now = std::time::Instant::now();
        Self {
            label,
            started_at: now,
            last_heartbeat_at: now,
        }
    }

    /// Elapsed time since the tool started.
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Whether a heartbeat is due (first after `HEARTBEAT_FIRST_AFTER`,
    /// then every `HEARTBEAT_EVERY`).
    pub fn heartbeat_due(&self) -> bool {
        let since_start = self.started_at.elapsed();
        let since_last = self.last_heartbeat_at.elapsed();
        if since_start < HEARTBEAT_FIRST_AFTER {
            return false;
        }
        if self.last_heartbeat_at == self.started_at {
            return true;
        }
        since_last >= HEARTBEAT_EVERY
    }
}

/// Pick the oldest tool that is due for a heartbeat, if any.
pub(crate) fn oldest_due_heartbeat(
    active: &std::collections::HashMap<String, ActiveToolDisplay>,
) -> Option<(&String, &ActiveToolDisplay)> {
    active
        .iter()
        .filter(|(_, td)| td.heartbeat_due())
        .min_by_key(|(_, td)| td.started_at)
}

/// Compute the delay until the next heartbeat should fire.
/// Returns `HEARTBEAT_EVERY` when no tools are active. The caller can
/// still safely poll the timer, but avoids scheduling an extreme
/// `Duration::MAX` sleep.
pub(crate) fn next_heartbeat_delay(
    active: &std::collections::HashMap<String, ActiveToolDisplay>,
) -> Duration {
    if active.is_empty() {
        return HEARTBEAT_EVERY;
    }
    active
        .values()
        .map(|td| {
            let elapsed = td.started_at.elapsed();
            if elapsed < HEARTBEAT_FIRST_AFTER {
                HEARTBEAT_FIRST_AFTER - elapsed
            } else if td.last_heartbeat_at == td.started_at {
                Duration::ZERO
            } else {
                let since_last = td.last_heartbeat_at.elapsed();
                if since_last >= HEARTBEAT_EVERY {
                    Duration::ZERO
                } else {
                    HEARTBEAT_EVERY - since_last
                }
            }
        })
        .min()
        .unwrap_or(Duration::MAX)
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn label_bash_command() {
        let label = tool_label("Bash", &json!({"command": "cargo test -p thclaws-core"}));
        assert!(label.starts_with("Bash ("));
        assert!(label.contains("cargo test"));
    }

    #[test]
    fn label_bash_truncation() {
        let long = "x".repeat(200);
        let label = tool_label("Bash", &json!({"command": long}));
        assert!(label.contains('…'));
        let inner = label
            .strip_prefix("Bash (")
            .unwrap()
            .strip_suffix(')')
            .unwrap();
        assert!(inner.chars().count() <= PREVIEW_CAP + 1); // +1 for …
    }

    #[test]
    fn label_read_path() {
        let label = tool_label("Read", &json!({"path": "src/main.rs"}));
        assert_eq!(label, "Read (src/main.rs)");
    }

    #[test]
    fn label_edit_path() {
        let label = tool_label("Edit", &json!({"path": "crates/core/src/repl.rs"}));
        assert_eq!(label, "Edit (crates/core/src/repl.rs)");
    }

    #[test]
    fn label_grep_pattern() {
        let label = tool_label("Grep", &json!({"pattern": "ToolCallStart"}));
        assert_eq!(label, "Grep (ToolCallStart)");
    }

    #[test]
    fn label_webfetch_url() {
        let label = tool_label(
            "WebFetch",
            &json!({"url": "https://example.com/api/v1/data"}),
        );
        assert!(label.starts_with("WebFetch ("));
        assert!(label.contains("example.com"));
    }

    #[test]
    fn label_task_agent() {
        let label = tool_label("Task", &json!({"agent": "dev-glm"}));
        assert_eq!(label, "Task (agent=dev-glm)");
    }

    #[test]
    fn label_unknown_tool() {
        let label = tool_label("FancyTool", &json!({}));
        assert_eq!(label, "FancyTool");
    }

    #[test]
    fn label_fallback_on_missing_field() {
        let label = tool_label("Bash", &json!({}));
        assert_eq!(label, "Bash");
    }

    #[test]
    fn redact_token() {
        let result = redact_secrets("curl -H token=abc123 https://api.example.com");
        assert!(result.contains("token=<redacted>"));
        assert!(!result.contains("abc123"));
    }

    #[test]
    fn redact_bearer() {
        let result = redact_secrets("Authorization: Bearer sk-12345");
        assert!(result.contains("Authorization: Bearer <redacted>"));
        assert!(!result.contains("sk-12345"));
    }

    #[test]
    fn redact_api_key() {
        let result = redact_secrets("api_key=deadbeef123&other=val");
        assert!(result.contains("api_key=<redacted>"));
        assert!(!result.contains("deadbeef"));
        assert!(result.contains("&other=val"));
    }

    #[test]
    fn redact_case_insensitive_and_cli_flags() {
        let result = redact_secrets("TOKEN=abc --token def --api-key=ghi authorization:bearer jkl");
        assert!(result.contains("TOKEN=<redacted>"));
        assert!(result.contains("--token <redacted>"));
        assert!(result.contains("--api-key=<redacted>"));
        assert!(result.contains("authorization:bearer <redacted>"));
        assert!(!result.contains("abc"));
        assert!(!result.contains("def"));
        assert!(!result.contains("ghi"));
        assert!(!result.contains("jkl"));
    }

    #[test]
    fn labels_redact_non_bash_fields() {
        let web = tool_label(
            "WebFetch",
            &json!({"url": "https://example.com/?token=abc"}),
        );
        let grep = tool_label("Grep", &json!({"pattern": "API_KEY=secret"}));
        let question = tool_label(
            "AskUserQuestion",
            &json!({"question": "Use Authorization: Bearer sk-123?"}),
        );
        assert!(web.contains("token=<redacted>"));
        assert!(grep.contains("API_KEY=<redacted>"));
        assert!(question.contains("Bearer <redacted>"));
        assert!(!web.contains("abc"));
        assert!(!grep.contains("secret"));
        assert!(!question.contains("sk-123"));
    }

    #[test]
    fn redact_json_value_redacts_nested_strings() {
        let redacted = redact_json_value(&json!({
            "command": "curl -H 'Authorization: Bearer sk-123'",
            "nested": { "url": "https://example.com/?token=abc" },
            "todos": [{ "content": "normal task", "status": "pending" }]
        }));
        assert_eq!(redacted["todos"][0]["content"], "normal task");
        assert!(!redacted.to_string().contains("sk-123"));
        assert!(!redacted.to_string().contains("token=abc"));
    }

    #[test]
    fn redact_json_value_redacts_sensitive_keys() {
        let redacted = redact_json_value(&json!({
            "token": "sk-123",
            "api_key": "deadbeef",
            "password": "p@ss",
            "Authorization": "Bearer sk-456",
            "safe_field": "visible",
            "nested": { "secret": "shhh", "data": "ok" }
        }));
        assert_eq!(redacted["token"], "<redacted>");
        assert_eq!(redacted["api_key"], "<redacted>");
        assert_eq!(redacted["password"], "<redacted>");
        assert_eq!(redacted["Authorization"], "<redacted>");
        assert_eq!(redacted["safe_field"], "visible");
        assert_eq!(redacted["nested"]["secret"], "<redacted>");
        assert_eq!(redacted["nested"]["data"], "ok");
    }

    #[test]
    fn redact_preserves_normal_text() {
        let input = "cargo test --no-run";
        assert_eq!(redact_secrets(input), input);
    }

    #[test]
    fn sanitize_strips_control_chars() {
        let result = sanitize_label_field("hello\nworld\ttab");
        assert_eq!(result, "hello world tab");
    }

    #[test]
    fn format_duration_sub_second() {
        assert_eq!(format_duration(Duration::from_millis(300)), "0s");
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_secs(42)), "42s");
    }

    #[test]
    fn format_duration_minutes_seconds() {
        assert_eq!(format_duration(Duration::from_secs(72)), "1m 12s");
    }

    #[test]
    fn format_tool_heartbeat_text() {
        let text = format_tool_heartbeat("Bash (sleep 60)", Duration::from_secs(45));
        assert!(text.contains("still running"));
        assert!(text.contains("45s"));
    }

    #[test]
    fn active_tool_heartbeat_not_due_immediately() {
        let td = ActiveToolDisplay::new("test".to_string());
        assert!(!td.heartbeat_due());
    }

    #[test]
    fn active_tool_first_heartbeat_due_after_first_interval() {
        let mut td = ActiveToolDisplay::new("test".to_string());
        let start = std::time::Instant::now() - HEARTBEAT_FIRST_AFTER;
        td.started_at = start;
        td.last_heartbeat_at = start;
        assert!(td.heartbeat_due());
    }

    #[test]
    fn active_tool_second_heartbeat_waits_for_repeat_interval() {
        let mut td = ActiveToolDisplay::new("test".to_string());
        td.started_at = std::time::Instant::now() - HEARTBEAT_FIRST_AFTER - Duration::from_secs(1);
        td.last_heartbeat_at = std::time::Instant::now();
        assert!(!td.heartbeat_due());
        td.last_heartbeat_at = std::time::Instant::now() - HEARTBEAT_EVERY;
        assert!(td.heartbeat_due());
    }

    #[test]
    fn next_heartbeat_delay_handles_empty_and_due() {
        let empty = std::collections::HashMap::new();
        assert_eq!(next_heartbeat_delay(&empty), HEARTBEAT_EVERY);

        let mut active = std::collections::HashMap::new();
        let mut td = ActiveToolDisplay::new("test".to_string());
        let start = std::time::Instant::now() - HEARTBEAT_FIRST_AFTER;
        td.started_at = start;
        td.last_heartbeat_at = start;
        active.insert("1".to_string(), td);
        assert_eq!(next_heartbeat_delay(&active), Duration::ZERO);
    }

    #[test]
    fn label_ask_user_question() {
        let label = tool_label(
            "AskUserQuestion",
            &json!({"question": "Which library should we use?"}),
        );
        assert!(label.starts_with("AskUserQuestion ("));
        assert!(label.contains("Which library"));
    }

    #[test]
    fn label_skill_name() {
        let label = tool_label("Skill", &json!({"name": "prp-plan"}));
        assert_eq!(label, "Skill (prp-plan)");
    }

    #[test]
    fn label_websearch_query() {
        let label = tool_label("WebSearch", &json!({"query": "rust tokio select"}));
        assert_eq!(label, "WebSearch (rust tokio select)");
    }
}
