//! Org policy file — the foundation every Enterprise Edition control
//! sits on. See `dev-plan/01-enterprise-edition.md` for the full design.
//!
//! ## Architectural principle
//!
//! The policy file is a *gate*, not a feature. Without one, thClaws
//! behaves exactly as today (open-core UX, no enforcement). Present a
//! verified policy file and the `policies.*` blocks selectively turn
//! enforcement on. Present an *un*verified one and the binary refuses
//! to start — silent fallback would defeat the point.
//!
//! ## Resolution flow
//!
//! 1. `KeySource::resolve()` — find a verification key (compile-time
//!    embed wins, falls back to env var, ultimately `None`).
//! 2. `Policy::find_file()` — search `THCLAWS_POLICY_FILE`, then
//!    `/etc/thclaws/policy.json`, then `~/.config/thclaws/policy.json`.
//! 3. If a file exists: parse → verify signature → check binding →
//!    check expiry → return `ActivePolicy`.
//! 4. If anything fails → `PolicyError`. The startup wrapper prints
//!    `refuse_message()` and exits non-zero.
//! 5. If no file exists: return `Ok(None)`. Today's behavior.
//!
//! ## What lives here vs elsewhere
//!
//! This module owns: file format, signature verification, expiry/binding
//! checks, the `ActivePolicy` accessor that other features read at
//! decision points. It does NOT own: enforcement of any specific policy
//! (branding, allow-list, gateway, SSO) — those live in their respective
//! modules and *consult* `policy::active()` to see whether to apply.

pub mod allowlist;
pub mod error;
pub mod verify;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

pub use allowlist::{check_url, AllowDecision};
pub use error::PolicyError;
pub use verify::{KeySource, EMBEDDED_PUBKEY_BASE64};

/// The signed policy baked in at build time. Empty when none was
/// embedded — the open-core default.
pub const EMBEDDED_POLICY_JSON_BASE64: &str = env!("THCLAWS_EMBEDDED_POLICY_JSON");

/// `true` when this build must have a policy to run. Set by `build.rs`
/// when the binary carries both a key and a policy (a per-customer
/// build), or when `THCLAWS_REQUIRE_POLICY=1` forces it. A maintainer's
/// local build, which picks up only `policy.pub`, stays runnable.
pub const POLICY_REQUIRED: bool = matches!(env!("THCLAWS_POLICY_REQUIRED").as_bytes(), b"1");

/// Policy schema version this build understands. Forward-compat guard:
/// a policy declaring a higher version refuses to load rather than
/// silently skipping unknown blocks.
pub const SUPPORTED_VERSION: u32 = 1;

/// Top-level policy document, parsed from JSON. The `signature` field
/// is checked separately by `verify::verify_policy`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub version: u32,
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub issued_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<Binding>,
    #[serde(default)]
    pub policies: Policies,
    /// Base64-encoded Ed25519 signature over the canonical-JSON form
    /// of this document with the `signature` field removed. Required
    /// for verification — `MissingSignature` if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Binding {
    /// Human-readable org id. Logged at startup so misdeployments are
    /// visible in support diagnostics.
    #[serde(default)]
    pub org_id: String,
    /// Optional binary fingerprint (e.g. `sha256:...`). When set, must
    /// match the running binary's fingerprint or the policy refuses
    /// to apply. Prevents lifting a customer policy onto a non-customer
    /// build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_fingerprint: Option<String>,
}

/// Per-feature policy blocks. Each has `enabled: bool` so the policy
/// file can selectively activate features without forcing all of them.
/// Disabled or omitted blocks fall back to open-core default behavior.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Policies {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branding: Option<BrandingPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<GatewayPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sso: Option<SsoPolicy>,
    /// Phase 5 — client-side tool-call audit (RFC 0001).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<AuditPolicy>,
    /// Phase 8 — what the agent and the binary may do on this machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimePolicy>,
    /// Phase 6 — installs come only from the org's private marketplace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marketplace: Option<MarketplacePolicy>,
}

/// Skills, plugins, subagents and MCP servers install only by name from
/// the catalogue at `url` (the engine's marketplace.json schema), never
/// from a URL or a path. Unreachable catalogue = nothing installable.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MarketplacePolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub url: String,
}

/// Phase 8 — the block that says *no*.
///
/// Every other block configures where thClaws points: which brand,
/// which gateway, which IdP, which audit sink. This one restricts what
/// it may do at all, which is the question an admin actually asks
/// first. Before it existed, `audit` could record that a user opened a
/// cloud tunnel or ran `Bash`, but nothing could prevent either — and a
/// user editing their own `settings.json` could always return to
/// auto-approve.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimePolicy {
    #[serde(default)]
    pub enabled: bool,
    /// Force the permission mode: `ask`, `auto` or `plan`. Applied
    /// after settings and after CLI flags, so `--permission-mode auto`
    /// cannot climb over it. Unset leaves the user's own choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Tool names the agent may not use. Removed from every registry
    /// so the model never sees them, and refused again at dispatch in
    /// case a registry was built somewhere this did not reach.
    #[serde(default)]
    pub deny_tools: Vec<String>,
    /// thClaws Remote — the tunnel that makes this machine's agent
    /// reachable from the cloud. Default true (today's behaviour); set
    /// false and no pairing or reconnect can start.
    #[serde(default = "default_true")]
    pub allow_remote: bool,
    /// `--serve`: the HTTP server exposing the web UI and the
    /// OpenAI-compatible API. Default true; set false and the binary
    /// refuses to bind.
    #[serde(default = "default_true")]
    pub allow_serve: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuditPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub sinks: Vec<AuditSinkConfig>,
    /// Emit the bounded `summary` field. Default true.
    #[serde(default = "default_true")]
    pub include_summary: bool,
    /// Add `x-thclaws-session` / `x-thclaws-turn` to provider requests
    /// so gateway-side logs join on the same keys. Default true.
    #[serde(default = "default_true")]
    pub correlate_gateway: bool,
}

fn default_batch() -> usize {
    50
}

