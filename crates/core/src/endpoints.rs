//! Plain-file–backed storage for provider base URLs.
//!
//! Base URLs aren't secrets so there's no need for the OS keychain. This
//! module persists them in `~/.config/thclaws/endpoints.json` as a flat
//! `{ ENV_VAR: "url" }` map and injects them into the process environment
//! at startup — only setting vars that aren't already set, preserving the
//! precedence: shell export > endpoints file > dotenv files.
//!
//! The map is keyed by env var name (e.g. `"OLLAMA_BASE_URL"`) rather than
//! provider name so two providers can share a single override (Ollama and
//! Ollama-Anthropic both read `OLLAMA_BASE_URL`).

use crate::error::{Error, Result};
use crate::providers::ProviderKind;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn path() -> Option<PathBuf> {
    crate::util::home_dir().map(|h| {
        h.join(format!(
            ".config/{}/endpoints.json",
            crate::profile::app_dir_name()
        ))
    })
}

fn read_map() -> BTreeMap<String, String> {
    let Some(p) = path() else {
        return BTreeMap::new();
    };
    let Ok(contents) = std::fs::read_to_string(&p) else {
        return BTreeMap::new();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

fn write_map(map: &BTreeMap<String, String>) -> Result<()> {
    let Some(p) = path() else {
        return Err(Error::Config("cannot locate user home directory".into()));
    };
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Config(format!("create dir: {e}")))?;
    }
    let json =
        serde_json::to_string_pretty(map).map_err(|e| Error::Config(format!("serialize: {e}")))?;
    std::fs::write(&p, json).map_err(|e| Error::Config(format!("write: {e}")))
}

/// Set the base URL for a provider by its short name (e.g. `"ollama"`).
/// Trims trailing slashes and rejects empty values.
pub fn set(provider: &str, url: &str) -> Result<()> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(Error::Config("base URL is empty".into()));
    }
    let env_var = env_var_for(provider)
        .ok_or_else(|| Error::Config(format!("provider '{provider}' has no endpoint")))?;
    let mut map = read_map();
    map.insert(env_var.to_string(), trimmed.to_string());
    write_map(&map)
}

/// Get the persisted base URL for a provider, if any.
pub fn get(provider: &str) -> Option<String> {
    let env_var = env_var_for(provider)?;
    read_map().get(env_var).cloned()
}

/// Delete the persisted base URL for a provider. No-op if unset.
pub fn clear(provider: &str) -> Result<()> {
    let Some(env_var) = env_var_for(provider) else {
        return Ok(());
    };
    let mut map = read_map();
    map.remove(env_var);
    write_map(&map)
}

/// UI-facing snapshot: provider, env var name, current persisted URL (if
/// any), and the default to show as placeholder.
#[derive(Debug, Clone)]
pub struct EndpointStatus {
    pub provider: &'static str,
    pub env_var: &'static str,
    pub configured_url: Option<String>,
    pub default_url: &'static str,
}

pub fn status() -> Vec<EndpointStatus> {
    let mut seen_env_vars: std::collections::HashSet<&'static str> = Default::default();
    let mut out: Vec<EndpointStatus> = ProviderKind::ALL
        .iter()
        .filter(|p| p.endpoint_user_configurable())
        .filter_map(|p| {
            let env_var = p.endpoint_env()?;
            // Dedupe by env var: Ollama and Ollama-Anthropic share
            // OLLAMA_BASE_URL, so we surface just one row in the UI.
            if !seen_env_vars.insert(env_var) {
                return None;
            }
            let default_url = p.default_endpoint()?;
            // Read the live env var rather than just our JSON file: the
            // effective URL may come from any of shell export > endpoints.json
            // > .env files (all injected into env at startup). Showing the
            // process env gives the user the URL that's actually in use.
            Some(EndpointStatus {
                provider: p.name(),
                env_var,
                configured_url: std::env::var(env_var).ok(),
                default_url,
            })
        })
        .collect();
    out.push(EndpointStatus {
        provider: AISERVER,
        env_var: AISERVER_ENV,
        configured_url: std::env::var(AISERVER_ENV).ok(),
        default_url: "http://<box>:8100",
    });
    out
}

/// Inject persisted base URLs into the process environment. Call **before**
/// [`crate::dotenv::load_dotenv`] so precedence is: shell > endpoints file
/// > dotenv.
pub fn load_into_env() {
    for (var, url) in read_map() {
        if std::env::var(&var).is_err() {
            std::env::set_var(&var, &url);
        }
    }
}

/// The AI Server Suite is no provider of its own (its models go through
/// `litellm`), but its address is set beside its key like one.
pub const AISERVER: &str = "aiserver";
const AISERVER_ENV: &str = "THCLAWS_AISERVER_API";

pub fn env_var_for(provider: &str) -> Option<&'static str> {
    if provider == AISERVER {
        return Some(AISERVER_ENV);
    }
    ProviderKind::from_name(provider).and_then(|p| p.endpoint_env())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_only_exposes_user_configurable_endpoints() {
        let s = status();
        let providers: Vec<_> = s.iter().map(|e| e.provider).collect();
        // Self-hosted backend is editable; the Anthropic-compat variant is
        // deduped because both share OLLAMA_BASE_URL.
        assert!(providers.contains(&"ollama"));
        assert!(!providers.contains(&"ollama-anthropic"));
        // Hosted services are locked.
        assert!(!providers.contains(&"dashscope"));
    }

    #[test]
    fn the_ai_server_suite_has_an_address_row() {
        assert_eq!(env_var_for(AISERVER), Some("THCLAWS_AISERVER_API"));
        assert!(status().iter().any(|e| e.provider == AISERVER));
    }

    #[test]
    fn env_var_for_unknown_provider_is_none() {
        assert_eq!(env_var_for("anthropic"), None);
        assert_eq!(env_var_for("bogus"), None);
    }
}
