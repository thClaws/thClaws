//! `Bash` — run an arbitrary shell command via `/bin/sh -c`.
//!
//! Always requires approval (`requires_approval -> true`) until allow-list
//! patterns land. Captures stdout + stderr separately, interleaves in the
//! returned string, and enforces a default 120000ms timeout (max 600000ms).
//! On timeout the child is killed and any partial output is discarded —
//! we report the timeout clearly rather than return half-baked state.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::time::{timeout, Duration};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

#[derive(Default)]
pub struct BashTool {
    /// When set, a session cancel (Stop / Esc / Cmd+. — all send
    /// `shell_cancel`) kills the in-flight child process PROMPTLY instead of
    /// letting it run to its own `timeout_ms`. The default `with_builtins`
    /// registration carries `None`; `run_worker` re-registers a cancel-aware
    /// instance over it (register overwrites by tool name) with the session's
    /// token. Reported by @Mayth01 (issue #182).
    cancel: Option<crate::cancel::CancelToken>,
}

impl BashTool {
    /// Cancel-aware constructor: the returned tool races the given token
    /// against the child's wait, so a mid-call cancel kills the process.
    pub fn with_cancel(cancel: crate::cancel::CancelToken) -> Self {
        Self {
            cancel: Some(cancel),
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn audit_summary(&self, input: &Value) -> Option<super::AuditSummary> {
        input
            .get("command")
            .and_then(Value::as_str)
            .map(super::AuditSummary::text)
    }

    fn description(&self) -> &'static str {
        "Run a shell command via `/bin/sh -c`. Captures stdout and stderr. \
         Default timeout: 120000ms (override with `timeout` in milliseconds, max 600000). \
         Always requires approval. Use this for general operations (git, build, \
         test, curl, ls -l, rm, etc.) that the specialized tools don't cover. \
         Runs from the workspace root. Invoke programs by name so the shell \
         resolves them via PATH (e.g. `python script.py`) — this works even when \
         the interpreter is installed outside the workspace. Do NOT fabricate an \
         absolute path to an interpreter; a guessed/wrong path just fails. \
         Reference scripts and files by paths inside the workspace. (Only the \
         `cwd` argument is sandboxed to the workspace — the command itself is not, \
         but a made-up path won't exist.) \
         IMPORTANT: For long-running processes (servers, watchers, dev servers), \
         append ` &` to run in background, or use `timeout 10 command` to sample \
         initial output. Never run a server in foreground — it blocks until timeout."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to run"
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory (default: current directory)"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default 120000, max 600000)"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Legacy alias: timeout in seconds (converted to ms internally)"
                },
                "description": {
                    "type": "string",
                    "description": "Brief description of what this command does"
                }
            },
            "required": ["command"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        // Every Bash command requires approval. (Pre-#125 this had an
        // `is_destructive_command` branch returning `true` above an
        // unconditional `true` — a dead discriminator. `is_destructive_command`
        // still drives the destructive-command *warning* highlight below;
        // the gate itself is unconditional until an elevated approval tier
        // exists.)
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_command = req_str(&input, "command")?;
        let cwd = input.get("cwd").and_then(Value::as_str);

        let resolved_cwd = if let Some(c) = cwd {
            crate::sandbox::Sandbox::check(c)?
        } else if let Some(root) = crate::sandbox::Sandbox::root() {
            root
        } else {
            crate::workdir::current_workdir()
        };

        // Auto-activate venv for pip/python commands when no venv exists yet.
        let raw_command = maybe_wrap_with_venv(raw_command, &resolved_cwd);

        let timeout_ms = input
            .get("timeout")
            .and_then(Value::as_u64)
            .or_else(|| {
                input
                    .get("timeout_secs")
                    .and_then(Value::as_u64)
                    .map(|s| s * 1000)
            })
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        // Chained commands like "pip install X && uvicorn app --port 8800":
        // Split at `&&`, run setup parts synchronously, then run the server
        // part with a short capture timeout so it doesn't block forever.
        let (setup_parts, server_part) = split_chained_server_command(&raw_command);

        // Run setup commands first (if any).
        let mut setup_output = String::new();
        if !setup_parts.is_empty() {
            let setup_cmd = setup_parts.join(" && ");
            eprintln!(
                "\x1b[33m[running setup: {}]{}\x1b[0m",
                setup_cmd.chars().take(80).collect::<String>(),
                if setup_cmd.len() > 80 { "…" } else { "" }
            );
            setup_output = run_shell_command(
                &setup_cmd,
                &resolved_cwd,
                timeout_ms,
                false,
                self.cancel.as_ref(),
            )
            .await?;
            // If setup failed, return its output (includes exit code).
            if setup_output.contains("[exit code") {
                return Ok(setup_output);
            }
            // If there's no server part, just return setup output.
            if server_part.is_none() {
                return Ok(setup_output);
            }
        }

        // If we split out a server part, ensure venv is activated for it too.
        let command = match server_part {
            Some(ref srv) => {
                let venv_activate = resolved_cwd.join(".venv/bin/activate");
                if venv_activate.exists() {
                    format!("source {} && {}", venv_activate.display(), srv)
                } else {
                    srv.clone()
                }
            }
            None => raw_command.to_string(),
        };
        let is_server = is_server_command(&command) && !command.trim().ends_with('&');

        // Lead-only hard block. The team lead is a coordinator — destructive
        // workspace ops have repeatedly cascade-killed teammate worktrees
        // and processes when the LLM lead reached for `git reset --hard` or
        // `rm -rf` to "clean up" unexpected state. The prompt rule alone is
        // honor-system in --accept-all mode; this is the seatbelt.
        if let Some(reason) = lead_forbidden_command(&command) {
            return Err(Error::Tool(format!(
                "team lead is not allowed to run this command: it would {reason}. \
                 Lead is a COORDINATOR — destructive workspace ops belong to \
                 teammates inside their own worktrees, never the lead. If a \
                 merge looks weird or git state is unexpected, send a message \
                 to the user describing what you see — do NOT attempt recovery \
                 with `git reset`, `rm -rf`, or `git worktree remove`. Use \
                 `git status`, `git log`, `git diff` to inspect; use TeamMerge \
                 and SendMessage to act."
            )));
        }
        // Teammate-only hard block. Catches the cross-branch `git reset
        // --hard main` pattern that wiped frontend's worktree last run.
        // Same-branch recovery (HEAD~N, sha) stays allowed.
        if let Some(reason) = teammate_forbidden_command(&command) {
            return Err(Error::Tool(format!(
                "teammate is not allowed to run this command: it would {reason}."
            )));
        }

        if is_destructive_command(&command) {
            eprintln!(
                "\x1b[33m⚠ destructive command detected: {}\x1b[0m",
                command.chars().take(80).collect::<String>()
            );
        }

        if is_server {
            eprintln!(
                "\x1b[33m[server command detected — will capture 5s of startup then return]\x1b[0m"
            );
        }

        let effective_timeout = if is_server { 5000 } else { timeout_ms };
        let mut server_output = run_shell_command(
            &command,
            &resolved_cwd,
            effective_timeout,
            is_server,
            self.cancel.as_ref(),
        )
        .await?;

        // F30: concurrent team members sharing one git index collide on
        // `.git/index.lock` ("fatal: Unable to create '.../.git/index.lock':
        // File exists"). That's a transient race, not a real failure — retry
        // a few times with a short backoff before surfacing it. Gated on the
        // specific error signature so ordinary output isn't re-run, and
        // skipped for server commands (which are expected to keep running).
        if !is_server {
            let mut tries: u32 = 0;
            while tries < 3
                && server_output.contains("index.lock")
                && server_output.contains("File exists")
            {
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(250 * tries as u64)).await;
                server_output = run_shell_command(
                    &command,
                    &resolved_cwd,
                    effective_timeout,
                    is_server,
                    self.cancel.as_ref(),
                )
                .await?;
            }
        }

        // Combine setup output with server output.
        if setup_output.is_empty() {
            Ok(server_output)
        } else {
            Ok(format!("{setup_output}\n{server_output}"))
        }
    }
}

/// Run a single shell command, capturing stdout/stderr.
/// If `is_server` is true, a timeout is expected — the server keeps running
/// and we return immediately without killing it.
async fn run_shell_command(
    command: &str,
    cwd: &std::path::Path,
    timeout_ms: u64,
    is_server: bool,
    cancel: Option<&crate::cancel::CancelToken>,
) -> Result<String> {
    let out = run_shell_command_inner(command, cwd, timeout_ms, is_server, true, cancel).await?;
    // If the OS confiner couldn't enforce, it bailed BEFORE running the
    // command (exiting with a no-enforce sentinel, NOT the command's output).
    // Re-run unconfined so the command actually executes — the workspace /
    // container remains the security boundary in environments where the
    // kernel confiner is unavailable (e.g. a hosted runner whose container
    // kernel returns EINVAL from the Landlock syscalls). Without this, every
    // Bash call (and `!` shell escape) fails with "landlock setup failed,
    // exit 78" and no output.
    if crate::confine::output_shows_no_enforce(&out) {
        // Remember so later commands skip the doomed confined attempt.
        crate::confine::mark_no_enforce();
        return run_shell_command_inner(command, cwd, timeout_ms, is_server, false, cancel).await;
    }
    Ok(out)
}

/// Kills a spawned command's whole process group when dropped.
///
/// `Child::kill` signals only the process tokio spawned. With the confiner on
/// that is bwrap, and the command the user typed is its grandchild: killing
/// bwrap leaves the grandchild reparented to init, still running, while the UI
/// says "(interrupted)". Pairing `process_group(0)` at spawn with a group kill
/// here covers every way this function can end — cancel, timeout, an error
/// return, or the whole future being dropped out from under us by the worker
/// loop's cancel race. (public issue #194)
///
/// Disarmed on the deliberate background-server path, where outliving the call
/// is the entire point.
struct KillGroupOnDrop {
    /// `None` once disarmed or already killed, and on platforms where we have
    /// no group to kill.
    pgid: Option<i32>,
}

impl KillGroupOnDrop {
    #[cfg(unix)]
    fn arm(child: &tokio::process::Child) -> Self {
        // With `process_group(0)` the child leads its own group, so the group
        // id is its pid.
        Self {
            pgid: child.id().map(|id| id as i32),
        }
    }

    #[cfg(not(unix))]
    fn arm(_child: &tokio::process::Child) -> Self {
        Self { pgid: None }
    }

    /// Let the group outlive this call.
    fn disarm(&mut self) {
        self.pgid = None;
    }

    /// Kill now rather than waiting for the drop, and don't kill twice.
    fn kill_now(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            kill_group(pgid);
        }
    }
}

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        self.kill_now();
    }
}

