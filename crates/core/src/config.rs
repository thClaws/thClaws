//! Runtime configuration for the native agent.
//!
//! Load order (higher wins):
//!   1. CLI flags
//!   2. `.thclaws/settings.json` (project)
//!   3. `~/.config/thclaws/settings.json` (user)
//!   4. Compiled-in defaults (credential-aware — see `preferred_default_model`)
//!
//! The user-global `~/.claude/settings.json` is deliberately NOT a model
//! source: importing Claude Code's `model` (e.g. a `…[1m]` alias thClaws
//! can't route) silently overrode the credential-aware default and left
//! users on a provider they never chose. Project-level `.claude/settings.json`
//! (committed per-repo) is still honored as a migration convenience.
//!
//! API keys are never stored in config files — only in env vars or `.env` files.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Process-wide one-shot model override set by `app.rs` from
/// `--model` / `--set-model` before any dispatch path. Applied at the
/// end of `AppConfig::load`, after the project overlay, so every
/// surface (CLI, GUI, --serve) sees the same model without each
/// having to re-implement the override step. `clear_cli_model_override`
/// drops it — used by the GUI's auto-fallback path so a broken
/// `--model` doesn't pin the session to an unreachable provider after
/// the fallback has already switched.
static CLI_MODEL_OVERRIDE: RwLock<Option<String>> = RwLock::new(None);

/// Stash a CLI-supplied model so subsequent `AppConfig::load` calls
/// return it as `config.model`. Called once at startup from `app.rs`.
pub fn set_cli_model_override(model: String) {
    if let Ok(mut guard) = CLI_MODEL_OVERRIDE.write() {
        *guard = Some(model);
    }
}

/// Drop any active CLI model override. Called by the GUI's auto-fallback
/// flow when the user's `--model` choice is unreachable and a different
/// provider is being promoted to the project default.
pub fn clear_cli_model_override() {
    if let Ok(mut guard) = CLI_MODEL_OVERRIDE.write() {
        *guard = None;
    }
}

/// Inspect the active CLI model override. Returns `None` when no
/// override is set. Primarily for tests.
pub fn cli_model_override() -> Option<String> {
    CLI_MODEL_OVERRIDE.read().ok().and_then(|g| g.clone())
}

/// Where a skill's model recommendation lands when its own candidates
/// are gone: cheap, fast, vision-capable, and — the reason it isn't a
/// DeepSeek model — NOT a thinking model. Swapping mid-turn into one
/// sends it a history whose earlier assistant turn carries no
/// `reasoning_content`, which DeepSeek rejects outright.
pub const DEFAULT_SKILL_MODEL_FALLBACK: &str = "gpt-4.1-mini";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppConfig {
    /// Model identifier, e.g. `claude-sonnet-4-6` or `gpt-4o`.
    pub model: String,

    /// Max tokens to request from the provider per turn.
    pub max_tokens: u32,

    /// Permission mode: `auto`, `ask`, `accept_all`.
    pub permissions: String,

    /// System prompt override. Empty → use provider-derived default.
    pub system_prompt: String,

    /// Anthropic extended-thinking token budget. `None` or 0 → disabled.
    pub thinking_budget: Option<u32>,

    /// Per-chunk idle timeout (seconds) applied to every streaming
    /// provider's `byte_stream.next()` await. If the provider goes
    /// silent for this long mid-stream the response is aborted with
    /// an actionable error ("stream idle for Ns — try again") so the
    /// UI doesn't hang silently until the user force-quits.
    /// Default 120 s — generous for research / long-reasoning turns
    /// where the model can legitimately pause mid-stream. The original
    /// constant (PR #81 / #83) was 30 s, which proved too tight for
    /// `/research` workloads + slow Anthropic Opus / GPT-5 reasoning.
    /// Set to `0` to fall back to the compile-time default of 120 s.
    #[serde(default = "default_stream_chunk_timeout_secs")]
    pub stream_chunk_timeout_secs: u64,

    /// Search engine for WebSearch tool: "auto" (default), "tavily", "brave", "serpapi", "youcom", "serply", "duckduckgo".
    pub search_engine: String,

    /// Allowed tool names (None = all). CLI: --allowed-tools
    #[serde(skip)]
    pub allowed_tools: Option<Vec<String>>,

    /// Disallowed tool names (None = none). CLI: --disallowed-tools
    #[serde(skip)]
    pub disallowed_tools: Option<Vec<String>>,

    /// Tool names that ALWAYS prompt for approval, even under
    /// `permissions: "auto"` (None = none). Interactive-only: it forces
    /// the approval gate on for these tools regardless of the mode, so it
    /// only has an effect where a human approver is wired (desktop GUI /
    /// CLI). Under an auto-approving sink (`-p`, team agents, multiuser
    /// pods) there's no one to prompt, so it's a silent no-op there.
    #[serde(skip)]
    pub ask_tools: Option<Vec<String>>,

    /// Resume session ID (None = new session). CLI: --resume
    #[serde(skip)]
    pub resume_session: Option<String>,

    /// Lifecycle hooks — shell commands fired on agent events.
    pub hooks: crate::hooks::HooksConfig,

    /// dev-plan/49: OS-level Bash confinement mode — `workspace` (default:
    /// writes confined to workspace + tmp + package-manager caches), `strict`
    /// (workspace + tmp only), or `off`. settings.json `bash.sandbox`. Falls
    /// back to unconfined (with a warning) on hosts where no OS confiner can
    /// enforce, so the default never breaks a command.
    pub bash_sandbox: String,
    /// Extra absolute (or `~/`) paths the confined Bash may write to.
    /// settings.json `bash.sandbox_write_paths`.
    pub bash_sandbox_write_paths: Vec<String>,
    /// Extra paths the confined Bash must NOT read. settings.json
    /// `bash.sandbox_deny_read`.
    pub bash_sandbox_deny_read: Vec<String>,

    /// Maximum agent loop iterations per turn (0 = unlimited).
    /// Default 200 — high enough for complex multi-step tasks.
    pub max_iterations: usize,

    /// Plan-mode context strategy at step boundaries (M6).
    /// - `"compact"` (default, M6.2): structurally trims pre-boundary
    ///   non-plan tool_result content to a placeholder, keeping plan
    ///   tool breadcrumbs and conversation shape intact.
    /// - `"clear"` (M6.4 opt-in): wipes the agent's chat history at
    ///   each step boundary, keeping only the original user prompt
    ///   for grounding. Most aggressive — forces full reliance on
    ///   `step.output` for cross-step data + the system reminder for
    ///   plan structure. Recommended only for very long plans
    ///   (20+ steps) where compaction alone isn't enough.
    pub plan_context_strategy: String,

    /// How installed skills are surfaced to the model (dev-plan/06 P2).
    /// Trade-offs between system-prompt token cost and discoverability.
    /// - `"full"` (default): every skill listed with name + description
    ///   + when_to_use trigger. Highest token cost; highest "model
    ///   always knows" coverage. Right for users with ≤20 skills.
    /// - `"names-only"`: list only skill names + a hint to call the
    ///   Skill / SkillSearch tools for detail. Constant per-skill cost
    ///   (~30 chars vs ~200) so 100 skills add ~3KB instead of ~20KB.
    /// - `"discover-tool-only"`: no skill names listed at all; system
    ///   prompt only mentions the SkillList / SkillSearch tools.
    ///   Constant-size prompt regardless of skill count. Risks model
    ///   missing skills it should have invoked — only set when token
    ///   budget matters more than guaranteed coverage.
    pub skills_listing_strategy: String,

    /// MCP servers to spawn at REPL startup. Each server's discovered tools
    /// are registered into the `ToolRegistry` alongside the native built-ins,
    /// prefixed with the server name (e.g. `"filesystem.read_file"`).
    pub mcp_servers: Vec<crate::mcp::McpServerConfig>,

    /// Names of active KMS (knowledge bases). Each active KMS's `index.md`
    /// is concatenated into the system prompt, and `KmsRead` / `KmsSearch`
    /// tools are registered. Empty by default — users opt in per-project
    /// via the sidebar or `/kms use NAME`.
    #[serde(default)]
    pub kms_active: Vec<String>,

    /// **Self-improving AI Agent (auto-learn).** Opt-in: when `true`,
    /// each session-end automatically files the just-closed session as
    /// a new page in a dedicated `self_learn` KMS (see
    /// [`Self::auto_learn_kms`]) and periodically reconciles that KMS to
    /// resolve contradictions. The dedicated KMS is separate from the
    /// user's hand-curated active KMSes — auto-ingested pages never
    /// touch them.
    ///
    /// Pipeline at session end (GUI / `--serve` only — CLI users wire
    /// `session_end` hook manually):
    ///
    ///   1. `KmsCreate({name: auto_learn_kms, scope: "project"})` —
    ///      idempotent bootstrap.
    ///   2. `/kms ingest <kms> <session-id>` — one session → one page.
    ///   3. `/kms reconcile <kms> --apply` — throttled per
    ///      [`Self::auto_learn_reconcile_hours`].
    ///
    /// Default `false` so users opt in deliberately (token cost,
    /// permission gate, predictability). See `dev-plan/27`.
    #[serde(default, alias = "autoLearn")]
    pub auto_learn: bool,

    /// "Job runner" mode (isolated-skills / session-per-run). When true the
    /// worker archives the current session and starts a fresh one BEFORE
    /// every user turn, so a workspace used to run the same skill on input
    /// after input never accumulates (and re-sends) earlier runs' context.
    /// Off by default — a normal chat keeps its history for multi-turn
    /// continuity. The prior session is saved to disk first, so nothing is
    /// lost: the audit trail stays complete (archive, not delete).
    #[serde(default, alias = "sessionResetPerTurn")]
    pub session_reset_per_turn: bool,

    /// KMS name target for auto-learn. Project-scope. Auto-created on
    /// first run. Default `self_learn` — dedicated audit-log-style KMS
    /// for session pages, kept separate from
    /// [`Self::kms_active`] vaults.
    #[serde(default = "default_auto_learn_kms", alias = "autoLearnKms")]
    pub auto_learn_kms: String,

    /// Minimum hours between automatic reconcile passes on the
    /// `self_learn` KMS. Session ingest runs every session-end;
    /// reconcile is the expensive pass and runs at most once per this
    /// many hours. Default `6` (≤ 4 reconciles / day even on heavy
    /// usage). Set higher for quieter workspaces, lower if you want
    /// faster contradiction resolution.
    #[serde(
        default = "default_auto_learn_reconcile_hours",
        alias = "autoLearnReconcileHours"
    )]
    pub auto_learn_reconcile_hours: u32,

    /// M6.39.5: opt-in to loading user-level Claude Code memory
    /// (`~/.claude/CLAUDE.md` and `~/.claude/AGENTS.md`) into
    /// thClaws's system prompt. Default `false` — the user's Claude
    /// Code identity (Pinn.AI bias, "use Claude Code's MCP tools",
    /// etc.) shouldn't bleed into thClaws's behavior just because
    /// both tools happen to live on the same machine.
    ///
    /// Project-level `<cwd>/.claude/CLAUDE.md` (committed to a repo)
    /// keeps loading regardless — that's repo-shared instructions,
    /// not user-personal config. The flag only affects the user-home
    /// `~/.claude/*` files.
    ///
    /// Set `true` if you intentionally maintain one CLAUDE.md across
    /// both tools and want the parity. The thClaws-native path
    /// (`~/.config/thclaws/CLAUDE.md`) loads either way.
    #[serde(default)]
    pub claude_md_compat: bool,

    /// When `true` and the active provider is OpenRouter, both the
    /// `/models` slash command and the post-key-entry model picker
    /// hide non-free rows. Persists at the project level so a user
    /// can keep "free only" on for a side project and off for paid
    /// work in another repo. Other providers ignore the flag.
    #[serde(default)]
    pub openrouter_free_only: bool,

    /// Opt-in flag for the native Gemini image-generation tools
    /// (`TextToImage`, `ImageToImage`). Off by default because the
    /// tools call paid Google APIs on the user's own key and write
    /// PNG files into the workspace — both reasons to require an
    /// explicit "yes" before they appear in the model's tool list.
    /// Requires `GEMINI_API_KEY` (or `GOOGLE_API_KEY`) in env too;
    /// `requires_env()` hides the tools when neither is set even
    /// with this flag flipped on.
    #[serde(default)]
    pub image_tools_enabled: bool,

    /// Opt-in flag for the HAL Public-API tools (`YouTubeTranscript`,
    /// `WebScrape`). Off by default — same posture as the media tools:
    /// they call a hosted service on the user's `HAL_API_KEY` (or the
    /// gateway). `requires_env()` still hides them when no key is set
    /// even with this flag on.
    #[serde(default, alias = "halEnabled")]
    pub hal_enabled: bool,

    /// dev-plan/55: detect Thai PII (national ID, phone, plate, titled
    /// names, user dictionary) and swap it for `[ID_1]`-style placeholders
    /// before text leaves the machine, restoring the real values in the
    /// reply. Off by default — the detector (`crate::sensitive`) is tuned
    /// for precision over recall, but masking rewrites the prompt, so it
    /// stays opt-in. settings.json `sensitive.enabled`.
    #[serde(default)]
    pub sensitive_enabled: bool,

    /// Engine-managed browser automation (docs/browser, Phase 0+1).
    /// When `true`, `AppConfig::load()` injects the official Playwright
    /// MCP server as a synthetic engine-managed stdio config named
    /// `browser` — the agent gets `browser.*` tools (navigate / click /
    /// snapshot / …) with no `/mcp add` or first-spawn approval.
    /// ON by default since 0.49.2; the injection is skipped silently
    /// when the launch command isn't on PATH (node-less desktops), and
    /// `"browserEnabled": false` opts out entirely.
    #[serde(default = "default_browser_enabled", alias = "browserEnabled")]
    pub browser_enabled: bool,

    /// Force headed/headless for the managed browser. `None` (default)
    /// = auto: headless on cloud runners (`THCLAWS_USES_GATEWAY=1`) and
    /// displayless Linux; headed elsewhere (the desktop "browse next to
    /// the agent" workflow).
    #[serde(default, alias = "browserHeadless")]
    pub browser_headless: Option<bool>,

    /// Per-provider gateway routing. Each entry is a provider name
    /// (lowercase, matches the gateway path segment): `openai`,
    /// `anthropic`, `google`, `openrouter`. When the active model's
    /// `ProviderKind` matches one of these names AND the gateway
    /// access key is available (keychain or `THCLAWS_GATEWAY_API_KEY`
    /// env var), the provider's HTTP layer rewrites its base URL +
    /// auth header to route through the gateway. The gateway base URL
    /// itself is fixed at `crate::providers::thclaws_gateway::GATEWAY_BASE_URL`
    /// — see that module for the staging override env var.
    #[serde(default)]
    pub gateway_use_for: Vec<String>,
    /// Single source of truth for the desktop proxy toggle: when true (and a
    /// CLI access token is present), every gateway-routable provider routes
    /// through the thClaws Gateway; when false, pure BYOK. `gateway_use_for`
    /// above is DERIVED from this in `load()` — never edited directly by the
    /// UI. Per-model eligibility (Featured + priced) is enforced at routing
    /// time, so a non-featured model falls back to BYOK even when this is on.
    #[serde(default)]
    pub gateway_proxy: bool,

    /// Per-skill model recommendations from settings.json. Overrides the
    /// `model:` field declared in the SKILL.md frontmatter for the named
    /// built-in skill. Lets users say "for my extract-and-save runs use
    /// claude-sonnet-4-6 not the gpt-4.1-nano default" without forking
    /// the entire SKILL.md body. Each new built-in that needs special
    /// model selection adds its own field here (e.g. `tts_skill_models`,
    /// `transcribe_skill_models`) — that pattern is more discoverable
    /// in settings.json than a generic `skill_models` map keyed by
    /// skill name.
    #[serde(default)]
    pub extract_save_skill_models: Option<crate::skills::SkillModelSpec>,

    /// Last-resort model for a skill's `model:` recommendation. Skills
    /// ship a model id in their frontmatter and vendors retire ids, so
    /// an older skill can name a model that no longer exists — swapping
    /// onto it 400s the whole turn. When every candidate is unknown to
    /// the catalogue (or has no usable key), this one is tried before
    /// giving up and keeping the session's own model. Same usability
    /// test as any candidate: unusable ⇒ skipped, never a hard failure.
    #[serde(default)]
    pub skill_model_fallback: Option<String>,

    /// Override the model for the built-in `translator` subagent.
    /// AgentDef.model is a single string (no priority list), so this
    /// is `Option<String>`, not `SkillModelSpec`. When set, the
    /// embedded translator.md's `model: gpt-4.1` is replaced before
    /// the AgentDef is registered with the factory; absent leaves the
    /// embedded default in place. Same per-agent named-field
    /// convention as `extract_save_skill_models` — future built-in
    /// subagents (dream, etc.) get `<name>_subagent_model` fields of
    /// their own rather than a generic map.
    #[serde(default)]
    pub translator_subagent_model: Option<String>,

    /// Default target URL for the `/deploy` slash command (dev-plan/28).
    /// Paired with the `remote-agent-token` keychain entry. Both can be
    /// overridden per-invocation with `--pod` / `--token`. Not
    /// sensitive (URL only) — token sits in the keychain.
    #[serde(default, alias = "remoteAgentUrl")]
    pub remote_agent_url: Option<String>,

    /// GUI Shell defaults (dev-plan/33 Tier 2). Two forms accepted:
    ///
    /// - **Shorthand** — `"guiShell": "session-explorer"` applies to
    ///   both the GUI Shell tab default and the `--serve --gui-shell`
    ///   fallback.
    /// - **Long form** — `"guiShell": { "tabDefault": "session-explorer",
    ///   "serveDefault": "image-generator" }` lets the two differ.
    ///
    /// `tabDefault` (when set) causes the Shell tab to auto-open that
    /// shell instead of showing the picker. `serveDefault` is read by
    /// `--serve` when no `--gui-shell` CLI flag is passed (Tier 2
    /// Task 14 wiring).
    #[serde(default, alias = "guiShell")]
    pub gui_shell: Option<GuiShellSetting>,

    /// Configuration for the `openrouter/fusion+` pseudo-model. Always
    /// present (defaulted); the `outerModel` / `analysisModels` etc. are
    /// only consulted when the active model is [`FUSION_PLUS_MODEL`].
    #[serde(default, alias = "openrouterFusion")]
    pub openrouter_fusion: FusionConfig,
}

