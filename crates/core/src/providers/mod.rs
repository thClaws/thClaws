//! Provider abstraction — streaming interface over one LLM backend.
//!
//! Wire formats (Anthropic, OpenAI, etc.) are adapted to a common
//! [`ProviderEvent`] stream. Higher layers consume only the stream.

use crate::error::Result;
use crate::types::{Message, ToolDef};
use async_trait::async_trait;
use futures::stream::BoxStream;

/// Idle timeout applied to each individual chunk in a streaming response.
/// If the provider sends no bytes for this many seconds the stream is
/// aborted with an error so the UI surfaces a "try again" message instead
/// of hanging silently until the user force-quits.
///
/// Stored as `AtomicU64` (seconds) so `AppConfig::load` callers can update
/// it live without rebuilding providers. The original PR #81 / #83 constant
/// was 30 s — too tight for `/research` and long-reasoning workloads where
/// the model can legitimately pause mid-stream. Default now 120 s; users
/// can override via `stream_chunk_timeout_secs` in `.thclaws/settings.json`
/// (or the user-scope settings).
static STREAM_CHUNK_TIMEOUT_SECS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(120);

/// Read the current idle timeout. Used by every streaming provider's
/// `byte_stream.next()` await — `tokio::time::timeout(stream_chunk_timeout(), ...)`.
pub(super) fn stream_chunk_timeout() -> std::time::Duration {
    let secs = STREAM_CHUNK_TIMEOUT_SECS.load(std::sync::atomic::Ordering::Relaxed);
    // Floor at 1 s to avoid `Duration::from_secs(0)` which would make
    // every chunk read instantly time out. Treat 0 as "default" since
    // `serde(default)` falls back to the same value anyway.
    std::time::Duration::from_secs(if secs == 0 { 120 } else { secs })
}

/// Push a new idle timeout from config. Called from worker init (CLI /
/// GUI / serve) after `AppConfig::load`. Live — affects in-flight provider
/// calls' NEXT chunk-await; the current `tokio::time::timeout` future
/// keeps its original deadline (acceptable — the user only notices on the
/// next idle anyway). Idempotent + thread-safe (atomic store).
pub fn set_stream_chunk_timeout_secs(secs: u64) {
    STREAM_CHUNK_TIMEOUT_SECS.store(secs, std::sync::atomic::Ordering::Relaxed);
}

pub mod agent_sdk;
pub mod anthropic;
pub mod assemble;
pub mod gateway;
pub mod gemini;
pub mod ollama;
pub mod ollama_cloud;
pub mod openai;
pub mod openai_responses;
pub mod opencode_go;
pub mod thclaws_gateway;

/// Registry of supported providers. Every new provider needs exactly one
/// variant here + matching arms in the methods below; the compiler catches
/// any omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    Anthropic,
    AtlasCloud,
    Unifically,
    MetaAi,
    NineRouter,
    AgentSdk,
    OpenAI,
    OpenAIResponses,
    /// ChatGPT-subscription Codex auth path. Same Responses-API wire
    /// shape as [`OpenAIResponses`] but targets `chatgpt.com/backend-api/codex`
    /// with a Bearer access_token (from [`crate::codex_auth`]) plus the
    /// `chatgpt-account-id` / `originator` / `OpenAI-Beta` headers. Auth
    /// is read from `~/.config/thclaws/auth/<profile>.json` (auto-imported
    /// from `~/.codex/auth.json` if absent).
    ChatGptCodex,
    OpenRouter,
    /// TokenRouter (tokenrouter.com) — OpenAI-compatible unified gateway
    /// to 300+ models. Same wire shape as [`OpenRouter`]; models route
    /// via the `tokenrouter/<vendor>/<model>` prefix (stripped before the
    /// upstream request). Key `TOKENROUTER_API_KEY`, base overridable via
    /// `TOKENROUTER_BASE_URL`.
    TokenRouter,
    Gemini,
    Ollama,
    OllamaAnthropic,
    OllamaCloud,
    DashScope,
    /// Alibaba Cloud's Singapore-region DashScope endpoint
    /// (`dashscope-intl.aliyuncs.com`). Same wire protocol as
    /// `DashScope` but a different account / region / key, so it
    /// gets its own variant and `qwen-cloud/` model namespace.
    QwenCloud,
    /// Alibaba Cloud Model Studio "SIS" workspace endpoints
    /// (`ws-<id>.<region>.maas.aliyuncs.com/compatible-mode/v1`).
    /// Same OpenAI-compatible wire as [`DashScope`], so it follows
    /// [`QwenCloud`]'s shape: own variant, own key, own `sis/` model
    /// namespace, Additional tier, BYOK only (the metered gateway sells
    /// the ten Featured providers and dropped the regional Alibaba
    /// siblings on 2026-08-10).
    ///
    /// Unlike every other hosted provider it has NO default endpoint:
    /// the host carries a workspace id, so it differs per account.
    /// `SIS_BASE_URL` is required and `build_provider` refuses without
    /// it — a shared default would silently route one account's traffic
    /// into another's workspace.
    Sis,
    ZAi,
    LMStudio,
    /// vLLM (`vllm serve`) — the standard self-hosted inference server for
    /// GPU boxes. OpenAI-compatible at `/v1`, default port 8000, no auth
    /// unless started with `--api-key`. Split out of [`OpenAICompat`] so it
    /// gets its own `vllm/` namespace, base-URL env, and local-provider
    /// treatment (gateway bypass, live model list) instead of sharing the
    /// generic compat slot. The served id must match what `vllm serve` was
    /// pointed at, so the default model is a placeholder.
    VLlm,
    /// llama.cpp's `llama-server` — OpenAI-compatible at `/v1`, default port
    /// 8080, no auth. Serves one GGUF at a time and ignores the `model`
    /// field in the request, so any id routes correctly.
    LlamaCpp,
    /// LiteLLM Proxy (`litellm --config config.yaml`) — the self-hosted
    /// OpenAI-compatible router, default port 4000. Split out of
    /// [`OpenAICompat`] so it gets its own `litellm/` namespace, base-URL
    /// env and Settings row. Model ids are whatever `model_name` aliases
    /// the operator declared in `model_list`, so no default can be right —
    /// `list_models` fills the picker from the live `/models`, and
    /// `/model/info` supplies the real context window.
    ///
    /// Auth is optional: a proxy started without `master_key` accepts any
    /// bearer, one with virtual keys wants `LITELLM_API_KEY`. Unlike vLLM /
    /// llama.cpp this is a *router*, not an inference server — the model it
    /// resolves to usually lives at OpenAI/Anthropic/etc., so it is
    /// deliberately absent from [`Self::is_local`] and the org-policy
    /// gateway's local bypass.
    LiteLlm,
    AzureAIFoundry,
    OpenAICompat,
    DeepSeek,
    ThaiLLM,
    Nvidia,
    Minimax,
    OpenCodeGo,
    /// Moonshot AI (moonshot.ai) — the Kimi family. OpenAI-compatible
    /// `/chat/completions` at `api.moonshot.ai/v1` (override to the
    /// mainland `api.moonshot.cn/v1` via `MOONSHOT_BASE_URL`). Models
    /// route via the `moonshot/<id>` prefix (e.g. `moonshot/kimi-k2.6`),
    /// stripped before the upstream request. Key `MOONSHOT_API_KEY`.
    Moonshot,
    /// xAI (x.ai) — the Grok family. OpenAI-compatible `/chat/completions`
    /// at `api.x.ai/v1` (override via `XAI_BASE_URL`). Models route via
    /// the `xai/<id>` prefix (e.g. `xai/grok-4.3`, stripped before the
    /// upstream request); bare `grok-*` ids also route here. Key
    /// `XAI_API_KEY`.
    XAi,
    /// Groq (groq.com) — LPU-hosted open models (Llama, GPT-OSS, Qwen,
    /// Kimi). OpenAI-compatible `/chat/completions` at
    /// `api.groq.com/openai/v1` (override via `GROQ_BASE_URL`). Models
    /// route via the `groq/<id>` prefix (e.g.
    /// `groq/llama-3.3-70b-versatile`), stripped before the upstream
    /// request. Key `GROQ_API_KEY` (shared with the whisper
    /// transcription path in `tools/watch_video.rs`).
    Groq,
}

/// Two-tier provider classification.
///
/// **Featured** providers are the curated set thClaws promotes: they are
/// (or will be) routable through the thClaws cloud gateway, their pricing
/// is verified against official vendor sources, and they are listed before
/// Additional providers in every model picker. **Additional** providers
/// still work (BYOK / local), they're just the long tail shown afterwards.
///
/// NOTE (gateway alignment, deferred): the gateway-routable set in
/// [`thclaws_gateway::provider_segment`] does not yet match Featured 1:1 —
/// `xai`/`moonshot` need server-side gateway routes added, and
/// `qwen-cloud`/`thaillm` are routable today but Additional. Those moves
/// ship with a later gateway deploy; the tier here is the source of truth
/// for "primary".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTier {
    Featured,
    Additional,
}

impl ProviderTier {
    /// Lowercase wire/display key (used in the model-list payload so the
    /// frontend can group + label sections).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Featured => "featured",
            Self::Additional => "additional",
        }
    }
}

/// Human-readable "where this provider actually points" — the env var
/// the user set, resolved to its current value. Used when a listing
/// comes back empty so the message names the endpoint that was asked
/// rather than a provider label the user never typed.
pub fn endpoint_hint(kind: ProviderKind) -> String {
    let (var, default) = match kind {
        ProviderKind::OpenAICompat => ("OPENAI_COMPAT_BASE_URL", "http://localhost:8000/v1"),
        ProviderKind::LiteLlm => ("LITELLM_BASE_URL", "http://localhost:4000/v1"),
        ProviderKind::Ollama | ProviderKind::OllamaAnthropic => {
            ("OLLAMA_BASE_URL", "http://localhost:11434")
        }
        _ => return crate::model_catalogue::provider_kind_name(kind).to_string(),
    };
    let base = std::env::var(var)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| default.to_string());
    format!("{base} ({var})")
}

impl ProviderKind {
    /// Curated display order for the Featured tier in model pickers — the
    /// priority order the product promotes. Additional providers follow,
    /// in `ALL` order. Must contain exactly the Featured providers (a test
    /// enforces this against `tier()`).
    pub const FEATURED_ORDER: &'static [Self] = &[
        Self::OpenAI,
        Self::Anthropic,
        Self::Gemini,
        Self::XAi,
        Self::DeepSeek,
        Self::DashScope,
        Self::Moonshot,
        Self::ZAi,
        Self::Minimax,
        Self::OpenRouter,
    ];

    /// Featured (primary) vs Additional (secondary) classification.
    /// The 10 Featured providers map to the standard provider kinds only —
    /// auth/protocol variants (OpenAI-Responses, ChatGPT-Codex, Agent-SDK)
    /// and regional siblings (QwenCloud) stay Additional.
    pub fn tier(&self) -> ProviderTier {
        match self {
            Self::OpenAI
            | Self::Anthropic
            | Self::Gemini
            | Self::XAi
            | Self::DeepSeek
            | Self::DashScope
            | Self::Moonshot
            | Self::ZAi
            | Self::Minimax
            | Self::OpenRouter => ProviderTier::Featured,
            _ => ProviderTier::Additional,
        }
    }

    /// Providers in display order for the `/providers` list and model
    /// pickers: Featured first (in FEATURED_ORDER), then Additional in
    /// ALL order. Iterating this and emitting a header when `tier()`
    /// changes yields the two grouped sections.
    pub fn display_ordered() -> Vec<Self> {
        let mut out: Vec<Self> = Self::FEATURED_ORDER.to_vec();
        out.extend(
            Self::ALL
                .iter()
                .copied()
                .filter(|k| k.tier() == ProviderTier::Additional),
        );
        out
    }
}

