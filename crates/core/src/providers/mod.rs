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
pub mod anthropic_agent;
pub mod assemble;
pub mod gemini;
pub mod ollama;
pub mod openai;
pub mod openai_responses;

/// Registry of supported providers. Every new provider needs exactly one
/// variant here + matching arms in the methods below; the compiler catches
/// any omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    AgenticPress,
    Anthropic,
    AnthropicAgent,
    AgentSdk,
    OpenAI,
    OpenAIResponses,
    OpenRouter,
    Gemini,
    Ollama,
    OllamaAnthropic,
    DashScope,
}

impl ProviderKind {
    pub const ALL: &'static [Self] = &[
        Self::AgenticPress,
        Self::Anthropic,
        Self::AnthropicAgent,
        Self::AgentSdk,
        Self::OpenAI,
        Self::OpenAIResponses,
        Self::OpenRouter,
        Self::Gemini,
        Self::Ollama,
        Self::OllamaAnthropic,
        Self::DashScope,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Self::AgenticPress => "agentic-press",
            Self::Anthropic => "anthropic",
            Self::AnthropicAgent => "anthropic-managed-agent",
            Self::AgentSdk => "anthropic-agent",
            Self::OpenAI => "openai",
            Self::OpenAIResponses => "openai-responses",
            Self::OpenRouter => "openrouter",
            Self::Gemini => "gemini",
            Self::Ollama => "ollama",
            Self::OllamaAnthropic => "ollama-anthropic",
            Self::DashScope => "dashscope",
        }
    }

    pub fn default_model(&self) -> &'static str {
        match self {
            Self::AgenticPress => "ap/gemma4-12b",
            Self::Anthropic => "claude-sonnet-4-6",
            Self::AnthropicAgent => "managed/claude-sonnet-4-6",
            Self::AgentSdk => "agent/claude-sonnet-4-6",
            Self::OpenAI => "gpt-4o",
            Self::OpenAIResponses => "codex/gpt-5.2-codex",
            Self::OpenRouter => "openrouter/anthropic/claude-sonnet-4-6",
            Self::Gemini => "gemini-2.0-flash",
            Self::Ollama => "ollama/llama3.2",
            Self::OllamaAnthropic => "oa/qwen3-coder",
            Self::DashScope => "qwen-max",
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
            _ => None,
        }
    }

    /// Whether the Settings UI should expose this provider's base URL. We
    /// keep hosted services (Agentic Press, DashScope) locked to their
    /// defaults so users can't accidentally mis-point them; only self-hosted
    /// backends like Ollama are surfaced for editing. The env var still
    /// overrides at startup for power users who need it.
    pub fn endpoint_user_configurable(&self) -> bool {
        matches!(self, Self::Ollama | Self::OllamaAnthropic)
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
            _ => None,
        }
    }

    /// Env var holding the API key, if any. Ollama has no auth.
    pub fn api_key_env(&self) -> Option<&'static str> {
        match self {
            Self::AgenticPress => Some("AGENTIC_PRESS_LLM_API_KEY"),
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::AnthropicAgent => Some("ANTHROPIC_API_KEY"),
            Self::AgentSdk => None, // Uses Claude Code's own auth
            Self::OpenAI => Some("OPENAI_API_KEY"),
            Self::OpenAIResponses => Some("OPENAI_API_KEY"),
            Self::OpenRouter => Some("OPENROUTER_API_KEY"),
            Self::Gemini => Some("GEMINI_API_KEY"),
            Self::Ollama => None,
            Self::OllamaAnthropic => None,
            Self::DashScope => Some("DASHSCOPE_API_KEY"),
        }
    }

    /// Resolve short model aliases to full names.
    /// e.g. "sonnet" → "claude-sonnet-4-6", "opus" → "claude-opus-4-6"
    pub fn resolve_alias(model: &str) -> String {
        match model {
            "sonnet" => "claude-sonnet-4-6".into(),
            "opus" => "claude-opus-4-6".into(),
            "haiku" => "claude-haiku-4-5".into(),
            "flash" => "gemini-2.0-flash".into(),
            other => other.to_string(),
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
        } else if model.starts_with("managed/") {
            Some(Self::AnthropicAgent)
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
        } else if model.starts_with("oa/") {
            Some(Self::OllamaAnthropic)
        } else if model.starts_with("ollama/") {
            Some(Self::Ollama)
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
