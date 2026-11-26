//! Discovery of a co-located AI Server (the DGX Spark appliance).
//!
//! thClaws ships on the same box as AI Server and starts with `--serve`, so
//! it should find the local LLM gateway by itself rather than making the
//! operator paste a base URL. AI Server answers that in one request:
//!
//! ```text
//! GET http://127.0.0.1:9000/.well-known/aiserver
//! ```
//!
//! The endpoint replies **only to loopback** (403 otherwise), so a 200 with
//! `product == "aiserver"` is proof we are on the same machine — no
//! heuristics, no config. See `docs/AISERVER-API-GUIDE.md` §1-5.
//!
//! What we do with it: point the `litellm` provider at the gateway
//! (`endpoints.openai`) and default the model to `litellm/auto`, which the
//! gateway routes to whichever model the user activated in the hub. Nothing
//! the user configured is touched — see [`apply_to_env`].

use serde::Deserialize;
use std::sync::OnceLock;

/// One entry of the discovery document's `endpoints` map.
#[derive(Debug, Clone, Deserialize)]
pub struct Endpoint {
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub up: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Endpoints {
    /// LiteLLM gateway, OpenAI-shaped (`:4000/v1`). The route we take.
    pub openai: Endpoint,
    /// Same gateway, Anthropic-shaped (`:4000`, no `/v1`).
    #[serde(default)]
    pub anthropic: Option<Endpoint>,
    /// The engine itself (`:8000/v1`) — lowest latency, no routing/`auto`.
    #[serde(default)]
    pub direct: Option<Endpoint>,
}

/// A Suite service announced in discovery (`slide_render`, `rag`, and each
/// studio). Shape agreed with dgx in xchg 20261025-1000.
#[derive(Debug, Clone, Deserialize)]
pub struct Service {
    pub base_url: String,
    #[serde(default)]
    pub up: bool,
    #[serde(default)]
    pub needs_key: bool,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub tls_selfsigned: bool,
    /// Only on `rag`: the engine's MCP endpoint (Streamable HTTP).
    #[serde(default)]
    pub mcp_url: Option<String>,
}

impl Service {
    /// Usable now: announced *and* its health check answered.
    pub fn live(&self) -> bool {
        self.up && !self.base_url.is_empty()
    }
}

/// The discovery document. Unknown fields are ignored on purpose: §7 of the
/// guide promises fields may be *added*, never removed or retyped.
#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    pub product: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub hardware: String,
    pub endpoints: Endpoints,
    /// Model id to send. Always `"auto"` today — the gateway resolves it to
    /// whatever the user activated, so thClaws never learns real model names.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_loaded: bool,
    #[serde(default)]
    pub hub: String,
    #[serde(default)]
    pub machine_id: String,
    /// The Suite's document service (DocConvert, OCR, previews, slides).
    #[serde(default)]
    pub slide_render: Option<Service>,
    /// Speech-to-text (asr-studio, https self-signed).
    #[serde(default)]
    pub asr: Option<Service>,
    /// RAG Engine (namespaces under the caller's key).
    #[serde(default)]
    pub rag: Option<Service>,
    /// video / voice / image / 3d — every block is always present; check `up`.
    #[serde(default)]
    pub studios: std::collections::BTreeMap<String, Service>,
}

impl Discovery {
    /// The model id to hand the `litellm` provider, e.g. `litellm/auto`.
    pub fn model_id(&self) -> String {
        let name = self.model.as_deref().unwrap_or("auto");
        format!("litellm/{name}")
    }

    /// True when the gateway is up *and* a model is loaded. `up` is a
    /// snapshot — the gateway restarts for ~5-15s whenever the user swaps
    /// models — so callers should treat `false` as "retry", never as
    /// "there is no AI Server here".
    pub fn ready(&self) -> bool {
        self.endpoints.openai.up && self.model_loaded
    }
}

static CACHED: OnceLock<Option<Discovery>> = OnceLock::new();

/// Hub base URL. Overridable so tests (and an operator debugging a
/// non-standard port) can retarget it; the real appliance always answers on
/// loopback :9000.
fn hub_base() -> String {
    std::env::var("THCLAWS_AISERVER_HUB")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:9000".to_string())
}

/// The box owner treats any outbound call as needing the user's say-so; left
/// alone, a model reaches for WebSearch to work around a failing local tool.
pub const WEB_ONLY_WHEN_ASKED: &str = "This machine is an on-premise AI Server. Use the internet \
     (WebSearch, WebFetch, browser) only when the user asks for information from the \
     internet or gives you a URL. Never go online on your own initiative, e.g. to work \
     around a local tool that failed or to look up how to do something: report what \
     failed and ask the user instead.";

/// Local AI: an AI Server box (discovery answered), or a workspace of the
/// thClaws EE Pack on one (`THCLAWS_LOCAL_AI=1`, set by the Pack's
/// provisioner — the pod can't see the Suite's discovery itself).
pub fn local_ai() -> bool {
    cached().is_some()
        || std::env::var("THCLAWS_LOCAL_AI")
            .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
            .unwrap_or(false)
}

/// Set by the process that found the box, for the agent processes it starts:
/// those are handed `LITELLM_BASE_URL` and so never probe themselves.
const BOX_ENV: &str = "THCLAWS_AISERVER_BOX";

/// On an AI Server box itself: this process found it, or the one that
/// started it did. A Suite set in Settings is not the box, whatever its
/// address (a tunnel to `127.0.0.1` included).
pub fn on_box() -> bool {
    cached().is_some() || std::env::var(BOX_ENV).is_ok_and(|v| v.trim() == "1")
}

/// `base` off the box; `base` plus [`WEB_ONLY_WHEN_ASKED`] on it.
pub fn web_tool_description(base: &'static str, cell: &'static OnceLock<String>) -> &'static str {
    if !local_ai() {
        return base;
    }
    cell.get_or_init(|| format!("{base}\n\n{WEB_ONLY_WHEN_ASKED}"))
}