impl ProviderKind {
    pub const ALL: &'static [Self] = &[
        Self::Anthropic,
        Self::AtlasCloud,
        Self::Unifically,
        Self::MetaAi,
        Self::NineRouter,
        Self::AgentSdk,
        Self::OpenAI,
        Self::OpenAIResponses,
        Self::ChatGptCodex,
        Self::OpenRouter,
        Self::TokenRouter,
        Self::Gemini,
        Self::Ollama,
        Self::OllamaAnthropic,
        Self::OllamaCloud,
        Self::DashScope,
        Self::QwenCloud,
        Self::Sis,
        Self::ZAi,
        Self::LMStudio,
        Self::VLlm,
        Self::LlamaCpp,
        Self::LiteLlm,
        Self::AzureAIFoundry,
        Self::OpenAICompat,
        Self::DeepSeek,
        Self::ThaiLLM,
        Self::Nvidia,
        Self::Minimax,
        Self::OpenCodeGo,
        Self::Moonshot,
        Self::XAi,
        Self::Groq,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::AtlasCloud => "atlascloud",
            Self::Unifically => "unifically",
            Self::MetaAi => "meta",
            Self::NineRouter => "9router",
            Self::AgentSdk => "anthropic-agent",
            Self::OpenAI => "openai",
            Self::OpenAIResponses => "openai-responses",
            Self::ChatGptCodex => "chatgpt-codex",
            Self::OpenRouter => "openrouter",
            Self::TokenRouter => "tokenrouter",
            Self::Gemini => "gemini",
            Self::Ollama => "ollama",
            Self::OllamaAnthropic => "ollama-anthropic",
            Self::OllamaCloud => "ollama-cloud",
            Self::DashScope => "dashscope",
            Self::QwenCloud => "qwen-cloud",
            Self::Sis => "sis",
            Self::ZAi => "zai",
            Self::LMStudio => "lmstudio",
            Self::VLlm => "vllm",
            Self::LlamaCpp => "llamacpp",
            Self::LiteLlm => "litellm",
            Self::AzureAIFoundry => "azure",
            Self::OpenAICompat => "openai-compat",
            Self::DeepSeek => "deepseek",
            Self::ThaiLLM => "thaillm",
            Self::Nvidia => "nvidia",
            Self::Minimax => "minimax",
            Self::OpenCodeGo => "opencode-go",
            Self::Moonshot => "moonshot",
            Self::XAi => "xai",
            Self::Groq => "groq",
        }
    }

    pub fn default_model(&self) -> &'static str {
        match self {
            Self::Anthropic => "claude-sonnet-4-6",
            Self::AtlasCloud => "atlascloud/qwen/qwen3.5-flash",
            Self::Unifically => "unifically/google/gemini-3.5-flash",
            // muse-spark reasons before it answers, so it needs a large
            // output budget — see the catalogue's max_output note.
            Self::MetaAi => "meta/muse-spark-1.2",
            // 9router routes by `<alias>/<model>`; `anthropic` is a standard
            // registry alias. Users normally pick from the live /models list.
            Self::NineRouter => "9router/anthropic/claude-sonnet-4.5",
            Self::AgentSdk => "agent/claude-sonnet-4-6",
            Self::OpenAI => "gpt-4.1",
            // gpt-5.2-codex was retired by OpenAI (404 on both
            // /v1/chat/completions and /v1/responses); gpt-5.3-codex is the
            // live successor. A dead default means every user who picks this
            // provider without naming a model gets a 404 on their first turn.
            Self::OpenAIResponses => "codex/gpt-5.3-codex",
            Self::ChatGptCodex => "chatgpt-codex/gpt-5.4",
            Self::OpenRouter => "openrouter/qwen/qwen3.7-plus",
            Self::TokenRouter => "tokenrouter/anthropic/claude-sonnet-4.5",
            // Pinned to a versioned ID (matching Anthropic / OpenAI
            // convention) rather than `gemini-flash-latest` — `-latest`
            // is a rolling Google-side alias that could promote into a
            // higher-tier model without warning, surprising users with
            // unexpected cost. Track upcoming retirement at:
            // https://ai.google.dev/gemini-api/docs/deprecations
            // Bumped to gemini-3.5-flash ahead of the 2026-06-17
            // gemini-2.5-flash shutdown; track the next retirement at the
            // deprecations page above.
            Self::Gemini => "gemini-3.5-flash",
            Self::Ollama => "ollama/llama3.2",
            Self::OllamaAnthropic => "oa/qwen3-coder",
            Self::OllamaCloud => "ollama-cloud/deepseek-v4-flash",
            Self::DashScope => "dashscope/qwen3.7-max",
            // Alibaba Singapore DashScope (`dashscope-intl.aliyuncs.com`).
            // Same OpenAI-compat wire protocol as DashScope, but a
            // separate region/account, so models route via the short
            // `qc/` prefix. Prefix is stripped before the request
            // reaches the upstream (which expects bare `qwen-max`,
            // `qwen-plus`, etc.).
            Self::QwenCloud => "qc/qwen-max",
            // Bootstrap only — enough to establish a connection so the
            // user can `/model sis/<id>` from the live roster. A SIS
            // workspace serves whatever its owner enabled, so no id is
            // guaranteed present; this one was verified against a real
            // workspace on 2026-09-10.
            Self::Sis => "sis/qwen3.8-flash",
            Self::ZAi => "zai/glm-5.2",
            // Most LMStudio installs change models constantly; this is a
            // placeholder that lets the connection establish so the user
            // can `/model lmstudio/<loaded-model>` to switch. list_models
            // will populate the GUI dropdown with whatever's actually
            // loaded.
            Self::LMStudio => "lmstudio/llama-3.2-3b-instruct",
            // vLLM serves exactly the model it was launched with, and the id
            // is whatever was passed to `vllm serve` (usually an HF path), so
            // no default can be right. This placeholder establishes the
            // connection; `list_models` then fills the picker with the one
            // real id and `/model vllm/<id>` switches to it.
            Self::VLlm => "vllm/served-model",
            // llama-server ignores the request's `model` field entirely (one
            // GGUF per process), so this placeholder actually works as-is.
            Self::LlamaCpp => "llamacpp/local-model",
            // LiteLLM routes on the operator's own `model_name` aliases, so
            // like vLLM there is no id we can know up front. `gpt-4o-mini` is
            // the alias LiteLLM's own quickstart config ships with; the picker
            // replaces it with the live `/models` listing.
            Self::LiteLlm => "litellm/gpt-4o-mini",
            // Azure AI Foundry deployments are user-specific (each subscription
            // names its own deployments), so there's no sensible default. The
            // placeholder routes to the right provider but forces the user to
            // override with `/model azure/<your-deployment>`.
            Self::AzureAIFoundry => "azure/<deployment>",
            // Generic OpenAI-compatible endpoint (SML Gateway, LiteLLM, Portkey,
            // vLLM, etc.). Users supply their own model id via /model oai/<id>;
            // the "oai/" prefix is stripped before the request goes upstream.
            Self::OpenAICompat => "oai/gpt-4o-mini",
            // DeepSeek's V4-flash model. `deepseek-v4-pro` is the higher-
            // tier sibling; older aliases `deepseek-chat` / `deepseek-reasoner`
            // still work on the wire but `/v1/models` only lists the V4 line,
            // so that's what catalogue-seed pulls in.
            Self::DeepSeek => "deepseek-v4-flash",
            // NSTDA / สวทช. Thai LLM aggregator (thaillm.or.th). Hosts
            // multiple Thai-language 8B models (OpenThaiGPT, Typhoon-S,
            // Pathumma, THaLLE) on an OpenAI-compatible endpoint. The
            // `thaillm/` prefix is stripped before the wire request, so
            // users type `/model thaillm/<id>` and the upstream sees the
            // bare model id. OpenThaiGPT v7.2 is the most general-purpose
            // default; users can `/model thaillm/<other>` to switch.
            Self::ThaiLLM => "thaillm/OpenThaiGPT-ThaiLLM-8B-Instruct-v7.2",
            // NVIDIA NIM — OpenAI-compatible hosted inference at integrate.api.nvidia.com.
            // Stored ids use a uniform `nvidia/` routing prefix; for NVIDIA-owned models
            // that yields a doubled prefix (`nvidia/nvidia/<name>`), the outer one stripped
            // by build_provider before the request. Override via NVIDIA_BASE_URL for on-prem.
            Self::Nvidia => "nvidia/nvidia/nemotron-3-super-120b-a12b",
            // MiniMax — Chinese AI lab, OpenAI-compatible endpoint at
            // api.minimax.io/v1. MiniMax-M3 is the latest flagship model.
            // MiniMax-M2 remains available. Models use the `minimax/<id>`
            // prefix; the prefix is stripped before the request reaches
            // the upstream.
            Self::Minimax => "minimax/MiniMax-M3",
            // OpenCodeGo (opencode.ai) — OpenAI-compatible hosted inference.
            // Models use the `opencode-go/<id>` prefix (e.g.
            // `opencode-go/kimi-k2.6`); the prefix is stripped before
            // the request reaches the upstream.
            Self::OpenCodeGo => "opencode-go/deepseek-v4-flash",
            // Moonshot AI — latest general Kimi flagship. The `-code`
            // variants (kimi-k2.7-code…) are coding-specialised; k2.6 is
            // the newest general-purpose model. `moonshot/` prefix is
            // stripped before the upstream request.
            Self::Moonshot => "moonshot/kimi-k2.6",
            // xAI — unified Grok flagship. `grok-4.3` supersedes the
            // grok-4 / grok-3 lines (those are aliases of it upstream).
            // The `xai/` prefix is stripped before the upstream request.
            Self::XAi => "xai/grok-4.3",
            // Groq — Llama 3.3 70B is the most general-purpose model on
            // the LPU cloud. The `groq/` prefix is stripped before the
            // upstream request.
            Self::Groq => "groq/llama-3.3-70b-versatile",
        }
    }

    /// Env var holding the base URL override, if the provider supports a
    /// configurable endpoint. Used by the Settings UI to let users point at
    /// self-hosted or regional endpoints.
    pub fn endpoint_env(&self) -> Option<&'static str> {
        match self {
            Self::TokenRouter => Some("TOKENROUTER_BASE_URL"),
            Self::AtlasCloud => Some("ATLASCLOUD_BASE_URL"),
            Self::Unifically => Some("UNIFICALLY_BASE_URL"),
            Self::MetaAi => Some("META_BASE_URL"),
            Self::NineRouter => Some("NINEROUTER_BASE_URL"),
            Self::DashScope => Some("DASHSCOPE_BASE_URL"),
            Self::QwenCloud => Some("QWENCLOUD_BASE_URL"),
            Self::Sis => Some("SIS_BASE_URL"),
            Self::Ollama => Some("OLLAMA_BASE_URL"),
            Self::OllamaAnthropic => Some("OLLAMA_BASE_URL"),
            Self::ZAi => Some("ZAI_BASE_URL"),
            Self::LMStudio => Some("LMSTUDIO_BASE_URL"),
            Self::VLlm => Some("VLLM_BASE_URL"),
            Self::LlamaCpp => Some("LLAMACPP_BASE_URL"),
            Self::LiteLlm => Some("LITELLM_BASE_URL"),
            Self::AzureAIFoundry => Some("AZURE_AI_FOUNDRY_ENDPOINT"),
            Self::OpenAICompat => Some("OPENAI_COMPAT_BASE_URL"),
            Self::DeepSeek => Some("DEEPSEEK_BASE_URL"),
            Self::ThaiLLM => Some("THAILLM_BASE_URL"),
            Self::Nvidia => Some("NVIDIA_BASE_URL"),
            Self::Minimax => Some("MINIMAX_BASE_URL"),
            Self::OpenCodeGo => Some("OPENCODE_GO_BASE_URL"),
            Self::Moonshot => Some("MOONSHOT_BASE_URL"),
            Self::XAi => Some("XAI_BASE_URL"),
            Self::Groq => Some("GROQ_BASE_URL"),
            _ => None,
        }
    }

    /// Whether the Settings UI should expose this provider's base URL. We
    /// keep hosted services (DashScope, Z.ai) locked to their
    /// defaults so users can't accidentally mis-point them; only self-hosted
    /// backends like Ollama and LMStudio are surfaced for editing. The env
    /// var still overrides at startup for power users who need it.
    pub fn endpoint_user_configurable(&self) -> bool {
        matches!(
            self,
            Self::Ollama
                | Self::OllamaAnthropic
                | Self::LMStudio
                | Self::VLlm
                | Self::LlamaCpp
                | Self::LiteLlm
                | Self::AzureAIFoundry
                | Self::OpenAICompat
                // Self-hosted router: base URL / port varies per user, so the
                // Settings base-URL field must be editable (not a fixed default).
                | Self::NineRouter,
        )
    }

    /// True when the *endpoint* owns the model list, not the shipped
    /// catalogue: Ollama lists what the user pulled, and a proxy
    /// (LiteLLM / vLLM / llama.cpp / LMStudio / an internal gateway) lists
    /// the operator's own aliases. For these, a successful `list_models()`
    /// is the complete, authoritative set — the catalogue may only supply
    /// *metadata* (context window) for an id the endpoint confirms.
    ///
    /// Why it matters: `/model <id>` caches a probed context window under
    /// the provider key (`model_catalogue::upsert_cache_entry`), but the
    /// endpoint is per-machine/per-URL. Letting those cached rows *add*
    /// ids made every model the user ever switched to linger in `/models`
    /// forever, long after the proxy stopped serving it.
    pub fn lists_from_endpoint(&self) -> bool {
        matches!(
            self,
            Self::Ollama
                | Self::OllamaAnthropic
                | Self::LMStudio
                | Self::VLlm
                | Self::LlamaCpp
                | Self::LiteLlm
                | Self::OpenAICompat
        )
    }

    /// Default base URL shown as a placeholder in the Settings UI when the
    /// user hasn't configured one. `None` for providers without an endpoint
    /// concept (Anthropic, OpenAI, etc. — those always hit the official API).
    pub fn default_endpoint(&self) -> Option<&'static str> {
        match self {
            Self::TokenRouter => Some("https://api.tokenrouter.com/v1"),
            Self::AtlasCloud => Some("https://api.atlascloud.ai/v1"),
            Self::Unifically => Some("https://api.unifically.com/v1"),
            Self::MetaAi => Some("https://api.meta.ai/v1"),
            // Self-hosted router; localhost default. Override per user via
            // NINEROUTER_BASE_URL for a remote / non-default-port instance.
            Self::NineRouter => Some("http://localhost:20128/v1"),
            Self::DashScope => Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            // International / Singapore region of DashScope.
            Self::QwenCloud => Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1"),
            Self::Ollama => Some("http://localhost:11434"),
            Self::OllamaAnthropic => Some("http://localhost:11434"),
            // Z.ai exposes the Coding Plan at /api/coding/paas/v4. The
            // general BigModel endpoint at https://open.bigmodel.cn/api/paas/v4
            // is also OpenAI-compatible — power users can override via
            // ZAI_BASE_URL if they don't have the Coding Plan SKU.
            Self::ZAi => Some("https://api.z.ai/api/coding/paas/v4"),
            // LMStudio exposes an OpenAI-compatible endpoint at /v1.
            // Default port 1234; users routinely change it, hence the
            // editable Settings field above.
            Self::LMStudio => Some("http://localhost:1234/v1"),
            // `vllm serve` binds 0.0.0.0:8000 and exposes /v1.
            Self::VLlm => Some("http://localhost:8000/v1"),
            // `llama-server` binds 127.0.0.1:8080 and exposes /v1.
            Self::LlamaCpp => Some("http://localhost:8080/v1"),
            // `litellm --config config.yaml` binds 0.0.0.0:4000 and exposes
            // the OpenAI surface at the root — `/v1` is accepted too, and is
            // what keeps the derived `/models` and `/model/info` probes on
            // the same prefix.
            Self::LiteLlm => Some("http://localhost:4000/v1"),
            Self::AzureAIFoundry => Some("https://{resource}.services.ai.azure.com"),
            // Generic OAI-compat: users always set their own URL; this
            // placeholder just hints at the expected shape (path ending in /v1).
            Self::OpenAICompat => Some("http://localhost:8000/v1"),
            Self::DeepSeek => Some("https://api.deepseek.com/v1"),
            Self::ThaiLLM => Some("http://thaillm.or.th/api/v1"),
            Self::Nvidia => Some("https://integrate.api.nvidia.com/v1"),
            // MiniMax international endpoint (api.minimax.io). The China
            // endpoint at api.minimax.chat uses a different auth scheme
            // (GroupId query param) and is NOT a drop-in OpenAI-compat
            // target — power users on the China platform must override
            // via MINIMAX_BASE_URL and accept that some calls may fail.
            // The legacy api.minimaxi.com URL was rejected by some
            // tenants with "invalid api key (2049)" — .io is the
            // current public OpenAI-compatible URL.
            Self::Minimax => Some("https://api.minimax.io/v1"),
            // OpenCodeGo — hosted gateway at opencode.ai.
            Self::OpenCodeGo => Some("https://opencode.ai/zen/go/v1"),
            // Moonshot AI — international endpoint. Mainland users
            // override to https://api.moonshot.cn/v1 via MOONSHOT_BASE_URL.
            Self::Moonshot => Some("https://api.moonshot.ai/v1"),
            // xAI — public OpenAI-compatible endpoint.
            Self::XAi => Some("https://api.x.ai/v1"),
            // Groq — the OpenAI-compat surface lives under /openai/v1.
            Self::Groq => Some("https://api.groq.com/openai/v1"),
            _ => None,
        }
    }

    /// True when this provider runs on the user's own machine, so text sent
    /// to it never leaves the host. Drives dev-plan/55 masking: PII is worth
    /// hiding from a cloud endpoint, but masking a local model only degrades
    /// the answer for no privacy gain.
    ///
    /// `OpenAICompat` is deliberately NOT listed even though it's usually a
    /// local runtime — its base URL is user-supplied and can point anywhere,
    /// and the safe default for a privacy gate is to treat unknown as remote.
    /// `LiteLlm` is out for the same reason: the proxy is self-hosted, but it
    /// is a router whose upstream is normally OpenAI/Anthropic/…, so the text
    /// does leave the host.
    pub fn is_local(&self) -> bool {
        matches!(
            self,
            Self::Ollama | Self::OllamaAnthropic | Self::LMStudio | Self::VLlm | Self::LlamaCpp
        )
    }

    /// True when the user has a usable API key for this provider —
    /// either via the OS keychain (`secrets::get`) or the relevant
    /// env var (set directly or loaded from `.env`). Providers with
    /// no auth requirement (Ollama, LMStudio, AgentSdk) always
    /// return true. Used by the skill-recommended-model resolver to
    /// pick the first candidate the user can actually call.
    pub fn has_key_available(&self) -> bool {
        // OpenAI-compatible endpoints are local runtimes (vLLM / llama.cpp /
        // SGLang / Atlas) — auth optional, never require a key to be usable.
        // LiteLLM the same: a proxy started without `master_key` takes any
        // bearer, so a missing LITELLM_API_KEY must not mark it unusable.
        if matches!(self, Self::OpenAICompat | Self::LiteLlm) {
            return true;
        }
        let Some(env_var) = self.api_key_env() else {
            return true; // No auth required (local runtimes, AgentSdk).
        };
        if std::env::var(env_var).is_ok_and(|v| !v.is_empty()) {
            return true;
        }
        crate::secrets::get(self.name()).is_some_and(|v| !v.is_empty())
    }

    /// Env var holding the API key, if any. Ollama has no auth.
    pub fn api_key_env(&self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::AgentSdk => None, // Uses Claude Code's own auth
            Self::OpenAI => Some("OPENAI_API_KEY"),
            Self::OpenAIResponses => Some("OPENAI_API_KEY"),
            // ChatGptCodex auths via OAuth access_token stored in
            // ~/.config/thclaws/auth/<profile>.json — no env var.
            Self::ChatGptCodex => None,
            Self::OpenRouter => Some("OPENROUTER_API_KEY"),
            Self::TokenRouter => Some("TOKENROUTER_API_KEY"),
            Self::AtlasCloud => Some("ATLASCLOUD_API_KEY"),
            Self::Unifically => Some("UNIFICALLY_API_KEY"),
            Self::MetaAi => Some("META_API_KEY"),
            Self::NineRouter => Some("NINEROUTER_API_KEY"),
            Self::Gemini => Some("GEMINI_API_KEY"),
            Self::Ollama => None,
            Self::OllamaAnthropic => None,
            Self::OllamaCloud => Some("OLLAMA_CLOUD_API_KEY"),
            Self::DashScope => Some("DASHSCOPE_API_KEY"),
            Self::QwenCloud => Some("QWENCLOUD_API_KEY"),
            Self::Sis => Some("SIS_API_KEY"),
            Self::ZAi => Some("ZAI_API_KEY"),
            Self::LMStudio => None, // Local runtime, no auth.
            // Self-hosted; auth only if started with --api-key, which
            // users set through the generic compat provider instead.
            Self::VLlm | Self::LlamaCpp => None,
            // Optional — set only when the proxy runs with a master key or
            // mints virtual keys. Absent, `build_provider` sends a
            // placeholder bearer rather than refusing to build.
            Self::LiteLlm => Some("LITELLM_API_KEY"),
            Self::AzureAIFoundry => Some("AZURE_AI_FOUNDRY_API_KEY"),
            Self::OpenAICompat => Some("OPENAI_COMPAT_API_KEY"),
            Self::DeepSeek => Some("DEEPSEEK_API_KEY"),
            Self::ThaiLLM => Some("THAILLM_API_KEY"),
            Self::Nvidia => Some("NVIDIA_API_KEY"),
            Self::Minimax => Some("MINIMAX_API_KEY"),
            Self::OpenCodeGo => Some("OPENCODE_GO_API_KEY"),
            Self::Moonshot => Some("MOONSHOT_API_KEY"),
            Self::XAi => Some("XAI_API_KEY"),
            Self::Groq => Some("GROQ_API_KEY"),
        }
    }

    /// Resolve short model aliases to full names — **provider-blind**.
    /// e.g. "sonnet" → "claude-sonnet-4-6", "opus" → "claude-opus-4-6".
    /// Use this for explicit user-typed `/model <alias>` commands where
    /// the user intends to switch providers along with the model. For
    /// passive resolution (agent defs, etc.) where the current provider
    /// must be preserved, use `resolve_alias_for_provider` instead.
    ///
    /// Matching is case-insensitive: `OpenThaiGPT`, `openthaigpt`, and
    /// `OPENTHAIGPT` all resolve the same way. Non-alias inputs are
    /// returned with their original casing preserved (model ids upstream
    /// are case-sensitive — only the alias *lookup* is folded).
    pub fn resolve_alias(model: &str) -> String {
        match model.to_lowercase().as_str() {
            "sonnet" => "claude-sonnet-4-6".into(),
            "opus" => "claude-opus-4-6".into(),
            "haiku" => "claude-haiku-4-5".into(),
            "flash" => "gemini-2.5-flash".into(),
            "openthaigpt" => "thaillm/OpenThaiGPT-ThaiLLM-8B-Instruct-v7.2".into(),
            "pathumma" => "thaillm/Pathumma-ThaiLLM-qwen3-8b-think-3.0.0".into(),
            "thalle" => "thaillm/THaLLE-0.2-ThaiLLM-8B-fa".into(),
            "typhoon" => "thaillm/Typhoon-S-ThaiLLM-8B-Instruct".into(),
            _ => model.to_string(),
        }
    }

    /// Provider-aware alias resolution. Returns the full model id within
    /// the given provider's namespace, or `None` if the alias doesn't
    /// belong there (e.g. `sonnet` requested on a native OpenAI provider).
    ///
    /// Used by SpawnTeammate so that an agent def saying `model: sonnet`
    /// keeps the team on the project's chosen provider — without this,
    /// the global `resolve_alias` would surprise-switch a worktree
    /// teammate to native Anthropic even if the project is on OpenRouter.
    pub fn resolve_alias_for_provider(model: &str, provider: Self) -> Option<String> {
        // Match against the lowercased input so callers can write `Sonnet`
        // or `OpenThaiGPT` and still hit the alias table — the resolved
        // upstream id retains its original casing.
        let lower = model.to_lowercase();
        let anthropic_id = match lower.as_str() {
            "sonnet" => Some("claude-sonnet-4-6"),
            "opus" => Some("claude-opus-4-6"),
            "haiku" => Some("claude-haiku-4-5"),
            _ => None,
        };
        let google_id = match lower.as_str() {
            "flash" => Some("gemini-2.5-flash"),
            _ => None,
        };
        let thaillm_id = match lower.as_str() {
            "openthaigpt" => Some("thaillm/OpenThaiGPT-ThaiLLM-8B-Instruct-v7.2"),
            "pathumma" => Some("thaillm/Pathumma-ThaiLLM-qwen3-8b-think-3.0.0"),
            "thalle" => Some("thaillm/THaLLE-0.2-ThaiLLM-8B-fa"),
            "typhoon" => Some("thaillm/Typhoon-S-ThaiLLM-8B-Instruct"),
            _ => None,
        };

        match provider {
            Self::Anthropic => anthropic_id.map(String::from),
            Self::Gemini => google_id.map(String::from),
            Self::ThaiLLM => thaillm_id.map(String::from),
            Self::OpenRouter => {
                if let Some(id) = anthropic_id {
                    return Some(format!("openrouter/anthropic/{id}"));
                }
                if let Some(id) = google_id {
                    return Some(format!("openrouter/google/{id}"));
                }
                None
            }
            // Providers without a notion of these aliases. Returning None
            // signals "alias doesn't apply here" so the caller can fall
            // back to whatever default the user had configured rather than
            // surprise-switching to a different provider.
            Self::OpenAI
            | Self::OpenAIResponses
            | Self::AtlasCloud
            | Self::Unifically
            | Self::MetaAi
            // 9router uses full `9router/<alias>/<model>` ids; no short-alias
            // table (the alias segment is 9router's own, typed explicitly).
            | Self::NineRouter
            | Self::ChatGptCodex
            | Self::AgentSdk
            | Self::Ollama
            | Self::OllamaAnthropic
            | Self::OllamaCloud
            | Self::DashScope
            | Self::QwenCloud
            | Self::Sis
            | Self::ZAi
            | Self::LMStudio
            | Self::VLlm
            | Self::LlamaCpp
            | Self::LiteLlm
            | Self::AzureAIFoundry
            | Self::OpenAICompat
            | Self::DeepSeek
            | Self::Nvidia
            | Self::OpenCodeGo
            // TokenRouter uses full `tokenrouter/<vendor>/<model>` ids; no
            // short-alias table (users type the explicit id).
            | Self::TokenRouter
            | Self::Minimax
            | Self::Moonshot
            | Self::XAi
            | Self::Groq => None,
        }
    }

    /// Detect the provider implied by a model string prefix.
    /// Also resolves short aliases first.
    pub fn detect(model: &str) -> Option<Self> {
        let model = &Self::resolve_alias(model);
        if model.starts_with("openrouter/") {
            // Check openrouter/ first — it's the most specific prefix.
            // Models look like openrouter/anthropic/claude-sonnet-4-6.
            Some(Self::OpenRouter)
        } else if model.starts_with("atlascloud/") {
            Some(Self::AtlasCloud)
        } else if model.starts_with("unifically/") {
            Some(Self::Unifically)
        } else if model.starts_with("meta/") {
            // Meta AI (api.meta.ai) — BYOK only, no gateway route. Ids look
            // like meta/muse-spark-1.2; the prefix is stripped before the
            // upstream request. OpenRouter proxies the same models, but its
            // ids always arrive as `openrouter/meta/...` and that branch is
            // checked first — same arrangement as `nvidia/` above.
            Some(Self::MetaAi)
        } else if model.starts_with("9router/") {
            // Self-hosted 9router gateway. Ids look like
            // `9router/kr/claude-sonnet-4.5`; the `9router/` prefix is stripped
            // before the request so 9router sees `kr/claude-sonnet-4.5`.
            Some(Self::NineRouter)
        } else if model.starts_with("tokenrouter/") {
            // TokenRouter (tokenrouter.com) — OpenAI-compatible unified
            // gateway. Models look like tokenrouter/anthropic/claude-sonnet-4.5;
            // the `tokenrouter/` prefix is stripped before the upstream call.
            Some(Self::TokenRouter)
        } else if model.starts_with("agent/") {
            Some(Self::AgentSdk)
        } else if model.starts_with("claude-") {
            Some(Self::Anthropic)
        } else if model.starts_with("chatgpt-codex/") {
            // ChatGPT-subscription Codex path — MUST be checked before the
            // bare `codex/` / `model.contains("codex")` arm below, else
            // the broader match steals the route.
            Some(Self::ChatGptCodex)
        } else if model.starts_with("codex/") || model.contains("codex") {
            Some(Self::OpenAIResponses)
        } else if model.starts_with("gpt-") && model.contains("-pro") {
            // OpenAI's `-pro` tier is Responses-only: `gpt-5-pro`,
            // `gpt-5.2-pro`, `gpt-5.4-pro`, `gpt-5.5-pro` and their dated
            // snapshots all answer `/v1/responses` and 404 on
            // `/v1/chat/completions` with "This is not a chat model". They
            // used to land on the OpenAI arm below and fail every time.
            //
            // Guarded on the `gpt-` prefix so it cannot catch a `-pro` model
            // from another vendor; every routed id here is one of OpenAI's
            // own bare names, since prefixed ones (openrouter/…, atlascloud/…)
            // are matched earlier.
            Some(Self::OpenAIResponses)
        } else if model.starts_with("gpt-")
            || model.starts_with("o1-")
            || model.starts_with("o3-")
            || model.starts_with("o3")
            || model.starts_with("o4-")
        {
            Some(Self::OpenAI)
        } else if model.starts_with("gemini-") || model.starts_with("gemma-") {
            // Gemma open-weights models are served via the same Gemini API
            // (generativelanguage.googleapis.com) and use the same auth, so
            // they route through the Gemini provider. Covers `gemma-3-*`,
            // `gemma-3n-*`, `gemma-4-*`, etc.
            Some(Self::Gemini)
        } else if model.starts_with("qc/") {
            // Alibaba Cloud Singapore DashScope (`dashscope-intl.aliyuncs.com`).
            // Models look like `qc/qwen-max`, `qc/qwen-plus`, etc.; the
            // `qc/` prefix is stripped before the request reaches the
            // upstream so it sees the bare `qwen-*` id.
            Some(Self::QwenCloud)
        } else if model.starts_with("sis/") {
            // Alibaba Model Studio workspace endpoint. Models look like
            // `sis/qwen3.8-flash`; the prefix is stripped before the
            // request reaches the upstream, which expects the bare id.
            Some(Self::Sis)
        } else if model.starts_with("dashscope/") {
            // Alibaba Cloud mainland DashScope routing prefix. Models look
            // like `dashscope/qwen-max`, `dashscope/deepseek-v3.2`,
            // `dashscope/kimi-k2.6`, etc.; the `dashscope/` prefix is
            // stripped by `build_provider` before the request reaches
            // Alibaba's upstream so it sees the bare id. Bare `qwen-*` /
            // `qwq-*` ids still route to DashScope below — backward
            // compat for settings that pre-date this prefix being
            // canonical.
            Some(Self::DashScope)
        } else if model.starts_with("qwen") || model.starts_with("qwq-") {
            Some(Self::DashScope)
        } else if model.starts_with("deepseek-") {
            // DeepSeek's bare model IDs (deepseek-chat, deepseek-reasoner,
            // deepseek-coder, …) are unique enough that no namespace prefix
            // is needed — same shape as Anthropic's `claude-` and OpenAI's
            // `gpt-`. Prefix is NOT stripped on the wire.
            Some(Self::DeepSeek)
        } else if model.starts_with("thaillm/") {
            // NSTDA Thai LLM aggregator. Models look like
            // thaillm/OpenThaiGPT-ThaiLLM-8B-Instruct-v7.2 — the
            // "thaillm/" prefix is stripped before the request reaches
            // the OpenAI-compatible upstream at thaillm.or.th.
            Some(Self::ThaiLLM)
        } else if model.starts_with("zai/") {
            // Z.ai (GLM Coding Plan). Models look like zai/glm-4.6.
            // The "zai/" prefix is stripped before forwarding to the
            // OpenAI-compatible upstream.
            Some(Self::ZAi)
        } else if model.starts_with("minimax/") {
            // MiniMax (minimaxi.com). Models look like
            // minimax/MiniMax-M2. The prefix is stripped before
            // reaching the OpenAI-compatible upstream.
            Some(Self::Minimax)
        } else if model.starts_with("oai/") {
            // Generic OpenAI-compatible endpoint (SML Gateway, LiteLLM,
            // Portkey, vLLM, internal proxies, etc.). The "oai/" prefix
            // is stripped before forwarding to the upstream API.
            Some(Self::OpenAICompat)
        } else if model.starts_with("lmstudio/") {
            // LMStudio (local runtime, OpenAI-compatible at /v1).
            // Models look like lmstudio/<loaded-model-id>; the prefix
            // is stripped before the request reaches LMStudio.
            Some(Self::LMStudio)
        } else if model.starts_with("vllm/") {
            // Self-hosted vLLM. Models look like vllm/<served-id>, which is
            // often an HF path (vllm/Qwen/Qwen3-8B) — only the leading
            // "vllm/" is stripped, so the remaining slashes survive.
            Some(Self::VLlm)
        } else if model.starts_with("llamacpp/") {
            // llama.cpp's llama-server. The id after the prefix is cosmetic
            // (the server serves whichever GGUF it was started with).
            Some(Self::LlamaCpp)
        } else if model.starts_with("litellm/") {
            // Self-hosted LiteLLM proxy. Models look like
            // litellm/<model_name-alias>, and an alias may itself be
            // namespaced (litellm/azure/gpt-4o) — only the leading
            // "litellm/" is stripped, so the rest survives to the proxy.
            Some(Self::LiteLlm)
        } else if model.starts_with("oa/") {
            Some(Self::OllamaAnthropic)
        } else if model.starts_with("ollama/") {
            Some(Self::Ollama)
        } else if model.starts_with("ollama-cloud/") {
            Some(Self::OllamaCloud)
        } else if model.starts_with("azure/") {
            Some(Self::AzureAIFoundry)
        } else if model.starts_with("nvidia/") {
            // NVIDIA NIM (integrate.api.nvidia.com). The catalogue stores
            // every NIM model under a uniform `nvidia/` routing prefix
            // regardless of upstream owner namespace — `nvidia/nvidia/<name>`
            // for NVIDIA-owned models, `nvidia/meta/<name>`, `nvidia/google/<name>`
            // etc. for third-party-owned. `build_provider` strips the outer
            // `nvidia/` so the upstream sees the original namespaced id.
            // OpenRouter proxies the same models as `openrouter/nvidia/...`;
            // the `openrouter/` check above catches those first.
            Some(Self::Nvidia)
        } else if model.starts_with("opencode-go/") {
            Some(Self::OpenCodeGo)
        } else if model.starts_with("moonshot/") {
            // Moonshot AI (Kimi family). Models look like
            // moonshot/kimi-k2.6 or moonshot/moonshot-v1-128k; the
            // `moonshot/` prefix is stripped before the request reaches
            // the OpenAI-compatible upstream at api.moonshot.ai.
            Some(Self::Moonshot)
        } else if model.starts_with("xai/") || model.starts_with("grok-") {
            // xAI (Grok). Canonical ids carry an `xai/` prefix
            // (xai/grok-4.3); it's stripped before the upstream request.
            // Bare `grok-*` ids route here too for nicer UX — they pass
            // through unchanged (the upstream expects the bare id).
            // openrouter/x-ai/grok-* is caught by the `openrouter/`
            // branch above, so this never steals those.
            Some(Self::XAi)
        } else if model.starts_with("groq/") {
            // Groq (LPU cloud). Models look like
            // groq/llama-3.3-70b-versatile or groq/moonshotai/kimi-k2-instruct;
            // the `groq/` prefix is stripped before the request reaches
            // the OpenAI-compatible upstream at api.groq.com/openai/v1.
            Some(Self::Groq)
        } else {
            None
        }
    }

    /// Look up by lowercase provider name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.name() == name)
    }
}

