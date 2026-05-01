//! catalogue-seed — operator tool that merges provider `/v1/models`
//! output into the baseline catalogue JSON without overwriting
//! hand-curated rows.
//!
//! Providers probed (each gated on the presence of its API key so the
//! tool degrades gracefully when only some keys are configured):
//!
//!   - OpenRouter   (always, no key needed)  → long-tail filler
//!   - Anthropic    (ANTHROPIC_API_KEY)      → real dated ids, context from OpenRouter or existing curation
//!   - OpenAI       (OPENAI_API_KEY)         → real dated ids, context from OpenRouter or existing curation
//!   - Gemini       (GEMINI_API_KEY)         → real ids + inputTokenLimit
//!   - DeepSeek     (DEEPSEEK_API_KEY)       → V4 line (flash/pro), context from OpenRouter mirror or default 128K
//!   - Ollama       (if OLLAMA_HOST reachable, default http://localhost:11434)
//!
//! New ids are inserted into the appropriate `providers.<name>.models`
//! submap. Hand-curated rows are never overwritten — the `id` is the
//! map key and we only write when absent. Stale rows are left in place;
//! a vendor removing a model doesn't delete its entry automatically
//! (operator deletes manually after reviewing the diff).
//!
//! Usage:
//!   cargo run --bin catalogue-seed -- [path/to/model_catalogue.json]
//!
//! Exit non-zero on any hard failure so CI can gate a refresh PR.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use thclaws_core::model_catalogue::{Catalogue, ModelEntry, ProviderCatalogue, CURRENT_SCHEMA};

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/models";
const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/models";
const OPENAI_URL: &str = "https://api.openai.com/v1/models";
const GEMINI_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const OLLAMA_CLOUD_URL: &str = "https://ollama.com/v1/models";
const DEEPSEEK_URL: &str = "https://api.deepseek.com/v1/models";
const THAILLM_URL: &str = "http://thaillm.or.th/api/v1/models";
const DEFAULT_TARGET: &str = "crates/core/resources/model_catalogue.json";

// ── Wire types ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct OpenRouterEnvelope {
    data: Vec<OpenRouterModel>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModel {
    id: String,
    #[serde(default)]
    context_length: Option<u32>,
    #[serde(default)]
    top_provider: Option<TopProvider>,
}