/// Probe once and remember the answer for the rest of the process. Safe to
/// call from anywhere; only the first call does I/O.
///
/// Skipped entirely when `LITELLM_BASE_URL` is already set — the user (or
/// their shell / `endpoints.json` / `.env`) has pointed the provider
/// somewhere deliberate, and we neither override that nor spend a request
/// finding out we're not allowed to.
pub async fn probe_and_cache() -> Option<&'static Discovery> {
    if CACHED.get().is_none() {
        let found = if std::env::var("LITELLM_BASE_URL").is_ok() {
            None
        } else {
            fetch().await
        };
        let _ = CACHED.set(found);
    }
    cached()
}

/// The cached discovery result, or `None` when we never found an AI Server
/// (or never probed). Never does I/O — sync callers like
/// `preferred_default_model` read through this.
pub fn cached() -> Option<&'static Discovery> {
    CACHED.get().and_then(|o| o.as_ref())
}

async fn fetch() -> Option<Discovery> {
    let url = format!("{}/.well-known/aiserver", hub_base());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let d: Discovery = resp.json().await.ok()?;
    // Guard against something unrelated squatting on :9000.
    (d.product == "aiserver").then_some(d)
}

/// Point the `litellm` provider at the discovered gateway, without
/// disturbing anything already configured.
///
/// Only ever *fills in* unset vars, so the precedence documented in
/// [`crate::endpoints`] still holds: shell export > `endpoints.json` >
/// `.env` > this. Returns true when it set the base URL.
pub fn apply_to_env(d: &Discovery) -> bool {
    let mut set_base = false;
    // The Suite API on the box's one port. Exported so an agent's own
    // `--serve` child, which skips discovery once LITELLM_BASE_URL is set,
    // still knows the Suite is here.
    if std::env::var("THCLAWS_AISERVER_API").is_err() {
        std::env::set_var("THCLAWS_AISERVER_API", "http://127.0.0.1:8100");
    }
    std::env::set_var(BOX_ENV, "1");
    if std::env::var("THCLAWS_AISERVER_MODEL").is_err() {
        std::env::set_var("THCLAWS_AISERVER_MODEL", d.model_id());
    }
    if std::env::var("LITELLM_BASE_URL").is_err() {
        std::env::set_var("LITELLM_BASE_URL", &d.endpoints.openai.base_url);
        set_base = true;
    }
    // The guide is explicit that `sk-local` must not be hardcoded — it is a
    // per-machine value that may change — so it is only ever read from the
    // response.
    if let Some(key) = d.endpoints.openai.api_key.as_deref() {
        if !key.is_empty() && std::env::var("LITELLM_API_KEY").is_err() {
            std::env::set_var("LITELLM_API_KEY", key);
        }
    }
    // On the box, documents render on the box: without this the tools fall
    // back to the cloud gateway, i.e. the file leaves the machine.
    // Every block is always announced, installed or not — only a live one
    // is worth pointing at.
    if let Some(sr) = d.slide_render.as_ref().filter(|s| s.live()) {
        if std::env::var("SLIDE_RENDER_API").is_err() {
            std::env::set_var("SLIDE_RENDER_API", &sr.base_url);
            if let Some(key) = sr.api_key.as_deref().filter(|k| !k.is_empty()) {
                if std::env::var("SLIDE_RENDER_TOKEN").is_err() {
                    std::env::set_var("SLIDE_RENDER_TOKEN", key);
                }
            }
        }
    }
    set_base
}

/// The Suite's RAG Engine as an engine-managed MCP server (dev-plan/71):
/// `search_knowledge`, `read_document`, `list_sources` over the caller's
/// namespaces. Loopback needs no key; with `AISERVER_API_KEY` set it is sent
/// (§13.2), which also selects that key's own namespaces.
pub fn rag_mcp_config() -> Option<crate::mcp::McpServerConfig> {
    rag_mcp_config_for(cached()?, std::env::var("AISERVER_API_KEY").ok())
}

fn rag_mcp_config_for(d: &Discovery, key: Option<String>) -> Option<crate::mcp::McpServerConfig> {
    let rag = d.rag.as_ref().filter(|r| r.live())?;
    let url = rag.mcp_url.clone().filter(|u| !u.is_empty())?;
    let mut headers = std::collections::HashMap::new();
    if let Some(k) = key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty()) {
        headers.insert("X-API-Key".to_string(), k);
    }
    Some(crate::mcp::McpServerConfig {
        name: "aiserver-rag".into(),
        transport: "http".into(),
        command: String::new(),
        args: Vec::new(),
        env: std::collections::HashMap::new(),
        url,
        headers,
        trusted: false,
        engine_managed: true,
    })
}

/// Probe, wire up the environment, and tell the operator what happened.
/// Called once at startup, before any `AppConfig::load`.
pub async fn bootstrap() {
    let Some(d) = probe_and_cache().await else {
        apply_remote();
        return;
    };
    if apply_to_env(d) {
        eprintln!(
            "\x1b[32m[aiserver] {} v{} on {} — LLM via {}\x1b[0m",
            d.vendor, d.version, d.hardware, d.endpoints.openai.base_url
        );
    }
    if !d.ready() {
        // Not an error: the gateway also reports down for a few seconds
        // whenever the user switches models.
        eprintln!(
            "\x1b[33m[aiserver] no model is loaded yet — open {} to pick one\x1b[0m",
            d.hub
        );
    }
}

