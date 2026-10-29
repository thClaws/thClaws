//! Shared-agent mode (cloud-only) — see dev-plan/41.
//!
//! When `THCLAWS_SHARED_AGENT_DIR` points at a mounted, read-only
//! "company brain", the engine runs a **shared agent**: instructions,
//! KMS, skills/commands, and MCP *config* come from that directory and
//! are locked; BYOK is refused (gateway-only); the member's private
//! working dir keeps sessions, files, MCP credentials, and their own
//! additive KMS/MCP.
//!
//! The security boundary lives here in code, not in filesystem
//! permissions — settings/instruction *layering* would otherwise let a
//! member smuggle overrides in via user scope. So the loaders for
//! instructions (`context.rs`), config (`config.rs`), and KMS
//! (`kms.rs`) all consult these helpers and hard-ignore member-scope
//! sources when shared mode is active.

use std::path::PathBuf;

const SHARED_DIR_ENV: &str = "THCLAWS_SHARED_AGENT_DIR";
const STRICT_ENV: &str = "THCLAWS_SHARED_STRICT";
const MODEL_LOCKED_ENV: &str = "THCLAWS_SHARED_MODEL_LOCKED";

/// Provider segments forced through the gateway in shared mode. Mirrors
/// the gateway-routable set in `providers::thclaws_gateway`. Anything not
/// listed (ollama, lmstudio, …) has no gateway route and is simply
/// unavailable to a shared agent — by design, shared agents are
/// gateway-billed only.
///
/// These are exactly the ten `ProviderTier::Featured` providers: what the
/// gateway sells is what it proxies. qwen-cloud / thaillm / groq were
/// dropped 2026-08-10 — see `thclaws_gateway::provider_segment` for why.
/// They remain fully usable with the user's own key.
pub const GATEWAY_ALL_PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "google",
    "openrouter",
    "dashscope",
    "zai",
    "deepseek",
    "minimax",
    "xai",
    "moonshot",
];

const GATEWAY_PROVIDERS_ENV: &str = "THCLAWS_GATEWAY_PROVIDERS";

/// The provider segments to route through the gateway. `THCLAWS_GATEWAY_PROVIDERS`
/// (comma list), when set, REPLACES [`GATEWAY_ALL_PROVIDERS`]: an install
/// whose gateway serves one upstream only — its own model workspace, e.g.
/// `sis` — sets exactly that, and the provisioner passes it to the runner.
/// Unset or blank = the compiled Featured set, which is thclaws.cloud and
/// every desktop.
pub fn gateway_routed_providers() -> Vec<String> {
    routed_for(
        crate::policy::thclaws_gateway(),
        std::env::var(GATEWAY_PROVIDERS_ENV).ok().as_deref(),
    )
}

/// A `kind: "thclaws"` gateway policy's provider list outranks
/// `THCLAWS_GATEWAY_PROVIDERS`, the way the policy outranks every setting.
fn routed_for(policy: Option<&crate::policy::GatewayPolicy>, env: Option<&str>) -> Vec<String> {
    match policy {
        Some(g) => parse_gateway_providers(Some(&g.providers.join(","))),
        None => parse_gateway_providers(env),
    }
}

/// True when this install runs through its organisation's own thClaws
/// gateway by policy — the desktop counterpart of a hosted runner's
/// `THCLAWS_USES_GATEWAY=1`.
pub fn policy_gateway_mode() -> bool {
    crate::policy::thclaws_gateway().is_some()
}

/// True when an organisation pays for this install's calls: an EE desktop on
/// its org gateway, or a hosted runner on a customer install (SIS, plan-09
/// tenants), which the api marks with a gateway provider list or its brand.
/// Stock thclaws.cloud sends neither, so its users still opt in per workspace.
pub fn org_billed() -> bool {
    let set = |k: &str| std::env::var(k).is_ok_and(|v| !v.trim().is_empty());
    policy_gateway_mode()
        || (std::env::var("THCLAWS_USES_GATEWAY").ok().as_deref() == Some("1")
            && (gateway_providers_locked() || set("THCLAWS_BRAND_NAME")))
}

/// True when `THCLAWS_GATEWAY_PROVIDERS` names at least one provider: an
/// install locked to its gateway. There, nothing counts as having its own
/// credentials — not the keyless local runtimes, not the Agent SDK — so the
/// picker offers only what the gateway routes and `build_provider` refuses
/// anything else. Unset = today's behaviour everywhere.
pub fn gateway_providers_locked() -> bool {
    locked_for(
        crate::policy::thclaws_gateway(),
        std::env::var(GATEWAY_PROVIDERS_ENV).ok().as_deref(),
    )
}

fn locked_for(policy: Option<&crate::policy::GatewayPolicy>, env: Option<&str>) -> bool {
    policy.is_some() || locked_by(env)
}

fn locked_by(raw: Option<&str>) -> bool {
    raw.is_some_and(|v| v.split(',').any(|s| !s.trim().is_empty()))
}

fn parse_gateway_providers(raw: Option<&str>) -> Vec<String> {
    let listed: Vec<String> = raw
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if listed.is_empty() {
        GATEWAY_ALL_PROVIDERS
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        listed
    }
}

/// The shared-brain directory when shared mode is active, else `None`.
/// Read fresh each call (cheap); the env var is set once at pod start.
pub fn shared_agent_dir() -> Option<PathBuf> {
    match std::env::var(SHARED_DIR_ENV) {
        Ok(v) if !v.trim().is_empty() => Some(PathBuf::from(v.trim())),
        _ => None,
    }
}