#[derive(Debug, Deserialize)]
struct TopProvider {
    #[serde(default)]
    max_completion_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct AnthropicEnvelope {
    data: Vec<AnthropicModel>,
}
#[derive(Debug, Deserialize)]
struct AnthropicModel {
    id: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIEnvelope {
    data: Vec<OpenAIModel>,
}
#[derive(Debug, Deserialize)]
struct OpenAIModel {
    id: String,
}

#[derive(Debug, Deserialize)]
struct GeminiEnvelope {
    models: Vec<GeminiModel>,
}
#[derive(Debug, Deserialize)]
struct GeminiModel {
    name: String,
    #[serde(default, rename = "inputTokenLimit")]
    input_token_limit: Option<u32>,
    #[serde(default, rename = "outputTokenLimit")]
    output_token_limit: Option<u32>,
}

// ── Main ────────────────────────────────────────────────────────────

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(summary) => {
            println!("{summary}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("catalogue-seed: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<String, String> {
    // Pick up API keys from a workspace-root .env, regardless of where
    // `cargo run --bin catalogue-seed` was invoked from. Standard
    // load_dotenv handles ./.env and ~/.config/thclaws/.env; the
    // walking-up pass catches the workspace .env when this binary
    // is run from a nested crate dir (the typical case in the dev
    // workspace where the public-side root Cargo.toml doesn't exist).
    thclaws_core::dotenv::load_dotenv();
    if let Ok(cwd) = std::env::current_dir() {
        thclaws_core::dotenv::load_dotenv_walking_up(&cwd);
    }

    let target: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if Path::new(DEFAULT_TARGET).exists() {
                DEFAULT_TARGET.into()
            } else {
                "resources/model_catalogue.json".into()
            }
        });

    let existing =
        std::fs::read_to_string(&target).map_err(|e| format!("read {}: {e}", target.display()))?;
    let mut cat: Catalogue =
        serde_json::from_str(&existing).map_err(|e| format!("parse {}: {e}", target.display()))?;
    if cat.schema != CURRENT_SCHEMA {
        return Err(format!(
            "target has schema {}, expected {CURRENT_SCHEMA}",
            cat.schema
        ));
    }

    let today = today_iso();
    let mut report = Vec::new();

    // 1. OpenRouter — public, always runs. Also gives us context data
    //    we can reuse when we later discover bare Anthropic/OpenAI ids
    //    (which OpenRouter proxies as `anthropic/<id>` / `openai/<id>`).
    let openrouter_rows = match fetch_openrouter().await {
        Ok(rows) => rows,
        Err(e) => {
            report.push(format!("  openrouter: FAILED ({e})"));
            Vec::new()
        }
    };
    let openrouter_ctx_by_bare: HashMap<String, u32> = openrouter_rows
        .iter()
        .filter_map(|m| {
            let ctx = m.context_length?;
            let bare = m.id.rsplit('/').next().unwrap_or(&m.id).to_string();
            Some((bare, ctx))
        })
        .collect();
    let added_or = merge_openrouter(&mut cat, openrouter_rows, &today);
    push_provider_stats(&mut report, "openrouter", &added_or, None);

    // 2. Anthropic / OpenAI — need API key, gives us canonical dated
    //    ids. Context is not returned, so we pair each id with whatever
    //    OpenRouter reported for the matching `anthropic/<id>` or
    //    `openai/<id>` row; fall back to the provider's default_context.
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        match fetch_anthropic(&key).await {
            Ok(ids) => {
                let added = merge_discovered(
                    &mut cat,
                    "anthropic",
                    ANTHROPIC_URL,
                    ids,
                    &openrouter_ctx_by_bare,
                    &today,
                );
                push_provider_stats(&mut report, "anthropic", &added, None);
            }
            Err(e) => report.push(format!("  anthropic:   FAILED ({e})")),
        }
    } else {
        report.push("  anthropic:   skipped (no ANTHROPIC_API_KEY)".into());
    }

    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        match fetch_openai(&key).await {
            Ok(ids) => {
                let (kept, dropped): (Vec<_>, Vec<_>) =
                    ids.into_iter().partition(|id| is_openai_chat(id));
                let added = merge_discovered(
                    &mut cat,
                    "openai",
                    OPENAI_URL,
                    kept,
                    &openrouter_ctx_by_bare,
                    &today,
                );
                let suffix = format!(
                    "({} filtered: fine-tunes/audio/image/embedding)",
                    dropped.len()
                );
                push_provider_stats(&mut report, "openai", &added, Some(&suffix));
            }
            Err(e) => report.push(format!("  openai:      FAILED ({e})")),
        }
    } else {
        report.push("  openai:      skipped (no OPENAI_API_KEY)".into());
    }

    // 3. Gemini — gives us context directly in the list response.
    if let Ok(key) = std::env::var("GEMINI_API_KEY") {
        match fetch_gemini(&key).await {
            Ok(rows) => {
                let before = rows.len();
                let rows: Vec<_> = rows
                    .into_iter()
                    .filter(|m| {
                        let id = m.name.strip_prefix("models/").unwrap_or(&m.name);
                        is_gemini_chat(id)
                    })
                    .collect();
                let filtered = before - rows.len();
                let added = merge_gemini(&mut cat, rows, &today);
                let suffix = format!("({filtered} filtered: imagen/veo/gemma/embedding/tts)");
                push_provider_stats(&mut report, "gemini", &added, Some(&suffix));
            }
            Err(e) => report.push(format!("  gemini:      FAILED ({e})")),
        }
    } else {
        report.push("  gemini:      skipped (no GEMINI_API_KEY)".into());
    }