/// Where the AIServer Suite API v1 (`/v1/...` on :8100) is reached from
/// this engine, with the bearer to send. On the box: loopback, no key. In a
/// Pack workspace (`THCLAWS_LOCAL_AI=1`): the pod cannot reach the host, so
/// through the Pack gateway's `/aiserver` relay on the user's gateway key;
/// the gateway adds the box key and the user's identity. `None` = no Suite.
pub fn suite_api() -> Option<(String, Option<String>)> {
    if let Ok(u) = std::env::var("THCLAWS_AISERVER_API") {
        let u = u.trim().trim_end_matches('/').to_string();
        if !u.is_empty() {
            let key = std::env::var("THCLAWS_AISERVER_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty());
            return Some((u, key));
        }
    }
    if cached().is_some() {
        return Some(("http://127.0.0.1:8100".to_string(), None));
    }
    if local_ai() {
        let base = crate::providers::thclaws_gateway::resolve_base_url();
        let key = crate::providers::thclaws_gateway::resolve_access_key()?;
        return Some((
            format!("{}/aiserver", base.trim_end_matches('/')),
            Some(key),
        ));
    }
    None
}

/// The model a Suite on another machine starts on: its stable alias, so the
/// box owner can swap models without anyone re-configuring.
const REMOTE_MODEL: &str = "litellm/llm-default";

/// The `LITELLM_*` values [`apply_remote`] set, so it updates or removes only
/// its own and never a LiteLLM the user configured.
static REMOTE_DERIVED: std::sync::Mutex<Option<(String, Option<String>)>> =
    std::sync::Mutex::new(None);

/// A Suite address as a user types it in Settings (`192.168.0.114`,
/// `box.local`, `https://box:8101`, even `http://box:8100/v1`) → the API
/// base. No port means the Suite's: 8100, or 8101 for https.
pub fn normalize_suite_url(input: &str) -> Option<String> {
    let s = input.trim().trim_end_matches('/');
    if s.is_empty() {
        return None;
    }
    let full = if s.contains("://") {
        s.to_string()
    } else {
        format!("http://{s}")
    };
    let mut u = reqwest::Url::parse(&full).ok()?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        return None;
    }
    if u.port().is_none() {
        let port = if u.scheme() == "https" { 8101 } else { 8100 };
        u.set_port(Some(port)).ok()?;
    }
    u.set_path("");
    u.set_query(None);
    u.set_fragment(None);
    Some(u.as_str().trim_end_matches('/').to_string())
}

/// A Suite on another machine, set in Settings (`THCLAWS_AISERVER_API` +
/// `THCLAWS_AISERVER_API_KEY`): its models through the `litellm` provider
/// (the Suite speaks OpenAI on `/v1`), the other capabilities through
/// [`suite_api`]. Re-run whenever those settings change. On the box itself
/// and in a Pack workspace the Suite is already wired, so this does nothing.
pub fn apply_remote() {
    if cached().is_some() || local_ai() {
        return;
    }
    let mut derived = REMOTE_DERIVED.lock().unwrap_or_else(|e| e.into_inner());
    let (prev_base, prev_key) = match derived.clone() {
        Some((b, k)) => (Some(b), k),
        None => (None, None),
    };
    let free = |var: &str, prev: Option<&str>| match std::env::var(var) {
        Err(_) => true,
        Ok(v) => prev == Some(v.as_str()),
    };
    let set = |var: &str| std::env::var(var).ok().filter(|v| !v.trim().is_empty());
    match set("THCLAWS_AISERVER_API") {
        None => {
            if prev_base.is_some() && free("LITELLM_BASE_URL", prev_base.as_deref()) {
                std::env::remove_var("LITELLM_BASE_URL");
            }
            if prev_key.is_some() && free("LITELLM_API_KEY", prev_key.as_deref()) {
                std::env::remove_var("LITELLM_API_KEY");
            }
            if std::env::var("THCLAWS_AISERVER_MODEL").as_deref() == Ok(REMOTE_MODEL) {
                std::env::remove_var("THCLAWS_AISERVER_MODEL");
            }
            *derived = None;
        }
        Some(api) => {
            if !free("LITELLM_BASE_URL", prev_base.as_deref()) {
                return;
            }
            let base = format!("{}/v1", api.trim().trim_end_matches('/'));
            std::env::set_var("LITELLM_BASE_URL", &base);
            let key = set("THCLAWS_AISERVER_API_KEY");
            let key = if free("LITELLM_API_KEY", prev_key.as_deref()) {
                match &key {
                    Some(k) => std::env::set_var("LITELLM_API_KEY", k),
                    None => std::env::remove_var("LITELLM_API_KEY"),
                }
                key
            } else {
                None
            };
            if std::env::var("THCLAWS_AISERVER_MODEL").is_err() {
                std::env::set_var("THCLAWS_AISERVER_MODEL", REMOTE_MODEL);
            }
            *derived = Some((base, key));
        }
    }
    crate::providers::invalidate_picker_cache();
}

/// Settings' "Test" for the Suite: is it a Suite, which version, does it
/// take the key, and which of the nine capabilities are up.
pub async fn check_suite() -> serde_json::Value {
    let Some((base, key)) = suite_api() else {
        return serde_json::json!({ "ok": false, "error": "No AI Server Suite address is set." });
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return serde_json::json!({ "ok": false, "error": e.to_string() }),
    };
    let fail = |e: String| serde_json::json!({ "ok": false, "base": base, "error": e });
    let health: serde_json::Value = match client.get(format!("{base}/v1/health")).send().await {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        Ok(r) => return fail(format!("{base} answered HTTP {}.", r.status().as_u16())),
        Err(e) => return fail(format!("Cannot reach {base}: {e}")),
    };
    if health["product"].as_str() != Some("aiserver-suite") {
        return fail(format!("{base} is not an AI Server Suite."));
    }
    let mut req = client.get(format!("{base}/v1/capabilities"));
    if let Some(k) = &key {
        req = req.bearer_auth(k);
    }
    let caps: serde_json::Value = match req.send().await {
        Ok(r) if r.status().as_u16() == 401 => {
            return fail(if key.is_some() {
                "The Suite refused the key.".into()
            } else {
                "The Suite needs a key (ais_…).".into()
            })
        }
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        Ok(r) => return fail(format!("Capabilities: HTTP {}.", r.status().as_u16())),
        Err(e) => return fail(format!("Capabilities: {e}")),
    };
    let capabilities: Vec<serde_json::Value> = caps["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| serde_json::json!({ "id": c["id"], "available": c["available"] }))
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "ok": true,
        "base": base,
        "version": health["version"],
        "capabilities": capabilities,
    })
}

