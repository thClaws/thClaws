//! Browser sign-in for the cloud CLI token (loopback + PKCE).
//!
//! Opens `<cloud>/cli/authorize` in the user's browser with a loopback
//! redirect, a CSRF `state` and an S256 challenge. The user signs in there
//! the normal way (their IdP, the deployment's domain and test-user rules),
//! allows this device, and the browser returns to our loopback with a
//! one-time code, which we trade — with the verifier only we hold — for a
//! `thc_` token named after this machine. The token lands where
//! `cloud::token()` (and so the gateway credential) reads it.
//!
//! An enterprise deployment gives these tokens a lifetime; when the gateway
//! answers `token_expired`, signing in again is this same flow.

use serde_json::Value;

use crate::error::{Error, Result};
use crate::sso::loopback::{CallbackResult, LoopbackServer};
use crate::sso::pkce::PkcePair;

/// Long enough to sign in through an IdP with MFA; short enough that an
/// abandoned attempt does not hold a port open for long.
const CALLBACK_TIMEOUT_SECS: u64 = 300;

/// The authorize URL the browser opens.
pub fn authorize_url(
    cloud_url: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    device: &str,
) -> String {
    format!(
        "{}/cli/authorize?redirect_uri={}&state={}&code_challenge={}&device={}",
        cloud_url.trim_end_matches('/'),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(state),
        urlencoding::encode(challenge),
        urlencoding::encode(device),
    )
}

/// The code from the loopback callback, refusing a denial, a missing code,
/// or a `state` that is not ours (a forged callback).
pub fn callback_code(cb: &CallbackResult, expected_state: &str) -> Result<String> {
    if let Some(err) = cb.error.as_deref() {
        return Err(Error::Config(if err == "access_denied" {
            "sign-in was denied in the browser".into()
        } else {
            format!("sign-in failed: {err}")
        }));
    }
    if cb.state.as_deref() != Some(expected_state) {
        return Err(Error::Config(
            "sign-in callback state did not match — refusing it".into(),
        ));
    }
    cb.code
        .clone()
        .filter(|c| !c.is_empty())
        .ok_or_else(|| Error::Config("sign-in callback carried no code".into()))
}

/// This machine's name, for the token's label in the account's key list.
pub fn device_name() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "desktop".into());
    format!("thClaws on {host}").chars().take(80).collect()
}

/// Run the whole flow and store the token. Blocks on the browser (up to
/// `CALLBACK_TIMEOUT_SECS`); call it off the UI thread.
pub async fn login(cloud_url: &str) -> Result<()> {
    let started = std::time::Instant::now();
    let t = |msg: String| trace(&format!("{:>5.1}s {msg}", started.elapsed().as_secs_f32()));
    let result = login_traced(cloud_url, &t).await;
    match &result {
        Ok(()) => t("done — signed in".into()),
        Err(e) => t(format!("failed: {e}")),
    }
    result
}

/// One `[sign-in]` line on stderr, which the desktop keeps in
/// `.thclaws/state/logs/engine.log` (`/logs`). Never a token, only its
/// public prefix.
pub(crate) fn trace(msg: &str) {
    eprintln!("[sign-in pid={}] {msg}", std::process::id());
}

pub(crate) fn token_prefix(token: &str) -> String {
    token.chars().take(12).collect()
}

