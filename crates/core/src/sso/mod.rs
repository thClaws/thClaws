//! OIDC SSO integration (Enterprise Edition Phase 4).
//!
//! Drives the desktop OIDC authorization-code + PKCE flow against any
//! standards-compliant IdP — Okta, Azure AD / Entra ID, Auth0,
//! Keycloak, Google Workspace, AWS Cognito, etc. — selected by the
//! `policies.sso.issuer_url` field in the active org policy.
//!
//! ## What this module owns
//!
//! - `login(policy)` — interactive flow: discovery → PKCE → browser open
//!   → loopback callback → token exchange → store
//! - `logout(policy)` — clear cached tokens
//! - `current_session(policy)` — read cached session (lazy refresh
//!   when within the refresh window)
//! - `current_access_token(policy)` — convenience for the gateway
//!   integration; returns `None` when no session is active
//! - `decode_id_token_claims(jwt)` — parse displayable claims out of
//!   the id_token without verifying its signature (signature check is
//!   the gateway's job; we only read for display)
//!
//! ## What this module does NOT own
//!
//! - JWT signature verification — that's the gateway's responsibility.
//!   Our access_token gets relayed to the gateway, which validates it
//!   against the IdP's JWKS.
//! - Authorization decisions — we don't gate features on group claims;
//!   that's again the gateway's job (or future per-feature policy).
//! - GUI surfacing — Phase 4 ships CLI slash commands (`/sso login`,
//!   `/sso logout`, `/sso status`). GUI sidebar integration follows
//!   in a Phase 4 follow-up commit.

pub mod discovery;
pub mod loopback;
pub mod pkce;
pub mod storage;

use std::sync::Mutex;
use std::sync::OnceLock;

use base64::Engine;

use crate::error::{Error, Result};
use crate::policy::SsoPolicy;

pub use storage::Session;

/// In-process cache of the most recently loaded session, scoped by
/// issuer. Avoids hitting the keychain on every gateway request.
/// Refreshed when login / logout / refresh write through here.
static CACHE: OnceLock<Mutex<Option<Session>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<Session>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Refresh the access_token early enough that clock skew between
/// thClaws and the IdP / gateway doesn't surface as a 401.
const REFRESH_WINDOW_SECS: u64 = 60;

/// Drive an OIDC authorization-code + PKCE login flow. Opens the
/// user's browser, listens on a loopback port for the callback,
/// exchanges the auth code for tokens, persists the session.
///
/// Returns the resulting `Session` for callers that want to display
/// "logged in as alice@acme" immediately.
pub async fn login(policy: &SsoPolicy) -> Result<Session> {
    if policy.issuer_url.trim().is_empty() {
        return Err(Error::Config(
            "sso.issuer_url is empty — cannot start login".into(),
        ));
    }
    if policy.client_id.trim().is_empty() {
        return Err(Error::Config(
            "sso.client_id is empty — cannot start login".into(),
        ));
    }
    let doc = discovery::fetch(&policy.issuer_url).await?;
    let pkce = pkce::PkcePair::generate();
    let server = loopback::LoopbackServer::bind()?;
    let redirect_uri = server.redirect_uri();
    let state = generate_state();
    let scopes = "openid email profile";

    let mut authz_url = format!(
        "{base}?response_type=code&client_id={client}&redirect_uri={redirect}&scope={scope}&state={state}&code_challenge={challenge}&code_challenge_method=S256",
        base = doc.authorization_endpoint,
        client = url_encode(&policy.client_id),
        redirect = url_encode(&redirect_uri),
        scope = url_encode(scopes),
        state = url_encode(&state),
        challenge = url_encode(&pkce.challenge),
    );
    if let Some(audience) = &policy.audience {
        if !audience.trim().is_empty() {
            authz_url.push_str(&format!("&audience={}", url_encode(audience)));
        }
    }

    eprintln!("opening browser for SSO login: {}", policy.issuer_url);
    if let Err(e) = open_browser(&authz_url) {
        eprintln!(
            "could not open browser automatically ({e}). Visit this URL manually:\n  {authz_url}"
        );
    }

    let callback = server.accept_one(300)?; // 5 min user-action window
    if let Some(err) = &callback.error {
        return Err(Error::Tool(format!(
            "OIDC login failed: {err} ({})",
            callback.error_description.as_deref().unwrap_or("")
        )));
    }
    let code = callback
        .code
        .ok_or_else(|| Error::Tool("OIDC callback missing `code` parameter".into()))?;
    if callback.state.as_deref() != Some(state.as_str()) {
        return Err(Error::Tool(
            "OIDC callback `state` did not match — refusing to exchange (possible CSRF)".into(),
        ));
    }

    let client_secret = resolve_client_secret(policy);
    let session = exchange_code(
        &doc.token_endpoint,
        &policy.client_id,
        client_secret.as_deref(),
        &code,
        &pkce.verifier,
        &redirect_uri,
        &policy.issuer_url,
    )
    .await?;

    storage::save(&session)?;
    update_cache(Some(session.clone()));
    Ok(session)
}

