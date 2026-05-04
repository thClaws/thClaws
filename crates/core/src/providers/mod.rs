//! Provider abstraction — streaming interface over one LLM backend.
//!
//! Wire formats (Anthropic, OpenAI, etc.) are adapted to a common
//! [`ProviderEvent`] stream. Higher layers consume only the stream.

use crate::error::Result;
use crate::types::{Message, ToolDef};
use async_trait::async_trait;
use futures::stream::BoxStream;

pub mod agent_sdk;
pub mod anthropic;
pub mod assemble;
pub mod gateway;
pub mod gemini;
pub mod ollama;
pub mod ollama_cloud;
pub mod openai;
pub mod openai_responses;

/// Registry of supported providers. Every new provider needs exactly one
/// variant here + matching arms in the methods below; the compiler catches
/// any omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    AgenticPress,
    Anthropic,
    AgentSdk,
    OpenAI,
    OpenAIResponses,
    OpenRouter,
    Gemini,
    Ollama,
    OllamaAnthropic,
    OllamaCloud,
    DashScope,
    ZAi,
    LMStudio,
    AzureAIFoundry,
    OpenAICompat,
    DeepSeek,
    ThaiLLM,
    Nvidia,
}

impl ProviderKind {
    pub const ALL: &'static [Self] = &[
        Self::AgenticPress,
        Self::Anthropic,
        Self::AgentSdk,
        Self::OpenAI,
        Self::OpenAIResponses,
        Self::OpenRouter,
        Self::Gemini,
        Self::Ollama,
        Self::OllamaAnthropic,
        Self::OllamaCloud,
        Self::DashScope,
        Self::ZAi,
        Self::LMStudio,
        Self::AzureAIFoundry,
        Self::OpenAICompat,
        Self::DeepSeek,
        Self::ThaiLLM,
        Self::Nvidia,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Self::AgenticPress => "agentic-press",
            Self::Anthropic => "anthropic",
            Self::AgentSdk => "anthropic-agent",
            Self::OpenAI => "openai",
            Self::OpenAIResponses => "openai-responses",
            Self::OpenRouter => "openrouter",
            Self::Gemini => "gemini",
            Self::Ollama => "ollama",
            Self::OllamaAnthropic => "ollama-anthropic",
            Self::OllamaCloud => "ollama-cloud",
            Self::DashScope => "dashscope",
            Self::ZAi => "zai",
            Self::LMStudio => "lmstudio",
            Self::AzureAIFoundry => "azure",
            Self::OpenAICompat => "openai-compat",
            Self::DeepSeek => "deepseek",
            Self::ThaiLLM => "thaillm",
            Self::Nvidia => "nvidia",
        }
    }

    pub fn default_model(&self) -> &'static str {
        match self {
            Self::AgenticPress => "ap/gemma4-12b",
            Self::Anthropic => "claude-sonnet-4-6",
            Self::AgentSdk => "agent/claude-sonnet-4-6",
            Self::OpenAI => "gpt-4o",
            Self::OpenAIResponses => "codex/gpt-5.2-codex",
            Self::OpenRouter => "openrouter/anthropic/claude-sonnet-4-6",
            // Pinned to a versioned ID (matching Anthropic / OpenAI
            // convention) rather than `gemini-flash-latest` — `-latest`
            // is a rolling Google-side alias that could promote into a
            // higher-tier model without warning, surprising users with
            // unexpected cost. Track upcoming retirement at:
            // https://ai.google.dev/gemini-api/docs/deprecations
            // Next bump deadline: 2026-06-17 (gemini-2.5-flash shutdown).
            Self::Gemini => "gemini-2.5-flash",
            Self::Ollama => "ollama/llama3.2",
            Self::OllamaAnthropic => "oa/qwen3-coder",
            Self::OllamaCloud => "ollama-cloud/deepseek-v4-flash",
            Self::DashScope => "qwen-max",
            Self::ZAi => "zai/glm-4.6",
            // Most LMStudio installs change models constantly; this is a
            // placeholder that lets the connection establish so the user
            // can `/model lmstudio/<loaded-model>` to switch. list_models
            // will populate the GUI dropdown with whatever's actually
            // loaded.
            Self::LMStudio => "lmstudio/llama-3.2-3b-instruct",
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
            // Model IDs keep the `nvidia/` prefix on the wire (that's NVIDIA's own namespace,
            // not a thClaws routing prefix). Override via NVIDIA_BASE_URL for on-prem NIM.
            Self::Nvidia => "nvidia/nemotron-3-super-120b-a12b",
        }
    }

    /// Env var holding the base URL override, if the provider supports a
    /// configurable endpoint. Used by the Settings UI to let users point at
    /// self-hosted or regional endpoints.
    pub fn endpoint_env(&self) -> Option<&'static str> {
        match self {
            // Agentic Press is a hosted gateway with a fixed URL — no env
            // override, no UI knob. Build-time only.
            Self::DashScope => Some("DASHSCOPE_BASE_URL"),
            Self::Ollama => Some("OLLAMA_BASE_URL"),
            Self::OllamaAnthropic => Some("OLLAMA_BASE_URL"),
            Self::ZAi => Some("ZAI_BASE_URL"),
            Self::LMStudio => Some("LMSTUDIO_BASE_URL"),
            Self::AzureAIFoundry => Some("AZURE_AI_FOUNDRY_ENDPOINT"),
            Self::OpenAICompat => Some("OPENAI_COMPAT_BASE_URL"),
            Self::DeepSeek => Some("DEEPSEEK_BASE_URL"),
            Self::ThaiLLM => Some("THAILLM_BASE_URL"),
            Self::Nvidia => Some("NVIDIA_BASE_URL"),
            _ => None,
        }
    }

    /// Whether the Settings UI should expose this provider's base URL. We
    /// keep hosted services (Agentic Press, DashScope, Z.ai) locked to their
    /// defaults so users can't accidentally mis-point them; only self-hosted
    /// backends like Ollama and LMStudio are surfaced for editing. The env
    /// var still overrides at startup for power users who need it.
    pub fn endpoint_user_configurable(&self) -> bool {
        matches!(
            self,
            Self::Ollama
                | Self::OllamaAnthropic
                | Self::LMStudio
                | Self::AzureAIFoundry
                | Self::OpenAICompat,
        )
    }

    /// Default base URL shown as a placeholder in the Settings UI when the
    /// user hasn't configured one. `None` for providers without an endpoint
    /// concept (Anthropic, OpenAI, etc. — those always hit the official API).
    pub fn default_endpoint(&self) -> Option<&'static str> {
        match self {
            // Agentic Press URL is fixed build-time; no UI placeholder.
            Self::DashScope => Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
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
            Self::AzureAIFoundry => Some("https://{resource}.services.ai.azure.com"),
            // Generic OAI-compat: users always set their own URL; this
            // placeholder just hints at the expected shape (path ending in /v1).
            Self::OpenAICompat => Some("http://localhost:8000/v1"),
            Self::DeepSeek => Some("https://api.deepseek.com/v1"),
            Self::ThaiLLM => Some("http://thaillm.or.th/api/v1"),
            Self::Nvidia => Some("https://integrate.api.nvidia.com/v1"),
            _ => None,
        }
    }

    /// Env var holding the API key, if any. Ollama has no auth.
    pub fn api_key_env(&self) -> Option<&'static str> {
        match self {
            Self::AgenticPress => Some("AGENTIC_PRESS_LLM_API_KEY"),
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::AgentSdk => None, // Uses Claude Code's own auth
            Self::OpenAI => Some("OPENAI_API_KEY"),
            Self::OpenAIResponses => Some("OPENAI_API_KEY"),
            Self::OpenRouter => Some("OPENROUTER_API_KEY"),
            Self::Gemini => Some("GEMINI_API_KEY"),
            Self::Ollama => None,
            Self::OllamaAnthropic => None,
            Self::OllamaCloud => Some("OLLAMA_CLOUD_API_KEY"),
            Self::DashScope => Some("DASHSCOPE_API_KEY"),
            Self::ZAi => Some("ZAI_API_KEY"),
            Self::LMStudio => None, // Local runtime, no auth.
            Self::AzureAIFoundry => Some("AZURE_AI_FOUNDRY_API_KEY"),
            Self::OpenAICompat => Some("OPENAI_COMPAT_API_KEY"),
            Self::DeepSeek => Some("DEEPSEEK_API_KEY"),
            Self::ThaiLLM => Some("THAILLM_API_KEY"),
            Self::Nvidia => Some("NVIDIA_API_KEY"),
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
            Self::AgenticPress => {
                // ap/* mirrors the same families with an `ap/` prefix.
                if let Some(id) = anthropic_id {
                    return Some(format!("ap/{id}"));
                }
                if let Some(id) = google_id {
                    return Some(format!("ap/{id}"));
                }
                None
            }
            // Providers without a notion of these aliases. Returning None
            // signals "alias doesn't apply here" so the caller can fall
            // back to whatever default the user had configured rather than
            // surprise-switching to a different provider.
            Self::OpenAI
            | Self::OpenAIResponses
            | Self::AgentSdk
            | Self::Ollama
            | Self::OllamaAnthropic
            | Self::OllamaCloud
            | Self::DashScope
            | Self::ZAi
            | Self::LMStudio
            | Self::AzureAIFoundry
            | Self::OpenAICompat
            | Self::DeepSeek
            | Self::Nvidia => None,
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
        } else if model.starts_with("ap/") {
            Some(Self::AgenticPress)
        } else if model.starts_with("agent/") {
            Some(Self::AgentSdk)
        } else if model.starts_with("claude-") {
            Some(Self::Anthropic)
        } else if model.starts_with("codex/") || model.contains("codex") {
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
        } else if model.starts_with("oa/") {
            Some(Self::OllamaAnthropic)
        } else if model.starts_with("ollama/") {
            Some(Self::Ollama)
        } else if model.starts_with("ollama-cloud/") {
            Some(Self::OllamaCloud)
        } else if model.starts_with("azure/") {
            Some(Self::AzureAIFoundry)
        } else if model.starts_with("nvidia/") {
            // NVIDIA NIM (integrate.api.nvidia.com). Model IDs like
            // `nvidia/nemotron-3-super-120b-a12b` keep the `nvidia/` prefix
            // on the wire — it is part of the upstream model ID, not stripped.
            // OpenRouter proxies the same models as `openrouter/nvidia/...`;
            // the `openrouter/` check above catches those first.
            Some(Self::Nvidia)
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

#[derive(Debug, Clone)]
pub struct StreamRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    /// Anthropic extended-thinking budget. `None` disables thinking.
    pub thinking_budget: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
}

