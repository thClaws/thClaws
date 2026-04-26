//! Model catalogue: context window lookups for each model thClaws
//! might talk to. Used by auto-compaction, `/compact`, and the
//! fork-on-big-session flow to pick thresholds based on the *actual*
//! model's context window instead of a blanket compile-time constant.
//!
//! Three layers, checked in order:
//! 1. User cache at `~/.config/thclaws/model_catalogue.json`, written
//!    when the user runs `/models refresh` or when the daily auto-
//!    refresh background task succeeds.
//! 2. Embedded baseline compiled into the binary — also guarantees
//!    we have something usable at first launch with no network.
//! 3. Per-provider fallback for ids neither layer knows about.
//!
//! Remote refresh URL is `thclaws.ai/api/model_catalogue.json`, which
//! will eventually host a server-side aggregation of OpenRouter +
//! Gemini + hand-curated data. Until that endpoint exists the refresh
//! fails silently and we keep using the embedded baseline (plus any
//! prior cache the user had).
//!
//! Cache is schema-versioned so a future incompatible change can
//! reject old caches cleanly without crashing.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Schema history:
/// - v1: flat `models: Vec<{id, context, provider}>`.
/// - v2: v1 + per-row `source` / `verified_at` / `max_output`.
/// - v3: provider-keyed maps (`providers.<name>.models.<real_id>`) + top-level
///   `aliases` for user-friendly → canonical id resolution. Ids are now the
///   exact strings each provider's `/v1/models` endpoint returns (dated
///   variants like `claude-sonnet-4-5-20250929`, not aliased families).
///
/// The loader hard-rejects mismatched schemas, so an outdated cache is
/// ignored cleanly rather than silently serving stale rows.
pub const CURRENT_SCHEMA: u32 = 3;

/// Remote URL the client fetches from when the user runs
/// `/models refresh` or the daily auto-refresh fires. Expected to
/// serve the same JSON shape as the embedded baseline (same schema).
pub const REMOTE_URL: &str = "https://thclaws.ai/api/model_catalogue.json";

/// Hard-coded last-resort context size when nothing else matches. Set
/// to match OpenAI's oldest mainline model (gpt-4o) — conservative
/// enough to be safe on smaller Ollama checkpoints too.
pub const GLOBAL_FALLBACK: u32 = 128_000;

/// How often the auto-refresh background task is allowed to hit the
/// network — once per day, with `fetched_at` in the cache as the
/// marker.
pub const AUTO_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Embedded baseline catalogue. Shipped with every build so first
/// launch (no cache, no internet) still has real context-window data.
/// Regenerated via the `/models refresh` flow when the user wants
/// fresher data and has connectivity.
pub const BASELINE_JSON: &str = include_str!("../resources/model_catalogue.json");

/// One model row, keyed by its real id in the owning `ProviderCatalogue`
/// map. All fields are optional so the catalogue can list a known id
/// whose context hasn't been verified yet (`None` falls through to the
/// provider's `default_context` at lookup time — same semantics as a
/// missing row, but the id stays visible for `/models` listings).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
    /// Max output tokens per turn, when the vendor publishes a separate
    /// limit from the total context window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u32>,
    /// Where this row was sourced from — a vendor doc URL for hand-verified
    /// rows, a provider list URL for auto-discovered rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// ISO-8601 date this row was last verified against its `source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<String>,
}

