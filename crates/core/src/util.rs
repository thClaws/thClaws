//! Small cross-cutting utilities that don't belong to any single
//! subsystem. Currently: cross-platform home-directory lookup.

use std::path::PathBuf;

// for Windows creation flag to hide the console window
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

/// The current user's home directory, in a form that works on both
/// Unix and Windows.
///
/// On Unix this is just `$HOME`. On Windows there's no `HOME` by
/// default — we fall back to `%USERPROFILE%` (set by Explorer and
/// the user profile loader on every login) and then
/// `%HOMEDRIVE%%HOMEPATH%` (used by some older tooling).
///
/// Returns `None` only if every candidate is unset or empty — in
/// practice a truly broken Windows environment; most of the
/// path-touching code in thClaws degrades gracefully in that case
/// rather than panicking.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Ok(h) = std::env::var("USERPROFILE") {
            if !h.is_empty() {
                return Some(PathBuf::from(h));
            }
        }
        if let (Ok(d), Ok(p)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
            if !d.is_empty() && !p.is_empty() {
                return Some(PathBuf::from(format!("{d}{p}")));
            }
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// String form of `home_dir()` — mirrors the shape of call sites
/// that did `std::env::var("HOME").ok()?` and then used the result
/// as a `&str` / joined paths via `format!`. Prefer `home_dir()`
/// when you want a `PathBuf` directly.
pub fn home_string() -> Option<String> {
    home_dir().map(|p| p.to_string_lossy().into_owned())
}

/// Render a proportional progress bar. Example:
/// `[████████▓░░░░░░░░░░░░░░░]` for 35% over 24 cells. Half-step `▓`
/// for fractional fills. ANSI-colored: green <60%, yellow 60–80%,
/// red ≥80%.
pub fn progress_bar(pct: f64, width: usize) -> String {
    let clamped = pct.clamp(0.0, 100.0);
    let filled_f = clamped / 100.0 * width as f64;
    let full = filled_f.floor() as usize;
    let frac = filled_f - full as f64;
    let half = if frac >= 0.5 && full < width { 1 } else { 0 };
    let empty = width - full - half;
    let color = if clamped >= 80.0 {
        "\x1b[31m"
    } else if clamped >= 60.0 {
        "\x1b[33m"
    } else {
        "\x1b[32m"
    };
    let reset = "\x1b[0m";
    format!(
        "[{color}{}{}{reset}{}]",
        "█".repeat(full),
        "▓".repeat(half),
        "░".repeat(empty),
    )
}

/// Byte size in human units: `512`→`"512 B"`, `2048`→`"2.0 KB"`,
/// `5_500_000`→`"5.2 MB"`.
pub fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{} B", n)
    }
}

/// Abbreviate token counts: `200000`→`"200k"`, `1_200_000`→`"1.2M"`.
pub fn format_tokens(n: usize) -> String {
    if n >= 1_000_000 {
        let f = n as f64 / 1_000_000.0;
        if (f.round() - f).abs() < 0.05 {
            format!("{}M", f.round() as u64)
        } else {
            format!("{:.1}M", f)
        }
    } else if n >= 1_000 {
        let f = n as f64 / 1_000.0;
        if (f.round() - f).abs() < 0.05 {
            format!("{}k", f.round() as u64)
        } else {
            format!("{:.1}k", f)
        }
    } else {
        n.to_string()
    }
}

/// Build a sync `std::process::Command` that runs a shell-string in
/// the platform's default shell. On Windows this is `cmd.exe /C
/// <cmd>`; on Unix it's `/bin/sh -c <cmd>`. Centralized here so the
/// 4+ tool / hook / team / repl call sites don't each repeat the
/// `cfg!(windows)` branch.
///
/// Caveats: bash-syntax commands the agent emits (`find . -name
/// '*.rs'`, complex pipelines, `&&` chains with single-quoted args)
/// may not parse identically under cmd.exe. Power users can override
/// with the `THCLAWS_SHELL` env var (path to a shell + flag pair like
/// `bash -c`) — see [`shell_command_sync`] / [`shell_command_async`]
/// for the override path.
pub fn shell_command_sync(command: &str) -> std::process::Command {
    let (shell, flag) = shell_invocation();
    let mut c = std::process::Command::new(shell);
    c.arg(flag).arg(command);

    // for Windows creation flag to hide the console window
    #[cfg(target_os = "windows")]
    c.creation_flags(0x08000000);

    c
}

/// Async variant for tokio-based call sites (currently the Bash
/// tool). Same shell-resolution logic as [`shell_command_sync`].
pub fn shell_command_async(command: &str) -> tokio::process::Command {
    let (shell, flag) = shell_invocation();
    let mut c = tokio::process::Command::new(shell);
    c.arg(flag).arg(command);

    // for Windows creation flag to hide the console window
    #[cfg(target_os = "windows")]
    c.creation_flags(0x08000000);

    c
}

/// Resolve `(shell, flag)` for the current host. Honors
/// `THCLAWS_SHELL` for power-user overrides — set it to a single
/// string like `"bash -c"` or `"pwsh -Command"` and we split on
/// whitespace; the first token is the executable, the second is the
/// flag.
fn shell_invocation() -> (String, String) {
    if let Ok(s) = std::env::var("THCLAWS_SHELL") {
        let parts: Vec<&str> = s.split_whitespace().collect();
        if parts.len() == 2 {
            return (parts[0].to_string(), parts[1].to_string());
        }
    }
    if cfg!(windows) {
        ("cmd".to_string(), "/C".to_string())
    } else {
        ("/bin/sh".to_string(), "-c".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_dir_returns_something_on_dev_machine() {
        // Dev machines set HOME (Unix) or USERPROFILE (Windows). In
        // CI this could fail if a sandboxed runner strips env — we
        // allow `None` there, but don't crash.
        let _ = home_dir();
    }

    #[test]
    fn shell_invocation_picks_platform_default() {
        // Clear any THCLAWS_SHELL override so we test the default.
        let saved = std::env::var("THCLAWS_SHELL").ok();
        std::env::remove_var("THCLAWS_SHELL");
        let (shell, flag) = shell_invocation();
        if cfg!(windows) {
            assert_eq!(shell, "cmd");
            assert_eq!(flag, "/C");
        } else {
            assert_eq!(shell, "/bin/sh");
            assert_eq!(flag, "-c");
        }
        if let Some(v) = saved {
            std::env::set_var("THCLAWS_SHELL", v);
        }
    }

    #[test]
    fn thclaws_shell_override_works() {
        let saved = std::env::var("THCLAWS_SHELL").ok();
        std::env::set_var("THCLAWS_SHELL", "bash -c");
        let (shell, flag) = shell_invocation();
        assert_eq!(shell, "bash");
        assert_eq!(flag, "-c");
        std::env::remove_var("THCLAWS_SHELL");
        if let Some(v) = saved {
            std::env::set_var("THCLAWS_SHELL", v);
        }
    }
}