#[cfg(unix)]
fn kill_group(pgid: i32) {
    // SAFETY: kill(2) with a normal signal is always safe to call; a group
    // that has already exited returns ESRCH, which we ignore. The negative
    // pid is what makes this address the group rather than one process.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_pgid: i32) {}

async fn run_shell_command_inner(
    command: &str,
    cwd: &std::path::Path,
    timeout_ms: u64,
    is_server: bool,
    confined: bool,
    cancel: Option<&crate::cancel::CancelToken>,
) -> Result<String> {
    // dev-plan/49: route through the OS confiner — returns a sandbox-exec /
    // bwrap-wrapped `sh -c` when bash.sandbox is on and a confiner is
    // available, else a plain `sh -c` (unchanged). One chokepoint, so
    // subagent/workflow Bash is confined identically. `confined=false` is the
    // unconfined re-run path when the confiner couldn't enforce.
    let mut cmd = if confined {
        crate::confine::shell_command_async(command)
    } else {
        crate::util::shell_command_async(command)
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(cwd);

    // M6.8 B1: signal "I'm not interactive" to common CLI tools so
    // they don't open prompts the sandbox can't answer (`pnpm create
    // vite` had been failing with `└ Operation cancelled` because its
    // interactive picker hit EOF on stdin). Most modern Node / Python
    // / package-manager CLIs respect at least one of these. The env
    // is additive; user shells can still override per-command via
    // `VAR=value cmd` syntax.
    apply_noninteractive_env(&mut cmd);
    scrub_sensitive_env(&mut cmd);

    // Own process group, so a kill reaches the whole tree rather than the one
    // process tokio tracks. Under the confiner the direct child is bwrap (or
    // sandbox-exec), and the command the user actually ran is its grandchild —
    // killing the direct child left that grandchild reparented to init and
    // running to completion, which is what made Stop report "(interrupted)"
    // while a `sleep 45` kept going. (public issue #194)
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Tool(format!("spawn: {e}")))?;

    // Kill the group from *any* drop, not just the cancel branch below. The
    // worker loop races the whole turn stream against cancel with `biased`
    // priority and returns on cancel, dropping this future before the select
    // below is ever polled again — so the explicit kill there never runs on
    // the path that matters. A Drop impl is the only thing that survives that.
    let mut group = KillGroupOnDrop::arm(&child);

    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| Error::Tool("missing stdout pipe".into()))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| Error::Tool("missing stderr pipe".into()))?;

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        buf
    });

    // Race the session cancel against the child's wait so Stop / Esc / Cmd+.
    // kills a long-running command (build, test, `sleep 45`) PROMPTLY instead
    // of letting it run to `timeout_ms`. `cancelled()` returns immediately if
    // the token is already set, but the worker resets it before each turn (the
    // same discipline the main agent loop relies on), so this only fires on a
    // genuine mid-call cancel. `None` (e.g. the `!` shell escape) keeps the
    // old timeout-only behaviour. Server commands never race — a timeout there
    // is the expected "left it running in the background" path. (issue #182)
    let wait_result = match (cancel, is_server) {
        (Some(tok), false) => {
            tokio::select! {
                biased;
                _ = tok.cancelled() => {
                    // The guard kills the group on the way out; this is the
                    // prompt path for a cancel we actually observed.
                    group.kill_now();
                    let _ = child.kill().await;
                    return Err(Error::Tool(format!(
                        "cancelled while running: {command}"
                    )));
                }
                r = timeout(Duration::from_millis(timeout_ms), child.wait()) => r,
            }
        }
        _ => timeout(Duration::from_millis(timeout_ms), child.wait()).await,
    };
    match wait_result {
        Err(_) if is_server => {
            // Server command — timeout is expected. Server keeps running, so
            // the group must survive this function returning.
            group.disarm();
            //
            // M6.8: drain stdout/stderr with a short sub-timeout so we
            // capture boot-log output (port number, ready banner) and,
            // critically, surface anything a misclassified scaffolder
            // printed before getting stuck. Earlier code dropped both
            // reader tasks here, which silently lost the actual output
            // when `is_server_command` had a false positive. The
            // 200ms drain window is long enough to flush typical
            // banner output without blocking on a quiet server's
            // long-lived pipe.
            let drain = Duration::from_millis(200);
            let stdout_drained = tokio::time::timeout(drain, stdout_task).await;
            let stderr_drained = tokio::time::timeout(drain, stderr_task).await;
            let stdout_bytes = stdout_drained.ok().and_then(|r| r.ok()).unwrap_or_default();
            let stderr_bytes = stderr_drained.ok().and_then(|r| r.ok()).unwrap_or_default();
            let stdout = String::from_utf8_lossy(&stdout_bytes);
            let stderr = String::from_utf8_lossy(&stderr_bytes);

            let mut parts: Vec<String> = Vec::new();
            parts.push(
                "Server started and running in background.\n\
                 The process will continue after this tool returns.\n\
                 Use `curl localhost:PORT` or a browser to verify."
                    .to_string(),
            );
            // Append captured boot output if any — the model gets to
            // see ready banners, port numbers, or (on a misfire) the
            // actual scaffolder output that explains what really
            // happened.
            let trimmed_out = stdout.trim_end_matches('\n');
            let trimmed_err = stderr.trim_end_matches('\n');
            if !trimmed_out.is_empty() {
                parts.push(format!("\n[stdout]\n{trimmed_out}"));
            }
            if !trimmed_err.is_empty() {
                parts.push(format!("\n[stderr]\n{trimmed_err}"));
            }
            Ok(parts.join(""))
        }
        Err(_) => {
            group.kill_now();
            let _ = child.kill().await;
            Err(Error::Tool(format!(
                "timeout after {}ms running: {command}",
                timeout_ms
            )))
        }
        Ok(Err(e)) => Err(Error::Tool(format!("wait: {e}"))),
        Ok(Ok(status)) => {
            let stdout_bytes = stdout_task.await.unwrap_or_default();
            let stderr_bytes = stderr_task.await.unwrap_or_default();
            let stdout = String::from_utf8_lossy(&stdout_bytes);
            let stderr = String::from_utf8_lossy(&stderr_bytes);
            let exit_code = status.code().unwrap_or(-1);
            let out = format_output(&stdout, &stderr, exit_code);
            // dev-plan/49: if the OS sandbox likely blocked a write, tell the
            // model it's the sandbox (not a real perms error) + how to allow it.
            Ok(match crate::confine::denied_hint(&out) {
                Some(hint) => format!("{out}\n\n{hint}"),
                None => out,
            })
        }
    }
}

/// Split a chained command like "pip install X && uvicorn app --port 8800"
/// into setup parts and an optional server part. If the last segment of a
/// `&&`-chain is a server command, it's extracted separately so we can run
/// setup synchronously and then start the server with a short capture timeout.
fn split_chained_server_command(cmd: &str) -> (Vec<String>, Option<String>) {
    // Only split on top-level `&&` (not inside quotes/subshells — good enough
    // for the common pip install && uvicorn pattern).
    let parts: Vec<&str> = cmd.split("&&").map(|s| s.trim()).collect();
    if parts.len() < 2 {
        // Single command — no splitting needed.
        return (vec![], None);
    }
    let last = parts.last().unwrap();
    if is_server_command(last) {
        let setup: Vec<String> = parts[..parts.len() - 1]
            .iter()
            .map(|s| s.to_string())
            .collect();
        (setup, Some(last.to_string()))
    } else {
        // No server command at the end — run as one unit.
        (vec![], None)
    }
}

/// If `cmd` contains a bare `pip install` and there's no venv in the cwd,
/// create one and activate it before running the command.
fn maybe_wrap_with_venv(cmd: &str, cwd: &std::path::Path) -> String {
    if !needs_venv(cmd) {
        return cmd.to_string();
    }
    // Already inside a venv (e.g. the command itself sources activate)?
    if cmd.contains("activate") || cmd.contains("venv/bin/") || cmd.contains(".venv/bin/") {
        return cmd.to_string();
    }
    let venv_dir = cwd.join(".venv");
    if venv_dir.join("bin/activate").exists() {
        // venv exists but isn't activated — activate it.
        eprintln!("\x1b[33m[auto-activating .venv before pip]\x1b[0m");
        format!("source {}/bin/activate && {}", venv_dir.display(), cmd)
    } else {
        // No venv at all — create + activate.
        eprintln!("\x1b[33m[creating .venv and activating before pip]\x1b[0m");
        format!(
            "python3 -m venv {} && source {}/bin/activate && {}",
            venv_dir.display(),
            venv_dir.display(),
            cmd
        )
    }
}

/// Does this command need a Python venv? Trigger ONLY on commands that
/// actually install or run framework servers — pip/pipx/poetry/uv,
/// long-running server runners (uvicorn/gunicorn/flask), and the
/// pytest/celery toolchains.
///
/// Plain `python3 script.py` does NOT trigger here. The previous
/// heuristic fired on every `python3 ` prefix, which:
///   - Wrapped agent-shipped stdlib scripts (e.g. image-generator's
///     batch.py) in `python3 -m venv && source && python3 script.py`,
///   - Printed `[creating .venv and activating before pip]` +
///     `⚠ destructive command detected` warnings the model then
///     mis-attributed to the script itself,
///   - Created a `.venv/` directory inside agent workspaces that had
///     no business owning one.
/// If a user actually needs venv-bound python, they call `pip` or
/// activate the venv themselves; both still get auto-handled here.
fn needs_venv(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    lower.contains("pip install")
        || lower.contains("pip3 install")
        || lower.contains("pipx ")
        || lower.contains("poetry install")
        || lower.contains("poetry add")
        || lower.contains("uv pip ")
        || lower.contains("uvicorn ")
        || lower.contains("gunicorn ")
        || lower.contains("hypercorn ")
        || lower.contains("flask run")
        || lower.contains("django")
        || lower.contains("manage.py")
        || lower.contains("fastapi")
        || lower.contains("pytest")
        || lower.contains("celery ")
}

/// Detect commands that are potentially destructive to the filesystem or system.
///
/// This feeds the approval prompt's risk-highlighting; `BashTool` already
/// requires approval for every command. We lowercase + normalise
/// whitespace before matching so a crafty `rm  -rf` (double-space) or
/// tab-separated variant can't slip past the classifier just because
/// it doesn't hit the exact ASCII byte sequence we listed.
/// True when this process is a teammate (spawned by SpawnTeammate with
/// `THCLAWS_TEAM_AGENT` set), as opposed to the lead or a standalone session.
fn is_teammate_process() -> bool {
    std::env::var("THCLAWS_TEAM_AGENT").is_ok()
}

