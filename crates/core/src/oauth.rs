//! OAuth 2.1 + PKCE for MCP HTTP servers.
//!
//! Flow (per MCP spec / RFC 9728):
//!
//!   1. POST to MCP server without auth → 401 + WWW-Authenticate header
//!      containing `resource_metadata` URL.
//!   2. GET `/.well-known/oauth-protected-resource` → `authorization_servers[]`.
//!   3. GET `<auth-server>/.well-known/oauth-authorization-server` → RFC 8414
//!      metadata (authorize_endpoint, token_endpoint, etc.).
//!   4. Generate PKCE code_verifier + code_challenge.
//!   5. Open browser to `authorize_endpoint` with redirect to a local
//!      ephemeral HTTP server.
//!   6. User consents; browser redirects to our callback with `?code=...`.
//!   7. Exchange code + code_verifier for access_token + refresh_token.
//!   8. Store tokens to `~/.config/thclaws/oauth_tokens.json`.
//!   9. Attach `Authorization: Bearer <at>` to every subsequent MCP POST.
//!  10. On 401 during a session, try refresh_token → new access_token.

use crate::error::{Error, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

const CALLBACK_PORT_START: u16 = 19150;
const CALLBACK_PORT_END: u16 = 19160;
/// Fallback client_id used when the authorization server does NOT advertise
/// a `registration_endpoint`. When DCR is supported (the MCP spec norm) we
/// POST `/register` (RFC 7591) and use the issued client_id instead.
const CLIENT_ID: &str = "thclaws";
const CLIENT_NAME: &str = "thClaws";

// ── Token storage ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenStore {
    /// Keyed by MCP server URL (the resource endpoint, not the auth server).
    pub tokens: HashMap<String, TokenEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenEntry {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_endpoint: String,
    /// Unix timestamp when the access token expires (0 = unknown).
    pub expires_at: u64,
    /// Origin (scheme+host+port) of the authorization server that issued
    /// this token. Used on retrieval to reject token reuse if an attacker
    /// swaps the MCP server's OAuth discovery to point at a different
    /// authorization server. `None` on entries saved before this field
    /// existed; such entries are treated as unvalidated and re-auth is
    /// forced.
    #[serde(default)]
    pub authorization_server: Option<String>,
    /// Client ID used to obtain this token. For RFC 7591 dynamically-
    /// registered clients we MUST send the same `client_id` to the token
    /// endpoint when refreshing — the static fallback would be rejected.
    /// `None` on entries saved before DCR support existed.
    #[serde(default)]
    pub client_id: Option<String>,
    /// Client secret issued by DCR, if any. Public PKCE clients usually
    /// receive `None`; confidential clients receive a secret that must be
    /// presented on token exchange / refresh.
    #[serde(default)]
    pub client_secret: Option<String>,
}

impl TokenStore {
    fn path() -> Option<PathBuf> {
        crate::util::home_dir().map(|h| h.join(".config/thclaws/oauth_tokens.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Serialise + write with `0600` permissions on Unix so other users /
    /// processes on the machine can't read cached refresh tokens. On
    /// Windows we rely on the default per-user ACL.
    ///
    /// Ordering is critical:
    ///   1. Ensure parent dir exists and is `0700` (so a sibling process
    ///      can't have snuck a world-readable `oauth_tokens.json` in
    ///      before we got here; also prevents directory-listing recon).
    ///   2. Open the file. If it exists, `mode(0o600)` on `open` is
    ///      ignored — so we must tighten perms *before* writing secrets.
    ///   3. Chmod `0600` via `set_permissions`.
    ///   4. Only then write the token bytes.
    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(parent) {
                    let mut perms = meta.permissions();
                    perms.set_mode(0o700);
                    let _ = std::fs::set_permissions(parent, perms);
                }
            }
        }
        let contents = serde_json::to_string_pretty(self).unwrap_or_default();

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            use std::os::unix::fs::PermissionsExt;
            // Open with 0600 on creation. If the file pre-existed under a
            // looser mode (sibling process, stale umask), tighten BEFORE
            // writing secrets. `mode()` on `open` is ignored when the
            // file already exists, so `set_permissions` is the real gate.
            let open_result = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path);
            if let Ok(mut f) = open_result {
                if let Ok(meta) = std::fs::metadata(&path) {
                    let mut perms = meta.permissions();
                    perms.set_mode(0o600);
                    let _ = std::fs::set_permissions(&path, perms);
                }
                use std::io::Write;
                let _ = f.write_all(contents.as_bytes());
            }
        }

        #[cfg(not(unix))]
        {
            let _ = std::fs::write(&path, &contents);
        }
    }

    pub fn get(&self, server_url: &str) -> Option<&TokenEntry> {
        self.tokens.get(server_url)
    }

    /// Retrieve a cached token entry only if its authorization server
    /// matches the currently-discovered one. Callers that just did
    /// OAuth discovery can pass the `expected_as_origin` to defend
    /// against cross-server token reuse (e.g. DNS hijack swapping the
    /// OAuth server underneath a previously-trusted MCP URL).
    pub fn get_validated(&self, server_url: &str, expected_as_origin: &str) -> Option<&TokenEntry> {
        let entry = self.tokens.get(server_url)?;
        match entry.authorization_server.as_deref() {
            Some(origin) if origin == expected_as_origin => Some(entry),
            _ => None,
        }
    }

    pub fn set(&mut self, server_url: &str, entry: TokenEntry) {
        self.tokens.insert(server_url.to_string(), entry);
        self.save();
    }

    pub fn remove(&mut self, server_url: &str) {
        self.tokens.remove(server_url);
        self.save();
    }
}