async fn login_traced(cloud_url: &str, t: &(dyn Fn(String) + Sync)) -> Result<()> {
    let (slot, _) = super::token_slot();
    t(format!(
        "start: cloud={cloud_url} slot={slot} backend={:?} (chosen={:?}) version={}",
        crate::secrets::resolved_backend(),
        crate::secrets::get_backend(),
        crate::desktop_update::client_value(),
    ));
    let server = LoopbackServer::bind()?;
    let redirect = server.redirect_uri();
    t(format!("listening on {redirect}"));
    let pkce = PkcePair::generate();
    let state = crate::sso::generate_state();
    let url = authorize_url(
        cloud_url,
        &redirect,
        &state,
        &pkce.challenge,
        &device_name(),
    );
    match crate::sso::open_browser(&url) {
        Ok(()) => t("browser opened; waiting for the callback".into()),
        Err(e) => {
            t(format!("could not open the browser ({e}); printed the URL"));
            eprintln!("Open this URL in your browser to sign in:\n  {url}");
        }
    }
    let cb = tokio::task::spawn_blocking(move || server.accept_one(CALLBACK_TIMEOUT_SECS))
        .await
        .map_err(|e| Error::Config(format!("sign-in listener: {e}")))??;
    let code = callback_code(&cb, &state)?;
    t("callback received, state matches; exchanging the code".into());

    let resp = crate::desktop_update::tag_cloud(reqwest::Client::new().post(format!(
        "{}/api/auth/cli-token/exchange",
        cloud_url.trim_end_matches('/')
    )))
    .json(&serde_json::json!({
        "code": code,
        "code_verifier": pkce.verifier,
        "redirect_uri": redirect,
    }))
    .send()
    .await
    .map_err(|e| Error::Config(format!("sign-in exchange: {}", error_chain(&e))))?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    t(format!("exchange answered {status}"));
    if let Some(msg) = exchange_update_required(status.as_u16(), &body) {
        return Err(Error::Config(msg));
    }
    if !status.is_success() {
        let detail = body
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or("refused");
        return Err(Error::Config(format!(
            "sign-in exchange {status}: {detail}"
        )));
    }
    let token = body
        .get("token")
        .and_then(Value::as_str)
        .filter(|t| super::looks_like_cli_token(t))
        .ok_or_else(|| Error::Config("sign-in exchange returned no token".into()))?;
    // A macOS keychain permission prompt blocks right here; a log that
    // stops at this line means one is waiting, maybe behind a window.
    t(format!("storing token {} …", token_prefix(token)));
    super::set_token(token)?;
    t(format!(
        "stored; this process now reads {}",
        match super::token() {
            Some(read) if read == token => "the new token".to_string(),
            Some(read) => format!("a DIFFERENT token {}", token_prefix(&read)),
            None => "NO token".to_string(),
        }
    ));
    Ok(())
}

/// reqwest's top-level message hides the cause ("error sending request");
/// the chain names it (DNS, refused, TLS certificate, timeout).
pub(crate) fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

/// What a sign-out managed to do. The local token is always gone afterwards;
/// the server-side revoke needs the cloud to be reachable (company network or
/// VPN), and without it the token stays valid until it expires or an admin
/// revokes it.
#[derive(Debug, Clone, PartialEq)]
pub enum SignOut {
    Revoked,
    NotSignedIn,
    LocalOnly(String),
}

/// Sign this machine out of `cloud_url`: revoke the token on the server
/// (`DELETE /api/auth/cli-tokens/current`), then forget it locally either way.
pub async fn sign_out(cloud_url: &str) -> Result<SignOut> {
    let Some(token) = super::token() else {
        return Ok(SignOut::NotSignedIn);
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Config(format!("http client: {e}")))?;
    let revoked = crate::desktop_update::tag_cloud(client.delete(format!(
        "{}/api/auth/cli-tokens/current",
        cloud_url.trim_end_matches('/')
    )))
    .bearer_auth(&token)
    .send()
    .await;
    super::clear_token()?;
    Ok(match revoked {
        // 401: already revoked or expired — nothing left to revoke.
        Ok(r) if r.status().is_success() || r.status().as_u16() == 401 => SignOut::Revoked,
        Ok(r) => SignOut::LocalOnly(format!("the server answered {}", r.status())),
        Err(e) => SignOut::LocalOnly(if e.is_connect() || e.is_timeout() {
            format!(
                "{} is unreachable — connect to the company network or VPN",
                cloud_url.trim_end_matches('/')
            )
        } else {
            e.to_string()
        }),
    })
}

/// The account this machine is signed in as, from `/api/auth/me`. `None`
/// when not signed in or the cloud cannot be reached.
pub async fn whoami(cloud_url: &str) -> Option<String> {
    match whoami_detail(cloud_url).await {
        Ok(email) => Some(email),
        Err(why) => {
            // The sidebar shows "Sign in" for every one of these; only the
            // log tells "no token" from "token, but the cloud said no".
            trace(&format!("whoami: not signed in — {why}"));
            None
        }
    }
}