/// Accepts both the string shorthand and the structured long form so
/// `settings.json` stays terse for users who don't need to split tab
/// vs serve defaults.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum GuiShellSetting {
    /// `"guiShell": "session-explorer"` — same id used for both modes.
    Shorthand(String),
    /// Long form with separate per-mode defaults.
    Long {
        #[serde(default, alias = "tabDefault")]
        tab_default: Option<String>,
        #[serde(default, alias = "serveDefault")]
        serve_default: Option<String>,
    },
}

impl GuiShellSetting {
    /// Shell id to auto-open in the GUI Shell tab (`None` → show picker).
    pub fn tab_default(&self) -> Option<&str> {
        match self {
            GuiShellSetting::Shorthand(s) => Some(s.as_str()),
            GuiShellSetting::Long { tab_default, .. } => tab_default.as_deref(),
        }
    }

    /// Shell id to fall back to when `--serve` is launched without
    /// `--gui-shell` (Task 14 consumer).
    pub fn serve_default(&self) -> Option<&str> {
        match self {
            GuiShellSetting::Shorthand(s) => Some(s.as_str()),
            GuiShellSetting::Long { serve_default, .. } => serve_default.as_deref(),
        }
    }
}

/// Pseudo-model id for the configurable OpenRouter Fusion variant. Unlike
/// the bare `openrouter/fusion` (which uses OpenRouter's default panel),
/// selecting this routes through the user's [`FusionConfig`]: the engine
/// calls `outer_model` with the `openrouter:fusion` tool attached, carrying
/// the configured panel / judge / limits.
pub const FUSION_PLUS_MODEL: &str = "openrouter/fusion+";

/// OpenRouter Fusion (`openrouter/fusion+`) configuration. The inner fields
/// map 1:1 to the snake_case keys the `openrouter:fusion` tool expects; only
/// fields the user actually set are emitted (empty / `None` ⇒ OpenRouter
/// defaults). See the OpenRouter Fusion router docs for semantics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FusionConfig {
    /// thClaws-form model id (`openrouter/<vendor>/<model>`) used as the
    /// outer / orchestrator call. The `openrouter/` prefix is stripped
    /// before the wire request like any other OpenRouter model.
    #[serde(rename = "outerModel")]
    pub outer_model: String,
    /// Panel models — OpenRouter ids (e.g. `anthropic/claude-opus-4.8` or
    /// the floating `~anthropic/claude-opus-latest`). Empty ⇒ omitted ⇒
    /// OpenRouter's default quality preset (Opus + GPT + Gemini). 1–8.
    #[serde(rename = "analysisModels")]
    pub analysis_models: Vec<String>,
    /// Judge model that synthesizes the structured analysis. `None` ⇒
    /// defaults to the outer model.
    #[serde(rename = "judgeModel", skip_serializing_if = "Option::is_none")]
    pub judge_model: Option<String>,
    /// Max tool-calling steps per panel / judge call (1–16). `None` ⇒ 8.
    #[serde(rename = "maxToolCalls", skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// Max output tokens (incl. reasoning) per inner call. `None` ⇒
    /// provider default.
    #[serde(
        rename = "maxCompletionTokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_completion_tokens: Option<u32>,
    /// Sampling temperature (0–2) forwarded to panel + judge. `None` ⇒
    /// provider default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Reasoning config forwarded to panel + judge — `{effort?, max_tokens?}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<serde_json::Value>,
    /// `"auto"` (the outer model decides when to call fusion — coexists with
    /// the agent's own tools) or `"required"` (force the panel every turn).
    #[serde(rename = "toolChoice")]
    pub tool_choice: String,
}

impl Default for FusionConfig {
    fn default() -> Self {
        Self {
            outer_model: "openrouter/openai/gpt-4.1".to_string(),
            analysis_models: Vec::new(),
            judge_model: None,
            max_tool_calls: None,
            max_completion_tokens: None,
            temperature: None,
            reasoning: None,
            tool_choice: "auto".to_string(),
        }
    }
}

impl FusionConfig {
    /// Build the `openrouter:fusion` tool object for the request `tools`
    /// array. Omits the `parameters` block entirely when nothing was set
    /// (equivalent to the default panel, but with our outer model).
    pub fn tool_json(&self) -> serde_json::Value {
        let mut params = serde_json::Map::new();
        let models: Vec<&String> = self
            .analysis_models
            .iter()
            .filter(|m| !m.trim().is_empty())
            .collect();
        if !models.is_empty() {
            params.insert("analysis_models".into(), serde_json::json!(models));
        }
        if let Some(m) = self.judge_model.as_ref().filter(|m| !m.trim().is_empty()) {
            params.insert("model".into(), serde_json::json!(m));
        }
        if let Some(n) = self.max_tool_calls {
            params.insert("max_tool_calls".into(), serde_json::json!(n));
        }
        if let Some(n) = self.max_completion_tokens {
            params.insert("max_completion_tokens".into(), serde_json::json!(n));
        }
        if let Some(t) = self.temperature {
            params.insert("temperature".into(), serde_json::json!(t));
        }
        if let Some(r) = &self.reasoning {
            params.insert("reasoning".into(), r.clone());
        }
        let mut tool = serde_json::json!({ "type": "openrouter:fusion" });
        if !params.is_empty() {
            tool["parameters"] = serde_json::Value::Object(params);
        }
        tool
    }

    /// `tool_choice` body value, or `None` to omit (let the model decide).
    /// Only `"required"` is emitted — `"auto"` is OpenRouter's default.
    pub fn tool_choice_value(&self) -> Option<serde_json::Value> {
        match self.tool_choice.trim() {
            "required" => Some(serde_json::json!("required")),
            _ => None,
        }
    }
}

/// Default stream-chunk idle timeout. Used by `serde(default = ...)`
/// for backward-compat with settings files that pre-date the field.
fn default_stream_chunk_timeout_secs() -> u64 {
    120
}

fn default_auto_learn_kms() -> String {
    "self_learn".to_string()
}

/// Browser automation default — ON since 0.49.2 (docs/browser). The
/// injection in `AppConfig::load()` still degrades gracefully: it's
/// skipped when the launch command isn't on PATH, so node-less
/// desktops see the Browser tab's setup hint instead of spawn errors.
fn default_browser_enabled() -> bool {
    // `THCLAWS_BROWSER_ENABLED=0` flips the DEFAULT off for a whole
    // fleet without touching anyone's settings.json — cloud runners set
    // it, because the managed Chromium is spawned at startup whether or
    // not a workspace ever asks for a browser tool, and it OOM-killed a
    // runner that never did. This is only the default: a workspace that
    // sets `"browserEnabled": true` still gets it, since project/user
    // settings are layered on top of this value.
    !matches!(
        std::env::var("THCLAWS_BROWSER_ENABLED")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// Resolve a launch command on PATH (or as an absolute path). Shared
/// by the browser-MCP injection guard and the `browser_status_get`
/// IPC arm so both agree on whether the managed browser can start.
/// `.cmd` is the Windows npm-shim extension.
pub fn command_on_path(command: &str) -> bool {
    let p = std::path::Path::new(command);
    if p.is_absolute() {
        return p.is_file();
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .any(|d| d.join(command).is_file() || d.join(format!("{command}.cmd")).is_file())
        })
        .unwrap_or(false)
}

fn default_auto_learn_reconcile_hours() -> u32 {
    6
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            model: "claude-sonnet-4-6".to_string(),
            // 32K leaves room for a full HTML page / long markdown doc in
            // one turn. Auto-escalates to 64K (ESCALATED_MAX_TOKENS) if the
            // model hits the cap mid-turn.
            max_tokens: 32000,
            permissions: "auto".to_string(),
            system_prompt: String::new(),
            // 10K thinking budget suits the "design a small component"
            // class of task without burning budget on trivial edits.
            thinking_budget: None,
            stream_chunk_timeout_secs: default_stream_chunk_timeout_secs(),
            search_engine: "auto".to_string(),
            allowed_tools: None,
            disallowed_tools: None,
            ask_tools: None,
            resume_session: None,
            hooks: crate::hooks::HooksConfig::default(),
            bash_sandbox: "workspace".to_string(),
            bash_sandbox_write_paths: Vec::new(),
            bash_sandbox_deny_read: Vec::new(),
            // 50 tool-use rounds is enough for everything short of
            // teammate-orchestrated multi-agent flows, and surfaces
            // runaway loops earlier than the old 200.
            max_iterations: 50,
            // M6.2 compact-between-steps is the safe default. M6.4
            // opt-in `clear` requires explicit project config.
            plan_context_strategy: "compact".to_string(),
            // dev-plan/06 P2: "full" is the safe default — preserves
            // pre-P2 behavior. Power users with many skills can opt
            // into "names-only" or "discover-tool-only".
            skills_listing_strategy: "full".to_string(),
            mcp_servers: Vec::new(),
            kms_active: Vec::new(),
            auto_learn: false,
            session_reset_per_turn: false,
            auto_learn_kms: default_auto_learn_kms(),
            auto_learn_reconcile_hours: default_auto_learn_reconcile_hours(),
            claude_md_compat: false,
            openrouter_free_only: false,
            image_tools_enabled: false,
            hal_enabled: false,
            sensitive_enabled: false,
            browser_enabled: default_browser_enabled(),
            browser_headless: None,
            gateway_use_for: Vec::new(),
            gateway_proxy: false,
            extract_save_skill_models: None,
            skill_model_fallback: Some(DEFAULT_SKILL_MODEL_FALLBACK.to_string()),
            translator_subagent_model: None,
            remote_agent_url: None,
            gui_shell: None,
            openrouter_fusion: FusionConfig::default(),
        }
    }
}

/// Permissions field: accepts both string ("auto"/"ask") and Claude Code's
/// object format (`{"allow": ["Read", "Bash(*)"], "deny": ["WebFetch"]}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PermissionsConfig {
    /// Simple mode string: "auto" or "ask".
    Mode(String),
    /// Claude Code style: allow/deny lists with optional glob patterns.
    Rules {
        #[serde(default)]
        allow: Vec<String>,
        #[serde(default)]
        deny: Vec<String>,
    },
}

impl PermissionsConfig {
    /// Resolve to a permission mode string.
    /// If allow list is non-empty, treat as "auto" (tools are pre-approved).
    pub fn mode(&self) -> &str {
        match self {
            Self::Mode(s) => s.as_str(),
            Self::Rules { allow, .. } => {
                if allow.is_empty() {
                    "ask"
                } else {
                    "auto"
                }
            }
        }
    }

    /// Extract allowed tool names (stripping glob patterns like "Bash(*)").
    pub fn allowed_tools(&self) -> Option<Vec<String>> {
        match self {
            Self::Mode(_) => None,
            Self::Rules { allow, .. } if allow.is_empty() => None,
            Self::Rules { allow, .. } => {
                Some(
                    allow
                        .iter()
                        .map(|s| {
                            // "Bash(*)" → "Bash", "Read" → "Read"
                            if let Some(idx) = s.find('(') {
                                s[..idx].to_string()
                            } else {
                                s.clone()
                            }
                        })
                        .collect(),
                )
            }
        }
    }

    /// Extract denied tool names.
    pub fn disallowed_tools(&self) -> Option<Vec<String>> {
        match self {
            Self::Mode(_) => None,
            Self::Rules { deny, .. } if deny.is_empty() => None,
            Self::Rules { deny, .. } => Some(
                deny.iter()
                    .map(|s| {
                        if let Some(idx) = s.find('(') {
                            s[..idx].to_string()
                        } else {
                            s.clone()
                        }
                    })
                    .collect(),
            ),
        }
    }
}

/// Project-level config stored in `.thclaws/settings.json`.
///
/// Also loads `.thclaws/mcp.json` for project-level MCP servers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    pub model: Option<String>,
    /// Accepts "auto", "ask", or {"allow": [...], "deny": [...]}.
    pub permissions: Option<PermissionsConfig>,
    /// Lifecycle hooks (ch13): shell snippets fired on agent events.
    /// Was documented but never deserialized — the key silently dropped
    /// here, so `config.hooks` stayed default() and no hook ever ran
    /// (issue #180). `skip_serializing_if` keeps `save()`'s overlay from
    /// stamping `"hooks": null` into settings files that don't use them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<crate::hooks::HooksConfig>,
    #[serde(rename = "maxTokens")]
    pub max_tokens: Option<u32>,
    #[serde(rename = "maxIterations")]
    pub max_iterations: Option<usize>,
    /// M6.4 opt-in: plan-mode context strategy at step boundaries.
    /// Accepts `"compact"` (default — see AppConfig docs) or `"clear"`.
    /// Anything else falls back to `"compact"`.
    #[serde(rename = "planContextStrategy")]
    pub plan_context_strategy: Option<String>,
    /// dev-plan/06 P2: how installed skills are surfaced in the
    /// system prompt. Accepts `"full"` (default), `"names-only"`,
    /// `"discover-tool-only"`. Anything else falls back to `"full"`.
    #[serde(rename = "skillsListingStrategy")]
    pub skills_listing_strategy: Option<String>,
    /// Per-skill model override for the built-in `extract-and-save`
    /// skill. Single string or array (priority list). When set, takes
    /// precedence over the SKILL.md frontmatter `model:` field. See
    /// `AppConfig::extract_save_skill_models` for the design rationale
    /// (per-skill named fields scale better than a generic map for
    /// the small set of built-in skills with special model needs).
    #[serde(rename = "extract_save_skill_models", alias = "extractSaveSkillModels")]
    pub extract_save_skill_models: Option<crate::skills::SkillModelSpec>,
    /// Last-resort model when a skill's recommended model is unknown or
    /// unusable. Defaults to `gpt-4.1-mini`; set to `""`
    /// to disable the fallback and keep the session's model instead.
    #[serde(rename = "skill_model_fallback", alias = "skillModelFallback")]
    pub skill_model_fallback: Option<String>,
    /// Override the model for the built-in `translator` subagent. See
    /// `AppConfig::translator_subagent_model` for design rationale.
    #[serde(
        rename = "translator_subagent_model",
        alias = "translatorSubagentModel"
    )]
    pub translator_subagent_model: Option<String>,
    #[serde(rename = "thinkingBudget")]
    pub thinking_budget: Option<u32>,
    #[serde(rename = "searchEngine")]
    pub search_engine: Option<String>,
    /// Tool names allowed (flat list, thClaws native format).
    #[serde(rename = "allowedTools")]
    pub allowed_tools: Option<Vec<String>>,
    /// Tool names disallowed (flat list, thClaws native format).
    #[serde(rename = "disallowedTools")]
    pub disallowed_tools: Option<Vec<String>>,
    /// Tool names that always prompt for approval even under
    /// `permissions: "auto"` (interactive surfaces only — see the
    /// `AppConfig::ask_tools` docs).
    #[serde(rename = "askTools")]
    pub ask_tools: Option<Vec<String>>,
    /// GUI window width (logical pixels). When `None`, the GUI picks
    /// a size at startup based on the primary monitor's logical
    /// resolution: 1760×962 on workstation-class displays
    /// (≥1920×1080) and 1200×800 on smaller / laptop screens. See
    /// `gui::run_gui_inner` for the resolution logic.
    #[serde(rename = "windowWidth")]
    pub window_width: Option<f64>,
    /// GUI window height (logical pixels). See `window_width` for the
    /// conditional-default behavior — both fields share the same
    /// monitor-resolution-based fallback.
    #[serde(rename = "windowHeight")]
    pub window_height: Option<f64>,
    /// User-controlled GUI zoom multiplier applied via wry's
    /// `WebView::zoom()` so HiDPI / 4K displays can be tuned without
    /// changing OS-level display scaling. `None` (default) leaves the
    /// WebView at its native 1.0 scale; values typically range from
    /// 0.75 to 2.0. Persisted on every change made through the
    /// Settings panel. Issue #47.
    #[serde(rename = "guiScale")]
    pub gui_scale: Option<f64>,
    /// Enable the Agent Teams feature (TeamCreate, SpawnTeammate, SendMessage,
    /// CheckInbox, TeamTask*, TeamMerge, lead coordination prompt, inbox
    /// poller, GUI Team tab). Off by default because teams spin up multiple
    /// concurrent agent processes and can burn tokens quickly.
    ///
    /// This flag ONLY affects Agent Teams. The `Task` sub-agent tool stays
    /// enabled either way — subagents run in-process as a single recursive
    /// agent and don't spawn parallel processes, so they don't share the
    /// token-burn concern that motivated making Teams opt-in.
    #[serde(
        rename = "teamEnabled",
        deserialize_with = "null_team_enabled_is_false"
    )]
    pub team_enabled: Option<bool>,
    /// Opt-in flag for the GUI's PTY-backed `Shell` tab. Default off
    /// because the tab gives the user an unsandboxed live shell with
    /// no agent-side permission gating — fine for power users, easy
    /// to footgun for someone new to the tool. Flip to `true` to
    /// surface the tab; the agent-rendered `Terminal` tab and the
    /// iframe-based `UI` tab are always available regardless.
    #[serde(rename = "shellTabEnabled")]
    pub shell_tab_enabled: Option<bool>,
    /// Opt-in flag for the built-in media tools — `TextToImage`,
    /// `ImageToImage`, `TextToVideo`, `ImageToVideo`, `MediaJobStatus`
    /// (dev-plan/40). Gates them for the agent and for shells' direct
    /// `callTool` path; the built-in Media Studio shell auto-enables them
    /// regardless (it's the media on-ramp). Accepts either
    /// `mediaToolsEnabled` (preferred) or the legacy `imageToolsEnabled`.
    #[serde(rename = "imageToolsEnabled", alias = "mediaToolsEnabled")]
    pub image_tools_enabled: Option<bool>,
    /// Opt-in flag for the HAL Public-API tools (`YouTubeTranscript`,
    /// `WebScrape`). See [`AppConfig::hal_enabled`].
    #[serde(rename = "halEnabled")]
    pub hal_enabled: Option<bool>,
    /// Sensitive-data masking — `{ "enabled": true }`. Nested (not a bare
    /// flag) because dev-plan/55 §6 adds mode routing here later:
    /// tokenize (mask + restore) vs gate (refuse to send).
    pub sensitive: Option<SensitiveSettings>,
    /// Engine-managed Playwright browser automation. See
    /// [`AppConfig::browser_enabled`].
    #[serde(rename = "browserEnabled")]
    pub browser_enabled: Option<bool>,
    /// Headed/headless override for the managed browser. See
    /// [`AppConfig::browser_headless`].
    #[serde(rename = "browserHeadless")]
    pub browser_headless: Option<bool>,
    /// Show the desktop SSO sign-in button (Google / enterprise IdP). Off by
    /// default — the sign-in flow isn't wired to a usable feature yet, so the
    /// button stays hidden until this is set true in `.thclaws/settings.json`.
    #[serde(rename = "ssoSignInEnabled")]
    pub sso_sign_in_enabled: Option<bool>,
    /// Print the assistant's raw text to stderr after each turn (dim, fenced
    /// block). Same effect as `THCLAWS_SHOW_RAW=1`. The env var wins if set.
    /// Useful when debugging model output / formatting issues.
    #[serde(rename = "showRawResponse")]
    pub show_raw_response: Option<bool>,
    /// Knowledge-base settings — `{ "active": ["name1", ...] }`.
    pub kms: Option<KmsSettings>,
    /// dev-plan/49 OS-level Bash confinement —
    /// `{ "sandbox": "workspace", "sandbox_write_paths": [...], ... }`.
    pub bash: Option<BashSettings>,
    /// Auto-learn — file each ended session as a page in a dedicated
    /// KMS and periodically reconcile it. See
    /// [`AppConfig::auto_learn`] for the full design. Default off
    /// (None ⇒ false).
    #[serde(rename = "autoLearn")]
    pub auto_learn: Option<bool>,
    /// Override the default auto-learn KMS name (`self_learn`). See
    /// [`AppConfig::auto_learn_kms`].
    #[serde(rename = "autoLearnKms")]
    pub auto_learn_kms: Option<String>,
    /// Minimum hours between auto-learn reconcile passes. See
    /// [`AppConfig::auto_learn_reconcile_hours`].
    #[serde(rename = "autoLearnReconcileHours")]
    pub auto_learn_reconcile_hours: Option<u32>,
    /// When set, applies to AppConfig.openrouter_free_only on load.
    /// Stored as Option so a missing field falls through to the
    /// compiled default (`false`).
    #[serde(rename = "openrouterFreeOnly")]
    pub openrouter_free_only: Option<bool>,
    /// Provider names to route through the thClaws Gateway: any
    /// subset of `["openai", "anthropic", "google", "openrouter"]`.
    /// Base URL is fixed (see
    /// `crate::providers::thclaws_gateway::GATEWAY_BASE_URL`); only
    /// per-provider opt-in lives in user-visible config.
    #[serde(rename = "gatewayUseFor")]
    pub gateway_use_for: Option<Vec<String>>,
    /// Desktop proxy toggle (single flag). `true` = route every gateway-routable
    /// provider through the thClaws Gateway; `false`/absent = BYOK. Supersedes
    /// the legacy `gatewayUseFor` list (which is migrated to `true` when
    /// non-empty). Written by the GUI proxy checkbox.
    #[serde(rename = "gatewayProxy")]
    pub gateway_proxy: Option<bool>,
    /// Default target for `/deploy`. See
    /// [`AppConfig::remote_agent_url`].
    #[serde(rename = "remoteAgentUrl")]
    pub remote_agent_url: Option<String>,
    /// Telegram adapter binding (dev-plan/29). When present, a repo can
    /// ship a bot config alongside its agents. The bot **token** still
    /// resolves env-first (`TELEGRAM_BOT_TOKEN`) per
    /// [`crate::telegram::TelegramConfig::resolved_token`], so a token
    /// need never be committed here. The user-runtime
    /// `~/.config/thclaws/telegram.json` is the GUI's source of truth;
    /// this project layer is read at load for headless / shipped setups.
    pub telegram: Option<crate::telegram::TelegramConfig>,
    /// thClaws.cloud catalog client binding (dev-plan/34). When set,
    /// `thclaws cloud {login, publish, get, list}` talks to this URL
    /// instead of the public `https://thclaws.cloud` default. Override
    /// at runtime with `--cloud-url` or `THCLAWS_CLOUD_URL`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud: Option<crate::cloud::CloudConfig>,
    /// Agent identity (dev-plan/34 Option A). This folder's
    /// authoritative `{id, name, description, uuid}`. The UUID is
    /// server-assigned on first `cloud publish` and written back here so
    /// subsequent publishes update the same catalog entry. The engine
    /// surfaces `agent.name` in the GUI title bar / CLI prompt so the
    /// user always knows which agent they're running.
    ///
    /// Identity moved out of `manifest.json` to a single source of
    /// truth — `manifest.json` keeps version, pricing, requires,
    /// permissions, etc. CLI fuses both at publish time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentConfig>,
    /// dev-plan/33 Tier 2 — pin a default GUI Shell for the UI tab
    /// (`tabDefault`) and/or `--serve` (`serveDefault`). See
    /// [`AppConfig::gui_shell`] for the parsed shape. Without this
    /// field on ProjectConfig, serde silently drops the JSON block on
    /// deserialize, so `guiShell.tabDefault` in `.thclaws/settings.json`
    /// never reaches the picker.
    #[serde(rename = "guiShell", skip_serializing_if = "Option::is_none")]
    pub gui_shell: Option<GuiShellSetting>,
    /// Configuration for the `openrouter/fusion+` pseudo-model. See
    /// [`AppConfig::openrouter_fusion`]. Absent ⇒ compiled defaults.
    #[serde(rename = "openrouterFusion", skip_serializing_if = "Option::is_none")]
    pub openrouter_fusion: Option<FusionConfig>,
    /// Workspace layout schema version for this project's `.thclaws/`.
    /// `2` = runtime state consolidated under `.thclaws/state/` (sessions,
    /// team, todos, kms, workflows, …); absent / `< 2` = legacy flat layout
    /// that [`ProjectConfig::migrate_workspace_if_needed`] moves on open.
    /// Read RAW from the project file only (never the merged `AppConfig`) —
    /// it describes this physical directory, not layered config, so a
    /// user-level flag must NOT make every project look migrated.
    #[serde(rename = "workspaceVersion", skip_serializing_if = "Option::is_none")]
    pub workspace_version: Option<u32>,
}

