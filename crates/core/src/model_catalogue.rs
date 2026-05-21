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
use std::collections::{BTreeMap, HashMap};
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
/// Schema bumped to 4 in v0.11.0 (dev-plan/24) when ModelEntry gained
/// the 5 optional `*_per_mtok` pricing fields. v3 catalogues load as
/// `None` (the `from_json_str` guard rejects mismatched schemas), which
/// falls through to compiled-in defaults — no panic, just no pricing
/// available until the on-disk file is refreshed.
pub const CURRENT_SCHEMA: u32 = 4;

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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// `true` when the upstream provider lists this model as free
    /// (both prompt and completion token prices = 0). Currently
    /// only populated by OpenRouter; other providers' entries
    /// leave this unset. Drives the "Free only" toggle in the
    /// Settings → OpenRouter card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free: Option<bool>,
    /// `Some(false)` when the upstream provider lists this row with a
    /// non-text output modality (e.g. Lyria → audio, Imagen → image,
    /// embedding endpoints → vector). Such rows must not appear in the
    /// chat-model picker because the agent feeds the response straight
    /// into the conversation. `Some(true)` when output explicitly
    /// includes text; `None` means the catalogue source doesn't
    /// publish modality info — treat as chat-capable (legacy default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<bool>,

    // ── pricing (dev-plan/24) ─────────────────────────────────────
    // USD per million tokens. All fields optional so the catalogue can
    // list a known id whose pricing hasn't been verified yet (`None`
    // ⇒ `compute_cost_usd` returns `None` for that token type;
    // partially-priced entries get `Some(partial)` with the missing
    // type's contribution as zero — caller can log a warning).
    //
    // Currency convention: USD. Multi-currency belongs at the
    // orchestrator's billing edge, not in the catalogue.
    /// Per-million uncached prompt tokens. e.g. 3.00 for Claude Sonnet
    /// 4.6 → $3 / 1M input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_mtok: Option<f64>,
    /// Per-million completion (output) tokens. e.g. 15.00 for Sonnet
    /// 4.6.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_mtok: Option<f64>,
    /// Per-million tokens READ from prompt cache. Anthropic's cache-
    /// read rate (e.g. $0.30/M for Sonnet 4.6), OpenAI's cached-
    /// input rate. When unset and the usage has cached tokens,
    /// callers may fall back to `input_per_mtok` (no discount).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_mtok: Option<f64>,
    /// Per-million tokens WRITTEN to prompt cache. Anthropic charges
    /// extra for cache creation ($3.75/M for Sonnet 4.6 5m TTL). The
    /// 5m vs 1h TTL distinction is collapsed into one field for v1
    /// (per dev-plan/24); split if a real use case needs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_per_mtok: Option<f64>,
    /// Per-million reasoning tokens (OpenAI o1-series visible-thinking
    /// billing, Claude extended thinking when separately billed).
    /// Most providers fold these into output_per_mtok — leave unset
    /// there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_per_mtok: Option<f64>,
    /// `true` when this model is billed through a subscription tier
    /// (ChatGPT Plus/Pro/Team for Codex models, enterprise contracts).
    /// Per-token math doesn't reflect actual cost — surfaces should
    /// show "tier-billed" instead of a $ amount. `compute_cost_usd`
    /// returns `None` for these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_billed: Option<bool>,
}