/// [`whoami`], with the reason when it is not an email.
pub async fn whoami_detail(cloud_url: &str) -> std::result::Result<String, String> {
    let token = super::token().ok_or("no token stored for this cloud")?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = crate::desktop_update::tag_cloud(
        client.get(format!("{}/api/auth/me", cloud_url.trim_end_matches('/'))),
    )
    .bearer_auth(&token)
    .send()
    .await
    .map_err(|e| {
        format!(
            "token {}: {} unreachable: {}",
            token_prefix(&token),
            cloud_url,
            error_chain(&e)
        )
    })?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let body: String = body.chars().take(200).collect();
        return Err(format!(
            "token {}: /api/auth/me answered {status} {body}",
            token_prefix(&token)
        ));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("/api/auth/me body: {e}"))?;
    body.get("email")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "/api/auth/me carried no email".to_string())
}

/// `/cloud doctor`: every link between "the browser said Allowed" and
/// "the sidebar shows the account", checked live. Each line also goes to
/// the log, so a tester's `/logs` carries the same report.
pub async fn doctor_lines(cloud_url: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |line: String| {
        trace(&format!("doctor: {line}"));
        out.push(line);
    };
    let short = |v: &Option<String>| {
        v.as_deref()
            .map(token_prefix)
            .unwrap_or_else(|| "(none)".into())
    };

    add(format!(
        "Build:      {} (pid {})",
        crate::desktop_update::client_value(),
        std::process::id()
    ));
    add(format!(
        "Cloud:      {cloud_url}{}",
        if crate::policy::thclaws_cloud_url().is_some() {
            " (from org policy)"
        } else {
            ""
        }
    ));
    add(format!(
        "Gateway:    {}",
        if crate::shared::gateway_providers_locked() {
            "locked to the org gateway (needs the sign-in token)"
        } else {
            "not locked"
        }
    ));

    add(format!(
        "Profile:    {}{}",
        crate::profile::config_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| crate::profile::app_dir_name().to_string()),
        match crate::profile::customer() {
            Some(c) => format!(
                " (customer build `{c}`; keychain service `{}`)",
                crate::profile::keychain_service()
            ),
            None => String::new(),
        }
    ));
    let (slot, env) = super::token_slot();
    add(format!(
        "Storage:    {:?} (chosen: {}) — keychain item `{slot}`, env `{env}`",
        crate::secrets::resolved_backend(),
        crate::secrets::get_backend()
            .map(|b| b.as_str().to_string())
            .unwrap_or_else(|| "never — the first-run storage question was not answered".into()),
    ));
    let from_env = std::env::var(&env).ok().filter(|t| !t.trim().is_empty());
    add(format!("  env var:          {}", short(&from_env)));
    let uses_keychain = crate::secrets::resolved_backend() == crate::secrets::Backend::Keychain;
    let mut keychain_answered = true;
    let (mut cached, mut fresh_token) = (None, None);
    if !uses_keychain {
        add(format!(
            "  keychain:         not used (token lives in {})",
            crate::dotenv::user_dotenv_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "the user .env".into())
        ));
    } else {
        // Both keychain reads off the async thread, with a deadline: a macOS
        // permission prompt blocks them until someone answers it. "live" reads
        // past this process's cache.
        let slot_c = slot.clone();
        let reads = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                (
                    crate::secrets::get(&slot_c).filter(|t| !t.trim().is_empty()),
                    crate::secrets::bundle_get_uncached(&slot_c),
                )
            }),
        )
        .await;
        keychain_answered = matches!(reads, Ok(Ok(_)));
        (cached, fresh_token) = match reads {
            Ok(Ok((cached, fresh))) => {
                add(format!("  keychain (cache): {}", short(&cached)));
                let fresh = match fresh {
                    Ok(v) => {
                        let v = v.filter(|t| !t.trim().is_empty());
                        add(format!("  keychain (live):  {}", short(&v)));
                        v
                    }
                    Err(e) => {
                        add(format!("  keychain (live):  error — {e}"));
                        None
                    }
                };
                (cached, fresh)
            }
            Ok(Err(e)) => {
                add(format!("  keychain:         read panicked — {e}"));
                (None, None)
            }
            Err(_) => {
                add("  keychain:         no answer in 10s — a macOS keychain prompt is probably waiting (check behind other windows)".into());
                (None, None)
            }
        };
    }
    if fresh_token.is_some() && fresh_token != cached {
        add("  ⚠ the keychain holds a token this process has not seen (signed in from another window/agent?) — quit and reopen the app".into());
    }
    // Everything below reads the token again; with the keychain stuck on a
    // prompt that would hang the report, so it stops at what is known.
    if keychain_answered {
        let resolved = super::token();
        add(format!("Token used: {}", short(&resolved)));
        let cfg = crate::config::AppConfig::load().unwrap_or_default();
        add(format!(
            "Model:      {} → ready: {}",
            cfg.model,
            crate::providers::provider_has_credentials(&cfg)
        ));
    }

    for var in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ] {
        if let Ok(v) = std::env::var(var) {
            add(format!("Proxy:      {var}={v}"));
        }
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    if let Ok(client) = client {
        let started = std::time::Instant::now();
        let probe = client
            .get(format!(
                "{}/api/auth/providers",
                cloud_url.trim_end_matches('/')
            ))
            .send()
            .await;
        add(match probe {
            Ok(r) => format!(
                "Reach:      {} in {} ms",
                r.status(),
                started.elapsed().as_millis()
            ),
            Err(e) => format!("Reach:      FAILED — {}", error_chain(&e)),
        });
    }
    if keychain_answered {
        add(match whoami_detail(cloud_url).await {
            Ok(email) => format!("Account:    {email}"),
            Err(why) => format!("Account:    not signed in — {why}"),
        });
    }
    out
}

