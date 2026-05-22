//! Runtime configuration for the native agent.
//!
//! Load order (higher wins):
//!   1. CLI flags
//!   2. `.thclaws/settings.json` (project)
//!   3. `~/.config/thclaws/settings.json` (user)
//!   4. `~/.claude/settings.json` (Claude Code fallback)
//!   5. Compiled-in defaults
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

    /// Search engine for WebSearch tool: "auto" (default), "tavily", "brave", "duckduckgo".
    pub search_engine: String,

    /// Allowed tool names (None = all). CLI: --allowed-tools
    #[serde(skip)]
    pub allowed_tools: Option<Vec<String>>,

    /// Disallowed tool names (None = none). CLI: --disallowed-tools
    #[serde(skip)]
    pub disallowed_tools: Option<Vec<String>>,

    /// Resume session ID (None = new session). CLI: --resume
    #[serde(skip)]
    pub resume_session: Option<String>,

    /// Lifecycle hooks — shell commands fired on agent events.
    pub hooks: crate::hooks::HooksConfig,

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
}

/// Default stream-chunk idle timeout. Used by `serde(default = ...)`
/// for backward-compat with settings files that pre-date the field.
fn default_stream_chunk_timeout_secs() -> u64 {
    120
}

fn default_auto_learn_kms() -> String {
    "self_learn".to_string()
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
            thinking_budget: Some(10000),
            stream_chunk_timeout_secs: default_stream_chunk_timeout_secs(),
            search_engine: "auto".to_string(),
            allowed_tools: None,
            disallowed_tools: None,
            resume_session: None,
            hooks: crate::hooks::HooksConfig::default(),
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
            auto_learn_kms: default_auto_learn_kms(),
            auto_learn_reconcile_hours: default_auto_learn_reconcile_hours(),
            claude_md_compat: false,
            openrouter_free_only: false,
            gateway_use_for: Vec::new(),
            extract_save_skill_models: None,
            translator_subagent_model: None,
            remote_agent_url: None,
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
    /// Print the assistant's raw text to stderr after each turn (dim, fenced
    /// block). Same effect as `THCLAWS_SHOW_RAW=1`. The env var wins if set.
    /// Useful when debugging model output / formatting issues.
    #[serde(rename = "showRawResponse")]
    pub show_raw_response: Option<bool>,
    /// Knowledge-base settings — `{ "active": ["name1", ...] }`.
    pub kms: Option<KmsSettings>,
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
    /// Default target for `/deploy`. See
    /// [`AppConfig::remote_agent_url`].
    #[serde(rename = "remoteAgentUrl")]
    pub remote_agent_url: Option<String>,
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
            max_tokens: None,
            max_iterations: None,
            plan_context_strategy: None,
            skills_listing_strategy: None,
            extract_save_skill_models: None,
            translator_subagent_model: None,
            thinking_budget: None,
            search_engine: None,
            allowed_tools: None,
            disallowed_tools: None,
            window_width: None,
            window_height: None,
            gui_scale: None,
            team_enabled: Some(false),
            show_raw_response: None,
            kms: None,
            auto_learn: None,
            auto_learn_kms: None,
            auto_learn_reconcile_hours: None,
            openrouter_free_only: None,
            gateway_use_for: None,
            remote_agent_url: None,
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
            return serde_json::from_str(&contents).ok();
        }
        // Try .claude/settings.json (Claude Code compat).
        let claude_path = std::env::current_dir().ok()?.join(".claude/settings.json");
        if claude_path.exists() {
            let contents = std::fs::read_to_string(&claude_path).ok()?;
            return serde_json::from_str(&contents).ok();
        }
        None
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, s)?;
        Ok(())
    }

    /// First-run bootstrap: when neither `.thclaws/settings.json` nor a
    /// Claude Code fallback (`.claude/settings.json`) exists in the
    /// project, write `.thclaws/settings.json` with the project-launch
    /// defaults. Returns whether a file was written. Silent on I/O
    /// errors — bootstrap is best-effort and shouldn't kill startup.
    pub fn ensure_default_exists() -> bool {
        let path = Self::path();
        if path.exists() {
            return false;
        }
        let claude_path = std::env::current_dir()
            .ok()
            .map(|p| p.join(".claude/settings.json"));
        if let Some(p) = claude_path {
            if p.exists() {
                return false;
            }
        }
        // Hand-rolled JSON enumerating every ProjectConfig field at
        // its default value so users discover available knobs by
        // opening the file rather than consulting the manual. Unknown
        // keys (`_doc`) are tolerated by the loader (no
        // `deny_unknown_fields`). Removing a field falls back to the
        // compiled-in default; null on an Option field has the same
        // effect. Keep this list in sync with `ProjectConfig` whenever
        // a field is added.
        let body = r#"{
  "_doc": "thClaws project settings. Every available field is listed below at its default value — change a value to override, or delete a field (or set it to null on Option fields) to inherit the global default. windowWidth/windowHeight default to a monitor-resolution-aware size picked at GUI startup (1760x962 on >=1920x1080 displays, 1200x800 otherwise) when left null. See user-manual ch10 for the field reference.",
  "model": "gpt-4.1",
  "permissions": "auto",
  "maxTokens": 32000,
  "maxIterations": 50,
  "thinkingBudget": 10000,
  "searchEngine": "auto",
  "planContextStrategy": "compact",
  "skillsListingStrategy": "full",
  "teamEnabled": false,
  "showRawResponse": false,
  "allowedTools": null,
  "disallowedTools": null,
  "windowWidth": null,
  "windowHeight": null,
  "guiScale": null,
  "extract_save_skill_models": null,
  "translator_subagent_model": null,
  "claude_md_compat": false,
  "openrouterFreeOnly": false,
  "kms": { "active": [] }
}
"#;
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return false;
            }
        }
        std::fs::write(&path, body).is_ok()
    }

    /// Replace the active-KMS list in `.thclaws/settings.json` and
    /// write it back. Preserves every other field that was already
    /// there. Creates the file if it doesn't exist yet.
    pub fn set_active_kms(active: Vec<String>) -> Result<()> {
        let mut current = Self::load().unwrap_or_default();
        current.kms = Some(KmsSettings { active });
        current.save()
    }

    /// Merge overrides into an AppConfig (non-None fields win).
    pub fn apply_to(&self, config: &mut AppConfig) {
        if let Some(ref m) = self.model {
            config.model = crate::providers::ProviderKind::resolve_alias(m);
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
        if let Some(ref kms) = self.kms {
            config.kms_active = kms.active.clone();
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
        if let Some(ref m) = self.translator_subagent_model {
            config.translator_subagent_model = Some(m.clone());
        }
        if let Some(b) = self.openrouter_free_only {
            config.openrouter_free_only = b;
        }
        if let Some(ref providers) = self.gateway_use_for {
            config.gateway_use_for = providers
                .iter()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Some(ref url) = self.remote_agent_url {
            let trimmed = url.trim();
            if !trimmed.is_empty() {
                config.remote_agent_url = Some(trimmed.to_string());
            }
        }
    }

    pub fn set_model(&mut self, model: &str) {
        self.model = Some(model.to_string());
    }

    /// Persist the `/deploy` default target URL. Pair with the
    /// `remote-agent-token` keychain entry (managed by
    /// [`crate::secrets`]) for the bearer token.
    pub fn set_remote_agent_url(&mut self, url: Option<&str>) {
        self.remote_agent_url = url.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
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

    /// Persist the set of providers routed through the gateway.
    pub fn set_gateway_use_for(&mut self, providers: Vec<String>) {
        self.gateway_use_for = Some(
            providers
                .into_iter()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
        );
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
    /// Load config following the documented precedence.
    /// Load order: env override → user settings.json → Claude Code fallback →
    ///             defaults → project overlay.
    pub fn load() -> Result<Self> {
        // 1. Explicit env override.
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(p) = std::env::var("THCLAWS_CONFIG") {
            candidates.push(PathBuf::from(p));
        }
        // 2. User-level: ~/.config/thclaws/settings.json.
        candidates.extend(Self::user_config_paths());

        let mut config = None;
        for path in &candidates {
            if !path.exists() {
                continue;
            }
            let contents = std::fs::read_to_string(path)?;
            let pc: ProjectConfig = serde_json::from_str(&contents)
                .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
            let mut cfg = Self::default();
            pc.apply_to(&mut cfg);
            config = Some(cfg);
            break;
        }

        // 3. Claude Code fallback.
        if config.is_none() {
            config = Self::load_claude_code_fallback();
        }

        let mut config = config.unwrap_or_default();

        // User-level MCP: ~/.config/thclaws/mcp.json, then ~/.claude/mcp.json.
        if config.mcp_servers.is_empty() {
            config.mcp_servers = Self::load_user_mcp_servers();
        }

        // Project-level overrides from .thclaws/settings.json (or legacy .thclaws.toml).
        if let Some(project) = ProjectConfig::load() {
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

        // CLI `--model` / `--set-model` override (set by app.rs once at
        // startup). Applied last so it wins over user, project, and the
        // Claude Code fallback — matches the precedence the module docs
        // promise. Cleared by `clear_cli_model_override` if auto-fallback
        // decides the user's choice was unreachable.
        if let Some(m) = cli_model_override() {
            config.model = m;
        }

        Ok(config)
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

    /// Fallback: read Claude Code's `~/.claude/settings.json` if our config
    /// is missing. Extracts model, permission mode. Returns None if not found.
    pub fn load_claude_code_fallback() -> Option<Self> {
        let home = crate::util::home_dir()?;
        let path = home.join(".claude/settings.json");
        let contents = std::fs::read_to_string(path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&contents).ok()?;
        let mut config = Self::default();
        if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
            config.model = crate::providers::ProviderKind::resolve_alias(m);
        }
        if let Some(mode) = v
            .get("permissions")
            .and_then(|p| p.get("default_mode"))
            .and_then(|m| m.as_str())
        {
            config.permissions = match mode {
                "bypassPermissions" | "acceptEdits" => "auto",
                _ => "ask",
            }
            .to_string();
        }
        Some(config)
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
        let kind = self.detect_provider_kind().ok()?;
        let var = kind.api_key_env()?;
        // Treat an exported-but-empty env var ("ANTHROPIC_API_KEY=") as
        // unset and fall through to the keychain. A stale shell rc or
        // VS Code env injection can leave the var present but blank;
        // returning Some("") from here would produce an empty bearer
        // token and a confusing 401 on every request.
        if let Ok(value) = std::env::var(var) {
            if !value.trim().is_empty() {
                if std::env::var("THCLAWS_KEYCHAIN_TRACE").is_ok() {
                    eprintln!(
                        "\x1b[35m[keychain pid={}] api_key_from_env({}) → from env {}\x1b[0m",
                        std::process::id(),
                        kind.name(),
                        var
                    );
                }
                return Some(value);
            }
        }
        if std::env::var("THCLAWS_KEYCHAIN_TRACE").is_ok() {
            eprintln!(
                "\x1b[35m[keychain pid={}] api_key_from_env({}) → env {} unset or blank, falling back to keychain\x1b[0m",
                std::process::id(), kind.name(), var
            );
        }
        // Fall back to the keychain under the provider's short name.
        crate::secrets::get(kind.name())
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
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// Serializes tests that mutate process-global env vars
    /// (`THCLAWS_PROJECT_ROOT`, `THCLAWS_CONFIG`, etc.). Without this,
    /// cargo's default parallel runner lets one test's `remove_var`
    /// race with another's mid-flight `read_to_string`.
    static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn default_config_is_anthropic_sonnet() {
        let c = AppConfig::default();
        assert_eq!(c.model, "claude-sonnet-4-6");
        assert_eq!(c.detect_provider().unwrap(), "anthropic");
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
    #[test]
    fn ensure_default_exists_writes_full_template_then_is_idempotent() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        assert_eq!(parsed.model.as_deref(), Some("gpt-4.1"));

        // Idempotent: a user edit survives a second bootstrap call.
        std::fs::write(&path, r#"{"model":"custom-model"}"#).unwrap();
        assert!(!ProjectConfig::ensure_default_exists());
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("custom-model"));

        std::env::remove_var("THCLAWS_PROJECT_ROOT");
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
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_cli_model_override();
        assert_eq!(cli_model_override(), None);
        set_cli_model_override("cli-override-model".into());
        assert_eq!(cli_model_override().as_deref(), Some("cli-override-model"));
        clear_cli_model_override();
        assert_eq!(cli_model_override(), None);
    }
}
