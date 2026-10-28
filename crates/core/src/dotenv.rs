//! Minimal `.env` loader. Reads `KEY=VALUE` lines and sets them as env vars
//! if they aren't already set (existing env takes precedence).
//!
//! Load order (earlier wins):
//!   1. Process environment (explicit `export` in shell)
//!   2. `./.env` (project-local)
//!   3. `~/.config/thclaws/.env` (global)
//!
//! Lines starting with `#` are comments. Empty lines are skipped. Values
//! can be optionally quoted with `"` or `'` (quotes are stripped).

use std::path::{Path, PathBuf};

/// Load `.env` files from standard locations. Call once at startup before
/// any config or provider code reads env vars.
pub fn load_dotenv() {
    let candidates = [global_dotenv_path(), Some(PathBuf::from(".env"))];
    // Load global first, then project-local. Since we only set vars that
    // aren't already present, project-local effectively overrides global
    // because it's loaded second (and the first load set them).
    // Wait — that's backwards. We want project-local to win over global.
    // So load global first (sets unset vars), then project-local (can't
    // override because vars are already set). That's wrong.
    //
    // Fix: load project-local FIRST, then global. Already-set vars are
    // skipped, so project-local values stick.
    for path in candidates.into_iter().rev().flatten() {
        let project = path.is_relative();
        let restrict = project
            && !std::env::current_dir()
                .map(|d| crate::config::trust::is_folder_trusted(&d))
                .unwrap_or(false);
        load_file_with(&path, restrict);
    }
}

fn global_dotenv_path() -> Option<PathBuf> {
    crate::util::home_dir()
        .map(|h| h.join(format!(".config/{}/.env", crate::profile::app_dir_name())))
}

/// Resolve the user-scope `.env` path, exposed so higher layers can
/// write to it (the keychain-fallback path in the Settings modal).
pub fn user_dotenv_path() -> Option<PathBuf> {
    global_dotenv_path()
}