/// Per-token-type usage counts for one agent run. Fed into
/// [`Catalogue::compute_cost_usd`]. All fields default to 0 so callers
/// can populate only what their provider surfaces (Anthropic, OpenAI's
/// Responses API, and Gemini each expose a different subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Total prompt tokens INCLUDING cached portion. Subtract
    /// `cached_input_tokens` before pricing to avoid double-counting.
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Subset of `prompt_tokens` that hit a cache (cheap read).
    pub cached_input_tokens: u32,
    /// New tokens added to the cache this turn (expensive write).
    /// Distinct from `cached_input_tokens` — that's READ, this is
    /// WRITE.
    pub cache_creation_tokens: u32,
    /// OpenAI o1-style hidden reasoning tokens. Folded into output
    /// pricing for most providers — leave 0 if your provider doesn't
    /// distinguish.
    pub reasoning_tokens: u32,
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
    /// mapped to their entry. `BTreeMap` (not `HashMap`) so serde
    /// emits keys in deterministic alphabetical order — `make
    /// catalogue` produces clean line-by-line diffs instead of a
    /// 6K-line key-shuffle whenever one row changes.
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,
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
    /// gives the map key directly. `BTreeMap` for deterministic
    /// serde order — see `ProviderCatalogue::models` docstring.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderCatalogue>,
    /// User-friendly id → real id. Lets callers pass `claude-sonnet-4-6`
    /// and have it resolved to the current canonical dated variant
    /// (`claude-sonnet-4-6-20261001`). Entries are optional — bare real
    /// ids still look up directly. `BTreeMap` for stable diffs.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
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
        self.lookup_field(model, |e| e.context)
    }

    /// Same matching rules as `lookup_context` but returns the
    /// model's documented max-output-tokens limit (per-row
    /// `maxOutput`), used to cap `max_tokens` on completion calls.
    /// `None` if the row exists but has no `maxOutput`, OR if no row
    /// matches at all — caller picks a safe default.
    pub fn lookup_max_output(&self, model: &str) -> Option<u32> {
        self.lookup_field(model, |e| e.max_output)
    }

    /// Resolve `model` (via alias + vendor-prefix stripping) to the
    /// catalogue entry. Returns the FIRST matching entry: same lookup
    /// order as `lookup_field` but returns the whole row instead of
    /// one numeric field. Used by [`compute_cost_usd`] which needs
    /// multiple fields atomically.
    pub fn find_entry(&self, model: &str) -> Option<&ModelEntry> {
        let canonical = self.resolve_alias(model);
        if let Some(e) = self.find_entry_in_any_provider(canonical) {
            return Some(e);
        }
        let mut remaining = canonical;
        while let Some(idx) = remaining.find('/') {
            remaining = &remaining[idx + 1..];
            if let Some(e) = self.find_entry_in_any_provider(remaining) {
                return Some(e);
            }
        }
        None
    }

    fn find_entry_in_any_provider(&self, id: &str) -> Option<&ModelEntry> {
        let kind_name = crate::providers::ProviderKind::detect(id).map(provider_kind_name);
        if let Some(name) = kind_name {
            if let Some(pc) = self.providers.get(name) {
                if let Some(e) = pc.models.get(id) {
                    return Some(e);
                }
            }
        }
        for pc in self.providers.values() {
            if let Some(e) = pc.models.get(id) {
                return Some(e);
            }
        }
        None
    }

    /// Compute USD cost for a single agent run's token usage.
    ///
    /// Decision tree:
    /// - Model not in catalogue ⇒ `None` (caller surfaces "Cost unavailable")
    /// - Entry has `tier_billed: true` ⇒ `None` (caller surfaces
    ///   "Tier-billed" — per-token math doesn't reflect actual billing)
    /// - Entry has `free: true` ⇒ `Some(0.0)` (regardless of other prices)
    /// - Otherwise sum contributions for whichever pricing fields are set;
    ///   missing field for a non-zero usage count means that piece is
    ///   priced at $0 (caller can detect "partial" by inspecting the
    ///   entry beforehand if it cares).
    ///
    /// Cache math (Anthropic-style): `prompt_tokens` includes the cached
    /// portion. Subtract `cached_input_tokens` so uncached input pays
    /// `input_per_mtok`, cached portion pays `cached_input_per_mtok`,
    /// and newly-written cache tokens pay `cache_creation_per_mtok` on
    /// top. Reasoning tokens (OpenAI o1) are billed separately at
    /// `reasoning_per_mtok` when set; otherwise folded into completion.
    pub fn compute_cost_usd(&self, model: &str, usage: &TokenUsage) -> Option<f64> {
        let entry = self.find_entry(model)?;
        if entry.tier_billed == Some(true) {
            return None;
        }
        if entry.free == Some(true) {
            return Some(0.0);
        }
        let per_m = |count: u32, rate: Option<f64>| -> f64 {
            rate.map_or(0.0, |r| (count as f64 / 1_000_000.0) * r)
        };
        // Cache math: prompt_tokens is the total (Anthropic convention).
        // Uncached input = prompt_tokens - cached_input_tokens. Saturating
        // subtract guards against a buggy provider sending cached > prompt.
        let uncached_input = usage
            .prompt_tokens
            .saturating_sub(usage.cached_input_tokens);
        let uncached = per_m(uncached_input, entry.input_per_mtok);
        let cached = per_m(
            usage.cached_input_tokens,
            entry.cached_input_per_mtok.or(entry.input_per_mtok),
        );
        let cache_write = per_m(usage.cache_creation_tokens, entry.cache_creation_per_mtok);
        let output = per_m(usage.completion_tokens, entry.output_per_mtok);
        let reasoning = per_m(
            usage.reasoning_tokens,
            entry.reasoning_per_mtok.or(entry.output_per_mtok),
        );
        let total = uncached + cached + cache_write + output + reasoning;
        // If nothing was priced (entry exists but every per-mtok field is
        // None), return None rather than $0 — `Some(0.0)` is reserved
        // for explicit `free: true`.
        if entry.input_per_mtok.is_none()
            && entry.output_per_mtok.is_none()
            && entry.cached_input_per_mtok.is_none()
            && entry.cache_creation_per_mtok.is_none()
            && entry.reasoning_per_mtok.is_none()
        {
            return None;
        }
        Some(total)
    }

    fn lookup_field(
        &self,
        model: &str,
        get: impl Fn(&ModelEntry) -> Option<u32> + Copy,
    ) -> Option<u32> {
        let canonical = self.resolve_alias(model);
        if let Some(n) = self.lookup_field_in_any_provider(canonical, get) {
            return Some(n);
        }
        let mut remaining = canonical;
        while let Some(idx) = remaining.find('/') {
            remaining = &remaining[idx + 1..];
            if let Some(n) = self.lookup_field_in_any_provider(remaining, get) {
                return Some(n);
            }
        }
        None
    }

    fn lookup_field_in_any_provider(
        &self,
        id: &str,
        get: impl Fn(&ModelEntry) -> Option<u32> + Copy,
    ) -> Option<u32> {
        let kind_name = crate::providers::ProviderKind::detect(id).map(provider_kind_name);
        if let Some(name) = kind_name {
            if let Some(pc) = self.providers.get(name) {
                if let Some(e) = pc.models.get(id) {
                    if let Some(n) = get(e) {
                        return Some(n);
                    }
                }
            }
        }
        for pc in self.providers.values() {
            if let Some(e) = pc.models.get(id) {
                if let Some(n) = get(e) {
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

/// Runtime layered view. Lookup order, top wins:
/// 1. **User overrides** loaded from `modelOverrides` in project /
///    user `settings.json` (project wins per-key, layered into the
///    same map at load time).
/// 2. **User cache** at `~/.config/thclaws/model_catalogue.json`.
/// 3. **Embedded baseline** compiled into the binary.
/// 4. **Provider default** (from `default_context` on the matched
///    provider catalogue).
/// 5. **Global fallback** (`GLOBAL_FALLBACK`).
///
/// Override keys are `provider_name/model_id` (e.g.
/// `anthropic/claude-sonnet-4-6`). Bare-model keys (`gpt-4o`) match
/// too, after alias-resolve and `vendor/` prefix-strip on the
/// candidate id — same matching rules the catalogue itself uses.
pub struct EffectiveCatalogue {
    pub cache: Option<Catalogue>,
    pub baseline: Catalogue,
    /// User overrides keyed by `provider/model`. `context` wins over
    /// every catalogue layer; `max_output` wins over the same field
    /// in catalogue rows.
    pub overrides: HashMap<String, ModelEntry>,
}

/// Where `effective_context_window_with` resolved its return value.
/// `Override` and `Catalogue` are both "known" sizes; `Fallback`
/// signals the caller to consider nudging `/models refresh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextSource {
    /// User-defined override won — the size came from `modelOverrides`
    /// in project or user settings.json.
    Override,
    /// Found in the user cache or embedded baseline catalogue.
    Catalogue,
    /// No catalogue match — using provider default or global fallback.
    Fallback,
}

impl ContextSource {
    /// `true` for sizes the user can trust (override or catalogue),
    /// `false` for fallbacks. Used to gate the "no catalogue entry"
    /// warning in the `/model` switch flow.
    pub fn is_known(self) -> bool {
        matches!(self, Self::Override | Self::Catalogue)
    }
}

impl EffectiveCatalogue {
    pub fn load() -> Self {
        let baseline = Catalogue::from_json_str(BASELINE_JSON)
            .expect("embedded baseline catalogue must parse");
        let cache = cache_path()
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .and_then(|s| Catalogue::from_json_str(&s));
        let overrides = load_overrides_from_settings();
        Self {
            cache,
            baseline,
            overrides,
        }
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

    /// Two-tier cost computation. Tries the user cache (fresher pricing
    /// from `/models refresh`) first, then the embedded baseline.
    /// `None` propagates from `Catalogue::compute_cost_usd` when the
    /// model is unpriced (e.g. tier-billed) or unknown.
    pub fn compute_cost_usd(&self, model: &str, usage: &TokenUsage) -> Option<f64> {
        if let Some(c) = &self.cache {
            if let Some(cost) = c.compute_cost_usd(model, usage) {
                return Some(cost);
            }
        }
        self.baseline.compute_cost_usd(model, usage)
    }

    /// Look up an override for `model`, applying the same alias and
    /// prefix-strip rules the catalogue uses. Override keys may be
    /// `provider/model` (preferred) or bare `model` — both forms are
    /// tried for each candidate. Aliases resolve both directions:
    /// forward (alias → canonical) when the user types an alias, and
    /// reverse (canonical → alias) so an override keyed by the friendly
    /// name still wins after a `/models refresh` rotates the canonical id.
    pub fn lookup_override(&self, model: &str) -> Option<u32> {
        if self.overrides.is_empty() {
            return None;
        }
        let kind_name = crate::providers::ProviderKind::detect(model).map(provider_kind_name);
        let mut candidates: Vec<String> = vec![model.to_string()];
        // Forward alias: alias-keyed input → canonical id.
        if let Some(c) = &self.cache {
            let resolved = c.resolve_alias(model);
            if resolved != model && !candidates.iter().any(|s| s == resolved) {
                candidates.push(resolved.to_string());
            }
        }
        let resolved = self.baseline.resolve_alias(model);
        if resolved != model && !candidates.iter().any(|s| s == resolved) {
            candidates.push(resolved.to_string());
        }
        // Reverse alias: canonical-keyed input → any alias whose value
        // matches. Lets an override written as `anthropic/claude-sonnet-4-6`
        // still apply to a config that's been pinned to the dated variant.
        let cache_aliases = self.cache.as_ref().map(|c| &c.aliases);
        for table in [Some(&self.baseline.aliases), cache_aliases]
            .into_iter()
            .flatten()
        {
            for (alias, canonical) in table {
                if canonical == model && !candidates.iter().any(|s| s == alias) {
                    candidates.push(alias.clone());
                }
            }
        }
        // Add prefix-stripped variants of every candidate, in order.
        let mut all: Vec<String> = Vec::new();
        for c in &candidates {
            all.push(c.clone());
            let mut rem = c.as_str();
            while let Some(idx) = rem.find('/') {
                rem = &rem[idx + 1..];
                if !all.iter().any(|s| s == rem) {
                    all.push(rem.to_string());
                }
            }
        }
        for id in &all {
            if let Some(name) = kind_name {
                if let Some(e) = self.overrides.get(&format!("{name}/{id}")) {
                    if let Some(n) = e.context {
                        return Some(n);
                    }
                }
            }
            if let Some(e) = self.overrides.get(id) {
                if let Some(n) = e.context {
                    return Some(n);
                }
            }
        }
        None
    }

    pub fn provider_default(&self, provider: &str) -> Option<u32> {
        self.cache
            .as_ref()
            .and_then(|c| c.provider_default(provider))
            .or_else(|| self.baseline.provider_default(provider))
    }

    /// Two-tier exact lookup for `max_output`. Mirrors `lookup_exact`
    /// for the context window. Used to cap `max_tokens` on
    /// completion calls so we don't blow past the model's documented
    /// limit (e.g. gpt-4.1 = 32768).
    pub fn lookup_max_output_exact(&self, model: &str) -> Option<u32> {
        if let Some(c) = &self.cache {
            if let Some(n) = c.lookup_max_output(model) {
                return Some(n);
            }
        }
        self.baseline.lookup_max_output(model)
    }

    /// Override-layer max_output lookup. Same matching rules as
    /// `lookup_override` but reads `maxOutput` instead of `context`
    /// from each candidate override entry.
    pub fn lookup_max_output_override(&self, model: &str) -> Option<u32> {
        if self.overrides.is_empty() {
            return None;
        }
        let kind_name = crate::providers::ProviderKind::detect(model).map(provider_kind_name);
        let mut candidates: Vec<String> = vec![model.to_string()];
        if let Some(c) = &self.cache {
            let resolved = c.resolve_alias(model);
            if resolved != model && !candidates.iter().any(|s| s == resolved) {
                candidates.push(resolved.to_string());
            }
        }
        let resolved = self.baseline.resolve_alias(model);
        if resolved != model && !candidates.iter().any(|s| s == resolved) {
            candidates.push(resolved.to_string());
        }
        let mut all: Vec<String> = Vec::new();
        for c in &candidates {
            all.push(c.clone());
            let mut rem = c.as_str();
            while let Some(idx) = rem.find('/') {
                rem = &rem[idx + 1..];
                if !all.iter().any(|s| s == rem) {
                    all.push(rem.to_string());
                }
            }
        }
        for id in &all {
            if let Some(name) = kind_name {
                if let Some(e) = self.overrides.get(&format!("{name}/{id}")) {
                    if let Some(n) = e.max_output {
                        return Some(n);
                    }
                }
            }
            if let Some(e) = self.overrides.get(id) {
                if let Some(n) = e.max_output {
                    return Some(n);
                }
            }
        }
        None
    }

    /// Merged model listing for one provider — baseline rows plus user-cache
    /// rows, with cache winning on metadata when the same id appears in both.
    /// Override rows for the same provider are folded in last (override wins
    /// on `context`, plus a synthetic `source: "override"` is stamped so
    /// callers can render an override marker).
    /// Returns `(id, entry)` pairs sorted by id. Consumed by the `/models`
    /// slash command to render a catalogue-based list instead of hitting the
    /// provider's live `/v1/models` endpoint.
    pub fn list_models_for_provider(&self, provider: &str) -> Vec<(String, ModelEntry)> {
        let mut out: HashMap<String, ModelEntry> = HashMap::new();
        // Back-compat: older caches keyed AgentSdk as "agent-sdk"
        // (the pre-fix `provider_kind_name` return value). The
        // canonical key is now "anthropic-agent" — accept both at
        // lookup time so an outdated `~/.config/thclaws/model_catalogue.json`
        // doesn't surface as an empty picker until the user
        // remembers to `/models refresh`.
        let legacy_alias: Option<&str> = match provider {
            "anthropic-agent" => Some("agent-sdk"),
            _ => None,
        };
        if let Some(pc) = self.baseline.providers.get(provider) {
            for (id, e) in &pc.models {
                out.insert(id.clone(), e.clone());
            }
        }
        if let Some(c) = &self.cache {
            let mut cache_provider = c.providers.get(provider);
            if cache_provider.is_none() {
                if let Some(alias) = legacy_alias {
                    cache_provider = c.providers.get(alias);
                }
            }
            if let Some(pc) = cache_provider {
                for (id, e) in &pc.models {
                    // Cache row wins on metadata it actually carries,
                    // but when the cache lacks a field that the baseline
                    // knows about, prefer the baseline value rather than
                    // nulling it. Matters for `free` after a `/models
                    // refresh` written by an older binary (pre-`free`
                    // field) — without this carve-out, every cached row
                    // would mask the baseline's `free: true`.
                    let merged = match out.remove(id) {
                        Some(baseline) => ModelEntry {
                            context: e.context.or(baseline.context),
                            max_output: e.max_output.or(baseline.max_output),
                            source: e.source.clone().or(baseline.source),
                            verified_at: e.verified_at.clone().or(baseline.verified_at),
                            free: e.free.or(baseline.free),
                            chat: e.chat.or(baseline.chat),
                            input_per_mtok: e.input_per_mtok.or(baseline.input_per_mtok),
                            output_per_mtok: e.output_per_mtok.or(baseline.output_per_mtok),
                            cached_input_per_mtok: e
                                .cached_input_per_mtok
                                .or(baseline.cached_input_per_mtok),
                            cache_creation_per_mtok: e
                                .cache_creation_per_mtok
                                .or(baseline.cache_creation_per_mtok),
                            reasoning_per_mtok: e
                                .reasoning_per_mtok
                                .or(baseline.reasoning_per_mtok),
                            tier_billed: e.tier_billed.or(baseline.tier_billed),
                        },
                        None => e.clone(),
                    };
                    out.insert(id.clone(), merged);
                }
            }
        }
        // Fold matching overrides on top. Keys are `provider/model_id`; a
        // bare-id key without `/` doesn't get folded into a per-provider
        // listing (it isn't scoped to one provider).
        let prefix = format!("{provider}/");
        for (key, entry) in &self.overrides {
            let Some(id) = key.strip_prefix(&prefix) else {
                continue;
            };
            let merged = match out.remove(id) {
                Some(mut existing) => {
                    if let Some(n) = entry.context {
                        existing.context = Some(n);
                    }
                    if let Some(n) = entry.max_output {
                        existing.max_output = Some(n);
                    }
                    existing.source = Some("override".to_string());
                    existing
                }
                None => ModelEntry {
                    context: entry.context,
                    max_output: entry.max_output,
                    source: Some("override".to_string()),
                    verified_at: None,
                    free: entry.free,
                    chat: entry.chat,
                    ..Default::default()
                },
            };
            out.insert(id.to_string(), merged);
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

/// Resolve the effective context window for `model`. Layered fallback:
/// user override → user cache → baseline → provider default → global
/// fallback. The returned `ContextSource` distinguishes override hits
/// from catalogue hits and from fallbacks — callers (e.g. `/models`
/// rendering, the no-catalogue-entry warning) use it to decide when to
/// nudge the user.
pub fn effective_context_window(model: &str) -> u32 {
    effective_context_window_with(&EffectiveCatalogue::load(), model).0
}

/// Look up the effective `max_output` (max completion tokens) for a
/// model: override > catalogue > `None`. Callers cap their requested
/// `max_tokens` against this so we don't hit per-model 400 errors
/// (e.g. gpt-4.1 = 32768). `None` means no documented limit was
/// found — caller picks a safe default.
pub fn effective_max_output(model: &str) -> Option<u32> {
    let cat = EffectiveCatalogue::load();
    cat.lookup_max_output_override(model)
        .or_else(|| cat.lookup_max_output_exact(model))
}

pub fn effective_context_window_with(
    cat: &EffectiveCatalogue,
    model: &str,
) -> (u32, ContextSource) {
    if let Some(n) = cat.lookup_override(model) {
        return (n, ContextSource::Override);
    }
    if let Some(n) = cat.lookup_exact(model) {
        return (n, ContextSource::Catalogue);
    }
    let provider_name = crate::providers::ProviderKind::detect(model)
        .map(|k| provider_kind_name(k))
        .unwrap_or("");
    if !provider_name.is_empty() {
        if let Some(n) = cat.provider_default(provider_name) {
            return (n, ContextSource::Fallback);
        }
    }
    (cat.fallback(), ContextSource::Fallback)
}

/// Read `modelOverrides` blocks from project + user `settings.json`,
/// project wins per-key. Standalone (no `crate::config` dep) so the
/// catalogue stays leaf-level in the dep graph. Schema:
///
/// ```json
/// {
///   "modelOverrides": {
///     "anthropic/claude-sonnet-4-6": { "context": 200000, "maxOutput": 32768 }
///   }
/// }
/// ```
pub fn load_overrides_from_settings() -> HashMap<String, ModelEntry> {
    let mut out: HashMap<String, ModelEntry> = HashMap::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(home) = crate::util::home_dir() {
        paths.push(home.join(".config/thclaws/settings.json"));
    }
    let project_root = std::env::var("THCLAWS_PROJECT_ROOT")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    if let Some(root) = project_root {
        paths.push(root.join(".thclaws").join("settings.json"));
    }
    for path in &paths {
        let Ok(s) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(v): std::result::Result<serde_json::Value, _> = serde_json::from_str(&s) else {
            continue;
        };
        let Some(obj) = v.get("modelOverrides").and_then(|m| m.as_object()) else {
            continue;
        };
        for (k, raw) in obj {
            let entry = ModelEntry {
                context: raw
                    .get("context")
                    .and_then(|c| c.as_u64())
                    .map(|n| n as u32),
                max_output: raw
                    .get("maxOutput")
                    .and_then(|c| c.as_u64())
                    .map(|n| n as u32),
                source: Some("override".to_string()),
                verified_at: None,
                free: None,
                chat: None,
                ..Default::default()
            };
            // Project (read second) wins per-key.
            out.insert(k.clone(), entry);
        }
    }
    out
}

/// Write or remove a `modelOverrides` entry in project or user
/// `settings.json`, preserving every other field. `entry: None` clears
/// the key. Returns the path written to.
pub fn save_override(
    key: &str,
    entry: Option<ModelEntry>,
    scope: OverrideScope,
) -> Result<PathBuf, RefreshError> {
    let path = match scope {
        OverrideScope::User => {
            let home = crate::util::home_dir().ok_or(RefreshError::NoHome)?;
            home.join(".config/thclaws/settings.json")
        }
        OverrideScope::Project => {
            let root = std::env::var("THCLAWS_PROJECT_ROOT")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::current_dir().ok())
                .ok_or(RefreshError::NoHome)?;
            root.join(".thclaws").join("settings.json")
        }
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RefreshError::Io(e.to_string()))?;
    }
    let mut root: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let overrides = root
        .as_object_mut()
        .unwrap()
        .entry("modelOverrides".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !overrides.is_object() {
        *overrides = serde_json::json!({});
    }
    let map = overrides.as_object_mut().unwrap();
    match entry {
        Some(e) => {
            let mut row = serde_json::Map::new();
            if let Some(n) = e.context {
                row.insert("context".to_string(), serde_json::json!(n));
            }
            if let Some(n) = e.max_output {
                row.insert("maxOutput".to_string(), serde_json::json!(n));
            }
            map.insert(key.to_string(), serde_json::Value::Object(row));
        }
        None => {
            map.remove(key);
            if map.is_empty() {
                root.as_object_mut().unwrap().remove("modelOverrides");
            }
        }
    }
    let body = serde_json::to_string_pretty(&root).map_err(|e| RefreshError::Io(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| RefreshError::Io(e.to_string()))?;
    std::fs::rename(&tmp, &path).map_err(|e| RefreshError::Io(e.to_string()))?;
    Ok(path)
}

/// Where a `modelOverrides` write lands: user-global (most cases) or
/// scoped to the current project's `.thclaws/settings.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideScope {
    User,
    Project,
}

/// Stable short identifier matching the `provider` field in the
/// catalogue JSON. Mirrors `ProviderKind::name` except for
/// `Ollama`/`OllamaAnthropic`/`AgentSdk` which we namespace
/// differently in the catalogue for clarity.
/// Render a catalogue model id as the canonical, routable form for the
/// `/models` slash command + the GUI model picker.
///
/// Strategy:
/// 1. If the id is already qualified with the provider's path segment
///    (e.g. catalogue stored `dashscope/qwen-vl-plus` or
///    `agent/claude-sonnet-4-6`), keep it as-is. **Never double-prefix.**
/// 2. Else if `ProviderKind::detect(id)` resolves to the same provider
///    we're rendering for, the bare id is already self-routing (e.g.
///    Anthropic's `claude-sonnet-4-6`, OpenAI's `gpt-4o`, DashScope's
///    `qwen-max`). Keep bare.
/// 3. Else prefix with `<provider>/` so `/model <copied-id>` can route
///    through `ProviderKind::detect`. This is what OpenRouter rows
///    rely on — the catalogue stores them bare (`google/gemma-…:free`)
///    and the prefix makes them routable.
///
/// The pre-fix logic compared `ProviderKind::name()` to
/// `provider_kind_name()` for the "same provider" check, which silently
/// diverged on AgentSdk (`anthropic-agent` vs `agent-sdk`) and caused
/// every AgentSdk row to over-prefix. Two same-source-of-truth checks
/// here instead: literal prefix match + detect-to-the-same-kind.
pub fn canonical_model_id(provider_name: &str, id: &str) -> String {
    let prefix = format!("{}/", provider_name);
    if id.starts_with(&prefix) {
        return id.to_string();
    }
    if let Some(detected) = crate::providers::ProviderKind::detect(id) {
        if provider_kind_name(detected) == provider_name {
            return id.to_string();
        }
    }
    format!("{provider_name}/{id}")
}

pub fn provider_kind_name(k: crate::providers::ProviderKind) -> &'static str {
    use crate::providers::ProviderKind;
    match k {
        ProviderKind::Anthropic => "anthropic",
        // Must match `ProviderKind::name()` so a round-trip via
        // `detect_provider()` → `provider_kind_name(kind)` returns
        // the same key. Pre-fix this returned `"agent-sdk"` while
        // `name()` returned `"anthropic-agent"`; the divergence
        // broke /models for the agent/* provider — `/model<enter>`
        // looked up the catalogue under "anthropic-agent", missed
        // the renamed `agent-sdk` section, and the picker never
        // opened.
        ProviderKind::AgentSdk => "anthropic-agent",
        ProviderKind::OpenAI => "openai",
        ProviderKind::OpenAIResponses => "openai-responses",
        ProviderKind::ChatGptCodex => "chatgpt-codex",
        ProviderKind::OpenRouter => "openrouter",
        ProviderKind::Gemini => "gemini",
        ProviderKind::Ollama => "ollama",
        ProviderKind::OllamaAnthropic => "ollama-anthropic",
        ProviderKind::OllamaCloud => "ollama-cloud",
        ProviderKind::DashScope => "dashscope",
        ProviderKind::QwenCloud => "qwen-cloud",
        ProviderKind::AgenticPress => "agentic-press",
        ProviderKind::ZAi => "zai",
        ProviderKind::LMStudio => "lmstudio",
        ProviderKind::AzureAIFoundry => "azure",
        ProviderKind::OpenAICompat => "openai-compat",
        ProviderKind::DeepSeek => "deepseek",
        ProviderKind::ThaiLLM => "thaillm",
        ProviderKind::Nvidia => "nvidia",
        ProviderKind::OpenCodeGo => "opencode-go",
        ProviderKind::Minimax => "minimax",
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
mod canonical_id_tests {
    use super::canonical_model_id;

    // Anthropic / OpenAI / Gemini / DashScope: catalogue stores bare,
    // self-routing ids — the helper must keep them bare and NEVER
    // prepend the provider name.
    #[test]
    fn anthropic_bare_id_stays_bare() {
        assert_eq!(
            canonical_model_id("anthropic", "claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn openai_bare_id_stays_bare() {
        assert_eq!(canonical_model_id("openai", "gpt-4o"), "gpt-4o");
    }

    #[test]
    fn gemini_bare_id_stays_bare() {
        assert_eq!(
            canonical_model_id("gemini", "gemini-2.5-flash"),
            "gemini-2.5-flash"
        );
    }

    #[test]
    fn dashscope_bare_qwen_id_stays_bare() {
        assert_eq!(canonical_model_id("dashscope", "qwen-max"), "qwen-max");
    }

    // The user-reported bug: catalogue rows for DashScope had a mix of
    // bare ids AND ids that already included the `dashscope/` prefix.
    // The pre-fix logic prepended a second one. Helper must idempotently
    // keep already-qualified ids unchanged.
    #[test]
    fn dashscope_already_prefixed_id_is_not_double_prefixed() {
        assert_eq!(
            canonical_model_id("dashscope", "dashscope/qwen-vl-plus"),
            "dashscope/qwen-vl-plus"
        );
    }

    // OpenRouter is the original motivation: catalogue rows look like
    // `google/gemma-3-27b-it:free` (bare, no `openrouter/` prefix), and
    // those don't route via `ProviderKind::detect` without qualification.
    #[test]
    fn openrouter_bare_vendor_id_gets_prefixed() {
        assert_eq!(
            canonical_model_id("openrouter", "google/gemma-3-27b-it:free"),
            "openrouter/google/gemma-3-27b-it:free"
        );
    }

    // OpenRouter ids that already include the prefix in the catalogue
    // stay as-is.
    #[test]
    fn openrouter_already_prefixed_id_stays_single_prefixed() {
        assert_eq!(
            canonical_model_id("openrouter", "openrouter/anthropic/claude-sonnet-4-6"),
            "openrouter/anthropic/claude-sonnet-4-6"
        );
    }

    // AgentSdk: pre-fix the name() vs provider_kind_name() mismatch
    // (`anthropic-agent` vs `agent-sdk`) sent the detect-arm to the
    // wrong key. Both functions now return `"anthropic-agent"`; this
    // test pins the canonical post-rename name.
    #[test]
    fn agent_sdk_id_stays_single_prefixed() {
        assert_eq!(
            canonical_model_id("anthropic-agent", "agent/claude-sonnet-4-6"),
            "agent/claude-sonnet-4-6"
        );
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

    fn baseline_only() -> EffectiveCatalogue {
        EffectiveCatalogue {
            cache: None,
            baseline: Catalogue::from_json_str(BASELINE_JSON).unwrap(),
            overrides: HashMap::new(),
        }
    }

    #[test]
    fn lookup_finds_exact_model() {
        let c = baseline_only();
        let (n, src) = effective_context_window_with(&c, "claude-sonnet-4-6");
        assert_eq!(n, 200_000);
        assert_eq!(src, ContextSource::Catalogue);
    }

    #[test]
    fn lookup_strips_vendor_prefix() {
        let c = baseline_only();
        let (n, src) = effective_context_window_with(&c, "openrouter/anthropic/claude-sonnet-4-6");
        assert_eq!(n, 200_000);
        assert_eq!(src, ContextSource::Catalogue);
    }

    #[test]
    fn lookup_falls_back_to_provider_default_and_flags_unknown() {
        let c = baseline_only();
        let (n, src) = effective_context_window_with(&c, "claude-future-x99");
        assert_eq!(n, 200_000);
        assert_eq!(src, ContextSource::Fallback);
    }

    #[test]
    fn lookup_falls_back_to_global_for_unknown_provider() {
        let c = baseline_only();
        let (n, src) = effective_context_window_with(&c, "unknown-vendor/unknown-model");
        assert_eq!(n, GLOBAL_FALLBACK);
        assert_eq!(src, ContextSource::Fallback);
    }

    #[test]
    fn cache_overrides_baseline() {
        let baseline = Catalogue::from_json_str(BASELINE_JSON).unwrap();
        let cache_json = r#"{
            "schema": 4,
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
        let eff = EffectiveCatalogue {
            cache,
            baseline,
            overrides: HashMap::new(),
        };
        let (n, src) = effective_context_window_with(&eff, "claude-sonnet-4-6");
        assert_eq!(n, 1_048_576);
        assert_eq!(src, ContextSource::Catalogue);
    }

    fn override_entry(context: u32) -> ModelEntry {
        ModelEntry {
            context: Some(context),
            max_output: None,
            source: Some("override".into()),
            verified_at: None,
            free: None,
            chat: None,
            ..Default::default()
        }
    }

    #[test]
    fn override_beats_user_cache() {
        // Same id is in both the user cache (256k) and overrides (100k).
        // Override wins.
        let baseline = Catalogue::from_json_str(BASELINE_JSON).unwrap();
        let cache = Catalogue::from_json_str(
            r#"{
            "schema": 4,
            "providers": {
                "anthropic": {
                    "default_context": 200000,
                    "models": {
                        "claude-sonnet-4-6": {"context": 256000}
                    }
                }
            }
        }"#,
        );
        let mut overrides = HashMap::new();
        overrides.insert(
            "anthropic/claude-sonnet-4-6".to_string(),
            override_entry(100_000),
        );
        let eff = EffectiveCatalogue {
            cache,
            baseline,
            overrides,
        };
        let (n, src) = effective_context_window_with(&eff, "claude-sonnet-4-6");
        assert_eq!(n, 100_000);
        assert_eq!(src, ContextSource::Override);
    }

    #[test]
    fn override_resolves_alias() {
        // `claude-sonnet-4-6` is an alias for the dated variant. Override
        // is keyed against the alias (the form the user typed) and still
        // wins for the dated lookup.
        let json = r#"{
            "schema": 4,
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
        let baseline = Catalogue::from_json_str(json).unwrap();
        let mut overrides = HashMap::new();
        overrides.insert(
            "anthropic/claude-sonnet-4-6".to_string(),
            override_entry(50_000),
        );
        let eff = EffectiveCatalogue {
            cache: None,
            baseline,
            overrides,
        };
        let (n, src) = effective_context_window_with(&eff, "claude-sonnet-4-6-20261001");
        assert_eq!(n, 50_000);
        assert_eq!(src, ContextSource::Override);
    }

    #[test]
    fn override_strips_vendor_prefix() {
        // `openrouter/anthropic/claude-sonnet-4-6` should match an
        // override keyed `anthropic/claude-sonnet-4-6` once the
        // outermost `openrouter/` prefix is stripped.
        let baseline = Catalogue::from_json_str(BASELINE_JSON).unwrap();
        let mut overrides = HashMap::new();
        overrides.insert(
            "anthropic/claude-sonnet-4-6".to_string(),
            override_entry(64_000),
        );
        let eff = EffectiveCatalogue {
            cache: None,
            baseline,
            overrides,
        };
        let (n, src) =
            effective_context_window_with(&eff, "openrouter/anthropic/claude-sonnet-4-6");
        assert_eq!(n, 64_000);
        assert_eq!(src, ContextSource::Override);
    }

    #[test]
    fn override_removal_falls_back_to_catalogue() {
        // Empty override map → catalogue layer wins as before.
        let eff = baseline_only();
        let (n, src) = effective_context_window_with(&eff, "claude-sonnet-4-6");
        assert_eq!(n, 200_000);
        assert_eq!(src, ContextSource::Catalogue);
    }

    #[test]
    fn override_can_exceed_catalogue_value() {
        // Trust + warn policy: override wins even when above the
        // catalogue value. (Caller surfaces a warning at save-time.)
        let baseline = Catalogue::from_json_str(BASELINE_JSON).unwrap();
        let mut overrides = HashMap::new();
        overrides.insert(
            "anthropic/claude-sonnet-4-6".to_string(),
            override_entry(2_000_000),
        );
        let eff = EffectiveCatalogue {
            cache: None,
            baseline,
            overrides,
        };
        let (n, src) = effective_context_window_with(&eff, "claude-sonnet-4-6");
        assert_eq!(n, 2_000_000);
        assert_eq!(src, ContextSource::Override);
    }

    #[test]
    fn bare_model_override_key_works() {
        // Key without a `provider/` prefix matches when the bare id
        // equals the input (or any prefix-stripped form of it).
        let baseline = Catalogue::from_json_str(BASELINE_JSON).unwrap();
        let mut overrides = HashMap::new();
        overrides.insert("claude-sonnet-4-6".to_string(), override_entry(80_000));
        let eff = EffectiveCatalogue {
            cache: None,
            baseline,
            overrides,
        };
        let (n, src) = effective_context_window_with(&eff, "claude-sonnet-4-6");
        assert_eq!(n, 80_000);
        assert_eq!(src, ContextSource::Override);
    }

    #[test]
    fn list_models_marks_override_rows() {
        let baseline = Catalogue::from_json_str(
            r#"{
            "schema": 4,
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
        let mut overrides = HashMap::new();
        overrides.insert("ollama/ollama/llama3.2".to_string(), override_entry(32_768));
        let eff = EffectiveCatalogue {
            cache: None,
            baseline,
            overrides,
        };
        let rows = eff.list_models_for_provider("ollama");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.context, Some(32_768));
        assert_eq!(rows[0].1.source.as_deref(), Some("override"));
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
            "schema": 4,
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
            "schema": 4,
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
        let eff = EffectiveCatalogue {
            cache,
            baseline,
            overrides: HashMap::new(),
        };
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

    /// User-reported bug: on /provider anthropic-agent, `/model<enter>`
    /// asked the catalogue for "anthropic-agent" (the value of
    /// `Kind::name()` + `detect_provider()`), but the baseline JSON
    /// keyed AgentSdk under "agent-sdk" — so `list_models_for_provider`
    /// returned 0 rows and the model-picker modal never opened.
    ///
    /// Post-fix, both names point at the same provider rows. The
    /// baseline+seed code uses the canonical "anthropic-agent" key;
    /// older user caches that still use "agent-sdk" are picked up via
    /// the legacy-alias fallback in `list_models_for_provider`.
    #[test]
    fn list_models_for_provider_resolves_anthropic_agent() {
        let baseline = Catalogue::from_json_str(
            r#"{
            "schema": 4,
            "providers": {
                "anthropic-agent": {
                    "default_context": 200000,
                    "models": {
                        "agent/claude-sonnet-4-6": {"context": 200000}
                    }
                }
            }
        }"#,
        )
        .unwrap();
        let eff = EffectiveCatalogue {
            cache: None,
            baseline,
            overrides: HashMap::new(),
        };
        let rows = eff.list_models_for_provider("anthropic-agent");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "agent/claude-sonnet-4-6");
    }

    /// Back-compat: a user with a `/models refresh` cache written by
    /// the pre-fix binary keyed AgentSdk as "agent-sdk". The fixed
    /// `list_models_for_provider` accepts that legacy key when called
    /// with the canonical "anthropic-agent" name, so the picker opens
    /// for users who haven't yet `/models refresh`'d.
    #[test]
    fn list_models_for_provider_legacy_agent_sdk_cache_falls_back() {
        let baseline = Catalogue::from_json_str(r#"{"schema": 4, "providers": {}}"#).unwrap();
        let cache = Catalogue::from_json_str(
            r#"{
            "schema": 4,
            "providers": {
                "agent-sdk": {
                    "default_context": 200000,
                    "models": {
                        "agent/claude-opus-4-6": {"context": 200000}
                    }
                }
            }
        }"#,
        );
        let eff = EffectiveCatalogue {
            cache,
            baseline,
            overrides: HashMap::new(),
        };
        let rows = eff.list_models_for_provider("anthropic-agent");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "agent/claude-opus-4-6");
    }

    #[test]
    fn cache_missing_free_inherits_baseline() {
        // Regression: a `/models refresh` written by an older binary
        // omits the `free` field entirely. The cache row must not
        // silently mask the baseline's `free: true`, otherwise the
        // "Free only" filter shows almost nothing.
        let baseline = Catalogue::from_json_str(
            r#"{
            "schema": 4,
            "providers": {
                "openrouter": {
                    "default_context": 8192,
                    "models": {
                        "vendor/free-model:free": {"context": 131072, "free": true},
                        "vendor/paid-model":     {"context": 131072, "free": false}
                    }
                }
            }
        }"#,
        )
        .unwrap();
        // Cache row mirrors the pre-`free` schema: no `free` key at all.
        let cache = Catalogue::from_json_str(
            r#"{
            "schema": 4,
            "providers": {
                "openrouter": {
                    "default_context": 8192,
                    "models": {
                        "vendor/free-model:free": {"context": 200000},
                        "vendor/paid-model":     {"context": 200000}
                    }
                }
            }
        }"#,
        );
        let eff = EffectiveCatalogue {
            cache,
            baseline,
            overrides: HashMap::new(),
        };
        let rows = eff.list_models_for_provider("openrouter");
        let by_id: HashMap<_, _> = rows.into_iter().collect();
        // Cache wins on context (200k overrides 131k)…
        assert_eq!(by_id["vendor/free-model:free"].context, Some(200_000));
        // …but `free` falls back to the baseline value, not None.
        assert_eq!(by_id["vendor/free-model:free"].free, Some(true));
        assert_eq!(by_id["vendor/paid-model"].free, Some(false));
    }

    #[test]
    fn blank_context_falls_through_to_provider_default() {
        // A known id with `context: null` is visible in the catalogue
        // but triggers the provider-default fallback on lookup.
        let json = r#"{
            "schema": 4,
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
            overrides: HashMap::new(),
        };
        let (n, src) = effective_context_window_with(&eff, "qwen3-0.6b");
        assert_eq!(n, 131072); // from dashscope.default_context
        assert_eq!(src, ContextSource::Fallback); // provider-default, not a verified entry
    }

    #[test]
    fn aliases_resolve_to_canonical() {
        let json = r#"{
            "schema": 4,
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
            "schema": 4,
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
            "schema": 4,
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

    // ── compute_cost_usd (dev-plan/24) ─────────────────────────────

    fn pricing_catalogue() -> Catalogue {
        Catalogue::from_json_str(
            r#"{
                "schema": 4,
                "providers": {
                    "anthropic": {
                        "models": {
                            "claude-sonnet-4-6": {
                                "input_per_mtok": 3.0,
                                "output_per_mtok": 15.0,
                                "cached_input_per_mtok": 0.3,
                                "cache_creation_per_mtok": 3.75
                            }
                        }
                    },
                    "openai": {
                        "models": {
                            "o1-mini": {
                                "input_per_mtok": 3.0,
                                "output_per_mtok": 12.0,
                                "cached_input_per_mtok": 1.5,
                                "reasoning_per_mtok": 12.0
                            },
                            "gpt-4o": {
                                "input_per_mtok": 2.5,
                                "output_per_mtok": 10.0
                            }
                        }
                    },
                    "openrouter": {
                        "models": {
                            "openrouter/free-tier-model": {
                                "free": true
                            }
                        }
                    },
                    "openai-codex": {
                        "models": {
                            "chatgpt-codex/gpt-5.4": {
                                "tier_billed": true
                            }
                        }
                    },
                    "google": {
                        "models": {
                            "gemini-context-only": {
                                "context": 1000000
                            }
                        }
                    }
                }
            }"#,
        )
        .expect("pricing catalogue parses")
    }

    #[test]
    fn cost_anthropic_with_cache_tiers() {
        // 1000 prompt (200 cached read, so 800 uncached), 500 completion,
        // 100 cache writes.
        //   uncached input  800/1M * $3.00 = $0.00240
        //   cached read     200/1M * $0.30 = $0.00006
        //   cache write     100/1M * $3.75 = $0.000375
        //   completion      500/1M * $15.00 = $0.00750
        //   sum             = $0.010335
        let c = pricing_catalogue();
        let cost = c
            .compute_cost_usd(
                "claude-sonnet-4-6",
                &TokenUsage {
                    prompt_tokens: 1000,
                    completion_tokens: 500,
                    cached_input_tokens: 200,
                    cache_creation_tokens: 100,
                    reasoning_tokens: 0,
                },
            )
            .expect("priced");
        assert!((cost - 0.010335).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn cost_reasoning_tokens_billed_separately() {
        let c = pricing_catalogue();
        let cost = c
            .compute_cost_usd(
                "o1-mini",
                &TokenUsage {
                    prompt_tokens: 1000,
                    completion_tokens: 500,
                    cached_input_tokens: 0,
                    cache_creation_tokens: 0,
                    reasoning_tokens: 200,
                },
            )
            .expect("priced");
        // 1000 * 3.0/1M + 500 * 12.0/1M + 200 * 12.0/1M
        // = 0.003 + 0.006 + 0.0024 = 0.0114
        assert!((cost - 0.0114).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn cost_falls_back_to_input_when_cached_rate_absent() {
        // gpt-4o has only input + output prices. cached_input_tokens
        // bills at input_per_mtok (no discount fallback).
        let c = pricing_catalogue();
        let cost = c
            .compute_cost_usd(
                "gpt-4o",
                &TokenUsage {
                    prompt_tokens: 1000,
                    completion_tokens: 500,
                    cached_input_tokens: 400,
                    cache_creation_tokens: 0,
                    reasoning_tokens: 0,
                },
            )
            .expect("priced");
        // 600 * 2.5/1M + 400 * 2.5/1M + 500 * 10.0/1M
        // = 0.0015 + 0.001 + 0.005 = 0.0075
        assert!((cost - 0.0075).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn cost_free_tier_returns_zero() {
        let c = pricing_catalogue();
        let cost = c
            .compute_cost_usd(
                "openrouter/free-tier-model",
                &TokenUsage {
                    prompt_tokens: 1_000_000,
                    completion_tokens: 1_000_000,
                    ..Default::default()
                },
            )
            .expect("free entry");
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn cost_tier_billed_returns_none() {
        let c = pricing_catalogue();
        let result = c.compute_cost_usd(
            "chatgpt-codex/gpt-5.4",
            &TokenUsage {
                prompt_tokens: 1000,
                completion_tokens: 500,
                ..Default::default()
            },
        );
        assert!(
            result.is_none(),
            "tier-billed must return None so UI shows 'tier-billed' instead of $0"
        );
    }

    #[test]
    fn cost_unknown_model_returns_none() {
        let c = pricing_catalogue();
        let result = c.compute_cost_usd(
            "not-in-catalogue/whatever-1.0",
            &TokenUsage {
                prompt_tokens: 100,
                ..Default::default()
            },
        );
        assert!(result.is_none());
    }

    #[test]
    fn cost_entry_without_any_prices_returns_none() {
        let c = pricing_catalogue();
        let result = c.compute_cost_usd(
            "gemini-context-only",
            &TokenUsage {
                prompt_tokens: 100,
                ..Default::default()
            },
        );
        assert!(result.is_none());
    }

    #[test]
    fn cost_saturating_sub_guards_against_buggy_cached_gt_prompt() {
        let c = pricing_catalogue();
        let cost = c
            .compute_cost_usd(
                "claude-sonnet-4-6",
                &TokenUsage {
                    prompt_tokens: 100,
                    cached_input_tokens: 200, // > prompt
                    completion_tokens: 0,
                    cache_creation_tokens: 0,
                    reasoning_tokens: 0,
                },
            )
            .expect("priced");
        // uncached 0 + cached 200 * 0.3/1M = 0.00006
        assert!((cost - 0.00006).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn schema_v3_rejected_after_bump() {
        let v3 = r#"{"schema": 3, "providers": {}}"#;
        assert!(
            Catalogue::from_json_str(v3).is_none(),
            "v3 must not load against v4 binary — falls through to compiled-in baseline"
        );
    }
}