// ── Discovery ────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct OAuthMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
    /// Origin (scheme://host[:port]) of the authorization server that
    /// advertised these endpoints. Cached alongside issued tokens so a
    /// later discovery that points at a *different* AS cannot silently
    /// reuse a previously-issued token.
    pub authorization_server_origin: String,
}

/// Extract a scheme+host+port origin from a URL string. Fails hard on
/// unparseable input — accepting a garbage fingerprint would let an
/// attacker controlling the AS metadata match arbitrary strings on
/// subsequent discoveries, silently defeating the AS-binding check.
fn origin_of(url_str: &str) -> Result<String> {
    let u = url::Url::parse(url_str)
        .map_err(|e| Error::Provider(format!("oauth: cannot parse AS url '{url_str}': {e}")))?;
    let host = u
        .host_str()
        .ok_or_else(|| Error::Provider(format!("oauth: AS url '{url_str}' has no host")))?;
    Ok(match u.port() {
        Some(p) => format!("{}://{}:{}", u.scheme(), host, p),
        None => format!("{}://{}", u.scheme(), host),
    })
}

/// Discover the OAuth authorization server for an MCP HTTP endpoint.
/// Returns (resource_metadata_url, auth_server_url, metadata).
pub async fn discover(client: &Client, mcp_url: &str) -> Result<OAuthMetadata> {
    // Step 1: derive the server origin from the MCP URL and fetch the
    // resource metadata at the well-known path. The well-known document
    // lives at the server ROOT, not under the MCP path.
    //   https://mcp.artech.cloud/mcp/  →  https://mcp.artech.cloud
    let origin = match url::Url::parse(mcp_url) {
        Ok(u) => format!("{}://{}", u.scheme(), u.host_str().unwrap_or("localhost")),
        Err(_) => mcp_url
            .trim_end_matches('/')
            .rsplit_once("/mcp")
            .map(|(base, _)| base.to_string())
            .unwrap_or_else(|| mcp_url.trim_end_matches('/').to_string()),
    };
    let resource_meta_url = format!("{origin}/.well-known/oauth-protected-resource");
    eprintln!("\x1b[2m[oauth] fetching {resource_meta_url}\x1b[0m");

    let resource_resp = client
        .get(&resource_meta_url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("oauth discovery: {e}")))?;
    let resource: Value = resource_resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("oauth resource metadata: {e}")))?;

    let auth_server = resource
        .get("authorization_servers")
        .and_then(|a| a.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Error::Provider("oauth: no authorization_servers in resource metadata".into())
        })?
        .to_string();

    // Step 2: fetch the auth server's RFC 8414 metadata.
    let meta_url = format!(
        "{}/.well-known/oauth-authorization-server",
        auth_server.trim_end_matches('/')
    );
    let meta_resp = client
        .get(&meta_url)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("oauth server metadata: {e}")))?;
    let meta: Value = meta_resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("oauth server metadata json: {e}")))?;

    let authorization_endpoint = meta
        .get("authorization_endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Provider("missing authorization_endpoint".into()))?
        .to_string();
    let token_endpoint = meta
        .get("token_endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Provider("missing token_endpoint".into()))?
        .to_string();
    let registration_endpoint = meta
        .get("registration_endpoint")
        .and_then(|v| v.as_str())
        .map(String::from);
    let scopes_supported: Vec<String> = meta
        .get("scopes_supported")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let authorization_server_origin = origin_of(&auth_server)?;

    Ok(OAuthMetadata {
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
        scopes_supported,
        authorization_server_origin,
    })
}

// ── PKCE ─────────────────────────────────────────────────────────────

/// Fill `buf` with cryptographically-random bytes. OAuth state + PKCE
/// security depends on this — if the CSPRNG is unavailable we hard-fail
/// rather than silently degrading to a timestamp-derived value that an
/// attacker on the same machine could predict within microseconds.
fn secure_random(buf: &mut [u8]) -> Result<()> {
    getrandom::getrandom(buf).map_err(|e| {
        Error::Provider(format!(
            "OS CSPRNG unavailable for OAuth security: {e}. Refusing to proceed."
        ))
    })
}

fn generate_pkce() -> Result<(String, String)> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use sha2::{Digest, Sha256};

    let mut verifier_bytes = [0u8; 32];
    secure_random(&mut verifier_bytes)?;
    let code_verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);

    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

    Ok((code_verifier, code_challenge))
}