/// Distinguish a benign `git reset --hard` ref (recovery on the teammate's
/// own branch) from the dangerous "reset to a different branch" pattern
/// that wiped frontend's worktree in our last run.
///
/// Allowed (safe): `HEAD`, `HEAD~N`, `HEAD^`, `HEAD@{N}`, hex shas (≥7 hex
/// chars), tags (`tags/...`).
/// Blocked: anything else — bare branch names like `main`, `master`, `dev`,
/// remote refs like `origin/main`, sibling team branches like `team/backend`.
fn ref_resets_to_different_branch(target: &str) -> bool {
    if target.is_empty() {
        return false;
    }
    let lower = target.to_lowercase();
    if lower == "head" || lower.starts_with("head~") || lower.starts_with("head^") {
        return false;
    }
    if lower.starts_with("head@{") {
        return false;
    }
    if lower.starts_with("tags/") || lower.starts_with("refs/tags/") {
        return false;
    }
    // Hex SHA (full or abbreviated, ≥7 chars). Anything less is too short
    // to disambiguate and most likely a branch name.
    if target.len() >= 7 && target.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    true
}

// ── thClaws#125: shell-aware seatbelt hardening ─────────────────────────
// The lead/teammate guards below historically matched destructive commands
// by lowercase + collapse-whitespace + substring search. That's defeated by
// any shell-quoting trick `/bin/sh -c` expands back to the destructive form
// (`r''m -rf`, `$(printf rm)`, `${x:-rm}`, backticks, `{rm,-rf,..}`,
// `IFS`-splicing, `eval $'\x72\x6d'`, arg-order swaps, quoted verbs). We
// harden by (a) resolving each command through `shell_words` — defeating
// quotes, escapes, wrapper prefixes, arg order, and `eval`/`sh -c`
// indirection — and (b) refusing any command that carries an *unresolved*
// substitution AND a destructive flag signal. No string analysis is airtight
// against a determined model (OS-level confinement is the real boundary,
// tracked separately) but this raises the prompt-injection bar a lot.

/// Wrapper verbs that prefix a real command without changing what runs.
const WRAPPER_PREFIXES: &[&str] = &[
    "sudo", "env", "nice", "time", "nohup", "command", "stdbuf", "setsid", "ionice",
];

/// The lead's hard-block table: `(pattern as it appears in the canonical
/// lowercased form, human reason)`.
const LEAD_BLOCKED: &[(&str, &str)] = &[
    ("git reset --hard", "discard committed work via hard reset"),
    ("git clean -f", "delete untracked files"),
    ("git clean -d", "delete untracked directories"),
    ("git push --force", "rewrite shared history with force-push"),
    ("git push -f ", "rewrite shared history with force-push"),
    ("git rebase", "rewrite committed history"),
    (
        "git worktree remove",
        "kill a teammate's active worktree (and its process)",
    ),
    (
        "git worktree prune",
        "purge worktree metadata referenced by live teammates",
    ),
    ("git checkout -- ", "discard a teammate's uncommitted work"),
    ("git checkout .", "discard a teammate's uncommitted work"),
    (
        "git restore --worktree",
        "discard a teammate's uncommitted work",
    ),
    ("git restore .", "discard a teammate's uncommitted work"),
    (
        "git merge --abort",
        "tear down a merge instead of resolving via the responsible teammate",
    ),
    ("rm -rf", "destructively remove files"),
    ("rm -fr", "destructively remove files"),
    ("rm -r ", "recursively remove files"),
];

const LEAD_OBFUSCATED: &str = "obfuscated/dynamic destructive command refused in team-lead mode — the seatbelt can't verify a command built via $VAR / $(...) / backticks / brace-expansion / eval. Run a plain literal command, or hand the destructive step to the responsible teammate";

const TEAMMATE_RESET_REASON: &str = "reset to a different branch / remote ref — would discard your branch's commits and overwrite your worktree with someone else's tree. Use `git reset --hard HEAD~N` or `git reset --hard <sha>` if you genuinely need to undo your own commits, OR ask the lead to handle the merge instead";

/// `cmd` contains shell substitution/expansion whose effective form can't be
/// resolved statically (var/command substitution, ANSI-C `$'…'`, `{a,b}`).
fn looks_obfuscated(cmd: &str) -> bool {
    cmd.contains('$') || cmd.contains('`') || brace_list(cmd)
}

fn brace_list(cmd: &str) -> bool {
    let mut depth = 0i32;
    for c in cmd.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth = (depth - 1).max(0),
            ',' if depth > 0 => return true,
            _ => {}
        }
    }
    false
}

/// Destructive flag signals that stay literal even when the verb is
/// obfuscated. Kept to unambiguous multi-char flags to avoid false hits on
/// legitimate obfuscated-but-harmless commands (`echo $(date)`).
fn has_destructive_signal(cmd: &str) -> bool {
    let l = cmd.to_lowercase();
    [
        "-rf",
        "-fr",
        "--hard",
        "--force",
        "--no-preserve-root",
        "--delete",
    ]
    .iter()
    .any(|s| l.contains(s))
}

/// Split a command line into segments on the operators that begin a new
/// command (`;`, newline, `&&`, `||`, `|`, `&`). Coarse but enough to
/// isolate each verb for prefix-stripping.
fn split_shell_segments(cmd: &str) -> Vec<String> {
    // `char_indices()` yields (byte_pos, char) tuples where every
    // `byte_pos` is on a UTF-8 char boundary, so `cmd[pos..]` is
    // always a valid slice. The previous byte-arithmetic version
    // panicked on any multi-byte UTF-8 (em-dash etc.) that LLM-
    // generated commands inadvertently include — see issue #141.
    //
    // Order matters: check the two-char operators `&&` / `||` BEFORE
    // the single-char `&` / `|` branch, else the single-char arm
    // steals the first byte of the pair and `cur` ends up with a
    // stray `&` between two segments.
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut chars = cmd.char_indices().peekable();
    while let Some((pos, c)) = chars.next() {
        if cmd[pos..].starts_with("&&") || cmd[pos..].starts_with("||") {
            segs.push(std::mem::take(&mut cur));
            chars.next(); // consume the second char of the pair
        } else if c == ';' || c == '\n' || c == '|' || c == '&' {
            segs.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    segs.push(cur);
    segs.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// Resolve `cmd` to effective token-lists: per segment, tokenise (resolving
/// quotes/escapes/order), strip `VAR=` + wrapper prefixes, and recurse into
/// the string argument of `eval` / `sh -c` / `bash -c` / `zsh -c` /
/// `dash -c`. Falls back to whitespace tokens when `shell_words` can't parse.
fn effective_commands(cmd: &str) -> Vec<Vec<String>> {
    // Join shell line-continuations (`\<newline>`) first, else the segment
    // splitter would treat the newline as a command separator and break
    // `rm \⏎ -rf …` into two harmless-looking halves.
    let cmd = cmd.replace("\\\n", " ");
    let mut out = Vec::new();
    for seg in split_shell_segments(&cmd) {
        let tokens = shell_words::split(&seg)
            .unwrap_or_else(|_| seg.split_whitespace().map(String::from).collect());
        let mut start = 0;
        while start < tokens.len() {
            let t = &tokens[start];
            let is_assign = t
                .split_once('=')
                .map(|(k, _)| {
                    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
                .unwrap_or(false);
            let is_wrap = WRAPPER_PREFIXES.contains(&t.to_ascii_lowercase().as_str());
            if is_assign || is_wrap {
                start += 1;
            } else {
                break;
            }
        }
        let eff = &tokens[start..];
        let Some(verb) = eff.first().map(|v| v.to_ascii_lowercase()) else {
            continue;
        };
        // Recurse into `<sh> -c "<inner>"` and `eval <inner…>`.
        if matches!(verb.as_str(), "sh" | "bash" | "zsh" | "dash" | "ksh") {
            if let Some(pos) = eff.iter().position(|t| t == "-c") {
                if let Some(inner) = eff.get(pos + 1) {
                    out.extend(effective_commands(inner));
                    continue;
                }
            }
        } else if verb == "eval" {
            out.extend(effective_commands(&eff[1..].join(" ")));
            continue;
        }
        out.push(eff.to_vec());
    }
    out
}

fn lead_blocked_in(padded_lower: &str) -> Option<&'static str> {
    LEAD_BLOCKED
        .iter()
        .find(|(p, _)| padded_lower.contains(p))
        .map(|(_, w)| *w)
}

/// `rm` invoked with a recursive flag anywhere in its args (defeats
/// arg-order swaps like `rm ../teammate -rf`).
fn rm_is_recursive(tokens: &[String]) -> bool {
    if tokens.first().map(|v| v.eq_ignore_ascii_case("rm")) != Some(true) {
        return false;
    }
    tokens[1..].iter().any(|a| {
        let a = a.to_ascii_lowercase();
        (a.starts_with('-') && !a.starts_with("--") && a.contains('r')) || a == "--recursive"
    })
}

/// `git reset --hard <cross-branch-ref>` in a resolved token list.
fn teammate_reset_reason(tokens: &[String]) -> Option<&'static str> {
    let lc: Vec<String> = tokens.iter().map(|t| t.to_ascii_lowercase()).collect();
    let is_git_reset = lc.first().map(|s| s == "git").unwrap_or(false)
        && lc.get(1).map(|s| s == "reset").unwrap_or(false);
    if !is_git_reset {
        return None;
    }
    let hpos = lc.iter().position(|s| s == "--hard")?;
    let reftok = tokens.get(hpos + 1)?;
    ref_resets_to_different_branch(reftok).then_some(TEAMMATE_RESET_REASON)
}

/// Commands a teammate must never run. Catches the cross-branch
/// `git reset --hard <ref>` footgun that has wiped teammate worktrees —
/// even when the verb is shell-quoted (`git rese''t --hard main`) or built
/// via substitution. `git reset --hard HEAD~N` / `<sha>` stay allowed.
pub fn teammate_forbidden_command(cmd: &str) -> Option<&'static str> {
    if !is_teammate_process() {
        return None;
    }
    teammate_block_reason(cmd)
}

/// The teammate seatbelt decision, independent of the process-role flag so
/// it's unit-testable without setting global state. See
/// [`teammate_forbidden_command`].
fn teammate_block_reason(cmd: &str) -> Option<&'static str> {
    // Resolved forms (quote/order/wrapper/-c-defeating) + the raw command.
    for tokens in effective_commands(cmd) {
        if let Some(why) = teammate_reset_reason(&tokens) {
            return Some(why);
        }
    }
    if let Some(why) =
        teammate_reset_reason(&cmd.split_whitespace().map(String::from).collect::<Vec<_>>())
    {
        return Some(why);
    }
    // Substitution we can't resolve, but a literal `--hard <cross-branch>`
    // is still visible — refuse it.
    if looks_obfuscated(cmd) {
        if let Some(after) = cmd.split("--hard").nth(1) {
            if let Some(reftok) = after.split_whitespace().next() {
                if ref_resets_to_different_branch(reftok) {
                    return Some(TEAMMATE_RESET_REASON);
                }
            }
        }
    }
    None
}

/// Commands the team lead must never run. Returns the human-readable reason
/// or None when allowed. Always None for non-lead processes — teammates
/// legitimately use these inside their own worktrees. Shell-aware (#125):
/// resolves quoting/order/wrappers and refuses obfuscated destructive forms.
pub fn lead_forbidden_command(cmd: &str) -> Option<&'static str> {
    if !crate::team::is_team_lead() {
        return None;
    }
    lead_block_reason(cmd)
}

