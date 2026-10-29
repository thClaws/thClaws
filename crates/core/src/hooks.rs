//! Lifecycle hooks — user-defined shell commands that fire on agent events.
//!
//! Configured in `~/.config/thclaws/settings.json` (user) or
//! `.thclaws/settings.json` (project):
//!
//! ```json
//! {
//!   "hooks": {
//!     "pre_tool_use":  "echo \"tool: $THCLAWS_TOOL_NAME\" >> /tmp/thclaws.log",
//!     "post_tool_use": "echo \"done: $THCLAWS_TOOL_NAME\" >> /tmp/thclaws.log",
//!     "session_start": "notify-send 'thClaws started'",
//!     "session_end":   "notify-send 'thClaws ended'",
//!     "timeout_secs":  5,
//!     "fail_closed":   false
//!   }
//! }
//! ```
//!
//! ## `pre_tool_use` gate (the security-relevant one)
//!
//! `pre_tool_use` runs **synchronously as a gate** before every tool call
//! (including Bash, on every surface, and inherited by subagents). The hook
//! receives the tool name in `$THCLAWS_TOOL_NAME`, a *truncated* preview in
//! `$THCLAWS_TOOL_INPUT`, and — since the dev-plan/48 audit — the **FULL,
//! untruncated** tool input as JSON on **stdin** (`$THCLAWS_TOOL_INPUT_ON_STDIN=1`).
//! Read stdin to screen the complete command; the env var alone is capped at
//! [`MAX_HOOK_ENV_BYTES`] and a long command could hide its tail past it.
//!
//! Decision: `exit 2` **denies** (stderr is shown to the model as the reason).
//! By default everything else **allows** (fail-open — accidents/audit). Set
//! `"fail_closed": true` to make the gate your boundary: then a timeout, spawn
//! failure, or any non-`exit 0` outcome **denies**. Note: command screening is
//! still not a hard sandbox (obfuscation / absolute-path writes) — OS-level
//! confinement remains the real boundary.
//!
//! Hook commands run via `/bin/sh -c` (or platform default — see
//! [`crate::util::shell_command_sync`]). Hooks are fire-and-forget — the
//! agent loop does NOT wait for the hook to complete before proceeding.
//! A reaper task is spawned per child to call `wait()` so child processes
//! don't leak as zombies (M6.35 HOOK5). The reaper enforces a per-hook
//! timeout (default 5s, configurable via `timeout_secs`) and SIGKILLs on
//! expiry (M6.35 HOOK7).
//!
//! Hook stdin/stdout/stderr are redirected to `/dev/null` so a chatty
//! hook can't corrupt the parent terminal / GUI chat surface (M6.35
//! HOOK6). Hook scripts that want to log should write to a file
//! explicitly (e.g. `>> ~/.thclaws/hook.log`).
//!
//! Hook spawn failures are surfaced via `eprintln!` (always) plus an
//! optional broadcaster (M6.35 HOOK10) — `shared_session::run_worker`
//! registers one that forwards to `ViewEvent::SlashOutput` so GUI users
//! see broken hooks in the chat.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Default per-hook timeout in seconds. A hook child that hasn't exited
/// by this point is SIGKILLed by the reaper. Configurable per-config
/// via `HooksConfig.timeout_secs`.
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 5;