pub use assemble::{assemble, collect_turn, AssembledEvent, TurnResult};

/// Find the first occurrence of `needle` in `haystack` (byte-slice equivalent
/// of `str::find`). Used by every streaming provider to locate event
/// boundaries (`b"\n\n"` for SSE, `b"\n"` for NDJSON, `b"\r\n\r\n"` for
/// Gemini's CRLF SSE) on a `Vec<u8>` buffer rather than a `String`.
///
/// M6.21 BUG H1: pre-fix every provider buffered chunks as
/// `String::from_utf8_lossy(&chunk)`. When TCP delivered a chunk that
/// ended mid-multi-byte-UTF-8-char (any 2-3 byte char split at the packet
/// boundary), `from_utf8_lossy` inserted U+FFFD for the trailing partial
/// byte, AND for the next chunk's leading continuation byte — corrupting
/// the original character into two replacement chars. Affected every
/// non-ASCII response (Thai, Chinese, Japanese, emoji, accented Latin)
/// when the response was large enough to span TCP packets.
///
/// Fix: buffer raw bytes, find the event boundary on bytes (the boundary
/// markers themselves are ASCII-safe), then decode only the complete
/// event before parsing. Complete SSE/NDJSON events are valid UTF-8 by
/// construction (the JSON inside is well-formed UTF-8), so the decode is
/// always safe at the boundary.
pub(crate) fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Scrub an API key from an error response body before surfacing it.
///
/// Some LLM providers echo the offending `Authorization` header (or the
/// `?key=...` query param, in Gemini's case) into 4xx/5xx response
/// bodies. Those bodies end up in user-visible error messages via
/// `Error::Provider(format!("http {status}: {text}"))`. Passing the
/// body through this helper first ensures the key never appears in
/// logs, session JSONL, or the REPL output.
pub(crate) fn redact_key(text: &str, key: &str) -> String {
    if key.len() < 8 {
        // Don't redact values shorter than 8 chars — they're more likely
        // false positives than real secrets.
        return text.to_string();
    }
    text.replace(key, "<redacted-api-key>")
}

/// Turn a provider error string into a one-line human-readable message
/// the chat UI can show in an error bubble. Handles the common shape
/// providers surface as `Error::Provider("http <status> <text>: <body>")`,
/// where `<body>` is usually JSON. OpenRouter's body looks like
/// `{"error":{"message":"Provider returned error","code":429,
/// "metadata":{"raw":"..., add your own key to ..."}}}` — the
/// `metadata.raw` field is the one a human needs to read; the rest is
/// machine framing the user shouldn't have to skim.
///
/// Returns the cleanest available message, prefixed with a status-class
/// label (`Rate limited`, `Auth failed`, …) when one can be derived
/// from the HTTP status. Falls back to the original text when nothing
/// parses.
pub fn humanize_provider_error(raw: &str) -> String {
    let trimmed = raw
        .trim_start_matches("Error: ")
        .trim_start_matches("provider error: ")
        .trim();
    let Some(brace) = trimmed.find('{') else {
        return raw.to_string();
    };
    let head = trimmed[..brace].trim_end_matches(':').trim();
    let body = &trimmed[brace..];

    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return raw.to_string();
    };
    // Walk the known paths in priority order. OpenRouter exposes the
    // upstream's human message at `error.metadata.raw`; OpenAI-shape
    // bodies put it at `error.message`; some compat-layers flatten to
    // `message`.
    let extracted = v
        .pointer("/error/metadata/raw")
        .and_then(|x| x.as_str())
        .or_else(|| v.pointer("/error/message").and_then(|x| x.as_str()))
        .or_else(|| v.pointer("/message").and_then(|x| x.as_str()))
        .map(|s| s.to_string());
    let Some(msg) = extracted else {
        return raw.to_string();
    };

    let label = if head.contains("429") {
        "Rate limited"
    } else if head.contains("401") || head.contains("403") {
        "Auth failed"
    } else if head.contains("402") {
        "Credits required"
    } else if head.contains("500")
        || head.contains("502")
        || head.contains("503")
        || head.contains("504")
    {
        "Provider error"
    } else if head.starts_with("http ") {
        "HTTP error"
    } else {
        "Error"
    };
    format!("{label}: {msg}")
}