/// The lead seatbelt decision, independent of the process-role flag so it's
/// unit-testable without setting global state. See [`lead_forbidden_command`].
fn lead_block_reason(cmd: &str) -> Option<&'static str> {
    // 1) Unresolvable substitution carrying a destructive flag → refuse.
    if looks_obfuscated(cmd) && has_destructive_signal(cmd) {
        return Some(LEAD_OBFUSCATED);
    }
    // 2) Resolved segments: arg-order-proof `rm` check + the block table
    //    against the canonical (quote/order/wrapper/-c-resolved) form.
    for tokens in effective_commands(cmd) {
        if rm_is_recursive(&tokens) {
            return Some("destructively remove files");
        }
        let canon = format!(" {} ", tokens.join(" ").to_ascii_lowercase());
        if let Some(why) = lead_blocked_in(&canon) {
            return Some(why);
        }
    }
    // 3) Raw substring fallback — never weaker than the pre-hardening guard.
    let collapsed: String = cmd
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    lead_blocked_in(&format!(" {collapsed} "))
}

pub fn is_destructive_command(cmd: &str) -> bool {
    let raw = cmd.to_lowercase();
    // Collapse any run of whitespace (tabs, newlines, multi-space) to a
    // single space AND pad with a space on both ends so patterns that
    // want to match a flag-in-context (e.g. ` -delete`, ` source `) can
    // anchor against the padding without missing commands that happen
    // to start or end with the target token.
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let padded = format!(" {collapsed} ");
    let lower = padded.as_str();

    let simple_patterns = [
        // Filesystem destruction
        "rm -rf",
        "rm -fr",
        "rmdir",
        "rm -r",
        "rm -f ",
        "mv ",
        "truncate",
        "> /",
        "dd if=",
        "mkfs",
        "shred ",
        "wipe ",
        // Permission/ownership sweeps
        "chmod -r",
        "chown -r",
        // Process control
        "kill -9",
        "killall",
        "pkill",
        // Privilege escalation
        "sudo ",
        "doas ",
        // System power state
        "shutdown",
        "reboot",
        "poweroff",
        "halt ",
        "systemctl poweroff",
        "systemctl reboot",
        "systemctl halt",
        // Fork-bomb
        ":(){ :|:& };:",
        // Low-level format
        "format ",
        // Git history + working-tree destruction
        "git reset --hard",
        "git clean -f",
        "git clean -d",
        "git push --force",
        "git push -f ",
        "git push --delete",
        "git branch -d ",
        "git branch -d",
        "git tag -d ",
        "git filter-branch",
        "git filter-repo",
        "git update-ref -d",
        "git checkout -- ",
        "git checkout .",
        "git restore --staged",
        "git restore --worktree",
        "git restore .",
        "git stash drop",
        "git stash clear",
        // Archive / sync that can silently overwrite
        "tar --overwrite",
        "rsync --delete",
        "rsync -a --delete",
        // Filesystem search-and-destroy — match the flag with a
        // leading space so it catches `find ... -delete` regardless of
        // trailing args, without being triggered by the literal string
        // `-delete` appearing mid-word.
        " -delete",
        " -exec rm",
        // Low-level removal
        "unlink ",
        "fallocate -p",
        // Piped script execution (dot-source, `source`, process sub)
        " . ./",
        " . /",
        " source ",
        "| bash",
        "|bash",
        "| zsh",
        "|zsh",
        "| python",
        "|python",
        "| perl",
        "|perl",
        "| ruby",
        "|ruby",
        " bash <(",
        " zsh <(",
        " sh <(",
        " python <(",
        // Windows destructive (matched post-lowercase)
        "del /f",
        "del /s",
        "del /q",
        "rd /s",
        "rd /q",
        "cipher /w",
        // Container / orchestrator destruction
        "docker rm -f",
        "docker rmi -f",
        "docker system prune",
        "docker volume rm",
        "docker network rm",
        "podman rm -f",
        "podman system prune",
        "kubectl delete",
        "helm uninstall",
        "helm delete",
        "terraform destroy",
        // Cloud CLIs
        "aws s3 rb",
        "aws s3 rm",
        "aws ec2 terminate-instances",
        "aws rds delete",
        "gcloud compute instances delete",
        "gcloud projects delete",
        "az group delete",
        // SQL (very coarse — only blocks the obvious DDL/DML)
        "drop database",
        "drop table",
        "truncate table",
        "delete from ",
        // Package-manager wipes
        "apt-get remove",
        "apt remove",
        "yum remove",
        "dnf remove",
        "brew uninstall",
        "npm uninstall -g",
        "pnpm remove -g",
        "pip uninstall -y",
        "cargo uninstall",
        // Filesystem snapshot
        "zfs destroy",
        "btrfs subvolume delete",
    ];
    if simple_patterns.iter().any(|p| lower.contains(p)) {
        return true;
    }

    // Detect piping download commands into a shell: curl ... | sh, wget ... | bash
    if lower.contains("| sh")
        || lower.contains("|sh")
        || lower.contains("| bash")
        || lower.contains("|bash")
        || lower.contains("| zsh")
        || lower.contains("|zsh")
    {
        if lower.contains("curl") || lower.contains("wget") || lower.contains("fetch ") {
            return true;
        }
    }

    false
}

/// Detect commands that start long-running server processes.
///
/// Token-aware (M6.8): walks past package-manager / runner prefixes
/// (`npx`, `pnpm exec`, `yarn exec`, `bun x`, `python -m`) to the real
/// leaf command, then checks whether the leaf + sub-command names a
/// known server. Bias is toward FALSE on ambiguous cases — false
/// positives silently corrupt output (we drop stdout/stderr and tell
/// the model "Server started" when the command actually scaffolded
/// files, as `npx vite init` did in the test session at
/// sess-18ab8129d6eafbd8.jsonl). False negatives just hit the regular
/// timeout with a clear error.
pub fn is_server_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    // Only match if NOT already backgrounded.
    if lower.trim().ends_with('&') {
        return false;
    }

    // Look at the LAST segment of an `&&` or `;` chain — earlier
    // segments are typically `cd X` / `mkdir -p Y` / dependency
    // installs that exit. Only the last command can be persistent.
    let last_chain = lower.rsplit("&&").next().unwrap_or(&lower).trim();
    let last = last_chain.rsplit(';').next().unwrap_or(last_chain).trim();

    let tokens: Vec<&str> = last.split_whitespace().collect();
    if tokens.is_empty() {
        return false;
    }

    let leaf_idx = find_leaf_command(&tokens);
    let leaf_raw = tokens.get(leaf_idx).copied().unwrap_or("");
    // Strip npm-style version suffix from the leaf so `vite@latest` /
    // `next@14` / `eslint@^8` resolve to the bare command name.
    let leaf = leaf_raw.split('@').next().unwrap_or(leaf_raw);
    let sub = tokens.get(leaf_idx + 1).copied().unwrap_or("");
    let third = tokens.get(leaf_idx + 2).copied().unwrap_or("");

    classify_leaf_as_server(leaf, sub, third)
}

/// Walk past package-manager / runner prefixes to find the actual
/// leaf command. Returns the index of the leaf in the tokens slice.
///
/// Examples (returns index of marked token):
///   `npx vite dev`              → 1 (`vite`)
///   `bun x vite dev`            → 2 (`vite`)
///   `pnpm exec vite build`      → 2 (`vite`)
///   `pnpm dlx create-vite`      → 2 (`create-vite`)
///   `yarn exec vite preview`    → 2 (`vite`)
///   `python -m http.server`     → 2 (`http.server`)
///   Otherwise                   → 0 (no walk past)
fn find_leaf_command(tokens: &[&str]) -> usize {
    if tokens.is_empty() {
        return 0;
    }
    let first = tokens[0];

    // npx <cmd>
    if first == "npx" || first == "bunx" {
        return 1.min(tokens.len().saturating_sub(1));
    }

    // bun x <cmd>
    if first == "bun" && tokens.get(1) == Some(&"x") {
        return 2.min(tokens.len().saturating_sub(1));
    }

    // pnpm exec / pnpm dlx / yarn exec
    if (first == "pnpm" && matches!(tokens.get(1), Some(&"exec") | Some(&"dlx")))
        || (first == "yarn" && tokens.get(1) == Some(&"exec"))
    {
        return 2.min(tokens.len().saturating_sub(1));
    }

    // python -m <module>
    if (first == "python" || first == "python3") && tokens.get(1) == Some(&"-m") {
        return 2.min(tokens.len().saturating_sub(1));
    }

    0
}

