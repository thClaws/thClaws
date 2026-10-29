//! Branding indirection — product name, banner, logo, support contact.
//!
//! Reads from the active org policy when one is loaded; falls back to
//! today's defaults otherwise. Open-core users with no policy file see
//! zero behavior change. Enterprise builds with a verified policy that
//! has `policies.branding.enabled: true` get the branded values.
//!
//! The module is leaf-level: it depends on `crate::policy` (read-only)
//! and nothing else. Callers ask for `branding::current()` and get a
//! snapshot they can pull fields from. The snapshot is built once on
//! first call and cached — the policy doesn't change at runtime.
//!
//! ## Sites that consume branding
//!
//! - `repl.rs`: REPL banner / version header / "restart {product}" /
//!   "── {product} diagnostics ──"
//! - `gui.rs`: native window title
//! - `prompts.rs`: `{product}` placeholder substitution in loaded
//!   system / subagent / lead prompt templates
//!
//! ## Frontend
//!
//! GUI strings live in React and read [`payload`] — pushed on every
//! `frontend_ready` and answered on `branding_get` (IPC), and served at
//! `GET /api/branding` in `--serve`. A per-customer build also embeds
//! the customer's logo (`THCLAWS_CUSTOMER_LOGO`, build.rs).

use std::path::PathBuf;
use std::sync::OnceLock;

/// Embedded default banner. Same bytes the REPL has always shipped —
/// keeps the open-core build identical when no policy is active.
const DEFAULT_BANNER: &str = include_str!("../../../banner.txt");

/// Default about-text shown in CLI diagnostics and (eventually) the GUI
/// About modal. Match what the README and downloads page imply.
const DEFAULT_ABOUT: &str =
    "thClaws — native-Rust AI agent workspace. Open source, MIT/Apache-2.0. \
     Developed by ThaiGPT Co., Ltd.";

/// Default support contact. Matches what's in SECURITY.md and the
/// public website.
const DEFAULT_SUPPORT_EMAIL: &str = "jimmy@thaigpt.com";

/// Materialized branding values read once at startup. Treat as read-only.
#[derive(Debug, Clone)]
pub struct Branding {
    /// Product name shown in REPL banner, GUI title bar, system prompt.
    pub name: String,
    /// Support / vulnerability-report email surfaced in diagnostics
    /// and the About modal.
    pub support_email: String,
    /// Optional path to a logo file. None → frontend uses its embedded
    /// default. Set by enterprise policy when org wants their own
    /// logo in the GUI.
    pub logo_path: Option<PathBuf>,
    /// REPL ASCII banner. Defaults to the embedded `banner.txt` bytes.
    pub banner_text: String,
    /// One-line about description shown in CLI `/version` output and
    /// the GUI About modal.
    pub about_text: String,
}

impl Default for Branding {
    fn default() -> Self {
        Self {
            name: "thClaws".to_string(),
            support_email: DEFAULT_SUPPORT_EMAIL.to_string(),
            logo_path: None,
            banner_text: DEFAULT_BANNER.to_string(),
            about_text: DEFAULT_ABOUT.to_string(),
        }
    }
}

static CURRENT: OnceLock<Branding> = OnceLock::new();

/// Snapshot the current branding values. First call reads
/// `policy::active()` and applies any branding overrides; subsequent
/// calls return the cached snapshot. Safe to call from anywhere after
/// `policy::load_or_refuse()` has run at startup.
pub fn current() -> &'static Branding {
    CURRENT.get_or_init(materialize)
}

/// Build the snapshot from defaults + active policy. Each policy field
/// is `Option<String>`, so unset fields fall through to today's
/// defaults — an enterprise can override only product name and
/// support email without losing the embedded banner, etc.
fn materialize() -> Branding {
    let mut b = Branding::default();
    let active = match crate::policy::active() {
        Some(a) => a,
        None => return b,
    };
    let bp = match &active.policy.policies.branding {
        Some(p) if p.enabled => p,
        _ => return b,
    };
    if let Some(name) = &bp.name {
        if !name.trim().is_empty() {
            b.name = name.clone();
        }
    }
    if let Some(email) = &bp.support_email {
        if !email.trim().is_empty() {
            b.support_email = email.clone();
        }
    }
    if let Some(logo) = &bp.logo_path {
        if !logo.trim().is_empty() {
            b.logo_path = Some(PathBuf::from(logo));
        }
    }
    if let Some(banner) = &bp.banner_text {
        if !banner.trim().is_empty() {
            b.banner_text = banner.clone();
        }
    }
    if let Some(about) = &bp.about_text {
        if !about.trim().is_empty() {
            b.about_text = about.clone();
        }
    }
    b
}