    // 4. Ollama Cloud — OpenAI-compatible /v1/models lists the cloud
    //    catalog (NOT the user's local Ollama; that one needs a local
    //    daemon to be running). Each id needs the `ollama-cloud/` prefix
    //    before merging because that's how thClaws routes cloud models
    //    distinctly from local Ollama (which uses `ollama/` prefix).
    if let Ok(key) = std::env::var("OLLAMA_CLOUD_API_KEY") {
        match fetch_ollama_cloud(&key).await {
            Ok(ids) => {
                let prefixed: Vec<String> = ids
                    .into_iter()
                    .map(|id| format!("ollama-cloud/{id}"))
                    .collect();
                // Seed the provider entry with a sensible default context
                // so merge_discovered doesn't skip rows for "no context".
                // 262144 covers most current cloud models; specific rows
                // can be hand-bumped later (e.g. deepseek-v4-flash at 1M).
                let pc = cat
                    .providers
                    .entry("ollama-cloud".into())
                    .or_insert_with(ProviderCatalogue::default);
                if pc.default_context.is_none() {
                    pc.default_context = Some(262144);
                }
                let added = merge_discovered(
                    &mut cat,
                    "ollama-cloud",
                    OLLAMA_CLOUD_URL,
                    prefixed,
                    &openrouter_ctx_by_bare,
                    &today,
                );
                push_provider_stats(&mut report, "ollama-cloud", &added, None);
            }
            Err(e) => report.push(format!("  ollama-cloud: FAILED ({e})")),
        }
    } else {
        report.push("  ollama-cloud: skipped (no OLLAMA_CLOUD_API_KEY)".into());
    }

    // 4b. DeepSeek — OpenAI-compatible `/v1/models` lists their V4 line
    //     (deepseek-v4-flash, deepseek-v4-pro). Bare model ids — no
    //     prefix-namespacing on our side, since `deepseek-` is enough
    //     for ProviderKind::detect. Default context seeded conservatively
    //     at 128K (V4 line ships with a longer window but specific rows
    //     can be hand-bumped after operator review of the diff).
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY") {
        match fetch_deepseek(&key).await {
            Ok(ids) => {
                let pc = cat
                    .providers
                    .entry("deepseek".into())
                    .or_insert_with(ProviderCatalogue::default);
                if pc.default_context.is_none() {
                    pc.default_context = Some(131072);
                }
                let added = merge_discovered(
                    &mut cat,
                    "deepseek",
                    DEEPSEEK_URL,
                    ids,
                    &openrouter_ctx_by_bare,
                    &today,
                );
                push_provider_stats(&mut report, "deepseek", &added, None);
            }
            Err(e) => report.push(format!("  deepseek:    FAILED ({e})")),
        }
    } else {
        report.push("  deepseek:    skipped (no DEEPSEEK_API_KEY)".into());
    }

    // 4c. ThaiLLM — NSTDA / สวทช aggregator at thaillm.or.th. OpenAI-
    //     compatible /v1/models lists OpenThaiGPT, Typhoon-S, Pathumma,
    //     THaLLE — all 8B Thai-tuned models on Llama-3.1-8B / Qwen3-8B
    //     bases (native 128K context). Each id is namespaced with the
    //     `thaillm/` prefix so ProviderKind::detect routes correctly,
    //     mirroring the ollama-cloud pattern.
    if let Ok(key) = std::env::var("THAILLM_API_KEY") {
        match fetch_thaillm(&key).await {
            Ok(ids) => {
                let prefixed: Vec<String> =
                    ids.into_iter().map(|id| format!("thaillm/{id}")).collect();
                let pc = cat
                    .providers
                    .entry("thaillm".into())
                    .or_insert_with(ProviderCatalogue::default);
                if pc.default_context.is_none() {
                    pc.default_context = Some(131072);
                }
                let added = merge_discovered(
                    &mut cat,
                    "thaillm",
                    THAILLM_URL,
                    prefixed,
                    &openrouter_ctx_by_bare,
                    &today,
                );
                push_provider_stats(&mut report, "thaillm", &added, None);
            }
            Err(e) => report.push(format!("  thaillm:     FAILED ({e})")),
        }
    } else {
        report.push("  thaillm:     skipped (no THAILLM_API_KEY)".into());
    }

    // 5. Derive agent-sdk rows from anthropic. The Claude CLI subprocess
    //    (ProviderKind::AgentSdk) accepts any claude-* model id; thClaws
    //    routes it as `agent/<id>`. So for every claude-* row in the
    //    anthropic catalogue, mirror an `agent/<id>` row into agent-sdk
    //    with the same context. Skips ids already present so hand-curated
    //    overrides win on metadata. Closes the lag pattern from
    //    thclaws/thclaws#26 — Anthropic ships a new variant, native picks
    //    it up via /v1/models, this step propagates it to agent-sdk.
    let claude_ids: Vec<(String, Option<u32>)> = cat
        .providers
        .get("anthropic")
        .map(|p| {
            p.models
                .iter()
                .map(|(id, e)| (id.clone(), e.context))
                .collect()
        })
        .unwrap_or_default();
    if !claude_ids.is_empty() {
        let agent_pc = cat
            .providers
            .entry("agent-sdk".into())
            .or_insert_with(ProviderCatalogue::default);
        if agent_pc.default_context.is_none() {
            agent_pc.default_context = Some(200000);
        }
        let mut stats = MergeStats::default();
        for (claude_id, ctx) in claude_ids {
            let agent_id = format!("agent/{claude_id}");
            if agent_pc.models.contains_key(&agent_id) {
                stats.unchanged += 1;
                continue;
            }
            agent_pc.models.insert(
                agent_id.clone(),
                ModelEntry {
                    context: ctx,
                    max_output: None,
                    source: Some(format!("derived:{claude_id}")),
                    verified_at: Some(today.clone()),
                },
            );
            stats.added.push(agent_id);
        }
        push_provider_stats(
            &mut report,
            "agent-sdk",
            &stats,
            Some("(derived from anthropic)"),
        );
    }

    cat.source = format!("baseline {today}");
    cat.fetched_at = format!("{today}T00:00:00Z");

    let out = serde_json::to_string_pretty(&cat).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(&target, out).map_err(|e| format!("write {}: {e}", target.display()))?;

    let total: usize = cat.providers.values().map(|p| p.models.len()).sum();
    Ok(format!(
        "wrote {} ({total} total rows):\n{}",
        target.display(),
        report.join("\n")
    ))
}