/// Maximum byte length of THCLAWS_TOOL_INPUT / THCLAWS_TOOL_OUTPUT env
/// var values. Larger values get truncated at a UTF-8 char boundary
/// with a `" … [truncated, originally <N> bytes]"` suffix (M6.35 HOOK9).
/// 8KB is generous for most tool inputs while staying well under
/// per-arg env limits (POSIX MAX_ARG_STRLEN ≈ 128KB but conservative).
pub const MAX_HOOK_ENV_BYTES: usize = 8192;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HooksConfig {
    pub pre_tool_use: Option<String>,
    pub post_tool_use: Option<String>,
    pub post_tool_use_failure: Option<String>,
    pub permission_denied: Option<String>,
    pub session_start: Option<String>,
    pub session_end: Option<String>,
    pub pre_compact: Option<String>,
    pub post_compact: Option<String>,
    /// Per-hook timeout. M6.35 HOOK7. Defaults to
    /// [`DEFAULT_HOOK_TIMEOUT_SECS`] when None / unset.
    pub timeout_secs: Option<u64>,
    /// dev-plan/48 security audit #2: when true, the `pre_tool_use` GATE
    /// fails **closed** — a timeout, spawn failure, or any non-zero /
    /// non-`exit 0` outcome **denies** the tool call instead of allowing
    /// it. Default `false` (fail-open, backward compatible) so a plain
    /// audit hook still behaves as before. Turn on when the hook IS your
    /// boundary and "the gate couldn't cleanly approve" must mean "block".
    pub fail_closed: bool,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            pre_tool_use: None,
            post_tool_use: None,
            post_tool_use_failure: None,
            permission_denied: None,
            session_start: None,
            session_end: None,
            pre_compact: None,
            post_compact: None,
            timeout_secs: None,
            fail_closed: false,
        }
    }
}

impl HooksConfig {
    /// Get the command for a hook event, if configured.
    pub fn get(&self, event: HookEvent) -> Option<&str> {
        let cmd = match event {
            HookEvent::PreToolUse => self.pre_tool_use.as_deref(),
            HookEvent::PostToolUse => self.post_tool_use.as_deref(),
            HookEvent::PostToolUseFailure => self.post_tool_use_failure.as_deref(),
            HookEvent::PermissionDenied => self.permission_denied.as_deref(),
            HookEvent::SessionStart => self.session_start.as_deref(),
            HookEvent::SessionEnd => self.session_end.as_deref(),
            HookEvent::PreCompact => self.pre_compact.as_deref(),
            HookEvent::PostCompact => self.post_compact.as_deref(),
        };
        cmd.filter(|s| !s.is_empty())
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS))
    }

    /// True when at least one hook is configured. Lets call sites
    /// short-circuit env-var construction when nothing's listening.
    pub fn any_configured(&self) -> bool {
        self.pre_tool_use.is_some()
            || self.post_tool_use.is_some()
            || self.post_tool_use_failure.is_some()
            || self.permission_denied.is_some()
            || self.session_start.is_some()
            || self.session_end.is_some()
            || self.pre_compact.is_some()
            || self.post_compact.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionDenied,
    SessionStart,
    SessionEnd,
    PreCompact,
    PostCompact,
}

impl HookEvent {
    /// Snake-case name matching the config field. M6.35 HOOK8 — pre-fix
    /// `format!("{event:?}")` emitted PascalCase ("PreToolUse"),
    /// inconsistent with the snake_case config keys hook scripts switch
    /// on (`case "$THCLAWS_HOOK_EVENT" in pre_tool_use) ...`).
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "pre_tool_use",
            HookEvent::PostToolUse => "post_tool_use",
            HookEvent::PostToolUseFailure => "post_tool_use_failure",
            HookEvent::PermissionDenied => "permission_denied",
            HookEvent::SessionStart => "session_start",
            HookEvent::SessionEnd => "session_end",
            HookEvent::PreCompact => "pre_compact",
            HookEvent::PostCompact => "post_compact",
        }
    }
}

// ── Error broadcaster (M6.35 HOOK10) ─────────────────────────────────

type ErrorBroadcaster = Box<dyn Fn(String) + Send + Sync>;

fn broadcaster_slot() -> &'static Mutex<Option<ErrorBroadcaster>> {
    static SLOT: OnceLock<Mutex<Option<ErrorBroadcaster>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Register a closure that receives hook error messages. The GUI worker
/// registers one that forwards to `ViewEvent::SlashOutput` so a hook
/// spawn failure / non-zero exit / timeout is visible in the chat
/// surface — pre-fix errors only went to stderr (invisible in GUI).
pub fn set_error_broadcaster<F>(f: F)
where
    F: Fn(String) + Send + Sync + 'static,
{
    if let Ok(mut g) = broadcaster_slot().lock() {
        *g = Some(Box::new(f));
    }
}