/// Upsert a `KEY=value` line into the user-scope `~/.config/thclaws/.env`.
/// Creates the file if it doesn't exist. Preserves surrounding lines,
/// comments, and formatting — only the matching key's value is changed
/// (or the line appended if no match).
///
/// Used as a fallback when the OS keychain isn't available (headless
/// Linux without Secret Service, or a user who declined the keychain
/// permission prompt on macOS).
pub fn upsert_user_env(var: &str, value: &str) -> crate::error::Result<PathBuf> {
    let path = user_dotenv_path()
        .ok_or_else(|| crate::error::Error::Config("cannot locate user home directory".into()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(String::from).collect();
    let mut replaced = false;
    let prefix = format!("{var}=");
    for line in lines.iter_mut() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with(&prefix) {
            *line = format!("{var}={value}");
            replaced = true;
            break;
        }
    }
    if !replaced {
        lines.push(format!("{var}={value}"));
    }
    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(&path, out)?;
    Ok(path)
}

/// Remove a `KEY=...` line from the user-scope `.env`. Silently succeeds
/// if the file or the key doesn't exist.
pub fn remove_from_user_env(var: &str) -> crate::error::Result<Option<PathBuf>> {
    let Some(path) = user_dotenv_path() else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let existing = std::fs::read_to_string(&path)?;
    let prefix = format!("{var}=");
    let out: String = existing
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with(&prefix)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = out;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(&path, out)?;
    Ok(Some(path))
}

/// Variables that must NEVER be settable from a `.env` file. A
/// malicious project-local `.env` could otherwise steer the process to
/// a poisoned binary (`PATH`), load an attacker's dynamic library
/// (`LD_PRELOAD`, `DYLD_*`), redirect proxy traffic
/// (`HTTP_PROXY`/`HTTPS_PROXY`), or move the home directory out from
/// under us. Setting these through the user's shell profile is fine —
/// the attack surface is specifically *project-local* `.env` files
/// shipped by cloned repos.
const DOTENV_BLOCKLIST: &[&str] = &[
    // Binary resolution / library injection
    "PATH",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "LD_AUDIT",
    "DYLD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_FALLBACK_FRAMEWORK_PATH",
    // Identity / filesystem root
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    // Temp dir hijack (symlink races, predictable paths)
    "TMPDIR",
    "TMP",
    "TEMP",
    // Editor / pager invoked by git, less, man, gh, etc.
    "EDITOR",
    "VISUAL",
    "PAGER",
    // Network interception
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    // Language-runtime module injection. Each of these lets a `.env`
    // pre-load arbitrary attacker-controlled code whenever a project
    // tool spawns the corresponding interpreter.
    "NODE_OPTIONS",
    "NODE_PATH",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "PYTHONHOME",
    "PYTHONUSERBASE",
    "RUBYOPT",
    "RUBYLIB",
    "PERL5OPT",
    "PERL5LIB",
    "PERL5DB",
    // Git tooling hijack — git spawns these for auth, remote ops, and
    // to resolve its own helpers.
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
    "GIT_EXEC_PATH",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_TEMPLATE_DIR",
    // SSH agent forwarding + askpass
    "SSH_ASKPASS",
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    // thclaws internals an attacker could hijack
    "THCLAWS_MCP_ALLOW_ALL",
    "THCLAWS_CONFIG_DIR",
    "THCLAWS_DATA_DIR",
    // Shell hook injection
    "BASH_ENV",
    "ENV",
    "PROMPT_COMMAND",
    "PS1",
    "PS2",
    "PS4",
    "ZDOTDIR",
    "IFS",
];

/// Suffixes of variables that point the engine at a host. A project `.env`
/// in an untrusted folder setting one of these would send the user's own
/// credentials to whoever wrote it (audit H2), so it only takes effect once
/// the folder is trusted.
const ENDPOINT_SUFFIXES: &[&str] = &["_URL", "_BASE", "_API_BASE", "_ENDPOINT", "_HOST"];

/// Name endings an untrusted project `.env` may set: credentials and nothing
/// else. A denylist kept missing levers (`XDG_CONFIG_HOME` moved the MCP
/// allowlist into the repo, `CLAUDE_BIN` named a binary in it), so outside
/// a trusted folder only a key-shaped name gets through.
const UNTRUSTED_ALLOWED_SUFFIXES: &[&str] = &["_API_KEY", "_KEY", "_TOKEN", "_SECRET"];

/// Keys an untrusted project `.env` may not set on top of [`is_blocked_key`]:
/// everything that isn't a credential, and every `THCLAWS_*` knob even when
/// it looks like one.
fn is_untrusted_project_key(key: &str) -> bool {
    let up = key.to_ascii_uppercase();
    up.starts_with("THCLAWS_")
        || ENDPOINT_SUFFIXES.iter().any(|s| up.ends_with(s))
        || !UNTRUSTED_ALLOWED_SUFFIXES.iter().any(|s| up.ends_with(s))
}

fn is_blocked_key(key: &str) -> bool {
    let up = key.to_ascii_uppercase();
    // Exact blocklist match.
    if DOTENV_BLOCKLIST.iter().any(|b| *b == up) {
        return true;
    }
    // Any LC_* / LANG override is safe, but anything starting with
    // DYLD_ or LD_ that we didn't enumerate should still be blocked —
    // the loader honours runtime-specific variants we may not have
    // listed.
    if up.starts_with("LD_") || up.starts_with("DYLD_") || up.starts_with("XDG_") {
        return true;
    }
    false
}

/// Walk up from `start` to the filesystem root, loading the first `.env`
/// file found. Stops at the first match or when no parent remains. Used
/// by operator tools (catalogue-seed) where the binary is invoked from a
/// nested crate dir but the canonical `.env` lives at the workspace
/// root. Idempotent: already-set env vars are preserved (same semantics
/// as `load_dotenv`).
pub fn load_dotenv_walking_up(start: &Path) {
    let mut dir = Some(start.to_path_buf());
    while let Some(d) = dir {
        let candidate = d.join(".env");
        if candidate.is_file() {
            load_file(&candidate);
            return;
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }
}

fn load_file(path: &Path) {
    load_file_with(path, false)
}

fn load_file_with(path: &Path, untrusted_project: bool) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let mut value = value.trim();

        // Strip optional quotes.
        if (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''))
        {
            value = &value[1..value.len() - 1];
        }

        if is_blocked_key(key) {
            eprintln!(
                "\x1b[33m[dotenv] ignoring {} from {} — security-sensitive var cannot be set via .env\x1b[0m",
                key,
                path.display()
            );
            continue;
        }
        if untrusted_project && is_untrusted_project_key(key) {
            eprintln!(
                "\x1b[33m[dotenv] ignoring {} from {} — an untrusted folder's .env may set only API keys and tokens\x1b[0m",
                key,
                path.display()
            );
            continue;
        }

        // Only set if not already in the environment.
        if std::env::var(key).is_err() {
            std::env::set_var(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_file_sets_unset_vars() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "# comment\nTEST_DOTENV_A=hello\nTEST_DOTENV_B=\"quoted\"\n",
        )
        .unwrap();

        // Make sure they're not already set.
        std::env::remove_var("TEST_DOTENV_A");
        std::env::remove_var("TEST_DOTENV_B");

        load_file(&path);

        assert_eq!(std::env::var("TEST_DOTENV_A").unwrap(), "hello");
        assert_eq!(std::env::var("TEST_DOTENV_B").unwrap(), "quoted");

        // Cleanup.
        std::env::remove_var("TEST_DOTENV_A");
        std::env::remove_var("TEST_DOTENV_B");
    }

    #[test]
    fn blocked_keys_cover_runtime_hijack_vectors() {
        // Regression guard: every new runtime hijack vector added to
        // DOTENV_BLOCKLIST should stay blocked.
        for key in [
            "NODE_OPTIONS",
            "PYTHONPATH",
            "PYTHONSTARTUP",
            "RUBYOPT",
            "PERL5OPT",
            "PERL5LIB",
            "GIT_SSH",
            "GIT_SSH_COMMAND",
            "GIT_ASKPASS",
            "GIT_EXEC_PATH",
            "SSH_ASKPASS",
            "SSH_AUTH_SOCK",
            "TMPDIR",
            "TMP",
            "TEMP",
            "EDITOR",
            "VISUAL",
            "PAGER",
        ] {
            assert!(is_blocked_key(key), "{key} must be blocked");
        }
    }

    #[test]
    fn blocked_keys_are_not_loaded() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "PATH=/attacker/bin\n\
             LD_PRELOAD=/tmp/evil.so\n\
             DYLD_INSERT_LIBRARIES=/tmp/x.dylib\n\
             LD_SOMETHING_NEW=hi\n\
             THCLAWS_MCP_ALLOW_ALL=1\n\
             TEST_DOTENV_OK=safe\n",
        )
        .unwrap();

        let orig_path = std::env::var("PATH").ok();
        std::env::remove_var("LD_PRELOAD");
        std::env::remove_var("DYLD_INSERT_LIBRARIES");
        std::env::remove_var("LD_SOMETHING_NEW");
        std::env::remove_var("THCLAWS_MCP_ALLOW_ALL");
        std::env::remove_var("TEST_DOTENV_OK");

        load_file(&path);

        // Dangerous keys must be ignored (including LD_*/DYLD_* variants
        // we haven't individually enumerated).
        assert_eq!(std::env::var("PATH").ok(), orig_path);
        assert!(std::env::var("LD_PRELOAD").is_err());
        assert!(std::env::var("DYLD_INSERT_LIBRARIES").is_err());
        assert!(std::env::var("LD_SOMETHING_NEW").is_err());
        assert!(std::env::var("THCLAWS_MCP_ALLOW_ALL").is_err());
        // Non-dangerous keys still load.
        assert_eq!(std::env::var("TEST_DOTENV_OK").unwrap(), "safe");

        std::env::remove_var("TEST_DOTENV_OK");
    }

    #[test]
    fn untrusted_project_env_cannot_redirect_endpoints() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "THCLAWS_GATEWAY_BASE_URL=https://evil.example\n\
             OPENAI_COMPAT_BASE_URL=https://evil.example\n\
             AZURE_AI_FOUNDRY_ENDPOINT=https://evil.example\n\
             OPENAI_API_BASE=https://evil.example\n\
             THCLAWS_TRUST_PROJECT=1\n\
             TEST_DOTENV_UNTRUSTED_API_KEY=safe\n",
        )
        .unwrap();
        let keys = [
            "THCLAWS_GATEWAY_BASE_URL",
            "OPENAI_COMPAT_BASE_URL",
            "AZURE_AI_FOUNDRY_ENDPOINT",
            "OPENAI_API_BASE",
            "THCLAWS_TRUST_PROJECT",
        ];
        let saved: Vec<_> = keys.iter().map(|k| std::env::var(k).ok()).collect();
        for k in keys {
            std::env::remove_var(k);
        }
        std::env::remove_var("TEST_DOTENV_UNTRUSTED_API_KEY");

        load_file_with(&path, true);
        for k in keys {
            assert!(
                std::env::var(k).is_err(),
                "{k} must not load from an untrusted .env"
            );
        }
        assert_eq!(
            std::env::var("TEST_DOTENV_UNTRUSTED_API_KEY").unwrap(),
            "safe"
        );

        load_file_with(&path, false);
        assert_eq!(
            std::env::var("OPENAI_COMPAT_BASE_URL").unwrap(),
            "https://evil.example",
            "a trusted folder keeps its endpoint overrides"
        );

        for (k, v) in keys.iter().zip(saved) {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        std::env::remove_var("TEST_DOTENV_UNTRUSTED_API_KEY");
    }

    #[test]
    fn untrusted_project_env_sets_only_credentials() {
        // Re-audit N1/N2: XDG_CONFIG_HOME pointed the MCP allowlist into the
        // repo; CLAUDE_BIN named a binary in it.
        for key in [
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "CLAUDE_BIN",
            "SLIDE_RENDER_API",
            "GIT_AUTHOR_NAME",
            "PLAYWRIGHT_BROWSERS_PATH",
            "THCLAWS_BASH_SANDBOX",
            "THCLAWS_ALLOW_PRIVATE_FETCH",
            "THCLAWS_INSIDE_DOCKER",
            "THCLAWS_GATEWAY_API_KEY",
        ] {
            assert!(
                is_untrusted_project_key(key),
                "{key} must need a trusted folder"
            );
        }
        for key in [
            "OPENAI_API_KEY",
            "HAL_API_KEY",
            "SLIDE_RENDER_TOKEN",
            "AWS_SECRET",
        ] {
            assert!(!is_untrusted_project_key(key), "{key} is a credential");
        }
        assert!(
            is_blocked_key("XDG_CONFIG_HOME"),
            "no .env moves the config dir"
        );
    }

    #[test]
    fn existing_env_takes_precedence() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "TEST_DOTENV_C=from_file\n").unwrap();

        std::env::set_var("TEST_DOTENV_C", "from_shell");
        load_file(&path);
        assert_eq!(std::env::var("TEST_DOTENV_C").unwrap(), "from_shell");

        std::env::remove_var("TEST_DOTENV_C");
    }
}
