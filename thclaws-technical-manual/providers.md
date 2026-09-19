# Providers — the router

Every LLM call funnels through `crate::providers`. The layer's contract is one trait — `Provider` — that exposes a single `stream(StreamRequest) -> EventStream` method and a closed enum — `ProviderKind` — that catalogues the 31 supported backends. A model string like `claude-sonnet-4-6` or `openrouter/anthropic/claude-opus-4-6` enters at the top, gets prefix-detected to a `ProviderKind`, then `build_provider(&config)` returns an `Arc<dyn Provider>` configured with the right URL, auth, and model-prefix-strip rules. The Agent loop sees only the trait — wire-format differences (Anthropic SSE event types vs OpenAI `data:` chunks vs Ollama NDJSON vs Gemini's nested `parts`) are normalized into a small `ProviderEvent` vocabulary, which `assemble.rs` then folds into `ContentBlock`s the agent persists.

This doc is the routing/dispatch layer. Each wire-format family has its own deep-dive manual:
- [`provider-anthropic.md`](provider-anthropic.md) — Messages API SSE (3 variants: Anthropic, OllamaAnthropic, AzureAIFoundry)
- [`provider-openai.md`](provider-openai.md) — Chat Completions SSE (16 variants: OpenAI, OpenRouter, TokenRouter, DashScope, QwenCloud, ZAi, LMStudio, OpenAICompat, DeepSeek, ThaiLLM, Nvidia, Minimax, Moonshot, XAi, Groq, OpenCodeGo)
- [`provider-responses.md`](provider-responses.md) — Responses API for codex/o-series (1 variant: OpenAIResponses)
- [`provider-gemini.md`](provider-gemini.md) — Google generativelanguage SSE (1 variant: Gemini)
- [`provider-ollama.md`](provider-ollama.md) — `/api/chat` NDJSON (2 variants: Ollama, OllamaCloud)
- [`provider-agentsdk.md`](provider-agentsdk.md) — `claude` CLI subprocess (1 variant: AgentSdk)
- [`provider-gateway.md`](provider-gateway.md) — EE policy-driven gateway overlay

**Source modules:**
- `crates/core/src/providers/mod.rs` — `ProviderKind`, `Provider` trait, `StreamRequest`, `ProviderEvent`, `Usage`, `RawDump`, `redact_key`
- `crates/core/src/providers/assemble.rs` — `assemble`, `AssembledEvent`, `collect_turn`, `<think>` tag splitting
- `crates/core/src/repl.rs::build_provider` — the actual dispatch table
- `crates/core/src/repl.rs::build_provider_with_fallback` — startup fallback walker
- `crates/core/src/providers/gateway.rs` — EE override (consulted first inside `build_provider`)

**Cross-references:**
- [`agentic-loop.md`](agentic-loop.md) — `Agent::run_turn` consumes the `EventStream` returned by `Provider::stream`
- [`context-composer.md`](context-composer.md) — what populates `StreamRequest.system` and `messages`
- [`sessions.md`](sessions.md) — the persisted `Session.messages` is what gets handed to `Provider::stream`

---

## 1. The contract

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    async fn stream(&self, req: StreamRequest) -> Result<EventStream>;

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Err(Error::Provider("list_models not supported by this provider".into()))
    }
}