// ── Merge helpers ───────────────────────────────────────────────────

/// Per-provider seed result. Captures everything the operator might want
/// to see in the report: which ids were inserted, which were already
/// present (so they don't wonder "did the seed see it?"), and which the
/// seed had to drop for lack of usable metadata.
#[derive(Default)]
pub struct MergeStats {
    pub added: Vec<String>,
    pub unchanged: usize,
    pub skipped_no_context: usize,
}

/// Format the per-provider report lines. Header always shows added +
/// unchanged counts; appends "skipped (no context)" only when nonzero so
/// the common case stays terse. Each new id is listed on its own bullet
/// (capped at MAX_LIST_IDS to keep an unusually large refresh from
/// dumping hundreds of lines). `suffix` carries provider-specific extras
/// (e.g. OpenAI's "X filtered: fine-tunes/audio/image/embedding").
const MAX_LIST_IDS: usize = 30;

fn push_provider_stats(
    report: &mut Vec<String>,
    provider: &str,
    stats: &MergeStats,
    suffix: Option<&str>,
) {
    let count = stats.added.len();
    let label = format!("{provider}:");
    let mut header = format!("  {label:12} +{count} new, {} unchanged", stats.unchanged);
    if stats.skipped_no_context > 0 {
        header.push_str(&format!(
            ", {} skipped (no context)",
            stats.skipped_no_context
        ));
    }
    if let Some(s) = suffix {
        header.push(' ');
        header.push_str(s);
    }
    report.push(header);
    if count == 0 {
        return;
    }
    let mut sorted = stats.added.clone();
    sorted.sort();
    let shown = sorted.iter().take(MAX_LIST_IDS);
    for id in shown {
        report.push(format!("                 · {id}"));
    }
    if count > MAX_LIST_IDS {
        report.push(format!(
            "                 … (+{} more)",
            count - MAX_LIST_IDS
        ));
    }
}