fn report_error(msg: String) {
    eprintln!("\x1b[33m[hook] {msg}\x1b[0m");
    let g = broadcaster_slot().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(f) = g.as_ref() {
        f(msg);
    }
}

// ── UTF-8 safe byte truncation (M6.35 HOOK9) ─────────────────────────

/// Truncate a string to at most `max_bytes` bytes, walking back to the
/// largest UTF-8 char boundary ≤ `max_bytes`. When trimmed, appends a
/// `" … [truncated, originally <N> bytes]"` marker so hook scripts can
/// detect the truncation. Pre-fix `chars().take(1000).collect()`
/// truncated by char count without a marker — multi-byte scripts (Thai,
/// Chinese, emoji) silently produced 4× larger env vars.
pub fn truncate_for_env(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{} … [truncated, originally {} bytes]", &s[..cut], s.len())
}

// ── Fire (zombie-reaped, timeout-bounded, null-stdio) ────────────────

/// Fire a hook command with the given environment variables.
/// Fire-and-forget — returns immediately. A tokio reaper task is
/// spawned per child to call `wait()` (so children don't leak as
/// zombies, M6.35 HOOK5) bounded by [`HooksConfig::timeout`] (M6.35
/// HOOK7 — SIGKILL on expiry). Stdin/stdout/stderr go to `/dev/null`
/// (M6.35 HOOK6) so a chatty hook can't corrupt the parent terminal.
///
/// Must be called from within a tokio runtime context (the reaper
/// uses `tokio::spawn`).
pub fn fire(config: &HooksConfig, event: HookEvent, env: &HashMap<String, String>) {
    let Some(cmd) = config.get(event) else { return };

    // Build a tokio Command (reusing the platform-shell logic via
    // shell_command_async so THCLAWS_SHELL overrides apply).
    let mut command = crate::util::shell_command_async(cmd);
    for k in crate::tools::bash::ENGINE_SECRET_ENV {
        command.env_remove(k);
    }

    command.env("THCLAWS_HOOK_EVENT", event.name());
    for (k, v) in env {
        command.env(k, v);
    }
    // M6.35 HOOK6: redirect all three to /dev/null (or NUL on Windows
    // — Stdio::null is cross-platform). Pre-fix the inherited stdio
    // mixed hook output into the parent terminal / GUI chat surface.
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());

    let timeout = config.timeout();
    let event_name = event.name();
    match command.spawn() {
        Ok(mut child) => {
            // M6.35 HOOK5 + HOOK7: reap + timeout. Detached so the
            // caller doesn't await.
            tokio::spawn(async move {
                match tokio::time::timeout(timeout, child.wait()).await {
                    Ok(Ok(status)) => {
                        if !status.success() {
                            let code = status
                                .code()
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "signal".to_string());
                            report_error(format!("{event_name} exited non-zero (code {code})"));
                        }
                    }
                    Ok(Err(e)) => {
                        report_error(format!("{event_name} wait failed: {e}"));
                    }
                    Err(_) => {
                        // Timed out — kill (best-effort).
                        let _ = child.kill().await;
                        report_error(format!(
                            "{event_name} timed out after {}s — killed",
                            timeout.as_secs()
                        ));
                    }
                }
            });
        }
        Err(e) => {
            report_error(format!("{event_name} spawn failed: {e}"));
        }
    }
}

// ── Convenience helpers ──────────────────────────────────────────────

/// Fire a pre_tool_use hook with tool name and (truncated) input.
pub fn fire_pre_tool_use(config: &HooksConfig, tool_name: &str, input: &str) {
    let mut env = HashMap::new();
    env.insert("THCLAWS_TOOL_NAME".into(), tool_name.into());
    env.insert(
        "THCLAWS_TOOL_INPUT".into(),
        truncate_for_env(input, MAX_HOOK_ENV_BYTES),
    );
    fire(config, HookEvent::PreToolUse, &env);
}