pub type EventStream = BoxStream<'static, Result<ProviderEvent>>;
```

`stream` is the only mandatory method. `list_models` defaults to `Err` so providers that can't list (Ollama, AgentSdk, generic OpenAICompat) don't need to lie. Cloud providers with a `/v1/models` endpoint override.

### `StreamRequest`

```rust
pub struct StreamRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    pub thinking_budget: Option<u32>,   // Anthropic extended thinking; None disables
}
```

`model` is the user-facing string with provider prefix intact (e.g. `openrouter/anthropic/claude-sonnet-4-6`). The provider impl strips the routing prefix before serializing the request.

### `ProviderEvent` — the normalized wire vocabulary

```rust
pub enum ProviderEvent {
    MessageStart { model: String },
    TextDelta(String),
    ThinkingDelta(String),                          // structured reasoning (DeepSeek/o-series/OllamaCloud)
    ToolUseStart { id: String, name: String },
    ToolUseDelta { partial_json: String },
    ContentBlockStop,
    MessageStop {
        stop_reason: Option<String>,
        usage: Option<Usage>,
    },
}
```

Every wire format gets adapted into this 7-variant vocabulary. The agent loop, the assembler, and any UI subscriber speak `ProviderEvent` and never touch raw SSE / NDJSON.

### `Usage`

```rust
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
}
```

Cache fields are `Option` because only Anthropic reports them today. `Usage::accumulate(&other)` is what the agent loop uses to track cumulative tokens across iterations of one turn.

---

## 2. `ProviderKind` — the closed catalogue

```rust
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
    ChatGptCodex,
    OpenRouter,
    TokenRouter,
    Gemini,
    Ollama,
    OllamaAnthropic,
    OllamaCloud,
    DashScope,
    QwenCloud,
    ZAi,
    LMStudio,
    VLlm,
    LlamaCpp,
    LiteLlm,
    AzureAIFoundry,
    OpenAICompat,
    DeepSeek,
    ThaiLLM,
    Nvidia,
    Minimax,
    OpenCodeGo,
    Moonshot,
    XAi,
    Groq,
}
```

31 variants. (`AgenticPress` was removed.) `ALL: &'static [Self]` lists them in display order for the Settings UI. Every helper method below (`name`, `default_model`, `endpoint_env`, `default_endpoint`, `endpoint_user_configurable`, `api_key_env`, `resolve_alias_for_provider`) is a `match` over the full enum — adding a variant means updating every method, and the compiler enforces it.

### Catalogue table