impl Default for Usage {
    fn default() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
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
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: Option<String>,
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
    },
    ToolUseDelta {
        partial_json: String,
    },
    ContentBlockStop,
    MessageStop {
        stop_reason: Option<String>,
        usage: Option<Usage>,
    },
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
}

/// Does the active provider have credentials (env var set) or is it
/// a no-auth local provider? Used by sidebar/UI code (and the
/// `model_set` / `config_poll` IPC arms in M6.36) to flag the active
/// provider's readiness without spinning up a real provider instance.
///
/// M6.36 SERVE9e: lifted out of `gui.rs` to an always-on home so the
/// WS transport's IPC handlers can use the same readiness check.
pub fn provider_has_credentials(cfg: &crate::config::AppConfig) -> bool {
    kind_has_credentials(cfg.detect_provider_kind().ok())
}

/// True when `kind` has credentials available (env var, or no-auth
/// local provider). Same logic the GUI's auto-fallback path uses.
pub fn kind_has_credentials(kind: Option<ProviderKind>) -> bool {
    let Some(kind) = kind else { return false };
    match kind {
        ProviderKind::AgentSdk => true,
        ProviderKind::Ollama | ProviderKind::OllamaAnthropic | ProviderKind::LMStudio => true,
        other => other
            .api_key_env()
            .and_then(|v| std::env::var(v).ok())
            .map(|val| !val.trim().is_empty())
            .unwrap_or(false),
    }
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
    let ollama_live: Vec<String> = {
        let base = std::env::var("OLLAMA_BASE_URL")
            .unwrap_or_else(|_| crate::providers::ollama::DEFAULT_BASE_URL.to_string());
        let provider = crate::providers::ollama::OllamaProvider::new().with_base_url(base);
        match tokio::time::timeout(
            std::time::Duration::from_millis(800),
            provider.list_models(),
        )
        .await
        {
            Ok(Ok(models)) => models.into_iter().map(|m| m.id).collect(),
            _ => Vec::new(),
        }
    };
    let mut groups: Vec<serde_json::Value> = Vec::new();
    for kind in ProviderKind::ALL {
        let name = kind.name();
        let mut model_ids: std::collections::BTreeMap<String, Option<u32>> =
            std::collections::BTreeMap::new();
        for (id, entry) in cat.list_models_for_provider(name) {
            let canonical = if ProviderKind::detect(&id) == Some(*kind) {
                id
            } else {
                format!("{name}/{id}")
            };
            model_ids.insert(canonical, entry.context);
        }
        if matches!(kind, ProviderKind::Ollama) {
            for id in &ollama_live {
                model_ids.entry(id.clone()).or_insert(None);
            }
        }
        if model_ids.is_empty() {
            continue;
        }
        let model_rows: Vec<serde_json::Value> = model_ids
            .into_iter()
            .map(|(id, ctx)| serde_json::json!({ "id": id, "context": ctx }))
            .collect();
        groups.push(serde_json::json!({
            "provider": name,
            "models": model_rows,
        }));
    }
    serde_json::json!({
        "type": "all_models_list",
        "groups": groups,
        "ollama_reachable": !ollama_live.is_empty(),
    })
    .to_string()
}