#[cfg(test)]
mod humanize_tests {
    use super::humanize_provider_error;

    #[test]
    fn openrouter_429_extracts_metadata_raw() {
        let raw = r#"provider error: http 429 Too Many Requests: {"error":{"message":"Provider returned error","code":429,"metadata":{"raw":"google/gemma-4-31b-it:free is temporarily rate-limited upstream. Please retry shortly.","provider_name":"Google AI Studio","is_byok":false}},"user_id":"user_2fB406KYrj4unLbk7vdmuNtcRrF"}"#;
        let out = humanize_provider_error(raw);
        assert_eq!(
            out,
            "Rate limited: google/gemma-4-31b-it:free is temporarily rate-limited upstream. Please retry shortly."
        );
    }

    #[test]
    fn openai_style_extracts_error_message() {
        let raw = r#"provider error: http 401 Unauthorized: {"error":{"message":"Invalid API key provided.","type":"invalid_request_error"}}"#;
        let out = humanize_provider_error(raw);
        assert_eq!(out, "Auth failed: Invalid API key provided.");
    }

    #[test]
    fn unparseable_falls_back_to_original() {
        let raw = "provider error: connection refused";
        let out = humanize_provider_error(raw);
        assert_eq!(out, raw);
    }

    #[test]
    fn server_5xx_labels_as_provider_error() {
        let raw = r#"provider error: http 503 Service Unavailable: {"error":{"message":"Backend is busy"}}"#;
        let out = humanize_provider_error(raw);
        assert_eq!(out, "Provider error: Backend is busy");
    }
}

/// Optional debug helper: when `THCLAWS_SHOW_RAW=1` (env) or
/// `showRawResponse: true` (settings.json) is set, providers accumulate the
/// assistant's text as it streams and dump a fenced dim block to stderr at
/// end-of-turn so the user can compare what the model actually emitted vs
/// what got rendered.
///
/// Env var wins over settings so quick one-off debug runs don't require
/// editing config.
pub struct RawDump {
    enabled: bool,
    label: String,
    buf: String,
}

impl RawDump {
    pub fn new(label: impl Into<String>) -> Self {
        let enabled = match std::env::var("THCLAWS_SHOW_RAW").ok() {
            Some(v) => !v.is_empty() && v != "0",
            None => crate::config::ProjectConfig::load()
                .and_then(|c| c.show_raw_response)
                .unwrap_or(false),
        };
        Self {
            enabled,
            label: label.into(),
            buf: String::new(),
        }
    }

    pub fn push(&mut self, s: &str) {
        if self.enabled {
            self.buf.push_str(s);
        }
    }

    /// Print the accumulated text and clear the buffer. Safe to call
    /// repeatedly; only emits when there's something new and the flag is on.
    pub fn flush(&mut self) {
        if !self.enabled || self.buf.is_empty() {
            return;
        }
        eprintln!(
            "\n\x1b[35m─── raw response [{}] ({} chars, {} bytes) ───\x1b[0m\n\x1b[2m{}\x1b[0m\n\x1b[35m───\x1b[0m",
            self.label,
            self.buf.chars().count(),
            self.buf.len(),
            self.buf
        );
        self.buf.clear();
    }
}

impl Drop for RawDump {
    fn drop(&mut self) {
        self.flush();
    }
}

/// One knob for reasoning depth across every provider. thClaws stores
/// and transports it as a token budget (`StreamRequest::thinking_budget`)
/// so old settings files (`thinkingBudget: 10000`) keep working; each
/// provider maps the level onto whatever it exposes:
///
/// | level | Anthropic budget | OpenAI o*/gpt-5 effort | DeepSeek/GLM | Qwen | Gemini 2.5 budget (flash/pro) | Gemini 3 level | Ollama |
/// |---|---|---|---|---|---|---|---|
/// | 0 off    | none   | minimal/low | disabled | off           | 0 / 128    | low  | think:false |
/// | 1 low    | 2 048  | low         | enabled  | on, 2 048     | 2 048      | low  | think:true  |
/// | 2 medium | 10 000 | medium      | enabled  | on, 10 000    | 10 000     | high | think:true  |
/// | 3 high   | 32 000 | high        | enabled  | on, 32 000    | 24 576/32 000 | high | think:true |
/// | auto     | provider default (no field sent) |||||||
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingLevel {
    Off,
    Low,
    Medium,
    High,
}

impl ThinkingLevel {
    pub const LOW_BUDGET: u32 = 2_048;
    pub const MEDIUM_BUDGET: u32 = 10_000;
    pub const HIGH_BUDGET: u32 = 32_000;

    /// `0`–`3`, `off|low|medium|high`, or `auto` (→ `None`). A bare
    /// number ≥ 100 is taken as a raw token budget (legacy `/thinking N`).
    pub fn parse(s: &str) -> Option<Option<u32>> {
        let t = s.trim().to_ascii_lowercase();
        match t.as_str() {
            "auto" | "default" => Some(None),
            "0" | "off" | "none" => Some(Some(0)),
            "1" | "low" | "min" | "minimal" => Some(Some(Self::LOW_BUDGET)),
            "2" | "medium" | "med" | "mid" => Some(Some(Self::MEDIUM_BUDGET)),
            // No `"max"` alias. It used to land here, on HIGH_BUDGET, which is
            // not a maximum of anything: the arm below accepts any budget from
            // 100 up, so `/thinking 200000` is both legal and larger, and
            // anything over 16_000 already reports itself as "high". A word
            // that promises a ceiling while quietly picking 32_000 is worse
            // than no word — the usage line offers `<tokens>` for people who
            // want more, and never advertised `max` in the first place.
            "3" | "high" => Some(Some(Self::HIGH_BUDGET)),
            _ => t.parse::<u32>().ok().filter(|n| *n >= 100).map(|n| Some(n)),
        }
    }

    pub fn to_budget(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Low => Self::LOW_BUDGET,
            Self::Medium => Self::MEDIUM_BUDGET,
            Self::High => Self::HIGH_BUDGET,
        }
    }

    /// Bucket any budget back into a level; `None` = auto.
    pub fn from_budget(b: Option<u32>) -> Option<Self> {
        match b {
            None => None,
            Some(0) => Some(Self::Off),
            Some(n) if n <= 4_096 => Some(Self::Low),
            Some(n) if n <= 16_000 => Some(Self::Medium),
            Some(_) => Some(Self::High),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn number(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
        }
    }

    /// `{level, name, budget}` for the sidebar selector; `level: null` = auto.
    pub fn json(b: Option<u32>) -> serde_json::Value {
        match Self::from_budget(b) {
            None => serde_json::json!({"level": null, "name": "auto", "budget": null}),
            Some(l) => serde_json::json!({"level": l.number(), "name": l.name(), "budget": b}),
        }
    }

    /// `thinking_update` envelope broadcast after any change.
    pub fn update_payload(b: Option<u32>) -> serde_json::Value {
        serde_json::json!({"type": "thinking_update", "thinking": Self::json(b)})
    }

    /// Human label for status lines: `2 medium (10000 tokens)` / `auto`.
    pub fn describe(b: Option<u32>) -> String {
        match (Self::from_budget(b), b) {
            (None, _) => "auto (provider default)".to_string(),
            (Some(l), Some(n)) => format!("{} {} ({n} tokens)", l.number(), l.name()),
            (Some(l), None) => format!("{} {}", l.number(), l.name()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StreamRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    /// Extended-thinking budget — the wire form of [`ThinkingLevel`]
    /// (`ThinkingLevel::to_budget` / `from_budget`). `None` = provider default; `Some(0)` =
    /// explicitly OFF on providers that expose a switch (DeepSeek/GLM
    /// `thinking.type=disabled`, Qwen `enable_thinking=false`, OpenAI
    /// o-series/gpt-5 `reasoning_effort=low`, Gemini flash
    /// `thinkingBudget=0`); Anthropic sends no thinking block either way.
    /// Research uses `Some(0)`: a 10k-char digest on deepseek-v4 spent
    /// 5–13k reasoning tokens (77–89 s) versus 4–7 s with thinking off.
    pub thinking_budget: Option<u32>,
    /// Per-call override for the per-chunk idle timeout. `None` falls
    /// back to the global `stream_chunk_timeout()` (driven by the user
    /// `stream_chunk_timeout_secs` setting). `Some(d)` forces `d` for
    /// this one request only — used by known long-running features
    /// (research pipeline, `/kms html`) that legitimately need ≥15 min
    /// of stream idleness without raising the user's default.
    pub stream_chunk_timeout_override: Option<std::time::Duration>,
}

/// Hard 15-minute idle ceiling reserved for features that orchestrate
/// long-running single LLM calls (research synthesis, KMS HTML
/// generation). Passed in `StreamRequest::stream_chunk_timeout_override`
/// so each call overrides the user's `stream_chunk_timeout_secs`
/// setting without changing the global default for normal chat.
pub const LONG_RUNNING_STREAM_CHUNK_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(900);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    /// dev-plan/24: hidden reasoning tokens for OpenAI o1/o3 family.
    /// OpenAI surfaces these via `completion_tokens_details.
    /// reasoning_tokens` on Chat Completions and `output_tokens_
    /// details.reasoning_tokens` on Responses API. Anthropic's
    /// extended-thinking tokens are folded into output and aren't
    /// separately billed, so the Anthropic assembler leaves this
    /// as `None`. `Some(0)` ⇒ provider explicitly reported zero
    /// reasoning tokens (distinct from "didn't report").
    pub reasoning_output_tokens: Option<u32>,
}

impl Default for Usage {
    fn default() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            reasoning_output_tokens: None,
        }
    }
}

impl Usage {
    /// Accumulate another usage into this one (for cumulative tracking).
    pub fn accumulate(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens = match (
            self.cache_creation_input_tokens,
            other.cache_creation_input_tokens,
        ) {
            (Some(a), Some(b)) => Some(a + b),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        self.cache_read_input_tokens =
            match (self.cache_read_input_tokens, other.cache_read_input_tokens) {
                (Some(a), Some(b)) => Some(a + b),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
        self.reasoning_output_tokens =
            match (self.reasoning_output_tokens, other.reasoning_output_tokens) {
                (Some(a), Some(b)) => Some(a + b),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: Option<String>,
}

/// Display-only progress signal from the provider layer.
/// Never accumulated into messages or persisted — renderers use it to
/// drive spinners and tool-activity indicators.
#[derive(Debug, Clone, PartialEq)]
pub enum ProgressKind {
    /// Provider is idle / waiting for first response chunk.
    Thinking,
    /// A tool started within the provider (agent-SDK internal execution).
    ToolStart { id: String, label: String },
    /// A tool finished within the provider.
    ToolDone {
        id: String,
        label: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    MessageStart {
        model: String,
    },
    TextDelta(String),
    /// Reasoning/chain-of-thought delta from thinking models (DeepSeek
    /// `reasoning_content`, OpenAI o-series reasoning, etc.). Folded by
    /// `assemble` into a `ContentBlock::Thinking` block so the agent can
    /// echo it back on subsequent turns (required by DeepSeek's API).
    ThinkingDelta(String),
    ToolUseStart {
        id: String,
        name: String,
        thought_signature: Option<String>,
    },
    ToolUseDelta {
        partial_json: String,
    },
    ContentBlockStop,
    MessageStop {
        stop_reason: Option<String>,
        usage: Option<Usage>,
    },
    /// Display-only progress signal — not content, never persisted.
    /// Providers emit this instead of sending spinner frames as
    /// [`TextDelta`] so animation never leaks into logs, session
    /// JSONL, GUI adapters, or accumulated assistant text.
    Progress(ProgressKind),
}

pub type EventStream = BoxStream<'static, Result<ProviderEvent>>;

#[async_trait]
pub trait Provider: Send + Sync {
    async fn stream(&self, req: StreamRequest) -> Result<EventStream>;

    /// List models available from this provider. Default impl returns an
    /// error indicating the provider hasn't overridden it. Sorted by id.
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Err(crate::error::Error::Provider(
            "list_models not supported by this provider".into(),
        ))
    }

    /// Provider-side session identifier for resume support. The
    /// `anthropic-agent` SDK provider holds this in an internal
    /// `Arc<Mutex<Option<String>>>` populated from the first response
    /// frame that surfaces a `session_id`. Other providers don't
    /// maintain server-side conversation state and return `None`.
    ///
    /// Callers (the worker / REPL loop) read this after each
    /// `stream()` completes and persist any change to the session
    /// JSONL via `Session::append_provider_state_to` so a process
    /// restart or `/load` can rehydrate the id via
    /// [`Self::set_provider_session_id`] and the next `stream()` call
    /// passes `--resume <uuid>` to the subprocess.
    fn provider_session_id(&self) -> Option<String> {
        None
    }

    /// Reapply a previously-persisted provider session id, used by
    /// the worker right after `Session::load_from` so the next
    /// `stream()` call resumes the SDK's server-side conversation
    /// instead of starting fresh (the bug this trait method exists
    /// to fix). Default impl is a no-op — only the
    /// `anthropic-agent` provider overrides.
    fn set_provider_session_id(&self, _id: Option<String>) {}
}

/// Does the active provider have credentials (env var set) or is it
/// a no-auth local provider? Used by sidebar/UI code (and the
/// `model_set` / `config_poll` IPC arms in M6.36) to flag the active
/// provider's readiness without spinning up a real provider instance.
///
/// M6.36 SERVE9e: lifted out of `gui.rs` to an always-on home so the
/// WS transport's IPC handlers can use the same readiness check.
pub fn provider_has_credentials(cfg: &crate::config::AppConfig) -> bool {
    let kind = cfg.detect_provider_kind().ok();
    if kind_has_credentials(kind) {
        return true;
    }
    // A gateway-routed provider is "ready" even without a local key:
    // the gateway supplies credentials against the user's CLI token.
    // Without this, ticking the per-provider proxy toggle still leaves
    // the sidebar showing "no API key". Mirrors preferred_default_model().
    kind.is_some_and(|k| crate::providers::thclaws_gateway::for_kind(cfg, k).is_some())
}

/// True when `kind` has credentials available (env var, auth file, or
/// no-auth local provider). Same logic the GUI's auto-fallback path uses.
pub fn kind_has_credentials(kind: Option<ProviderKind>) -> bool {
    let Some(kind) = kind else { return false };
    match kind {
        ProviderKind::AgentSdk => true,
        ProviderKind::Ollama
        | ProviderKind::OllamaAnthropic
        | ProviderKind::LMStudio
        // Self-hosted runtimes — reachability is the real gate, not a key.
        | ProviderKind::VLlm
        | ProviderKind::LlamaCpp => true,
        // OpenAI-compatible = local runtimes (vLLM / llama.cpp / SGLang / Atlas)
        // pointed at OPENAI_COMPAT_BASE_URL; auth is optional (key sent if set).
        // LiteLLM is self-hosted with optional auth on the same terms.
        ProviderKind::OpenAICompat | ProviderKind::LiteLlm => true,
        // ChatGptCodex auths via a file-based OAuth token, not an env
        // var, so the generic api_key_env() probe below always misses.
        ProviderKind::ChatGptCodex => {
            match crate::codex_auth_store::resolve_for_profile("default") {
                Ok(Some(auth)) => !auth.is_expired(60),
                Ok(None) => false,
                Err(e) => {
                    eprintln!("\x1b[33m[codex-auth] credential check failed: {e}\x1b[0m");
                    false
                }
            }
        }
        other => other
            .api_key_env()
            .and_then(|v| std::env::var(v).ok())
            .map(|val| !val.trim().is_empty())
            .unwrap_or(false),
    }
}

/// Whether `kind` is something this user could actually send a request to —
/// by its own credentials, or with the gateway supplying them.
///
/// The model picker lists only reachable providers (issue #215). Kept as its
/// own function rather than inlined in the loop so the rule can be tested: it
/// decides what a user sees, and getting it wrong in the strict direction
/// makes working providers vanish, which is worse than the noise it removes.
fn kind_is_reachable(cfg: &crate::config::AppConfig, kind: ProviderKind) -> bool {
    kind_has_credentials(Some(kind)) || thclaws_gateway::for_kind(cfg, kind).is_some()
}

/// The `<provider>/` prefix a listing from `kind`'s endpoint must carry so
/// a copied id routes back through [`ProviderKind::detect`]. Mirrors the
/// `with_strip_model_prefix` argument each arm of `repl::build_provider`
/// passes; `openai-compat` is the one place where the prefix (`oai/`) and
/// the provider name diverge.
fn listing_model_prefix(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::LMStudio => "lmstudio/",
        ProviderKind::VLlm => "vllm/",
        ProviderKind::LlamaCpp => "llamacpp/",
        ProviderKind::LiteLlm => "litellm/",
        ProviderKind::OpenAICompat => "oai/",
        _ => "",
    }
}

/// Build a throwaway provider whose only job is `list_models()` — for a
/// `kind` that is NOT necessarily the session's active one. `build_provider`
/// can't serve this: it derives the kind from `config.model`, so the picker
/// could never ask "what does the LiteLLM box have?" while the user sits on
/// Anthropic.
///
/// Same env resolution as [`openai::probe_compat_limits`]: per-kind base-URL
/// env → `default_endpoint()`, key from the keychain → the per-kind env →
/// a placeholder (these backends have optional auth and ignore the header
/// when unset). Returns `None` for a kind the endpoint doesn't own.
pub fn build_listing_provider(kind: ProviderKind) -> Option<std::sync::Arc<dyn Provider>> {
    if !kind.lists_from_endpoint() {
        return None;
    }
    let base = kind
        .endpoint_env()
        .and_then(|v| std::env::var(v).ok())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| kind.default_endpoint().map(String::from))?;
    if matches!(kind, ProviderKind::Ollama | ProviderKind::OllamaAnthropic) {
        // Both Ollama kinds list from the same native `/api/tags`.
        return Some(std::sync::Arc::new(
            crate::providers::ollama::OllamaProvider::new().with_base_url(base),
        ));
    }
    let key = crate::secrets::get(kind.name())
        .or_else(|| kind.api_key_env().and_then(|v| std::env::var(v).ok()))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "local-no-auth".to_string());
    let url = if base.ends_with("/chat/completions") {
        base
    } else {
        format!("{}/chat/completions", base.trim_end_matches('/'))
    };
    Some(std::sync::Arc::new(
        crate::providers::openai::OpenAIProvider::new(key)
            .with_base_url(url)
            .with_strip_model_prefix(listing_model_prefix(kind)),
    ))
}

/// Ask `kind`'s endpoint for its model list, bounded so an unreachable host
/// can't stall a picker. `None` = we could not reach it (keep the catalogue);
/// `Some(ids)` = the authoritative set, empty included.
pub async fn live_model_ids(kind: ProviderKind, timeout_ms: u64) -> Option<Vec<String>> {
    let provider = build_listing_provider(kind)?;
    match tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        provider.list_models(),
    )
    .await
    {
        Ok(Ok(models)) => Some(models.into_iter().map(|m| m.id).collect()),
        _ => None,
    }
}