/// All models known for one provider, plus the provider-level metadata
/// (list URL, default context fallback).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderCatalogue {
    /// The `/v1/models`-style endpoint this provider's ids come from.
    /// Informational; not hit at runtime by the loader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_url: Option<String>,
    /// Fallback context window used when a model id is routed to this
    /// provider but isn't in the `models` map (e.g. a freshly-released
    /// checkpoint the catalogue hasn't indexed yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_context: Option<u32>,
    /// Real model ids (exactly as the provider's API returns them)
    /// mapped to their entry.
    #[serde(default)]
    pub models: HashMap<String, ModelEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalogue {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub fetched_at: String,
    /// Provider name → its catalogue. Provider names match the strings
    /// `provider_kind_name()` returns, so `ProviderKind::detect(id)`
    /// gives the map key directly.
    #[serde(default)]
    pub providers: HashMap<String, ProviderCatalogue>,
    /// User-friendly id → real id. Lets callers pass `claude-sonnet-4-6`
    /// and have it resolved to the current canonical dated variant
    /// (`claude-sonnet-4-6-20261001`). Entries are optional — bare real
    /// ids still look up directly.
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    /// Last-resort fallback when neither an entry nor a provider default
    /// is known.
    #[serde(default)]
    pub fallback: Option<u32>,
}

impl Catalogue {
    pub fn from_json_str(s: &str) -> Option<Self> {
        let parsed: Self = serde_json::from_str(s).ok()?;
        if parsed.schema != CURRENT_SCHEMA {
            return None;
        }
        Some(parsed)
    }

    /// Resolve `model` through the alias table to its canonical id. If
    /// no alias matches, the input is returned unchanged (it may
    /// already be canonical).
    pub fn resolve_alias<'a>(&'a self, model: &'a str) -> &'a str {
        self.aliases.get(model).map(String::as_str).unwrap_or(model)
    }

    /// Look up a model's context window. Resolves aliases, detects the
    /// owning provider from the id, and searches that provider's map.
    /// Falls back to stripping `vendor/` prefixes (so `agent/claude-...`
    /// still finds `claude-...` when routed through the same provider).
    /// Returns `None` when neither the exact id nor any prefix-stripped
    /// form is catalogued — callers apply provider-default / global
    /// fallback themselves.
    pub fn lookup_context(&self, model: &str) -> Option<u32> {
        let canonical = self.resolve_alias(model);
        if let Some(n) = self.lookup_in_any_provider(canonical) {
            return Some(n);
        }
        // Strip leading `vendor/` segments (e.g. `agent/claude-...`,
        // `openrouter/anthropic/claude-...`) and retry.
        let mut remaining = canonical;
        while let Some(idx) = remaining.find('/') {
            remaining = &remaining[idx + 1..];
            if let Some(n) = self.lookup_in_any_provider(remaining) {
                return Some(n);
            }
        }
        None
    }

    fn lookup_in_any_provider(&self, id: &str) -> Option<u32> {
        // Prefer the provider the id's routing prefix indicates; fall
        // back to scanning every provider's map for the id as a
        // safety net (cheap — total catalogue is a few hundred rows).
        // A matched entry with `context: None` is treated the same as
        // a missing entry — the caller falls through to the provider
        // default.
        let kind_name = crate::providers::ProviderKind::detect(id).map(provider_kind_name);
        if let Some(name) = kind_name {
            if let Some(pc) = self.providers.get(name) {
                if let Some(e) = pc.models.get(id) {
                    if let Some(n) = e.context {
                        return Some(n);
                    }
                }
            }
        }
        for pc in self.providers.values() {
            if let Some(e) = pc.models.get(id) {
                if let Some(n) = e.context {
                    return Some(n);
                }
            }
        }
        None
    }

    pub fn provider_default(&self, provider: &str) -> Option<u32> {
        self.providers
            .get(provider)
            .and_then(|pc| pc.default_context)
    }
}

/// Runtime layered view: user cache overrides baseline; baseline
/// always loaded so fallbacks work even when the cache is empty.
pub struct EffectiveCatalogue {
    pub cache: Option<Catalogue>,
    pub baseline: Catalogue,
}

impl EffectiveCatalogue {
    pub fn load() -> Self {
        let baseline = Catalogue::from_json_str(BASELINE_JSON)
            .expect("embedded baseline catalogue must parse");
        let cache = cache_path()
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .and_then(|s| Catalogue::from_json_str(&s));
        Self { cache, baseline }
    }