/// Whether this engine can reach a Suite at all (no I/O): what decides if
/// the aiserver-suite shell is offered.
pub fn suite_reachable() -> bool {
    std::env::var("THCLAWS_AISERVER_API").is_ok_and(|v| !v.trim().is_empty()) || local_ai()
}

/// A Suite path a shell may call: `/v1/...`, never box administration.
pub fn suite_path_allowed(path: &str) -> bool {
    path.starts_with("/v1/")
        && !path.starts_with("/v1/admin")
        && !path.contains("..")
        && !path.contains("//")
        && !path.contains('#')
        && path.chars().all(|c| !c.is_control() && c != ' ')
}

/// A workspace-relative file path a shell names: no absolute paths, no `..`.
pub fn workspace_file(root: &std::path::Path, rel: &str) -> Result<std::path::PathBuf, String> {
    use std::path::Component;
    let rel = rel.trim().trim_start_matches("./");
    let p = std::path::Path::new(rel);
    if rel.is_empty() || p.is_absolute() {
        return Err(format!("'{rel}' must be a path inside the workspace"));
    }
    if p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!("'{rel}' must not leave the workspace"));
    }
    Ok(root.join(p))
}

/// One Suite call for a GUI shell (`thclaws.aiserver.request`). JSON and
/// text replies come back inline; anything else (audio, images, video, GLB)
/// is written to `saveTo` in the workspace, since a shell has nowhere else to
/// keep tens of MB. Multipart `form` fields are `{name, value}`,
/// `{name, file}` (a workspace file) or `{name, data, filename}` (base64,
/// e.g. a file the user picked in the browser).
pub async fn suite_request(
    spec: &serde_json::Value,
    root: &std::path::Path,
) -> Result<serde_json::Value, String> {
    use base64::Engine as _;
    let (base, key) = suite_api().ok_or("no AIServer Suite on this machine")?;
    let path = spec.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let (path_only, query) = path.split_once('?').unwrap_or((path, ""));
    if !suite_path_allowed(path_only) {
        return Err(format!("path '{path_only}' is not allowed"));
    }
    let method = match spec
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("GET")
        .to_ascii_uppercase()
        .as_str()
    {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "DELETE" => reqwest::Method::DELETE,
        m => return Err(format!("method {m} is not allowed")),
    };
    let save_to = match spec
        .get("saveTo")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        Some(s) => Some((s.trim().to_string(), workspace_file(root, s)?)),
        None => None,
    };
    let timeout = spec
        .get("timeoutSecs")
        .and_then(|v| v.as_u64())
        .unwrap_or(600)
        .clamp(5, 1800);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let url = if query.is_empty() {
        format!("{base}{path_only}")
    } else {
        format!("{base}{path_only}?{query}")
    };
    let mut rb = client.request(method, &url);
    if let Some(k) = key {
        rb = rb.bearer_auth(k);
    }
    if let Some(form) = spec.get("form").and_then(|v| v.as_array()) {
        let mut mp = reqwest::multipart::Form::new();
        for f in form {
            let name = f
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or("form field without a name")?
                .to_string();
            if let Some(rel) = f.get("file").and_then(|v| v.as_str()) {
                let p = workspace_file(root, rel)?;
                let bytes = tokio::fs::read(&p)
                    .await
                    .map_err(|e| format!("read {rel}: {e}"))?;
                let fname = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".into());
                mp = mp.part(
                    name,
                    reqwest::multipart::Part::bytes(bytes).file_name(fname),
                );
            } else if let Some(b64) = f.get("data").and_then(|v| v.as_str()) {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64.split_once(',').map(|(_, d)| d).unwrap_or(b64))
                    .map_err(|e| format!("field {name}: bad base64: {e}"))?;
                let fname = f
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .unwrap_or("upload")
                    .to_string();
                mp = mp.part(
                    name,
                    reqwest::multipart::Part::bytes(bytes).file_name(fname),
                );
            } else {
                let v = match f.get("value") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(serde_json::Value::Null) | None => continue,
                    Some(other) => other.to_string(),
                };
                mp = mp.text(name, v);
            }
        }
        rb = rb.multipart(mp);
    } else if let Some(j) = spec.get("json") {
        rb = rb.json(j);
    }
    let resp = rb.send().await.map_err(|e| format!("AIServer: {e}"))?;
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mut headers = serde_json::Map::new();
    for h in ["x-audio-seconds", "x-usage-characters", "retry-after"] {
        if let Some(v) = resp.headers().get(h).and_then(|v| v.to_str().ok()) {
            headers.insert(h.into(), v.into());
        }
    }
    let textual = ctype.starts_with("application/json") || ctype.starts_with("text/");
    let mut out = serde_json::json!({ "status": status, "ok": (200..300).contains(&status), "contentType": ctype, "headers": headers });
    match save_to {
        Some((rel, p))
            if (200..300).contains(&status) && !ctype.starts_with("application/json") =>
        {
            if let Some(dir) = p.parent() {
                tokio::fs::create_dir_all(dir)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| format!("AIServer body: {e}"))?;
            tokio::fs::write(&p, &bytes)
                .await
                .map_err(|e| format!("write {rel}: {e}"))?;
            out["saved"] = serde_json::json!({ "path": rel, "bytes": bytes.len() });
        }
        _ if textual || !(200..300).contains(&status) => {
            let text = resp
                .text()
                .await
                .map_err(|e| format!("AIServer body: {e}"))?;
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(j) => out["json"] = j,
                Err(_) => out["text"] = text.chars().take(200_000).collect::<String>().into(),
            }
        }
        _ if spec.get("inline").and_then(|v| v.as_bool()) == Some(true) => {
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| format!("AIServer body: {e}"))?;
            if bytes.len() > INLINE_MAX_BYTES {
                return Err(format!(
                    "the reply is {} MB, over the {} MB inline limit",
                    bytes.len() / (1024 * 1024),
                    INLINE_MAX_BYTES / (1024 * 1024)
                ));
            }
            out["data"] = base64::engine::general_purpose::STANDARD
                .encode(&bytes)
                .into();
        }
        _ => {
            return Err(format!(
                "the reply is {ctype}: pass saveTo to keep it in the workspace"
            ))
        }
    }
    Ok(out)
}