/// Build the `model_picker_open` payload for the session's **active**
/// provider — the per-provider modal `/model` (no arg) opens, and the one
/// the GUI pops after a key is stored.
///
/// `None` when there is nothing worth opening: fewer than `min_rows`
/// choices, or an endpoint-owned provider we could not reach (better no
/// modal than a modal listing models the box does not serve).
///
/// Extracted from two copies that had drifted — `shell_dispatch`'s `/model`
/// arm and `ipc`'s post-key switch.
pub async fn build_model_picker_payload(
    cfg: &crate::config::AppConfig,
    min_rows: usize,
) -> Option<String> {
    let kind = cfg.detect_provider_kind().ok()?;
    let name = crate::model_catalogue::provider_kind_name(kind);
    let cat = crate::model_catalogue::EffectiveCatalogue::load();
    let mut rows = cat.list_models_for_provider(name);
    rows.retain(|(_, e)| e.chat != Some(false));
    if name == "openrouter" && cfg.openrouter_free_only {
        rows.retain(|(_, e)| e.free == Some(true));
    }
    // Gateway routing is strictly metered: unpriced models 400 upstream.
    if crate::providers::thclaws_gateway::hides_unpriced_models(cfg, name) {
        rows.retain(|(_, e)| e.input_per_mtok.is_some() && e.output_per_mtok.is_some());
    }
    if kind.lists_from_endpoint() {
        // Built from the live config (not `build_listing_provider`) so each
        // kind keeps its own id prefix — `ollama-anthropic` in particular
        // lists over the Anthropic surface, not `/api/tags`.
        let provider = crate::repl::build_provider(cfg).ok()?;
        let live = tokio::time::timeout(
            std::time::Duration::from_millis(2000),
            provider.list_models(),
        )
        .await
        .ok()?
        .ok()?;
        let meta: std::collections::HashMap<String, crate::model_catalogue::ModelEntry> =
            rows.drain(..).collect();
        rows = live
            .into_iter()
            .map(|m| {
                let entry = meta.get(&m.id).cloned().unwrap_or_default();
                (m.id, entry)
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
    }
    if rows.len() < min_rows {
        return None;
    }
    let model_rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|(id, e)| {
            serde_json::json!({
                "id": crate::model_catalogue::canonical_model_id(name, id),
                "context": e.context,
                // dev-plan/57: the window may be the provider's blanket
                // default rather than a published figure. The picker renders
                // those as `200k?` — printing a floor as a specification is
                // what #190 was.
                "context_unverified": e.context_unverified(),
                "max_output": e.max_output,
                // Plan-10: surfaced for the OpenRouter "Free only" toggle in
                // the Settings modal. Other providers leave this None.
                "free": e.free,
            })
        })
        .collect();
    Some(
        serde_json::json!({
            "type": "model_picker_open",
            "provider": name,
            "current": cfg.model,
            "models": model_rows,
        })
        .to_string(),
    )
}

/// Build the cross-provider model-list payload the sidebar's inline
/// model picker dropdown consumes. Catalogue rows for every known
/// provider plus a live Ollama probe so models the user just
/// `ollama pull`-ed appear without a restart.
///
/// M6.36 SERVE9g — moved from `gui.rs` so the WS transport's
/// `request_all_models` IPC arm can call it from the always-on
/// dispatch table. Async because of the Ollama probe (`tokio::time::
/// timeout` against a possibly-unreachable host).
pub async fn build_all_models_payload() -> String {
    let cat = crate::model_catalogue::EffectiveCatalogue::load();
    let app_cfg = crate::config::AppConfig::load().unwrap_or_default();
    let free_only_or = app_cfg.openrouter_free_only;
    // Providers whose endpoint owns the list (`lists_from_endpoint`): ask
    // each one directly instead of rendering catalogue rows it may no
    // longer serve. Probed concurrently so the dropdown still opens in one
    // timeout, not one per backend.
    //
    // Only Ollama is probed unconditionally (long-standing behaviour — it is
    // the one backend users expect to "just be there"); the rest need a
    // configured base URL, so we don't knock on localhost:1234/4000/8000/8080
    // for every user who never set them up. OllamaAnthropic is left out: it
    // shares Ollama's host but needs `ollama-anthropic/` ids, and it has no
    // catalogue rows either, so it has never appeared in this picker.
    const ENDPOINT_OWNED: &[ProviderKind] = &[
        ProviderKind::Ollama,
        ProviderKind::LMStudio,
        ProviderKind::VLlm,
        ProviderKind::LlamaCpp,
        ProviderKind::LiteLlm,
        ProviderKind::OpenAICompat,
    ];
    let probe_kinds: Vec<ProviderKind> = ENDPOINT_OWNED
        .iter()
        .copied()
        .filter(|k| {
            matches!(k, ProviderKind::Ollama)
                || k.endpoint_env()
                    .and_then(|v| std::env::var(v).ok())
                    .is_some_and(|s| !s.trim().is_empty())
        })
        .collect();
    let endpoint_live: std::collections::HashMap<ProviderKind, Vec<String>> =
        futures::future::join_all(probe_kinds.into_iter().map(|k| async move {
            let ms = if matches!(k, ProviderKind::Ollama) {
                800
            } else {
                1500
            };
            (k, live_model_ids(k, ms).await)
        }))
        .await
        .into_iter()
        .filter_map(|(k, ids)| ids.map(|ids| (k, ids)))
        .collect();
    let ollama_live: Vec<String> = endpoint_live
        .get(&ProviderKind::Ollama)
        .cloned()
        .unwrap_or_default();
    let opencodego_live: Vec<String> = {
        let key = std::env::var("OPENCODE_GO_API_KEY").ok();
        match key {
            Some(k) => {
                let base = std::env::var("OPENCODE_GO_BASE_URL")
                    .unwrap_or_else(|_| crate::providers::opencode_go::DEFAULT_API_URL.to_string());
                let provider =
                    crate::providers::opencode_go::OpencodeGoProvider::new(k).with_base_url(base);
                match tokio::time::timeout(
                    std::time::Duration::from_millis(1500),
                    provider.list_models(),
                )
                .await
                {
                    Ok(Ok(models)) => models.into_iter().map(|m| m.id).collect(),
                    _ => Vec::new(),
                }
            }
            None => Vec::new(),
        }
    };
    // Each entry carries a sort rank so Featured providers list first
    // (in FEATURED_ORDER), then Additional providers in ALL order.
    let mut groups: Vec<(u32, serde_json::Value)> = Vec::new();
    for (all_idx, kind) in ProviderKind::ALL.iter().enumerate() {
        let name = kind.name();
        // Hide `codex/` (OpenAIResponses) from the picker for now: it 401s in
        // hosted pods (no gateway overlay yet — metering-audit follow-up) and
        // overlaps the plain openai `gpt-5-codex` rows + the `chatgpt-codex/`
        // subscription path. Routing still works via an explicit
        // `/model codex/<id>`; this only drops it from the listed options.
        if matches!(kind, ProviderKind::OpenAIResponses) {
            continue;
        }
        // Hosted multiuser pods have NO BYOK — a non-routable provider or an
        // unpriced model can't be served there, so hide them. On desktop we
        // show EVERYTHING (BYOK works for any model) and tag each row with
        // `featured` so the UI can decide where the proxy switch applies.
        let pod = crate::workdir::is_multiuser();
        if pod && kind.tier() != ProviderTier::Featured {
            continue;
        }
        // Issue #215: a provider the user cannot actually reach has no
        // business in the picker. This listed all ~30 kinds fully populated
        // from the shipped catalogue, so someone holding a single key scrolled
        // past hundreds of models they would get a 401 from — the one they
        // could use was the hard one to find.
        //
        // `kind_has_credentials` is the same predicate the GUI's auto-fallback
        // already trusts, and it is not merely "is there an API key": keyless
        // local runtimes (Ollama, vLLM, llama.cpp, LM Studio, OpenAI-compatible)
        // and the file-OAuth Codex path all answer true on their own terms.
        // The gateway arm matters just as much — a gateway-routed provider is
        // reachable with no local key at all, and dropping it here would take
        // away the models hosted users have.
        if !kind_is_reachable(&app_cfg, *kind) {
            continue;
        }
        let provider_featured = kind.tier() == ProviderTier::Featured;
        // (id) -> (context, featured, context_unverified). `featured` =
        // gateway-servable: a Featured-tier provider with a priced catalogue
        // entry. `context_unverified` marks a window that is the provider's
        // blanket default rather than a published figure (dev-plan/57) — the
        // live rows appended below have no catalogue entry at all, so they
        // carry no window and nothing to qualify.
        let mut model_ids: std::collections::BTreeMap<String, (Option<u32>, bool, bool)> =
            std::collections::BTreeMap::new();
        let is_openrouter = matches!(kind, ProviderKind::OpenRouter);
        for (id, entry) in cat.list_models_for_provider(name) {
            if entry.chat == Some(false) {
                continue;
            }
            if is_openrouter && free_only_or && entry.free != Some(true) {
                continue;
            }
            let priced = entry.input_per_mtok.is_some() && entry.output_per_mtok.is_some();
            // In a pod, unpriced rows would 400 (strictly metered) — hide them.
            if pod && !priced {
                continue;
            }
            let canonical = crate::model_catalogue::canonical_model_id(name, &id);
            model_ids.insert(
                canonical,
                (
                    entry.context,
                    provider_featured && priced,
                    entry.context_unverified(),
                ),
            );
        }
        // The endpoint is authoritative: keep only what it reports, and let
        // the catalogue contribute the context window for the ids that
        // survive. A row cached from an earlier `/model` switch no longer
        // resurrects a model the proxy has since dropped.
        if let Some(live) = endpoint_live.get(kind) {
            let catalogued = std::mem::take(&mut model_ids);
            for id in live {
                let canonical = crate::model_catalogue::canonical_model_id(name, id);
                let meta = catalogued
                    .get(&canonical)
                    .copied()
                    .unwrap_or((None, false, false));
                model_ids.insert(canonical, meta);
            }
        }
        if matches!(kind, ProviderKind::OpenCodeGo) {
            for id in &opencodego_live {
                model_ids.entry(id.clone()).or_insert((None, false, false));
            }
        }
        if model_ids.is_empty() {
            continue;
        }
        let model_rows: Vec<serde_json::Value> = model_ids
            .into_iter()
            .map(|(id, (ctx, featured, ctx_unverified))| {
                serde_json::json!({
                    "id": id,
                    "context": ctx,
                    "context_unverified": ctx_unverified,
                    "featured": featured,
                })
            })
            .collect();
        let tier = kind.tier();
        let rank = match tier {
            ProviderTier::Featured => ProviderKind::FEATURED_ORDER
                .iter()
                .position(|p| p == kind)
                .map(|p| p as u32)
                .unwrap_or(99),
            // Additional providers sort after every Featured one, keeping
            // their relative ALL order.
            ProviderTier::Additional => 100 + all_idx as u32,
        };
        groups.push((
            rank,
            serde_json::json!({
                "provider": name,
                "tier": tier.as_str(),
                "models": model_rows,
            }),
        ));
    }
    groups.sort_by_key(|(rank, _)| *rank);
    let groups: Vec<serde_json::Value> = groups.into_iter().map(|(_, g)| g).collect();
    serde_json::json!({
        "type": "all_models_list",
        "groups": groups,
        "ollama_reachable": !ollama_live.is_empty(),
    })
    .to_string()
}

/// Pick the default model for the highest-priority provider the user
/// actually has usable credentials for — their own API key (env or
/// keychain) **or** a gateway route — scanning in the order
/// DashScope → OpenAI → Anthropic. Used at startup / new-session to
/// replace the compiled-in Anthropic placeholder when the user hasn't
/// explicitly pinned a model, so a fresh install with (say) only a
/// DashScope key lands on DashScope instead of an unconfigured
/// Anthropic. Returns `None` when none of the three are configured, in
/// which case the caller keeps the compiled-in default.
///
/// Distinct from `build_provider_with_fallback` (repl.rs), which swaps to
/// a probed-reachable local runtime — in memory only — after the
/// configured provider fails to build; this picks the preferred *paid*
/// default when nothing is configured yet.
/// Model ids that have left the catalogue, and what to use instead.
///
/// A persisted `model` counts as "explicit" at load, so the credential-aware
/// default never re-runs for an existing workspace — a retired id would sit in
/// `.thclaws/settings.json` forever. That is not a cosmetic problem: a model
/// with no priced catalogue entry is not gateway-servable, so
/// `gateway_overlay_for_model` returns `None`, routing falls through to BYOK,
/// and a gateway user with no keys of their own gets "No keys" on a workspace
/// that worked yesterday.
///
/// Both the bare id and the `deepseek/`-prefixed spelling are listed. They fail
/// differently — the bare one matches no catalogue entry at all, while the
/// prefixed one still resolves to OpenRouter's copy of the same retired model
/// and would route to a DeepSeek endpoint that no longer serves it — so both
/// need repointing.
pub const RETIRED_MODELS: &[(&str, &str)] = &[
    // DeepSeek retired the V4 flash line from api.deepseek.com (it left the
    // `deepseek` catalogue block on 2026-09-10); `deepseek-flash` is the
    // successor and is priced, hence gateway-servable.
    ("deepseek-v4-flash", "deepseek-flash"),
    ("deepseek/deepseek-v4-flash", "deepseek-flash"),
];

/// The successor for a retired model id, or `None` when `model` is still good.
/// Case-insensitive to match the rest of the model-id handling here.
pub fn retired_model_successor(model: &str) -> Option<&'static str> {
    RETIRED_MODELS
        .iter()
        .find(|(old, _)| model.eq_ignore_ascii_case(old))
        .map(|(_, new)| *new)
}