/// On-disk shape of the `agent` block in `./.thclaws/settings.json`.
/// See [`crate::config::ProjectConfig::agent`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentConfig {
    /// URL-safe slug used as the catalog path component
    /// (`/a/<id>` on thclaws.cloud) and as the folder's stable name.
    /// User-chosen; lowercase letters/digits/hyphens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Display name shown in the catalog grid + GUI title bar.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Short pitch (≤500 chars). Shown on the agent detail page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Server-assigned UUID. Empty/absent on a fresh local agent.
    /// `cloud publish` populates it from the server's response and
    /// reads it on subsequent publishes to identify the same agent
    /// (even if the folder is renamed). `cloud unbind` clears it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
}

fn null_team_enabled_is_false<'de, D>(d: D) -> std::result::Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<bool>::deserialize(d)?.unwrap_or(false)))
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            model: None,
            permissions: None,
            hooks: None,
            max_tokens: None,
            max_iterations: None,
            plan_context_strategy: None,
            skills_listing_strategy: None,
            extract_save_skill_models: None,
            skill_model_fallback: None,
            translator_subagent_model: None,
            thinking_budget: None,
            search_engine: None,
            allowed_tools: None,
            disallowed_tools: None,
            ask_tools: None,
            window_width: None,
            window_height: None,
            gui_scale: None,
            team_enabled: Some(false),
            shell_tab_enabled: Some(false),
            image_tools_enabled: Some(false),
            hal_enabled: Some(false),
            sensitive: None,
            browser_enabled: None,
            browser_headless: None,
            sso_sign_in_enabled: None,
            show_raw_response: None,
            kms: None,
            bash: None,
            auto_learn: None,
            auto_learn_kms: None,
            auto_learn_reconcile_hours: None,
            openrouter_free_only: None,
            gateway_use_for: None,
            gateway_proxy: None,
            remote_agent_url: None,
            telegram: None,
            cloud: None,
            agent: None,
            gui_shell: None,
            openrouter_fusion: None,
            workspace_version: None,
        }
    }
}

/// On-disk shape of the KMS block in `.thclaws/settings.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KmsSettings {
    /// Names of KMS attached to this project's chats. Multi-select:
    /// every name in the list gets its `index.md` spliced into the
    /// system prompt.
    pub active: Vec<String>,
}

impl AppConfig {
    /// Push the settings that live in process-wide state, rather than being
    /// read from an `AppConfig` at use time. Call once per surface right
    /// after loading (REPL, `-p`, GUI/serve worker) and again whenever the
    /// settings file is re-read.
    ///
    /// One function on purpose: these hooks used to be pasted per entry
    /// point, and both had already drifted — `-p` calls
    /// `run_print_mode_with` directly, so a hook on the `run_print_mode`
    /// wrapper armed nothing (dev-plan/55 masking shipped off in print mode,
    /// and `stream_chunk_timeout_secs` was never applied there at all).
    pub fn apply_process_globals(&self) {
        crate::providers::set_stream_chunk_timeout_secs(self.stream_chunk_timeout_secs);
        // dev-plan/55 PII masking. The custom-dictionary term list has no
        // settings field yet (plan step 1's CSV) — empty until it does.
        crate::sensitive::configure(self.sensitive_enabled, Vec::new());
    }
}

/// dev-plan/55: `sensitive` block in settings.json — PII masking before
/// text leaves the machine. See [`AppConfig::sensitive_enabled`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SensitiveSettings {
    pub enabled: Option<bool>,
}

/// dev-plan/49: `bash` block in settings.json — OS-level Bash confinement.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BashSettings {
    /// `off` | `workspace` | `strict`.
    pub sandbox: Option<String>,
    /// Extra paths the confined Bash may write (absolute or `~/`).
    pub sandbox_write_paths: Option<Vec<String>>,
    /// Extra paths the confined Bash must not read.
    pub sandbox_deny_read: Option<Vec<String>>,
}

/// Shallow-overlay merge for `save()`'s non-destructive write. Two
/// preservation rules:
///
///   - Keys in `target` but absent from `overlay` stay untouched
///     (`_doc` comments, unknown shorthand aliases like `guiShell`
///     that serde's `alias` only handles on deserialize, etc.).
///   - Keys in `overlay` whose value is JSON null are skipped.
///     ProjectConfig is field-heavy with `Option<T>` serialising
///     to null when unset; without this rule every save() would
///     balloon settings.json with a `"feature": null` line per
///     unset field, drowning the keys the user actually cares about.
///
/// Non-object inputs at the top level are ignored (settings.json is
/// always an object at root by convention).
fn overlay_object(target: &mut serde_json::Value, overlay: &serde_json::Value) {
    let (Some(t), Some(o)) = (target.as_object_mut(), overlay.as_object()) else {
        return;
    };
    for (k, v) in o {
        if v.is_null() {
            continue;
        }
        t.insert(k.clone(), v.clone());
    }
}

/// Parse a settings.json into ProjectConfig; on serde failure log a
/// one-line warning to stderr (with file path + serde's column/line
/// hint) and return None. Without this, a single trailing comma or
/// missing brace silently defaults every opt-in feature to off and
/// the user sees "I enabled `shellTabEnabled`/`teamEnabled`/… but
/// nothing happened." Observed in the wild 2026-06-03.
fn parse_or_warn(contents: &str, path: &std::path::Path) -> Option<ProjectConfig> {
    match serde_json::from_str::<ProjectConfig>(contents) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!(
                "[thclaws] {} parse failed: {e} — falling back to defaults. Every opt-in flag (teamEnabled, shellTabEnabled, …) will read as `false` until the file is valid JSON.",
                path.display()
            );
            None
        }
    }
}

impl ProjectConfig {
    /// Returns `<workspace>/.thclaws/`. Prefers `$THCLAWS_PROJECT_ROOT`
    /// (set by SpawnTeammate when spawning into a worktree subdirectory)
    /// so worktree teammates load the project's settings.json instead of
    /// looking under their worktree cwd and falling through to user
    /// config — same model as the sandbox's project-root resolution.
    /// Falls back to current_dir for standalone (non-team) invocations.
    fn project_dir() -> PathBuf {
        let root = match std::env::var("THCLAWS_PROJECT_ROOT") {
            Ok(s) if !s.is_empty() => PathBuf::from(s),
            _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        root.join(".thclaws")
    }

    /// Primary path: `.thclaws/settings.json`
    pub fn path() -> PathBuf {
        Self::project_dir().join("settings.json")
    }

    pub fn load() -> Option<Self> {
        // Try .thclaws/settings.json first.
        let json_path = Self::path();
        if json_path.exists() {
            let contents = std::fs::read_to_string(&json_path).ok()?;
            return parse_or_warn(&contents, &json_path);
        }
        // Try .claude/settings.json (Claude Code compat).
        let claude_path = std::env::current_dir().ok()?.join(".claude/settings.json");
        if claude_path.exists() {
            let contents = std::fs::read_to_string(&claude_path).ok()?;
            return parse_or_warn(&contents, &claude_path);
        }
        None
    }

    /// Persist this config to `.thclaws/settings.json`. Non-destructive
    /// when the file already exists: merges this struct's keys into the
    /// on-disk JSON via a top-level object overlay rather than writing
    /// the serialised `ProjectConfig` verbatim. Preserves:
    ///
    ///   - `_doc` and any other user comments at the top level
    ///   - Unknown keys (e.g. `guiShell` shorthand the model only sees
    ///     via the `alias` attribute on deserialize — without merge,
    ///     a `save()` after `load()` would drop it because serde
    ///     serialises by field name, not alias)
    ///   - The file's existing key ordering for known keys (we only
    ///     overwrite values)
    ///
    /// First-time save (no existing file) writes the struct verbatim.
    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let new_value = serde_json::to_value(self)?;
        let mut base = if path.exists() {
            let raw = std::fs::read(&path)?;
            // Unparseable existing file → start from empty {} rather
            // than fail outright (parse_or_warn already logs the
            // warning on the read side). Better to write a clean
            // overlay than to bail and leave bad JSON on disk.
            serde_json::from_slice::<serde_json::Value>(&raw)
                .unwrap_or_else(|_| serde_json::json!({}))
        } else {
            // Fresh file (e.g. `cloud get` extracting into a new
            // folder): start from empty so the overlay's null-skipping
            // produces a minimal settings.json with only the keys
            // the caller actually set.
            serde_json::json!({})
        };
        overlay_object(&mut base, &new_value);

        let s = serde_json::to_string_pretty(&base)?;
        std::fs::write(&path, s)?;
        Ok(())
    }

    /// First-run bootstrap: when neither `.thclaws/settings.json` nor a
    /// Claude Code fallback (`.claude/settings.json`) exists in the
    /// project, write `.thclaws/settings.json` with the project-launch
    /// defaults. Returns whether a file was written. Silent on I/O
    /// errors — bootstrap is best-effort and shouldn't kill startup.
    pub fn ensure_default_exists() -> bool {
        // `project_dir()` is `<root>/.thclaws`, with the root taken from
        // `THCLAWS_PROJECT_ROOT` before the cwd — the same place `path()`
        // and `load()` use.
        match Self::project_dir().parent() {
            Some(root) => Self::ensure_default_exists_in(root),
            None => false,
        }
    }