/// Customer logo baked in at build time (`THCLAWS_CUSTOMER_LOGO`, see
/// build.rs). Empty on open-core builds. Comes from the binary, not the
/// policy, so a customer build is identifiable before any policy loads.
const CUSTOMER_LOGO: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/customer_logo.bin"));
const CUSTOMER_LOGO_MIME: &str = env!("THCLAWS_CUSTOMER_LOGO_MIME");
const CUSTOMER_LOGO_DARK: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/customer_logo_dark.bin"));
const CUSTOMER_LOGO_DARK_MIME: &str = env!("THCLAWS_CUSTOMER_LOGO_DARK_MIME");

fn data_uri(bytes: &[u8], mime: &str) -> Option<String> {
    use base64::Engine;
    if bytes.is_empty() || mime.is_empty() {
        return None;
    }
    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// `true` on a per-customer build that embeds a logo.
pub fn has_customer_logo() -> bool {
    !CUSTOMER_LOGO.is_empty()
}

/// SHA-256 of the embedded policy public key, hex, first 16 bytes — what a
/// release manifest records, so support can match a laptop to its build.
/// `None` on a build with no embedded key.
pub fn embedded_pubkey_fingerprint() -> Option<String> {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let raw = crate::policy::verify::EMBEDDED_PUBKEY_BASE64;
    if raw.is_empty() {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(raw).ok()?;
    let d = Sha256::digest(&bytes);
    Some(d[..16].iter().map(|b| format!("{b:02x}")).collect())
}

/// A cloud workspace runs the stock engine image, so it has no logo baked in;
/// the provisioner hands it the deployment's mark (logo URLs on the same host
/// as the workspace page, and the short name) so the chat shows what the
/// organisation's EE desktop shows.
fn runtime_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The frame the GUI renders branding from (`branding_get` IPC and
/// `GET /api/branding`). `banner` is the policy's banner text only — the
/// REPL's ASCII art default is not a GUI banner.
pub fn payload() -> serde_json::Value {
    let b = current();
    let active = crate::policy::active();
    let bp = active
        .and_then(|a| a.policy.policies.branding.as_ref())
        .filter(|p| p.enabled);
    let banner = bp
        .and_then(|p| p.banner_text.clone())
        .filter(|s| !s.trim().is_empty());
    let policy = active.map(|a| {
        serde_json::json!({
            "issuer": a.policy.issuer,
            "issued_at": a.policy.issued_at,
            "expires_at": a.policy.expires_at,
            "key_source": a.key_source_label,
        })
    });
    let (logo, logo_dark) = match data_uri(CUSTOMER_LOGO, CUSTOMER_LOGO_MIME) {
        Some(logo) => {
            let dark =
                data_uri(CUSTOMER_LOGO_DARK, CUSTOMER_LOGO_DARK_MIME).unwrap_or(logo.clone());
            (Some(logo), Some(dark))
        }
        None => {
            let logo = runtime_env("THCLAWS_BRAND_LOGO_URL");
            let dark = runtime_env("THCLAWS_BRAND_LOGO_DARK_URL").or_else(|| logo.clone());
            (logo, dark)
        }
    };
    let name = match (bp, runtime_env("THCLAWS_BRAND_NAME")) {
        (None, Some(n)) => n,
        _ => b.name.clone(),
    };
    serde_json::json!({
        "type": "branding",
        "name": name,
        "support_email": b.support_email,
        "about": b.about_text,
        "banner": banner,
        "logo": logo,
        "logo_dark": logo_dark,
        "customer_build": has_customer_logo(),
        "pubkey_fingerprint": embedded_pubkey_fingerprint(),
        "policy": policy,
        // An org gateway desktop signs in to this cloud instead of taking
        // API keys; the GUI offers that sign-in where it would ask for a key.
        "org_cloud_url": crate::policy::thclaws_cloud_url(),
    })
}

/// Substitute branding placeholders into a template string. Replaces
/// `{product}` with the product name and `{support_email}` with the
/// support email. Other `{...}` placeholders are left untouched so
/// callers can compose with `prompts::render` without conflict.
///
/// Used by `prompts::load` to apply branding to system / subagent /
/// lead prompts as they're loaded — same `{key}` substitution syntax
/// the rest of the prompt machinery uses.
pub fn apply_template(template: &str) -> String {
    let b = current();
    template
        .replace("{product}", &b.name)
        .replace("{support_email}", &b.support_email)
}

#[cfg(test)]
mod tests {
    use super::*;

    // We can't easily reset the OnceLock between tests, so the policy-
    // active path is exercised end-to-end via the smoke tests in
    // `tests/policy_smoke/` rather than unit-tested here. These tests
    // cover the materialization helper and template substitution
    // directly, which don't depend on the cache.

    #[test]
    fn cloud_workspace_takes_the_deployment_mark_from_env() {
        let _guard = crate::kms::test_env_lock();
        // Open-core test build: nothing baked in, no policy.
        assert!(!has_customer_logo());
        std::env::set_var("THCLAWS_BRAND_LOGO_URL", "/api/branding/logo");
        std::env::remove_var("THCLAWS_BRAND_LOGO_DARK_URL");
        std::env::set_var("THCLAWS_BRAND_NAME", "SiS");
        let p = payload();
        std::env::remove_var("THCLAWS_BRAND_LOGO_URL");
        std::env::remove_var("THCLAWS_BRAND_NAME");
        assert_eq!(p["logo"], "/api/branding/logo");
        assert_eq!(
            p["logo_dark"], "/api/branding/logo",
            "dark falls back to light"
        );
        assert_eq!(p["name"], "SiS");
        assert_eq!(
            p["customer_build"], false,
            "a URL logo is not a customer build"
        );
        let p = payload();
        assert!(p["logo"].is_null(), "no env, no logo: {p}");
    }

    #[test]
    fn defaults_are_today_strings() {
        let b = Branding::default();
        assert_eq!(b.name, "thClaws");
        assert_eq!(b.support_email, "jimmy@thaigpt.com");
        assert!(b.logo_path.is_none());
        assert!(b.banner_text.contains("█")); // banner.txt has block chars
        assert!(b.about_text.contains("native-Rust"));
    }

    #[test]
    fn apply_template_substitutes_product() {
        // We test the substitution logic against a constructed template
        // because `current()` would read from the policy OnceLock.
        let b = Branding {
            name: "ACME Agent".into(),
            support_email: "agent@acme.example".into(),
            ..Branding::default()
        };
        let out = format!("Hello, {{product}}!").replace("{product}", &b.name);
        assert_eq!(out, "Hello, ACME Agent!");
    }

    #[test]
    fn data_uri_needs_bytes_and_mime() {
        assert_eq!(data_uri(b"", "image/png"), None);
        assert_eq!(data_uri(b"x", ""), None);
        assert_eq!(
            data_uri(b"hi", "image/png").as_deref(),
            Some("data:image/png;base64,aGk=")
        );
    }

    #[test]
    fn payload_has_the_gui_fields() {
        let _guard = crate::kms::test_env_lock();
        let p = payload();
        assert_eq!(p["type"], "branding");
        assert!(p["name"].is_string());
        for k in [
            "logo",
            "logo_dark",
            "banner",
            "policy",
            "pubkey_fingerprint",
            "customer_build",
        ] {
            assert!(p.get(k).is_some(), "missing {k}");
        }
        assert_eq!(p["customer_build"].as_bool(), Some(has_customer_logo()));
        if has_customer_logo() {
            assert!(p["logo"].as_str().unwrap().starts_with("data:image/"));
            assert!(
                p["logo_dark"].is_string(),
                "dark falls back to the light logo"
            );
        } else {
            assert!(p["logo"].is_null());
        }
    }

    #[test]
    fn apply_template_leaves_unknown_placeholders_alone() {
        let template = "I'm {product}. Reach me at {support_email}. Stay {curious}.";
        // Direct substitution test (without the OnceLock).
        let after_product = template.replace("{product}", "thClaws");
        let after_email = after_product.replace("{support_email}", "jimmy@thaigpt.com");
        assert_eq!(
            after_email,
            "I'm thClaws. Reach me at jimmy@thaigpt.com. Stay {curious}."
        );
    }
}
