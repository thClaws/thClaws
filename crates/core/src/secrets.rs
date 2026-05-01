//! OS-keychain–backed storage for LLM API keys.
//!
//! Backend: macOS Keychain, Windows Credential Manager, or Linux Secret
//! Service (via the `keyring` crate). Service name is `"thclaws"`, account
//! is the provider short name (`"agentic-press"`, `"anthropic"`, `"openai"`,
//! `"gemini"`, `"dashscope"`).
//!
//! Keys are decrypted only when read. At startup [`load_into_env`] pulls
//! any stored keys and sets the matching env var *if it isn't already
//! set*, preserving the precedence: shell export > keychain > dotenv file.

use crate::error::{Error, Result};
use crate::providers::ProviderKind;

const SERVICE: &str = "thclaws";
/// Single-item account name used by the "one bundle, one ACL" layout.
/// All provider keys live inside one JSON blob at
/// service="thclaws", account="api-keys" so macOS only asks once for
/// the whole set. Legacy per-provider entries are still migrated
/// lazily into the bundle on first read for backwards compat.
const BUNDLE_ACCOUNT: &str = "api-keys";

/// Where the user has chosen to store API keys. Picked once via the
/// Settings UI's initial prompt and persisted so we never ping the OS
/// keychain for users who opted out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Keychain,
    Dotenv,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Keychain => "keychain",
            Backend::Dotenv => "dotenv",
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct BackendFile {
    backend: Option<Backend>,
}

fn backend_path() -> Option<std::path::PathBuf> {
    crate::util::home_dir().map(|h| h.join(".config/thclaws/secrets.json"))
}

/// Read the user's stored backend preference. `None` means the user
/// hasn't been asked yet.
pub fn get_backend() -> Option<Backend> {
    let path = backend_path()?;
    let contents = std::fs::read_to_string(&path).ok()?;
    let parsed: BackendFile = serde_json::from_str(&contents).ok()?;
    parsed.backend
}

/// Persist the user's choice. Callers should invoke this before the
/// first `set` / `get` so keychain prompts are skipped for dotenv
/// users.
pub fn set_backend(backend: Backend) -> Result<()> {
    let path =
        backend_path().ok_or_else(|| Error::Config("cannot locate user home directory".into()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = BackendFile {
        backend: Some(backend),
    };
    let json = serde_json::to_string_pretty(&file)
        .map_err(|e| Error::Config(format!("serialize: {e}")))?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// Resolve the backend to use *right now*. When the user hasn't
/// picked yet we return `Dotenv`, which effectively blocks every
/// keychain-touching path (`get` / `set` / `clear` / `status` /
/// `load_into_env`) until the Settings UI prompts the user to
/// choose. This is the whole point — a fresh launch must not cause
/// keychain access prompts.
fn resolved_backend() -> Backend {
    get_backend().unwrap_or(Backend::Dotenv)
}

/// Providers whose keys thClaws manages in the keychain.
///
/// Excludes `Ollama`, `OllamaAnthropic`, `AgentSdk` (no API key), and the
/// variants that share an env var with another provider (we dedupe by env
/// var name in the UI layer).
const MANAGED: &[ProviderKind] = &[
    ProviderKind::AgenticPress,
    ProviderKind::Anthropic,
    ProviderKind::OpenAI,
    ProviderKind::OpenRouter,
    ProviderKind::Gemini,
    ProviderKind::DashScope,
    ProviderKind::OllamaCloud,
    ProviderKind::ZAi,
    ProviderKind::AzureAIFoundry,
    ProviderKind::OpenAICompat,
    ProviderKind::DeepSeek,
    ProviderKind::ThaiLLM,
];

fn entry(provider: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, provider)
        .map_err(|e| Error::Config(format!("keychain open failed: {e}")))
}

fn bundle_entry() -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, BUNDLE_ACCOUNT)
        .map_err(|e| Error::Config(format!("keychain open failed: {e}")))
}