    /// Two-tier exact lookup. Returns `None` if neither layer has the
    /// model — caller decides whether to fall back or warn.
    pub fn lookup_exact(&self, model: &str) -> Option<u32> {
        if let Some(c) = &self.cache {
            if let Some(n) = c.lookup_context(model) {
                return Some(n);
            }
        }
        self.baseline.lookup_context(model)
    }

    pub fn provider_default(&self, provider: &str) -> Option<u32> {
        self.cache
            .as_ref()
            .and_then(|c| c.provider_default(provider))
            .or_else(|| self.baseline.provider_default(provider))
    }

    /// Merged model listing for one provider — baseline rows plus user-cache
    /// rows, with cache winning on metadata when the same id appears in both.
    /// Returns `(id, entry)` pairs sorted by id. Consumed by the `/models`
    /// slash command to render a catalogue-based list instead of hitting the
    /// provider's live `/v1/models` endpoint.
    pub fn list_models_for_provider(&self, provider: &str) -> Vec<(String, ModelEntry)> {
        let mut out: HashMap<String, ModelEntry> = HashMap::new();
        if let Some(pc) = self.baseline.providers.get(provider) {
            for (id, e) in &pc.models {
                out.insert(id.clone(), e.clone());
            }
        }
        if let Some(c) = &self.cache {
            if let Some(pc) = c.providers.get(provider) {
                for (id, e) in &pc.models {
                    out.insert(id.clone(), e.clone()); // cache wins
                }
            }
        }
        let mut rows: Vec<(String, ModelEntry)> = out.into_iter().collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    pub fn fallback(&self) -> u32 {
        self.cache
            .as_ref()
            .and_then(|c| c.fallback)
            .or(self.baseline.fallback)
            .unwrap_or(GLOBAL_FALLBACK)
    }
}

/// Resolve the effective context window for `model`. Falls back in
/// order: user cache → baseline → provider default → global fallback.
/// `known_in_catalogue` returns `false` when the value is a
/// provider-default or global-fallback — callers can use that to
/// nudge the user toward `/models refresh`.
pub fn effective_context_window(model: &str) -> u32 {
    effective_context_window_with(&EffectiveCatalogue::load(), model).0
}

pub fn effective_context_window_with(cat: &EffectiveCatalogue, model: &str) -> (u32, bool) {
    if let Some(n) = cat.lookup_exact(model) {
        return (n, true);
    }
    let provider_name = crate::providers::ProviderKind::detect(model)
        .map(|k| provider_kind_name(k))
        .unwrap_or("");
    if !provider_name.is_empty() {
        if let Some(n) = cat.provider_default(provider_name) {
            return (n, false);
        }
    }
    (cat.fallback(), false)
}

/// Stable short identifier matching the `provider` field in the
/// catalogue JSON. Mirrors `ProviderKind::name` except for
/// `Ollama`/`OllamaAnthropic`/`AgentSdk` which we namespace
/// differently in the catalogue for clarity.
pub fn provider_kind_name(k: crate::providers::ProviderKind) -> &'static str {
    use crate::providers::ProviderKind;
    match k {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::AgentSdk => "agent-sdk",
        ProviderKind::OpenAI => "openai",
        ProviderKind::OpenAIResponses => "openai-responses",
        ProviderKind::OpenRouter => "openrouter",
        ProviderKind::Gemini => "gemini",
        ProviderKind::Ollama => "ollama",
        ProviderKind::OllamaAnthropic => "ollama-anthropic",
        ProviderKind::OllamaCloud => "ollama-cloud",
        ProviderKind::DashScope => "dashscope",
        ProviderKind::AgenticPress => "agentic-press",
        ProviderKind::ZAi => "zai",
        ProviderKind::LMStudio => "lmstudio",
        ProviderKind::AzureAIFoundry => "azure",
    }
}