fn merge_openrouter(cat: &mut Catalogue, rows: Vec<OpenRouterModel>, today: &str) -> MergeStats {
    let pc = cat
        .providers
        .entry("openrouter".into())
        .or_insert_with(|| ProviderCatalogue {
            list_url: Some(OPENROUTER_URL.into()),
            default_context: Some(128_000),
            models: HashMap::new(),
        });
    let mut stats = MergeStats::default();
    for m in rows {
        let Some(ctx) = m.context_length else {
            stats.skipped_no_context += 1;
            continue;
        };
        if pc.models.contains_key(&m.id) {
            stats.unchanged += 1;
            continue;
        }
        pc.models.insert(
            m.id.clone(),
            ModelEntry {
                context: Some(ctx),
                max_output: m
                    .top_provider
                    .as_ref()
                    .and_then(|p| p.max_completion_tokens),
                source: Some(OPENROUTER_URL.into()),
                verified_at: Some(today.into()),
            },
        );
        stats.added.push(m.id);
    }
    stats
}

/// Ids came from the provider's `/v1/models` (so they're real). Context
/// is not in that response, so we look up each id's bare form in the
/// `openrouter_ctx_by_bare` map (OpenRouter usually proxies the same
/// model and publishes its context). When OpenRouter doesn't know
/// either, we still insert the id with the provider's default context
/// so the user can at least pick it — the `source` flag says it's
/// unverified context.
fn merge_discovered(
    cat: &mut Catalogue,
    provider: &str,
    list_url: &str,
    ids: Vec<String>,
    openrouter_ctx_by_bare: &HashMap<String, u32>,
    today: &str,
) -> MergeStats {
    let pc = cat
        .providers
        .entry(provider.into())
        .or_insert_with(ProviderCatalogue::default);
    if pc.list_url.is_none() {
        pc.list_url = Some(list_url.into());
    }
    let default_ctx = pc.default_context;
    let mut stats = MergeStats::default();
    for id in ids {
        if pc.models.contains_key(&id) {
            stats.unchanged += 1;
            continue;
        }
        let (ctx, source) = match openrouter_ctx_by_bare.get(&id).copied() {
            Some(n) => (n, format!("{OPENROUTER_URL} via bare id")),
            None => match default_ctx {
                Some(n) => (n, format!("{list_url} (context unverified)")),
                None => {
                    stats.skipped_no_context += 1;
                    continue;
                }
            },
        };
        pc.models.insert(
            id.clone(),
            ModelEntry {
                context: Some(ctx),
                max_output: None,
                source: Some(source),
                verified_at: Some(today.into()),
            },
        );
        stats.added.push(id);
    }
    stats
}

fn merge_gemini(cat: &mut Catalogue, rows: Vec<GeminiModel>, today: &str) -> MergeStats {
    let pc = cat
        .providers
        .entry("gemini".into())
        .or_insert_with(|| ProviderCatalogue {
            list_url: Some(GEMINI_URL.into()),
            default_context: Some(1_000_000),
            models: HashMap::new(),
        });
    let mut stats = MergeStats::default();
    for m in rows {
        // Gemini returns ids like `models/gemini-1.5-pro` — strip the
        // leading `models/` to match the rest of the codebase.
        let id = m
            .name
            .strip_prefix("models/")
            .unwrap_or(&m.name)
            .to_string();
        let Some(ctx) = m.input_token_limit else {
            stats.skipped_no_context += 1;
            continue;
        };
        if pc.models.contains_key(&id) {
            stats.unchanged += 1;
            continue;
        }
        pc.models.insert(
            id.clone(),
            ModelEntry {
                context: Some(ctx),
                max_output: m.output_token_limit,
                source: Some(GEMINI_URL.into()),
                verified_at: Some(today.into()),
            },
        );
        stats.added.push(id);
    }
    stats
}