/// Decision returned by [`fire_pre_tool_use_gate`].
pub enum PreToolDecision {
    Allow,
    /// Deny the tool call; the string is surfaced to the model as the
    /// reason (the hook's stderr).
    Deny(String),
}

/// Run the `pre_tool_use` hook (if any) **synchronously as a gate** and
/// decide whether the tool call may proceed. Exit code **2** denies the
/// call (Claude-Code convention) with the hook's stderr as the reason;
/// every other outcome — no hook configured, success, any other non-zero
/// code, timeout, or spawn failure — **allows** it (fail-open). This is the
/// soft enforcement layer for accidents / accountability / policy; a hard
/// adversary boundary is OS-level confinement, not command screening.
///
/// Additive: a hook that exits 0 (or anything but 2) behaves exactly as the
/// fire-and-forget [`fire_pre_tool_use`] did before — only an explicit
/// `exit 2` now blocks. Must run inside a tokio runtime.
pub async fn fire_pre_tool_use_gate(
    config: &HooksConfig,
    tool_name: &str,
    input: &str,
) -> PreToolDecision {
    let Some(cmd) = config.get(HookEvent::PreToolUse) else {
        return PreToolDecision::Allow;
    };
    // Decide what a gate that COULDN'T cleanly approve means: deny when
    // `fail_closed`, else allow (the historical fail-open default).
    let on_gate_failure = |why: String| -> PreToolDecision {
        report_error(format!(
            "{why} — {}",
            if config.fail_closed {
                "denying (fail-closed)"
            } else {
                "allowing (fail-open)"
            }
        ));
        if config.fail_closed {
            PreToolDecision::Deny(format!("pre_tool_use gate could not approve: {why}"))
        } else {
            PreToolDecision::Allow
        }
    };

    let mut command = crate::util::shell_command_async(cmd);
    for k in crate::tools::bash::ENGINE_SECRET_ENV {
        command.env_remove(k);
    }
    command.env("THCLAWS_HOOK_EVENT", HookEvent::PreToolUse.name());
    command.env("THCLAWS_TOOL_NAME", tool_name);
    // The env var stays truncated (per-arg env limits), but the FULL,
    // untruncated tool input is also piped on STDIN — so a screening hook
    // sees the entire command even when it exceeds MAX_HOOK_ENV_BYTES (an
    // env-only hook could otherwise be bypassed by padding the command past
    // 8KB and hiding the dangerous tail). `THCLAWS_TOOL_INPUT_ON_STDIN=1`
    // tells the hook the full payload is available there.
    command.env(
        "THCLAWS_TOOL_INPUT",
        truncate_for_env(input, MAX_HOOK_ENV_BYTES),
    );
    command.env("THCLAWS_TOOL_INPUT_ON_STDIN", "1");
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return on_gate_failure(format!("spawn failed: {e}")),
    };
    // Stream the full input on stdin from a detached task so a hook that
    // writes a lot to stdout can't deadlock against our write.
    if let Some(mut stdin) = child.stdin.take() {
        let full = input.to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            let _ = stdin.write_all(full.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }

    let out = match tokio::time::timeout(config.timeout(), child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return on_gate_failure(format!("wait failed: {e}")),
        Err(_) => {
            return on_gate_failure(format!("timed out after {}s", config.timeout().as_secs()))
        }
    };

    match out.status.code() {
        // Explicit deny (Claude-Code convention) — stderr is the reason.
        Some(2) => {
            let reason = String::from_utf8_lossy(&out.stderr).trim().to_string();
            PreToolDecision::Deny(if reason.is_empty() {
                "blocked by pre_tool_use hook".to_string()
            } else {
                reason
            })
        }
        // Clean approve.
        Some(0) => PreToolDecision::Allow,
        // Any other outcome: fail-open allows (historical), fail-closed
        // denies (the hook didn't cleanly approve).
        other => {
            if config.fail_closed {
                let reason = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let code = other
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into());
                PreToolDecision::Deny(format!(
                    "pre_tool_use gate exited {code} without approving (fail-closed){}",
                    if reason.is_empty() {
                        String::new()
                    } else {
                        format!(": {reason}")
                    }
                ))
            } else {
                PreToolDecision::Allow
            }
        }
    }
}