/// In-memory cache of the bundle contents for the lifetime of this
/// process. Populated on the first read; subsequent `get` / `set`
/// calls never touch the keychain again, so N providers = 1 macOS
/// prompt per launch.
fn bundle_cache() -> &'static std::sync::RwLock<Option<std::collections::HashMap<String, String>>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<std::sync::RwLock<Option<std::collections::HashMap<String, String>>>> =
        OnceLock::new();
    CACHE.get_or_init(|| std::sync::RwLock::new(None))
}

/// Read (and cache) the bundle. On first access, also migrate any
/// legacy per-provider entries into the bundle so users who set keys
/// before this change don't lose them.
fn read_bundle() -> std::collections::HashMap<String, String> {
    {
        let guard = bundle_cache().read().unwrap();
        if let Some(map) = &*guard {
            return map.clone();
        }
    }
    let mut map: std::collections::HashMap<String, String> =
        match bundle_entry().ok().and_then(|e| e.get_password().ok()) {
            Some(json) => serde_json::from_str(&json).unwrap_or_default(),
            None => std::collections::HashMap::new(),
        };
    // Migrate legacy per-provider entries. One-shot per process.
    let mut migrated = false;
    for p in MANAGED {
        let name = p.name();
        if map.contains_key(name) {
            continue;
        }
        if let Some(key) = entry(name).ok().and_then(|e| e.get_password().ok()) {
            map.insert(name.to_string(), key);
            migrated = true;
        }
    }
    if migrated {
        let _ = write_bundle(&map);
    }
    *bundle_cache().write().unwrap() = Some(map.clone());
    map
}

fn write_bundle(map: &std::collections::HashMap<String, String>) -> Result<()> {
    let json =
        serde_json::to_string(map).map_err(|e| Error::Config(format!("serialize bundle: {e}")))?;
    bundle_entry()?
        .set_password(&json)
        .map_err(|e| Error::Config(format!("keychain write failed: {e}")))?;
    *bundle_cache().write().unwrap() = Some(map.clone());
    Ok(())
}

/// Store a key in the keychain. Respects the user's backend choice —
/// when they picked `Dotenv`, returns an error up front instead of
/// touching the keychain (which would trigger an OS prompt on macOS
/// even if we then fall back). All provider keys live inside a
/// single keychain entry so the user only sees one ACL prompt per
/// launch regardless of how many providers they've configured.
pub fn set(provider: &str, key: &str) -> Result<()> {
    if resolved_backend() == Backend::Dotenv {
        return Err(Error::Config("keychain disabled by user preference".into()));
    }
    let mut map = read_bundle();
    map.insert(provider.to_string(), key.to_string());
    write_bundle(&map)
}

/// Retrieve a key from the keychain. Goes through the single
/// `api-keys` bundle entry (cached for the lifetime of the process)
/// so N providers = 1 macOS prompt per launch.
pub fn get(provider: &str) -> Option<String> {
    if std::env::var("THCLAWS_DISABLE_KEYCHAIN").is_ok() {
        log_trace(&format!(
            "get({provider}) → blocked by THCLAWS_DISABLE_KEYCHAIN"
        ));
        return None;
    }
    if resolved_backend() == Backend::Dotenv {
        log_trace(&format!(
            "get({provider}) → backend=dotenv, skipping keychain"
        ));
        return None;
    }
    log_trace(&format!("get({provider}) → bundle lookup"));
    let map = read_bundle();
    let result = map.get(provider).cloned();
    log_trace(&format!(
        "get({provider}) → returned {}",
        if result.is_some() { "<key>" } else { "None" }
    ));
    result
}

fn log_trace(msg: &str) {
    if std::env::var("THCLAWS_KEYCHAIN_TRACE").is_ok() {
        let pid = std::process::id();
        let loaded = std::env::var("THCLAWS_KEYCHAIN_LOADED").is_ok();
        eprintln!("\x1b[35m[keychain pid={pid} loaded_flag={loaded}] {msg}\x1b[0m");
    }
}