// ── HTTP ────────────────────────────────────────────────────────────

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))
}

async fn fetch_openrouter() -> Result<Vec<OpenRouterModel>, String> {
    let resp = client()?
        .get(OPENROUTER_URL)
        .send()
        .await
        .map_err(|e| format!("GET {OPENROUTER_URL}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("openrouter HTTP {}", resp.status()));
    }
    let env: OpenRouterEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.data)
}

async fn fetch_anthropic(key: &str) -> Result<Vec<String>, String> {
    let resp = client()?
        .get(ANTHROPIC_URL)
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .map_err(|e| format!("GET {ANTHROPIC_URL}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("anthropic HTTP {}", resp.status()));
    }
    let env: AnthropicEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.data.into_iter().map(|m| m.id).collect())
}

async fn fetch_openai(key: &str) -> Result<Vec<String>, String> {
    let resp = client()?
        .get(OPENAI_URL)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("GET {OPENAI_URL}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("openai HTTP {}", resp.status()));
    }
    let env: OpenAIEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.data.into_iter().map(|m| m.id).collect())
}

/// Fetch DeepSeek's `/v1/models` (OpenAI-compatible). At the time of
/// writing this returns just the V4 line (`deepseek-v4-flash`,
/// `deepseek-v4-pro`); older aliases like `deepseek-chat` and
/// `deepseek-reasoner` still work on the chat completions endpoint as
/// wire-level aliases but aren't listed by `/v1/models`, so they don't
/// land in the catalogue automatically. Operators can hand-add them.
async fn fetch_deepseek(key: &str) -> Result<Vec<String>, String> {
    let resp = client()?
        .get(DEEPSEEK_URL)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("GET {DEEPSEEK_URL}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("deepseek HTTP {}", resp.status()));
    }
    let env: OpenAIEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.data.into_iter().map(|m| m.id).collect())
}

/// Fetch the model list from NSTDA's Thai LLM aggregator. The endpoint
/// is OpenAI-compatible — `/v1/models` returns `{data:[{id, object,
/// owned_by}]}` for each Thai model hosted (OpenThaiGPT, Typhoon-S,
/// Pathumma, THaLLE, etc.). Returns bare ids; caller adds the
/// `thaillm/` prefix to namespace them.
async fn fetch_thaillm(key: &str) -> Result<Vec<String>, String> {
    let resp = client()?
        .get(THAILLM_URL)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("GET {THAILLM_URL}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("thaillm HTTP {}", resp.status()));
    }
    let env: OpenAIEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.data.into_iter().map(|m| m.id).collect())
}

/// Fetch the cloud catalog from Ollama Cloud's OpenAI-compatible
/// `/v1/models` endpoint. Returns bare model ids (e.g. `kimi-k2.5`,
/// `gpt-oss:120b`) — caller adds the `ollama-cloud/` prefix to namespace
/// them in the catalogue. The same key works against `/api/tags` for
/// richer metadata (size, modified_at) but we don't currently consume
/// those fields, and the OpenAI-compatible shape lets us reuse
/// OpenAIEnvelope without a new struct.
async fn fetch_ollama_cloud(key: &str) -> Result<Vec<String>, String> {
    let resp = client()?
        .get(OLLAMA_CLOUD_URL)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("GET {OLLAMA_CLOUD_URL}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("ollama-cloud HTTP {}", resp.status()));
    }
    let env: OpenAIEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.data.into_iter().map(|m| m.id).collect())
}

async fn fetch_gemini(key: &str) -> Result<Vec<GeminiModel>, String> {
    let url = format!("{GEMINI_URL}?key={key}");
    let resp = client()?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("GET gemini: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("gemini HTTP {}", resp.status()));
    }
    let env: GeminiEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.models)
}

// ── Filters ─────────────────────────────────────────────────────────
//
// Provider `/v1/models` endpoints dump everything they serve: image
// gen, audio, embeddings, fine-tunes. For a chat/reasoning catalogue
// we only want text-in/text-out models. Filters are conservative —
// prefix allowlist + substring denylist — and easy to audit.