| Variant | Wire family | Default model | Routing prefix | API key env | Endpoint env | Default endpoint | UI editable? |
|---|---|---|---|---|---|---|---|
| `Anthropic` | Anthropic Messages | `claude-sonnet-4-6` | `claude-` | `ANTHROPIC_API_KEY` | — | api.anthropic.com (fixed) | no |
| `AgentSdk` | subprocess | `agent/claude-sonnet-4-6` | `agent/` | (uses Claude Code auth) | — | — | no |
| `AzureAIFoundry` | Anthropic Messages | `azure/<deployment>` | `azure/` | `AZURE_AI_FOUNDRY_API_KEY` | `AZURE_AI_FOUNDRY_ENDPOINT` | https://{resource}.services.ai.azure.com | yes |
| `OllamaAnthropic` | Anthropic Messages | `oa/qwen3-coder` | `oa/` | none | `OLLAMA_BASE_URL` | http://localhost:11434 | yes |
| `OpenAI` | OpenAI Chat | `gpt-4.1` | `gpt-`/`o1-`/`o3-`/`o4-` | `OPENAI_API_KEY` | — | api.openai.com (fixed) | no |
| `OpenAIResponses` | OpenAI Responses | `codex/gpt-5.2-codex` | `codex/` (or contains `codex`) | `OPENAI_API_KEY` | — | api.openai.com (fixed) | no |
| `ChatGptCodex` | OpenAI Responses | `chatgpt-codex/gpt-5.4` | `chatgpt-codex/` (checked BEFORE `codex/`) | — (OAuth via Codex CLI auto-imported from `~/.codex/auth.json`) | — | chatgpt.com/backend-api/codex/responses (fixed) | no |
| `OpenRouter` | OpenAI Chat | `openrouter/qwen/qwen3.7-plus` | `openrouter/` | `OPENROUTER_API_KEY` | — | openrouter.ai (fixed) | no |
| `TokenRouter` | OpenAI Chat | `tokenrouter/anthropic/claude-sonnet-4.5` | `tokenrouter/` | `TOKENROUTER_API_KEY` | `TOKENROUTER_BASE_URL` | tokenrouter.com (fixed) | no |
| `DashScope` | OpenAI Chat | `dashscope/qwen3.7-max` | `dashscope/`/`qwen`/`qwq-` | `DASHSCOPE_API_KEY` | `DASHSCOPE_BASE_URL` | dashscope.aliyuncs.com/compatible-mode/v1 | no |
| `QwenCloud` | OpenAI Chat | `qc/qwen-max` | `qc/` | `QWENCLOUD_API_KEY` | `QWENCLOUD_BASE_URL` | dashscope-intl.aliyuncs.com/compatible-mode/v1 | no |
| `ZAi` | OpenAI Chat | `zai/glm-5.2` | `zai/` | `ZAI_API_KEY` | `ZAI_BASE_URL` | api.z.ai/api/coding/paas/v4 | no |
| `LMStudio` | OpenAI Chat | `lmstudio/llama-3.2-3b-instruct` | `lmstudio/` | none | `LMSTUDIO_BASE_URL` | localhost:1234/v1 | yes |
| `OpenAICompat` | OpenAI Chat | `oai/gpt-4o-mini` | `oai/` | `OPENAI_COMPAT_API_KEY` | `OPENAI_COMPAT_BASE_URL` | localhost:8000/v1 | yes |
| `DeepSeek` | OpenAI Chat | `deepseek-v4-flash` | `deepseek-` | `DEEPSEEK_API_KEY` | `DEEPSEEK_BASE_URL` | api.deepseek.com/v1 | no |
| `ThaiLLM` | OpenAI Chat | `thaillm/OpenThaiGPT-ThaiLLM-8B-Instruct-v7.2` | `thaillm/` | `THAILLM_API_KEY` | `THAILLM_BASE_URL` | thaillm.or.th/api/v1 | no |
| `Nvidia` | OpenAI Chat | `nvidia/nvidia/nemotron-3-super-120b-a12b` | `nvidia/` | `NVIDIA_API_KEY` | `NVIDIA_BASE_URL` | integrate.api.nvidia.com/v1 | no |
| `Minimax` | OpenAI Chat | `minimax/MiniMax-M3` | `minimax/` | `MINIMAX_API_KEY` | `MINIMAX_BASE_URL` | api.minimax.io/v1 | no |
| `Moonshot` | OpenAI Chat | `moonshot/kimi-k2.6` | `moonshot/` | `MOONSHOT_API_KEY` | `MOONSHOT_BASE_URL` | api.moonshot.ai/v1 | no |
| `XAi` | OpenAI Chat | `xai/grok-4.3` | `xai/` | `XAI_API_KEY` | `XAI_BASE_URL` | api.x.ai/v1 | no |
| `Groq` | OpenAI Chat | `groq/llama-3.3-70b-versatile` | `groq/` | `GROQ_API_KEY` | `GROQ_BASE_URL` | api.groq.com/openai/v1 | no |
| `OpenCodeGo` | OpenAI Chat | `opencode-go/deepseek-v4-flash` | `opencode-go/` | `OPENCODE_GO_API_KEY` | `OPENCODE_GO_BASE_URL` | opencode.ai (fixed) | no |
| `Gemini` | Google Gemini | `gemini-3.5-flash` | `gemini-`/`gemma-` | `GEMINI_API_KEY` | — | generativelanguage.googleapis.com (fixed) | no |
| `Ollama` | Ollama NDJSON | `ollama/llama3.2` | `ollama/` | none | `OLLAMA_BASE_URL` | http://localhost:11434 | yes |
| `OllamaCloud` | Ollama NDJSON | `ollama-cloud/deepseek-v4-flash` | `ollama-cloud/` | `OLLAMA_CLOUD_API_KEY` | — | ollama.com (fixed) | no |
| `AtlasCloud` | OpenAI Chat Completions | `atlascloud/qwen/qwen3.5-flash` | `atlascloud/` | `ATLASCLOUD_API_KEY` | `ATLASCLOUD_BASE_URL` | https://api.atlascloud.ai/v1 | no |
| `Unifically` | OpenAI Chat Completions | `unifically/google/gemini-3.5-flash` | `unifically/` | `UNIFICALLY_API_KEY` | `UNIFICALLY_BASE_URL` | https://api.unifically.com/v1 | no |
| `MetaAi` | OpenAI Chat Completions | `meta/muse-spark-1.2` | `meta/` | `META_API_KEY` | `META_BASE_URL` | https://api.meta.ai/v1 | no |
| `NineRouter` | OpenAI Chat Completions | `9router/anthropic/claude-sonnet-4.5` | `9router/` | `NINEROUTER_API_KEY` | `NINEROUTER_BASE_URL` | http://localhost:20128/v1 | yes |
| `LiteLlm` | OpenAI Chat Completions | `litellm/gpt-4o-mini` | `litellm/` | `LITELLM_API_KEY` | `LITELLM_BASE_URL` | http://localhost:4000/v1 | yes |
| `VLlm` | OpenAI Chat Completions | `vllm/served-model` | `vllm/` | — | `VLLM_BASE_URL` | http://localhost:8000/v1 | yes |
| `LlamaCpp` | OpenAI Chat Completions | `llamacpp/local-model` | `llamacpp/` | — | `LLAMACPP_BASE_URL` | http://localhost:8080/v1 | yes |