    /// The body of the first-run settings file: the documented template, with
    /// the gateway default applied when a credential is present. One source
    /// for both the create path and [`Self::backfill_defaults_in`], so an
    /// agent that arrived by migration cannot drift from one created new.
    fn default_settings_body() -> String {
        // Hand-rolled JSON enumerating every ProjectConfig field at
        // its default value so users discover available knobs by
        // opening the file rather than consulting the manual. Unknown
        // keys (`_doc`) are tolerated by the loader (no
        // `deny_unknown_fields`). Removing a field falls back to the
        // compiled-in default; null on an Option field has the same
        // effect. Keep this list in sync with `ProjectConfig` whenever
        // a field is added.
        let template = r#"{
  "_doc": "thClaws project settings. Every available field is listed below at its default value — change a value to override, or delete a field (or set it to null on Option fields) to inherit the global default. windowWidth/windowHeight default to a monitor-resolution-aware size picked at GUI startup (1760x962 on >=1920x1080 displays, 1200x800 otherwise) when left null. See user-manual ch10 for the field reference.",
  "_doc_model": "null = pick automatically from the first provider you have credentials for (DeepSeek → DashScope → OpenAI → Anthropic). Set an explicit id (e.g. \"claude-sonnet-4-6\") to pin one.",
  "_doc_askTools": "Tool names that ALWAYS prompt for approval even under permissions:auto (e.g. [\"Bash\",\"Write\"]). Interactive only — desktop GUI / CLI where a human can answer; ignored under -p, team agents, and multiuser pods (auto-approved, no one to ask).",
  "_doc_browserEnabled": "Managed Playwright/Chromium browser tools. NEW workspaces start with this OFF — headless Chromium is the heaviest thing the engine launches (it OOM-killed a 1Gi cloud runner that never used it), so it's opt-in per workspace rather than a cost every project pays. Set true to turn it on; delete the key to inherit the built-in default (on). Existing workspaces are unaffected.",
  "workspaceVersion": 2,
  "model": null,
  "permissions": "auto",
  "askTools": [],
  "maxTokens": 32000,
  "maxIterations": 50,
  "thinkingBudget": 10000,
  "searchEngine": "auto",
  "planContextStrategy": "compact",
  "skillsListingStrategy": "full",
  "teamEnabled": false,
  "shellTabEnabled": false,
  "imageToolsEnabled": false,
  "browserEnabled": false,
  "halEnabled": false,
  "showRawResponse": false,
  "allowedTools": null,
  "disallowedTools": null,
  "windowWidth": null,
  "windowHeight": null,
  "guiScale": null,
  "extract_save_skill_models": null,
  "skill_model_fallback": "gpt-4.1-mini",
  "translator_subagent_model": null,
  "claude_md_compat": false,
  "openrouterFreeOnly": false,
  "kms": { "active": [] }
}
"#;
        // First-run on a fresh folder + a usable gateway credential (cloud
        // login or a pasted gateway key) → default the proxy ON, so a
        // logged-in user gets a working session immediately instead of a
        // "no API key found for provider …" wall. Only ever written here at
        // first-run bootstrap, never onto an existing config, so it can't
        // flip a project the user deliberately set to BYOK; they can still
        // turn it off in Settings → thClaws.cloud. `has_access_key` is the
        // same gate the GUI uses to enable the proxy checkbox (cloud token /
        // gateway key — not network-validated here). Skipped under tests
        // (keychain/env-dependent → would be nondeterministic).
        if !cfg!(test) && crate::providers::thclaws_gateway::has_access_key() {
            Self::inject_default_gateway_proxy(template)
        } else {
            template.to_string()
        }
    }

    /// [`ensure_default_exists`] for a project folder other than the cwd —
    /// a bot created empty on a workspace shelf gets the same first-run file a
    /// new folder opened in the app does.
    pub fn ensure_default_exists_in(dir: &std::path::Path) -> bool {
        let path = dir.join(".thclaws").join("settings.json");
        if path.exists() {
            return false;
        }
        if dir.join(".claude/settings.json").exists() {
            return false;
        }
        let body = Self::default_settings_body();
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return false;
            }
            Self::ensure_state_scaffold(parent);
        }
        std::fs::write(&path, body).is_ok()
    }

    /// Add the template's missing keys to a settings file that already exists,
    /// and return the names added.
    ///
    /// A workspace migrated into an agent keeps whatever `settings.json` it
    /// had — often two keys — while an agent created new gets the full
    /// documented template. `ensure_default_exists_in` cannot close that gap:
    /// it returns early precisely because the file exists. So this fills the
    /// holes instead, and only the holes — a key the file already carries is
    /// left alone, an explicit `null` included, so a deliberate choice is
    /// never overwritten by a default.
    pub fn backfill_defaults_in(dir: &std::path::Path) -> Vec<String> {
        let none = Vec::new();
        let path = dir.join(".thclaws").join("settings.json");
        let Ok(raw) = std::fs::read(&path) else {
            return none;
        };
        let Ok(mut current) = serde_json::from_slice::<serde_json::Value>(&raw) else {
            // Unparseable: leave it alone rather than rewrite over whatever
            // the user has in there. `parse_or_warn` already reports it.
            return none;
        };
        let Ok(defaults) =
            serde_json::from_str::<serde_json::Value>(&Self::default_settings_body())
        else {
            return none;
        };
        let (Some(obj), Some(defaults)) = (current.as_object_mut(), defaults.as_object()) else {
            return none;
        };
        let mut added = Vec::new();
        for (k, v) in defaults {
            if !obj.contains_key(k) {
                obj.insert(k.clone(), v.clone());
                added.push(k.clone());
            }
        }
        if added.is_empty() {
            return added;
        }
        let Ok(body) = serde_json::to_string_pretty(obj) else {
            return none;
        };
        if std::fs::write(&path, body).is_err() {
            return none;
        }
        added
    }

    /// Insert `"gatewayProxy": true` into the first-run settings template.
    /// Anchored on the opening brace (not the model line) so it keeps
    /// working if the default model ever changes.
    fn inject_default_gateway_proxy(template: &str) -> String {
        template.replacen("{\n", "{\n  \"gatewayProxy\": true,\n", 1)
    }

    /// Current `.thclaws/` layout schema version. Bump when the physical
    /// directory layout changes and add a step to
    /// [`Self::migrate_workspace_if_needed`].
    pub const CURRENT_WORKSPACE_VERSION: u32 = 2;

    /// Runtime-state entries that lived at the top of `.thclaws/` before
    /// workspace v2 and now live under `.thclaws/state/`. Moved wholesale
    /// (dirs plus the loose files) on first open of a legacy workspace.
    /// `workflows/` is handled separately (see [`Self::migrate_legacy_workflows`])
    /// because it mixes authored `.js` (config) with per-run state.
    const LEGACY_STATE_ENTRIES: &'static [&'static str] = &[
        "sessions",
        "team",
        "todos.md",
        "phone-home.json",
        "kms",
        "schedule",
        "usage",
        "usage.jsonl",
        "media-jobs.jsonl",
        "cache",
        "browser-profile",
    ];

    /// Config-tier directory holding the agent's authored workflow
    /// scripts (`.js`). Deployable and swapped with the agent; seeded
    /// into the runtime `state/workflows/` on open by
    /// [`Self::seed_agent_workflows`].
    const AGENT_WORKFLOW_DIR: &'static str = "agent_workflow";

    /// Create `.thclaws/state/` and a `state/.gitignore` (`*`) so runtime
    /// state is never accidentally committed. Best-effort. `thclaws_dir`
    /// is the project's `.thclaws/`.
    fn ensure_state_scaffold(thclaws_dir: &std::path::Path) {
        let state = thclaws_dir.join("state");
        if std::fs::create_dir_all(&state).is_err() {
            return;
        }
        let gi = state.join(".gitignore");
        if !gi.exists() {
            let _ = std::fs::write(&gi, "*\n");
        }
    }

    /// Migrate a legacy flat `.thclaws/` to the v2 layout (runtime state
    /// under `state/`). Idempotent, presence-driven, move-not-delete: only
    /// entries that exist at the top level AND are absent under `state/`
    /// are moved, so a re-run heals a partially-migrated tree and never
    /// clobbers already-migrated data. Reads `workspaceVersion` RAW from
    /// the project `settings.json` — never the merged `AppConfig`, or a
    /// user-level flag would make every project look migrated. Skipped in
    /// multiuser serve pods (shared `/workspace/.thclaws/` would race and
    /// uses a different per-user layout).
    ///
    /// Call BEFORE [`Self::ensure_default_exists`] at every project-open
    /// hook. A brand-new workspace (no runtime dirs, no settings.json) is
    /// a no-op here — `ensure_default_exists` then mints a v2 default.
    pub fn migrate_workspace_if_needed() {
        if crate::workdir::is_multiuser() {
            return;
        }
        let dir = Self::project_dir();
        // Spill files (agent.rs `maybe_truncate_to_disk`) live under this
        // tree now so the model can read them back; nothing else sweeps them.
        // Once per project-open, never mid-session — the running session's
        // truncation footers have to keep naming files that still exist.
        crate::agent::prune_tool_output_dir(&dir);
        Self::migrate_workspace_at(&dir);
    }

    /// Migration core, parameterised by the project's `.thclaws/` dir so
    /// it can be exercised against a temp dir without touching process
    /// cwd or env. `migrate_workspace_if_needed` is the production entry.
    fn migrate_workspace_at(thclaws_dir: &std::path::Path) {
        let settings = thclaws_dir.join("settings.json");

        // An existing but unparseable settings.json means we can't trust
        // the version and must not risk clobbering it — do nothing.
        let existing: Option<ProjectConfig> = if settings.exists() {
            match std::fs::read_to_string(&settings) {
                Ok(raw) => match serde_json::from_str::<ProjectConfig>(&raw) {
                    Ok(c) => Some(c),
                    Err(_) => return,
                },
                Err(_) => return,
            }
        } else {
            None
        };

        let version = existing
            .as_ref()
            .and_then(|c| c.workspace_version)
            .unwrap_or(0);
        if version >= Self::CURRENT_WORKSPACE_VERSION {
            // Freshly cloned / deployed workspace already declares v2 —
            // ensure the scaffold and (re)seed authored workflows, then stop.
            if thclaws_dir.exists() {
                Self::ensure_state_scaffold(thclaws_dir);
                Self::seed_agent_workflows(thclaws_dir);
            }
            return;
        }

        // version < CURRENT: legacy or brand-new. Move whatever runtime
        // entries are present at the top level under `state/`.
        let state = thclaws_dir.join("state");
        let mut moved_any = false;
        for entry in Self::LEGACY_STATE_ENTRIES {
            let src = thclaws_dir.join(entry);
            let dst = state.join(entry);
            if src.exists() && !dst.exists() {
                if let Some(parent) = dst.parent() {
                    if std::fs::create_dir_all(parent).is_err() {
                        continue;
                    }
                }
                if std::fs::rename(&src, &dst).is_ok() {
                    moved_any = true;
                }
            }
        }
        moved_any |= Self::migrate_legacy_workflows(thclaws_dir);

        // Stamp the version only when this is an actual project (we moved
        // legacy state, or a settings.json already exists). A truly
        // brand-new empty dir is left for `ensure_default_exists` to
        // initialise as v2, so we don't scatter `.thclaws/` into arbitrary
        // cwds the user merely opened.
        if moved_any || existing.is_some() {
            Self::write_workspace_version(thclaws_dir);
            Self::ensure_state_scaffold(thclaws_dir);
            Self::seed_agent_workflows(thclaws_dir);
        }
    }

    /// Non-destructively set `workspaceVersion` in
    /// `thclaws_dir/settings.json`, preserving every other key (`_doc`,
    /// user config). Creates a minimal file when none exists. Best-effort.
    fn write_workspace_version(thclaws_dir: &std::path::Path) {
        let path = thclaws_dir.join("settings.json");
        let mut base = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let Some(obj) = base.as_object_mut() else {
            return;
        };
        obj.insert(
            "workspaceVersion".into(),
            serde_json::json!(Self::CURRENT_WORKSPACE_VERSION),
        );
        if std::fs::create_dir_all(thclaws_dir).is_err() {
            return;
        }
        if let Ok(s) = serde_json::to_string_pretty(&base) {
            let _ = std::fs::write(&path, s);
        }
    }

    /// Split a legacy `.thclaws/workflows/` into the v2 shape: authored
    /// `*.js` scripts move to `agent_workflow/` (config tier), per-run
    /// state subdirs move to `state/workflows/`. Presence-driven and
    /// move-not-delete like the main migration. Returns whether anything
    /// moved. Drops the legacy dir if it ends up empty.
    fn migrate_legacy_workflows(thclaws_dir: &std::path::Path) -> bool {
        let legacy = thclaws_dir.join("workflows");
        if !legacy.is_dir() {
            return false;
        }
        let authored = thclaws_dir.join(Self::AGENT_WORKFLOW_DIR);
        let runtime = thclaws_dir.join("state").join("workflows");
        let entries = match std::fs::read_dir(&legacy) {
            Ok(e) => e,
            Err(_) => return false,
        };
        let mut moved = false;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let is_js = path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("js");
            let dst_root = if is_js { &authored } else { &runtime };
            let dst = dst_root.join(&name);
            if dst.exists() || std::fs::create_dir_all(dst_root).is_err() {
                continue;
            }
            if std::fs::rename(&path, &dst).is_ok() {
                moved = true;
            }
        }
        // Best-effort: drop the legacy dir once we've emptied it.
        let _ = std::fs::remove_dir(&legacy);
        moved
    }

    /// Seed the runtime workflow dir from the agent's authored scripts:
    /// copy every `agent_workflow/*.js` into `state/workflows/`,
    /// overwriting (the authored copy is the source of truth). Per-run
    /// state subdirs under `state/workflows/` are untouched. No-op when
    /// `agent_workflow/` is absent. Best-effort.
    fn seed_agent_workflows(thclaws_dir: &std::path::Path) {
        let authored = thclaws_dir.join(Self::AGENT_WORKFLOW_DIR);
        if !authored.is_dir() {
            return;
        }
        let runtime = thclaws_dir.join("state").join("workflows");
        let entries = match std::fs::read_dir(&authored) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("js") {
                continue;
            }
            let Some(name) = path.file_name() else {
                continue;
            };
            if std::fs::create_dir_all(&runtime).is_err() {
                return;
            }
            let _ = std::fs::copy(&path, runtime.join(name));
        }
    }

    /// Replace the active-KMS list in `.thclaws/settings.json` and
    /// write it back. Preserves every other field that was already
    /// there. Creates the file if it doesn't exist yet.
    pub fn set_active_kms(active: Vec<String>) -> Result<()> {
        let mut current = Self::load().unwrap_or_default();
        current.kms = Some(KmsSettings { active });
        current.save()
    }

    /// Persist `name` as attached (idempotent). Used by `/research` so a
    /// KMS it just wrote into becomes the default target of the next run.
    pub fn attach_kms(name: &str) -> Result<bool> {
        let mut active = Self::load()
            .and_then(|c| c.kms)
            .map(|k| k.active)
            .unwrap_or_default();
        if active.iter().any(|n| n == name) {
            return Ok(false);
        }
        active.push(name.to_string());
        Self::set_active_kms(active)?;
        Ok(true)
    }

    /// After `kms::rename`: swap the attached name so the attachment
    /// follows the KMS. Returns whether anything changed.
    pub fn rename_attached_kms(old: &str, new: &str) -> Result<bool> {
        let mut active = Self::load()
            .and_then(|c| c.kms)
            .map(|k| k.active)
            .unwrap_or_default();
        let mut changed = false;
        for n in active.iter_mut() {
            if n == old {
                *n = new.to_string();
                changed = true;
            }
        }
        if changed {
            active.dedup();
            Self::set_active_kms(active)?;
        }
        Ok(changed)
    }

    /// Names attached on disk that the in-memory config doesn't know yet
    /// (a background `/research` attached its KMS). Merges them in and
    /// returns the additions so the caller can refresh tools/prompt.
    pub fn sync_kms_active(config: &mut AppConfig) -> Vec<String> {
        let on_disk = Self::load()
            .and_then(|c| c.kms)
            .map(|k| k.active)
            .unwrap_or_default();
        let mut added = Vec::new();
        for n in on_disk {
            if !config.kms_active.iter().any(|a| a == &n) {
                config.kms_active.push(n.clone());
                added.push(n);
            }
        }
        added
    }

    /// Merge overrides into an AppConfig (non-None fields win).
    pub fn apply_to(&self, config: &mut AppConfig) {
        if let Some(ref m) = self.model {
            config.model = crate::providers::ProviderKind::resolve_alias(m);
        }
        if let Some(ref h) = self.hooks {
            config.hooks = h.clone();
        }
        if let Some(ref p) = self.permissions {
            config.permissions = p.mode().to_string();
            // Claude Code style: {"allow": [...]} populates allowed_tools.
            if let Some(tools) = p.allowed_tools() {
                config.allowed_tools = Some(tools);
            }
            if let Some(tools) = p.disallowed_tools() {
                config.disallowed_tools = Some(tools);
            }
        }
        if let Some(n) = self.max_tokens {
            config.max_tokens = n;
        }
        if let Some(n) = self.max_iterations {
            config.max_iterations = n;
        }
        if let Some(ref s) = self.plan_context_strategy {
            // Validate at the merge boundary so unknown values are
            // ignored (rather than reaching the driver and causing a
            // silent fallback). The driver matches on this string.
            match s.as_str() {
                "compact" | "clear" => config.plan_context_strategy = s.clone(),
                _ => {
                    // Leave default; the warning surface here would be
                    // a one-time stderr print on load, but config.rs
                    // doesn't have a logging channel yet — defer.
                }
            }
        }
        if let Some(ref s) = self.skills_listing_strategy {
            // Same merge-boundary validation as plan_context_strategy.
            // Unknown values silently fall back to the default ("full").
            match s.as_str() {
                "full" | "names-only" | "discover-tool-only" => {
                    config.skills_listing_strategy = s.clone()
                }
                _ => {}
            }
        }
        if let Some(b) = self.thinking_budget {
            config.thinking_budget = Some(b);
        }
        if let Some(ref s) = self.search_engine {
            config.search_engine = s.clone();
        }
        // Flat allowedTools/disallowedTools (thClaws native format) — applied after
        // permissions.allow/deny so they can override.
        if let Some(ref tools) = self.allowed_tools {
            config.allowed_tools = Some(tools.clone());
        }
        if let Some(ref tools) = self.disallowed_tools {
            config.disallowed_tools = Some(tools.clone());
        }
        if let Some(ref tools) = self.ask_tools {
            config.ask_tools = Some(tools.clone());
        }
        if let Some(ref kms) = self.kms {
            config.kms_active = kms.active.clone();
        }
        if let Some(ref bash) = self.bash {
            if let Some(ref s) = bash.sandbox {
                config.bash_sandbox = s.clone();
            }
            if let Some(ref w) = bash.sandbox_write_paths {
                config.bash_sandbox_write_paths = w.clone();
            }
            if let Some(ref d) = bash.sandbox_deny_read {
                config.bash_sandbox_deny_read = d.clone();
            }
        }
        if let Some(b) = self.auto_learn {
            config.auto_learn = b;
        }
        if let Some(ref name) = self.auto_learn_kms {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                config.auto_learn_kms = trimmed.to_string();
            }
        }
        if let Some(h) = self.auto_learn_reconcile_hours {
            config.auto_learn_reconcile_hours = h;
        }
        if let Some(ref spec) = self.extract_save_skill_models {
            config.extract_save_skill_models = Some(spec.clone());
        }
        // Present-but-empty disables the fallback ("" → None); absent
        // leaves the built-in default in place.
        if let Some(ref m) = self.skill_model_fallback {
            config.skill_model_fallback = (!m.trim().is_empty()).then(|| m.trim().to_string());
        }
        if let Some(ref m) = self.translator_subagent_model {
            config.translator_subagent_model = Some(m.clone());
        }
        if let Some(b) = self.openrouter_free_only {
            config.openrouter_free_only = b;
        }
        if let Some(b) = self.image_tools_enabled {
            config.image_tools_enabled = b;
        }
        if let Some(b) = self.hal_enabled {
            config.hal_enabled = b;
        }
        if let Some(ref sensitive) = self.sensitive {
            if let Some(b) = sensitive.enabled {
                config.sensitive_enabled = b;
            }
        }
        if let Some(b) = self.browser_enabled {
            config.browser_enabled = b;
        }
        if let Some(b) = self.browser_headless {
            config.browser_headless = Some(b);
        }
        // Proxy toggle. Explicit `gatewayProxy` wins; otherwise migrate a
        // legacy non-empty `gatewayUseFor` list to "proxy on". The effective
        // `gateway_use_for` is DERIVED from this flag in `load()`, so we set
        // the flag here, not the list.
        if let Some(proxy) = self.gateway_proxy {
            config.gateway_proxy = proxy;
        } else if let Some(ref providers) = self.gateway_use_for {
            if providers.iter().any(|s| !s.trim().is_empty()) {
                config.gateway_proxy = true;
            }
        }
        if let Some(ref url) = self.remote_agent_url {
            let trimmed = url.trim();
            if !trimmed.is_empty() {
                config.remote_agent_url = Some(trimmed.to_string());
            }
        }
        if let Some(ref gs) = self.gui_shell {
            config.gui_shell = Some(gs.clone());
        }
        if let Some(ref f) = self.openrouter_fusion {
            config.openrouter_fusion = f.clone();
        }
    }

    pub fn set_model(&mut self, model: &str) {
        self.model = Some(model.to_string());
    }

    /// Persist the thinking level (`Some(budget)`) or clear it back to
    /// auto (`None`). `save()`'s overlay skips nulls, so clearing has
    /// to drop the key from the file explicitly.
    pub fn persist_thinking_budget(budget: Option<u32>) -> Result<()> {
        let path = Self::path();
        let mut base = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if !base.is_object() {
            base = serde_json::json!({});
        }
        let obj = base.as_object_mut().unwrap();
        match budget {
            Some(b) => {
                obj.insert("thinkingBudget".into(), serde_json::json!(b));
            }
            None => {
                obj.remove("thinkingBudget");
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&base)?)?;
        Ok(())
    }

    /// Persist the `/deploy` default target URL. Pair with the
    /// `remote-agent-token` keychain entry (managed by
    /// [`crate::secrets`]) for the bearer token.
    pub fn set_remote_agent_url(&mut self, url: Option<&str>) {
        self.remote_agent_url = url.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    }

    /// Merge an [`AgentConfig`] block into project settings. Fields
    /// passed as `Some` overwrite; `None` leaves existing values
    /// untouched (so a partial update — e.g. just `uuid` — preserves
    /// name/description).
    pub fn merge_agent(&mut self, updates: AgentConfig) {
        let mut current = self.agent.clone().unwrap_or_default();
        if updates.id.is_some() {
            current.id = updates.id;
        }
        if updates.name.is_some() {
            current.name = updates.name;
        }
        if updates.description.is_some() {
            current.description = updates.description;
        }
        if updates.uuid.is_some() {
            current.uuid = updates.uuid;
        }
        let empty = current.id.is_none()
            && current.name.is_none()
            && current.description.is_none()
            && current.uuid.is_none();
        self.agent = if empty { None } else { Some(current) };
    }

    /// Drop just the server-assigned UUID — used by `cloud unbind`
    /// when the user wants to fork a copy into a fresh catalog entry.
    /// Leaves id / name / description in place.
    pub fn clear_agent_uuid(&mut self) {
        if let Some(agent) = self.agent.as_mut() {
            agent.uuid = None;
        }
    }

    /// Persist the thClaws.cloud catalog URL (dev-plan/34). Pair with
    /// the `cloud-token` keychain entry (managed by [`crate::cloud`])
    /// for the bearer token.
    pub fn set_cloud_url(&mut self, url: Option<&str>) {
        let normalized = url
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty());
        self.cloud = match (self.cloud.take(), normalized) {
            (Some(mut existing), v) => {
                existing.url = v;
                Some(existing)
            }
            (None, Some(v)) => Some(crate::cloud::CloudConfig { url: Some(v) }),
            (None, None) => None,
        };
    }

    /// Persist the GUI zoom factor. Clamped to a sane range so a
    /// stray slider value can't push text into single-pixel territory
    /// or fill the screen — matches the bounds VS Code / Slack use.
    pub fn set_gui_scale(&mut self, scale: f64) {
        let clamped = scale.clamp(0.5, 3.0);
        self.gui_scale = Some(clamped);
    }

    /// Persist the permission mode (`"auto"` / `"ask"`) to project
    /// settings. Overwrites any existing `{allow, deny}` block — GUI
    /// and REPL only toggle the simple mode, so the complex form
    /// rewrites whenever the user flips `/permissions`.
    pub fn set_permissions_mode(&mut self, mode: &str) {
        self.permissions = Some(PermissionsConfig::Mode(mode.to_string()));
    }

    /// Persist the desktop proxy toggle (single flag). Also clears any legacy
    /// `gatewayUseFor` list so the two representations can't drift apart.
    pub fn set_gateway_proxy(&mut self, on: bool) {
        self.gateway_proxy = Some(on);
        self.gateway_use_for = None;
    }

    /// Set (or clear with `None`) the workspace's default GUI Shell. Uses
    /// the shorthand form so it applies to BOTH the GUI Shell tab
    /// (`tabDefault`) and `--serve` (`serveDefault`) — i.e. "the default UI
    /// for this workspace". Persisted to the project `.thclaws/settings.json`.
    pub fn set_gui_shell_default(&mut self, shell_id: Option<&str>) {
        self.gui_shell = shell_id.map(|id| GuiShellSetting::Shorthand(id.to_string()));
    }

    /// Load project-level MCP servers. Checks (in order):
    /// 1. `.mcp.json` (project root — Claude Code primary location)
    /// 2. `.thclaws/mcp.json`
    /// 3. `.claude/mcp.json`
    pub fn load_mcp_servers() -> Vec<crate::mcp::McpServerConfig> {
        let cwd = std::env::current_dir().unwrap_or_default();
        let paths = [
            cwd.join(".mcp.json"),                // Claude Code primary
            Self::project_dir().join("mcp.json"), // thClaws
            cwd.join(".claude/mcp.json"),         // Claude Code legacy
        ];
        for path in &paths {
            if let Some(servers) = Self::parse_mcp_json(path) {
                if !servers.is_empty() {
                    return servers;
                }
            }
        }
        Vec::new()
    }

    fn parse_mcp_json(path: &Path) -> Option<Vec<crate::mcp::McpServerConfig>> {
        let contents = std::fs::read_to_string(path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&contents).ok()?;
        let servers = v.get("mcpServers").and_then(|s| s.as_object())?;
        let parsed: Vec<crate::mcp::McpServerConfig> = servers
            .iter()
            .filter_map(|(name, cfg)| {
                let transport = cfg
                    .get("transport")
                    .and_then(|t| t.as_str())
                    .unwrap_or("stdio")
                    .to_string();
                if transport == "http" {
                    // HTTP transport: needs a URL, optional headers.
                    let url = cfg.get("url")?.as_str()?.to_string();
                    let headers: std::collections::HashMap<String, String> = cfg
                        .get("headers")
                        .and_then(|h| h.as_object())
                        .map(|obj| {
                            obj.iter()
                                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                                .collect()
                        })
                        .unwrap_or_default();
                    let trusted = cfg
                        .get("trusted")
                        .and_then(|t| t.as_bool())
                        .unwrap_or(false);
                    return Some(crate::mcp::McpServerConfig {
                        name: name.clone(),
                        transport,
                        command: String::new(),
                        args: Vec::new(),
                        env: std::collections::HashMap::new(),
                        url,
                        headers,
                        trusted,
                        engine_managed: false,
                    });
                }
                // Stdio transport: needs a command.
                let command = cfg.get("command")?.as_str()?.to_string();
                let args: Vec<String> = cfg
                    .get("args")
                    .and_then(|a| a.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let env: std::collections::HashMap<String, String> = cfg
                    .get("env")
                    .and_then(|e| e.as_object())
                    .map(|obj| {
                        obj.iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                let trusted = cfg
                    .get("trusted")
                    .and_then(|t| t.as_bool())
                    .unwrap_or(false);
                Some(crate::mcp::McpServerConfig {
                    name: name.clone(),
                    transport,
                    command,
                    args,
                    env,
                    url: String::new(),
                    headers: std::collections::HashMap::new(),
                    trusted,
                    engine_managed: false,
                })
            })
            .collect();
        // Org-policy gate (Phase 2): when policies.plugins.enabled with
        // allow_external_mcp: false, reject HTTP MCP servers whose URL
        // host isn't in `allowed_hosts`. Stdio entries pass through —
        // gating arbitrary stdio commands is a separate sub-policy
        // (admin's mcp.json content = admin's responsibility).
        let filtered: Vec<crate::mcp::McpServerConfig> = if crate::policy::external_mcp_disallowed()
        {
            parsed
                .into_iter()
                .filter(|s| {
                    if s.transport != "http" {
                        return true;
                    }
                    match crate::policy::check_url(&s.url) {
                        crate::policy::AllowDecision::Allowed
                        | crate::policy::AllowDecision::NoPolicy => true,
                        crate::policy::AllowDecision::Denied { reason } => {
                            eprintln!("\x1b[33m[mcp] '{}' skipped: {}\x1b[0m", s.name, reason);
                            false
                        }
                    }
                })
                .collect()
        } else {
            parsed
        };
        Some(filtered)
    }
}

/// Insert or replace an MCP server in the on-disk `mcp.json` file.
/// `user=true` writes to `~/.config/thclaws/mcp.json`, otherwise
/// `.thclaws/mcp.json` (project-local). Returns the path written to.
pub fn save_mcp_server(server: &crate::mcp::McpServerConfig, user: bool) -> Result<PathBuf> {
    let path = mcp_config_path(user)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Read existing file (if any) into a Value so we preserve unknown keys
    // and the order of sibling servers.
    let mut root: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({"mcpServers": {}}));

    if !root
        .get("mcpServers")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        root["mcpServers"] = serde_json::json!({});
    }

    let mut entry = serde_json::Map::new();
    entry.insert("transport".into(), serde_json::json!(server.transport));
    if server.transport == "http" {
        entry.insert("url".into(), serde_json::json!(server.url));
        if !server.headers.is_empty() {
            entry.insert("headers".into(), serde_json::json!(server.headers));
        }
    } else {
        entry.insert("command".into(), serde_json::json!(server.command));
        if !server.args.is_empty() {
            entry.insert("args".into(), serde_json::json!(server.args));
        }
        if !server.env.is_empty() {
            entry.insert("env".into(), serde_json::json!(server.env));
        }
    }
    root["mcpServers"][server.name.as_str()] = serde_json::Value::Object(entry);

    let pretty = serde_json::to_string_pretty(&root)
        .map_err(|e| Error::Config(format!("serialize mcp.json: {e}")))?;
    std::fs::write(&path, pretty)?;
    Ok(path)
}