pub fn preferred_default_model(cfg: &crate::config::AppConfig) -> Option<String> {
    // On a DGX Spark appliance the box *is* the provider: AI Server's
    // gateway is already running on loopback and needs no credentials, so it
    // outranks any cloud tier below. Read-only — the probe happened once at
    // startup (`aiserver::bootstrap`), and only when nothing had pinned
    // `LITELLM_BASE_URL`, so this cannot override a configured provider.
    if let Some(d) = crate::aiserver::cached() {
        return Some(d.model_id());
    }
    // Ordered (provider, model) preference: the first provider the user can
    // reach — own key OR a gateway route — picks the session default. Models
    // are pinned explicitly (not `kind.default_model()`) so the credential-
    // aware default can prefer a specific tier per provider independent of
    // each provider's standalone default. All four are priced in the
    // catalogue, so they're gateway-servable for proxied sessions.
    const ORDER: &[(ProviderKind, &str)] = &[
        (ProviderKind::DeepSeek, "deepseek-flash"),
        (ProviderKind::DashScope, "dashscope/qwen3.7-max"),
        (ProviderKind::OpenAI, "gpt-5.5"),
        (ProviderKind::Anthropic, "claude-sonnet-4-6"),
    ];
    for (kind, model) in ORDER {
        let has_key = kind_has_credentials(Some(*kind));
        let via_gateway = crate::providers::thclaws_gateway::for_kind(cfg, *kind).is_some();
        if has_key || via_gateway {
            return Some((*model).to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {

    /// Issue #215: the picker listed every provider kind fully populated from
    /// the shipped catalogue, so a user with one key scrolled past hundreds of
    /// models that would 401. It now lists only what the user can reach.
    ///
    /// The strict direction is the dangerous one — a predicate that is too
    /// harsh makes working providers disappear — so this pins both sides:
    /// a keyed provider without its key is out, and a keyless local runtime
    /// stays in.
    #[test]
    fn the_picker_lists_only_providers_the_user_can_reach() {
        let _g = crate::kms::test_env_lock();
        let cfg = crate::config::AppConfig::default();

        let key_var = ProviderKind::Anthropic
            .api_key_env()
            .expect("anthropic is key-authenticated");
        let prev = std::env::var(key_var).ok();

        std::env::remove_var(key_var);
        assert!(
            !super::kind_is_reachable(&cfg, ProviderKind::Anthropic),
            "a key-authenticated provider with no key must not be listed"
        );

        std::env::set_var(key_var, "sk-test");
        assert!(
            super::kind_is_reachable(&cfg, ProviderKind::Anthropic),
            "and must come back the moment the key is set"
        );
        std::env::remove_var(key_var);

        // Keyless by nature: reachability is the gate, not a credential.
        // These are the ones a naive "has an API key?" check would wrongly
        // hide, leaving local-model users with an empty picker.
        for kind in [
            ProviderKind::Ollama,
            ProviderKind::LMStudio,
            ProviderKind::VLlm,
            ProviderKind::LlamaCpp,
            ProviderKind::OpenAICompat,
            ProviderKind::LiteLlm,
        ] {
            assert!(
                super::kind_is_reachable(&cfg, kind),
                "{} needs no key and must stay listed",
                kind.name()
            );
        }

        match prev {
            Some(v) => std::env::set_var(key_var, v),
            None => std::env::remove_var(key_var),
        }
    }
    /// `/models` on a user-pointed provider says which endpoint it asked,
    /// because "no models for openai-compat" tells the user nothing about
    /// the box they actually configured.
    #[test]
    fn endpoint_hint_names_the_configured_url() {
        let _g = crate::kms::test_env_lock();
        std::env::remove_var("OPENAI_COMPAT_BASE_URL");
        let d = super::endpoint_hint(ProviderKind::OpenAICompat);
        assert!(d.contains("localhost:8000"), "default shown: {d}");
        assert!(d.contains("OPENAI_COMPAT_BASE_URL"), "names the var: {d}");

        std::env::set_var("OPENAI_COMPAT_BASE_URL", "http://vllm.internal:9000/v1");
        let set = super::endpoint_hint(ProviderKind::OpenAICompat);
        assert!(set.contains("vllm.internal:9000"), "got: {set}");
        std::env::remove_var("OPENAI_COMPAT_BASE_URL");

        // A catalogued provider has no user endpoint — fall back to its name.
        assert_eq!(super::endpoint_hint(ProviderKind::Anthropic), "anthropic");
    }

    use super::*;

    // `codex/` (OpenAIResponses) models are hidden from the cross-provider
    // picker (they 401 in hosted + overlap plain openai codex rows). The
    // catalogue's `openai-responses` block keys are `codex/gpt-5-codex` etc.,
    // so a regression that drops the filter would resurface `codex/` here.
    #[tokio::test]
    async fn picker_hides_codex_openai_responses_models() {
        let payload = build_all_models_payload().await;
        assert!(
            !payload.contains("codex/gpt-5"),
            "codex/ (OpenAIResponses) models must not appear in the model picker payload"
        );
    }

    #[test]
    fn accumulate_sums_all_token_fields_including_reasoning() {
        let mut acc = Usage::default();
        acc.accumulate(&Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_input_tokens: Some(5),
            cache_read_input_tokens: None,
            reasoning_output_tokens: Some(7),
        });
        acc.accumulate(&Usage {
            input_tokens: 50,
            output_tokens: 10,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(8),
            reasoning_output_tokens: Some(3),
        });
        assert_eq!(acc.input_tokens, 150);
        assert_eq!(acc.output_tokens, 30);
        assert_eq!(acc.cache_creation_input_tokens, Some(5));
        assert_eq!(acc.cache_read_input_tokens, Some(8));
        // The bug: reasoning tokens used to be dropped → would stay None.
        assert_eq!(acc.reasoning_output_tokens, Some(10));
    }

    /// Regression: a provider routed through the thClaws gateway (the
    /// per-provider proxy toggle on + a CLI/gateway token present) must
    /// count as "having credentials" even with no local API key, so the
    /// sidebar stops showing "no API key" for a working proxied provider.
    /// Test isolation: serialise the `THCLAWS_GATEWAY_API_KEY` mutation
    /// so a sibling env-reading test doesn't see ghost state.
    #[test]
    fn provider_has_credentials_honors_gateway_route() {
        // Share the one lock with the other tests that mutate
        // THCLAWS_GATEWAY_API_KEY / provider-key env vars — distinct mutexes
        // would let them race and intermittently clear each other's state.
        let _guard = PREF_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = crate::config::AppConfig::default();
        // Detects as Gemini; segment "google" is the gateway key.
        cfg.model = "gemini-2.5-flash".to_string();
        cfg.gateway_use_for = vec!["google".to_string()];
        std::env::set_var("THCLAWS_GATEWAY_API_KEY", "gw_v1_test");
        assert!(
            provider_has_credentials(&cfg),
            "gateway toggle + token → ready, no local key needed"
        );
        std::env::remove_var("THCLAWS_GATEWAY_API_KEY");
    }

    /// `set_stream_chunk_timeout_secs` must be reflected by the
    /// next `stream_chunk_timeout()` call — the providers read this
    /// on every chunk-await, so a config reload that drops the
    /// timeout from 120 s to 60 s should take effect immediately.
    /// Test isolation: snapshot + restore the global so concurrent
    /// tests aren't affected (the live atomic is process-wide).
    #[test]
    fn stream_chunk_timeout_setter_round_trips() {
        let prev = STREAM_CHUNK_TIMEOUT_SECS.load(std::sync::atomic::Ordering::Relaxed);
        set_stream_chunk_timeout_secs(60);
        assert_eq!(stream_chunk_timeout(), std::time::Duration::from_secs(60));
        set_stream_chunk_timeout_secs(300);
        assert_eq!(stream_chunk_timeout(), std::time::Duration::from_secs(300));
        // Restore so other tests aren't poisoned.
        STREAM_CHUNK_TIMEOUT_SECS.store(prev, std::sync::atomic::Ordering::Relaxed);
    }

    /// `0` means "use the default" (matches the `serde(default)` fallback
    /// for an absent settings key). Without this guard a misconfigured
    /// `stream_chunk_timeout_secs: 0` in settings.json would make every
    /// chunk-await time out instantly — the worst possible UX.
    #[test]
    fn stream_chunk_timeout_zero_falls_back_to_default() {
        let prev = STREAM_CHUNK_TIMEOUT_SECS.load(std::sync::atomic::Ordering::Relaxed);
        set_stream_chunk_timeout_secs(0);
        assert_eq!(stream_chunk_timeout(), std::time::Duration::from_secs(120));
        STREAM_CHUNK_TIMEOUT_SECS.store(prev, std::sync::atomic::Ordering::Relaxed);
    }

    /// M6.21 BUG H1: `find_bytes` must locate `\n\n` (and other
    /// boundaries) on raw byte slices, allowing providers to buffer
    /// chunks as `Vec<u8>` rather than `String::from_utf8_lossy(&chunk)`
    /// per-chunk (which corrupts multi-byte UTF-8 chars at TCP packet
    /// boundaries). The fix's correctness hinges on this helper
    /// returning the same byte index `str::find` would for the same
    /// content.
    #[test]
    fn find_bytes_locates_sse_and_ndjson_boundaries() {
        // Empty needle → None
        assert_eq!(find_bytes(b"hello", b""), None);
        // Needle larger than haystack → None
        assert_eq!(find_bytes(b"hi", b"hello"), None);
        // Needle absent → None
        assert_eq!(find_bytes(b"hello world", b"\n\n"), None);
        // Standard SSE boundary
        assert_eq!(find_bytes(b"data: {}\n\nmore", b"\n\n"), Some(8));
        // CRLF SSE boundary (Gemini)
        assert_eq!(find_bytes(b"data: {}\r\n\r\nmore", b"\r\n\r\n"), Some(8));
        // NDJSON boundary
        assert_eq!(find_bytes(b"{\"a\":1}\n{\"b\":2}", b"\n"), Some(7));
        // First occurrence wins
        assert_eq!(find_bytes(b"a\n\nb\n\nc", b"\n\n"), Some(1));
    }

    /// M6.21 BUG H1: regression test for the actual UTF-8 corruption
    /// scenario. A multi-byte UTF-8 char split across two byte chunks
    /// must round-trip cleanly when reassembled via the byte-buffer
    /// pattern; pre-fix `from_utf8_lossy(&chunk)` per-chunk produced
    /// U+FFFD pairs.
    #[test]
    fn byte_buffer_preserves_utf8_split_across_chunks() {
        // The Thai char ก (U+0E01) encodes as 0xE0 0xB8 0x81 — 3 bytes.
        // SSE event `data: {"text":"ก"}\n\n` split between bytes 16 and 17
        // (mid-Thai-char):
        let chunk1: &[u8] = &[
            b'd', b'a', b't', b'a', b':', b' ', b'{', b'"', b't', b'e', b'x', b't', b'"', b':',
            b'"', 0xE0, 0xB8, // first 2 bytes of ก
        ];
        let chunk2: &[u8] = &[
            0x81, b'"', b'}', b'\n', b'\n', // last byte of ก + closing
        ];

        // PRE-FIX equivalent: from_utf8_lossy each chunk, push to String
        let mut bad_buffer = String::new();
        bad_buffer.push_str(&String::from_utf8_lossy(chunk1));
        bad_buffer.push_str(&String::from_utf8_lossy(chunk2));
        assert!(
            bad_buffer.contains('\u{FFFD}'),
            "pre-fix path must produce U+FFFD chars (got: {bad_buffer:?})"
        );
        assert!(
            !bad_buffer.contains('ก'),
            "pre-fix path corrupts ก into replacement chars"
        );

        // POST-FIX path: byte buffer, decode at boundary
        let mut good_buffer: Vec<u8> = Vec::new();
        good_buffer.extend_from_slice(chunk1);
        good_buffer.extend_from_slice(chunk2);
        let boundary = find_bytes(&good_buffer, b"\n\n").expect("event boundary present");
        let event_bytes = &good_buffer[..boundary + 2];
        let event_text = String::from_utf8_lossy(event_bytes);
        assert!(
            event_text.contains('ก'),
            "post-fix path preserves ก (got: {event_text:?})"
        );
        assert!(
            !event_text.contains('\u{FFFD}'),
            "post-fix path produces no replacement chars"
        );
    }

    /// Provider-aware alias resolution must keep the alias inside the
    /// caller's namespace. The whole point is to stop a passive agent-def
    /// load (`model: sonnet`) from surprise-switching the team to native
    /// Anthropic when the project chose OpenRouter.
    #[test]
    fn resolve_alias_for_provider_stays_in_namespace() {
        // OpenRouter project → Anthropic-family aliases stay on OpenRouter.
        assert_eq!(
            ProviderKind::resolve_alias_for_provider("sonnet", ProviderKind::OpenRouter).as_deref(),
            Some("openrouter/anthropic/claude-sonnet-4-6"),
        );
        assert_eq!(
            ProviderKind::resolve_alias_for_provider("opus", ProviderKind::OpenRouter).as_deref(),
            Some("openrouter/anthropic/claude-opus-4-6"),
        );
        assert_eq!(
            ProviderKind::resolve_alias_for_provider("flash", ProviderKind::OpenRouter).as_deref(),
            Some("openrouter/google/gemini-2.5-flash"),
        );

        // Native Anthropic project → no prefix.
        assert_eq!(
            ProviderKind::resolve_alias_for_provider("sonnet", ProviderKind::Anthropic).as_deref(),
            Some("claude-sonnet-4-6"),
        );

        // Native Gemini project → flash resolves natively, sonnet doesn't.
        assert_eq!(
            ProviderKind::resolve_alias_for_provider("flash", ProviderKind::Gemini).as_deref(),
            Some("gemini-2.5-flash"),
        );
        assert_eq!(
            ProviderKind::resolve_alias_for_provider("sonnet", ProviderKind::Gemini),
            None,
        );

        // Providers with no alias notion return None — caller falls back
        // to default config rather than surprise-switching providers.
        assert!(ProviderKind::resolve_alias_for_provider("sonnet", ProviderKind::OpenAI).is_none());
        assert!(ProviderKind::resolve_alias_for_provider("sonnet", ProviderKind::Ollama).is_none());
        assert!(
            ProviderKind::resolve_alias_for_provider("sonnet", ProviderKind::DashScope).is_none()
        );
        assert!(
            ProviderKind::resolve_alias_for_provider("sonnet", ProviderKind::DeepSeek).is_none()
        );

        // DeepSeek model IDs are bare and detected by the `deepseek-` prefix.
        assert_eq!(
            ProviderKind::detect("deepseek-chat"),
            Some(ProviderKind::DeepSeek)
        );
        assert_eq!(
            ProviderKind::detect("deepseek-reasoner"),
            Some(ProviderKind::DeepSeek)
        );

        // Non-aliases pass through as None — they don't need translation.
        assert!(ProviderKind::resolve_alias_for_provider(
            "claude-opus-4-7",
            ProviderKind::OpenRouter
        )
        .is_none());
    }

    #[test]
    fn alias_lookup_is_case_insensitive_for_thaillm_and_anthropic() {
        // ThaiLLM model aliases — the canonical model id has mixed
        // casing (OpenThaiGPT, THaLLE), so the alias table must accept
        // any casing the user types. Resolved id keeps upstream casing.
        assert_eq!(
            ProviderKind::resolve_alias("OpenThaiGPT"),
            "thaillm/OpenThaiGPT-ThaiLLM-8B-Instruct-v7.2"
        );
        assert_eq!(
            ProviderKind::resolve_alias("openthaigpt"),
            "thaillm/OpenThaiGPT-ThaiLLM-8B-Instruct-v7.2"
        );
        assert_eq!(
            ProviderKind::resolve_alias("OPENTHAIGPT"),
            "thaillm/OpenThaiGPT-ThaiLLM-8B-Instruct-v7.2"
        );
        assert_eq!(
            ProviderKind::resolve_alias("THaLLE"),
            "thaillm/THaLLE-0.2-ThaiLLM-8B-fa"
        );
        assert_eq!(
            ProviderKind::resolve_alias("typhoon"),
            "thaillm/Typhoon-S-ThaiLLM-8B-Instruct"
        );
        assert_eq!(
            ProviderKind::resolve_alias("Pathumma"),
            "thaillm/Pathumma-ThaiLLM-qwen3-8b-think-3.0.0"
        );

        // Existing Anthropic / Google aliases still resolve, including
        // mixed casing — proves the lowercase fold doesn't regress them.
        assert_eq!(ProviderKind::resolve_alias("Sonnet"), "claude-sonnet-4-6");
        assert_eq!(ProviderKind::resolve_alias("FLASH"), "gemini-2.5-flash");

        // Unknown input passes through with original casing intact —
        // upstream model ids are case-sensitive, so we must NOT lowercase
        // the returned id when there's no alias hit.
        assert_eq!(
            ProviderKind::resolve_alias("Custom-Model-V2"),
            "Custom-Model-V2"
        );
    }

    #[test]
    fn alias_for_provider_only_resolves_within_correct_provider() {
        // `openthaigpt` resolves only when current provider is ThaiLLM —
        // SpawnTeammate uses this to keep a worktree on its parent
        // provider rather than surprise-switching mid-team.
        assert_eq!(
            ProviderKind::resolve_alias_for_provider("openthaigpt", ProviderKind::ThaiLLM)
                .as_deref(),
            Some("thaillm/OpenThaiGPT-ThaiLLM-8B-Instruct-v7.2")
        );
        assert!(
            ProviderKind::resolve_alias_for_provider("OpenThaiGPT", ProviderKind::Anthropic)
                .is_none()
        );
        assert!(
            ProviderKind::resolve_alias_for_provider("sonnet", ProviderKind::ThaiLLM).is_none()
        );
    }

    #[test]
    fn sis_is_byok_only_and_has_no_shared_default_endpoint() {
        // `sis/` routes to the workspace endpoint; bare qwen ids keep
        // going to mainland DashScope, so the two stay distinguishable
        // the same way `qc/` does.
        assert_eq!(
            ProviderKind::detect("sis/qwen3.8-flash"),
            Some(ProviderKind::Sis)
        );
        assert_eq!(
            ProviderKind::detect("qwen-max"),
            Some(ProviderKind::DashScope),
            "bare qwen-* must not be captured by the sis prefix"
        );
        assert_eq!(ProviderKind::Sis.name(), "sis");
        assert_eq!(ProviderKind::Sis.api_key_env(), Some("SIS_API_KEY"));
        assert_eq!(ProviderKind::Sis.endpoint_env(), Some("SIS_BASE_URL"));

        // The load-bearing one. A SIS host embeds a workspace id, so a
        // shared default would route one account's traffic into
        // another's workspace. `build_provider` refuses without
        // SIS_BASE_URL; this pins the absence so nobody "fixes" it by
        // adding a plausible-looking URL.
        assert_eq!(
            ProviderKind::Sis.default_endpoint(),
            None,
            "SIS endpoints are per-workspace — there is no default to share"
        );

        // Additional, not Featured: the metered gateway sells the ten
        // Featured providers and dropped the regional Alibaba siblings
        // on 2026-08-10. SIS is BYOK, like QwenCloud.
        assert_eq!(ProviderKind::Sis.tier(), ProviderTier::Additional);
        assert!(
            !ProviderKind::FEATURED_ORDER.contains(&ProviderKind::Sis),
            "SIS must not appear in the Featured order"
        );
    }

    #[test]
    fn detect_qc_prefix_routes_to_qwen_cloud_provider() {
        // `qc/` prefix is the short routing tag for Alibaba's
        // Singapore-region DashScope. Bare `qwen-*` (no prefix) still
        // routes to mainland DashScope so the two regions stay
        // explicitly distinguishable.
        assert_eq!(
            ProviderKind::detect("qc/qwen-max"),
            Some(ProviderKind::QwenCloud)
        );
        assert_eq!(
            ProviderKind::detect("qc/qwen-plus"),
            Some(ProviderKind::QwenCloud)
        );
        assert_eq!(
            ProviderKind::detect("qwen-max"),
            Some(ProviderKind::DashScope),
            "bare qwen-* still routes to mainland DashScope"
        );
        assert_eq!(
            ProviderKind::QwenCloud.api_key_env(),
            Some("QWENCLOUD_API_KEY")
        );
        assert_eq!(
            ProviderKind::QwenCloud.endpoint_env(),
            Some("QWENCLOUD_BASE_URL")
        );
        assert_eq!(
            ProviderKind::QwenCloud.default_endpoint(),
            Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1")
        );
        assert_eq!(ProviderKind::QwenCloud.name(), "qwen-cloud");
        assert_eq!(ProviderKind::QwenCloud.default_model(), "qc/qwen-max");
    }

    #[test]
    fn detect_self_hosted_runtimes_split_out_of_openai_compat() {
        // vLLM and llama.cpp used to share the generic `oai/` slot. They now
        // carry their own namespace so each gets its own base URL and shows
        // up separately in Settings; `oai/` keeps working for everything else.
        assert_eq!(
            ProviderKind::detect("vllm/Qwen/Qwen3-8B"),
            Some(ProviderKind::VLlm),
            "an HF-path served id keeps its inner slashes"
        );
        assert_eq!(
            ProviderKind::detect("llamacpp/local-model"),
            Some(ProviderKind::LlamaCpp)
        );
        assert_eq!(
            ProviderKind::detect("oai/whatever"),
            Some(ProviderKind::OpenAICompat),
            "the generic compat endpoint is unchanged"
        );

        for kind in [ProviderKind::VLlm, ProviderKind::LlamaCpp] {
            // Self-hosted: no key to check, editable endpoint, and never a
            // gateway route (provider_segment falls through to None).
            assert_eq!(kind.api_key_env(), None);
            assert!(kind.endpoint_user_configurable());
            assert!(kind_has_credentials(Some(kind)));
            assert!(ProviderKind::ALL.contains(&kind));
        }
        assert_eq!(ProviderKind::VLlm.name(), "vllm");
        assert_eq!(ProviderKind::VLlm.endpoint_env(), Some("VLLM_BASE_URL"));
        assert_eq!(
            ProviderKind::VLlm.default_endpoint(),
            Some("http://localhost:8000/v1")
        );
        assert_eq!(ProviderKind::LlamaCpp.name(), "llamacpp");
        assert_eq!(
            ProviderKind::LlamaCpp.endpoint_env(),
            Some("LLAMACPP_BASE_URL")
        );
        assert_eq!(
            ProviderKind::LlamaCpp.default_endpoint(),
            Some("http://localhost:8080/v1")
        );
    }

    /// The set of providers whose *endpoint* owns the model list. Getting
    /// this wrong in either direction is a real bug: a missing kind keeps
    /// showing models the box dropped, an extra one throws away a shipped
    /// catalogue in favour of whatever a hosted API happens to enumerate.
    #[test]
    fn lists_from_endpoint_covers_user_pointed_backends() {
        for k in [
            ProviderKind::Ollama,
            ProviderKind::OllamaAnthropic,
            ProviderKind::LMStudio,
            ProviderKind::VLlm,
            ProviderKind::LlamaCpp,
            ProviderKind::LiteLlm,
            ProviderKind::OpenAICompat,
        ] {
            assert!(
                k.lists_from_endpoint(),
                "{} points at a user endpoint",
                k.name()
            );
        }
        for k in [
            ProviderKind::Anthropic,
            ProviderKind::OpenAI,
            ProviderKind::OpenRouter,
            ProviderKind::Gemini,
            ProviderKind::DashScope,
            // Hosted, and its live list is probed separately in the picker.
            ProviderKind::OpenCodeGo,
        ] {
            assert!(
                !k.lists_from_endpoint(),
                "{} ships a catalogue — don't let a listing replace it",
                k.name()
            );
        }
    }

    /// `build_listing_provider` must reach the configured box, and must
    /// re-prefix ids so a copied row pastes straight into `/model`.
    #[tokio::test]
    async fn live_model_ids_prefixes_and_is_authoritative() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    {"id": "qwen3.8-flash-next-udq2", "object": "model"},
                    {"id": "auto", "object": "model"},
                ]
            })))
            .mount(&server)
            .await;

        let _guard = crate::kms::test_env_lock();
        let saved = std::env::var("LITELLM_BASE_URL").ok();
        std::env::set_var("LITELLM_BASE_URL", format!("{}/v1", server.uri()));
        let ids = live_model_ids(ProviderKind::LiteLlm, 5000).await;
        match saved {
            Some(v) => std::env::set_var("LITELLM_BASE_URL", v),
            None => std::env::remove_var("LITELLM_BASE_URL"),
        }

        let mut ids = ids.expect("endpoint reachable");
        ids.sort();
        assert_eq!(ids, vec!["litellm/auto", "litellm/qwen3.8-flash-next-udq2"]);
    }

    /// An unreachable endpoint must report "unknown" (`None`), never an
    /// empty list — the callers keep their catalogue rows on `None` and
    /// would otherwise render a provider as having no models at all.
    #[tokio::test]
    async fn live_model_ids_is_none_when_endpoint_is_down() {
        let _guard = crate::kms::test_env_lock();
        let saved = std::env::var("LITELLM_BASE_URL").ok();
        // Reserved-for-documentation address: nothing listens, connect fails.
        std::env::set_var("LITELLM_BASE_URL", "http://127.0.0.1:1/v1");
        let ids = live_model_ids(ProviderKind::LiteLlm, 2000).await;
        match saved {
            Some(v) => std::env::set_var("LITELLM_BASE_URL", v),
            None => std::env::remove_var("LITELLM_BASE_URL"),
        }
        assert!(
            ids.is_none(),
            "unreachable endpoint is 'unknown', not 'empty'"
        );
    }

    /// The bug this whole change exists for: `/model litellm/x` caches a
    /// probed context window under the `litellm` provider key, but the
    /// endpoint is per-machine. Once the operator swapped models on the
    /// proxy, that cached row kept advertising a model `GET /v1/models` no
    /// longer returns — in `/models`, in the sidebar picker, everywhere.
    #[tokio::test]
    async fn picker_payload_drops_cached_models_the_endpoint_no_longer_serves() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    {"id": "qwen3.8-flash-next-udq2", "object": "model"},
                    {"id": "auto", "object": "model"},
                ]
            })))
            .mount(&server)
            .await;

        let home = tempfile::tempdir().expect("tempdir");
        let cat_dir = home.path().join("thclaws");
        std::fs::create_dir_all(&cat_dir).expect("mkdir");
        // A cache exactly like the one that shipped this report: `auto` is
        // still served, `qwen38-ud-q2-mtp` is long gone, and both were
        // written by `/model` probes against different base URLs.
        std::fs::write(
            cat_dir.join("model_catalogue.json"),
            r#"{
                "schema": 4,
                "providers": {
                    "litellm": {
                        "models": {
                            "litellm/auto": {"context": 262144, "source": "probe"},
                            "litellm/qwen38-ud-q2-mtp": {"context": 262144, "source": "probe"}
                        }
                    }
                },
                "fallback": 128000
            }"#,
        )
        .expect("write cache");

        let _guard = crate::kms::test_env_lock();
        let saved_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let saved_base = std::env::var("LITELLM_BASE_URL").ok();
        std::env::set_var("XDG_CONFIG_HOME", home.path());
        std::env::set_var("LITELLM_BASE_URL", format!("{}/v1", server.uri()));

        let mut cfg = crate::config::AppConfig::default();
        cfg.model = "litellm/auto".to_string();
        let payload = build_model_picker_payload(&cfg, 2).await;

        match saved_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match saved_base {
            Some(v) => std::env::set_var("LITELLM_BASE_URL", v),
            None => std::env::remove_var("LITELLM_BASE_URL"),
        }

        let payload: serde_json::Value =
            serde_json::from_str(&payload.expect("picker opens for a reachable proxy"))
                .expect("valid json");
        let mut ids: Vec<&str> = payload["models"]
            .as_array()
            .expect("models array")
            .iter()
            .map(|m| m["id"].as_str().expect("id"))
            .collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec!["litellm/auto", "litellm/qwen3.8-flash-next-udq2"],
            "the endpoint's list is the whole list"
        );
        // …and the cache is still useful for the ids that survived.
        let auto = payload["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "litellm/auto")
            .expect("auto listed");
        assert_eq!(
            auto["context"].as_u64(),
            Some(262144),
            "a surviving id keeps its cached context window"
        );
    }

    /// Hosted providers have no endpoint to ask.
    #[test]
    fn build_listing_provider_declines_hosted_kinds() {
        assert!(build_listing_provider(ProviderKind::Anthropic).is_none());
        assert!(build_listing_provider(ProviderKind::OpenRouter).is_none());
    }

    #[test]
    fn litellm_is_a_self_hosted_proxy_with_optional_auth() {
        assert_eq!(
            ProviderKind::detect("litellm/gpt-4o-mini"),
            Some(ProviderKind::LiteLlm)
        );
        assert_eq!(
            ProviderKind::detect("litellm/azure/my-deployment"),
            Some(ProviderKind::LiteLlm),
            "a namespaced alias keeps its inner slashes"
        );
        assert_eq!(ProviderKind::LiteLlm.name(), "litellm");
        assert_eq!(
            ProviderKind::LiteLlm.endpoint_env(),
            Some("LITELLM_BASE_URL")
        );
        assert_eq!(
            ProviderKind::LiteLlm.default_endpoint(),
            Some("http://localhost:4000/v1")
        );
        assert!(ProviderKind::LiteLlm.endpoint_user_configurable());
        assert!(ProviderKind::ALL.contains(&ProviderKind::LiteLlm));
        // The key is optional — usable (and buildable) with none set.
        assert_eq!(ProviderKind::LiteLlm.api_key_env(), Some("LITELLM_API_KEY"));
        assert!(ProviderKind::LiteLlm.has_key_available());
        assert!(kind_has_credentials(Some(ProviderKind::LiteLlm)));
        // A router, not local inference: masking and the org-policy gateway
        // must keep treating its traffic as leaving the host.
        assert!(!ProviderKind::LiteLlm.is_local());
        assert_eq!(
            crate::providers::thclaws_gateway::provider_segment(ProviderKind::LiteLlm),
            None,
            "self-hosted — never metered through the thClaws gateway"
        );
    }

    // Serialises the env-var mutation in `preferred_default_model_*`
    // tests (api-key + gateway-key vars are process-global).
    static PREF_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn tier_classifies_featured_vs_additional() {
        // The 10 Featured (primary) providers.
        for k in [
            ProviderKind::OpenAI,
            ProviderKind::Anthropic,
            ProviderKind::Gemini,
            ProviderKind::XAi,
            ProviderKind::DeepSeek,
            ProviderKind::DashScope,
            ProviderKind::Moonshot,
            ProviderKind::ZAi,
            ProviderKind::Minimax,
            ProviderKind::OpenRouter,
        ] {
            assert_eq!(k.tier(), ProviderTier::Featured, "{k:?} should be Featured");
        }
        // Variants / regional siblings / local stay Additional.
        for k in [
            ProviderKind::OpenAIResponses,
            ProviderKind::ChatGptCodex,
            ProviderKind::AgentSdk,
            ProviderKind::AtlasCloud,
            ProviderKind::Unifically,
            ProviderKind::NineRouter,
            ProviderKind::QwenCloud,
            ProviderKind::ThaiLLM,
            ProviderKind::Nvidia,
            ProviderKind::Ollama,
            ProviderKind::OpenCodeGo,
        ] {
            assert_eq!(
                k.tier(),
                ProviderTier::Additional,
                "{k:?} should be Additional"
            );
        }
    }

    #[test]
    fn featured_order_matches_tier_set() {
        use std::collections::HashSet;
        let from_order: HashSet<ProviderKind> =
            ProviderKind::FEATURED_ORDER.iter().copied().collect();
        assert_eq!(
            from_order.len(),
            ProviderKind::FEATURED_ORDER.len(),
            "FEATURED_ORDER has duplicates"
        );
        let from_tier: HashSet<ProviderKind> = ProviderKind::ALL
            .iter()
            .copied()
            .filter(|k| k.tier() == ProviderTier::Featured)
            .collect();
        assert_eq!(
            from_order, from_tier,
            "FEATURED_ORDER must list exactly the Featured-tier providers"
        );
        assert_eq!(ProviderKind::FEATURED_ORDER.len(), 10);
    }

    #[test]
    fn display_ordered_is_featured_then_additional() {
        let ord = ProviderKind::display_ordered();
        // Every provider exactly once — no drops, no duplicates.
        assert_eq!(ord.len(), ProviderKind::ALL.len());
        let uniq: std::collections::HashSet<_> = ord.iter().copied().collect();
        assert_eq!(uniq.len(), ord.len());
        // Featured block first, in FEATURED_ORDER.
        let n = ProviderKind::FEATURED_ORDER.len();
        assert_eq!(&ord[..n], ProviderKind::FEATURED_ORDER);
        // Then every remaining entry is Additional.
        assert!(ord[n..]
            .iter()
            .all(|k| k.tier() == ProviderTier::Additional));
    }

    #[test]
    fn preferred_default_provider_models_match_requested() {
        assert_eq!(
            ProviderKind::DashScope.default_model(),
            "dashscope/qwen3.7-max"
        );
        assert_eq!(ProviderKind::OpenAI.default_model(), "gpt-4.1");
        assert_eq!(ProviderKind::Anthropic.default_model(), "claude-sonnet-4-6");
    }

    /// Every model `preferred_default_model` can hand back must have a priced
    /// catalogue entry, because that is exactly what makes it gateway-servable
    /// (`gateway_overlay_for_model` → `is_priced`). When `deepseek-v4-flash`
    /// left the `deepseek` catalogue block upstream this invariant broke
    /// silently: the default still pointed at it, routing found no price, fell
    /// through to BYOK, and gateway users with no keys of their own were told
    /// "No keys". Pin the invariant rather than the one id that broke it.
    #[test]
    fn every_preferred_default_is_priced_and_therefore_gateway_servable() {
        let cat = crate::model_catalogue::EffectiveCatalogue::load();
        for (kind, model) in [
            (ProviderKind::DeepSeek, "deepseek-flash"),
            (ProviderKind::DashScope, "dashscope/qwen3.7-max"),
            (ProviderKind::OpenAI, "gpt-5.5"),
            (ProviderKind::Anthropic, "claude-sonnet-4-6"),
        ] {
            assert!(
                cat.is_priced(model),
                "{kind:?} default {model} has no priced catalogue entry, so the \
                 gateway cannot serve it and a keyless user falls back to BYOK"
            );
        }
    }

    /// A workspace pinned to a retired id must land on the successor, in both
    /// the bare and the `deepseek/`-prefixed spelling, and the successor must
    /// itself still be a live id.
    #[test]
    fn retired_model_ids_repoint_to_a_live_successor() {
        assert_eq!(
            retired_model_successor("deepseek-v4-flash"),
            Some("deepseek-flash")
        );
        assert_eq!(
            retired_model_successor("deepseek/deepseek-v4-flash"),
            Some("deepseek-flash")
        );
        // Case-insensitive, like the rest of the model-id handling.
        assert_eq!(
            retired_model_successor("DeepSeek-V4-Flash"),
            Some("deepseek-flash")
        );
        // A live id is left alone.
        assert_eq!(retired_model_successor("deepseek-flash"), None);
        assert_eq!(retired_model_successor("gpt-5.5"), None);
        // No entry may point at another retired entry, or load would need to
        // iterate to reach a live id.
        for (_, successor) in RETIRED_MODELS {
            assert_eq!(
                retired_model_successor(successor),
                None,
                "{successor} is itself retired — RETIRED_MODELS must map straight \
                 to a live id"
            );
        }
    }

    #[test]
    fn preferred_default_model_follows_deepseek_dashscope_openai_anthropic_order() {
        let _guard = PREF_ENV_LOCK.lock().unwrap();
        // Isolate from any real provider keys in the host env so only the
        // gateway route under test decides the pick.
        for v in [
            "DEEPSEEK_API_KEY",
            "DASHSCOPE_API_KEY",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
        ] {
            std::env::remove_var(v);
        }
        std::env::set_var("THCLAWS_GATEWAY_API_KEY", "gw_v1_test");

        let mut cfg = crate::config::AppConfig::default();

        // Only OpenAI gateway-routed → the pinned OpenAI default.
        cfg.gateway_use_for = vec!["openai".into()];
        assert_eq!(preferred_default_model(&cfg).as_deref(), Some("gpt-5.5"));

        // DashScope outranks OpenAI when both are available.
        cfg.gateway_use_for = vec!["openai".into(), "dashscope".into()];
        assert_eq!(
            preferred_default_model(&cfg).as_deref(),
            Some("dashscope/qwen3.7-max")
        );

        // DeepSeek outranks everything when in the routed set.
        cfg.gateway_use_for = vec!["openai".into(), "dashscope".into(), "deepseek".into()];
        assert_eq!(
            preferred_default_model(&cfg).as_deref(),
            Some("deepseek-flash")
        );

        // None configured (no gateway route, host keys cleared) → None so
        // the caller keeps the compiled-in default.
        cfg.gateway_use_for = vec![];
        let out = preferred_default_model(&cfg);
        std::env::remove_var("THCLAWS_GATEWAY_API_KEY");
        assert!(out.is_none());
    }

    // The catalogue stores DashScope rows with a `dashscope/` routing
    // prefix so heterogeneous Alibaba-hosted families (qwen, deepseek,
    // glm, kimi, …) all route through one provider — the bare-id arms
    // alone would misroute `deepseek-v3.2` to DeepSeek even though it's
    // Alibaba-hosted on this provider. Bare `qwen-*` still routes for
    // backward compat with pre-prefix settings.
    #[test]
    fn detect_dashscope_prefix_routes_to_dashscope_provider() {
        assert_eq!(
            ProviderKind::detect("dashscope/qwen-max"),
            Some(ProviderKind::DashScope)
        );
        assert_eq!(
            ProviderKind::detect("dashscope/deepseek-v3.2"),
            Some(ProviderKind::DashScope),
            "Alibaba-hosted deepseek must route to DashScope, not the bare-`deepseek-` arm",
        );
        assert_eq!(
            ProviderKind::detect("dashscope/kimi-k2.6"),
            Some(ProviderKind::DashScope)
        );
        assert_eq!(
            ProviderKind::detect("qwen-max"),
            Some(ProviderKind::DashScope),
            "bare qwen-* still routes to DashScope for backward compat",
        );
        assert_eq!(
            ProviderKind::DashScope.default_model(),
            "dashscope/qwen3.7-max"
        );
    }

    #[test]
    fn detect_minimax_prefix_routes_to_minimax_provider() {
        assert_eq!(
            ProviderKind::detect("minimax/MiniMax-M2"),
            Some(ProviderKind::Minimax)
        );
        assert_eq!(
            ProviderKind::detect("minimax/MiniMax-M1"),
            Some(ProviderKind::Minimax)
        );
        assert_eq!(ProviderKind::Minimax.api_key_env(), Some("MINIMAX_API_KEY"));
        assert_eq!(
            ProviderKind::Minimax.default_endpoint(),
            Some("https://api.minimax.io/v1")
        );
        assert_eq!(ProviderKind::Minimax.name(), "minimax");
        assert_eq!(ProviderKind::Minimax.default_model(), "minimax/MiniMax-M3");
    }

    #[test]
    fn detect_unifically_prefix_routes_to_unifically_provider() {
        assert_eq!(
            ProviderKind::detect("unifically/google/gemini-3.5-flash"),
            Some(ProviderKind::Unifically)
        );
        assert_eq!(
            ProviderKind::detect("unifically/anthropic/claude-sonnet-5"),
            Some(ProviderKind::Unifically)
        );
        assert_eq!(
            ProviderKind::Unifically.api_key_env(),
            Some("UNIFICALLY_API_KEY")
        );
        assert_eq!(
            ProviderKind::Unifically.endpoint_env(),
            Some("UNIFICALLY_BASE_URL")
        );
        assert_eq!(
            ProviderKind::Unifically.default_endpoint(),
            Some("https://api.unifically.com/v1")
        );
        assert_eq!(ProviderKind::Unifically.name(), "unifically");
        assert_eq!(
            ProviderKind::Unifically.default_model(),
            "unifically/google/gemini-3.5-flash"
        );
    }

    #[test]
    fn detect_atlascloud_prefix_routes_to_atlascloud_provider() {
        assert_eq!(
            ProviderKind::detect("atlascloud/qwen/qwen3.5-flash"),
            Some(ProviderKind::AtlasCloud)
        );
        assert_eq!(
            ProviderKind::detect("atlascloud/deepseek-ai/deepseek-v4-pro"),
            Some(ProviderKind::AtlasCloud)
        );
        assert_eq!(
            ProviderKind::AtlasCloud.api_key_env(),
            Some("ATLASCLOUD_API_KEY")
        );
        assert_eq!(
            ProviderKind::AtlasCloud.endpoint_env(),
            Some("ATLASCLOUD_BASE_URL")
        );
        assert_eq!(
            ProviderKind::AtlasCloud.default_endpoint(),
            Some("https://api.atlascloud.ai/v1")
        );
        assert_eq!(ProviderKind::AtlasCloud.name(), "atlascloud");
        assert_eq!(
            ProviderKind::AtlasCloud.default_model(),
            "atlascloud/qwen/qwen3.5-flash"
        );
    }

    /// OpenAI's `-pro` tier answers `/v1/responses` and 404s on
    /// `/v1/chat/completions` ("This is not a chat model"). Routing them to
    /// the chat provider is a guaranteed failure, which is what every
    /// `gpt-*-pro` row in the catalogue did until 2026-08-11.
    #[test]
    fn detect_routes_openai_pro_models_to_responses() {
        for m in [
            "gpt-5-pro",
            "gpt-5-pro-2025-10-06",
            "gpt-5.2-pro",
            "gpt-5.4-pro-2026-03-05",
            "gpt-5.5-pro",
        ] {
            assert_eq!(
                ProviderKind::detect(m),
                Some(ProviderKind::OpenAIResponses),
                "{m} must go to the Responses endpoint"
            );
        }
        // Non-pro OpenAI ids keep the chat path.
        for m in ["gpt-5", "gpt-5.4-mini", "gpt-5.6-terra"] {
            assert_eq!(ProviderKind::detect(m), Some(ProviderKind::OpenAI), "{m}");
        }
        // The `gpt-` guard keeps another vendor's `-pro` out of it; a
        // prefixed id is matched by its own branch further up.
        assert_eq!(
            ProviderKind::detect("openrouter/openai/gpt-5.5-pro"),
            Some(ProviderKind::OpenRouter)
        );
        assert_eq!(
            ProviderKind::detect("atlascloud/deepseek-ai/deepseek-v4-pro"),
            Some(ProviderKind::AtlasCloud)
        );
    }

    #[test]
    fn detect_meta_prefix_routes_to_meta_provider() {
        assert_eq!(
            ProviderKind::detect("meta/muse-spark-1.2"),
            Some(ProviderKind::MetaAi)
        );
        assert_eq!(
            ProviderKind::detect("meta/muse-spark-1.2-contributor"),
            Some(ProviderKind::MetaAi)
        );
        assert_eq!(ProviderKind::MetaAi.api_key_env(), Some("META_API_KEY"));
        assert_eq!(ProviderKind::MetaAi.endpoint_env(), Some("META_BASE_URL"));
        assert_eq!(
            ProviderKind::MetaAi.default_endpoint(),
            Some("https://api.meta.ai/v1")
        );
        assert_eq!(ProviderKind::MetaAi.name(), "meta");
        assert_eq!(ProviderKind::MetaAi.default_model(), "meta/muse-spark-1.2");
    }

    /// OpenRouter proxies the same models and the catalogue stores its ids
    /// unprefixed (`meta/muse-spark-1.2`), so the two only stay apart because
    /// `detect` checks `openrouter/` first. Reordering that chain would send
    /// every OpenRouter Meta route to api.meta.ai — with a key most users
    /// routing through OpenRouter do not have.
    #[test]
    fn openrouter_wrapper_wins_over_the_meta_prefix() {
        assert_eq!(
            ProviderKind::detect("openrouter/meta/muse-spark-1.2"),
            Some(ProviderKind::OpenRouter)
        );
    }

    /// Meta AI is BYOK-only on purpose: no gateway segment, so it must not
    /// leak into the sold tier or the routed set.
    #[test]
    fn meta_is_byok_only() {
        assert_eq!(ProviderKind::MetaAi.tier(), ProviderTier::Additional);
        assert_eq!(
            crate::providers::thclaws_gateway::provider_segment(ProviderKind::MetaAi),
            None,
            "Meta AI has no gateway route"
        );
        assert!(
            !crate::shared::GATEWAY_ALL_PROVIDERS.contains(&ProviderKind::MetaAi.name()),
            "Meta AI must stay out of the gateway-routed set"
        );
    }

    #[test]
    fn detect_9router_prefix_routes_to_ninerouter_provider() {
        assert_eq!(
            ProviderKind::detect("9router/kr/claude-sonnet-4.5"),
            Some(ProviderKind::NineRouter)
        );
        assert_eq!(
            ProviderKind::detect("9router/openai/gpt-4o-mini"),
            Some(ProviderKind::NineRouter)
        );
        assert_eq!(
            ProviderKind::NineRouter.api_key_env(),
            Some("NINEROUTER_API_KEY")
        );
        assert_eq!(
            ProviderKind::NineRouter.endpoint_env(),
            Some("NINEROUTER_BASE_URL")
        );
        assert_eq!(
            ProviderKind::NineRouter.default_endpoint(),
            Some("http://localhost:20128/v1")
        );
        assert_eq!(ProviderKind::NineRouter.name(), "9router");
    }

    #[test]
    fn detect_tokenrouter_prefix_routes_to_tokenrouter_provider() {
        // tokenrouter/ must be detected before the bare-vendor heuristics.
        assert_eq!(
            ProviderKind::detect("tokenrouter/anthropic/claude-sonnet-4.5"),
            Some(ProviderKind::TokenRouter)
        );
        assert_eq!(
            ProviderKind::detect("tokenrouter/openai/gpt-5.4-nano"),
            Some(ProviderKind::TokenRouter)
        );
        assert_eq!(
            ProviderKind::TokenRouter.api_key_env(),
            Some("TOKENROUTER_API_KEY")
        );
        assert_eq!(
            ProviderKind::TokenRouter.endpoint_env(),
            Some("TOKENROUTER_BASE_URL")
        );
        assert_eq!(
            ProviderKind::TokenRouter.default_endpoint(),
            Some("https://api.tokenrouter.com/v1")
        );
        assert_eq!(ProviderKind::TokenRouter.name(), "tokenrouter");
        assert_eq!(
            ProviderKind::from_name("tokenrouter"),
            Some(ProviderKind::TokenRouter)
        );
    }

    #[test]
    fn detect_thaillm_prefix_routes_to_thaillm_provider() {
        assert_eq!(
            ProviderKind::detect("thaillm/OpenThaiGPT-ThaiLLM-8B-Instruct-v7.2"),
            Some(ProviderKind::ThaiLLM)
        );
        assert_eq!(
            ProviderKind::detect("thaillm/Typhoon-S-ThaiLLM-8B-Instruct"),
            Some(ProviderKind::ThaiLLM)
        );
        assert_eq!(ProviderKind::ThaiLLM.api_key_env(), Some("THAILLM_API_KEY"));
        assert_eq!(
            ProviderKind::ThaiLLM.default_endpoint(),
            Some("http://thaillm.or.th/api/v1")
        );
        assert_eq!(ProviderKind::ThaiLLM.name(), "thaillm");
    }

    #[test]
    fn detect_opencodego_prefix_routes_to_opencodego_provider() {
        assert_eq!(
            ProviderKind::detect("opencode-go/kimi-k2.6"),
            Some(ProviderKind::OpenCodeGo)
        );
        assert_eq!(
            ProviderKind::detect("opencode-go/deepseek-v4-flash"),
            Some(ProviderKind::OpenCodeGo)
        );
        assert_eq!(
            ProviderKind::detect("opencode-go/qwen3.6-plus"),
            Some(ProviderKind::OpenCodeGo)
        );
        // Bare model without prefix does NOT route to OpenCodeGo.
        assert!(
            !matches!(
                ProviderKind::detect("kimi-k2.6"),
                Some(ProviderKind::OpenCodeGo)
            ),
            "bare model ids without opencode-go/ prefix must not match OpenCodeGo"
        );
        // Provider properties match the expected values.
        assert_eq!(
            ProviderKind::OpenCodeGo.api_key_env(),
            Some("OPENCODE_GO_API_KEY")
        );
        assert_eq!(
            ProviderKind::OpenCodeGo.endpoint_env(),
            Some("OPENCODE_GO_BASE_URL")
        );
        assert_eq!(
            ProviderKind::OpenCodeGo.default_endpoint(),
            Some("https://opencode.ai/zen/go/v1")
        );
        assert_eq!(ProviderKind::OpenCodeGo.name(), "opencode-go");
        assert_eq!(
            ProviderKind::OpenCodeGo.default_model(),
            "opencode-go/deepseek-v4-flash"
        );
    }

    #[test]
    fn detect_gemini_and_gemma_go_to_gemini() {
        assert_eq!(
            ProviderKind::detect("gemini-2.0-flash"),
            Some(ProviderKind::Gemini)
        );
        assert_eq!(
            ProviderKind::detect("gemma-3-12b-it"),
            Some(ProviderKind::Gemini)
        );
        assert_eq!(
            ProviderKind::detect("gemma-3n-e4b-it"),
            Some(ProviderKind::Gemini)
        );
        assert_eq!(
            ProviderKind::detect("gemma-4-26b-a4b-it"),
            Some(ProviderKind::Gemini)
        );
    }
}