/// Delete a provider's key from the keychain bundle. Also cleans up
/// the legacy per-provider entry if it still exists. Silently
/// succeeds when neither store has anything to remove.
pub fn clear(provider: &str) -> Result<()> {
    if resolved_backend() == Backend::Dotenv {
        return Ok(());
    }
    let mut map = read_bundle();
    map.remove(provider);
    if map.is_empty() {
        if let Ok(entry) = bundle_entry() {
            let _ = entry.delete_credential();
        }
        *bundle_cache().write().unwrap() = Some(map);
    } else {
        write_bundle(&map)?;
    }
    // Legacy single-item cleanup — harmless if already absent.
    if let Ok(entry) = entry(provider) {
        let _ = entry.delete_credential();
    }
    Ok(())
}

/// Source of an env var's current value, for UI display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// Env var is set but we can't tell where from (shell export, previous
    /// dotenv load, or prior keychain injection).
    Environment,
    /// No key is configured anywhere.
    None,
}

/// Status entry for one provider, suitable for rendering in the UI.
#[derive(Debug, Clone)]
pub struct KeyStatus {
    pub provider: &'static str,
    pub env_var: &'static str,
    pub configured_in_keychain: bool,
    pub env_source: KeySource,
    /// Length of the effective key in the process env (0 if unset). The UI
    /// renders an asterisk string of this length as a sentinel, giving the
    /// user a visual confirmation that a real key is loaded without ever
    /// exposing its contents.
    pub key_length: usize,
}

/// Snapshot the current state of every managed provider's API key. Used by
/// the UI to render "configured / not configured" and decide button state.
pub fn status() -> Vec<KeyStatus> {
    MANAGED
        .iter()
        .filter_map(|p| {
            let env_var = p.api_key_env()?;
            let configured = get(p.name()).is_some();
            let env_value = std::env::var(env_var).ok();
            let env_source = if env_value.is_some() {
                KeySource::Environment
            } else {
                KeySource::None
            };
            Some(KeyStatus {
                provider: p.name(),
                env_var,
                configured_in_keychain: configured,
                env_source,
                key_length: env_value.as_deref().map(str::len).unwrap_or(0),
            })
        })
        .collect()
}

/// Inject keychain-stored keys into the process environment, skipping any
/// env var that's already set. Call **before** [`crate::dotenv::load_dotenv`]
/// so the precedence shakes out as: shell export > keychain > dotenv.
pub fn load_into_env() {
    log_trace("load_into_env() enter");
    match get_backend() {
        Some(Backend::Keychain) => {}
        other => {
            log_trace(&format!(
                "load_into_env() → backend={:?}, returning early",
                other
            ));
            return;
        }
    }
    if std::env::var("THCLAWS_KEYCHAIN_LOADED").is_ok() {
        log_trace("load_into_env() → THCLAWS_KEYCHAIN_LOADED already set, returning early");
        return;
    }
    log_trace("load_into_env() → walking MANAGED providers");
    for p in MANAGED {
        let Some(env_var) = p.api_key_env() else {
            continue;
        };
        if std::env::var(env_var).is_ok() {
            log_trace(&format!(
                "load_into_env() → {} already in env, skip",
                env_var
            ));
            continue;
        }
        if let Some(key) = get(p.name()) {
            std::env::set_var(env_var, key);
        }
    }
    std::env::set_var("THCLAWS_KEYCHAIN_LOADED", "1");
    log_trace("load_into_env() → done, exported THCLAWS_KEYCHAIN_LOADED=1");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lists_known_providers() {
        let s = status();
        let names: Vec<_> = s.iter().map(|k| k.provider).collect();
        assert!(names.contains(&"agentic-press"));
        assert!(names.contains(&"anthropic"));
        assert!(names.contains(&"openai"));
        assert!(names.contains(&"gemini"));
        assert!(names.contains(&"dashscope"));
        assert!(names.contains(&"openai-compat"));
    }
}