/// Clear the cached session for `policy.issuer_url`. Idempotent —
/// calling on an already-logged-out client is fine.
pub fn logout(policy: &SsoPolicy) -> Result<()> {
    storage::clear(&policy.issuer_url)?;
    update_cache(None);
    Ok(())
}

/// Get the currently active session (loading from keychain on first
/// call). Returns `None` when no session is cached or stored.
///
/// Lazy refresh: if the session is within the refresh window, kicks
/// off a background refresh task. The current call returns the
/// soon-to-be-stale token immediately — gateway requests get a brief
/// window where they might 401 once before the refresh lands. Avoiding
/// blocking on refresh keeps the call site sync.
pub fn current_session(policy: &SsoPolicy) -> Option<Session> {
    {
        let guard = cache().lock().ok()?;
        if let Some(s) = guard.clone() {
            // Kick off refresh if we're inside the refresh window and
            // have a refresh_token. Background task; we don't await.
            if s.expires_within(REFRESH_WINDOW_SECS) && s.refresh_token.is_some() {
                spawn_background_refresh(policy.clone(), s.clone());
            }
            return Some(s);
        }
    }
    // Cache miss → load from keychain and prime cache.
    let stored = storage::load(&policy.issuer_url)?;
    update_cache(Some(stored.clone()));
    Some(stored)
}

/// Convenience for the gateway integration: returns the access token
/// when a session is active and not yet expired, `None` otherwise.
pub fn current_access_token(policy: &SsoPolicy) -> Option<String> {
    let s = current_session(policy)?;
    if s.is_expired() {
        return None;
    }
    Some(s.access_token)
}

/// Pretty status line for `/sso status` and the GUI sidebar.
pub fn status(policy: &SsoPolicy) -> String {
    if !policy.enabled {
        return "SSO not enabled in org policy".into();
    }
    match current_session(policy) {
        Some(s) => {
            let who = s
                .email
                .clone()
                .or_else(|| s.name.clone())
                .or_else(|| s.sub.clone())
                .unwrap_or_else(|| "(no identity claim)".into());
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let remaining = s.expires_at.saturating_sub(now);
            format!(
                "logged in as {who} (issuer: {}; access token expires in {}s)",
                s.issuer, remaining
            )
        }
        None => format!(
            "not logged in (issuer: {}; run /sso login)",
            policy.issuer_url
        ),
    }
}

/// Decode the *claims* portion of a JWT without verifying the
/// signature. Used to extract email / name / sub for display after a
/// successful token exchange. **Do not use for authorization decisions** —
/// signature verification is the gateway's responsibility, and any
/// authz decision in the client would be trivially bypassable.
pub fn decode_id_token_claims(jwt: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = jwt.splitn(3, '.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(parts[1]))
        .ok()?;
    serde_json::from_slice(&payload).ok()
}

// ── token exchange ──────────────────────────────────────────────────

async fn exchange_code(
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    issuer: &str,
) -> Result<Session> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| Error::Tool(format!("build http client: {e}")))?;
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", code),
        ("code_verifier", code_verifier),
        ("redirect_uri", redirect_uri),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    let resp = client
        .post(token_endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("token exchange POST: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Tool(format!(
            "token exchange failed: HTTP {status}: {body}"
        )));
    }
    let body: TokenResponse = resp
        .json()
        .await
        .map_err(|e| Error::Tool(format!("parse token response: {e}")))?;
    Ok(build_session(body, issuer, client_id))
}

async fn refresh_session(policy: SsoPolicy, prior: Session) -> Result<Session> {
    let refresh_token = prior.refresh_token.clone().ok_or_else(|| {
        Error::Tool(
            "no refresh_token in cached session — re-login required (run /sso login)".into(),
        )
    })?;
    let doc = discovery::fetch(&policy.issuer_url).await?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| Error::Tool(format!("build http client: {e}")))?;
    let secret = resolve_client_secret(&policy);
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("client_id", &policy.client_id),
        ("refresh_token", &refresh_token),
    ];
    if let Some(s) = secret.as_deref() {
        form.push(("client_secret", s));
    }
    let resp = client
        .post(&doc.token_endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("refresh POST: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Tool(format!(
            "token refresh failed: HTTP {status}: {body} — re-login required"
        )));
    }
    let body: TokenResponse = resp
        .json()
        .await
        .map_err(|e| Error::Tool(format!("parse refresh response: {e}")))?;
    let mut new_session = build_session(body, &policy.issuer_url, &policy.client_id);
    // Some IdPs don't issue a fresh refresh_token on every refresh —
    // carry the prior one forward in that case.
    if new_session.refresh_token.is_none() {
        new_session.refresh_token = prior.refresh_token.clone();
    }
    Ok(new_session)
}