`endpoint_user_configurable` returns `true` for `Ollama`, `OllamaAnthropic`, `LMStudio`, `AzureAIFoundry`, `OpenAICompat` — those are the 5 backends the Settings UI exposes a "base URL" field for. Hosted services stay locked.

---

## 3. `detect(model: &str) -> Option<ProviderKind>` — prefix routing

```rust
pub fn detect(model: &str) -> Option<Self> {
    let model = &Self::resolve_alias(model);
    if model.starts_with("openrouter/") { Some(Self::OpenRouter) }
    else if model.starts_with("tokenrouter/") { Some(Self::TokenRouter) }
    else if model.starts_with("agent/") { Some(Self::AgentSdk) }
    else if model.starts_with("claude-") { Some(Self::Anthropic) }
    else if model.starts_with("chatgpt-codex/") { Some(Self::ChatGptCodex) }  // BEFORE codex/
    else if model.starts_with("codex/") || model.contains("codex") { Some(Self::OpenAIResponses) }
    else if model.starts_with("gpt-") || model.starts_with("o1-")
         || model.starts_with("o3-") || model.starts_with("o3")
         || model.starts_with("o4-") { Some(Self::OpenAI) }
    else if model.starts_with("gemini-") || model.starts_with("gemma-") { Some(Self::Gemini) }
    else if model.starts_with("qc/") { Some(Self::QwenCloud) }
    else if model.starts_with("dashscope/") { Some(Self::DashScope) }  // canonical, before bare qwen
    else if model.starts_with("qwen") || model.starts_with("qwq-") { Some(Self::DashScope) }
    else if model.starts_with("deepseek-") { Some(Self::DeepSeek) }
    else if model.starts_with("thaillm/") { Some(Self::ThaiLLM) }
    else if model.starts_with("zai/") { Some(Self::ZAi) }
    else if model.starts_with("moonshot/") { Some(Self::Moonshot) }
    else if model.starts_with("xai/") { Some(Self::XAi) }
    else if model.starts_with("groq/") { Some(Self::Groq) }
    else if model.starts_with("nvidia/") { Some(Self::Nvidia) }
    else if model.starts_with("minimax/") { Some(Self::Minimax) }
    else if model.starts_with("opencode-go/") { Some(Self::OpenCodeGo) }
    else if model.starts_with("oai/") { Some(Self::OpenAICompat) }
    else if model.starts_with("lmstudio/") { Some(Self::LMStudio) }
    else if model.starts_with("oa/") { Some(Self::OllamaAnthropic) }
    else if model.starts_with("ollama/") { Some(Self::Ollama) }
    else if model.starts_with("ollama-cloud/") { Some(Self::OllamaCloud) }
    else if model.starts_with("azure/") { Some(Self::AzureAIFoundry) }
    else { None }
}
```