/// If `cfg.model`'s provider has no credentials, pick the first
/// provider that does and return its default model. Returns `None`
/// when the current model is already fine or nothing else is usable.
///
/// Called by the GUI at startup and after `api_key_set` so the
/// sidebar's active-provider indicator + persisted settings.json land
/// on whatever the user actually has configured. Same logic now
/// callable from the WS transport's settings handlers.
pub fn auto_fallback_model(cfg: &crate::config::AppConfig) -> Option<String> {
    if provider_has_credentials(cfg) {
        return None;
    }
    const ORDER: &[ProviderKind] = &[
        ProviderKind::Anthropic,
        ProviderKind::OpenAI,
        ProviderKind::AgenticPress,
        ProviderKind::OpenRouter,
        ProviderKind::Gemini,
        ProviderKind::DashScope,
        ProviderKind::ZAi,
        ProviderKind::DeepSeek,
        ProviderKind::ThaiLLM,
    ];
    for kind in ORDER {
        if kind_has_credentials(Some(*kind)) {
            return Some(kind.default_model().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // Agentic Press mirrors the family names with `ap/` prefix.
        assert_eq!(
            ProviderKind::resolve_alias_for_provider("opus", ProviderKind::AgenticPress).as_deref(),
            Some("ap/claude-opus-4-6"),
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