/// 128-bit (16-byte) random token encoded as URL-safe base64. Used for
/// the OAuth `state` parameter. 64 bits (prior implementation) was below
/// the RFC 6234 recommendation and brute-force / precompute-feasible.
fn generate_state() -> Result<String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let mut bytes = [0u8; 16];
    secure_random(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

// ── Dynamic Client Registration (RFC 7591) ──────────────────────────

/// Register a new OAuth client at the AS's `registration_endpoint`. Returns
/// `(client_id, Option<client_secret>)`. We register fresh per browser flow
/// with the *exact* loopback redirect_uri we just bound — strict ASes
/// reject `/authorize` if the redirect_uri wasn't registered, and RFC 8252
/// port-flexibility for loopback URIs is only a SHOULD, not honored
/// universally. Re-registering each flow trades a small amount of AS-side
/// churn for reliability across heterogeneous MCP servers.
async fn register_dynamic_client(
    client: &Client,
    registration_endpoint: &str,
    redirect_uri: &str,
) -> Result<(String, Option<String>)> {
    let body = serde_json::json!({
        "client_name": CLIENT_NAME,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": "native",
    });
    let resp = client
        .post(registration_endpoint)
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("oauth register: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::Provider(format!(
            "oauth register failed: {status} {text}"
        )));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("oauth register json: {e}")))?;
    let client_id = v
        .get("client_id")
        .and_then(|x| x.as_str())
        .ok_or_else(|| Error::Provider("register response missing client_id".into()))?
        .to_string();
    let client_secret = v
        .get("client_secret")
        .and_then(|x| x.as_str())
        .map(String::from);
    Ok((client_id, client_secret))
}

// ── Authorization flow ───────────────────────────────────────────────