/// Remove a server from the on-disk `mcp.json`. Returns
/// `(removed, path, removed_url)`: `removed` is false when the file
/// or the key didn't exist; `removed_url` is `Some(url)` when the
/// removed entry had an HTTP `url` (the caller uses this to drop any
/// cached OAuth token for that server — see [`crate::oauth::TokenStore`]).
pub fn remove_mcp_server(name: &str, user: bool) -> Result<(bool, PathBuf, Option<String>)> {
    let path = mcp_config_path(user)?;
    if !path.exists() {
        return Ok((false, path, None));
    }
    let contents = std::fs::read_to_string(&path)?;
    let mut root: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|e| Error::Config(format!("parse mcp.json: {e}")))?;
    let removed_entry = root
        .get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
        .and_then(|m| m.remove(name));
    let removed_url = removed_entry
        .as_ref()
        .and_then(|v| v.get("url"))
        .and_then(|u| u.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let removed = removed_entry.is_some();
    if removed {
        let pretty = serde_json::to_string_pretty(&root)
            .map_err(|e| Error::Config(format!("serialize mcp.json: {e}")))?;
        std::fs::write(&path, pretty)?;
    }
    Ok((removed, path, removed_url))
}

/// Public, non-failing view of [`mcp_config_path`] — callers that only
/// want to read one scope's file (e.g. "which scope is this server in?")
/// shouldn't have to handle a `Result` they'd only map to `None`.
pub fn mcp_config_path_for(user: bool) -> Option<PathBuf> {
    mcp_config_path(user).ok()
}

fn mcp_config_path(user: bool) -> Result<PathBuf> {
    if user {
        let home = crate::util::home_dir()
            .ok_or_else(|| Error::Config("cannot locate user home directory".into()))?;
        Ok(home.join(".config/thclaws/mcp.json"))
    } else {
        let cwd = std::env::current_dir()?;
        Ok(cwd.join(".thclaws").join("mcp.json"))
    }
}

