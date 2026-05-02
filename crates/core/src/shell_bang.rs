//! `!<command>` prompt prefix — user-initiated shell command execution.
//!
//! Mirrors the convention from IPython, Jupyter, Claude Code, and other
//! interactive shells where `!cmd` runs `cmd` in a subshell without
//! involving the agent. The output is shown to the user but NOT pushed
//! to the agent's history — this is for quick user actions between
//! turns (`!git status`, `!ls -la`, `!cargo check`) that the model
//! shouldn't be burdened with.
//!
//! Routes through [`crate::tools::BashTool`] so it inherits:
//!   - the [`crate::sandbox::Sandbox`] cwd restriction (same boundary
//!     enforcement file tools use)
//!   - the M6.8 non-interactive env vars (CI=1, NPM_CONFIG_YES, etc.)
//!   - venv auto-activation for pip/python commands
//!   - destructive-command detection
//!   - the lead/teammate forbidden-command guards
//!
//! Skipping the agent's approval gate is intentional and safe: the user
//! literally typed `!` to invoke this. They explicitly authorized the
//! shell call.

use crate::tools::Tool;

/// Detect a `!`-prefixed shell command and return the command text
/// (stripped of the prefix and trimmed). Returns `None` for non-bang
/// inputs and for `!` followed only by whitespace (those are no-ops,
/// not commands).
///
/// Note: `!=` and `!!` aren't intercepted here — the leading `!` is
/// followed by a non-space, non-`!` character to count as a bang
/// command. That keeps strings like `!=`/`!!` from accidentally
/// firing the shell escape when the user means to send literal text.
pub fn parse_bang(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('!')?;
    // Reject `!!` (no shell escape — leave as-is for whatever future
    // double-bang means) and pure whitespace after `!`.
    if rest.starts_with('!') {
        return None;
    }
    let cmd = rest.trim();
    if cmd.is_empty() {
        return None;
    }
    Some(cmd)
}

/// Run a `!`-prefixed shell command through the BashTool. Returns the
/// formatted output string on success, or an error message on failure.
/// Does NOT touch the agent's history.
///
/// The cwd defaults to the sandbox root (`Sandbox::root()`); BashTool
/// handles the resolution. Callers that want to display the result
/// should wrap it in their own prefix (e.g. `[!] cmd\n<output>`).
pub async fn run_bang_command(cmd: &str) -> Result<String, String> {
    let bash = crate::tools::BashTool;
    let input = serde_json::json!({ "command": cmd });
    bash.call(input)
        .await
        .map_err(|e| format!("[!] error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bang_strips_prefix_and_trims() {
        assert_eq!(parse_bang("!ls"), Some("ls"));
        assert_eq!(parse_bang("!  git status"), Some("git status"));
        assert_eq!(
            parse_bang("!cargo check --release"),
            Some("cargo check --release")
        );
    }

    #[test]
    fn parse_bang_handles_leading_whitespace() {
        // User pastes from an indented snippet — strip leading spaces
        // before checking the prefix.
        assert_eq!(parse_bang("   !pwd"), Some("pwd"));
        assert_eq!(parse_bang("\t!ls -la"), Some("ls -la"));
    }

    #[test]
    fn parse_bang_returns_none_for_non_bang_lines() {
        assert_eq!(parse_bang("ls"), None);
        assert_eq!(parse_bang("/help"), None);
        assert_eq!(parse_bang(""), None);
        assert_eq!(parse_bang("   "), None);
        assert_eq!(parse_bang("hello !world"), None);
    }

    #[test]
    fn parse_bang_returns_none_for_double_bang() {
        // `!!` is reserved for future use (e.g. "also push to agent
        // history"); don't accidentally fire shell escape.
        assert_eq!(parse_bang("!!ls"), None);
        assert_eq!(parse_bang("!!"), None);
    }

    #[test]
    fn parse_bang_returns_none_for_empty_command() {
        // Bare `!` with no command is a no-op, not a shell escape.
        assert_eq!(parse_bang("!"), None);
        assert_eq!(parse_bang("!   "), None);
        assert_eq!(parse_bang("!\t"), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_bang_command_executes_and_returns_output() {
        let out = run_bang_command("echo hello-bang").await.unwrap();
        assert!(out.contains("hello-bang"), "got: {out:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_bang_command_returns_error_for_nonzero_exit() {
        // Non-zero exit codes are rendered into the output string by
        // BashTool's format_output (with `[exit code N]`); they're not
        // promoted to a Rust Err. So the result is Ok with the exit
        // info embedded — same shape the agent loop sees.
        let out = run_bang_command("false").await.unwrap();
        assert!(out.contains("exit code"), "got: {out:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_bang_command_inherits_noninteractive_env() {
        // Sanity check that the BashTool routing applies the M6.8
        // non-interactive env vars. CI=1 is the canary.
        let out = run_bang_command("echo \"CI=$CI\"").await.unwrap();
        assert!(out.contains("CI=1"), "got: {out:?}");
    }
}