fn is_openai_chat(id: &str) -> bool {
    // User-specific fine-tunes look like `ft:base:org::suffix` —
    // never belong in a shipped baseline.
    if id.starts_with("ft:") {
        return false;
    }
    // Allowlist: only keep ids from known chat / reasoning families.
    let ok_prefix = ["gpt-", "o1", "o3", "o4", "o5", "chatgpt-"]
        .iter()
        .any(|p| id.starts_with(p));
    if !ok_prefix {
        return false;
    }
    // Denylist: modality-specific variants within those families.
    let skip = [
        "image",
        "-transcribe",
        "-realtime",
        "-audio",
        "-tts",
        "-search-preview",
    ];
    !skip.iter().any(|s| id.contains(s))
}

fn is_gemini_chat(id: &str) -> bool {
    // Google's catalogue includes imagen/veo/lyria/gemma/robotics/
    // embeddings/TTS alongside chat. Allow only `gemini-*`, then
    // deny modality-specific members of that family.
    if !id.starts_with("gemini-") {
        return false;
    }
    let skip = [
        "embedding",
        "-tts",
        "robotics",
        "-image",
        "-audio",
        "computer-use",
    ];
    if skip.iter().any(|s| id.contains(s)) {
        return false;
    }
    // Drop Gemini IDs Google has deprecated or shut down. Google
    // sometimes keeps them in the public list for "existing customer"
    // backward compat even after new-customer 404s start, which leads
    // to misleading entries in our catalogue (issue #32: user calls
    // gemini-2.0-flash → 404). Source of truth:
    //
    //     https://ai.google.dev/gemini-api/docs/deprecations
    //
    // Update this list when Google retires more IDs. Track the next
    // upcoming shutdown via the official deprecations page; the 2.5
    // family is on the clock for 2026-06-17.
    if is_retired_gemini(id) {
        return false;
    }
    true
}

/// Hard-list of Gemini model IDs we refuse to import even when the
/// upstream `/v1beta/models` endpoint still returns them. Sources:
/// <https://ai.google.dev/gemini-api/docs/deprecations>.
fn is_retired_gemini(id: &str) -> bool {
    // 1.x family — fully shut down (2025).
    if id.starts_with("gemini-1.") || id == "gemini-pro" || id == "gemini-pro-vision" {
        return true;
    }
    // 2.0 family — existing-customer-only since 2026-03-06; hard
    // shutdown 2026-06-01. Already 404s for new API keys, which is
    // exactly issue #32's symptom.
    if id.starts_with("gemini-2.0-flash") {
        return true;
    }
    // 3-pro-preview — already shut down 2026-03-09 (replaced by
    // gemini-3.1-pro-preview).
    if id == "gemini-3-pro-preview" {
        return true;
    }
    false
}

// ── Date stamp ──────────────────────────────────────────────────────

