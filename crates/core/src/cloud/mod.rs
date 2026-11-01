//! thClaws.cloud catalog client (dev-plan/34).
//!
//! An "AI Agent" in thClaws is a working folder. This module gives the
//! engine four CLI verbs to interact with the catalog at
//! `https://thclaws.cloud`:
//!
//! - `thclaws cloud login`   — paste a CLI token minted from the web dashboard
//! - `thclaws cloud publish` — tar the cwd (stripping secrets + sessions) and upload
//! - `thclaws cloud get`     — download an agent package and extract into a folder
//! - `thclaws cloud list`    — list your purchased / published agents
//!
//! URL precedence: org policy (`gateway.kind: "thclaws"`, `cloud_url`) →
//! `--cloud-url` flag → `THCLAWS_CLOUD_URL` env → `settings.json::cloud.url`
//! → default `https://thclaws.cloud`.
//!
//! Token precedence: `THCLAWS_CLOUD_TOKEN` env → secrets backend
//! (keychain or `~/.config/thclaws/.env`) → legacy
//! `~/.config/thclaws/cloud-token` file (kept readable so older logins
//! still work). The GUI Settings modal writes through `set_token` →
//! current secrets backend, same bundle as provider API keys.

use serde::{Deserialize, Serialize};

pub mod agent_cli;
pub mod agent_scaffold;
pub mod browser_login;
pub mod client;
pub mod cmd;
pub mod manifest;
pub mod pack;
pub mod wssync;

pub const DEFAULT_CLOUD_URL: &str = "https://thclaws.cloud";

const KEYCHAIN_KEY: &str = "cloud-token";
pub const ENV_TOKEN: &str = "THCLAWS_CLOUD_TOKEN";

/// On-disk shape of the `cloud` block in `.thclaws/settings.json` or
/// `~/.config/thclaws/settings.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CloudConfig {
    /// Base URL of the catalog backend. Defaults to `https://thclaws.cloud`
    /// when unset.
    pub url: Option<String>,
}

impl CloudConfig {
    pub fn resolved_url(&self) -> String {
        self.resolved_url_under(crate::policy::thclaws_cloud_url())
    }

    /// A thClaws-gateway policy's `cloud_url` outranks env and settings.
    fn resolved_url_under(&self, policy_url: Option<String>) -> String {
        if let Some(u) = policy_url {
            return u;
        }
        if let Ok(env_url) = std::env::var("THCLAWS_CLOUD_URL") {
            if !env_url.trim().is_empty() {
                return env_url.trim_end_matches('/').to_string();
            }
        }
        self.url
            .as_deref()
            .map(|s| s.trim_end_matches('/').to_string())
            .unwrap_or_else(|| DEFAULT_CLOUD_URL.to_string())
    }
}

/// Resolve the effective cloud URL with CLI-flag override.
pub fn resolve_cloud_url(cli_override: Option<&str>, config: Option<&CloudConfig>) -> String {
    if let Some(u) = crate::policy::thclaws_cloud_url() {
        return u;
    }
    if let Some(u) = cli_override {
        let t = u.trim();
        if !t.is_empty() {
            return t.trim_end_matches('/').to_string();
        }
    }
    match config {
        Some(c) => c.resolved_url(),
        None => CloudConfig::default().resolved_url(),
    }
}

/// Whether `url` is the catalog this install answers to on its own (org
/// policy, `THCLAWS_CLOUD_URL`, or thclaws.cloud) rather than one a project's
/// settings or a flag pointed it at.
pub fn is_own_catalog(url: &str) -> bool {
    url.trim_end_matches('/') == resolve_cloud_url(None, None)
}

/// URL resolved purely from persisted state (no `--cloud-url` flag).
/// Used by the IPC handler that powers the Settings modal — the modal
/// only ever shows what's persisted, not anything an in-process flag
/// might have overridden.
/// #208: a thClaws.cloud CLI token is minted as `thc_` plus random bytes. A
/// value without that prefix is a mis-paste (a gateway key, an API key, a stray
/// line), and saving it would break every cloud call in silence.
pub fn looks_like_cli_token(token: &str) -> bool {
    let t = token.trim();
    t.len() > "thc_".len() && t.starts_with("thc_") && !t.contains(char::is_whitespace)
}

pub fn persisted_url() -> Option<String> {
    let project_url = crate::config::ProjectConfig::load()
        .and_then(|c| c.cloud)
        .and_then(|c| c.url);
    if let Some(u) = project_url {
        let t = u.trim();
        if !t.is_empty() {
            return Some(t.trim_end_matches('/').to_string());
        }
    }
    None
}

/// Where this install keeps its CLI token: `(keychain key, env var)`.
/// An org-policy build signs in to its organisation's cloud, and that token
/// must not overwrite (or be mistaken for) the user's thclaws.cloud one on
/// the same machine, so it gets names of its own, keyed by the cloud host.
fn token_slot() -> (String, String) {
    token_slot_for(crate::policy::thclaws_cloud_url().as_deref())
}

fn token_slot_for(policy_cloud: Option<&str>) -> (String, String) {
    let host = policy_cloud
        .map(|u| u.split("://").nth(1).unwrap_or(u))
        .map(|u| u.split(['/', ':']).next().unwrap_or(u).to_ascii_lowercase())
        .filter(|h| !h.is_empty());
    match host {
        None => (KEYCHAIN_KEY.to_string(), ENV_TOKEN.to_string()),
        Some(h) => {
            let slug: String = h
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_uppercase()
                    } else {
                        '_'
                    }
                })
                .collect();
            (format!("{KEYCHAIN_KEY}:{h}"), format!("{ENV_TOKEN}_{slug}"))
        }
    }
}

