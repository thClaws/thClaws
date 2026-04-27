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
        }
    }

    /// Resolve short model aliases to full names — **provider-blind**.
    /// e.g. "sonnet" → "claude-sonnet-4-6", "opus" → "claude-opus-4-6"
    /// Use this for explicit user-typed `/model <alias>` commands where
    /// the user intends to switch providers along with the model. For
    /// passive resolution (agent defs, etc.) where the current provider
    /// must be preserved, use `resolve_alias_for_provider` instead.
    pub fn resolve_alias(model: &str) -> String {
        match model {
            "sonnet" => "claude-sonnet-4-6".into(),
            "opus" => "claude-opus-4-6".into(),
            "haiku" => "claude-haiku-4-5".into(),
            "flash" => "gemini-2.5-flash".into(),
            other => other.to_string(),
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
        // Anthropic-family aliases.
        let anthropic_id = match model {
            "sonnet" => Some("claude-sonnet-4-6"),
            "opus" => Some("claude-opus-4-6"),
            "haiku" => Some("claude-haiku-4-5"),
            _ => None,
        };
        // Google-family aliases (just `flash` for now).
        let google_id = match model {
            "flash" => Some("gemini-2.5-flash"),
            _ => None,
        };

        match provider {
            Self::Anthropic => anthropic_id.map(String::from),
            Self::Gemini => google_id.map(String::from),
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
            | Self::OpenAICompat => None,
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

#[cfg(test)]
mod tests {
    use super::*;

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

        // Non-aliases pass through as None — they don't need translation.
        assert!(ProviderKind::resolve_alias_for_provider(
            "claude-opus-4-7",
            ProviderKind::OpenRouter
        )
        .is_none());
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