#[derive(Debug, serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

fn build_session(t: TokenResponse, issuer: &str, client_id: &str) -> Session {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let expires_at = now + t.expires_in.unwrap_or(3600);
    let claims = t.id_token.as_deref().and_then(decode_id_token_claims);
    let email = claims
        .as_ref()
        .and_then(|c| c.get("email"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let name = claims
        .as_ref()
        .and_then(|c| c.get("name"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let sub = claims
        .as_ref()
        .and_then(|c| c.get("sub"))
        .and_then(|v| v.as_str())
        .map(String::from);
    Session {
        issuer: issuer.to_string(),
        client_id: client_id.to_string(),
        access_token: t.access_token,
        id_token: t.id_token,
        refresh_token: t.refresh_token,
        expires_at,
        email,
        name,
        sub,
    }
}

// ── helpers ─────────────────────────────────────────────────────────

fn update_cache(session: Option<Session>) {
    if let Ok(mut guard) = cache().lock() {
        *guard = session;
    }
}

fn spawn_background_refresh(policy: SsoPolicy, prior: Session) {
    tokio::spawn(async move {
        match refresh_session(policy.clone(), prior).await {
            Ok(new_session) => {
                let _ = storage::save(&new_session);
                update_cache(Some(new_session));
            }
            Err(e) => {
                eprintln!("[sso] background refresh failed: {e}");
            }
        }
    });
}

/// Resolve the optional client_secret. Order:
///   1. Inline `client_secret` in the policy (for non-confidential
///      secrets like Google Desktop OAuth — Google's own docs treat
///      these as embedded-in-binary, so embedding in policy is the
///      simplest enterprise-deploy story).
///   2. `client_secret_env` → look up the named env var (for real
///      confidential secrets that should never embed in artifacts).
///   3. `None` (true PKCE-only public clients).
///
/// Each layer treats blank/empty as "not set" so a left-over `=""`
/// line in `.env` or a stray space in the policy doesn't accidentally
/// authenticate as the empty string.
pub fn resolve_client_secret(policy: &SsoPolicy) -> Option<String> {
    if let Some(literal) = policy.client_secret.as_deref() {
        let trimmed = literal.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let var = policy.client_secret_env.as_deref()?.trim();
    if var.is_empty() {
        return None;
    }
    let value = std::env::var(var).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn generate_state() -> String {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).expect("OS RNG");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Minimal `application/x-www-form-urlencoded`-style URL encoding for
/// query parameters — encode any byte outside `[A-Za-z0-9_.~-]` as
/// `%XX`. Saves us pulling in the full `urlencoding` crate's API
/// just for these few call sites.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

fn open_browser(url: &str) -> std::io::Result<()> {
    use std::process::Command;

    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };

    #[cfg(target_os = "windows")]
    use std::os::windows::process::CommandExt;

    // for Windows creation flag to hide the console window
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    cmd.spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_handles_safe_and_unsafe() {
        assert_eq!(url_encode("abc-123_~."), "abc-123_~.");
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("a/b?c=d"), "a%2Fb%3Fc%3Dd");
        assert_eq!(url_encode("@#$"), "%40%23%24");
    }

    #[test]
    fn decode_id_token_claims_pulls_email_and_sub() {
        // Standard test JWT (no signature verification — we drop sig):
        // header: {"alg":"HS256","typ":"JWT"}
        // payload: {"sub":"alice","email":"alice@acme","name":"Alice"}
        let payload = serde_json::json!({
            "sub": "alice",
            "email": "alice@acme.example",
            "name": "Alice Sanders"
        });
        let header_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        let jwt = format!("{header_b64}.{payload_b64}.signature-bytes");
        let claims = decode_id_token_claims(&jwt).expect("decoded");
        assert_eq!(claims["sub"], "alice");
        assert_eq!(claims["email"], "alice@acme.example");
        assert_eq!(claims["name"], "Alice Sanders");
    }

    #[test]
    fn decode_id_token_claims_handles_padded_base64() {
        // Some IdPs (rare) pad their JWT segments. Accept both forms.
        let header_b64 = base64::engine::general_purpose::URL_SAFE.encode(br#"{"alg":"none"}"#);
        let payload = serde_json::json!({"sub": "alice"});
        let payload_b64 =
            base64::engine::general_purpose::URL_SAFE.encode(payload.to_string().as_bytes());
        let jwt = format!("{header_b64}.{payload_b64}.sig");
        assert!(decode_id_token_claims(&jwt).is_some());
    }

    #[test]
    fn decode_id_token_claims_rejects_garbage() {
        assert!(decode_id_token_claims("not.a.jwt-payload").is_none());
        assert!(decode_id_token_claims("only-one-part").is_none());
        assert!(decode_id_token_claims("").is_none());
    }

    #[test]
    fn build_session_extracts_claims_from_id_token() {
        let payload = serde_json::json!({"sub":"u1","email":"e@x","name":"N"});
        let header_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256"}"#);
        let payload_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        let jwt = format!("{header_b64}.{payload_b64}.sig");
        let resp = TokenResponse {
            access_token: "at".into(),
            id_token: Some(jwt),
            refresh_token: Some("rt".into()),
            expires_in: Some(3600),
        };
        let s = build_session(resp, "https://acme.okta.com", "thclaws-internal");
        assert_eq!(s.email.as_deref(), Some("e@x"));
        assert_eq!(s.name.as_deref(), Some("N"));
        assert_eq!(s.sub.as_deref(), Some("u1"));
        assert_eq!(s.client_id, "thclaws-internal");
    }

    #[test]
    fn build_session_handles_missing_id_token() {
        let resp = TokenResponse {
            access_token: "at".into(),
            id_token: None,
            refresh_token: None,
            expires_in: Some(900),
        };
        let s = build_session(resp, "https://example.com", "c");
        assert!(s.email.is_none());
        assert!(s.name.is_none());
        assert!(s.sub.is_none());
    }

    fn fixture_policy() -> SsoPolicy {
        SsoPolicy {
            enabled: true,
            provider: "oidc".into(),
            issuer_url: "https://accounts.google.com".into(),
            client_id: "test-client".into(),
            audience: None,
            client_secret: None,
            client_secret_env: None,
        }
    }

    #[test]
    fn resolve_client_secret_returns_none_when_unset() {
        let p = fixture_policy();
        assert!(resolve_client_secret(&p).is_none());
    }

    #[test]
    fn resolve_client_secret_reads_inline_literal() {
        let mut p = fixture_policy();
        p.client_secret = Some("GOCSPX-abcXYZ123".into());
        assert_eq!(
            resolve_client_secret(&p).as_deref(),
            Some("GOCSPX-abcXYZ123")
        );
    }

    #[test]
    fn resolve_client_secret_inline_wins_over_env() {
        // When both are set, inline takes priority — admin's explicit
        // intent is to use the literal, not whatever env var happens
        // to be set on the user's machine.
        std::env::set_var("THCLAWS_TEST_OIDC_LOSER", "from-env");
        let mut p = fixture_policy();
        p.client_secret = Some("from-policy".into());
        p.client_secret_env = Some("THCLAWS_TEST_OIDC_LOSER".into());
        assert_eq!(resolve_client_secret(&p).as_deref(), Some("from-policy"));
        std::env::remove_var("THCLAWS_TEST_OIDC_LOSER");
    }

    #[test]
    fn resolve_client_secret_reads_named_env_var() {
        std::env::set_var("THCLAWS_TEST_OIDC_SECRET", "shh-1234");
        let mut p = fixture_policy();
        p.client_secret_env = Some("THCLAWS_TEST_OIDC_SECRET".into());
        assert_eq!(resolve_client_secret(&p).as_deref(), Some("shh-1234"));
        std::env::remove_var("THCLAWS_TEST_OIDC_SECRET");
    }

    #[test]
    fn resolve_client_secret_inline_blank_falls_through_to_env() {
        // An inline value of `""` shouldn't authenticate as empty
        // string — it should fall through to the env var fallback.
        std::env::set_var("THCLAWS_TEST_OIDC_FALLBACK", "rescued");
        let mut p = fixture_policy();
        p.client_secret = Some("   ".into()); // whitespace-only
        p.client_secret_env = Some("THCLAWS_TEST_OIDC_FALLBACK".into());
        assert_eq!(resolve_client_secret(&p).as_deref(), Some("rescued"));
        std::env::remove_var("THCLAWS_TEST_OIDC_FALLBACK");
    }

    #[test]
    fn resolve_client_secret_treats_empty_env_as_unset() {
        std::env::set_var("THCLAWS_TEST_OIDC_BLANK", "");
        let mut p = fixture_policy();
        p.client_secret_env = Some("THCLAWS_TEST_OIDC_BLANK".into());
        assert!(resolve_client_secret(&p).is_none());
        std::env::remove_var("THCLAWS_TEST_OIDC_BLANK");
    }

    #[test]
    fn resolve_client_secret_treats_empty_var_name_as_unset() {
        let mut p = fixture_policy();
        p.client_secret_env = Some("".into());
        assert!(resolve_client_secret(&p).is_none());
    }

    #[test]
    fn generate_state_is_random_and_url_safe() {
        let a = generate_state();
        let b = generate_state();
        assert_ne!(a, b);
        for c in a.chars() {
            assert!(c.is_ascii_alphanumeric() || c == '-' || c == '_');
        }
    }
}