**Order matters.** Most-specific prefixes go first:
- `openrouter/anthropic/claude-sonnet-4-6` would also match `claude-` if checked later, but `openrouter/` wins.
- `codex/gpt-5.2-codex` — the bare `codex/` check fires before the `gpt-` check below.
- `ollama-cloud/` is tested before `ollama/` would match (the latter would be a prefix of the former and wins via specificity by listing first... actually no — the code checks `ollama/` BEFORE `ollama-cloud/`. Since both checks use `starts_with`, `ollama-cloud/llama` does NOT start with `ollama/` because of the dash. So order doesn't matter here, but the check is still correct.)
- `o3-mini` vs `o3` (without dash) — the `o3` check uses `starts_with("o3")` so `o3` and `o3-mini` both match.

`detect` runs `resolve_alias` first so the user can type `sonnet` and get `claude-` matched downstream.

### `resolve_alias` — short user-typed names

```rust
match model.to_lowercase().as_str() {
    "sonnet" => "claude-sonnet-4-6",
    "opus" => "claude-opus-4-6",
    "haiku" => "claude-haiku-4-5",
    "flash" => "gemini-2.5-flash",
    "openthaigpt" => "thaillm/OpenThaiGPT-ThaiLLM-8B-Instruct-v7.2",
    "pathumma" => "thaillm/Pathumma-ThaiLLM-qwen3-8b-think-3.0.0",
    "thalle" => "thaillm/THaLLE-0.2-ThaiLLM-8B-fa",
    "typhoon" => "thaillm/Typhoon-S-ThaiLLM-8B-Instruct",
    _ => model.to_string(),       // pass-through, original casing preserved
}
```

Case-insensitive lookup; unknown input passes through with original casing intact (upstream model ids are case-sensitive — the lookup is the only thing folded).

### `resolve_alias_for_provider` — provider-aware variant

Used by `SpawnTeammate` so `agent_def.model: sonnet` keeps the team on the project's chosen provider instead of surprise-switching. Returns `None` when the alias doesn't belong in the given provider's namespace:

| Alias | Anthropic | Gemini | OpenRouter | ThaiLLM | Other |
|---|---|---|---|---|---|
| `sonnet` | `claude-sonnet-4-6` | `None` | `openrouter/anthropic/claude-sonnet-4-6` | `None` | `None` |
| `flash` | `None` | `gemini-2.5-flash` | `openrouter/google/gemini-2.5-flash` | `None` | `None` |
| `openthaigpt` | `None` | `None` | `None` | `thaillm/OpenThaiGPT-...` | `None` |

`None` = "not in my namespace; caller should fall back to default config rather than surprise-switch."

---

## 4. `build_provider(&AppConfig) -> Result<Arc<dyn Provider>>` — the dispatch

Lives in `repl.rs` (NOT in `providers/mod.rs` — it depends on `AppConfig` which would create a cycle). Three-stage dispatch:

### Stage A: Gateway override (EE)

```rust
if crate::providers::gateway::should_route(kind) {
    if let Some(url) = crate::providers::gateway::gateway_url() {
        return Ok(Arc::new(OpenAIProvider::new(auth).with_base_url(chat_url)));
    }
}
```

When `policies.gateway.enabled: true`, every cloud provider is replaced with an `OpenAIProvider` pointing at the gateway URL — regardless of `kind`. User's per-provider keys are ignored. Local providers (Ollama, OllamaAnthropic, LMStudio, AgentSdk) bypass when `read_only_local_models_allowed` is set. See [`provider-gateway.md`](provider-gateway.md).

### Stage B: Auth-less providers

```rust
match kind {
    ProviderKind::AgentSdk => {/* spawn `claude` subprocess */}
    ProviderKind::Ollama => {/* OllamaProvider::new() */}
    ProviderKind::OllamaAnthropic => {/* AnthropicProvider with "ollama" as auth */}
    ProviderKind::LMStudio => {/* OpenAIProvider with dummy bearer */}
    _ => {}   // fall through
}
```

These don't call `config.api_key_from_env()` because they don't need a real API key.

### Stage C: Auth'd providers — fetch key, then construct

```rust
let api_key = config.api_key_from_env().ok_or_else(|| ...)?;
match kind {
    ProviderKind::Anthropic => OpenAIProvider::new(api_key),    // fixed URL
    ProviderKind::OpenAI => OpenAIProvider::new(api_key),       // fixed URL
    ProviderKind::OpenAIResponses => OpenAIResponsesProvider::new(api_key),
    ProviderKind::Gemini => GeminiProvider::new(api_key),

    ProviderKind::OpenRouter => OpenAIProvider with openrouter.ai + strip_prefix("openrouter/"),
    ProviderKind::DashScope => OpenAIProvider with $DASHSCOPE_BASE_URL,
    ProviderKind::ZAi => OpenAIProvider with $ZAI_BASE_URL + strip_prefix("zai/"),
    ProviderKind::AzureAIFoundry => AnthropicProvider with $AZURE_AI_FOUNDRY_ENDPOINT/anthropic/v1/messages,
    ProviderKind::OpenAICompat => OpenAIProvider with $OPENAI_COMPAT_BASE_URL + strip_prefix("oai/"),
    ProviderKind::DeepSeek => OpenAIProvider with $DEEPSEEK_BASE_URL,
    ProviderKind::ThaiLLM => OpenAIProvider with $THAILLM_BASE_URL + strip_prefix("thaillm/"),
    ProviderKind::OllamaCloud => OllamaCloudProvider::new(api_key),
    /* AgentSdk / Ollama / OllamaAnthropic / LMStudio handled above */
}
```

Per-variant URL normalization: if the env-var-provided URL doesn't already end in `/chat/completions`, the dispatch appends it (lines 1301-1305, 1316-1320, 1348-1352, 1366-1370, 1381-1385). User configures the *base* URL; provider impl gets the *full* endpoint URL.

### `api_key_from_env` cascade

`AppConfig::api_key_from_env` resolves the key for `kind.api_key_env()` from (in order):
1. Process env (shell export or .env loaded by `dotenv`)
2. OS keychain (via `secrets.rs`)

`Some` means a key was found at one of those sources. `None` returns the helpful error message naming the env var.

---

## 5. `build_provider_with_fallback(&mut AppConfig)` — startup degradation

Used at REPL startup so a missing key doesn't crash the app.

1. Try `build_provider(config)` with the user's configured model. Success → return.
2. Walk a preference order:
   ```
   Anthropic → OpenAI → OpenRouter → Gemini → DashScope →
   ZAi → ThaiLLM → Ollama → OllamaAnthropic → OllamaCloud
   ```
   For each, swap `config.model = kind.default_model()` and try again. First success wins.
3. Ollama variants are gated on a 500 ms `GET /api/version` probe (`ollama_is_reachable`) — without this, a user with no keys AND no local Ollama would get a noisy "model not found" loop on the first prompt.
4. If nothing works: restore the original `config.model`, return `(None, Some(warning))`. The REPL/worker degrades to a `NoProviderPlaceholder` that errors friendly on every `stream()` call so the user can still open the Settings modal.

The returned warning is what surfaces as the yellow `[startup] no API key for ... — falling back to ...` line.

---

## 6. `assemble` — fold raw events into `AssembledEvent`

```rust
pub fn assemble(inner: impl Stream<Item = Result<ProviderEvent>>)
    -> impl Stream<Item = Result<AssembledEvent>>
```

```rust
pub enum AssembledEvent {
    Text(String),
    Thinking(String),
    ToolUse(ContentBlock),                  // ContentBlock::ToolUse with parsed input
    ToolParseFailed { id, name, error },    // M6.17 L4 — graceful tool_use JSON failure
    Done { stop_reason, usage },
}
```

The state machine:
- `MessageStart { model }` — checks `is_implicit_thinking_model(&model)` (Qwen3, QwQ, DeepSeek-R1) and pre-seeds `ThinkState::in_block = true`. Those models stream raw chain-of-thought with no opening `<think>` tag — only a closing `</think>` before the answer.
- `TextDelta(s)` — passes through `split_think_text` which routes `<think>...</think>` content to `Thinking` and outside content to `Text`. The state struct buffers cross-chunk tag fragments (`<think` arriving in one chunk, `>` in the next).
- `ThinkingDelta(s)` — structured reasoning from providers that separate it (DashScope/OpenRouter `reasoning_content`, Ollama `message.thinking`, OpenAI o-series). Flips `ThinkState::in_block = false` to defuse the implicit-thinking pre-seed (mutually exclusive with structured reasoning).
- `ToolUseStart { id, name }` — opens a `BlockState::ToolUse { id, name, buf: String }`.
- `ToolUseDelta { partial_json }` — appends to `buf`.
- `ContentBlockStop` — closes the current block. For `ToolUse`, parses `buf` as JSON:
  - Empty → `ToolUse` with `input: {}`
  - Valid JSON → `ToolUse` with the parsed value
  - Parse error → `ToolParseFailed { id, name, error }` (M6.17 L4 — pre-fix this killed the whole turn via `?`; now the agent loop synthesizes an error tool_result)
- `MessageStop { stop_reason, usage }` → `Done { stop_reason, usage }`.

### `<think>` tag splitting

For models that emit raw chain-of-thought as text (Qwen3, DeepSeek-R1) the assembler strips `<think>...</think>` blocks and routes their content to `Thinking` events. The state machine handles tags split across chunks, the implicit-pre-seed case (no opening tag, only a closing one), and consecutive newlines after `</think>`.

The raw model name passed via `MessageStart` controls the pre-seed — only Qwen3/QwQ/DeepSeek-R1 trigger it. Plain `qwen` / `qwen2` / OpenAI / Anthropic models skip the pre-seed so they don't pay a 1KB lookahead delay on every turn.

### `collect_turn` — drain to a `TurnResult`

```rust
pub struct TurnResult {
    pub text: String,
    pub thinking: String,
    pub tool_uses: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
}
```

`collect_turn(stream).await` walks the assembled stream and produces a single `TurnResult` — used by tests and one-shot print mode. The interactive agent loop drives the stream live so it can emit UI updates as text streams.

---

## 7. `RawDump` — debug helper

```rust
pub struct RawDump { enabled: bool, label: String, buf: String }
```

When `THCLAWS_SHOW_RAW=1` (env) or `showRawResponse: true` (settings.json), each provider impl wraps its assistant text accumulation through a `RawDump` and dumps a fenced dim block to stderr at end-of-turn:

```
─── raw response [openai] (1234 chars, 1500 bytes) ───
the actual streamed text...
───
```

Env var wins over settings — quick one-off debug runs don't need config edits. `Drop` flushes any unflushed buffer so a failed turn still produces output.

---

## 8. `redact_key(text, key)` — log safety

```rust
pub(crate) fn redact_key(text: &str, key: &str) -> String {
    if key.len() < 8 { return text.to_string(); }
    text.replace(key, "<redacted-api-key>")
}
```

Some providers (notably Gemini, which echoes the `?key=...` query param into 4xx response bodies) leak the API key in error responses. Provider impls pass error bodies through `redact_key` before constructing the user-visible `Error::Provider(...)`. The 8-char minimum is a false-positive guard — short values are more likely accidental matches than real secrets.

---

## 9. Code organization

```
crates/core/src/providers/
├── mod.rs (~1990 LOC)
│   ├── ProviderKind                                   25 variants + ALL slice
│   │   ├── name / default_model / api_key_env
│   │   ├── endpoint_env / default_endpoint /
│   │   │   endpoint_user_configurable
│   │   ├── resolve_alias / resolve_alias_for_provider
│   │   ├── detect / from_name
│   ├── Provider trait                                 stream + list_models
│   ├── StreamRequest / Usage / ModelInfo / ProviderEvent
│   ├── EventStream type alias
│   ├── RawDump                                        debug helper
│   ├── redact_key                                     log safety
│   └── tests                                          alias resolution + detection
├── assemble.rs (692 LOC)
│   ├── AssembledEvent / TurnResult / BlockState / ThinkState
│   ├── assemble                                       core fold
│   ├── collect_turn                                   drain helper
│   ├── split_think_text + longest_tag_prefix          <think> tag handling
│   └── is_implicit_thinking_model                     Qwen3/QwQ/DeepSeek-R1 detection
├── anthropic.rs (806 LOC)                             Anthropic Messages SSE — see provider-anthropic.md
├── openai.rs (1429 LOC)                               OpenAI Chat Completions SSE — see provider-openai.md
├── openai_responses.rs (516 LOC)                      OpenAI Responses SSE — see provider-responses.md
├── gemini.rs (1102 LOC)                               Gemini SSE — see provider-gemini.md
├── ollama.rs (930 LOC)                                Ollama NDJSON — see provider-ollama.md
├── ollama_cloud.rs (420 LOC)                          Ollama Cloud NDJSON — see provider-ollama.md
├── agent_sdk.rs (392 LOC)                             claude subprocess — see provider-agentsdk.md
└── gateway.rs (201 LOC)                               EE policy override — see provider-gateway.md

crates/core/src/repl.rs
├── build_provider                                     the dispatch
└── build_provider_with_fallback                       startup degradation walker
```

---

## 10. Testing

`providers::tests` — alias resolution + detect:
- `resolve_alias_for_provider_stays_in_namespace` — sonnet on Anthropic stays `claude-sonnet-4-6`; sonnet on OpenRouter becomes `openrouter/anthropic/claude-sonnet-4-6`; sonnet on Gemini/OpenAI/Ollama/DashScope/DeepSeek/ThaiLLM returns `None`.
- `alias_lookup_is_case_insensitive_for_thaillm_and_anthropic` — `OpenThaiGPT`, `openthaigpt`, `OPENTHAIGPT`, `Sonnet`, `FLASH` all resolve; `Custom-Model-V2` passes through with original casing.
- `alias_for_provider_only_resolves_within_correct_provider` — `openthaigpt` resolves only when current is `ThaiLLM`; `OpenThaiGPT` on Anthropic returns `None`.
- `detect_thaillm_prefix_routes_to_thaillm_provider` — full round-trip + env var + endpoint + name.
- `detect_gemini_and_gemma_go_to_gemini` — all `gemma-3-*`, `gemma-3n-*`, `gemma-4-*` route to `Gemini`.

`assemble` is exercised by every per-provider impl test (each provider builds a scripted event stream, runs it through `assemble`, asserts the resulting `AssembledEvent` sequence).

Per-provider tests live in each impl file and are cataloged in the per-provider manuals.

---

## 11. Adding a new provider

If it's a new OpenAI-compat aggregator (most common case — your `oaisomething/` LiteLLM endpoint, internal proxy, etc.):

1. Add a variant to `ProviderKind` enum.
2. Add it to `ALL: &'static [Self]`.
3. Add arms to: `name`, `default_model`, `api_key_env`, `endpoint_env`, `default_endpoint`, `endpoint_user_configurable`, `resolve_alias_for_provider` (return `None`).
4. Add a prefix branch to `detect` (early enough to win over conflicting prefixes).
5. Add a `match` arm in `build_provider` (`repl.rs:1273`-ish):
   ```rust
   ProviderKind::Foo => {
       let base = std::env::var("FOO_BASE_URL").unwrap_or_else(|_| ...);
       let url = if base.ends_with("/chat/completions") { base }
                 else { format!("{}/chat/completions", base.trim_end_matches('/')) };
       Ok(Arc::new(OpenAIProvider::new(api_key).with_base_url(url).with_strip_model_prefix("foo/")))
   }
   ```
6. Add to `build_provider_with_fallback`'s `fallback_order` if you want it picked up at startup.

If it needs a new wire format: write a new impl module (`crates/core/src/providers/foo.rs`), add it to `mod.rs`'s `pub mod foo;`, and add the construction arm in `build_provider`. The new impl must implement `Provider::stream` returning an `EventStream` of `ProviderEvent`s — the agent loop and assembler take care of everything downstream.

The compiler enforces exhaustiveness on every `match ProviderKind` site, so missing an arm fails the build.