fn default_flush_secs() -> u64 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuditSinkConfig {
    File {
        /// strftime tokens allowed (`%Y-%m-%d`). `None` → daily file
        /// under the data dir.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth_header_template: Option<String>,
        #[serde(default = "default_batch")]
        batch: usize,
        #[serde(default = "default_flush_secs")]
        flush_secs: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BrandingPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginsPolicy {
    #[serde(default)]
    pub enabled: bool,
    /// Wildcard host patterns. Empty list with `enabled: true` means
    /// "no external sources allowed at all" — useful for paranoid
    /// air-gapped deployments.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// `false` rejects skills that ship executable `scripts/` dirs.
    /// Defaults to `true`; set it explicitly to lock scripts out.
    #[serde(default = "default_true")]
    pub allow_external_scripts: bool,
    /// `false` refuses stdio MCP servers and HTTP ones whose endpoint
    /// isn't in `allowed_hosts`. Defaults to `true` (no MCP restriction).
    #[serde(default = "default_true")]
    pub allow_external_mcp: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GatewayPolicy {
    #[serde(default)]
    pub enabled: bool,
    /// Replacement base URL. All provider HTTP calls route here.
    #[serde(default)]
    pub url: String,
    /// Header template. `{{sso_token}}` substituted from the active
    /// SSO session (Phase 4); literal text otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_header_template: Option<String>,
    /// When true, any HTTP request that doesn't match the gateway
    /// host is blocked. When false, allows direct provider access
    /// (gateway becomes "preferred" not "required").
    #[serde(default = "default_true")]
    pub fail_closed: bool,
    /// Escape valve: when gateway is unreachable, allow local Ollama
    /// (read-only model) so users aren't completely blocked.
    #[serde(default)]
    pub read_only_local_models_allowed: bool,
    /// `None` / `"openai_compat"` = the generic gateway above (the provider
    /// is replaced by an OpenAI client at `url`). `"thclaws"` = the
    /// customer's own thClaws gateway: the install runs locked to it the
    /// way a hosted runner does (see [`thclaws_gateway`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `kind: "thclaws"` only — the provider segments the gateway serves,
    /// e.g. `["sis"]`. The same list `THCLAWS_GATEWAY_PROVIDERS` carries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,
    /// `kind: "thclaws"` only — the customer's cloud (sign-in, CLI token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_url: Option<String>,
    /// `kind: "thclaws"` only — the model a desktop starts on when its
    /// settings name one the gateway does not serve (e.g. the open-core
    /// default). Must be under one of `providers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}

pub const GATEWAY_KIND_THCLAWS: &str = "thclaws";
const GATEWAY_KIND_OPENAI_COMPAT: &str = "openai_compat";

impl GatewayPolicy {
    fn kind_str(&self) -> &str {
        self.kind
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .unwrap_or(GATEWAY_KIND_OPENAI_COMPAT)
    }

    pub fn is_thclaws(&self) -> bool {
        self.kind_str().eq_ignore_ascii_case(GATEWAY_KIND_THCLAWS)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SsoPolicy {
    #[serde(default)]
    pub enabled: bool,
    /// `oidc` (only supported value in v1).
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub issuer_url: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// Optional inline client_secret. Use **only** for "non-confidential"
    /// secrets — Google's docs explicitly classify Desktop-app
    /// client_secrets as not-actually-secret because they ship embedded
    /// in every binary copy:
    ///
    /// > In this context, the client secret is obviously not treated
    /// > as a secret.
    ///
    /// For these IdPs, embedding here is the recommended pattern: one
    /// signed policy file carries everything the enterprise needs to
    /// deploy, no separate env-var distribution required. **Do not
    /// embed real confidential-client secrets here** (Okta confidential,
    /// Auth0 production, Azure AD with secret) — those leak from the
    /// policy file to every workstation and a single dump compromises
    /// the OAuth project. Use `clientSecretEnv` for those.
    ///
    /// Resolution order at token-exchange time: `client_secret` →
    /// `client_secret_env` → none.
    #[serde(
        rename = "clientSecret",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_secret: Option<String>,
    /// Optional. Names an env var holding a client_secret for the
    /// token exchange. Use this when the secret is *truly* secret and
    /// shouldn't end up on workstations as plaintext (real confidential
    /// clients). The env var itself is deployed via MDM / login script
    /// / OS keychain alongside the binary, in the same channel as the
    /// signed policy file.
    ///
    /// Modern PKCE-only clients (Okta public-client setting, Azure AD
    /// desktop, Keycloak public clients) leave both this and
    /// `client_secret` unset — secret-less PKCE flow is used.
    #[serde(
        rename = "clientSecretEnv",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_secret_env: Option<String>,
}

fn default_true() -> bool {
    true
}

/// The result of loading a verified policy. Held in a `OnceLock` set
/// at startup so feature modules can read it cheaply (`policy::active()`).
#[derive(Debug, Clone)]
pub struct ActivePolicy {
    pub source_path: PathBuf,
    pub policy: Policy,
    /// Human-readable description of which key verified the active
    /// policy ("embedded", "env", or "file (path)"). For diagnostics —
    /// not load-bearing.
    pub key_source_label: String,
}

static ACTIVE: OnceLock<Option<ActivePolicy>> = OnceLock::new();

/// Read the active policy. Returns `None` if no policy file was loaded
/// at startup (today's open-core behavior). Cheap — no IO.
pub fn active() -> Option<&'static ActivePolicy> {
    ACTIVE.get().and_then(|opt| opt.as_ref())
}

/// The active `runtime` block, or `None` when no policy is loaded or
/// the block is absent / disabled. Every accessor below funnels through
/// this, so "no policy" and "policy without this block" behave
/// identically to open-core.
pub fn runtime() -> Option<&'static RuntimePolicy> {
    active()
        .and_then(|a| a.policy.policies.runtime.as_ref())
        .filter(|r| r.enabled)
}

/// The active `gateway` block when it is the customer's own thClaws
/// gateway (`kind: "thclaws"`). The single accessor every lock knob reads
/// first — the routed/locked provider list, the gateway base URL, the
/// cloud URL, gateway activation — so a policy-governed desktop behaves
/// like a hosted runner whatever its env or settings say.
pub fn thclaws_gateway() -> Option<&'static GatewayPolicy> {
    active()
        .and_then(|a| a.policy.policies.gateway.as_ref())
        .filter(|g| g.enabled && g.is_thclaws())
}

/// The policy's thClaws gateway base URL, trailing slash trimmed.
pub fn thclaws_gateway_url() -> Option<String> {
    thclaws_gateway()
        .map(|g| g.url.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
}

/// The policy's cloud URL (sign-in / CLI token), trailing slash trimmed.
pub fn thclaws_cloud_url() -> Option<String> {
    thclaws_gateway()
        .and_then(|g| g.cloud_url.as_deref())
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
}

/// A provider failure that is really "can't reach the company gateway",
/// rewritten to tell the user what to do. `None` unless a thClaws-kind
/// gateway policy with `fail_closed` is active and `raw` is a network
/// failure (DNS, connect, timeout).
pub fn unreachable_gateway_message(raw: &str) -> Option<String> {
    let g = thclaws_gateway().filter(|g| g.fail_closed)?;
    network_failure_hint(raw, &g.url)
}

fn network_failure_hint(raw: &str, gateway_url: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    let network = [
        "error sending request",
        "dns error",
        "failed to lookup address",
        "tcp connect error",
        "connection refused",
        "connection reset",
        "timed out",
        "operation timed out",
        "no route to host",
        "network is unreachable",
    ]
    .iter()
    .any(|m| lower.contains(m));
    if !network {
        return None;
    }
    let host = gateway_url
        .trim()
        .split("://")
        .last()
        .unwrap_or(gateway_url)
        .split(['/', ':'])
        .next()
        .unwrap_or(gateway_url);
    Some(format!(
        "Can't reach your organisation's AI gateway ({host}). Connect to the company network or VPN, then try again."
    ))
}

/// Permission mode the org forces, lowercased. `None` = user's choice.
pub fn forced_permission_mode() -> Option<String> {
    runtime()
        .and_then(|r| r.permission_mode.as_deref())
        .map(|m| m.trim().to_ascii_lowercase())
        .filter(|m| matches!(m.as_str(), "ask" | "auto" | "plan"))
}

/// Tool names the org denies. Empty when unrestricted.
pub fn denied_tools() -> Vec<String> {
    runtime().map(|r| r.deny_tools.clone()).unwrap_or_default()
}

/// Whether a given tool may run at all under the active policy.
pub fn tool_allowed(name: &str) -> bool {
    tool_allowed_in(&denied_tools(), name)
}

pub fn tool_allowed_in(denied: &[String], name: &str) -> bool {
    !denied.iter().any(|d| d.eq_ignore_ascii_case(name))
}

/// Error text for a tool the org denies, shared by every dispatch path.
pub fn tool_denied_message(name: &str) -> String {
    format!("tool '{name}' is disabled by your organisation's policy")
}

use crate::permissions::PermissionMode;

fn mode_rank(m: PermissionMode) -> u8 {
    match m {
        PermissionMode::Auto => 0,
        PermissionMode::Plan => 2,
        _ => 1,
    }
}

fn mode_from_forced(f: &str) -> Option<PermissionMode> {
    match f {
        "auto" => Some(PermissionMode::Auto),
        "ask" => Some(PermissionMode::Ask),
        "plan" => Some(PermissionMode::Plan),
        _ => None,
    }
}

/// Never let the live mode be looser than the org-forced one. Stricter is
/// kept (plan mode while `ask` is forced; channel-gated asks).
pub fn clamp_mode_to(forced: Option<&str>, m: PermissionMode) -> PermissionMode {
    match forced.and_then(mode_from_forced) {
        Some(f) if mode_rank(m) < mode_rank(f) => f,
        _ => m,
    }
}

/// `clamp_mode_to` against the active policy. Multiuser keeps its own
/// forced Auto: approvals can't be routed to one member there.
pub fn clamp_permission_mode(m: PermissionMode) -> PermissionMode {
    if crate::workdir::is_multiuser() {
        return m;
    }
    clamp_mode_to(forced_permission_mode().as_deref(), m)
}

/// Refusal for a user-initiated mode switch (`/permissions`, a shell's
/// mode selector) that would leave the org-forced mode.
pub fn permission_mode_refusal_for(
    forced: Option<&str>,
    requested: PermissionMode,
) -> Option<String> {
    let f = forced?;
    let fm = mode_from_forced(f)?;
    if mode_rank(requested) == mode_rank(fm) {
        return None;
    }
    Some(format!(
        "Permission mode is set by your organisation ('{f}') and can't be changed."
    ))
}

pub fn permission_mode_refusal(requested: PermissionMode) -> Option<String> {
    permission_mode_refusal_for(forced_permission_mode().as_deref(), requested)
}

/// Whether thClaws Remote may connect.
pub fn remote_allowed() -> bool {
    runtime().map(|r| r.allow_remote).unwrap_or(true)
}

/// Whether `--serve` may bind.
pub fn serve_allowed() -> bool {
    runtime().map(|r| r.allow_serve).unwrap_or(true)
}

/// Convenience: which `KeySource` label the active policy was verified
/// against. `"none"` when no policy is active.
pub fn key_source_label() -> String {
    active()
        .map(|a| a.key_source_label.clone())
        .unwrap_or_else(|| "none".to_string())
}

/// `true` when a policy is active AND `policies.plugins.enabled: true`
/// AND `allow_external_scripts: false`. Callers (skill installer + load
/// path) consult this to decide whether to reject script-bearing skills.
pub fn external_scripts_disallowed() -> bool {
    active()
        .and_then(|a| a.policy.policies.plugins.as_ref())
        .map(|p| p.enabled && !p.allow_external_scripts)
        .unwrap_or(false)
}

/// `true` when a policy is active AND `policies.plugins.enabled: true`
/// AND `allow_external_mcp: false`. Callers (MCP loader) consult this
/// to decide whether to apply the host allow-list to HTTP MCP servers.
pub fn external_mcp_disallowed() -> bool {
    active()
        .and_then(|a| a.policy.policies.plugins.as_ref())
        .map(|p| p.enabled && !p.allow_external_mcp)
        .unwrap_or(false)
}

/// Env pair a hosted runner gets from its provisioner. It can only
/// tighten: an open-core user who sets it locks only themselves.
pub const ENV_INSTALL_SOURCE: &str = "THCLAWS_INSTALL_SOURCE";
pub const ENV_MARKETPLACE_URL: &str = "THCLAWS_MARKETPLACE_URL";

/// `Some(catalogue url)` when installs are restricted to a private
/// marketplace — from the signed policy, else from the runner env. An
/// empty url still locks (nothing installable) rather than falling open.
pub fn marketplace_only_url() -> Option<String> {
    if let Some(m) = active()
        .and_then(|a| a.policy.policies.marketplace.as_ref())
        .filter(|m| m.enabled)
    {
        return Some(m.url.trim().to_string());
    }
    marketplace_only_from_env(
        std::env::var(ENV_INSTALL_SOURCE).ok().as_deref(),
        std::env::var(ENV_MARKETPLACE_URL).ok().as_deref(),
    )
}

pub fn marketplace_only_from_env(source: Option<&str>, url: Option<&str>) -> Option<String> {
    source
        .filter(|s| s.trim().eq_ignore_ascii_case("marketplace"))
        .map(|_| url.unwrap_or("").trim().to_string())
}

/// Why the active policy refuses this MCP server, or `None` when it may
/// run. Checked where a server is saved and again where it is started,
/// so `/mcp add`, marketplace installs, plugin-contributed servers and
/// hand-edited mcp.json all meet the same rule.
pub fn mcp_server_refusal(cfg: &crate::mcp::McpServerConfig) -> Option<String> {
    if let Some(reason) = active().and_then(|a| mcp_server_refusal_with(cfg, &a.policy)) {
        return Some(reason);
    }
    if marketplace_only_url().is_some()
        && !cfg.engine_managed
        && !crate::marketplace::mcp_server_listed(cfg)
    {
        return Some(format!(
            "mcp '{}' blocked: only MCP servers from your organisation's marketplace may run",
            cfg.name
        ));
    }
    None
}

/// With `allow_external_mcp: false`, an HTTP server must match
/// `allowed_hosts` and a stdio server is refused outright: its command
/// typically fetches a package from npm or PyPI, which no host rule can
/// see. Engine-managed servers are the engine's own choice, not an
/// install, and pass.
pub fn mcp_server_refusal_with(
    cfg: &crate::mcp::McpServerConfig,
    policy: &Policy,
) -> Option<String> {
    let plugins = policy.policies.plugins.as_ref()?;
    if !plugins.enabled || plugins.allow_external_mcp || cfg.engine_managed {
        return None;
    }
    if cfg.transport == "http" {
        return match allowlist::check_url_with(&cfg.url, policy) {
            AllowDecision::Denied { reason } => Some(format!(
                "mcp '{}' blocked by org policy: {reason}",
                cfg.name
            )),
            _ => None,
        };
    }
    Some(format!(
        "mcp '{}' blocked by org policy: stdio MCP servers are disabled (allow_external_mcp: false)",
        cfg.name
    ))
}

/// Startup entry point. Call once, before `AppConfig::load()`. On
/// success, populates `ACTIVE` and returns whether a policy was loaded.
/// On failure, returns the error — caller prints `refuse_message()`
/// and exits non-zero.
pub fn load_or_refuse() -> Result<bool, PolicyError> {
    let key_source = KeySource::resolve()?;
    // Source order: a file on disk wins, so an org can rotate policy by
    // re-signing and redistributing one file — no rebuild. The embedded
    // copy is the fallback for a deployment with no endpoint management,
    // where the file may never have been placed or may have been deleted.
    // Both go through the identical signature / expiry / binding checks
    // below; embedding is a delivery mechanism, not a shortcut.
    let (path, body) = match find_file() {
        Some(p) => {
            let body = std::fs::read_to_string(&p).map_err(|e| PolicyError::Io {
                path: p.clone(),
                source: e,
            })?;
            (p, body)
        }
        None => match embedded_policy_json() {
            Some(body) => (PathBuf::from("<built-in>"), body),
            None => {
                if POLICY_REQUIRED {
                    return Err(PolicyError::PolicyRequired);
                }
                // Open-core: no policy anywhere is the normal state.
                // Cache `None` so `active()` skips the search next time.
                let _ = ACTIVE.set(None);
                return Ok(false);
            }
        },
    };
    let raw: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| PolicyError::InvalidJson {
            path: path.clone(),
            source: e,
        })?;
    let _ = verify::verify_policy(&raw, &key_source, &path)?;
    let policy: Policy = serde_json::from_value(raw).map_err(|e| PolicyError::InvalidJson {
        path: path.clone(),
        source: e,
    })?;
    if policy.version > SUPPORTED_VERSION {
        return Err(PolicyError::UnsupportedVersion {
            path,
            got: policy.version,
            supported: SUPPORTED_VERSION,
        });
    }
    if let Some(exp) = &policy.expires_at {
        if is_expired(exp) {
            return Err(PolicyError::Expired {
                path,
                expires_at: exp.clone(),
            });
        }
    }
    if let Some(binding) = &policy.binding {
        if let Some(expected_fp) = &binding.binary_fingerprint {
            let actual = binary_fingerprint();
            if !fingerprint_matches(expected_fp, &actual) {
                return Err(PolicyError::BindingMismatch {
                    path,
                    expected: expected_fp.clone(),
                });
            }
        }
    }
    validate_policies(&policy, &path)?;
    let active = ActivePolicy {
        source_path: path,
        policy,
        key_source_label: key_source.label(),
    };
    let _ = ACTIVE.set(Some(active));
    Ok(true)
}

/// Cross-check enabled sub-policies against their required fields.
/// Catches misconfigurations that would silently fail open at runtime —
/// e.g. `gateway.enabled: true` with `gateway.url` empty. Returns
/// `Err(PolicyError::InvalidConfig)` so the binary refuses to start
/// with a clear message naming the bad field.
fn validate_policies(policy: &Policy, path: &PathBuf) -> Result<(), PolicyError> {
    if let Some(g) = &policy.policies.gateway {
        if g.enabled && g.url.trim().is_empty() {
            return Err(PolicyError::InvalidConfig {
                path: path.clone(),
                message: "gateway.enabled but gateway.url is empty — would fail open at provider construction".into(),
            });
        }
        let kind = g.kind_str();
        if !kind.eq_ignore_ascii_case(GATEWAY_KIND_THCLAWS)
            && !kind.eq_ignore_ascii_case(GATEWAY_KIND_OPENAI_COMPAT)
        {
            return Err(PolicyError::InvalidConfig {
                path: path.clone(),
                message: format!(
                    "gateway.kind '{kind}' is not known — use \"{GATEWAY_KIND_THCLAWS}\" or \"{GATEWAY_KIND_OPENAI_COMPAT}\""
                ),
            });
        }
        if g.enabled && g.is_thclaws() && !g.providers.iter().any(|p| !p.trim().is_empty()) {
            return Err(PolicyError::InvalidConfig {
                path: path.clone(),
                message: "gateway.kind \"thclaws\" needs gateway.providers (the segments it serves, e.g. [\"sis\"]) — an empty list would unlock every provider".into(),
            });
        }
        if g.enabled && g.is_thclaws() {
            if let Some(d) = g
                .default_model
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
            {
                if crate::providers::locked_model_override(d, &g.providers, None).is_some() {
                    return Err(PolicyError::InvalidConfig {
                        path: path.clone(),
                        message: format!(
                            "gateway.default_model '{d}' is not served by gateway.providers [{}]",
                            g.providers.join(", ")
                        ),
                    });
                }
            }
        }
    }
    if let Some(s) = &policy.policies.sso {
        if s.enabled {
            if s.issuer_url.trim().is_empty() {
                return Err(PolicyError::InvalidConfig {
                    path: path.clone(),
                    message: "sso.enabled but sso.issuer_url is empty — OIDC discovery requires it"
                        .into(),
                });
            }
            if s.client_id.trim().is_empty() {
                return Err(PolicyError::InvalidConfig {
                    path: path.clone(),
                    message: "sso.enabled but sso.client_id is empty — OIDC requires it".into(),
                });
            }
        }
    }
    if let Some(a) = &policy.policies.audit {
        if a.enabled {
            if a.sinks.is_empty() {
                return Err(PolicyError::InvalidConfig {
                    path: path.clone(),
                    message: "audit.enabled but audit.sinks is empty — nothing would record".into(),
                });
            }
            for s in &a.sinks {
                if let AuditSinkConfig::Http { url, .. } = s {
                    if url.trim().is_empty() {
                        return Err(PolicyError::InvalidConfig {
                            path: path.clone(),
                            message: "audit http sink has an empty url".into(),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

/// Multi-line summary for `/policy status` (REPL + GUI).
pub fn status_text() -> String {
    let Some(a) = active() else {
        return "no org policy active (open-core defaults)".to_string();
    };
    let p = &a.policy;
    let on = |b: bool| if b { "on" } else { "off" };
    let mut lines = vec![
        format!(
            "policy: {} (issuer {}, key {})",
            a.source_path.display(),
            p.issuer,
            a.key_source_label
        ),
        format!("expires: {}", p.expires_at.as_deref().unwrap_or("never")),
        format!(
            "branding={} plugins={} gateway={} sso={}",
            on(p.policies
                .branding
                .as_ref()
                .map(|b| b.enabled)
                .unwrap_or(false)),
            on(p.policies
                .plugins
                .as_ref()
                .map(|b| b.enabled)
                .unwrap_or(false)),
            on(p.policies
                .gateway
                .as_ref()
                .map(|b| b.enabled)
                .unwrap_or(false)),
            on(p.policies.sso.as_ref().map(|b| b.enabled).unwrap_or(false)),
        ),
        crate::audit::status_line(),
    ];
    if let Some(g) = p.policies.gateway.as_ref().filter(|g| g.enabled) {
        lines.push(format!("gateway url: {} (kind {})", g.url, g.kind_str()));
        if g.is_thclaws() {
            lines.push(format!(
                "gateway providers: {} · cloud: {} · fail_closed={}",
                g.providers.join(", "),
                g.cloud_url.as_deref().unwrap_or("(settings/env)"),
                on(g.fail_closed)
            ));
            if let Some(d) = g.default_model.as_deref() {
                lines.push(format!("gateway default model: {d}"));
            }
        }
    }
    if let Some(r) = runtime() {
        let mut parts = vec![format!(
            "remote={} serve={}",
            on(r.allow_remote),
            on(r.allow_serve)
        )];
        if let Some(m) = forced_permission_mode() {
            parts.push(format!("permission mode forced to '{m}'"));
        }
        if !r.deny_tools.is_empty() {
            parts.push(format!("tools denied: {}", r.deny_tools.join(", ")));
        }
        lines.push(format!("runtime: {}", parts.join(" · ")));
    }
    lines.join("\n")
}

/// The policy baked in at build time, if any. Base64 in the binary so
/// arbitrary JSON survives `rustc-env`; decoded on the one call that
/// needs it rather than kept resident.
fn embedded_policy_json() -> Option<String> {
    use base64::Engine;
    if EMBEDDED_POLICY_JSON_BASE64.is_empty() {
        return None;
    }
    base64::engine::general_purpose::STANDARD
        .decode(EMBEDDED_POLICY_JSON_BASE64)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
}

/// Walk the documented search path and return the first existing file.
/// Documented in the module header so testing can predict where a file
/// will be picked up from.
pub fn find_file() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("THCLAWS_POLICY_FILE") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }
    let etc = PathBuf::from("/etc/thclaws/policy.json");
    if etc.exists() {
        return Some(etc);
    }
    if let Some(home) = crate::util::home_dir() {
        let user = home.join(format!(
            ".config/{}/policy.json",
            crate::profile::app_dir_name()
        ));
        if user.exists() {
            return Some(user);
        }
    }
    None
}

/// Compare an `expires_at` ISO-8601 string against the current host
/// time. Returns `true` if the policy has expired. Tolerates the
/// common subset of ISO-8601 the rest of the codebase emits: `YYYY-
/// MM-DDTHH:MM:SSZ` and `YYYY-MM-DD`. Anything we can't parse is
/// treated as not-yet-expired (we'd rather accept a slightly weird
/// timestamp than lock everyone out).
fn is_expired(iso: &str) -> bool {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let expiry_secs = match parse_iso8601(iso) {
        Some(s) => s,
        None => return false,
    };
    now_secs > expiry_secs
}

fn parse_iso8601(s: &str) -> Option<u64> {
    // Accept "YYYY-MM-DD" or "YYYY-MM-DDTHH:MM:SS[Z|+HH:MM]".
    // Discard timezone offset for now (treat as UTC).
    let s = s.trim();
    let (date, time) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut date_parts = date.split('-');
    let y: i64 = date_parts.next()?.parse().ok()?;
    let m: u32 = date_parts.next()?.parse().ok()?;
    let d: u32 = date_parts.next()?.parse().ok()?;
    let (h, mi, se) = match time {
        Some(t) => {
            let t = t.trim_end_matches('Z');
            // Strip optional offset.
            let t = t.split(['+', '-']).next().unwrap_or(t);
            let mut p = t.split(':');
            let h: u64 = p.next().unwrap_or("0").parse().ok()?;
            let mi: u64 = p.next().unwrap_or("0").parse().ok()?;
            let se: u64 = p.next().unwrap_or("0").parse().ok()?;
            (h, mi, se)
        }
        None => (0, 0, 0),
    };
    let days = days_from_civil(y, m, d);
    Some((days as u64) * 86_400 + h * 3600 + mi * 60 + se)
}

/// Civil date → days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as u64 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Fingerprint of the running binary. Used for `binding.binary_fingerprint`
/// matching. We compute SHA-256 of the executable on first use and cache
/// the result. The fingerprint format is `sha256:<hex>` or `sha256:<hex>`
/// matched against a prefix to allow partial-fingerprint policies.
pub fn binary_fingerprint() -> String {
    static FP: OnceLock<String> = OnceLock::new();
    FP.get_or_init(|| {
        use sha2::{Digest, Sha256};
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(_) => return String::from("sha256:unknown"),
        };
        match std::fs::read(&exe) {
            Ok(bytes) => {
                let mut h = Sha256::new();
                h.update(&bytes);
                format!("sha256:{:x}", h.finalize())
            }
            Err(_) => String::from("sha256:unknown"),
        }
    })
    .clone()
}

/// Compare an expected fingerprint against the actual one. Accepts
/// prefix matches: `sha256:abcd` matches any binary whose full
/// fingerprint starts with `abcd` after the algorithm prefix. Lets
/// admins ship policies that don't have to be reissued for every
/// rebuild that doesn't change the load-bearing code.
fn fingerprint_matches(expected: &str, actual: &str) -> bool {
    if expected == actual {
        return true;
    }
    let exp_inner = expected.strip_prefix("sha256:").unwrap_or(expected);
    let act_inner = actual.strip_prefix("sha256:").unwrap_or(actual);
    !exp_inner.is_empty() && act_inner.starts_with(exp_inner)
}

#[cfg(test)]
mod permission_pin_tests {
    use super::*;

    /// H4: an org-pinned mode can't be switched by a user setter, and the
    /// live mode never ends up looser than the pin.
    #[test]
    fn pinned_permission_mode_not_changeable() {
        let ask = Some("ask");
        assert!(permission_mode_refusal_for(ask, PermissionMode::Auto).is_some());
        assert!(permission_mode_refusal_for(ask, PermissionMode::Ask).is_none());
        assert!(permission_mode_refusal_for(ask, PermissionMode::LineGated).is_none());
        assert!(permission_mode_refusal_for(Some("auto"), PermissionMode::Ask).is_some());
        assert!(permission_mode_refusal_for(None, PermissionMode::Auto).is_none());
        let msg = permission_mode_refusal_for(ask, PermissionMode::Auto).unwrap();
        assert!(msg.contains("organisation"), "{msg}");

        assert_eq!(
            clamp_mode_to(ask, PermissionMode::Auto),
            PermissionMode::Ask
        );
        assert_eq!(
            clamp_mode_to(ask, PermissionMode::Plan),
            PermissionMode::Plan
        );
        assert_eq!(
            clamp_mode_to(Some("plan"), PermissionMode::Ask),
            PermissionMode::Plan
        );
        assert_eq!(
            clamp_mode_to(None, PermissionMode::Auto),
            PermissionMode::Auto
        );
        assert_eq!(
            clamp_mode_to(Some("auto"), PermissionMode::Ask),
            PermissionMode::Ask
        );
    }

    #[test]
    fn tool_allowed_in_is_case_insensitive() {
        let denied = vec!["Bash".to_string()];
        assert!(!tool_allowed_in(&denied, "bash"));
        assert!(tool_allowed_in(&denied, "Read"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_parses_date_only() {
        let secs = parse_iso8601("2026-04-27").unwrap();
        // 2026-04-27 00:00:00 UTC — sanity-check by feeding it back.
        assert!(secs > 1_700_000_000); // far enough into the future
    }

    #[test]
    fn iso_parses_full_timestamp() {
        let a = parse_iso8601("2026-04-27T00:00:00Z").unwrap();
        let b = parse_iso8601("2026-04-27").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn iso_handles_offset_by_treating_as_utc() {
        // We strip the offset rather than adjust — fine for expiry
        // semantics (off by hours, never by days).
        let a = parse_iso8601("2026-04-27T00:00:00+07:00").unwrap();
        let b = parse_iso8601("2026-04-27T00:00:00Z").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn iso_garbage_is_treated_as_unparseable() {
        assert!(parse_iso8601("hello world").is_none());
    }

    #[test]
    fn unparseable_expiry_is_not_treated_as_expired() {
        // We'd rather accept a weird timestamp than lock everyone out
        // because of a typo.
        assert!(!is_expired("not-a-date"));
    }

    #[test]
    fn past_expiry_is_expired() {
        assert!(is_expired("2020-01-01"));
    }

    #[test]
    fn future_expiry_is_not_expired() {
        // 50 years in the future, well past any test run.
        assert!(!is_expired("2076-01-01"));
    }

    #[test]
    fn fingerprint_exact_match() {
        assert!(fingerprint_matches("sha256:abcd1234", "sha256:abcd1234"));
    }

    #[test]
    fn fingerprint_prefix_match() {
        assert!(fingerprint_matches("sha256:abcd", "sha256:abcd1234ef"));
    }

    #[test]
    fn fingerprint_mismatch() {
        assert!(!fingerprint_matches("sha256:abcd", "sha256:beef1234"));
    }

    #[test]
    fn fingerprint_empty_expected_does_not_match() {
        // Defensive: an empty expected fingerprint would otherwise
        // satisfy `starts_with("")` for everything. Reject.
        assert!(!fingerprint_matches("sha256:", "sha256:abcd"));
    }

    #[test]
    fn validate_rejects_gateway_enabled_with_empty_url() {
        let p = Policy {
            version: 1,
            issuer: "test".into(),
            issued_at: String::new(),
            expires_at: None,
            binding: None,
            policies: Policies {
                gateway: Some(GatewayPolicy {
                    enabled: true,
                    url: String::new(),
                    auth_header_template: None,
                    fail_closed: true,
                    read_only_local_models_allowed: false,
                    ..Default::default()
                }),
                ..Default::default()
            },
            signature: None,
        };
        let result = validate_policies(&p, &PathBuf::from("/tmp/x.json"));
        assert!(matches!(result, Err(PolicyError::InvalidConfig { .. })));
    }

    fn gateway_policy(g: GatewayPolicy) -> Policy {
        Policy {
            version: 1,
            issuer: "test".into(),
            issued_at: String::new(),
            expires_at: None,
            binding: None,
            policies: Policies {
                gateway: Some(g),
                ..Default::default()
            },
            signature: None,
        }
    }

    fn thclaws_gateway_block(providers: &[&str]) -> GatewayPolicy {
        GatewayPolicy {
            enabled: true,
            url: "https://gateway.sis.example".into(),
            fail_closed: true,
            kind: Some("thclaws".into()),
            providers: providers.iter().map(|p| p.to_string()).collect(),
            cloud_url: Some("https://sis.example".into()),
            ..Default::default()
        }
    }

    #[test]
    fn gateway_kind_defaults_to_the_generic_gateway() {
        let mut g = GatewayPolicy {
            enabled: true,
            url: "https://gw.example".into(),
            ..Default::default()
        };
        assert!(!g.is_thclaws());
        g.kind = Some("openai_compat".into());
        assert!(!g.is_thclaws());
        assert!(
            validate_policies(&gateway_policy(g.clone()), &PathBuf::from("/tmp/x.json")).is_ok()
        );
        g.kind = Some(" THClaws ".into());
        assert!(g.is_thclaws());
    }

    #[test]
    fn validate_accepts_a_thclaws_gateway_with_providers() {
        let p = gateway_policy(thclaws_gateway_block(&["sis"]));
        assert!(validate_policies(&p, &PathBuf::from("/tmp/x.json")).is_ok());
    }

    #[test]
    fn validate_rejects_a_thclaws_gateway_without_providers() {
        for providers in [&[][..], &["  "][..]] {
            let p = gateway_policy(thclaws_gateway_block(providers));
            assert!(matches!(
                validate_policies(&p, &PathBuf::from("/tmp/x.json")),
                Err(PolicyError::InvalidConfig { .. })
            ));
        }
    }

    #[test]
    fn validate_checks_the_default_model_is_served() {
        let mut g = thclaws_gateway_block(&["sis"]);
        g.default_model = Some("sis/qwen3.8-flash".into());
        assert!(
            validate_policies(&gateway_policy(g.clone()), &PathBuf::from("/tmp/x.json")).is_ok()
        );
        g.default_model = Some("claude-sonnet-4-6".into());
        let err = validate_policies(&gateway_policy(g), &PathBuf::from("/tmp/x.json"));
        assert!(
            matches!(err, Err(PolicyError::InvalidConfig { ref message, .. }) if message.contains("default_model"))
        );
    }

    #[test]
    fn validate_rejects_an_unknown_gateway_kind() {
        let mut g = thclaws_gateway_block(&["sis"]);
        g.kind = Some("litellm".into());
        let err = validate_policies(&gateway_policy(g), &PathBuf::from("/tmp/x.json"));
        assert!(matches!(err, Err(PolicyError::InvalidConfig { .. })));
    }

    #[test]
    fn thclaws_gateway_fields_round_trip_and_are_absent_by_default() {
        let json = serde_json::to_string(&thclaws_gateway_block(&["sis"])).unwrap();
        let back: GatewayPolicy = serde_json::from_str(&json).unwrap();
        assert!(back.is_thclaws());
        assert_eq!(back.providers, vec!["sis"]);
        assert_eq!(back.cloud_url.as_deref(), Some("https://sis.example"));
        let plain = serde_json::to_string(&GatewayPolicy::default()).unwrap();
        assert!(
            !plain.contains("kind") && !plain.contains("providers") && !plain.contains("cloud_url")
        );
    }

    #[test]
    fn an_unreachable_gateway_asks_for_the_company_network() {
        let raw = "provider error: http: error sending request for url (https://gateway.sis.example/sis/chat/completions): dns error: failed to lookup address information";
        let msg = network_failure_hint(raw, "https://gateway.sis.example/").unwrap();
        assert!(msg.contains("gateway.sis.example"), "{msg}");
        assert!(msg.contains("VPN"), "{msg}");
        assert!(network_failure_hint(
            "provider error: http: tcp connect error: Connection refused",
            "https://gw.example:8443"
        )
        .unwrap()
        .contains("(gw.example)"));
        assert!(network_failure_hint(
            "provider error: http 429 Too Many Requests: {}",
            "https://gw.example"
        )
        .is_none());
    }

    #[test]
    fn without_a_policy_nothing_is_rewritten_or_locked() {
        if active().is_none() {
            assert!(thclaws_gateway().is_none());
            assert!(thclaws_gateway_url().is_none());
            assert!(thclaws_cloud_url().is_none());
            assert!(unreachable_gateway_message("error sending request for url (x)").is_none());
        }
    }

    fn audit_policy(a: AuditPolicy) -> Policy {
        Policy {
            version: 1,
            issuer: "test".into(),
            issued_at: String::new(),
            expires_at: None,
            binding: None,
            policies: Policies {
                audit: Some(a),
                ..Default::default()
            },
            signature: None,
        }
    }

    #[test]
    fn validate_rejects_audit_enabled_without_sinks() {
        let p = audit_policy(AuditPolicy {
            enabled: true,
            sinks: vec![],
            include_summary: true,
            correlate_gateway: true,
        });
        let r = validate_policies(&p, &PathBuf::from("/tmp/x.json"));
        assert!(matches!(r, Err(PolicyError::InvalidConfig { .. })));
    }

    #[test]
    fn validate_rejects_audit_http_sink_with_empty_url() {
        let p = audit_policy(AuditPolicy {
            enabled: true,
            sinks: vec![AuditSinkConfig::Http {
                url: "  ".into(),
                auth_header_template: None,
                batch: 50,
                flush_secs: 5,
            }],
            include_summary: true,
            correlate_gateway: true,
        });
        let r = validate_policies(&p, &PathBuf::from("/tmp/x.json"));
        assert!(matches!(r, Err(PolicyError::InvalidConfig { .. })));
    }

    #[test]
    fn validate_accepts_audit_disabled_without_sinks_and_file_sink_without_path() {
        let off = audit_policy(AuditPolicy::default());
        assert!(validate_policies(&off, &PathBuf::from("/tmp/x.json")).is_ok());
        let on = audit_policy(AuditPolicy {
            enabled: true,
            sinks: vec![AuditSinkConfig::File { path: None }],
            include_summary: true,
            correlate_gateway: false,
        });
        assert!(validate_policies(&on, &PathBuf::from("/tmp/x.json")).is_ok());
    }

    /// The block is optional and skipped when absent, so a policy signed
    /// before Phase 5 canonicalizes to the same bytes and keeps verifying.
    #[test]
    fn audit_block_round_trips_and_is_absent_by_default() {
        let doc: serde_json::Value = serde_json::json!({
            "version": 1, "issuer": "t", "issued_at": "", "policies": {
                "audit": {
                    "enabled": true,
                    "sinks": [
                        {"type": "file", "path": "~/.local/share/thclaws/audit/%Y-%m-%d.jsonl"},
                        {"type": "http", "url": "https://siem.example/thclaws",
                         "auth_header_template": "Bearer {{env:T}}"}
                    ]
                }
            }
        });
        let p: Policy = serde_json::from_value(doc).unwrap();
        let a = p.policies.audit.as_ref().unwrap();
        assert!(a.enabled && a.include_summary && a.correlate_gateway);
        assert_eq!(a.sinks.len(), 2);
        match &a.sinks[1] {
            AuditSinkConfig::Http {
                batch, flush_secs, ..
            } => {
                assert_eq!((*batch, *flush_secs), (50, 5));
            }
            other => panic!("expected http sink, got {other:?}"),
        }
        let back = serde_json::to_value(&p).unwrap();
        assert_eq!(back["policies"]["audit"]["sinks"][0]["type"], "file");

        let plain: Policy =
            serde_json::from_str(r#"{"version":1,"issuer":"t","issued_at":"","policies":{}}"#)
                .unwrap();
        assert!(plain.policies.audit.is_none());
        assert!(serde_json::to_value(&plain).unwrap()["policies"]
            .get("audit")
            .is_none());
    }

    #[test]
    fn validate_accepts_gateway_disabled_with_empty_url() {
        let p = Policy {
            version: 1,
            issuer: "test".into(),
            issued_at: String::new(),
            expires_at: None,
            binding: None,
            policies: Policies {
                gateway: Some(GatewayPolicy {
                    enabled: false,
                    url: String::new(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            signature: None,
        };
        assert!(validate_policies(&p, &PathBuf::from("/tmp/x.json")).is_ok());
    }

    #[test]
    fn validate_rejects_sso_enabled_with_empty_issuer() {
        let p = Policy {
            version: 1,
            issuer: "test".into(),
            issued_at: String::new(),
            expires_at: None,
            binding: None,
            policies: Policies {
                sso: Some(SsoPolicy {
                    enabled: true,
                    provider: "oidc".into(),
                    issuer_url: String::new(),
                    client_id: "client".into(),
                    audience: None,
                    client_secret: None,
                    client_secret_env: None,
                }),
                ..Default::default()
            },
            signature: None,
        };
        assert!(matches!(
            validate_policies(&p, &PathBuf::from("/tmp/x.json")),
            Err(PolicyError::InvalidConfig { .. })
        ));
    }

    #[test]
    fn runtime_block_parses_and_defaults_to_permissive() {
        // Absent block: every accessor answers like open-core.
        let p: Policy =
            serde_json::from_str(r#"{"version":1,"policies":{},"signature":"x"}"#).unwrap();
        assert!(p.policies.runtime.is_none());

        // Present but only naming a denial: the two booleans default on,
        // so a policy cannot switch off Remote by forgetting a field.
        let p: Policy = serde_json::from_str(
            r#"{"version":1,"policies":{"runtime":{"enabled":true,
                "permission_mode":"ask","deny_tools":["Bash"]}},"signature":"x"}"#,
        )
        .unwrap();
        let r = p.policies.runtime.as_ref().unwrap();
        assert!(r.enabled && r.allow_remote && r.allow_serve);
        assert_eq!(r.deny_tools, vec!["Bash".to_string()]);

        // A disabled block is inert, and an unknown mode is ignored
        // rather than obeyed as something it is not.
        let disabled = RuntimePolicy {
            enabled: false,
            permission_mode: Some("ask".into()),
            deny_tools: vec!["Bash".into()],
            allow_remote: false,
            allow_serve: false,
        };
        assert!(!disabled.enabled);
        let json = serde_json::to_string(&RuntimePolicy {
            enabled: true,
            permission_mode: Some("sideways".into()),
            ..Default::default()
        })
        .unwrap();
        let back: RuntimePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back.permission_mode.as_deref(), Some("sideways"));
    }

    #[test]
    fn accessors_are_permissive_without_a_policy() {
        // No policy is loaded in the unit-test process, which is the
        // open-core case every non-EE user runs.
        assert!(runtime().is_none());
        assert!(remote_allowed());
        assert!(serve_allowed());
        assert!(tool_allowed("Bash"));
        assert!(denied_tools().is_empty());
        assert!(forced_permission_mode().is_none());
    }

    #[test]
    fn policy_round_trips_through_json() {
        let policy = Policy {
            version: 1,
            issuer: "ACME".into(),
            issued_at: "2026-04-27T00:00:00Z".into(),
            expires_at: Some("2027-04-27T00:00:00Z".into()),
            binding: Some(Binding {
                org_id: "acme".into(),
                binary_fingerprint: Some("sha256:abcd".into()),
            }),
            policies: Policies {
                branding: Some(BrandingPolicy {
                    enabled: true,
                    name: Some("ACME Agent".into()),
                    ..Default::default()
                }),
                plugins: Some(PluginsPolicy {
                    enabled: true,
                    allowed_hosts: vec!["github.com/acme/*".into()],
                    allow_external_scripts: false,
                    allow_external_mcp: false,
                }),
                gateway: None,
                sso: None,
                audit: None,
                runtime: None,
                marketplace: None,
            },
            signature: Some("sig".into()),
        };
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: Policy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.issuer, "ACME");
        assert_eq!(
            parsed.policies.plugins.as_ref().unwrap().allowed_hosts,
            vec!["github.com/acme/*"]
        );
    }
}

#[cfg(test)]
mod marketplace_only_tests {
    use super::*;

    #[test]
    fn env_locks_only_when_it_says_marketplace() {
        assert_eq!(
            marketplace_only_from_env(
                Some("marketplace"),
                Some(" https://t/api/marketplace.json ")
            ),
            Some("https://t/api/marketplace.json".into())
        );
        assert_eq!(
            marketplace_only_from_env(Some("Marketplace"), None),
            Some(String::new())
        );
        assert_eq!(
            marketplace_only_from_env(Some("open"), Some("https://t")),
            None
        );
        assert_eq!(marketplace_only_from_env(None, Some("https://t")), None);
    }

    #[test]
    fn policy_block_parses() {
        let p: Policies = serde_json::from_str(
            r#"{"marketplace":{"enabled":true,"url":"https://sis.thclaws.ai/api/marketplace.json"}}"#,
        )
        .unwrap();
        let m = p.marketplace.unwrap();
        assert!(m.enabled);
        assert_eq!(m.url, "https://sis.thclaws.ai/api/marketplace.json");
    }
}

#[cfg(test)]
mod embedded_policy_tests {
    use super::*;

    /// The open-core build must carry neither, and must not require one.
    /// If this fails on CI, a key or policy leaked into a public build.
    #[test]
    fn open_core_build_embeds_nothing_and_requires_nothing() {
        if EMBEDDED_PUBKEY_BASE64.is_empty() {
            assert!(
                EMBEDDED_POLICY_JSON_BASE64.is_empty(),
                "a policy is embedded but no key is — it could never be verified"
            );
            assert!(
                !POLICY_REQUIRED,
                "a build with no key demands a policy it has no way to check"
            );
        }
    }

    /// A policy without a key is unverifiable, so that combination must
    /// never enable the requirement. Guards the build.rs condition.
    #[test]
    fn requirement_implies_a_key_to_verify_against() {
        if POLICY_REQUIRED {
            assert!(
                !EMBEDDED_PUBKEY_BASE64.is_empty()
                    || std::env::var("THCLAWS_POLICY_PUBLIC_KEY").is_ok(),
                "policy required but nothing to verify it with"
            );
        }
    }

    #[test]
    fn embedded_decoder_round_trips() {
        use base64::Engine;
        let json = r#"{"version":1,"issuer":"ACME"}"#;
        let b64 = base64::engine::general_purpose::STANDARD.encode(json);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .map(|b| String::from_utf8(b).unwrap());
        assert_eq!(decoded.unwrap(), json);
    }

    /// The message an ordinary employee sees must tell them where to put
    /// the file, not just that something failed.
    #[test]
    fn the_missing_policy_message_is_actionable() {
        let msg = PolicyError::PolicyRequired.refuse_message();
        assert!(msg.contains("/etc/thclaws/policy.json"), "no system path");
        assert!(
            msg.contains("~/.config/thclaws/policy.json"),
            "no user path"
        );
        assert!(
            msg.contains("Ask whoever provided thClaws"),
            "no route to a human"
        );
    }
}