impl AppConfig {
    /// Overwrite whatever the settings files and CLI flags decided with
    /// what the org policy's `runtime` block requires (Phase 8).
    ///
    /// Called at the end of every load path, and again by the binary
    /// after CLI flags are merged — a policy that a `--permission-mode`
    /// flag could climb over would not be a policy. Idempotent, so
    /// calling it twice is the intended usage.
    pub fn apply_runtime_policy(&mut self) {
        if let Some(mode) = crate::policy::forced_permission_mode() {
            if self.permissions != mode {
                eprintln!("[policy] permission mode forced to '{mode}' by org policy");
            }
            self.permissions = mode;
        }
        let denied = crate::policy::denied_tools();
        if denied.is_empty() {
            return;
        }
        let list = self.disallowed_tools.get_or_insert_with(Vec::new);
        for name in denied {
            if !list.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
                list.push(name);
            }
        }
    }

    /// Load config following the documented precedence.
    /// Load order: env override → user settings.json → Claude Code fallback →
    ///             defaults → project overlay.
    pub fn load() -> Result<Self> {
        // Shared-agent mode (dev-plan/41): the company brain is the ONLY
        // config source. Member scopes (user / project / ~/.claude) are
        // ignored so provider/tool/model settings can't be overridden,
        // and the gateway is forced (no BYOK).
        if crate::shared::is_active() {
            return Self::load_shared();
        }

        // 1. Explicit env override.
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(p) = std::env::var("THCLAWS_CONFIG") {
            candidates.push(PathBuf::from(p));
        }
        // 2. User-level: ~/.config/thclaws/settings.json.
        candidates.extend(Self::user_config_paths());

        // Tracks whether any settings layer (user / project / CLI
        // override) explicitly pinned a `model`. When it stays false the
        // effective model is just the compiled-in placeholder, and startup
        // is free to pick a credential-aware default provider (see the
        // `preferred_default_model` step below).
        let mut model_explicit = false;

        let mut config = None;
        for path in &candidates {
            if !path.exists() {
                continue;
            }
            let contents = std::fs::read_to_string(path)?;
            let pc: ProjectConfig = serde_json::from_str(&contents)
                .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
            if pc.model.is_some() {
                model_explicit = true;
            }
            let mut cfg = Self::default();
            pc.apply_to(&mut cfg);
            config = Some(cfg);
            break;
        }

        // No user-global Claude Code fallback: `~/.claude/settings.json`
        // must not seed `model`/permissions (it silently pulled in an
        // unroutable model the user never chose). `config` stays None here
        // when no thClaws layer exists, so `preferred_default_model` below
        // picks a credential-aware default instead.
        let mut config = config.unwrap_or_default();

        // User-level MCP: ~/.config/thclaws/mcp.json, then ~/.claude/mcp.json.
        if config.mcp_servers.is_empty() {
            config.mcp_servers = Self::load_user_mcp_servers();
        }

        // Project-level overrides from .thclaws/settings.json (or legacy .thclaws.toml).
        if let Some(project) = ProjectConfig::load() {
            if project.model.is_some() {
                model_explicit = true;
            }
            project.apply_to(&mut config);
        }

        // Project-level MCP servers from .thclaws/mcp.json (merged; project overrides user by name).
        let project_mcp = ProjectConfig::load_mcp_servers();
        if !project_mcp.is_empty() {
            let project_names: std::collections::HashSet<String> =
                project_mcp.iter().map(|s| s.name.clone()).collect();
            // Remove user-level servers that project overrides.
            config
                .mcp_servers
                .retain(|s| !project_names.contains(&s.name));
            config.mcp_servers.extend(project_mcp);
        }

        // Engine-managed browser automation (docs/browser, Phase 0+1):
        // `browserEnabled` (default ON since 0.49.2) injects the
        // official Playwright MCP server as a synthetic stdio config.
        // Engine-chosen — not from a cloned repo's mcp.json — so it
        // skips the first-spawn allowlist prompt (`engine_managed` is
        // #[serde(skip)]; JSON can never set it). A user/project server
        // already named `browser` wins, and the enterprise external-MCP
        // policy still applies. With the default flipped on, two
        // graceful-degradation guards matter:
        // - launch command must resolve on PATH (node-less desktops get
        //   the Browser tab's setup hint, not a spawn error per session)
        // - never under `cfg(test)` (the unit-test suite must not spawn
        //   npx / hit the npm registry)
        if config.browser_enabled
            && !cfg!(test)
            && !crate::policy::external_mcp_disallowed()
            && !config.mcp_servers.iter().any(|s| s.name == "browser")
        {
            let server = Self::browser_mcp_config(config.browser_headless);
            if command_on_path(&server.command) {
                config.mcp_servers.push(server);
            }
        }

        // CLI `--model` / `--set-model` override (set by app.rs once at
        // startup). Applied last so it wins over user, project, and the
        // Claude Code fallback — matches the precedence the module docs
        // promise. Cleared by `clear_cli_model_override` if auto-fallback
        // decides the user's choice was unreachable.
        if let Some(m) = cli_model_override() {
            config.model = m;
            model_explicit = true;
        }

        // Credential-aware default provider. When no layer pinned a model
        // the compiled-in `model` is an Anthropic placeholder; prefer the
        // first provider the user actually has credentials (own key or a
        // gateway route) for, in order DashScope → OpenAI → Anthropic, so
        // a fresh install / new session starts on a provider that's ready
        // instead of a stuck "no API key" Anthropic default. In-memory
        // only (not persisted): keeping it unpinned means the default
        // re-evaluates if the user later adds or removes a key. Skipped in
        // multiuser pods (the gateway-forced guest keeps the placeholder)
        // and under tests (env-dependent, would make the default
        // nondeterministic). A non-default `model` value from any layer
        // also counts as an explicit choice.
        // In any hosted gateway pod, every gateway-routable provider must
        // route through the gateway (the user has no BYOK keys; BYOK would
        // bypass billing + governance — dev-plan/42). Derive the routed set
        // from the engine's CURRENT `GATEWAY_ALL_PROVIDERS` rather than the
        // per-workspace `gatewayUseFor`: that list is written by a
        // provision-time init container whose default is baked then, so it
        // goes STALE when new gateway-routable providers ship — e.g. xai /
        // moonshot landed in v0.67.0 but pre-existing workspaces' lists
        // predate them, so those Featured providers wrongly fell back to
        // BYOK ("set XAI_API_KEY") in gateway mode. Keying off the engine
        // here keeps Featured == gateway-supported without re-provisioning.
        // `THCLAWS_USES_GATEWAY=1` marks a cloud gateway runner (the
        // provisioner sets it; desktop never does) and covers multiuser
        // pods too.
        //
        // This MUST run BEFORE the credential-aware default below:
        // `preferred_default_model` treats a gateway-routed provider as
        // "reachable", so if `gateway_use_for` were still empty at that
        // point a proxy user with no BYOK keys would find no reachable
        // provider and fall back to the compiled-in Anthropic placeholder
        // (claude-sonnet-4-6) instead of the intended gateway default
        // (deepseek-flash).
        let in_gateway_pod = crate::workdir::is_multiuser()
            || std::env::var("THCLAWS_USES_GATEWAY").ok().as_deref() == Some("1");
        // DERIVE the routed set from a single source of truth: the `gatewayProxy`
        // flag (desktop) or being in a gateway pod (cloud). When on, every
        // gateway-routable provider is in the set; when off, the set is empty
        // (pure BYOK). Deriving here — rather than persisting a per-provider
        // list — means the proxy can't get "stuck on" or partially toggle, and
        // newly-shipped routable providers are covered automatically. Per-model
        // eligibility (Featured + priced) is enforced at routing time, so a
        // non-featured model still falls back to BYOK even when the proxy is on.
        let gateway_on = in_gateway_pod || config.gateway_proxy;
        config.gateway_use_for = if gateway_on {
            crate::shared::GATEWAY_ALL_PROVIDERS
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            Vec::new()
        };

        // HAL is free and reachable through the gateway, and `WebFetch`
        // already keys its HAL path off `hal_available()` (true whenever the
        // gateway is active). Force the HAL *tools* on to match, so
        // `WebScrape` / `YouTubeTranscript` get registered — otherwise a skill
        // that names `WebScrape` (e.g. content-extractor's `/extract`) finds no
        // such tool and silently falls back to the browser, never touching HAL.
        // Direct BYOK mode still respects the explicit `halEnabled` toggle since
        // HAL calls cost money there.
        if gateway_on {
            config.hal_enabled = true;
        }

        if config.model != Self::default().model {
            model_explicit = true;
        }
        if !model_explicit && !cfg!(test) && !crate::workdir::is_multiuser() {
            if let Some(m) = crate::providers::preferred_default_model(&config) {
                config.model = m;
            }
        }

        // Repoint a workspace pinned to a model that has since left the
        // catalogue. This runs AFTER — and independently of — the block
        // above on purpose: a persisted `model` sets `model_explicit`, so
        // the credential-aware default never re-runs for an existing
        // workspace and a retired id would survive every restart. An id
        // with no priced catalogue entry is not gateway-servable, so a
        // gateway user with no BYOK keys hits "No keys" on a workspace
        // that worked before the model was retired upstream.
        if let Some(successor) = crate::providers::retired_model_successor(&config.model) {
            eprintln!(
                "[model] '{}' was retired upstream — using '{successor}' instead",
                config.model
            );
            config.model = successor.to_string();
        }

        config.apply_runtime_policy();
        Ok(config)
    }

    /// Shared-agent config loader (dev-plan/41). Reads ONLY the company
    /// brain at `$THCLAWS_SHARED_AGENT_DIR`: `settings.json` (read-only
    /// base), `mcp.json` (config only — credentials resolve per-member).
    /// Member-scope settings are never consulted. The gateway is forced
    /// for every routable provider (no BYOK), and the model is pinned
    /// when the company sets `THCLAWS_SHARED_MODEL_LOCKED`.
    fn load_shared() -> Result<Self> {
        let mut config = Self::default();

        // Company settings base. A malformed file would otherwise
        // silently disable every flag for every member, so surface the
        // parse error loudly instead of swallowing it.
        if let Some(path) = crate::shared::shared_settings_json() {
            if path.exists() {
                let contents = std::fs::read_to_string(&path)?;
                let pc: ProjectConfig = serde_json::from_str(&contents)
                    .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
                pc.apply_to(&mut config);
            }
        }

        // Shared MCP config (read-only). Credentials (OAuth, etc.)
        // resolve per-member at runtime, never from the shared brain.
        if let Some(path) = crate::shared::shared_mcp_json() {
            if let Some(servers) = ProjectConfig::parse_mcp_json(&path) {
                config.mcp_servers = servers;
            }
        }

        // Member-additive MCP from the private working dir, unless the
        // company enforces strict mode. A member can never override a
        // shared server by name.
        if !crate::shared::is_strict() {
            let shared_names: std::collections::HashSet<String> =
                config.mcp_servers.iter().map(|s| s.name.clone()).collect();
            for s in ProjectConfig::load_mcp_servers() {
                if !shared_names.contains(&s.name) {
                    config.mcp_servers.push(s);
                }
            }
        }

        // Gateway-only: force every gateway-routable provider through the
        // gateway, ignoring any BYOK/native provider config in any layer.
        // The gateway access key + base URL come from the pod's
        // THCLAWS_GATEWAY_* env (injected by provisioning).
        config.gateway_use_for = crate::shared::GATEWAY_ALL_PROVIDERS
            .iter()
            .map(|s| s.to_string())
            .collect();

        // Engine-managed browser MCP — same conditions as the normal path.
        if config.browser_enabled
            && !cfg!(test)
            && !crate::policy::external_mcp_disallowed()
            && !config.mcp_servers.iter().any(|s| s.name == "browser")
        {
            let server = Self::browser_mcp_config(config.browser_headless);
            if command_on_path(&server.command) {
                config.mcp_servers.push(server);
            }
        }

        // Model: company model wins. A member `--model` override applies
        // only when the company hasn't pinned the model.
        if !crate::shared::is_model_locked() {
            if let Some(m) = cli_model_override() {
                config.model = m;
            }
        }

        config.apply_runtime_policy();
        Ok(config)
    }

    /// The synthetic engine-managed Playwright MCP server config that
    /// `browserEnabled` injects. Headed/headless: explicit override
    /// wins; otherwise auto — headless on cloud runners
    /// (`THCLAWS_USES_GATEWAY=1`) and displayless Linux, headed
    /// elsewhere (the desktop "browse next to the agent" workflow).
    ///
    /// Launch command: `THCLAWS_BROWSER_MCP_CMD` overrides the whole
    /// command line (docs/browser Phase 2 — the cloud runner image
    /// sets `mcp-server-playwright --no-sandbox`, the preinstalled
    /// server, so pod cold starts never hit the npm registry); the
    /// desktop default is `npx -y @playwright/mcp@latest`.
    /// `--headless` is appended when resolved headless and not
    /// already present.
    pub fn browser_mcp_config(headless_override: Option<bool>) -> crate::mcp::McpServerConfig {
        let headless = headless_override.unwrap_or_else(|| {
            std::env::var("THCLAWS_USES_GATEWAY").ok().as_deref() == Some("1")
                || (cfg!(target_os = "linux")
                    && std::env::var_os("DISPLAY").is_none()
                    && std::env::var_os("WAYLAND_DISPLAY").is_none())
        });
        let (command, mut args) = match std::env::var("THCLAWS_BROWSER_MCP_CMD")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .and_then(|s| shell_words::split(&s).ok())
            .filter(|words| !words.is_empty())
        {
            Some(mut words) => {
                let cmd = words.remove(0);
                (cmd, words)
            }
            None => (
                "npx".to_string(),
                vec!["-y".to_string(), "@playwright/mcp@latest".to_string()],
            ),
        };
        if headless && !args.iter().any(|a| a == "--headless") {
            args.push("--headless".into());
        }
        // Vision capability adds the coordinate tools (mouse_*_xy,
        // mouse_wheel) that back the Browser tab's interactive
        // takeover — click/type/scroll on the screenshot (docs/browser
        // Phase 2 slice 2). Skipped when the override already pins a
        // --caps choice.
        if !args.iter().any(|a| a.starts_with("--caps")) {
            args.push("--caps=vision".into());
        }
        // Default to a wide desktop viewport — playwright-mcp's own
        // default is a narrow 1280×720, which renders pages mobile-ish.
        // Override with THCLAWS_BROWSER_VIEWPORT="W,H" (or pin
        // --viewport-size / --device via THCLAWS_BROWSER_MCP_CMD).
        if !args
            .iter()
            .any(|a| a.starts_with("--viewport-size") || a == "--device")
        {
            let viewport = std::env::var("THCLAWS_BROWSER_VIEWPORT")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "1920,1080".to_string());
            args.push("--viewport-size".into());
            args.push(viewport);
        }
        crate::mcp::McpServerConfig {
            name: "browser".into(),
            transport: "stdio".into(),
            command,
            args,
            env: std::collections::HashMap::new(),
            url: String::new(),
            headers: std::collections::HashMap::new(),
            trusted: false,
            engine_managed: true,
        }
    }

    /// User-level config path: `~/.config/thclaws/settings.json`.
    pub fn user_config_paths() -> Vec<PathBuf> {
        let Some(home) = crate::util::home_dir() else {
            return vec![];
        };
        vec![home.join(".config/thclaws/settings.json")]
    }

    /// Load MCP servers from user-level paths:
    /// `~/.config/thclaws/mcp.json`, then `~/.claude/mcp.json` as fallback.
    fn load_user_mcp_servers() -> Vec<crate::mcp::McpServerConfig> {
        let Some(home) = crate::util::home_dir() else {
            return vec![];
        };
        let paths = [
            home.join(".config/thclaws/mcp.json"),
            home.join(".claude/mcp.json"),
        ];
        for path in &paths {
            if let Some(servers) = ProjectConfig::parse_mcp_json(path) {
                if !servers.is_empty() {
                    return servers;
                }
            }
        }
        vec![]
    }

    /// Resolve the provider kind implied by the model string.
    pub fn detect_provider_kind(&self) -> Result<crate::providers::ProviderKind> {
        crate::providers::ProviderKind::detect(&self.model)
            .ok_or_else(|| Error::Config(format!("unknown model provider: {}", self.model)))
    }

    /// Short provider name ("anthropic", "openai", "gemini", "ollama").
    pub fn detect_provider(&self) -> Result<&'static str> {
        self.detect_provider_kind().map(|k| k.name())
    }

    /// Resolve the API key for the active provider, in this order:
    ///   1. Process env var (shell export, dotenv-loaded, or keychain
    ///      snapshot injected at our startup).
    ///   2. OS keychain (looked up live — matters for cross-process
    ///      consistency: the GUI sets a key via Settings, but an
    ///      already-spawned PTY-child REPL can't see the GUI process's
    ///      updated env. Both processes can, however, read the same
    ///      keychain entry.)
    /// Returns `None` when neither source has a key (providers without
    /// auth, like ollama, are OK either way).
    pub fn api_key_from_env(&self) -> Option<String> {
        // Trim whitespace and one pair of wrapping "…" / '…' quotes.
        // Defensive against env / keychain values that picked up a
        // copy-paste artefact (issue #145 — wrapping double quotes
        // turn `Bearer X` into `Bearer "X"`, which OpenRouter parses
        // as no bearer at all → `Missing Authentication header`).
        // Inlined so this helper has no other call sites and the
        // intent stays next to where it's used.
        fn sanitize_api_key(raw: &str) -> String {
            let trimmed = raw.trim();
            let b = trimmed.as_bytes();
            if b.len() >= 2
                && ((b[0] == b'"' && b[b.len() - 1] == b'"')
                    || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
            {
                trimmed[1..trimmed.len() - 1].to_string()
            } else {
                trimmed.to_string()
            }
        }
        // Body proper:
        let kind = self.detect_provider_kind().ok()?;
        let var = kind.api_key_env()?;
        // Treat an exported-but-empty env var ("ANTHROPIC_API_KEY=") as
        // unset and fall through to the keychain. A stale shell rc or
        // VS Code env injection can leave the var present but blank;
        // returning Some("") from here would produce an empty bearer
        // token and a confusing 401 on every request.
        if let Ok(value) = std::env::var(var) {
            let normalized = sanitize_api_key(&value);
            if !normalized.is_empty() {
                if std::env::var("THCLAWS_KEYCHAIN_TRACE").is_ok() {
                    eprintln!(
                        "\x1b[35m[keychain pid={}] api_key_from_env({}) → from env {}\x1b[0m",
                        std::process::id(),
                        kind.name(),
                        var
                    );
                }
                return Some(normalized);
            }
        }
        if std::env::var("THCLAWS_KEYCHAIN_TRACE").is_ok() {
            eprintln!(
                "\x1b[35m[keychain pid={}] api_key_from_env({}) → env {} unset or blank, falling back to keychain\x1b[0m",
                std::process::id(), kind.name(), var
            );
        }
        // Fall back to the keychain under the provider's short name.
        // Sanitize the keychain value too — entries written before the
        // `api_key_set` normalisation fix (issue #145) may still have
        // wrapping quotes / leading-trailing whitespace from the
        // original paste. `None` for empty-after-sanitize so callers
        // surface the friendlier "no API key found" rather than 401.
        crate::secrets::get(kind.name()).and_then(|raw| {
            let s = sanitize_api_key(&raw);
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        })
    }
}

/// Persist a model override into `.thclaws/settings.json` for the
/// `--set-model` flag. Safer than the obvious `load().unwrap_or_default()
/// + save()` pattern: if the file exists but fails to parse (transient
/// I/O, mid-edit, unknown field after a downgrade) we **refuse to
/// overwrite** rather than constructing a fresh defaults-everywhere
/// `ProjectConfig` and silently clobbering the user's other settings
/// (`maxTokens`, `allowedTools`, `kms.active`, etc.). Save errors
/// propagate as `Error::Config` so `app.rs` can surface them on stderr.
///
/// When the file doesn't exist we fall back to the regular
/// `ProjectConfig::load` chain (which also looks at
/// `.claude/settings.json`) so users migrating from Claude Code get
/// their existing settings preserved on first `--set-model` instead of
/// reset to defaults.
pub fn persist_model_to_project_settings(resolved_model: &str) -> Result<PathBuf> {
    let path = ProjectConfig::path();
    let fallback = || ProjectConfig::load().unwrap_or_default();
    persist_model_at_path(&path, fallback, resolved_model)?;
    Ok(path)
}