#[cfg(test)]
mod thinking_level_tests {
    use super::ThinkingLevel as L;

    /// `max` used to be an alias for `high`, i.e. 32_000 — while the raw-token
    /// arm accepts anything from 100 up, so a plain `/thinking 200000` was both
    /// accepted and larger. A word that promises a ceiling and silently picks a
    /// middling number is worse than no word; the usage line never offered it.
    #[test]
    fn max_is_not_an_alias_for_high_because_it_is_not_a_maximum() {
        assert_eq!(L::parse("max"), None, "`max` must be rejected, not aliased");
        // The honest route to more is still open, and really is larger.
        assert_eq!(L::parse("200000"), Some(Some(200_000)));
        assert!(200_000 > L::HIGH_BUDGET);
        // The documented words are untouched.
        assert_eq!(L::parse("high"), Some(Some(L::HIGH_BUDGET)));
        assert_eq!(L::parse("3"), Some(Some(L::HIGH_BUDGET)));
    }

    #[test]
    fn parse_levels_names_and_raw() {
        assert_eq!(L::parse("0"), Some(Some(0)));
        assert_eq!(L::parse("off"), Some(Some(0)));
        assert_eq!(L::parse("1"), Some(Some(L::LOW_BUDGET)));
        assert_eq!(L::parse("medium"), Some(Some(L::MEDIUM_BUDGET)));
        assert_eq!(L::parse("3"), Some(Some(L::HIGH_BUDGET)));
        assert_eq!(L::parse("auto"), Some(None));
        assert_eq!(L::parse("12000"), Some(Some(12000)));
        assert_eq!(L::parse("7"), None, "small bare numbers are not levels");
        assert_eq!(L::parse("bogus"), None);
    }