/// Largest binary reply [`suite_request`] hands back inline (`inline: true`):
/// a spoken sentence is tens of KB, so this only stops a runaway reply.
const INLINE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Suite TTS voice for voice mode when the user has not picked one.
pub const VOICE_DEFAULT: &str = "sample-1";

/// One clip from the composer's mic → text, via the Suite's ASR.
pub async fn voice_transcribe(data_b64: &str, mime: &str) -> Result<String, String> {
    let ext = match mime.split(';').next().unwrap_or("").trim() {
        "audio/webm" => "webm",
        "audio/ogg" => "ogg",
        "audio/mp4" | "audio/aac" => "m4a",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        _ => "webm",
    };
    let spec = serde_json::json!({
        "method": "POST",
        "path": "/v1/audio/transcriptions",
        "timeoutSecs": 120,
        "form": [
            { "name": "file", "data": data_b64, "filename": format!("voice.{ext}") },
            { "name": "model", "value": "asr-default" },
            { "name": "language", "value": "th" },
            { "name": "response_format", "value": "json" },
        ],
    });
    let r = suite_request(&spec, std::path::Path::new(".")).await?;
    if r["ok"].as_bool() != Some(true) {
        return Err(suite_error(&r));
    }
    r["json"]["text"]
        .as_str()
        .map(|s| s.trim().to_string())
        .ok_or_else(|| "ASR answered without text".into())
}

/// One sentence → speech via the Suite's TTS; returns (base64 audio, mime).
pub async fn voice_speak(text: &str, voice: &str) -> Result<(String, String), String> {
    let voice = if voice.trim().is_empty() {
        VOICE_DEFAULT
    } else {
        voice.trim()
    };
    let spec = serde_json::json!({
        "method": "POST",
        "path": "/v1/audio/speech",
        // One TTS worker per box and requests queue; a voice's first load
        // alone measured 167 s on the Station.
        "timeoutSecs": 300,
        "inline": true,
        "json": { "model": "tts-default", "input": text, "voice": voice, "response_format": "mp3" },
    });
    let r = suite_request(&spec, std::path::Path::new(".")).await?;
    if r["ok"].as_bool() != Some(true) {
        return Err(suite_error(&r));
    }
    let data = r["data"]
        .as_str()
        .ok_or("TTS answered without audio")?
        .to_string();
    let mime = r["contentType"]
        .as_str()
        .filter(|c| c.starts_with("audio/"))
        .unwrap_or("audio/mpeg")
        .to_string();
    Ok((data, mime))
}

/// Speaks `text` as a stream: raw PCM (s16le, mono) handed to `on_audio` as
/// the box makes it, so the sound starts within a second instead of after
/// the whole answer. Returns the sample rate. A refusal that says when to
/// come back (`Retry-After` on 503/504: an LLM is loading, the worker was
/// restarted) is waited out and retried, a few times, before any audio.
pub async fn voice_speak_stream(
    text: &str,
    voice: &str,
    mut on_audio: impl FnMut(u32, &[u8]),
) -> Result<u32, String> {
    const TRIES: usize = 5;
    const FIRST_BYTE_SECS: u64 = 300;
    const NEXT_CHUNK_SECS: u64 = 60;
    let (base, key) = suite_api().ok_or("no AIServer Suite on this machine")?;
    let voice = if voice.trim().is_empty() {
        VOICE_DEFAULT
    } else {
        voice.trim()
    };
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let body = serde_json::json!({
        "model": "tts-default", "input": text, "voice": voice,
        "response_format": "pcm", "stream": true,
    });
    let mut attempt = 0;
    let mut resp = loop {
        attempt += 1;
        let mut rb = client.post(format!("{base}/v1/audio/speech")).json(&body);
        if let Some(k) = &key {
            rb = rb.bearer_auth(k);
        }
        let resp = tokio::time::timeout(std::time::Duration::from_secs(FIRST_BYTE_SECS), rb.send())
            .await
            .map_err(|_| "busy (504): the voice did not start in time".to_string())?
            .map_err(|e| format!("AIServer: {e}"))?;
        let status = resp.status().as_u16();
        if resp.status().is_success() {
            break resp;
        }
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let reason = resp
            .headers()
            .get("x-aiserver-reason")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = resp.text().await.unwrap_or_default();
        let json = serde_json::from_str::<serde_json::Value>(&text).unwrap_or_default();
        let err = suite_error(&serde_json::json!({ "status": status, "json": json, "text": text }));
        match retry_after {
            Some(secs) if matches!(status, 503 | 504) && attempt < TRIES => {
                eprintln!("[voice] speak: {status} {reason}, retrying in {secs}s ({err})");
                tokio::time::sleep(std::time::Duration::from_secs(secs.clamp(1, 60))).await;
            }
            _ => return Err(err),
        }
    };
    let rate = resp
        .headers()
        .get("x-sample-rate")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|r| (8_000..=96_000).contains(r))
        .unwrap_or(24_000);
    // A chunk may end in the middle of a 16-bit sample: carry the odd byte.
    let mut carry: Option<u8> = None;
    loop {
        let chunk = tokio::time::timeout(
            std::time::Duration::from_secs(NEXT_CHUNK_SECS),
            resp.chunk(),
        )
        .await
        .map_err(|_| "busy (504): the voice stopped mid-answer".to_string())?
        // An error after audio has started is how the box ends a stream early.
        .unwrap_or(None);
        let Some(chunk) = chunk else { break };
        let mut buf = Vec::with_capacity(chunk.len() + 1);
        buf.extend(carry.take());
        buf.extend_from_slice(&chunk);
        if buf.len() % 2 == 1 {
            carry = buf.pop();
        }
        if !buf.is_empty() {
            on_audio(rate, &buf);
        }
    }
    Ok(rate)
}