fn today_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y } as i32;
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(20_567), (2026, 4, 24));
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn openai_filter_keeps_chat_drops_noise() {
        // Keep chat / reasoning.
        assert!(is_openai_chat("gpt-4o"));
        assert!(is_openai_chat("gpt-4o-mini"));
        assert!(is_openai_chat("gpt-4.1-2025-04-14"));
        assert!(is_openai_chat("o3"));
        assert!(is_openai_chat("o3-mini"));
        assert!(is_openai_chat("o4-mini"));
        assert!(is_openai_chat("chatgpt-4o-latest"));
        // Drop user fine-tunes and non-chat families.
        assert!(!is_openai_chat("ft:gpt-3.5-turbo-0613:org::abc"));
        assert!(!is_openai_chat("dall-e-3"));
        assert!(!is_openai_chat("davinci-002"));
        assert!(!is_openai_chat("babbage-002"));
        assert!(!is_openai_chat("whisper-1"));
        assert!(!is_openai_chat("tts-1"));
        assert!(!is_openai_chat("text-embedding-3-small"));
        assert!(!is_openai_chat("computer-use-preview"));
        // Drop audio / image / realtime variants of chat families.
        assert!(!is_openai_chat("gpt-image-1"));
        assert!(!is_openai_chat("chatgpt-image-latest"));
        assert!(!is_openai_chat("gpt-4o-audio-preview"));
        assert!(!is_openai_chat("gpt-4o-realtime-preview"));
        assert!(!is_openai_chat("gpt-4o-transcribe"));
        assert!(!is_openai_chat("gpt-4o-search-preview"));
        assert!(!is_openai_chat("gpt-4o-mini-tts"));
    }

    #[test]
    fn gemini_filter_keeps_chat_drops_noise() {
        // Currently-shipping chat IDs.
        assert!(is_gemini_chat("gemini-2.5-pro"));
        assert!(is_gemini_chat("gemini-2.5-flash"));
        assert!(is_gemini_chat("gemini-3.1-pro-preview"));
        assert!(is_gemini_chat("gemini-3-flash-preview"));
        assert!(is_gemini_chat("gemini-flash-latest"));
        // Non-gemini families dropped outright.
        assert!(!is_gemini_chat("imagen-4.0-generate-001"));
        assert!(!is_gemini_chat("veo-3.0-generate-001"));
        assert!(!is_gemini_chat("lyria-3-pro-preview"));
        assert!(!is_gemini_chat("gemma-3-27b-it"));
        assert!(!is_gemini_chat("aqa"));
        assert!(!is_gemini_chat("nano-banana-pro-preview"));
        // Gemini-prefixed but modality-specific → dropped.
        assert!(!is_gemini_chat("gemini-embedding-001"));
        assert!(!is_gemini_chat("gemini-2.5-flash-image"));
        assert!(!is_gemini_chat("gemini-3-pro-image-preview"));
        assert!(!is_gemini_chat("gemini-2.5-flash-preview-tts"));
        assert!(!is_gemini_chat("gemini-robotics-er-1.5-preview"));
        assert!(!is_gemini_chat("gemini-2.5-computer-use-preview-10-2025"));
    }

    #[test]
    fn gemini_filter_drops_retired_models() {
        // Issue #32: keep retired Gemini IDs out of the catalogue so
        // `make catalogue` runs against a still-listing upstream don't
        // re-add them. Source: ai.google.dev/gemini-api/docs/deprecations.
        // 1.x family — fully shut down 2025.
        assert!(is_retired_gemini("gemini-1.5-flash"));
        assert!(is_retired_gemini("gemini-1.5-pro"));
        assert!(is_retired_gemini("gemini-1.0-pro"));
        assert!(is_retired_gemini("gemini-pro"));
        assert!(is_retired_gemini("gemini-pro-vision"));
        // 2.0 family — existing-customer-only 2026-03-06; full shutdown
        // 2026-06-01. Issue #32's specific symptom (404 for new keys).
        assert!(is_retired_gemini("gemini-2.0-flash"));
        assert!(is_retired_gemini("gemini-2.0-flash-001"));
        assert!(is_retired_gemini("gemini-2.0-flash-lite"));
        assert!(is_retired_gemini("gemini-2.0-flash-lite-001"));
        // 3-pro-preview — already shut down 2026-03-09.
        assert!(is_retired_gemini("gemini-3-pro-preview"));
        // Currently-shipping IDs must not match the retirement filter.
        assert!(!is_retired_gemini("gemini-2.5-flash"));
        assert!(!is_retired_gemini("gemini-2.5-pro"));
        assert!(!is_retired_gemini("gemini-3.1-pro-preview"));
        assert!(!is_retired_gemini("gemini-3-flash-preview"));
        assert!(!is_retired_gemini("gemini-flash-latest"));
        // is_gemini_chat composes both filters — retired IDs drop out.
        assert!(!is_gemini_chat("gemini-2.0-flash"));
        assert!(!is_gemini_chat("gemini-3-pro-preview"));
        assert!(!is_gemini_chat("gemini-1.5-flash"));
    }
}