/// `GET /api/me/quota` for the signed-in user. `None` when signed out, on
/// any failure, or on 404 (the install doesn't bill by quota).
pub async fn quota(cloud_url: &str) -> Option<Value> {
    let token = super::token()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let resp = crate::desktop_update::tag_cloud(
        client.get(format!("{}/api/me/quota", cloud_url.trim_end_matches('/'))),
    )
    .bearer_auth(token)
    .send()
    .await
    .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

/// The `cloud_quota_result` frame: the api's body in quota mode (credits)
/// or plan mode (% of a 5-hour and a 7-day window), else `quota: null` so
/// the sidebar hides the line.
pub fn quota_payload(body: Option<Value>) -> Value {
    let quota = body.filter(|b| match b.get("mode").and_then(Value::as_str) {
        Some("quota") => b.get("month").is_some_and(|m| !m.is_null()),
        Some("plan") => b.get("windows").is_some_and(|w| w.is_object()),
        _ => false,
    });
    serde_json::json!({ "type": "cloud_quota_result", "quota": quota })
}

/// The exchange's 426 refusal of this build, humanized like the gateway's.
pub fn exchange_update_required(status: u16, body: &Value) -> Option<String> {
    if status != 426 {
        return None;
    }
    let raw = body.to_string();
    let raw = if crate::desktop_update::is_client_update_required(&raw) {
        raw
    } else {
        r#"{"error":{"type":"client_update_required"}}"#.to_string()
    };
    crate::desktop_update::note_error(&raw);
    crate::desktop_update::client_update_message(&raw)
}

/// The gateway's machine-readable refusal for a lapsed CLI token.
pub fn is_token_expired(body: &str) -> bool {
    body.contains("\"token_expired\"")
}

/// Advice when the token has lapsed, naming both ways to sign in again.
pub fn relogin_hint(cloud_url: &str) -> String {
    format!(
        "Your sign-in to {} has expired. Sign in again: Settings → Sign in with browser, \
         or run `thclaws cloud login --browser`.",
        cloud_url.trim_end_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_payload_passes_quota_mode_through_and_hides_the_rest() {
        let body = serde_json::json!({
            "mode": "quota", "credits_per_usd": 100,
            "month": {"used_credits": 350, "limit_credits": 1000, "resets_at": "2026-10-31T17:00:00Z"},
            "day": {"used_credits": 40, "limit_credits": null, "resets_at": "2026-10-01T17:00:00Z"},
            "rate": {"rps": 5, "burst": 10}
        });
        let p = quota_payload(Some(body));
        assert_eq!(p["type"], "cloud_quota_result");
        assert_eq!(p["quota"]["month"]["used_credits"], 350);
        assert_eq!(p["quota"]["month"]["limit_credits"], 1000);
        assert!(p["quota"]["day"]["limit_credits"].is_null());
        assert!(quota_payload(None)["quota"].is_null());
        let other = serde_json::json!({"mode": "credit", "month": {}});
        assert!(quota_payload(Some(other))["quota"].is_null());

        let plan = serde_json::json!({
            "mode": "plan", "plan": {"tier": "plan-pro", "label": "Pro", "units": 5},
            "unlimited": false, "warn_pct": 80, "month": null, "day": null, "rate": null,
            "windows": {"5h": {"used_pct": 37, "resets_at": "2026-10-29T12:00:00Z"},
                        "7d": {"used_pct": 12, "resets_at": null}}
        });
        let p = quota_payload(Some(plan));
        assert_eq!(p["quota"]["windows"]["5h"]["used_pct"], 37);
        assert!(p["quota"]["windows"]["7d"]["resets_at"].is_null());
    }

    fn cb(code: Option<&str>, state: Option<&str>, error: Option<&str>) -> CallbackResult {
        CallbackResult {
            code: code.map(String::from),
            state: state.map(String::from),
            error: error.map(String::from),
            error_description: None,
        }
    }

    #[test]
    fn authorize_url_encodes_every_part() {
        let u = authorize_url(
            "https://sis.thclaws.ai/",
            "http://localhost:5555/",
            "s t",
            "abc",
            "thClaws on Jimmy's Mac",
        );
        assert!(u.starts_with(
            "https://sis.thclaws.ai/cli/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A5555%2F&"
        ));
        assert!(u.contains("&state=s%20t&"));
        assert!(u.contains("&device=thClaws%20on%20Jimmy%27s%20Mac"));
    }

    #[test]
    fn callback_needs_our_state_and_a_code() {
        assert_eq!(
            callback_code(&cb(Some("c1"), Some("st"), None), "st").unwrap(),
            "c1"
        );
        assert!(callback_code(&cb(Some("c1"), Some("other"), None), "st").is_err());
        assert!(callback_code(&cb(Some("c1"), None, None), "st").is_err());
        assert!(callback_code(&cb(None, Some("st"), None), "st").is_err());
        let denied = callback_code(&cb(None, Some("st"), Some("access_denied")), "st");
        assert!(denied.unwrap_err().to_string().contains("denied"));
    }

    #[test]
    fn token_expired_is_the_gateway_code_only() {
        assert!(is_token_expired(
            r#"{"error":"Your sign-in has expired","code":"token_expired"}"#
        ));
        assert!(!is_token_expired(r#"{"error":"unknown or revoked key"}"#));
        assert!(!is_token_expired(r#"{"code":"access_revoked"}"#));
    }

    #[test]
    fn exchange_426_surfaces_the_update_message() {
        let body = serde_json::json!({"error": {
            "type": "client_update_required", "message": "m",
            "min_version": "0.140.0", "min_built_at": null,
            "download_url": "https://sis.thclaws.ai/download",
            "your_version": "0.139.0+abcdef1"}});
        let msg = exchange_update_required(426, &body).unwrap();
        assert!(msg.contains("(0.139.0+abcdef1) is no longer allowed by your organisation"));
        assert!(msg.ends_with("Download the update: https://sis.thclaws.ai/download"));
        // FastAPI may wrap it in `detail`; a bare 426 still explains itself.
        let wrapped = serde_json::json!({"detail": body});
        assert!(exchange_update_required(426, &wrapped)
            .unwrap()
            .contains("sis.thclaws.ai/download"));
        assert!(exchange_update_required(426, &Value::Null)
            .unwrap()
            .contains("no longer allowed"));
        assert!(exchange_update_required(400, &body).is_none());
    }

    #[test]
    fn device_name_is_bounded() {
        let d = device_name();
        assert!(d.starts_with("thClaws on "));
        assert!(d.chars().count() <= 80);
    }
}