/// Fire a post_tool_use (or _failure) hook with tool name and output.
pub fn fire_post_tool_use(config: &HooksConfig, tool_name: &str, output: &str, is_error: bool) {
    let event = if is_error {
        HookEvent::PostToolUseFailure
    } else {
        HookEvent::PostToolUse
    };
    let mut env = HashMap::new();
    env.insert("THCLAWS_TOOL_NAME".into(), tool_name.into());
    env.insert(
        "THCLAWS_TOOL_OUTPUT".into(),
        truncate_for_env(output, MAX_HOOK_ENV_BYTES),
    );
    env.insert("THCLAWS_TOOL_ERROR".into(), is_error.to_string());
    fire(config, event, &env);
}

/// Fire a permission_denied hook with tool name.
pub fn fire_permission_denied(config: &HooksConfig, tool_name: &str) {
    let mut env = HashMap::new();
    env.insert("THCLAWS_TOOL_NAME".into(), tool_name.into());
    fire(config, HookEvent::PermissionDenied, &env);
}

/// Fire session_start / session_end with session id + model.
pub fn fire_session(config: &HooksConfig, event: HookEvent, session_id: &str, model: &str) {
    let mut env = HashMap::new();
    env.insert("THCLAWS_SESSION_ID".into(), session_id.into());
    env.insert("THCLAWS_MODEL".into(), model.into());
    fire(config, event, &env);
}