/// Classify a resolved leaf command (after walking past runners) plus
/// its first two sub-args as either a server or not.
///
/// This is the table that replaces the prior loose `lower.contains()`
/// pattern list. Each entry names the server *mode* explicitly so
/// scaffolders / build commands using the same binary (e.g.
/// `vite init` / `vite build` / `webpack --watch`) don't false-positive.
fn classify_leaf_as_server(leaf: &str, sub: &str, third: &str) -> bool {
    match leaf {
        // Direct, unambiguous server programs (no sub-arg required).
        "uvicorn" | "gunicorn" | "hypercorn" | "ngrok" | "live-server" | "http-server" => true,

        // Frontend frameworks: sub-command discriminates. Bare `vite`
        // (with no sub) defaults to dev — server. `vite build` / `vite
        // init` / `vite optimize` etc. are not servers.
        "vite" | "next" | "nuxt" | "remix" | "astro" => {
            matches!(sub, "" | "dev" | "preview" | "start" | "serve" | "watch")
        }

        // webpack: only `webpack serve` is a server.
        "webpack" => sub == "serve",

        // Package managers: sub-command names the script.
        "npm" => match sub {
            "start" => true,
            "run" => matches!(third, "dev" | "start" | "serve" | "watch" | "preview"),
            _ => false,
        },
        // yarn / pnpm / bun: bare `pnpm dev` and `pnpm run dev` are
        // both legal forms. Match either shape — directly via the
        // sub-arg, OR via `run <script>` where script is a server
        // mode.
        "yarn" | "pnpm" | "bun" => {
            matches!(sub, "dev" | "start" | "serve" | "watch" | "preview")
                || (sub == "run"
                    && matches!(third, "dev" | "start" | "serve" | "watch" | "preview"))
        }

        // Python web frameworks
        "flask" => sub == "run",
        "django-admin" => sub == "runserver",
        "python" | "python3" => {
            matches!(
                sub,
                "app.py" | "main.py" | "server.py" | "run.py" | "wsgi.py" | "asgi.py"
            ) || (sub == "manage.py" && third == "runserver") // Django dev server
        }
        // After `python -m`, the leaf becomes the module name.
        "http.server" => true,

        // Ruby
        "rails" => matches!(sub, "server" | "s"),
        "ruby" => sub == "server",

        // PHP
        "php" => sub == "-s" || (sub == "artisan" && third == "serve"),

        // Go
        "go" => sub == "run",

        // Docker
        "docker" => {
            (sub == "compose" && third == "up")
                || (sub == "logs" && matches!(third, "-f" | "--follow"))
        }
        "docker-compose" => sub == "up" || (sub == "logs" && matches!(third, "-f" | "--follow")),

        // Kubernetes
        "kubectl" => sub == "port-forward" || (sub == "logs" && matches!(third, "-f" | "--follow")),
        "cloudflared" => sub == "tunnel",

        // Long-blocking utilities that never return on their own — these
        // would otherwise foreground-hang for the full Bash timeout.
        "tail" => matches!(sub, "-f" | "-F" | "--follow"),
        "watch" => true,
        "nc" | "ncat" | "netcat" => sub == "-l" || sub.starts_with("-l"),
        "ssh" => {
            matches!(sub, "-N" | "-L" | "-R" | "-D" | "-W")
                || matches!(third, "-N" | "-L" | "-R" | "-D" | "-W")
        }

        // `serve <dir>` — the `serve` npm package serves static files.
        // Only treat as server when there's a path argument.
        "serve" => !sub.is_empty(),

        // Bare `cargo run` is often a web server. With `--bin <name>`
        // / `--example <name>` it could be either; bias toward false
        // so output isn't silently swallowed.
        "cargo" => sub == "run" && (third.is_empty() || third.starts_with("--release")),

        // Direct node invocations — only canonical server filenames.
        "node" => matches!(
            sub,
            "server" | "server.js" | "index.js" | "app.js" | "start"
        ),

        _ => false,
    }
}

fn format_output(stdout: &str, stderr: &str, exit_code: i32) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !stdout.is_empty() {
        parts.push(stdout.trim_end_matches('\n').to_string());
    }
    if !stderr.is_empty() {
        parts.push(format!("[stderr]\n{}", stderr.trim_end_matches('\n')));
    }
    if exit_code != 0 {
        parts.push(format!("[exit code {exit_code}]"));
    }
    let body = if parts.is_empty() {
        String::new()
    } else {
        parts.join("\n")
    };

    // M6.8 B2: prepend a hint when the output looks like the command
    // failed because it required an interactive TTY. The sandbox
    // spawns with stdin = /dev/null, so any prompt that tries to
    // read stdin gets EOF and the CLI typically prints "Operation
    // cancelled" / "Aborted" / similar and exits non-zero. Without
    // this hint the model retries the same command verbatim (the
    // test session showed `pnpm create vite` retried twice). With
    // the hint the model has enough signal to switch to a
    // non-interactive variant or different scaffolder.
    if exit_code != 0 && looks_like_tty_required(stdout, stderr) {
        let hint = "[hint: this command appears to require an interactive TTY \
                    — the sandbox runs with stdin=/dev/null. Try non-interactive \
                    flags (e.g. --yes, --no-input, --skip-prompts) or a different \
                    scaffolder. Common: `pnpm create vite <dir> --template \
                    react-ts` and similar `create-*` CLIs need a target dir + \
                    template flag, not a current-dir invocation.]\n";
        format!("{hint}{body}")
    } else {
        body
    }
}

/// Detect output patterns that suggest the command failed because it
/// required interactive stdin. Used by `format_output` to prepend a
/// helpful hint instead of leaving the model staring at a cryptic
/// "Operation cancelled" line.
fn looks_like_tty_required(stdout: &str, stderr: &str) -> bool {
    let combined = format!("{stdout}\n{stderr}").to_lowercase();
    // Common error fragments emitted by interactive CLIs when stdin
    // is closed mid-prompt. Conservative — only fire on phrases that
    // unambiguously mean "I needed input and didn't get it."
    const FRAGMENTS: &[&str] = &[
        "operation cancelled",
        "operation canceled", // US spelling
        "operation aborted",
        "user aborted",
        "input is required",
        "tty is not available",
        "no tty",
        "stdin is not a tty",
        "interactive mode is not supported",
        "cannot prompt",
        "would prompt for",
        "no input available",
    ];
    FRAGMENTS.iter().any(|f| combined.contains(f))
}

