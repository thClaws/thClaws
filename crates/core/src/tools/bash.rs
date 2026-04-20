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
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn description(&self) -> &'static str {
        "Run a shell command via `/bin/sh -c`. Captures stdout and stderr. \
         Default timeout: 120000ms (override with `timeout` in milliseconds, max 600000). \
         Always requires approval. Use this for general operations (git, build, \
         test, curl, ls -l, rm, etc.) that the specialized tools don't cover. \
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

    fn requires_approval(&self, input: &Value) -> bool {
        // Always require approval, but flag destructive commands so the
        // approval prompt can highlight the risk.
        if let Some(cmd) = input.get("command").and_then(Value::as_str) {
            if is_destructive_command(cmd) {
                return true; // could be a higher tier in the future
            }
        }
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
            std::env::current_dir()?
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
            setup_output = run_shell_command(&setup_cmd, &resolved_cwd, timeout_ms, false).await?;
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
        let server_output =
            run_shell_command(&command, &resolved_cwd, effective_timeout, is_server).await?;

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
) -> Result<String> {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(cwd);

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Tool(format!("spawn: {e}")))?;

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

    let wait_result = timeout(Duration::from_millis(timeout_ms), child.wait()).await;
    match wait_result {
        Err(_) if is_server => {
            // Server command — timeout is expected. Server keeps running.
            // DON'T await reader tasks (pipes still open, would block forever).
            drop(stdout_task);
            drop(stderr_task);
            Ok(format!(
                "Server started and running in background.\n\
                 The process will continue after this tool returns.\n\
                 Use `curl localhost:PORT` or a browser to verify."
            ))
        }
        Err(_) => {
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
            Ok(format_output(&stdout, &stderr, exit_code))
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

/// Does this command need a Python venv? Any python/pip command should use
/// the project venv if one exists, plus specific tool commands.
fn needs_venv(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    // Any python/pip invocation should use the venv.
    lower.starts_with("python ")
        || lower.starts_with("python3 ")
        || lower.contains("pip install")
        || lower.contains("pip3 install")
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
pub fn is_destructive_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();

    let simple_patterns = [
        "rm -rf",
        "rm -fr",
        "rmdir",
        "rm -r",
        "mv ",
        "truncate",
        "> /",
        "dd if=",
        "mkfs",
        "chmod -R",
        "chown -R",
        "kill -9",
        "killall",
        "pkill",
        "sudo ",
        ":(){ :|:& };:",
        "format ",
    ];
    if simple_patterns.iter().any(|p| lower.contains(p)) {
        return true;
    }

    // Detect piping download commands into a shell: curl ... | sh, wget ... | bash
    if lower.contains("| sh")
        || lower.contains("|sh")
        || lower.contains("| bash")
        || lower.contains("|bash")
    {
        if lower.contains("curl") || lower.contains("wget") {
            return true;
        }
    }

    false
}

/// Detect commands that start long-running server processes.
pub fn is_server_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    // Only match if NOT already backgrounded.
    if lower.trim().ends_with('&') {
        return false;
    }

    let patterns = [
        "uvicorn ",
        "gunicorn ",
        "hypercorn ",
        "flask run",
        "django runserver",
        "manage.py runserver",
        "npm run dev",
        "npm start",
        "npx ",
        "yarn dev",
        "pnpm dev",
        "node server",
        "node index",
        "node app",
        "cargo run", // often a server in web projects
        "python -m http.server",
        "python3 -m http.server",
        "python -m uvicorn",
        "python3 -m uvicorn",
        "python -m flask",
        "python3 -m flask",
        "php -S ",
        "php artisan serve",
        "ruby server",
        "rails server",
        "rails s",
        "go run ",
        "docker compose up",
        "docker-compose up",
        "kubectl port-forward",
        "ngrok ",
        "cloudflared tunnel",
        "serve ",
        "live-server",
        "http-server",
        "next dev",
        "vite",
        "webpack serve",
    ];
    if patterns.iter().any(|p| lower.contains(p)) {
        return true;
    }

    // `python app.py`, `python main.py`, `python server.py`, `python run.py`
    // are almost always web servers in agentic coding contexts.
    // We match the script name as a standalone word (preceded by space).
    if lower.starts_with("python ") || lower.starts_with("python3 ") {
        let py_scripts = [
            " app.py",
            " main.py",
            " server.py",
            " run.py",
            " wsgi.py",
            " asgi.py",
        ];
        if py_scripts.iter().any(|p| lower.contains(p)) {
            return true;
        }
    }

    false
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
    if parts.is_empty() {
        String::new()
    } else {
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(unix)]
    #[tokio::test]
    async fn echoes_stdout() {
        let out = BashTool
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

    #[cfg(unix)]
    #[tokio::test]
    async fn captures_stderr() {
        let out = BashTool
            .call(json!({"command": "echo oops >&2"}))
            .await
            .unwrap();
        assert!(out.contains("[stderr]"));
        assert!(out.contains("oops"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_appended_to_output() {
        let out = BashTool
            .call(json!({"command": "echo done; exit 3"}))
            .await
            .unwrap();
        assert!(out.contains("done"));
        assert!(out.contains("[exit code 3]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdout_and_stderr_both_captured() {
        let out = BashTool
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
        let out = BashTool
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
        let out = BashTool
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

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_secs_legacy_alias_works() {
        let out = BashTool
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
        let err = BashTool.call(json!({})).await.unwrap_err();
        assert!(format!("{err}").contains("command"));
    }

    #[test]
    fn bash_requires_approval() {
        let bash = BashTool;
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
    fn needs_venv_detects_pip_and_python_tools() {
        assert!(needs_venv("pip install fastapi"));
        assert!(needs_venv("pip3 install uvicorn"));
        assert!(needs_venv("uvicorn main:app --port 8000"));
        assert!(needs_venv("gunicorn app:app"));
        assert!(needs_venv("pytest tests/"));
        assert!(needs_venv("flask run"));
        assert!(needs_venv("python app.py"));
        assert!(needs_venv("python3 main.py"));
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
}