/// Streams in flight, so the page can hang up on one (`voice_speak_cancel`).
static VOICE_STREAMS: std::sync::Mutex<Vec<(String, tokio::task::AbortHandle)>> =
    std::sync::Mutex::new(Vec::new());

pub fn voice_stream_register(id: &str, handle: tokio::task::AbortHandle) {
    let mut v = VOICE_STREAMS.lock().expect("voice streams lock");
    v.retain(|(_, h)| !h.is_finished());
    v.push((id.to_string(), handle));
}

pub fn voice_stream_cancel(id: &str) {
    let mut v = VOICE_STREAMS.lock().expect("voice streams lock");
    v.retain(|(i, h)| {
        if i == id {
            h.abort();
        }
        i != id && !h.is_finished()
    });
}

/// Voices the Suite offers, for the voice-mode picker. `mx-*` are lab
/// voices that must never be shown to a customer (dgx).
pub async fn voice_list() -> Result<Vec<serde_json::Value>, String> {
    let spec =
        serde_json::json!({ "method": "GET", "path": "/v1/audio/voices", "timeoutSecs": 15 });
    let r = suite_request(&spec, std::path::Path::new(".")).await?;
    if r["ok"].as_bool() != Some(true) {
        return Err(suite_error(&r));
    }
    Ok(customer_voices(&r["json"]))
}