/// Token resolution. Order:
/// 1. `THCLAWS_CLOUD_TOKEN` env (CI override).
/// 2. The active secrets backend (keychain or `~/.config/thclaws/.env`).
/// 3. Legacy `~/.config/thclaws/cloud-token` file from earlier MVP
///    builds (kept readable so installs predating the Settings UI keep
///    working).
pub fn token() -> Option<String> {
    let (key, env) = token_slot();
    if let Ok(t) = std::env::var(&env) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    if let Some(t) = crate::secrets::get(&key) {
        if !t.trim().is_empty() {
            return Some(t.trim().to_string());
        }
    }
    if key != KEYCHAIN_KEY {
        return None;
    }
    legacy_file_token()
}

/// Persist a CLI token via whichever backend the user picked for
/// provider API keys. Also pushes the value into the process env so
/// the in-flight CLI invocation can use it without a restart.
pub fn set_token(token: &str) -> crate::error::Result<()> {
    let (key, env) = token_slot();
    let backend = crate::secrets::resolved_backend();
    match backend {
        crate::secrets::Backend::Keychain => {
            crate::secrets::set(&key, token)?;
        }
        crate::secrets::Backend::Dotenv => {
            crate::dotenv::upsert_user_env(&env, token)?;
        }
    }
    std::env::set_var(&env, token);
    // Best-effort: remove the legacy plaintext file so users migrating
    // from earlier builds end up with a single source of truth.
    let _ = clear_legacy_file();
    Ok(())
}

/// Remove the token from the active backend AND the legacy file.
/// Idempotent. Also unsets the in-process env var so the next CLI
/// call doesn't see a stale value.
pub fn clear_token() -> crate::error::Result<()> {
    let (key, env) = token_slot();
    let backend = crate::secrets::resolved_backend();
    match backend {
        crate::secrets::Backend::Keychain => {
            let _ = crate::secrets::set(&key, "");
        }
        crate::secrets::Backend::Dotenv => {
            let _ = crate::dotenv::upsert_user_env(&env, "");
        }
    }
    std::env::remove_var(&env);
    let _ = clear_legacy_file();
    Ok(())
}

/// Whether the active secrets backend can durably persist a token.
/// Mirrors `remote_agent::keychain_writable` so the UI's "disabled
/// because nothing writable" branch behaves the same on both.
pub fn token_writable() -> bool {
    matches!(
        crate::secrets::get_backend(),
        Some(crate::secrets::Backend::Keychain) | Some(crate::secrets::Backend::Dotenv) | None
    )
}

fn legacy_file_path() -> std::path::PathBuf {
    crate::util::home_dir()
        .map(|h| h.join(".config").join(crate::profile::app_dir_name()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("cloud-token")
}

fn legacy_file_token() -> Option<String> {
    std::fs::read_to_string(legacy_file_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn clear_legacy_file() -> std::io::Result<()> {
    let p = legacy_file_path();
    if p.exists() {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

#[cfg(test)]
mod cli_token_tests {
    #[test]
    fn a_policy_cloud_gets_its_own_token_slot() {
        use super::token_slot_for as slot;
        assert_eq!(
            slot(None),
            ("cloud-token".into(), "THCLAWS_CLOUD_TOKEN".into())
        );
        assert_eq!(
            slot(Some("https://sis.thclaws.ai/")),
            (
                "cloud-token:sis.thclaws.ai".into(),
                "THCLAWS_CLOUD_TOKEN_SIS_THCLAWS_AI".into()
            )
        );
        assert_eq!(
            slot(Some("https://Acme.example:8443")).0,
            "cloud-token:acme.example"
        );
    }

    #[test]
    fn only_a_thc_token_looks_like_a_cli_token() {
        use super::looks_like_cli_token as ok;
        assert!(ok("thc_AbCdEfGhIjKlMnOp"));
        assert!(ok("  thc_AbCdEf  "), "surrounding whitespace is trimmed");
        assert!(!ok("thc_"), "prefix alone");
        assert!(!ok("gw_v1_abc"), "a gateway key");
        assert!(!ok("sk-ant-api03-xyz"), "a provider key");
        assert!(!ok("thc_abc def"), "two things pasted together");
        assert!(!ok(""));
    }
}

#[cfg(test)]
mod policy_url_tests {
    use super::CloudConfig;

    #[test]
    fn a_policy_cloud_url_outranks_settings() {
        let c = CloudConfig {
            url: Some("https://thclaws.cloud/".into()),
        };
        assert_eq!(
            c.resolved_url_under(Some("https://sis.example".into())),
            "https://sis.example"
        );
    }
}

#[cfg(test)]
mod token_storage_tests {
    /// A fresh install that never answered the storage question resolves
    /// to Dotenv everywhere. `set_token` used to assume Keychain there,
    /// which `secrets::set` refuses — so a CLI browser sign-in failed with
    /// "keychain disabled by user preference" before anything was saved.
    #[test]
    fn a_token_saves_before_the_storage_question_is_answered() {
        let _g = crate::kms::test_env_lock();
        let prev_home = std::env::var("HOME").ok();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let (_, env) = super::token_slot();
        std::env::remove_var(&env);
        assert!(
            crate::secrets::get_backend().is_none(),
            "nothing chosen yet"
        );

        let saved = super::set_token("thc_fresh_install_token");
        let dotenv =
            std::fs::read_to_string(crate::dotenv::user_dotenv_path().unwrap()).unwrap_or_default();
        let read_back = super::token();
        super::clear_token().unwrap();
        let after_clear = super::token();

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        saved.expect("set_token must succeed with no backend chosen");
        assert!(dotenv.contains(&format!("{env}=thc_fresh_install_token")));
        assert_eq!(read_back.as_deref(), Some("thc_fresh_install_token"));
        assert_eq!(after_clear, None);
    }
}