/// Path to the writable user cache. `None` only when the user has no
/// home directory (extremely rare / headless / broken Windows env).
pub fn cache_path() -> Option<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        crate::util::home_dir()?.join(".config")
    };
    Some(base.join("thclaws").join("model_catalogue.json"))
}

/// Age of the user cache based on its file mtime. `None` when the
/// cache doesn't exist (caller should treat as "refresh required").
/// The embedded baseline's age isn't tracked — it's effectively
/// whatever the binary ships with.
pub fn cache_age() -> Option<std::time::Duration> {
    let path = cache_path()?;
    let meta = std::fs::metadata(&path).ok()?;
    let modified = meta.modified().ok()?;
    modified.elapsed().ok()
}

/// Fetch the remote catalogue and, if it parses, write it to the
/// cache path atomically. Returns the new number of models on
/// success.
///
/// Silent-by-design: every error path returns `Err` that callers can
/// log quietly. Used by the `/models refresh` slash command and by
/// the daily auto-refresh background task.
pub async fn refresh_from_remote() -> Result<RefreshOutcome, RefreshError> {
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| RefreshError::Http(e.to_string()))?
        .get(REMOTE_URL)
        .send()
        .await
        .map_err(|e| RefreshError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(RefreshError::Http(format!("status {}", resp.status())));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| RefreshError::Http(e.to_string()))?;
    let parsed = Catalogue::from_json_str(&body).ok_or(RefreshError::Parse)?;
    let model_count: usize = parsed.providers.values().map(|p| p.models.len()).sum();
    write_cache(&body)?;
    Ok(RefreshOutcome {
        model_count,
        source: parsed.source,
    })
}

/// ISO-8601 date (`YYYY-MM-DD`) for today's UTC date, suitable for stamping
/// `verified_at` on catalogue rows. No chrono dep — one small date routine.
pub fn today_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64 + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y } as i32;
    format!("{y:04}-{m:02}-{d:02}")
}

/// Upsert a single model row into the user cache at `cache_path()`.
/// If no cache exists yet, seeds one from the embedded baseline so the
/// cache stays a valid schema-v3 document. Atomic write. Used by the
/// `/model <ollama-id>` flow to record context windows discovered via
/// `POST /api/show` so they persist across sessions.
pub fn upsert_cache_entry(
    provider: &str,
    model_id: &str,
    entry: ModelEntry,
) -> Result<(), RefreshError> {
    let path = cache_path().ok_or(RefreshError::NoHome)?;
    // Start from the existing cache, or fall back to the baseline so the
    // cache document is valid from first write.
    let mut cat: Catalogue = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| Catalogue::from_json_str(&s))
        .or_else(|| Catalogue::from_json_str(BASELINE_JSON))
        .ok_or(RefreshError::Parse)?;
    cat.providers
        .entry(provider.to_string())
        .or_default()
        .models
        .insert(model_id.to_string(), entry);
    let body = serde_json::to_string_pretty(&cat).map_err(|e| RefreshError::Io(e.to_string()))?;
    write_cache(&body)
}

fn write_cache(body: &str) -> Result<(), RefreshError> {
    let path = cache_path().ok_or(RefreshError::NoHome)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RefreshError::Io(e.to_string()))?;
    }
    // Atomic write: temp file + rename so a crashed mid-write doesn't
    // leave a corrupted catalogue on disk.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| RefreshError::Io(e.to_string()))?;
    std::fs::rename(&tmp, &path).map_err(|e| RefreshError::Io(e.to_string()))?;
    Ok(())
}

pub struct RefreshOutcome {
    pub model_count: usize,
    pub source: String,
}