/// Inner helper that all the safety / clobber logic lives in, parametrized
/// on the target path and the "file is missing" fallback. Pulled out so
/// the tests can exercise it without setting `THCLAWS_PROJECT_ROOT` — env
/// var mutations on the test thread race with `posix_spawn` in the
/// `schedule::tests` suite and trip EINVAL out of fork+exec.
fn persist_model_at_path<F>(path: &Path, missing_fallback: F, resolved_model: &str) -> Result<()>
where
    F: FnOnce() -> ProjectConfig,
{
    let mut project = match std::fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str::<ProjectConfig>(&contents).map_err(|e| {
            Error::Config(format!(
                "{} exists but is unreadable ({e}). Refusing to overwrite to avoid clobbering other settings. Fix or delete the file and retry.",
                path.display()
            ))
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing_fallback(),
        Err(e) => {
            return Err(Error::Config(format!(
                "failed to read {}: {e}",
                path.display()
            )));
        }
    };
    project.set_model(resolved_model);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&project)?;
    std::fs::write(path, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn apply_runtime_policy_is_a_noop_without_a_policy() {
        // No policy is loaded in the unit-test process: settings and CLI
        // keep whatever they decided, which is the open-core contract.
        let mut c = AppConfig {
            permissions: "auto".into(),
            disallowed_tools: Some(vec!["Ask".into()]),
            ..AppConfig::default()
        };
        c.apply_runtime_policy();
        assert_eq!(c.permissions, "auto");
        assert_eq!(
            c.disallowed_tools.as_deref(),
            Some(&["Ask".to_string()][..])
        );
        // Idempotent — the binary calls it twice by design.
        c.apply_runtime_policy();
        assert_eq!(c.disallowed_tools.as_ref().map(|v| v.len()), Some(1));
    }
    use super::*;

    /// Issue #180: `hooks` in settings.json must reach `config.hooks`.
    /// The key was documented (ch13) but ProjectConfig had no field for
    /// it, so serde dropped it and hooks never fired.
    #[test]
    fn hooks_in_settings_json_reach_app_config() {
        let raw = r#"{
            "model": "claude-sonnet-4-6",
            "hooks": {
                "pre_tool_use": "echo pre >> /tmp/h.log",
                "session_start": "echo start >> /tmp/h.log",
                "fail_closed": false
            }
        }"#;
        let pc: ProjectConfig = serde_json::from_str(raw).unwrap();
        assert!(pc.hooks.is_some(), "hooks key must deserialize");
        let mut cfg = AppConfig::default();
        pc.apply_to(&mut cfg);
        assert_eq!(
            cfg.hooks.pre_tool_use.as_deref(),
            Some("echo pre >> /tmp/h.log")
        );
        assert_eq!(
            cfg.hooks.session_start.as_deref(),
            Some("echo start >> /tmp/h.log")
        );
        assert!(cfg.hooks.any_configured());
        // No hooks key → default stays (all None), nothing fires.
        let pc2: ProjectConfig = serde_json::from_str(r#"{"model":"x"}"#).unwrap();
        let mut cfg2 = AppConfig::default();
        pc2.apply_to(&mut cfg2);
        assert!(!cfg2.hooks.any_configured());
    }

    #[test]
    fn legacy_gateway_use_for_migrates_to_proxy_flag() {
        // A pre-existing non-empty `gatewayUseFor` (the old per-provider list)
        // migrates to the single proxy flag = on.
        let mut pc = ProjectConfig::default();
        pc.gateway_use_for = Some(vec!["openai".to_string()]);
        let mut cfg = AppConfig::default();
        pc.apply_to(&mut cfg);
        assert!(cfg.gateway_proxy, "non-empty legacy list → proxy on");

        // Explicit `gatewayProxy` wins over the legacy list.
        let mut pc2 = ProjectConfig::default();
        pc2.gateway_proxy = Some(false);
        pc2.gateway_use_for = Some(vec!["openai".to_string()]);
        let mut cfg2 = AppConfig::default();
        pc2.apply_to(&mut cfg2);
        assert!(!cfg2.gateway_proxy, "explicit gatewayProxy=false wins");

        // Empty legacy list → stays off (pure BYOK).
        let mut pc3 = ProjectConfig::default();
        pc3.gateway_use_for = Some(vec![]);
        let mut cfg3 = AppConfig::default();
        pc3.apply_to(&mut cfg3);
        assert!(!cfg3.gateway_proxy, "empty legacy list → proxy off");
    }
    use tempfile::tempdir;

    // Env-mutating tests in this module use `crate::kms::test_env_lock`
    // (the crate-wide lock) rather than a local one, so they don't race
    // against tests in kms/plugins/context/agent that also flip cwd /
    // HOME / THCLAWS_PROJECT_ROOT.

    // dev-plan/42: a multiuser pod forces every gateway-routable provider
    // through the gateway, so a guest can't BYOK out of the owner's bill
    // via their writable settings/.env. Single-tenant leaves it alone.
    #[test]
    fn multiuser_forces_gateway_routable_providers() {
        let _guard = crate::kms::test_env_lock();

        // Single-tenant baseline: no blanket gateway-force.
        crate::workdir::set_multiuser(false);
        let plain = AppConfig::load().unwrap();

        // Multiuser: every gateway-routable provider is forced onto the
        // gateway. Reset the flag before asserting so a panic can't leak
        // multiuser mode into sibling tests.
        crate::workdir::set_multiuser(true);
        let multi = AppConfig::load().unwrap();
        crate::workdir::set_multiuser(false);

        for p in crate::shared::GATEWAY_ALL_PROVIDERS {
            assert!(
                multi.gateway_use_for.iter().any(|s| s == p),
                "multiuser must force {p} through the gateway"
            );
        }
        // The force is multiuser-specific, not unconditional.
        assert!(
            plain.gateway_use_for.len() < multi.gateway_use_for.len()
                || plain.gateway_use_for.is_empty(),
            "single-tenant must not blanket-force the gateway"
        );
    }

    #[test]
    fn gateway_active_forces_hal_tools_on() {
        let _guard = crate::kms::test_env_lock();
        // HAL is free + reachable through the gateway, so the HAL tools
        // (`WebScrape` / `YouTubeTranscript`) must auto-register even when
        // `halEnabled` was never set. Regression: content-extractor's `/extract`
        // named `WebScrape`, but its workspace had halEnabled:false alongside
        // gatewayProxy:true, so the tool wasn't registered and the subagent
        // silently fell back to the browser — never touching HAL.
        std::env::set_var("THCLAWS_USES_GATEWAY", "1");
        let cfg = AppConfig::load();
        std::env::remove_var("THCLAWS_USES_GATEWAY");
        assert!(
            cfg.unwrap().hal_enabled,
            "gateway-active config must force HAL tools on"
        );
    }

    #[test]
    fn default_config_is_anthropic_sonnet() {
        let c = AppConfig::default();
        assert_eq!(c.model, "claude-sonnet-4-6");
        assert_eq!(c.detect_provider().unwrap(), "anthropic");
    }

    #[test]
    fn fusion_tool_json_omits_unset_and_uses_snake_case() {
        // Empty config ⇒ bare tool, no parameters block (OpenRouter
        // default panel); "auto" tool_choice ⇒ omitted from the body.
        let f = FusionConfig::default();
        let tool = f.tool_json();
        assert_eq!(tool["type"], "openrouter:fusion");
        assert!(tool.get("parameters").is_none());
        assert!(f.tool_choice_value().is_none());

        // Populated config ⇒ snake_case parameter keys, only set fields.
        let f = FusionConfig {
            analysis_models: vec!["anthropic/claude-opus-4.8".into(), "  ".into()],
            judge_model: Some("openai/gpt-5.1".into()),
            max_tool_calls: Some(12),
            temperature: Some(0.7),
            tool_choice: "required".into(),
            ..Default::default()
        };
        let p = &f.tool_json()["parameters"];
        // blank entry filtered out
        assert_eq!(p["analysis_models"].as_array().unwrap().len(), 1);
        assert_eq!(p["analysis_models"][0], "anthropic/claude-opus-4.8");
        assert_eq!(p["model"], "openai/gpt-5.1");
        assert_eq!(p["max_tool_calls"], 12);
        assert_eq!(p["temperature"], 0.7);
        assert!(p.get("max_completion_tokens").is_none());
        assert_eq!(f.tool_choice_value().unwrap(), "required");
    }

    #[test]
    fn fusion_config_parses_camelcase_project_settings() {
        let json = r#"{
            "openrouterFusion": {
                "outerModel": "openrouter/anthropic/claude-opus-4.8",
                "analysisModels": ["anthropic/claude-opus-4.8", "openai/gpt-5.1"],
                "judgeModel": "openai/gpt-5.1",
                "maxToolCalls": 10,
                "toolChoice": "auto"
            }
        }"#;
        let c: ProjectConfig = serde_json::from_str(json).unwrap();
        let f = c.openrouter_fusion.unwrap();
        assert_eq!(f.outer_model, "openrouter/anthropic/claude-opus-4.8");
        assert_eq!(f.analysis_models.len(), 2);
        assert_eq!(f.judge_model.as_deref(), Some("openai/gpt-5.1"));
        assert_eq!(f.max_tool_calls, Some(10));
        // missing fields fall back to FusionConfig defaults
        assert!(f.temperature.is_none());
    }

    // dev-plan/33 Tier 2 — guiShell config parses both shapes.
    #[test]
    fn gui_shell_setting_parses_string_shorthand() {
        let json = r#"{ "guiShell": "session-explorer" }"#;
        let c: AppConfig = serde_json::from_str(json).unwrap();
        let s = c.gui_shell.unwrap();
        assert_eq!(s.tab_default(), Some("session-explorer"));
        assert_eq!(s.serve_default(), Some("session-explorer"));
    }

    #[test]
    fn gui_shell_setting_parses_long_form() {
        let json = r#"{
            "guiShell": {
                "tabDefault": "session-explorer",
                "serveDefault": "image-generator"
            }
        }"#;
        let c: AppConfig = serde_json::from_str(json).unwrap();
        let s = c.gui_shell.unwrap();
        assert_eq!(s.tab_default(), Some("session-explorer"));
        assert_eq!(s.serve_default(), Some("image-generator"));
    }

    #[test]
    fn gui_shell_setting_long_form_partial_is_ok() {
        let json = r#"{ "guiShell": { "tabDefault": "session-explorer" } }"#;
        let c: AppConfig = serde_json::from_str(json).unwrap();
        let s = c.gui_shell.unwrap();
        assert_eq!(s.tab_default(), Some("session-explorer"));
        assert_eq!(s.serve_default(), None);
    }

    #[test]
    fn gui_shell_setting_absent_by_default() {
        let c = AppConfig::default();
        assert!(c.gui_shell.is_none());
    }

    #[test]
    fn detect_provider_covers_known_prefixes() {
        let mut c = AppConfig::default();
        c.model = "gpt-4o".into();
        assert_eq!(c.detect_provider().unwrap(), "openai");
        c.model = "o1-preview".into();
        assert_eq!(c.detect_provider().unwrap(), "openai");
        c.model = "ollama/llama3.2".into();
        assert_eq!(c.detect_provider().unwrap(), "ollama");
        c.model = "gemini-2.5-flash".into();
        assert_eq!(c.detect_provider().unwrap(), "gemini");
    }

    #[test]
    fn detect_provider_rejects_unknown() {
        let mut c = AppConfig::default();
        c.model = "mysterymodel".into();
        assert!(c.detect_provider().is_err());
    }

    #[test]
    fn detect_provider_covers_openai_compat() {
        let mut c = AppConfig::default();
        c.model = "oai/gpt-4o-mini".into();
        assert_eq!(c.detect_provider().unwrap(), "openai-compat");
        c.model = "oai/llama-3.1-70b".into();
        assert_eq!(c.detect_provider().unwrap(), "openai-compat");
    }

    /// settings.json `extract_save_skill_models` deserializes from
    /// both string (single model) and array (priority list) forms.
    /// Backward compat: absent field stays `None` so older configs
    /// keep the v0.8.4 behaviour (frontmatter `model:` wins).
    #[test]
    fn extract_save_skill_models_accepts_string_and_array() {
        // Single string form.
        let single: ProjectConfig =
            serde_json::from_str(r#"{"extract_save_skill_models": "claude-sonnet-4-6"}"#).unwrap();
        assert_eq!(
            single.extract_save_skill_models,
            Some(crate::skills::SkillModelSpec::Single(
                "claude-sonnet-4-6".to_string()
            ))
        );

        // Priority-list form.
        let priority: ProjectConfig = serde_json::from_str(
            r#"{"extract_save_skill_models": ["claude-sonnet-4-6", "gpt-4o"]}"#,
        )
        .unwrap();
        match priority.extract_save_skill_models.unwrap() {
            crate::skills::SkillModelSpec::Priority(v) => assert_eq!(v.len(), 2),
            other => panic!("expected Priority, got {other:?}"),
        }

        // Camel-case alias also works (matches the rest of
        // ProjectConfig's field naming convention for users who
        // prefer camelCase keys).
        let camel: ProjectConfig =
            serde_json::from_str(r#"{"extractSaveSkillModels": "gpt-4o"}"#).unwrap();
        assert_eq!(
            camel.extract_save_skill_models,
            Some(crate::skills::SkillModelSpec::Single("gpt-4o".to_string()))
        );

        // Absent field → None.
        let absent: ProjectConfig = serde_json::from_str("{}").unwrap();
        assert!(absent.extract_save_skill_models.is_none());
    }

    /// A retired skill recommendation must not take the turn down with
    /// it: the fallback is on by default, and an explicit empty string
    /// turns it off for users who'd rather keep their own model.
    #[test]
    fn skill_model_fallback_defaults_on_and_can_be_disabled() {
        assert_eq!(
            AppConfig::default().skill_model_fallback.as_deref(),
            Some(DEFAULT_SKILL_MODEL_FALLBACK)
        );

        let set: ProjectConfig =
            serde_json::from_str(r#"{"skill_model_fallback": "qwen3.7-plus"}"#).unwrap();
        let mut cfg = AppConfig::default();
        set.apply_to(&mut cfg);
        assert_eq!(cfg.skill_model_fallback.as_deref(), Some("qwen3.7-plus"));

        let off: ProjectConfig = serde_json::from_str(r#"{"skillModelFallback": ""}"#).unwrap();
        let mut cfg = AppConfig::default();
        off.apply_to(&mut cfg);
        assert_eq!(cfg.skill_model_fallback, None, "empty string disables it");

        let absent: ProjectConfig = serde_json::from_str("{}").unwrap();
        let mut cfg = AppConfig::default();
        absent.apply_to(&mut cfg);
        assert_eq!(
            cfg.skill_model_fallback.as_deref(),
            Some(DEFAULT_SKILL_MODEL_FALLBACK),
            "an absent key keeps the default"
        );
    }

    /// settings.json `translator_subagent_model` deserializes from
    /// both snake_case and camelCase. Absent → None (current
    /// behaviour preserved).
    #[test]
    fn translator_subagent_model_settings_deserialize() {
        let snake: ProjectConfig =
            serde_json::from_str(r#"{"translator_subagent_model": "claude-sonnet-4-6"}"#).unwrap();
        assert_eq!(
            snake.translator_subagent_model.as_deref(),
            Some("claude-sonnet-4-6")
        );

        let camel: ProjectConfig =
            serde_json::from_str(r#"{"translatorSubagentModel": "gpt-4o"}"#).unwrap();
        assert_eq!(camel.translator_subagent_model.as_deref(), Some("gpt-4o"));

        let absent: ProjectConfig = serde_json::from_str("{}").unwrap();
        assert!(absent.translator_subagent_model.is_none());
    }

    /// ProjectConfig::apply_to propagates the override into AppConfig.
    #[test]
    fn translator_subagent_model_merges_into_app_config() {
        let pc = ProjectConfig {
            translator_subagent_model: Some("claude-sonnet-4-6".into()),
            ..Default::default()
        };
        let mut config = AppConfig::default();
        pc.apply_to(&mut config);
        assert_eq!(
            config.translator_subagent_model.as_deref(),
            Some("claude-sonnet-4-6")
        );
    }

    /// apply_to() merges the override from ProjectConfig into the
    /// resolved AppConfig.
    #[test]
    fn extract_save_skill_models_merges_into_app_config() {
        let pc = ProjectConfig {
            extract_save_skill_models: Some(crate::skills::SkillModelSpec::Single(
                "claude-sonnet-4-6".into(),
            )),
            ..Default::default()
        };
        let mut config = AppConfig::default();
        pc.apply_to(&mut config);
        assert_eq!(
            config.extract_save_skill_models,
            Some(crate::skills::SkillModelSpec::Single(
                "claude-sonnet-4-6".into()
            ))
        );
    }

    #[test]
    fn null_team_enabled_upgrades_to_false_on_load() {
        let loaded: ProjectConfig = serde_json::from_str(r#"{"teamEnabled": null}"#).unwrap();
        assert_eq!(loaded.team_enabled, Some(false));
        let reserialized = serde_json::to_string(&loaded).unwrap();
        assert!(reserialized.contains(r#""teamEnabled":false"#));
        assert!(!reserialized.contains(r#""teamEnabled":null"#));
    }

    #[test]
    fn default_serializes_team_enabled_false_not_null() {
        let json = serde_json::to_string(&ProjectConfig::default()).unwrap();
        assert!(
            json.contains(r#""teamEnabled":false"#),
            "expected explicit false, got: {json}"
        );
        assert!(!json.contains(r#""teamEnabled":null"#));
    }

    /// A workspace whose `.thclaws/settings.json` still pins a model that
    /// has since been retired upstream must come back on the successor.
    ///
    /// This is the whole point of doing it at load rather than in the
    /// credential-aware default: a persisted `model` makes `model_explicit`
    /// true, so that default never re-runs for an existing workspace. The
    /// retired id has no priced catalogue entry, so the gateway cannot serve
    /// it, routing falls through to BYOK, and a gateway user with no keys of
    /// their own is told "No keys" on a workspace that worked yesterday.
    #[test]
    fn load_repoints_a_workspace_pinned_to_a_retired_model() {
        let _guard = crate::kms::test_env_lock();
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".thclaws")).unwrap();

        let prev = std::env::var("THCLAWS_PROJECT_ROOT").ok();
        std::env::set_var("THCLAWS_PROJECT_ROOT", dir.path());

        let mut loaded = Vec::new();
        for pinned in ["deepseek-v4-flash", "deepseek/deepseek-v4-flash"] {
            std::fs::write(
                dir.path().join(".thclaws/settings.json"),
                format!(r#"{{"model": "{pinned}"}}"#),
            )
            .unwrap();
            loaded.push(AppConfig::load().unwrap().model);
        }

        // A live id must survive untouched — the repoint is targeted, not a
        // blanket rewrite of whatever the user pinned.
        std::fs::write(
            dir.path().join(".thclaws/settings.json"),
            r#"{"model": "claude-opus-4-6"}"#,
        )
        .unwrap();
        let untouched = AppConfig::load().unwrap().model;

        match prev {
            Some(v) => std::env::set_var("THCLAWS_PROJECT_ROOT", v),
            None => std::env::remove_var("THCLAWS_PROJECT_ROOT"),
        }

        assert_eq!(loaded[0], "deepseek-flash", "bare retired id must repoint");
        assert_eq!(
            loaded[1], "deepseek-flash",
            "prefixed retired id must repoint too"
        );
        assert_eq!(
            untouched, "claude-opus-4-6",
            "a live pin must be left alone"
        );
    }

    #[test]
    fn project_config_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let pc = ProjectConfig {
            model: Some("gpt-4o".into()),
            max_tokens: Some(4096),
            permissions: Some(PermissionsConfig::Mode("auto".into())),
            ..Default::default()
        };
        std::fs::write(&path, serde_json::to_string_pretty(&pc).unwrap()).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let loaded: ProjectConfig = serde_json::from_str(&contents).unwrap();
        assert_eq!(loaded.model.as_deref(), Some("gpt-4o"));
        assert_eq!(loaded.max_tokens, Some(4096));
    }

    #[test]
    fn partial_settings_fills_defaults() {
        let pc: ProjectConfig = serde_json::from_str(r#"{"model": "claude-opus-4-6"}"#).unwrap();
        let mut c = AppConfig::default();
        pc.apply_to(&mut c);
        assert_eq!(c.model, "claude-opus-4-6");
        // defaults retained for omitted fields
        assert_eq!(c.max_tokens, 32000);
        assert_eq!(c.permissions, "auto");
    }

    #[test]
    fn mcp_servers_loaded_from_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{
            "mcpServers": {
                "filesystem": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
                },
                "weather": {
                    "command": "/usr/local/bin/weatherd",
                    "args": []
                }
            }
        }"#,
        )
        .unwrap();

        let servers = ProjectConfig::parse_mcp_json(&path).unwrap();
        assert_eq!(servers.len(), 2);
        let fs_server = servers.iter().find(|s| s.name == "filesystem").unwrap();
        assert_eq!(fs_server.command, "npx");
        assert_eq!(fs_server.args.len(), 3);
    }

    #[test]
    fn permissions_claude_code_format() {
        let json = r#"{
            "permissions": {
                "allow": ["Read", "Glob", "Grep", "Write", "Edit", "Bash(*)"],
                "deny": ["WebFetch"]
            }
        }"#;
        let pc: ProjectConfig = serde_json::from_str(json).unwrap();
        let perms = pc.permissions.unwrap();
        assert_eq!(perms.mode(), "auto"); // has allow list → auto
        let allowed = perms.allowed_tools().unwrap();
        assert!(allowed.contains(&"Read".to_string()));
        assert!(allowed.contains(&"Bash".to_string())); // "Bash(*)" → "Bash"
        let denied = perms.disallowed_tools().unwrap();
        assert_eq!(denied, vec!["WebFetch"]);
    }

    #[test]
    fn permissions_simple_string_format() {
        let json = r#"{"permissions": "ask"}"#;
        let pc: ProjectConfig = serde_json::from_str(json).unwrap();
        let perms = pc.permissions.unwrap();
        assert_eq!(perms.mode(), "ask");
        assert!(perms.allowed_tools().is_none());
    }

    #[test]
    fn permissions_apply_to_config() {
        let json = r#"{
            "permissions": {
                "allow": ["Read", "Write", "Bash(*)"]
            }
        }"#;
        let pc: ProjectConfig = serde_json::from_str(json).unwrap();
        let mut cfg = AppConfig::default();
        pc.apply_to(&mut cfg);
        assert_eq!(cfg.permissions, "auto");
        assert_eq!(cfg.allowed_tools.unwrap(), vec!["Read", "Write", "Bash"]);
    }

    #[test]
    fn media_tools_flag_accepts_legacy_and_preferred_names() {
        // Legacy key (still the serialized form for back-compat).
        let a: ProjectConfig = serde_json::from_str(r#"{"imageToolsEnabled": true}"#).unwrap();
        assert_eq!(a.image_tools_enabled, Some(true));
        // Preferred alias — these tools are image + video now.
        let b: ProjectConfig = serde_json::from_str(r#"{"mediaToolsEnabled": true}"#).unwrap();
        assert_eq!(b.image_tools_enabled, Some(true));
    }

    /// First-run bootstrap exposes every ProjectConfig field name so
    /// users discover available knobs by opening the file, and is
    /// idempotent on second call (no clobbering of user edits).
    /// Combined into one test because both touch the global
    /// `THCLAWS_PROJECT_ROOT` env var; splitting them would race
    /// under cargo's default parallel test runner.
    ///
    /// When a new field is added to ProjectConfig, both the bootstrap
    /// body and the `expected` list below must grow — the field-list
    /// assertion fails otherwise.
    /// `THCLAWS_BROWSER_ENABLED=0` turns the browser off for a whole
    /// fleet — but only as the DEFAULT. A workspace that asks for it in
    /// settings.json still gets it, which is what makes this safe to set
    /// platform-wide.
    #[test]
    fn browser_env_sets_the_default_but_explicit_settings_still_win() {
        let _guard = crate::kms::test_env_lock();

        std::env::remove_var("THCLAWS_BROWSER_ENABLED");
        assert!(default_browser_enabled(), "unset → on");

        for off in ["0", "false", "off", "no", "FALSE"] {
            std::env::set_var("THCLAWS_BROWSER_ENABLED", off);
            assert!(!default_browser_enabled(), "{off} should disable");
            assert!(
                !AppConfig::default().browser_enabled,
                "{off} must reach the base config, not just the serde default"
            );
        }

        std::env::set_var("THCLAWS_BROWSER_ENABLED", "1");
        assert!(default_browser_enabled(), "1 → on");

        // The whole point: an explicit opt-in still overrides the fleet default.
        std::env::set_var("THCLAWS_BROWSER_ENABLED", "0");
        let mut cfg = AppConfig::default();
        assert!(!cfg.browser_enabled);
        let pc: ProjectConfig = serde_json::from_str(r#"{"browserEnabled": true}"#).unwrap();
        pc.apply_to(&mut cfg);
        assert!(
            cfg.browser_enabled,
            "settings.json wins over the env default"
        );

        std::env::remove_var("THCLAWS_BROWSER_ENABLED");
    }

    /// New workspaces opt OUT of the managed browser. The compiled
    /// default stays ON, so existing projects (whose settings.json has
    /// no such key) keep it — this is a first-run choice, not a global
    /// flip.
    #[test]
    fn new_workspace_template_disables_the_browser_without_moving_the_global_default() {
        let _guard = crate::kms::test_env_lock();
        let dir = tempdir().unwrap();
        std::env::set_var("THCLAWS_PROJECT_ROOT", dir.path());
        assert!(ProjectConfig::ensure_default_exists());

        let raw = std::fs::read_to_string(dir.path().join(".thclaws/settings.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["browserEnabled"], serde_json::json!(false));

        let cfg: ProjectConfig = serde_json::from_str(&raw).unwrap();
        assert_eq!(cfg.browser_enabled, Some(false), "the loader reads it back");

        // A workspace that simply doesn't mention the key is unchanged.
        let silent: ProjectConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(silent.browser_enabled, None);
        assert!(default_browser_enabled(), "global default still on");

        std::env::remove_var("THCLAWS_PROJECT_ROOT");
    }

    #[test]
    fn ensure_default_exists_writes_full_template_then_is_idempotent() {
        let _guard = crate::kms::test_env_lock();
        let dir = tempdir().unwrap();
        std::env::set_var("THCLAWS_PROJECT_ROOT", dir.path());

        assert!(ProjectConfig::ensure_default_exists());
        let path = dir.path().join(".thclaws/settings.json");
        let body = std::fs::read_to_string(&path).unwrap();

        let expected = [
            "model",
            "permissions",
            "maxTokens",
            "maxIterations",
            "planContextStrategy",
            "skillsListingStrategy",
            "extract_save_skill_models",
            "translator_subagent_model",
            "thinkingBudget",
            "searchEngine",
            "allowedTools",
            "disallowedTools",
            "windowWidth",
            "windowHeight",
            "guiScale",
            "teamEnabled",
            "showRawResponse",
            "kms",
        ];
        for field in expected {
            assert!(
                body.contains(&format!("\"{field}\"")),
                "bootstrap missing field {field}"
            );
        }
        let parsed: ProjectConfig = serde_json::from_str(&body).unwrap();
        // model is intentionally null in the bootstrap so the credential-aware
        // default (preferred_default_model) drives the choice on first run.
        assert!(parsed.model.is_none(), "bootstrap leaves model unpinned");

        // Idempotent: a user edit survives a second bootstrap call.
        std::fs::write(&path, r#"{"model":"custom-model"}"#).unwrap();
        assert!(!ProjectConfig::ensure_default_exists());
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("custom-model"));

        std::env::remove_var("THCLAWS_PROJECT_ROOT");
    }

    #[test]
    fn default_gateway_proxy_injection_parses_and_sets_flag() {
        // The real bootstrap gates the injection behind a live gateway
        // credential AND `!cfg!(test)`, so exercise the injection itself:
        // it must turn a clean template into valid JSON with gatewayProxy on
        // without disturbing the other fields.
        let template = r#"{
  "_doc": "...",
  "model": "gpt-4.1",
  "kms": { "active": [] }
}
"#;
        let body = ProjectConfig::inject_default_gateway_proxy(template);
        assert_ne!(body, template, "injection must change the template");
        let pc: ProjectConfig = serde_json::from_str(&body).unwrap();
        assert_eq!(pc.gateway_proxy, Some(true), "proxy defaulted on");
        assert_eq!(pc.model.as_deref(), Some("gpt-4.1"), "model untouched");
    }

    #[test]
    fn api_key_honors_env_per_provider() {
        // Disable the keychain fallback for this test — otherwise a
        // real entry on the developer's machine would make the
        // "returns None when env is unset" assertion flake.
        std::env::set_var("THCLAWS_DISABLE_KEYCHAIN", "1");
        let mut c = AppConfig::default();
        c.model = "gpt-4o".into();
        std::env::set_var("OPENAI_API_KEY", "sk-test-openai");
        assert_eq!(c.api_key_from_env().as_deref(), Some("sk-test-openai"));
        std::env::remove_var("OPENAI_API_KEY");
        assert_eq!(c.api_key_from_env(), None);
        std::env::remove_var("THCLAWS_DISABLE_KEYCHAIN");
    }

    /// Covers all three behaviors the `--set-model` polish points cared
    /// about: file-missing → fall-back-and-create, file-present →
    /// update model in place without touching other settings, and
    /// file-unreadable → bail rather than clobber. Driven through the
    /// `persist_model_at_path` helper with an explicit tempdir path so
    /// we don't need to mutate `THCLAWS_PROJECT_ROOT` — env-var
    /// mutations on a test thread race with `posix_spawn` in the
    /// concurrent `schedule::tests` suite (EINVAL out of fork+exec when
    /// the env table moves mid-walk).
    #[test]
    fn persist_model_at_path_handles_missing_existing_and_unreadable() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".thclaws/settings.json");
        let default_fallback = ProjectConfig::default;

        // (1) File missing → uses the fallback and writes it out.
        persist_model_at_path(&path, default_fallback, "gpt-test-1").unwrap();
        let pc: ProjectConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(pc.model.as_deref(), Some("gpt-test-1"));

        // (2) Existing settings (`maxTokens`) survive a model update —
        // guards against dome's original `load().unwrap_or_default()`
        // clobber footgun.
        std::fs::write(&path, r#"{"model":"old-model","maxTokens":12345}"#).unwrap();
        persist_model_at_path(&path, default_fallback, "gpt-test-2").unwrap();
        let after: ProjectConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after.model.as_deref(), Some("gpt-test-2"));
        assert_eq!(after.max_tokens, Some(12345));

        // (3) Unreadable existing file → Err, file unchanged. Without
        // this guard, a transient parse failure would silently reset
        // every sibling field to its default.
        std::fs::write(&path, "{not valid json").unwrap();
        let err = persist_model_at_path(&path, default_fallback, "gpt-test-3").unwrap_err();
        assert!(
            format!("{err}").contains("unreadable"),
            "expected bail message, got: {err}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not valid json");
    }

    /// In-memory CLI override APIs (set / get / clear) are the bridge
    /// that lets `app.rs` reach every dispatch surface's
    /// `AppConfig::load`. Test directly — no env vars — to avoid the
    /// `posix_spawn` race described above.
    #[test]
    fn cli_model_override_set_get_clear() {
        let _guard = crate::kms::test_env_lock();
        clear_cli_model_override();
        assert_eq!(cli_model_override(), None);
        set_cli_model_override("cli-override-model".into());
        assert_eq!(cli_model_override().as_deref(), Some("cli-override-model"));
        clear_cli_model_override();
        assert_eq!(cli_model_override(), None);
    }

    // ── workspace v1 → v2 layout migration ─────────────────────────────

    fn wv(dir: &std::path::Path) -> Option<u32> {
        let raw = std::fs::read_to_string(dir.join("settings.json")).ok()?;
        serde_json::from_str::<serde_json::Value>(&raw)
            .ok()?
            .get("workspaceVersion")?
            .as_u64()
            .map(|v| v as u32)
    }

    #[test]
    fn migrate_moves_legacy_runtime_into_state() {
        let dir = tempdir().unwrap();
        let tc = dir.path();
        std::fs::create_dir_all(tc.join("sessions")).unwrap();
        std::fs::write(tc.join("sessions/x.jsonl"), "s").unwrap();
        std::fs::create_dir_all(tc.join("kms/ai/pages")).unwrap();
        std::fs::write(tc.join("todos.md"), "- [ ] a").unwrap();
        std::fs::write(tc.join("media-jobs.jsonl"), "{\"id\":\"j1\"}").unwrap();
        // A config-tier dir must NOT move.
        std::fs::create_dir_all(tc.join("agents")).unwrap();
        std::fs::write(tc.join("agents/a.md"), "x").unwrap();

        ProjectConfig::migrate_workspace_at(tc);

        assert_eq!(
            std::fs::read_to_string(tc.join("state/sessions/x.jsonl")).unwrap(),
            "s"
        );
        assert!(tc.join("state/kms/ai/pages").is_dir());
        assert_eq!(
            std::fs::read_to_string(tc.join("state/todos.md")).unwrap(),
            "- [ ] a"
        );
        assert_eq!(
            std::fs::read_to_string(tc.join("state/media-jobs.jsonl")).unwrap(),
            "{\"id\":\"j1\"}",
            "in-flight video jobs move with their operation refs intact"
        );
        assert!(!tc.join("sessions").exists(), "legacy dir removed");
        assert!(!tc.join("todos.md").exists());
        assert!(!tc.join("media-jobs.jsonl").exists());
        assert!(tc.join("agents/a.md").is_file(), "config dir untouched");
        assert_eq!(wv(tc), Some(2), "stamped v2");
        assert_eq!(
            std::fs::read_to_string(tc.join("state/.gitignore")).unwrap(),
            "*\n"
        );
    }

    #[test]
    fn migrate_is_idempotent() {
        let dir = tempdir().unwrap();
        let tc = dir.path();
        std::fs::create_dir_all(tc.join("sessions")).unwrap();
        std::fs::write(tc.join("sessions/x.jsonl"), "s").unwrap();

        ProjectConfig::migrate_workspace_at(tc);
        ProjectConfig::migrate_workspace_at(tc);

        assert_eq!(
            std::fs::read_to_string(tc.join("state/sessions/x.jsonl")).unwrap(),
            "s"
        );
        assert_eq!(wv(tc), Some(2));
    }

    #[test]
    fn migrate_presence_driven_does_not_clobber_existing_state() {
        let dir = tempdir().unwrap();
        let tc = dir.path();
        // Both legacy and already-migrated sessions exist (partial state).
        std::fs::create_dir_all(tc.join("sessions")).unwrap();
        std::fs::write(tc.join("sessions/x.jsonl"), "LEGACY").unwrap();
        std::fs::create_dir_all(tc.join("state/sessions")).unwrap();
        std::fs::write(tc.join("state/sessions/x.jsonl"), "KEEP").unwrap();

        ProjectConfig::migrate_workspace_at(tc);

        // Target present → skipped; migrated copy preserved, not clobbered.
        assert_eq!(
            std::fs::read_to_string(tc.join("state/sessions/x.jsonl")).unwrap(),
            "KEEP"
        );
    }

    #[test]
    fn migrate_gating_skips_when_already_v2() {
        let dir = tempdir().unwrap();
        let tc = dir.path();
        std::fs::write(tc.join("settings.json"), r#"{"workspaceVersion":2}"#).unwrap();
        // A stray legacy dir must be left untouched once v2 is declared.
        std::fs::create_dir_all(tc.join("sessions")).unwrap();
        std::fs::write(tc.join("sessions/x.jsonl"), "s").unwrap();

        ProjectConfig::migrate_workspace_at(tc);

        assert!(tc.join("sessions/x.jsonl").is_file(), "not moved when v2");
        assert!(!tc.join("state/sessions").exists());
        assert!(
            tc.join("state/.gitignore").is_file(),
            "scaffold still ensured"
        );
    }

    #[test]
    fn migrate_brand_new_is_noop() {
        let dir = tempdir().unwrap();
        let tc = dir.path().join("thclaws");
        std::fs::create_dir_all(&tc).unwrap();

        ProjectConfig::migrate_workspace_at(&tc);

        assert!(
            !tc.join("settings.json").exists(),
            "no minting on empty dir"
        );
        assert!(!tc.join("state").exists());
    }

    #[test]
    fn migrate_unparseable_settings_is_noop() {
        let dir = tempdir().unwrap();
        let tc = dir.path();
        std::fs::write(tc.join("settings.json"), "{ this is not json").unwrap();
        std::fs::create_dir_all(tc.join("sessions")).unwrap();
        std::fs::write(tc.join("sessions/x.jsonl"), "s").unwrap();

        ProjectConfig::migrate_workspace_at(tc);

        assert!(
            tc.join("sessions/x.jsonl").is_file(),
            "no move on bad settings"
        );
        assert!(!tc.join("state").exists());
        assert_eq!(
            std::fs::read_to_string(tc.join("settings.json")).unwrap(),
            "{ this is not json",
            "bad settings left untouched"
        );
    }

    #[test]
    fn migrate_preserves_existing_settings_keys() {
        let dir = tempdir().unwrap();
        let tc = dir.path();
        std::fs::write(
            tc.join("settings.json"),
            r#"{"_doc":"keep me","model":"x"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(tc.join("sessions")).unwrap();
        std::fs::write(tc.join("sessions/x.jsonl"), "s").unwrap();

        ProjectConfig::migrate_workspace_at(tc);

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tc.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(v["_doc"], "keep me");
        assert_eq!(v["model"], "x");
        assert_eq!(v["workspaceVersion"], 2);
    }

    #[test]
    fn migrate_splits_legacy_workflows() {
        let dir = tempdir().unwrap();
        let tc = dir.path();
        std::fs::create_dir_all(tc.join("workflows/wf-1")).unwrap();
        std::fs::write(tc.join("workflows/run.js"), "// authored").unwrap();
        std::fs::write(tc.join("workflows/wf-1/state.jsonl"), "{}").unwrap();

        ProjectConfig::migrate_workspace_at(tc);

        // Authored script → config tier; run-state → state/.
        assert_eq!(
            std::fs::read_to_string(tc.join("agent_workflow/run.js")).unwrap(),
            "// authored"
        );
        assert!(tc.join("state/workflows/wf-1/state.jsonl").is_file());
        // Seed copies the authored script into the runtime dir too.
        assert_eq!(
            std::fs::read_to_string(tc.join("state/workflows/run.js")).unwrap(),
            "// authored"
        );
        assert!(!tc.join("workflows").exists(), "legacy dir removed");
    }

    #[test]
    fn migrate_seeds_authored_workflows_on_v2_open() {
        let dir = tempdir().unwrap();
        let tc = dir.path();
        std::fs::write(tc.join("settings.json"), r#"{"workspaceVersion":2}"#).unwrap();
        std::fs::create_dir_all(tc.join("agent_workflow")).unwrap();
        std::fs::write(tc.join("agent_workflow/run.js"), "// v2").unwrap();

        ProjectConfig::migrate_workspace_at(tc);

        assert_eq!(
            std::fs::read_to_string(tc.join("state/workflows/run.js")).unwrap(),
            "// v2"
        );
    }

    /// The masking toggle only bites if something actually pushes it into
    /// the process global. It shipped broken in `-p` once because the hook
    /// sat on a wrapper both binaries skip, so pin the single hook here —
    /// every surface calls this one function.
    #[test]
    fn apply_process_globals_arms_and_disarms_masking() {
        let _pin = crate::sensitive::pin_for_test();
        let mut cfg = AppConfig::default();

        cfg.sensitive_enabled = true;
        cfg.apply_process_globals();
        let armed = crate::sensitive::active().is_some();

        cfg.sensitive_enabled = false;
        cfg.apply_process_globals();
        let disarmed = crate::sensitive::active().is_none();

        assert!(armed, "enabled setting did not arm the masker");
        assert!(disarmed, "disabled setting left the masker armed");
    }

    /// dev-plan/55 masking toggle: the nested `sensitive` block reaches
    /// `AppConfig`, and its absence leaves masking OFF. A settings file that
    /// fails to parse silently defaults every flag off (see
    /// `ProjectConfig::load`), so pin the wiring rather than the UI.
    #[test]
    fn sensitive_block_drives_the_masking_flag() {
        let parse = |json: &str| {
            let pc: ProjectConfig = serde_json::from_str(json).unwrap();
            let mut cfg = AppConfig::default();
            pc.apply_to(&mut cfg);
            cfg.sensitive_enabled
        };
        assert!(parse(r#"{"sensitive":{"enabled":true}}"#));
        assert!(!parse(r#"{"sensitive":{"enabled":false}}"#));
        assert!(!parse(r#"{"sensitive":{}}"#), "empty block must not enable");
        assert!(!parse("{}"), "absent block must not enable");
    }
}