/// Run the full OAuth 2.1 + PKCE browser flow. Opens a browser, waits for
/// the callback on a local ephemeral HTTP server, exchanges the code for
/// tokens, and returns a `TokenEntry`. The caller is responsible for storing
/// it in the `TokenStore`.
pub async fn authorize(
    client: &Client,
    meta: &OAuthMetadata,
    _mcp_url: &str,
) -> Result<TokenEntry> {
    let (code_verifier, code_challenge) = generate_pkce()?;
    let state = generate_state()?;

    // Find a free local port for the callback server.
    let listener = find_listener().await?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::Provider(format!("callback addr: {e}")))?
        .port();
    let redirect_uri = format!("http://localhost:{port}/callback");

    // RFC 7591 dynamic client registration BEFORE /authorize. Required by
    // the MCP spec when the AS advertises a `registration_endpoint`; falls
    // back to the static client_id only when DCR is unavailable.
    let (effective_client_id, client_secret) =
        if let Some(reg_url) = meta.registration_endpoint.as_deref() {
            eprintln!("\x1b[2m[oauth] registering client at {reg_url}\x1b[0m");
            match register_dynamic_client(client, reg_url, &redirect_uri).await {
                Ok((cid, secret)) => {
                    eprintln!(
                        "\x1b[2m[oauth] registered client_id={cid}{}\x1b[0m",
                        if secret.is_some() { " (+secret)" } else { "" }
                    );
                    (cid, secret)
                }
                Err(e) => {
                    eprintln!(
                        "\x1b[33m[oauth] DCR failed ({e}); falling back to static client_id\x1b[0m"
                    );
                    (CLIENT_ID.to_string(), None)
                }
            }
        } else {
            (CLIENT_ID.to_string(), None)
        };

    // Build the authorization URL.
    let scope = if meta.scopes_supported.is_empty() {
        "hosting:read hosting:write deploy:write".to_string()
    } else {
        meta.scopes_supported.join(" ")
    };
    let auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}\
         &code_challenge={}&code_challenge_method=S256",
        meta.authorization_endpoint,
        urlencoding::encode(&effective_client_id),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(&scope),
        urlencoding::encode(&state),
        urlencoding::encode(&code_challenge),
    );

    eprintln!("\x1b[36m[oauth] opening browser for authorization…\x1b[0m");
    open_browser(&auth_url);

    // Wait for the callback.
    let code = wait_for_callback(listener, &state).await?;

    eprintln!("\x1b[36m[oauth] exchanging code for tokens…\x1b[0m");

    // Exchange code for tokens. For confidential clients (DCR-issued
    // secret) authenticate via Basic auth on the token endpoint, per
    // RFC 6749 §2.3.1; the body still carries client_id for parity.
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("redirect_uri", &redirect_uri),
        ("client_id", &effective_client_id),
        ("code_verifier", &code_verifier),
    ];
    if let Some(s) = client_secret.as_deref() {
        form.push(("client_secret", s));
    }
    let token_resp = client
        .post(&meta.token_endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("token exchange: {e}")))?;

    if !token_resp.status().is_success() {
        let text = token_resp.text().await.unwrap_or_default();
        return Err(Error::Provider(format!("token exchange failed: {text}")));
    }

    let tv: Value = token_resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("token json: {e}")))?;

    let access_token = tv
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Provider("token response missing access_token".into()))?
        .to_string();
    let refresh_token = tv
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(String::from);
    let expires_in = tv
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(3600);

    // Validate granted scope. RFC 6749 §5.1 says the `scope` field is
    // OPTIONAL in the response — if present, it's the set actually
    // granted. If it differs from what we asked for, warn loudly so
    // the user notices a server that silently narrowed / widened the
    // grant; we accept the narrower set either way (we have no way to
    // require the wider).
    if let Some(granted) = tv.get("scope").and_then(|v| v.as_str()) {
        let requested: std::collections::HashSet<&str> = scope.split_whitespace().collect();
        let got: std::collections::HashSet<&str> = granted.split_whitespace().collect();
        let missing: Vec<&str> = requested.difference(&got).copied().collect();
        let extra: Vec<&str> = got.difference(&requested).copied().collect();
        if !missing.is_empty() || !extra.is_empty() {
            eprintln!(
                "\x1b[33m[oauth] scope mismatch — requested: [{}], granted: [{}]\x1b[0m",
                scope, granted
            );
            if !missing.is_empty() {
                eprintln!(
                    "\x1b[33m  missing from grant: {}\x1b[0m",
                    missing.join(", ")
                );
            }
            if !extra.is_empty() {
                eprintln!(
                    "\x1b[33m  server added: {} (unexpected — verify AS is legit)\x1b[0m",
                    extra.join(", ")
                );
            }
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    eprintln!(
        "\x1b[32m[oauth] authorized successfully\x1b[0m\n\x1b[2m  token ({}B): {}…\n  has_refresh: {}\x1b[0m",
        access_token.len(),
        &access_token[..access_token.len().min(40)],
        refresh_token.is_some()
    );

    Ok(TokenEntry {
        access_token,
        refresh_token,
        token_endpoint: meta.token_endpoint.clone(),
        expires_at: now + expires_in,
        authorization_server: Some(meta.authorization_server_origin.clone()),
        client_id: Some(effective_client_id),
        client_secret,
    })
}

/// Try to refresh an expired token. Returns a new TokenEntry on success.
pub async fn refresh(client: &Client, entry: &TokenEntry) -> Result<TokenEntry> {
    let refresh_token = entry
        .refresh_token
        .as_ref()
        .ok_or_else(|| Error::Provider("no refresh_token available".into()))?;

    // Use the client_id this token was issued under. DCR-registered
    // clients have unique IDs that the AS will not honor under the
    // static fallback.
    let cid = entry.client_id.as_deref().unwrap_or(CLIENT_ID);
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", cid),
    ];
    if let Some(s) = entry.client_secret.as_deref() {
        form.push(("client_secret", s));
    }
    let resp = client
        .post(&entry.token_endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("token refresh: {e}")))?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::Provider(format!("token refresh failed: {text}")));
    }

    let tv: Value = resp
        .json()
        .await
        .map_err(|e| Error::Provider(format!("refresh json: {e}")))?;

    let access_token = tv
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Provider("refresh response missing access_token".into()))?
        .to_string();
    let new_refresh = tv
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| entry.refresh_token.clone());
    let expires_in = tv
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(3600);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Ok(TokenEntry {
        access_token,
        refresh_token: new_refresh,
        token_endpoint: entry.token_endpoint.clone(),
        expires_at: now + expires_in,
        authorization_server: entry.authorization_server.clone(),
        client_id: entry.client_id.clone(),
        client_secret: entry.client_secret.clone(),
    })
}