    #[test]
    fn budget_round_trips_through_buckets() {
        for l in [L::Off, L::Low, L::Medium, L::High] {
            assert_eq!(L::from_budget(Some(l.to_budget())), Some(l));
        }
        assert_eq!(L::from_budget(None), None);
        assert_eq!(L::from_budget(Some(12000)), Some(L::Medium));
        assert_eq!(L::json(Some(0))["level"], 0);
        assert!(L::json(None)["level"].is_null());
        assert_eq!(L::update_payload(Some(32000))["thinking"]["name"], "high");
    }
}

/// The HTTP client every provider streams through.
///
/// `reqwest::Client::new()` keeps idle connections forever and imposes
/// no connect timeout. Over a long-lived process that is a slow leak of
/// *latency*: the pool hands back a keep-alive socket the upstream shut
/// hours ago, and with no connect timeout the request waits on the OS
/// default before anything retries. The user sees a turn that produces
/// no first token for tens of seconds, on every provider and every
/// model, until the process restarts.
///
/// The three settings below are the same ones the cloud gateway adopted
/// in June for exactly this symptom (`thclaws-cloud/gateway/src/state.rs`).
/// A short idle timeout retires sockets before they go stale, TCP
/// keepalive notices a dead peer, and `connect_timeout` fails a dead
/// connection fast enough to retry cleanly.
///
/// Deliberately NO overall request timeout: a long generation is a
/// legitimately long response, and a timeout here would sever it.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .pool_idle_timeout(std::time::Duration::from_secs(20))
        .build()
        // A builder failure here means the TLS backend is unusable, in
        // which case a default client would not work either. Falling
        // back keeps construction infallible for every caller.
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[cfg(test)]
mod http_client_tests {
    /// Pins the settings, not the behaviour — reqwest exposes no getters,
    /// so the guard this test gives is that the builder still accepts the
    /// configuration and produces a client. The regression it exists for
    /// is someone replacing a provider's `super::http_client()` with a
    /// bare `Client::new()` again, which the grep in the sibling test
    /// below catches.
    #[test]
    fn builds() {
        let _ = super::http_client();
    }

    /// No provider may construct its own unconfigured client. A default
    /// `reqwest::Client` keeps idle sockets forever and has no connect
    /// timeout, which is how first-token latency degrades to tens of
    /// seconds in a long-lived process.
    #[test]
    fn no_provider_uses_a_bare_client() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/providers");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("providers dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some("mod.rs") {
                continue; // defines the helper + documents the anti-pattern
            }
            let src = std::fs::read_to_string(&path).expect("read provider");
            if src.contains("Client::new()") {
                offenders.push(path.file_name().unwrap().to_string_lossy().to_string());
            }
        }
        assert!(
            offenders.is_empty(),
            "these providers build an unconfigured HTTP client — use super::http_client(): {offenders:?}"
        );
    }
}