#[derive(Debug)]
pub enum RefreshError {
    Http(String),
    Parse,
    Io(String),
    NoHome,
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshError::Http(s) => write!(f, "http: {s}"),
            RefreshError::Parse => write!(f, "parse: remote returned invalid or wrong-schema JSON"),
            RefreshError::Io(s) => write!(f, "io: {s}"),
            RefreshError::NoHome => write!(f, "no home directory"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_parses() {
        let c = Catalogue::from_json_str(BASELINE_JSON).expect("baseline catalogue must parse");
        assert_eq!(c.schema, CURRENT_SCHEMA);
        assert!(c.providers.contains_key("anthropic"));
        let anth = c.providers.get("anthropic").unwrap();
        assert!(!anth.models.is_empty());
        assert_eq!(anth.default_context, Some(200_000));
    }

    #[test]
    fn lookup_finds_exact_model() {
        let c = EffectiveCatalogue {
            cache: None,
            baseline: Catalogue::from_json_str(BASELINE_JSON).unwrap(),
        };
        let (n, known) = effective_context_window_with(&c, "claude-sonnet-4-6");
        assert_eq!(n, 200_000);
        assert!(known);
    }

    #[test]
    fn lookup_strips_vendor_prefix() {
        let c = EffectiveCatalogue {
            cache: None,
            baseline: Catalogue::from_json_str(BASELINE_JSON).unwrap(),
        };
        let (n, known) =
            effective_context_window_with(&c, "openrouter/anthropic/claude-sonnet-4-6");
        assert_eq!(n, 200_000);
        assert!(known);
    }

    #[test]
    fn lookup_falls_back_to_provider_default_and_flags_unknown() {
        let c = EffectiveCatalogue {
            cache: None,
            baseline: Catalogue::from_json_str(BASELINE_JSON).unwrap(),
        };
        let (n, known) = effective_context_window_with(&c, "claude-future-x99");
        assert_eq!(n, 200_000);
        assert!(!known);
    }

    #[test]
    fn lookup_falls_back_to_global_for_unknown_provider() {
        let c = EffectiveCatalogue {
            cache: None,
            baseline: Catalogue::from_json_str(BASELINE_JSON).unwrap(),
        };
        let (n, known) = effective_context_window_with(&c, "unknown-vendor/unknown-model");
        assert_eq!(n, GLOBAL_FALLBACK);
        assert!(!known);
    }

    #[test]
    fn cache_overrides_baseline() {
        let baseline = Catalogue::from_json_str(BASELINE_JSON).unwrap();
        let cache_json = r#"{
            "schema": 3,
            "source": "test",
            "fetched_at": "2099-01-01T00:00:00Z",
            "providers": {
                "anthropic": {
                    "default_context": 200000,
                    "models": {
                        "claude-sonnet-4-6": {"context": 1048576}
                    }
                }
            },
            "aliases": {},
            "fallback": 128000
        }"#;
        let cache = Catalogue::from_json_str(cache_json);
        assert!(cache.is_some());
        let eff = EffectiveCatalogue { cache, baseline };
        let (n, known) = effective_context_window_with(&eff, "claude-sonnet-4-6");
        assert_eq!(n, 1_048_576);
        assert!(known);
    }

    #[test]
    fn wrong_schema_rejected() {
        let c = r#"{"schema": 99, "providers": {}}"#;
        assert!(Catalogue::from_json_str(c).is_none());
    }

    #[test]
    fn schema_2_cache_rejected_after_bump() {
        // A pre-bump user cache must not silently serve stale rows under v3
        // semantics — loader returns None and baseline takes over.
        let old = r#"{"schema": 2, "models": []}"#;
        assert!(Catalogue::from_json_str(old).is_none());
    }