/// Clock-skew safety margin. We proactively treat a token as expired
/// this many seconds before its real expiry so a clock drift between
/// us and the authorization server doesn't leave us waving a token
/// the server has just rejected. 5 minutes covers typical NTP drift
/// on consumer machines and matches common JWT `nbf`/`exp` tolerances.
const TOKEN_SKEW_MARGIN_SECS: u64 = 300;

/// Check whether a token entry is still valid.
pub fn is_valid(entry: &TokenEntry) -> bool {
    if entry.access_token.is_empty() {
        return false;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    entry.expires_at == 0 || now + TOKEN_SKEW_MARGIN_SECS < entry.expires_at
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Bind the OAuth callback server. Prefer an OS-assigned ephemeral port
/// (port 0) so another local process can't predict our callback port and
/// race us to claim it between runs. Fall back to the historical fixed
/// range only if the ephemeral bind fails (e.g. tight sandbox / firewall
/// policy on the loopback interface).
async fn find_listener() -> Result<tokio::net::TcpListener> {
    if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", 0u16)).await {
        return Ok(l);
    }
    for port in CALLBACK_PORT_START..=CALLBACK_PORT_END {
        if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            return Ok(l);
        }
    }
    Err(Error::Provider(format!(
        "oauth: could not bind callback server (ephemeral or ports {CALLBACK_PORT_START}-{CALLBACK_PORT_END})"
    )))
}