/// Apply non-interactive environment variables to a child command.
/// Most modern CLIs honour at least one of these to skip prompts and
/// auto-accept defaults. M6.8 B1 — workaround for the lack of a real
/// PTY in the Bash sandbox.
/// Keep platform credentials out of the shell's environment so a
/// `printenv` / `cat /proc/self/environ` can't exfiltrate them. Platform
/// internals (the raw gateway env, the multiuser HMAC secret, the cloud
/// token) are removed up front; provider API keys are removed only in a
/// multiuser/shared session, where the shell belongs to a guest who must
/// not read the owner's billable credentials. On a single-user session the
/// user's own keys stay available — and the *resolved* gateway credential
/// (env → keychain → cloud-token, same chain the providers use) is
/// re-injected as `THCLAWS_GATEWAY_{API_KEY,BASE_URL}` so agent scripts can
/// reach Gemini image/TTS through the metered gateway with zero .env setup.
fn scrub_sensitive_env(cmd: &mut tokio::process::Command) {
    const ALWAYS: &[&str] = &[
        "THCLAWS_CLOUD_HMAC_SECRET",
        "THCLAWS_GATEWAY_API_KEY",
        "THCLAWS_CLOUD_TOKEN",
    ];
    for k in ALWAYS {
        cmd.env_remove(k);
    }
    if crate::workdir::is_multiuser() {
        const SCOPED: &[&str] = &[
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "OPENROUTER_API_KEY",
            "DASHSCOPE_API_KEY",
            "QWENCLOUD_API_KEY",
            "ZAI_API_KEY",
            "DEEPSEEK_API_KEY",
            "MINIMAX_API_KEY",
            "THAILLM_API_KEY",
            "XAI_API_KEY",
            "MOONSHOT_API_KEY",
            "BRAVE_SEARCH_API_KEY",
            "BRAVE_API_KEY",
            "TAVILY_API_KEY",
            "HAL_API_KEY",
        ];
        for k in SCOPED {
            cmd.env_remove(k);
        }
        return;
    }
    // Single-user: re-inject the RESOLVED gateway credential after the scrub.
    // The blanket removal above protects multiuser guests, but on a
    // single-user session the gateway key bills the user themself — and
    // agent scripts (speech-generator, training-video-generator,
    // tutorial-studio) that call Gemini image/TTS have no other keyless
    // path: the sandbox blocks ~/.config + the keychain, so "I use the
    // gateway" users hit `no credential` from every subprocess. Resolving
    // via the same env→keychain→cloud-token chain the providers use means
    // a gateway-configured desktop just works.
    if let Some(key) = crate::providers::thclaws_gateway::resolve_access_key() {
        let base = std::env::var("THCLAWS_GATEWAY_BASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| crate::providers::thclaws_gateway::GATEWAY_BASE_URL.to_string());
        cmd.env("THCLAWS_GATEWAY_API_KEY", key);
        cmd.env("THCLAWS_GATEWAY_BASE_URL", base);
    }
}

fn apply_noninteractive_env(cmd: &mut tokio::process::Command) {
    // CI=1 is the most-respected signal. npm, pnpm, yarn, vite, jest,
    // ESLint, Prettier, Cypress, etc. all use it.
    cmd.env("CI", "1");
    // Some tools key on this stronger Yarn/Berry-style flag.
    cmd.env("CI_JOB_ID", "thclaws-sandbox");
    // npm's "auto-yes" for confirmation prompts.
    cmd.env("NPM_CONFIG_YES", "true");
    // pnpm honours its own confirm setting.
    cmd.env("PNPM_CONFIRM", "no");
    // apt / debconf — relevant when the model sudo-installs a package.
    cmd.env("DEBIAN_FRONTEND", "noninteractive");
    // Broad TTY signal — many tools fall back to non-interactive
    // behaviour when TERM=dumb (no curses, no progress bars, no
    // interactive prompts).
    cmd.env("TERM", "dumb");
    // homebrew honours this on macOS for its install/upgrade prompts.
    cmd.env("HOMEBREW_NO_AUTO_UPDATE", "1");
    cmd.env("HOMEBREW_NO_INSTALL_CLEANUP", "1");
    // pip's "yes to everything" + suppress interactive upgrade pitch.
    cmd.env("PIP_YES", "1");
    cmd.env("PIP_DISABLE_PIP_VERSION_CHECK", "1");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_removes_platform_secrets_keeps_others() {
        let mut cmd = crate::util::shell_command_async("true");
        cmd.env("THCLAWS_GATEWAY_API_KEY", "spend-me")
            .env("THCLAWS_CLOUD_HMAC_SECRET", "forge-me")
            .env("KEEP_ME", "1");
        scrub_sensitive_env(&mut cmd);
        let std = cmd.as_std();
        let removed = |key: &str| {
            std.get_envs()
                .any(|(k, v)| k == std::ffi::OsStr::new(key) && v.is_none())
        };
        assert!(
            removed("THCLAWS_GATEWAY_API_KEY"),
            "gateway key must be scrubbed"
        );
        assert!(
            removed("THCLAWS_CLOUD_HMAC_SECRET"),
            "hmac secret must be scrubbed"
        );
        assert!(
            std.get_envs()
                .any(|(k, v)| k == std::ffi::OsStr::new("KEEP_ME")
                    && v == Some(std::ffi::OsStr::new("1"))),
            "non-secret env must be preserved"
        );
    }
    use tempfile::tempdir;

    #[test]
    fn split_shell_segments_handles_multibyte_utf8() {
        // Regression for issue #141: the old byte-arithmetic version
        // panicked here because `i += 1` walks into the middle of the
        // 3-byte em-dash (U+2014, E2 80 94).
        let cmd = "echo hello — world; ls";
        let segs = split_shell_segments(cmd);
        assert_eq!(
            segs,
            vec!["echo hello — world".to_string(), " ls".to_string()]
        );

        // And a few more exotic Unicode points to be sure: a 4-byte
        // emoji (😀, U+1F600, F0 9F 98 80) right next to an operator.
        let cmd = "echo 😀 | grep .";
        let segs = split_shell_segments(cmd);
        assert_eq!(segs, vec!["echo 😀 ".to_string(), " grep .".to_string()]);

        // Mixed: Thai script (3-byte each) split by `&&`.
        let cmd = "echo สวัสดี && echo world";
        let segs = split_shell_segments(cmd);
        assert_eq!(
            segs,
            vec!["echo สวัสดี ".to_string(), " echo world".to_string()]
        );
    }

    #[test]
    fn split_shell_segments_distinguishes_double_and_single_operators() {
        // && and || vs single & and | — make sure the two-char check
        // wins so neither operator gets corrupted.
        assert_eq!(
            split_shell_segments("a && b"),
            vec!["a ".to_string(), " b".to_string()]
        );
        assert_eq!(
            split_shell_segments("a || b"),
            vec!["a ".to_string(), " b".to_string()]
        );
        assert_eq!(
            split_shell_segments("a & b"),
            vec!["a ".to_string(), " b".to_string()]
        );
        assert_eq!(
            split_shell_segments("a | b"),
            vec!["a ".to_string(), " b".to_string()]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn echoes_stdout() {
        let out = BashTool::default()
            .call(json!({"command": "echo hello-bash"}))
            .await
            .unwrap();
        assert_eq!(out, "hello-bash");
    }

    #[test]
    fn destructive_command_detection() {
        assert!(is_destructive_command("rm -rf /tmp/foo"));
        assert!(is_destructive_command("sudo apt install"));
        assert!(is_destructive_command("curl http://x | sh"));
        assert!(is_destructive_command("mv file1 file2"));
        assert!(!is_destructive_command("ls -la"));
        assert!(!is_destructive_command("echo hello"));
        assert!(!is_destructive_command("git status"));
        assert!(!is_destructive_command("cargo test"));
    }

    /// Teammates can recover from their own mistakes on their own branch
    /// (HEAD~N, sha) but must not reset to a different branch — that's
    /// the pattern that wiped frontend's worktree.
    #[test]
    fn teammate_forbidden_command_blocks_cross_branch_reset() {
        // Force teammate-mode by setting the env var. SAFETY: tests share
        // the process env, so set + restore around the assertions.
        std::env::set_var("THCLAWS_TEAM_AGENT", "frontend");

        // Cross-branch / remote-ref / sibling-branch resets — block.
        assert!(teammate_forbidden_command("git reset --hard main").is_some());
        assert!(teammate_forbidden_command("git reset --hard master").is_some());
        assert!(teammate_forbidden_command("git reset --hard origin/main").is_some());
        assert!(teammate_forbidden_command("git reset --hard team/backend").is_some());
        assert!(teammate_forbidden_command("git reset --hard dev").is_some());
        assert!(teammate_forbidden_command("git reset --hard feature-x").is_some());

        // Same-branch recovery — allowed.
        assert!(teammate_forbidden_command("git reset --hard HEAD").is_none());
        assert!(teammate_forbidden_command("git reset --hard HEAD~1").is_none());
        assert!(teammate_forbidden_command("git reset --hard HEAD~3").is_none());
        assert!(teammate_forbidden_command("git reset --hard HEAD^").is_none());
        assert!(teammate_forbidden_command("git reset --hard HEAD@{2}").is_none());
        assert!(teammate_forbidden_command("git reset --hard a11930a").is_none());
        assert!(teammate_forbidden_command("git reset --hard a11930af0e9c").is_none());

        // Tags — allowed.
        assert!(teammate_forbidden_command("git reset --hard tags/v1.0").is_none());

        // Other commands — allowed (they're for the destructive-warning
        // layer, not this one).
        assert!(teammate_forbidden_command("git status").is_none());
        assert!(teammate_forbidden_command("rm -rf node_modules").is_none());

        std::env::remove_var("THCLAWS_TEAM_AGENT");

        // When NOT a teammate, every command passes — the lead and
        // standalone sessions don't have this restriction (they have
        // their own guards or none).
        assert!(teammate_forbidden_command("git reset --hard main").is_none());
    }

    #[test]
    fn lead_forbidden_command_behavior() {
        // Tests share the AtomicBool, so toggle explicitly in this test
        // and never rely on default state. All assertions about "off"
        // run first and "on" later in the same test, then restore off.
        crate::team::set_is_team_lead(false);
        assert!(lead_forbidden_command("git reset --hard HEAD").is_none());
        assert!(lead_forbidden_command("rm -rf /tmp/anything").is_none());
        assert!(lead_forbidden_command("git worktree remove foo").is_none());
        assert!(lead_forbidden_command("ls").is_none());

        crate::team::set_is_team_lead(true);
        // Every command that historically cascade-killed a team run should
        // now return Some(reason) so BashTool can refuse it.
        assert!(lead_forbidden_command("git reset --hard d9199ba").is_some());
        assert!(lead_forbidden_command("git clean -fd").is_some());
        assert!(lead_forbidden_command("git push --force").is_some());
        assert!(lead_forbidden_command("git worktree remove .worktrees/backend").is_some());
        assert!(lead_forbidden_command("git worktree prune").is_some());
        assert!(lead_forbidden_command("git checkout -- src/foo.ts").is_some());
        assert!(lead_forbidden_command("git checkout .").is_some());
        assert!(lead_forbidden_command("git restore --worktree src/").is_some());
        assert!(lead_forbidden_command("git merge --abort").is_some());
        assert!(lead_forbidden_command("rm -rf docs/").is_some());
        assert!(lead_forbidden_command("rm -fr docs/").is_some());
        assert!(lead_forbidden_command("rm -r src/old").is_some());
        // Non-mutating git commands the lead legitimately uses stay open.
        assert!(lead_forbidden_command("git status").is_none());
        assert!(lead_forbidden_command("git log --oneline").is_none());
        assert!(lead_forbidden_command("git diff main..team/backend").is_none());
        assert!(lead_forbidden_command("git branch -v").is_none());

        // Restore default so other tests that share this static aren't
        // surprised by lingering lead-mode behavior.
        crate::team::set_is_team_lead(false);
    }

    #[test]
    fn lead_seatbelt_resists_shell_quoting() {
        // thClaws#125: every PoC bypass that slipped past the old substring
        // guard must now be blocked. Uses the gate-free decision helper so
        // it doesn't depend on (or perturb) global team-lead state.
        let blocked: &[&str] = &[
            // controls
            "rm -rf ../teammate",
            "git reset --hard main",
            // quote-splice / quoted verb / arg-order / line-continuation
            "r''m -rf ../teammate",
            "git rese''t --hard main",
            "git \"reset\" --hard main",
            "rm ../teammate -rf",
            "rm \\\n  -rf ../teammate",
            // substitution / expansion families
            "r=rm; $r -rf ../teammate",
            "${r:-rm} -rf ../teammate",
            "$(printf rm) -rf ../teammate",
            "`printf rm` -rf ../teammate",
            "{rm,-rf,../teammate}",
            "IFS=X; rm${IFS}-rf${IFS}../teammate",
            "eval $'\\x72\\x6d -rf ../teammate'",
            // sh -c / bash -c indirection
            "sh -c 'rm -rf ../teammate'",
            "bash -c \"git reset --hard main\"",
            // wrapper prefixes
            "sudo rm -rf /tmp/x",
            "env FOO=bar rm -rf docs/",
        ];
        for cmd in blocked {
            assert!(
                lead_block_reason(cmd).is_some(),
                "lead seatbelt should block: {cmd}"
            );
        }

        // Must NOT over-block legitimate work (including obfuscated-but-
        // harmless commands with no destructive signal).
        let allowed: &[&str] = &[
            "git status",
            "git log --oneline -5",
            "cargo build --release",
            "echo $(date)",
            "git diff $(git merge-base main HEAD)",
            "rm notes.txt",
            "npm run build",
        ];
        for cmd in allowed {
            assert!(
                lead_block_reason(cmd).is_none(),
                "lead seatbelt should allow: {cmd}"
            );
        }
    }

    #[test]
    fn teammate_seatbelt_resists_quoting() {
        // Cross-branch hard reset, blocked even when the verb is quoted or
        // built via substitution.
        assert!(teammate_block_reason("git reset --hard main").is_some());
        assert!(teammate_block_reason("git rese''t --hard main").is_some());
        assert!(teammate_block_reason("git \"reset\" --hard origin/main").is_some());
        assert!(teammate_block_reason("sh -c 'git reset --hard main'").is_some());
        assert!(teammate_block_reason("git $(printf reset) --hard main").is_some());
        // Same-branch recovery stays allowed.
        assert!(teammate_block_reason("git reset --hard HEAD~2").is_none());
        assert!(teammate_block_reason("git reset --hard d9199ba").is_none());
    }

    #[test]
    fn destructive_whitespace_normalisation() {
        // Double-space shouldn't smuggle rm -rf past the classifier.
        assert!(is_destructive_command("rm  -rf /tmp/foo"));
        // Tab-separated likewise.
        assert!(is_destructive_command("rm\t-rf /tmp/foo"));
        // Leading whitespace, multiple spaces between args.
        assert!(is_destructive_command("   rm   -rf    /tmp/foo"));
    }

    #[test]
    fn destructive_piped_interpreters_and_script_sourcing() {
        assert!(is_destructive_command("curl http://x | bash"));
        assert!(is_destructive_command("curl http://x | python"));
        assert!(is_destructive_command("curl http://x | perl"));
        assert!(is_destructive_command("curl http://x | ruby"));
        assert!(is_destructive_command("bash <(curl http://x)"));
        assert!(is_destructive_command("python <(curl http://x)"));
        assert!(is_destructive_command("cat script.sh | bash"));
        assert!(is_destructive_command("source ./install.sh"));
        assert!(is_destructive_command("cd /tmp && . ./boot.sh"));
    }

    #[test]
    fn destructive_find_and_archive() {
        assert!(is_destructive_command("find /tmp -name '*.tmp' -delete"));
        assert!(is_destructive_command("find /tmp -exec rm {} +"));
        assert!(is_destructive_command("rsync -a --delete src/ dst/"));
        assert!(is_destructive_command("tar xf archive.tar --overwrite"));
        assert!(is_destructive_command("unlink /tmp/stale.lock"));
    }

    #[test]
    fn destructive_git_working_tree() {
        assert!(is_destructive_command("git checkout -- src/main.rs"));
        assert!(is_destructive_command("git checkout ."));
        assert!(is_destructive_command("git restore --staged ."));
        assert!(is_destructive_command("git restore --worktree ."));
        assert!(is_destructive_command("git stash drop"));
        assert!(is_destructive_command("git stash clear"));
    }

    #[test]
    fn destructive_windows_equivalents() {
        assert!(is_destructive_command("del /f /s /q C:\\temp"));
        assert!(is_destructive_command("rd /s /q C:\\build"));
        assert!(is_destructive_command("cipher /w:C:"));
    }

    #[test]
    fn destructive_expanded_patterns() {
        // Git history destruction
        assert!(is_destructive_command("git reset --hard HEAD~3"));
        assert!(is_destructive_command("git clean -fd"));
        assert!(is_destructive_command("git push --force origin main"));
        assert!(is_destructive_command(
            "git filter-branch --index-filter ..."
        ));
        // Container / orchestrator
        assert!(is_destructive_command("docker rm -f mycontainer"));
        assert!(is_destructive_command("docker system prune -a"));
        assert!(is_destructive_command("kubectl delete ns production"));
        assert!(is_destructive_command("helm uninstall release"));
        assert!(is_destructive_command("terraform destroy -auto-approve"));
        // Cloud
        assert!(is_destructive_command("aws s3 rb s3://bucket --force"));
        assert!(is_destructive_command("gcloud projects delete my-proj"));
        assert!(is_destructive_command("az group delete --name rg1"));
        // SQL DDL
        assert!(is_destructive_command("psql -c 'DROP TABLE users'"));
        assert!(is_destructive_command("mysql -e 'truncate table logs'"));
        // Shutdown / reboot
        assert!(is_destructive_command("sudo shutdown -h now"));
        assert!(is_destructive_command("systemctl reboot"));
        // Data shredding
        assert!(is_destructive_command("shred -uz secret.txt"));
        // Curl-to-shell variants
        assert!(is_destructive_command(
            "curl https://x.test/install.sh | zsh"
        ));
        // Negatives
        assert!(!is_destructive_command("git log --oneline"));
        assert!(!is_destructive_command("kubectl get pods"));
        assert!(!is_destructive_command("docker ps"));
        assert!(!is_destructive_command("select * from users"));
        assert!(!is_destructive_command("aws s3 ls"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn captures_stderr() {
        let out = BashTool::default()
            .call(json!({"command": "echo oops >&2"}))
            .await
            .unwrap();
        assert!(out.contains("[stderr]"));
        assert!(out.contains("oops"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_appended_to_output() {
        let out = BashTool::default()
            .call(json!({"command": "echo done; exit 3"}))
            .await
            .unwrap();
        assert!(out.contains("done"));
        assert!(out.contains("[exit code 3]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdout_and_stderr_both_captured() {
        let out = BashTool::default()
            .call(json!({"command": "echo out; echo err >&2"}))
            .await
            .unwrap();
        assert!(out.contains("out"));
        assert!(out.contains("err"));
        assert!(out.contains("[stderr]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn honors_cwd_argument() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "").unwrap();
        let out = BashTool::default()
            .call(json!({
                "command": "ls",
                "cwd": dir.path().to_string_lossy(),
            }))
            .await
            .unwrap();
        assert!(out.contains("marker.txt"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_long_running_commands() {
        let out = BashTool::default()
            .call(json!({
                "command": "sleep 5",
                "timeout": 1000,
            }))
            .await;
        match out {
            Err(e) => {
                let s = format!("{e}");
                assert!(s.contains("timeout"), "expected timeout error, got: {s}");
            }
            Ok(out) => panic!("expected timeout error, got Ok: {out}"),
        }
    }

    // issue #182: a mid-call session cancel must kill the child promptly,
    // not wait out `timeout_ms`. Repro: a 30s sleep under a 60s timeout,
    // cancelled after 200ms — should error in well under a second.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_kills_in_flight_bash() {
        let cancel = crate::cancel::CancelToken::new();
        let bash = BashTool::with_cancel(cancel.clone());
        let firing = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            firing.cancel();
        });
        let start = std::time::Instant::now();
        let out = bash
            .call(json!({ "command": "sleep 30 && echo woke", "timeout": 60000 }))
            .await;
        let elapsed = start.elapsed();
        match out {
            Err(e) => assert!(
                format!("{e}").contains("cancelled"),
                "expected a cancelled error, got: {e}"
            ),
            Ok(o) => panic!("expected cancellation, got Ok: {o}"),
        }
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "cancel should kill promptly; took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_secs_legacy_alias_works() {
        let out = BashTool::default()
            .call(json!({
                "command": "sleep 5",
                "timeout_secs": 1,
            }))
            .await;
        match out {
            Err(e) => {
                let s = format!("{e}");
                assert!(s.contains("timeout"), "expected timeout error, got: {s}");
            }
            Ok(out) => panic!("expected timeout error, got Ok: {out}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_command_errors() {
        let err = BashTool::default().call(json!({})).await.unwrap_err();
        assert!(format!("{err}").contains("command"));
    }

    #[test]
    fn bash_requires_approval() {
        let bash = BashTool::default();
        assert!(bash.requires_approval(&json!({"command": "ls"})));
    }

    #[test]
    fn format_output_combines_parts() {
        assert_eq!(format_output("hello\n", "", 0), "hello");
        assert_eq!(
            format_output("", "oops\n", 1),
            "[stderr]\noops\n[exit code 1]"
        );
        assert_eq!(format_output("", "", 0), "");
    }

    #[test]
    fn needs_venv_detects_pip_and_server_tools() {
        // v0.35.4 tightened needs_venv: only pip/poetry/uv installs +
        // long-running server runners + pytest. Plain `python script.py`
        // does NOT trigger venv auto-wrap (was producing spurious
        // `[creating .venv]` warnings around agent-shipped stdlib
        // scripts; the agent then mis-attributed them to the script).
        assert!(needs_venv("pip install fastapi"));
        assert!(needs_venv("pip3 install uvicorn"));
        assert!(needs_venv("uvicorn main:app --port 8000"));
        assert!(needs_venv("gunicorn app:app"));
        assert!(needs_venv("pytest tests/"));
        assert!(needs_venv("flask run"));
        assert!(needs_venv("poetry install"));
        assert!(needs_venv("uv pip install requests"));
        // Plain python invocations — explicitly NOT venv-wrapped.
        assert!(!needs_venv("python app.py"));
        assert!(!needs_venv("python3 main.py"));
        assert!(!needs_venv("python3 .thclaws/scripts/batch.py"));
        assert!(!needs_venv("echo hello"));
        assert!(!needs_venv("cargo build"));
        assert!(!needs_venv("npm install express"));
    }

    #[test]
    fn server_detection_python_entry_points() {
        assert!(is_server_command("python app.py"));
        assert!(is_server_command("python3 app.py"));
        assert!(is_server_command("python main.py"));
        assert!(is_server_command("python server.py"));
        assert!(is_server_command("python run.py"));
        assert!(is_server_command("python -m uvicorn app:main"));
        assert!(is_server_command("python3 -m flask run"));
        // Not a known server entry point.
        assert!(!is_server_command("python test_app.py"));
        assert!(!is_server_command("python setup.py install"));
        // Already backgrounded.
        assert!(!is_server_command("python app.py &"));
    }

    // ── M6.8 Bug A: classifier false-positive narrowing ────────────────

    #[test]
    fn is_server_distinguishes_vite_dev_from_vite_init() {
        // The exact false-positive that broke the test session at
        // sess-18ab8129d6eafbd8.jsonl: `npx vite init` was flagged as
        // a server because the old classifier matched the bare
        // substring "vite". Now the classifier walks past `npx` to
        // the leaf `vite` and checks the sub-command.
        assert!(
            !is_server_command("npx vite@latest init --template react-ts"),
            "vite init is a synchronous scaffolder, not a server",
        );
        assert!(
            !is_server_command("npx vite@latest init . --template react-ts --force"),
            "even with --force, init scaffolds and exits",
        );
        assert!(
            !is_server_command("pnpm exec vite build"),
            "vite build is not a server",
        );
        assert!(
            !is_server_command("vite optimize"),
            "vite optimize is not a server",
        );

        // Real server invocations still match.
        assert!(is_server_command("vite"), "bare vite defaults to dev");
        assert!(is_server_command("vite dev"));
        assert!(is_server_command("vite preview"));
        assert!(is_server_command("npx vite@latest dev --port 3000"));
        assert!(is_server_command("pnpm exec vite preview"));
    }

    #[test]
    fn is_server_walks_past_runner_prefixes() {
        // Each of these resolves through find_leaf_command to the
        // real leaf: only the leaf + sub-command should drive the
        // classification. `npx <build-tool> build` must return false.
        assert!(!is_server_command("npx webpack --mode production"));
        assert!(!is_server_command(
            "pnpm dlx create-vite my-app --template react"
        ));
        assert!(!is_server_command("bun x next build"));
        assert!(!is_server_command("yarn exec next build"));

        // …but the dev-mode equivalents pass through correctly.
        assert!(is_server_command("npx webpack serve"));
        assert!(is_server_command("bun x next dev"));
        assert!(is_server_command("yarn exec next dev"));
    }

    #[test]
    fn is_server_npm_run_only_for_dev_scripts() {
        // `npm run` is a dispatcher — only some scripts are servers.
        assert!(is_server_command("npm run dev"));
        assert!(is_server_command("npm run start"));
        assert!(is_server_command("npm run serve"));
        assert!(is_server_command("npm run watch"));
        assert!(is_server_command("npm run preview"));
        assert!(is_server_command("npm start"));

        // `npm run build` / `test` / `lint` are NOT servers.
        assert!(!is_server_command("npm run build"));
        assert!(!is_server_command("npm run test"));
        assert!(!is_server_command("npm run lint"));
        assert!(!is_server_command("npm run typecheck"));
        assert!(!is_server_command("npm install"));
        assert!(!is_server_command("npm test"));
    }

    #[test]
    fn is_server_uses_last_chained_segment() {
        // Setup commands chained via && should be ignored — only the
        // final segment can be persistent.
        assert!(is_server_command("cd app && pnpm install && pnpm run dev"));
        assert!(!is_server_command(
            "cd app && pnpm install && pnpm run build"
        ));
        // Last segment is the scaffolder, not a server.
        assert!(!is_server_command(
            "mkdir -p webapp && cd webapp && pnpm create vite . --template react-ts"
        ));
    }

    #[test]
    fn is_server_cargo_run_with_explicit_bin_is_not_classified() {
        // `cargo run` (no args) — assume server in web projects.
        assert!(is_server_command("cargo run"));
        assert!(is_server_command("cargo run --release"));
        // `cargo run --bin <name>` could be either; bias toward false
        // so output isn't silently dropped on a misclassification.
        assert!(!is_server_command("cargo run --bin migrator"));
        assert!(!is_server_command("cargo run --example demo"));
    }

    // ── M6.8 Bug B2: TTY-required output detection ─────────────────────

    #[test]
    fn looks_like_tty_required_matches_common_phrases() {
        assert!(looks_like_tty_required("", "└  Operation cancelled"));
        assert!(looks_like_tty_required(
            "Setting up project...",
            "Operation cancelled by user",
        ));
        assert!(looks_like_tty_required("", "Error: stdin is not a TTY"));
        assert!(looks_like_tty_required("", "Input is required to continue"));
        assert!(looks_like_tty_required(
            "",
            "Cannot prompt: no TTY available"
        ));
    }

    #[test]
    fn looks_like_tty_required_does_not_misfire_on_normal_output() {
        // Normal compile errors / test failures shouldn't trigger the
        // hint. Conservative — only the unambiguous fragments fire.
        assert!(!looks_like_tty_required(
            "",
            "Error: cannot find module 'foo'"
        ));
        assert!(!looks_like_tty_required(
            "FAIL src/x.test.ts",
            "Test suite failed to run"
        ));
        assert!(!looks_like_tty_required("", "Permission denied"));
        assert!(!looks_like_tty_required("✓ build succeeded", ""));
    }

    #[test]
    fn format_output_prepends_tty_hint_when_detected() {
        let out = format_output("", "└  Operation cancelled\n", 1);
        assert!(out.contains("[hint:"), "hint should prepend: {out}");
        assert!(
            out.contains("interactive TTY"),
            "hint must name the cause: {out}",
        );
        assert!(
            out.contains("--yes") || out.contains("--no-input"),
            "hint must suggest non-interactive flags: {out}",
        );
        // The original output is preserved below the hint.
        assert!(out.contains("Operation cancelled"));
    }

    #[test]
    fn format_output_skips_hint_when_command_succeeded() {
        // Even if the output happens to contain a "cancelled" word
        // (e.g. a test name like `test_operation_cancelled`), don't
        // prepend the hint when the command exited 0.
        let out = format_output("test_operation_cancelled passed\n", "", 0);
        assert!(!out.contains("[hint:"));
    }

    #[test]
    fn format_output_skips_hint_for_normal_failures() {
        // Compile error, no TTY phrases — no hint.
        let out = format_output("", "error: expected `;`\n", 1);
        assert!(!out.contains("[hint:"));
        assert!(out.contains("expected"));
    }

    // ── M6.8 Bug B1: non-interactive env vars reach the child ──────────

    #[cfg(unix)]
    #[tokio::test]
    async fn ci_env_var_is_set_for_spawned_command() {
        // The most-respected non-interactive signal — every modern
        // npm/pnpm/yarn/vite/jest/ESLint/Prettier respects `CI=1`.
        // If this env var doesn't reach the child, all the other
        // workarounds in M6.8 B1 are also broken, so this acts as
        // the canary for the whole apply_noninteractive_env path.
        let out = BashTool::default()
            .call(json!({"command": "echo \"CI=$CI\""}))
            .await
            .unwrap();
        assert!(
            out.contains("CI=1"),
            "CI=1 must reach the spawned child: got {out:?}",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn term_dumb_reaches_child() {
        // TERM=dumb is the broad signal for "no curses, no progress
        // bars, no interactive prompts." Tools like `less` /
        // `git log` / `vim` use it to skip pager / fall back to
        // non-interactive behaviour.
        let out = BashTool::default()
            .call(json!({"command": "echo \"TERM=$TERM\""}))
            .await
            .unwrap();
        assert!(
            out.contains("TERM=dumb"),
            "TERM=dumb must reach the spawned child: got {out:?}",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn npm_config_yes_reaches_child() {
        // npm respects this for confirmation prompts. Sample test
        // ensures the env var array stays in sync with the code
        // (a future refactor that drops it should fail this test).
        let out = BashTool::default()
            .call(json!({"command": "echo \"NPM_CONFIG_YES=$NPM_CONFIG_YES\""}))
            .await
            .unwrap();
        assert!(
            out.contains("NPM_CONFIG_YES=true"),
            "NPM_CONFIG_YES=true must reach the spawned child: got {out:?}",
        );
    }

    #[test]
    fn venv_wrap_creates_venv_when_missing() {
        let dir = tempdir().unwrap();
        let wrapped = maybe_wrap_with_venv("pip install fastapi", dir.path());
        assert!(wrapped.contains("python3 -m venv"));
        assert!(wrapped.contains("source"));
        assert!(wrapped.contains("pip install fastapi"));
    }

    #[test]
    fn venv_wrap_activates_existing_venv() {
        let dir = tempdir().unwrap();
        let venv = dir.path().join(".venv/bin");
        std::fs::create_dir_all(&venv).unwrap();
        std::fs::write(venv.join("activate"), "").unwrap();
        let wrapped = maybe_wrap_with_venv("pip install fastapi", dir.path());
        assert!(
            !wrapped.contains("python3 -m venv"),
            "should not recreate venv"
        );
        assert!(wrapped.contains("source"));
        assert!(wrapped.contains("activate"));
    }

    #[test]
    fn venv_wrap_skips_when_already_activated() {
        let dir = tempdir().unwrap();
        let cmd = "source .venv/bin/activate && pip install fastapi";
        let wrapped = maybe_wrap_with_venv(cmd, dir.path());
        assert_eq!(wrapped, cmd, "should not double-wrap");
    }

    #[test]
    fn venv_wrap_skips_non_pip_commands() {
        let dir = tempdir().unwrap();
        let cmd = "echo hello";
        let wrapped = maybe_wrap_with_venv(cmd, dir.path());
        assert_eq!(wrapped, cmd);
    }

    #[test]
    fn split_chained_extracts_server_tail() {
        let (setup, server) =
            split_chained_server_command("pip install fastapi && uvicorn app:app --port 8800");
        assert_eq!(setup, vec!["pip install fastapi"]);
        assert_eq!(server.unwrap(), "uvicorn app:app --port 8800");
    }

    #[test]
    fn split_chained_no_server_returns_empty() {
        let (setup, server) = split_chained_server_command("pip install fastapi && echo done");
        assert!(setup.is_empty());
        assert!(server.is_none());
    }

    #[test]
    fn split_chained_single_command_no_split() {
        let (setup, server) = split_chained_server_command("uvicorn app:app --port 8800");
        assert!(setup.is_empty());
        assert!(server.is_none());
    }

    #[test]
    fn split_chained_multiple_setup_parts() {
        let (setup, server) = split_chained_server_command(
            "pip install fastapi && pip install uvicorn && uvicorn app:app --port 8800",
        );
        assert_eq!(setup, vec!["pip install fastapi", "pip install uvicorn"]);
        assert_eq!(server.unwrap(), "uvicorn app:app --port 8800");
    }

    #[test]
    fn venv_wrap_activates_for_uvicorn() {
        let dir = tempdir().unwrap();
        let venv = dir.path().join(".venv/bin");
        std::fs::create_dir_all(&venv).unwrap();
        std::fs::write(venv.join("activate"), "").unwrap();
        let wrapped = maybe_wrap_with_venv("uvicorn main:app --port 8800", dir.path());
        assert!(wrapped.contains("source"));
        assert!(wrapped.contains("activate"));
        assert!(wrapped.contains("uvicorn main:app --port 8800"));
    }

    // Issue #119 reproduction. Mutates the GLOBAL sandbox root + spawns
    // subprocesses, so it's #[ignore]d (would race the scheduler tests'
    // posix_spawn under the parallel runner). Run explicitly:
    //   cargo test --features gui -- --ignored --test-threads=1 repro_119
    //
    // Confirms which mechanism actually blocks the reporter's
    // `<abs>/python.exe script.py`:
    //   A) cwd OUTSIDE the workspace root → real sandbox denial.
    //   B) absolute exe path IN the command (no cwd) → NOT checked; runs
    //      and fails as an ordinary shell error (which the weak model
    //      then paraphrased as "rejected by the security policy").
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn repro_119_bash_sandbox_trigger() {
        let ws = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::env::set_var("THCLAWS_PROJECT_ROOT", ws.path());
        crate::sandbox::Sandbox::init().unwrap();

        // A) cwd outside the workspace root → sandbox denies.
        let a = BashTool::default()
            .call(json!({"command": "echo hi", "cwd": outside.path().to_str().unwrap()}))
            .await;
        eprintln!("[A cwd-outside] {a:?}");
        let a_err = a.expect_err("cwd outside root must be denied");
        let a_msg = format!("{a_err}");
        assert!(
            a_msg.contains("access denied") && a_msg.contains("outside the workspace root"),
            "A should be the sandbox boundary error; got: {a_msg}"
        );

        // B) command runs with default root; the command STRING is not
        //    path-checked, so an in-workspace echo just works.
        let b = BashTool::default()
            .call(json!({"command": "echo IN_WS_OK"}))
            .await
            .expect("default-root command should run");
        eprintln!("[B default-root] {b:?}");
        assert!(b.contains("IN_WS_OK"));

        // C) absolute exe path in the command, no cwd → NOT a sandbox
        //    denial. It runs and fails as a plain shell error (exit
        //    code / not found) — never "access denied".
        let c = BashTool::default()
            .call(json!({"command": "/no/such/python_zzz_119 --version"}))
            .await
            .expect("a failing command returns Ok(output-with-exit-code), not Err");
        eprintln!("[C abs-exe-no-cwd] {c:?}");
        assert!(
            !c.contains("access denied"),
            "an absolute exe path in the command must NOT hit the sandbox; got: {c}"
        );

        std::env::remove_var("THCLAWS_PROJECT_ROOT");
    }
}

#[cfg(all(test, unix))]
mod cancel_kills_the_tree {
    use std::time::Duration;

    /// The regression public issue #194 reported: Stop said "(interrupted)"
    /// while the command kept running. Asserting on the tool's return value
    /// cannot catch that — the UI was already correct. The only proof is an
    /// independent check that the process is gone, so this greps the process
    /// table for a uniquely-named sleep after the call returns.
    #[tokio::test]
    async fn cancel_leaves_no_descendant_running() {
        // A fractional duration nobody else will be running, so `ps` can pick
        // this exact process out.
        let marker = format!("31.{:05}", std::process::id() % 100000);
        let dir = tempfile::tempdir().unwrap();
        let cancel = crate::cancel::CancelToken::new();

        // The trailing `; true` matters: `sh -c` with a single command execs
        // it and tokio ends up holding the sleep's own pid, which
        // `Child::kill` does reach. With a second command the shell forks, the
        // sleep is a real child, and killing the shell alone leaves it
        // orphaned — the shape the confiner produces in the field.
        let cmd = format!("sleep {marker} ; true");
        let tok = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            tok.cancel();
        });

        let res =
            super::run_shell_command_inner(&cmd, dir.path(), 30_000, false, false, Some(&cancel))
                .await;
        assert!(res.is_err(), "cancel should surface as an error");

        // Give the signal a moment to be reaped, then look for survivors.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let ps = std::process::Command::new("ps")
            .args(["-eo", "command"])
            .output()
            .expect("ps");
        let listing = String::from_utf8_lossy(&ps.stdout);
        let survivors: Vec<&str> = listing.lines().filter(|l| l.contains(&marker)).collect();
        assert!(
            survivors.is_empty(),
            "cancel left the command running: {survivors:?}"
        );
    }
}