fn customer_voices(j: &serde_json::Value) -> Vec<serde_json::Value> {
    j["voices"]
        .as_array()
        .map(|vs| {
            vs.iter()
                .filter(|v| v["id"].as_str().is_some_and(|id| !id.starts_with("mx-")))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// (asr up, tts up) from `GET /v1/capabilities`.
pub async fn voice_capabilities() -> Result<(bool, bool), String> {
    let spec =
        serde_json::json!({ "method": "GET", "path": "/v1/capabilities", "timeoutSecs": 15 });
    let r = suite_request(&spec, std::path::Path::new(".")).await?;
    if r["ok"].as_bool() != Some(true) {
        return Err(suite_error(&r));
    }
    Ok((cap_up(&r["json"], "asr"), cap_up(&r["json"], "tts")))
}

fn cap_up(j: &serde_json::Value, id: &str) -> bool {
    j["data"].as_array().is_some_and(|caps| {
        caps.iter().any(|c| {
            c["id"].as_str() == Some(id)
                && c["available"].as_bool() != Some(false)
                && c["up"].as_bool() != Some(false)
        })
    })
}

fn suite_error(r: &serde_json::Value) -> String {
    let status = r["status"].as_u64().unwrap_or(0);
    let e = &r["json"]["error"];
    let code = e["code"].as_str().or(e["type"].as_str()).unwrap_or("");
    let msg = e["message"]
        .as_str()
        .or(r["json"]["detail"].as_str())
        .or(r["text"].as_str())
        .unwrap_or("");
    // 507: too little unified memory to start, don't retry. 409/429/503/504:
    // the GPU or worker is taken for now.
    let head = match status {
        507 => "memory short",
        409 | 429 | 503 | 504 => "busy",
        _ => "error",
    };
    let detail = [code, msg]
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(": ");
    let detail: String = detail.chars().take(300).collect();
    if detail.is_empty() {
        format!("{head} ({status})")
    } else {
        format!("{head} ({status}): {detail}")
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn voice_errors_name_busy_and_keep_the_suite_message() {
        let busy = serde_json::json!({ "status": 503, "json": { "error": { "code": "memory_short", "message": "free memory too low" } } });
        assert_eq!(
            suite_error(&busy),
            "busy (503): memory_short: free memory too low"
        );
        let plain = serde_json::json!({ "status": 400, "json": { "detail": "bad file" } });
        assert_eq!(suite_error(&plain), "error (400): bad file");
        assert_eq!(
            suite_error(&serde_json::json!({ "status": 500 })),
            "error (500)"
        );
        assert!(suite_error(&serde_json::json!({ "status": 507 })).starts_with("memory short"));
    }

    #[test]
    fn voice_mode_hides_lab_voices_and_reads_capabilities() {
        let v = serde_json::json!({ "voices": [{ "id": "sample-1" }, { "id": "mx-charon" }, { "id": "acme-ceo" }] });
        let ids: Vec<_> = customer_voices(&v)
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["sample-1", "acme-ceo"]);
        let caps = serde_json::json!({ "data": [{ "id": "asr", "up": true }, { "id": "tts", "available": false }] });
        assert!(cap_up(&caps, "asr"));
        assert!(!cap_up(&caps, "tts"));
        assert!(!cap_up(&caps, "music"));
    }

    #[test]
    fn web_tools_unchanged_off_the_box() {
        let _g = crate::kms::test_env_lock();
        static CELL: OnceLock<String> = OnceLock::new();
        assert!(cached().is_none());
        assert_eq!(web_tool_description("base", &CELL), "base");
    }

    #[test]
    fn an_ee_pack_workspace_counts_as_local_ai() {
        let _g = crate::kms::test_env_lock();
        std::env::remove_var("THCLAWS_LOCAL_AI");
        assert!(!local_ai());
        std::env::set_var("THCLAWS_LOCAL_AI", "1");
        assert!(local_ai());
        static CELL: OnceLock<String> = OnceLock::new();
        assert!(web_tool_description("base", &CELL).ends_with(WEB_ONLY_WHEN_ASKED));
        std::env::remove_var("THCLAWS_LOCAL_AI");
    }

    #[test]
    fn suite_addresses_take_the_suite_ports() {
        let n = |s| normalize_suite_url(s);
        assert_eq!(
            n("192.168.0.114").as_deref(),
            Some("http://192.168.0.114:8100")
        );
        assert_eq!(n(" box.local/ ").as_deref(), Some("http://box.local:8100"));
        assert_eq!(n("https://box").as_deref(), Some("https://box:8101"));
        assert_eq!(n("http://box:8100/v1").as_deref(), Some("http://box:8100"));
        assert_eq!(n("box:9100").as_deref(), Some("http://box:9100"));
        assert_eq!(n(""), None);
        assert_eq!(n("ftp://box"), None);
    }

    /// A Suite set in Settings drives `litellm` only where nobody else did,
    /// and clearing it takes back exactly what it set.
    #[test]
    fn a_remote_suite_fills_litellm_and_clears_only_its_own() {
        let _g = crate::kms::test_env_lock();
        let vars = [
            "THCLAWS_AISERVER_API",
            "THCLAWS_AISERVER_API_KEY",
            "THCLAWS_AISERVER_MODEL",
            "LITELLM_BASE_URL",
            "LITELLM_API_KEY",
            "THCLAWS_LOCAL_AI",
        ];
        let saved: Vec<_> = vars.iter().map(|v| (v, std::env::var(v).ok())).collect();
        for v in vars {
            std::env::remove_var(v);
        }

        std::env::set_var("THCLAWS_AISERVER_API", "http://box:8100");
        std::env::set_var("THCLAWS_AISERVER_API_KEY", "ais_test");
        apply_remote();
        assert_eq!(
            std::env::var("LITELLM_BASE_URL").unwrap(),
            "http://box:8100/v1"
        );
        assert_eq!(std::env::var("LITELLM_API_KEY").unwrap(), "ais_test");
        assert_eq!(
            std::env::var("THCLAWS_AISERVER_MODEL").unwrap(),
            REMOTE_MODEL
        );
        assert!(suite_reachable());

        std::env::set_var("THCLAWS_AISERVER_API", "http://other:8100");
        apply_remote();
        assert_eq!(
            std::env::var("LITELLM_BASE_URL").unwrap(),
            "http://other:8100/v1"
        );

        std::env::remove_var("THCLAWS_AISERVER_API");
        std::env::remove_var("THCLAWS_AISERVER_API_KEY");
        apply_remote();
        assert!(std::env::var("LITELLM_BASE_URL").is_err());
        assert!(std::env::var("LITELLM_API_KEY").is_err());
        assert!(std::env::var("THCLAWS_AISERVER_MODEL").is_err());

        std::env::set_var("LITELLM_BASE_URL", "http://my-litellm:4000/v1");
        std::env::set_var("THCLAWS_AISERVER_API", "http://box:8100");
        apply_remote();
        assert_eq!(
            std::env::var("LITELLM_BASE_URL").unwrap(),
            "http://my-litellm:4000/v1"
        );

        for v in vars {
            std::env::remove_var(v);
        }
        *REMOTE_DERIVED.lock().unwrap() = None;
        for (v, val) in saved {
            if let Some(val) = val {
                std::env::set_var(v, val);
            }
        }
    }

    // `THCLAWS_AISERVER_HUB` is process-global, so the two probe tests must
    // not overlap or one clears the other's target mid-request.
    static HUB_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn doc(product: &str) -> serde_json::Value {
        serde_json::json!({
            "product": product,
            "vendor": "AIServer.in.th",
            "version": "0.2.76",
            "hardware": "dgx-spark-gb10",
            "endpoints": {
                "openai":    {"base_url": "http://127.0.0.1:4000/v1", "api_key": "sk-local", "up": true},
                "anthropic": {"base_url": "http://127.0.0.1:4000", "api_key": "sk-local", "up": true},
                "direct":    {"base_url": "http://127.0.0.1:8000/v1", "up": true,
                              "note": "ต่อตรง engine"}
            },
            "model": "auto",
            "model_loaded": true,
            "hub": "http://127.0.0.1:9000",
            "machine_id": "2b50a0679bab64bc"
        })
    }

    #[test]
    fn parses_the_documented_response_and_ignores_unknown_fields() {
        let mut v = doc("aiserver");
        // §7: new fields may appear at any time; they must not break parsing.
        v["license"] = serde_json::json!({"tier": "pro"});
        v["models_available"] = serde_json::json!(["a", "b"]);
        let d: Discovery = serde_json::from_value(v).expect("parses");
        assert_eq!(d.endpoints.openai.base_url, "http://127.0.0.1:4000/v1");
        assert_eq!(d.endpoints.openai.api_key.as_deref(), Some("sk-local"));
        assert_eq!(d.model_id(), "litellm/auto");
        assert!(d.ready());
        assert_eq!(d.machine_id, "2b50a0679bab64bc");
    }

    #[test]
    fn suite_service_blocks_parse_as_frozen_by_dgx() {
        let mut v = doc("aiserver");
        v["studios"] = serde_json::json!({
            "video": {"base_url": "http://127.0.0.1:8189", "up": true, "needs_key": false, "tls_selfsigned": false},
            "voice": {"base_url": "https://127.0.0.1:8191", "up": true, "needs_key": false, "tls_selfsigned": true},
            "3d":    {"base_url": "http://127.0.0.1:8194", "up": false, "needs_key": false, "tls_selfsigned": false}
        });
        v["asr"] = serde_json::json!({"base_url": "https://127.0.0.1:8193", "up": true, "needs_key": false, "tls_selfsigned": true});
        v["rag"] = serde_json::json!({"base_url": "http://127.0.0.1:8195", "up": true, "needs_key": false,
                                      "tls_selfsigned": false, "mcp_url": "http://127.0.0.1:8195/mcp"});
        v["slide_render"] = serde_json::json!({"base_url": "http://127.0.0.1:8475", "up": false, "needs_key": false, "tls_selfsigned": false});
        let d: Discovery = serde_json::from_value(v).expect("parses");
        assert!(d.asr.as_ref().is_some_and(|a| a.live() && a.tls_selfsigned));
        assert_eq!(
            d.rag.as_ref().and_then(|r| r.mcp_url.as_deref()),
            Some("http://127.0.0.1:8195/mcp")
        );
        assert!(d.studios["voice"].live() && !d.studios["3d"].live());
        assert!(
            !d.slide_render.as_ref().unwrap().live(),
            "announced but not installed"
        );
    }

    #[test]
    fn rag_mcp_is_wired_only_when_live_and_sends_the_key_when_set() {
        let mut v = doc("aiserver");
        v["rag"] = serde_json::json!({"base_url": "http://127.0.0.1:8195", "up": true,
                                      "mcp_url": "http://127.0.0.1:8195/mcp"});
        let d: Discovery = serde_json::from_value(v.clone()).expect("parses");
        let open = rag_mcp_config_for(&d, None).expect("live rag → server");
        assert_eq!(open.transport, "http");
        assert!(open.engine_managed && open.headers.is_empty());
        let keyed = rag_mcp_config_for(&d, Some("ais_x".into())).unwrap();
        assert_eq!(
            keyed.headers.get("X-API-Key").map(String::as_str),
            Some("ais_x")
        );
        v["rag"]["up"] = serde_json::json!(false);
        let down: Discovery = serde_json::from_value(v).unwrap();
        assert!(rag_mcp_config_for(&down, None).is_none());
        let none: Discovery = serde_json::from_value(doc("aiserver")).unwrap();
        assert!(rag_mcp_config_for(&none, None).is_none());
    }

    #[test]
    fn slide_render_block_parses_and_is_optional() {
        let d: Discovery = serde_json::from_value(doc("aiserver")).expect("parses");
        assert!(d.slide_render.is_none());
        let mut v = doc("aiserver");
        v["slide_render"] = serde_json::json!({"base_url": "http://127.0.0.1:8475", "up": true, "needs_key": false});
        let d: Discovery = serde_json::from_value(v).expect("parses");
        let sr = d.slide_render.expect("slide_render present");
        assert_eq!(sr.base_url, "http://127.0.0.1:8475");
        assert!(sr.up && !sr.needs_key && sr.api_key.is_none());
    }

    /// `direct` carries no `api_key`; the struct must not require one.
    #[test]
    fn direct_endpoint_without_a_key_still_parses() {
        let d: Discovery = serde_json::from_value(doc("aiserver")).expect("parses");
        let direct = d.endpoints.direct.expect("direct present");
        assert!(direct.api_key.is_none());
    }

    #[test]
    fn a_loaded_model_is_required_for_ready() {
        let mut v = doc("aiserver");
        v["model_loaded"] = serde_json::json!(false);
        let d: Discovery = serde_json::from_value(v).expect("parses");
        assert!(!d.ready(), "no model loaded yet");
    }

    #[tokio::test]
    async fn fetch_rejects_another_service_squatting_on_the_port() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/aiserver"))
            .respond_with(ResponseTemplate::new(200).set_body_json(doc("something-else")))
            .mount(&server)
            .await;

        let _guard = HUB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("THCLAWS_AISERVER_HUB", server.uri());
        let got = fetch().await;
        std::env::remove_var("THCLAWS_AISERVER_HUB");
        assert!(got.is_none(), "product != aiserver must not count");
    }

    #[tokio::test]
    async fn fetch_reads_a_real_response() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/aiserver"))
            .respond_with(ResponseTemplate::new(200).set_body_json(doc("aiserver")))
            .mount(&server)
            .await;

        let _guard = HUB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("THCLAWS_AISERVER_HUB", server.uri());
        let got = fetch().await;
        std::env::remove_var("THCLAWS_AISERVER_HUB");
        assert_eq!(got.expect("found").model_id(), "litellm/auto");
    }

    #[test]
    fn suite_paths_exclude_admin_and_escapes() {
        for p in [
            "/v1/ocr",
            "/v1/jobs/vid_1/artifacts/0",
            "/v1/rag/collections/default:default/documents",
        ] {
            assert!(suite_path_allowed(p), "{p}");
        }
        for p in [
            "/v1/admin/models/x:load",
            "/api/discovery",
            "/v1/../api",
            "/v1//x",
            "v1/ocr",
            "/v1/a b",
        ] {
            assert!(!suite_path_allowed(p), "{p}");
        }
    }

    #[test]
    fn workspace_files_stay_inside() {
        let root = std::path::Path::new("/w");
        assert_eq!(
            workspace_file(root, "output/a.png").unwrap(),
            root.join("output").join("a.png")
        );
        assert_eq!(workspace_file(root, "./a.png").unwrap(), root.join("a.png"));
        assert!(workspace_file(root, "/etc/passwd").is_err());
        assert!(workspace_file(root, "../x").is_err());
        assert!(workspace_file(root, "a/../../x").is_err());
        assert!(workspace_file(root, "").is_err());
    }
}