/// Fire pre_compact / post_compact with message count + estimated tokens.
/// HOOK4: pre_compact / post_compact env vars were marked "—" in the
/// user manual; now exposes message_count + tokens so audit hooks can
/// log compaction footprint.
pub fn fire_compact(config: &HooksConfig, event: HookEvent, message_count: usize, tokens: usize) {
    let mut env = HashMap::new();
    env.insert("THCLAWS_COMPACT_MESSAGES".into(), message_count.to_string());
    env.insert("THCLAWS_COMPACT_TOKENS".into(), tokens.to_string());
    fire(config, event, &env);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gate_allows_when_no_hook() {
        let cfg = HooksConfig::default();
        assert!(matches!(
            fire_pre_tool_use_gate(&cfg, "bash", "{}").await,
            PreToolDecision::Allow
        ));
    }

    #[tokio::test]
    async fn gate_allows_on_exit_zero() {
        let cfg = HooksConfig {
            pre_tool_use: Some("true".into()),
            ..Default::default()
        };
        assert!(matches!(
            fire_pre_tool_use_gate(&cfg, "bash", "{}").await,
            PreToolDecision::Allow
        ));
    }

    #[tokio::test]
    async fn gate_allows_on_other_nonzero() {
        // Only exit 2 blocks; exit 1 stays allowed (additive with the old
        // fire-and-forget behaviour).
        let cfg = HooksConfig {
            pre_tool_use: Some("exit 1".into()),
            ..Default::default()
        };
        assert!(matches!(
            fire_pre_tool_use_gate(&cfg, "bash", "{}").await,
            PreToolDecision::Allow
        ));
    }

    #[tokio::test]
    async fn gate_denies_on_exit_two_with_stderr_reason() {
        let cfg = HooksConfig {
            pre_tool_use: Some("echo nope 1>&2; exit 2".into()),
            ..Default::default()
        };
        match fire_pre_tool_use_gate(&cfg, "bash", "{}").await {
            PreToolDecision::Deny(reason) => assert!(reason.contains("nope"), "reason={reason}"),
            PreToolDecision::Allow => panic!("exit 2 must deny"),
        }
    }

    /// audit #1: the FULL command reaches the hook on stdin, even when the
    /// dangerous part is PAST the 8KB env-var cutoff (the env-only bypass).
    #[tokio::test]
    async fn gate_sees_full_command_via_stdin_beyond_env_truncation() {
        // Hook screens stdin (default grep input) and blocks on FORBIDDEN.
        let cfg = HooksConfig {
            pre_tool_use: Some("grep -q FORBIDDEN && exit 2 || exit 0".into()),
            ..Default::default()
        };
        // FORBIDDEN sits AFTER >8KB of padding — truncated out of the env var,
        // but stdin carries the whole thing.
        let padding = "x".repeat(MAX_HOOK_ENV_BYTES + 500);
        let input = format!("{{\"command\":\"echo {padding}; : FORBIDDEN\"}}");
        match fire_pre_tool_use_gate(&cfg, "Bash", &input).await {
            PreToolDecision::Deny(_) => {}
            PreToolDecision::Allow => {
                panic!(
                    "gate missed FORBIDDEN past the env cutoff — stdin not delivering full input"
                )
            }
        }
    }

    /// audit #2: fail_closed denies on a non-0/non-2 exit and on timeout.
    #[tokio::test]
    async fn gate_fail_closed_denies_on_nonzero_and_timeout() {
        let cfg = HooksConfig {
            pre_tool_use: Some("exit 1".into()),
            fail_closed: true,
            ..Default::default()
        };
        assert!(
            matches!(
                fire_pre_tool_use_gate(&cfg, "Bash", "{}").await,
                PreToolDecision::Deny(_)
            ),
            "fail_closed must deny a non-zero exit"
        );

        let cfg_timeout = HooksConfig {
            pre_tool_use: Some("sleep 5".into()),
            fail_closed: true,
            timeout_secs: Some(1),
            ..Default::default()
        };
        assert!(
            matches!(
                fire_pre_tool_use_gate(&cfg_timeout, "Bash", "{}").await,
                PreToolDecision::Deny(_)
            ),
            "fail_closed must deny on timeout"
        );
    }

    #[test]
    fn get_returns_none_for_unconfigured_hooks() {
        let config = HooksConfig::default();
        assert!(config.get(HookEvent::PreToolUse).is_none());
        assert!(config.get(HookEvent::SessionStart).is_none());
    }

    #[test]
    fn get_returns_command_for_configured_hook() {
        let config = HooksConfig {
            pre_tool_use: Some("echo test".into()),
            ..Default::default()
        };
        assert_eq!(config.get(HookEvent::PreToolUse), Some("echo test"));
    }

    #[test]
    fn get_skips_empty_string() {
        let config = HooksConfig {
            pre_tool_use: Some(String::new()),
            ..Default::default()
        };
        assert!(config.get(HookEvent::PreToolUse).is_none());
    }

    /// M6.35 HOOK8: every event maps to its snake_case config-field name.
    /// Hook scripts that switch on $THCLAWS_HOOK_EVENT can rely on this.
    #[test]
    fn event_names_are_snake_case() {
        assert_eq!(HookEvent::PreToolUse.name(), "pre_tool_use");
        assert_eq!(HookEvent::PostToolUse.name(), "post_tool_use");
        assert_eq!(
            HookEvent::PostToolUseFailure.name(),
            "post_tool_use_failure"
        );
        assert_eq!(HookEvent::PermissionDenied.name(), "permission_denied");
        assert_eq!(HookEvent::SessionStart.name(), "session_start");
        assert_eq!(HookEvent::SessionEnd.name(), "session_end");
        assert_eq!(HookEvent::PreCompact.name(), "pre_compact");
        assert_eq!(HookEvent::PostCompact.name(), "post_compact");
    }

    /// M6.35 HOOK9: byte cap with truncation marker. Pre-fix
    /// `chars().take(1000).collect()` had no marker.
    #[test]
    fn truncate_for_env_passes_short_strings_unchanged() {
        let s = "hello world";
        assert_eq!(truncate_for_env(s, 100), s);
    }

    #[test]
    fn truncate_for_env_appends_marker_when_oversize() {
        let s = "x".repeat(MAX_HOOK_ENV_BYTES + 100);
        let out = truncate_for_env(&s, MAX_HOOK_ENV_BYTES);
        assert!(out.len() < MAX_HOOK_ENV_BYTES + 100);
        assert!(
            out.contains("[truncated, originally"),
            "missing marker: {}",
            &out[out.len().saturating_sub(80)..]
        );
        assert!(
            out.contains(&format!("originally {} bytes", s.len())),
            "marker missing original size"
        );
    }

    #[test]
    fn truncate_for_env_handles_multibyte_at_boundary() {
        // Thai chars are 3 bytes each. Construct a string whose byte at
        // MAX would fall mid-character; truncation must walk back to
        // the largest char boundary and produce valid UTF-8.
        let mut s = String::new();
        while s.len() <= 50 {
            s.push_str("เก็บข้อมูล");
        }
        let out = truncate_for_env(&s, 50);
        // Ensure no replacement char (U+FFFD) — invalid UTF-8 indicator.
        assert!(!out.contains('\u{FFFD}'), "invalid UTF-8 in truncation");
        assert!(out.contains("[truncated"));
    }

    /// Without an active tokio runtime we can't actually fire(). The
    /// no-hook short-circuit is the only path safe to test in a
    /// non-tokio context.
    #[test]
    fn fire_handles_missing_hook_gracefully() {
        let config = HooksConfig::default();
        fire(&config, HookEvent::PreToolUse, &HashMap::new());
    }

    /// M6.35 HOOK10: end-to-end test that fire() actually executes a
    /// command and the reaper completes without leaving zombies.
    /// Uses `true` (POSIX) which exits 0 immediately.
    #[tokio::test]
    async fn fire_actually_executes_command() {
        // Use a temp file so we can observe the side effect.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("fired");
        let cmd = format!("touch '{}'", marker.display());
        let config = HooksConfig {
            pre_tool_use: Some(cmd),
            ..Default::default()
        };
        fire(&config, HookEvent::PreToolUse, &HashMap::new());
        // Give the reaper task a beat to execute.
        for _ in 0..50 {
            if marker.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("hook did not execute within 1s — marker never appeared");
    }

    #[tokio::test]
    async fn fire_passes_env_vars_to_command() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("env_value");
        let cmd = format!(
            "echo \"$THCLAWS_TOOL_NAME:$THCLAWS_HOOK_EVENT\" > '{}'",
            marker.display()
        );
        let config = HooksConfig {
            pre_tool_use: Some(cmd),
            ..Default::default()
        };
        fire_pre_tool_use(&config, "Bash", "ls -la");
        for _ in 0..50 {
            if marker.exists() {
                let contents = std::fs::read_to_string(&marker).unwrap();
                assert!(
                    contents.contains("Bash:pre_tool_use"),
                    "expected snake_case event name + tool name, got: {}",
                    contents.trim()
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("hook did not write env-var output within 1s");
    }

    /// M6.35 HOOK7: a hook that hangs longer than `timeout_secs` gets
    /// SIGKILLed; the reaper completes without leaking the child.
    #[tokio::test]
    async fn fire_kills_hook_on_timeout() {
        let config = HooksConfig {
            // sleep 30s would leak without timeout enforcement.
            pre_tool_use: Some("sleep 30".into()),
            timeout_secs: Some(1),
            ..Default::default()
        };
        let start = std::time::Instant::now();
        fire(&config, HookEvent::PreToolUse, &HashMap::new());
        // The fire() returns immediately — we measure the reaper's
        // teardown completion via a process-counting probe. Simpler:
        // just sleep a bit longer than the timeout and trust the
        // reaper to have killed by then.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "fire should not block on the hung hook"
        );
        // No assertion on the kill itself (would need PID tracking) —
        // the test is mainly here to exercise the timeout path so a
        // future refactor that breaks it surfaces in coverage.
    }
}