/// True when shared-agent mode is active.
pub fn is_active() -> bool {
    shared_agent_dir().is_some()
}

fn flag(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref().map(str::trim),
        Some("1") | Some("true")
    )
}

/// Strict mode: members can't even *add* private skills/MCP/KMS on top of
/// the shared brain (fully deterministic agent). Only meaningful when
/// shared mode is active.
pub fn is_strict() -> bool {
    is_active() && flag(STRICT_ENV)
}

/// Model is pinned by the company — members can't switch.
pub fn is_model_locked() -> bool {
    is_active() && flag(MODEL_LOCKED_ENV)
}

/// Locked instructions file inside the shared brain (`$SHARED/AGENTS.md`).
pub fn shared_agents_md() -> Option<PathBuf> {
    shared_agent_dir().map(|d| d.join("AGENTS.md"))
}

/// Read-only company settings base (`$SHARED/settings.json`).
pub fn shared_settings_json() -> Option<PathBuf> {
    shared_agent_dir().map(|d| d.join("settings.json"))
}

/// Read-only MCP config (`$SHARED/mcp.json`); credentials stay per-member.
pub fn shared_mcp_json() -> Option<PathBuf> {
    shared_agent_dir().map(|d| d.join("mcp.json"))
}

/// Shared KMS root (`$SHARED/kms`, read-only).
pub fn shared_kms_root() -> Option<PathBuf> {
    shared_agent_dir().map(|d| d.join("kms"))
}

/// Shared skills dir (`$SHARED/skills`, read-only).
pub fn shared_skills_dir() -> Option<PathBuf> {
    shared_agent_dir().map(|d| d.join("skills"))
}

/// Shared slash-commands dir (`$SHARED/commands`, read-only).
pub fn shared_commands_dir() -> Option<PathBuf> {
    shared_agent_dir().map(|d| d.join("commands"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_providers_default_to_the_featured_set() {
        let all: Vec<String> = GATEWAY_ALL_PROVIDERS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(parse_gateway_providers(None), all);
        assert_eq!(parse_gateway_providers(Some("")), all);
        // A stray comma must not collapse the set to nothing.
        assert_eq!(parse_gateway_providers(Some(" , ")), all);
    }

    #[test]
    fn locked_only_when_a_provider_is_named() {
        assert!(!locked_by(None));
        assert!(!locked_by(Some("")));
        assert!(!locked_by(Some(" , ")));
        assert!(locked_by(Some("sis")));
        assert!(locked_by(Some(",sis,")));
    }

    fn thclaws_policy(providers: &[&str]) -> crate::policy::GatewayPolicy {
        crate::policy::GatewayPolicy {
            enabled: true,
            url: "https://gateway.sis.example".into(),
            kind: Some("thclaws".into()),
            providers: providers.iter().map(|p| p.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_thclaws_policy_locks_over_whatever_env_says() {
        let g = thclaws_policy(&["sis"]);
        assert_eq!(routed_for(Some(&g), Some("openai,anthropic")), vec!["sis"]);
        assert_eq!(routed_for(Some(&g), None), vec!["sis"]);
        assert!(locked_for(Some(&g), None));
        assert!(locked_for(Some(&g), Some("")));
    }

    #[test]
    fn without_a_policy_the_env_decides_as_before() {
        let all: Vec<String> = GATEWAY_ALL_PROVIDERS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(routed_for(None, None), all);
        assert_eq!(routed_for(None, Some("sis")), vec!["sis"]);
        assert!(!locked_for(None, None));
        assert!(locked_for(None, Some("sis")));
    }

    #[test]
    fn gateway_providers_env_replaces_the_set() {
        assert_eq!(parse_gateway_providers(Some("sis")), vec!["sis"]);
        assert_eq!(
            parse_gateway_providers(Some(" SIS , dashscope,")),
            vec!["sis", "dashscope"]
        );
    }

    /// Reuse the crate-wide env lock so shared-mode tests serialise with
    /// the kms/context tests that also mutate `THCLAWS_SHARED_AGENT_DIR`
    /// (and HOME/cwd) — a separate lock would race against those.
    use crate::kms::test_env_lock as env_lock;

    #[test]
    fn inactive_by_default() {
        let _g = env_lock();
        std::env::remove_var(SHARED_DIR_ENV);
        assert!(!is_active());
        assert!(shared_agent_dir().is_none());
        assert!(!is_strict());
        assert!(!is_model_locked());
    }

    #[test]
    fn active_with_dir_and_flags() {
        let _g = env_lock();
        std::env::set_var(SHARED_DIR_ENV, "/shared-agent");
        std::env::set_var(STRICT_ENV, "1");
        std::env::set_var(MODEL_LOCKED_ENV, "true");
        assert!(is_active());
        assert_eq!(shared_agent_dir(), Some(PathBuf::from("/shared-agent")));
        assert_eq!(
            shared_agents_md(),
            Some(PathBuf::from("/shared-agent/AGENTS.md"))
        );
        assert_eq!(shared_kms_root(), Some(PathBuf::from("/shared-agent/kms")));
        assert!(is_strict());
        assert!(is_model_locked());
        // Flags are inert when shared mode is off.
        std::env::remove_var(SHARED_DIR_ENV);
        assert!(!is_strict());
        assert!(!is_model_locked());
        std::env::remove_var(STRICT_ENV);
        std::env::remove_var(MODEL_LOCKED_ENV);
    }

    #[test]
    fn empty_dir_is_inactive() {
        let _g = env_lock();
        std::env::set_var(SHARED_DIR_ENV, "   ");
        assert!(!is_active());
        std::env::remove_var(SHARED_DIR_ENV);
    }
}