    #[test]
    fn list_models_for_provider_merges_baseline_and_cache() {
        let baseline = Catalogue::from_json_str(
            r#"{
            "schema": 3,
            "providers": {
                "ollama": {
                    "default_context": 8192,
                    "models": {
                        "ollama/llama3.2": {"context": 8192, "source": "baseline"}
                    }
                }
            }
        }"#,
        )
        .unwrap();
        let cache = Catalogue::from_json_str(
            r#"{
            "schema": 3,
            "providers": {
                "ollama": {
                    "default_context": 8192,
                    "models": {
                        "ollama/llama3.2":  {"context": 131072, "source": "user scan"},
                        "ollama/qwen2.5:7b": {"context": 32768, "source": "user scan"}
                    }
                }
            }
        }"#,
        );
        let eff = EffectiveCatalogue { cache, baseline };
        let rows = eff.list_models_for_provider("ollama");
        // Two distinct ids, sorted alphabetically.
        let ids: Vec<&str> = rows.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["ollama/llama3.2", "ollama/qwen2.5:7b"]);
        // Cache row wins on metadata for the overlapping id.
        assert_eq!(rows[0].1.context, Some(131_072));
        assert_eq!(rows[0].1.source.as_deref(), Some("user scan"));
        // Cache-only id is present.
        assert_eq!(rows[1].1.context, Some(32_768));
    }

    #[test]
    fn blank_context_falls_through_to_provider_default() {
        // A known id with `context: null` is visible in the catalogue
        // but triggers the provider-default fallback on lookup.
        let json = r#"{
            "schema": 3,
            "providers": {
                "dashscope": {
                    "default_context": 131072,
                    "models": {
                        "qwen3-0.6b": {}
                    }
                }
            }
        }"#;
        let c = Catalogue::from_json_str(json).expect("parses");
        // Entry exists, context stays None.
        assert!(c.providers["dashscope"].models.contains_key("qwen3-0.6b"));
        assert!(c.providers["dashscope"].models["qwen3-0.6b"]
            .context
            .is_none());
        // Lookup misses — caller applies provider default.
        assert!(c.lookup_context("qwen3-0.6b").is_none());
        let eff = EffectiveCatalogue {
            cache: None,
            baseline: c,
        };
        let (n, known) = effective_context_window_with(&eff, "qwen3-0.6b");
        assert_eq!(n, 131072); // from dashscope.default_context
        assert!(!known); // provider-default, not a verified entry
    }

    #[test]
    fn aliases_resolve_to_canonical() {
        let json = r#"{
            "schema": 3,
            "providers": {
                "anthropic": {
                    "default_context": 200000,
                    "models": {
                        "claude-sonnet-4-6-20261001": {"context": 200000}
                    }
                }
            },
            "aliases": {
                "claude-sonnet-4-6": "claude-sonnet-4-6-20261001"
            }
        }"#;
        let c = Catalogue::from_json_str(json).expect("parses");
        assert_eq!(c.lookup_context("claude-sonnet-4-6"), Some(200_000));
        assert_eq!(
            c.lookup_context("claude-sonnet-4-6-20261001"),
            Some(200_000)
        );
    }

    #[test]
    fn source_and_verified_at_round_trip() {
        let json = r#"{
            "schema": 3,
            "providers": {
                "anthropic": {
                    "default_context": 200000,
                    "models": {
                        "claude-sonnet-4-6": {
                            "context": 200000,
                            "source": "https://docs.anthropic.com/models",
                            "verified_at": "2026-04-24",
                            "max_output": 8192
                        }
                    }
                }
            },
            "fallback": 128000
        }"#;
        let c = Catalogue::from_json_str(json).expect("parses");
        let e = c.providers["anthropic"]
            .models
            .get("claude-sonnet-4-6")
            .unwrap();
        assert_eq!(
            e.source.as_deref(),
            Some("https://docs.anthropic.com/models")
        );
        assert_eq!(e.verified_at.as_deref(), Some("2026-04-24"));
        assert_eq!(e.max_output, Some(8192));
        // Entries without the optional fields still parse.
        let sparse = r#"{
            "schema": 3,
            "providers": {
                "test": {"models": {"x": {"context": 100}}}
            }
        }"#;
        let c2 = Catalogue::from_json_str(sparse).expect("sparse parses");
        let e2 = c2.providers["test"].models.get("x").unwrap();
        assert!(e2.source.is_none());
        assert!(e2.verified_at.is_none());
        assert!(e2.max_output.is_none());
    }
}