async fn wait_for_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut stream, _addr) =
        tokio::time::timeout(std::time::Duration::from_secs(300), listener.accept())
            .await
            .map_err(|_| {
                Error::Provider("oauth: timed out waiting for browser callback (5 min)".into())
            })?
            .map_err(|e| Error::Provider(format!("oauth accept: {e}")))?;

    let mut buf = vec![0u8; 4096];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| Error::Provider(format!("oauth read: {e}")))?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse the GET /callback?code=...&state=... line.
    let first_line = request.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("");
    let query = path.split('?').nth(1).unwrap_or("");
    let params: HashMap<&str, &str> = query
        .split('&')
        .filter_map(|p| {
            let mut kv = p.splitn(2, '=');
            Some((kv.next()?, kv.next().unwrap_or("")))
        })
        .collect();

    // Send a user-friendly response.
    let html = "<html><body style='font-family:system-ui;text-align:center;margin-top:80px'>\
                <h2>Authorized!</h2><p>You can close this tab and return to thClaws.</p>\
                </body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    let _ = stream.write_all(response.as_bytes()).await;

    // Validate state.
    let state = *params.get("state").unwrap_or(&"");
    if state != expected_state {
        return Err(Error::Provider(format!(
            "oauth: state mismatch (CSRF protection). Expected {expected_state}, got {state}"
        )));
    }

    if let Some(error) = params.get("error") {
        let desc = params.get("error_description").unwrap_or(&"");
        return Err(Error::Provider(format!("oauth denied: {error} {desc}")));
    }

    params
        .get("code")
        .map(|c| c.to_string())
        .ok_or_else(|| Error::Provider("oauth: callback missing 'code' parameter".into()))
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", url])
            .creation_flags(0x08000000)
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct HomeGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
        _dir: tempfile::TempDir,
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn scoped_home() -> HomeGuard {
        // Share the crate-wide env lock with kms tests to avoid racing
        // each other on HOME / cwd.
        let lock = crate::kms::test_env_lock();
        let prev = std::env::var("HOME").ok();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        HomeGuard {
            _lock: lock,
            prev,
            _dir: dir,
        }
    }

    fn sample_entry(issuer: Option<&str>) -> TokenEntry {
        TokenEntry {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            token_endpoint: "https://as.example/token".into(),
            expires_at: 0,
            authorization_server: issuer.map(String::from),
            client_id: None,
            client_secret: None,
        }
    }

    #[test]
    fn origin_of_rejects_unparseable_urls() {
        assert!(origin_of("not a url at all").is_err());
        assert!(origin_of("").is_err());
        // Missing host → still reject.
        assert!(origin_of("file:///etc/passwd").is_err());
        // Happy paths.
        assert_eq!(
            origin_of("https://as.example/foo").unwrap(),
            "https://as.example"
        );
        assert_eq!(
            origin_of("https://as.example:9443/foo").unwrap(),
            "https://as.example:9443"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_tightens_parent_dir_to_0700() {
        use std::os::unix::fs::PermissionsExt;
        let _home = scoped_home();
        // Pre-create parent dir with loose perms to simulate a
        // sibling process that got there first.
        let path = TokenStore::path().unwrap();
        let parent = path.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        let mut loose = std::fs::metadata(parent).unwrap().permissions();
        loose.set_mode(0o755);
        std::fs::set_permissions(parent, loose).unwrap();

        let mut store = TokenStore::default();
        store.set(
            "https://mcp.example",
            sample_entry(Some("https://as.example")),
        );

        let dir_mode = std::fs::metadata(parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "parent dir must be clamped to 0700");
    }

    #[test]
    fn state_is_high_entropy_and_unique() {
        let a = generate_state().unwrap();
        let b = generate_state().unwrap();
        assert_ne!(a, b);
        // 16 bytes → base64url without padding = 22 chars
        assert!(a.len() >= 22);
        assert!(b.len() >= 22);
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_file_with_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let _home = scoped_home();
        let mut store = TokenStore::default();
        store.set(
            "https://mcp.example",
            sample_entry(Some("https://as.example")),
        );
        let path = TokenStore::path().unwrap();
        assert!(path.exists(), "token file should have been created");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "token file must be 0600, got {mode:o}");
    }

    #[test]
    fn get_validated_rejects_mismatched_authorization_server() {
        let _home = scoped_home();
        let mut store = TokenStore::default();
        store.set(
            "https://mcp.example",
            sample_entry(Some("https://as-legit.example")),
        );
        // Same URL, different AS origin → must be treated as missing.
        assert!(store
            .get_validated("https://mcp.example", "https://as-legit.example")
            .is_some());
        assert!(store
            .get_validated("https://mcp.example", "https://as-evil.example")
            .is_none());
    }

    #[test]
    fn get_validated_rejects_legacy_entries_without_issuer() {
        let _home = scoped_home();
        let mut store = TokenStore::default();
        store.set("https://mcp.example", sample_entry(None));
        assert!(store
            .get_validated("https://mcp.example", "https://as.example")
            .is_none());
        // But the raw `get` still returns it so we can garbage-collect.
        assert!(store.get("https://mcp.example").is_some());
    }
}
