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
            search_engine: "auto".to_string(),
            allowed_tools: None,
            disallowed_tools: None,
            resume_session: None,
            hooks: crate::hooks::HooksConfig::default(),
            // 50 tool-use rounds is enough for everything short of
            // teammate-orchestrated multi-agent flows, and surfaces
            // runaway loops earlier than the old 200.
            max_iterations: 50,
            mcp_servers: Vec::new(),
            kms_active: Vec::new(),
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
    /// GUI window width (logical pixels). Default: 1100.
    #[serde(rename = "windowWidth")]
    pub window_width: Option<f64>,
    /// GUI window height (logical pixels). Default: 700.
    #[serde(rename = "windowHeight")]
    pub window_height: Option<f64>,
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
            thinking_budget: None,
            search_engine: None,
            allowed_tools: None,
            disallowed_tools: None,
            window_width: None,
            window_height: None,
            team_enabled: Some(false),
            show_raw_response: None,
            kms: None,
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
    }

    pub fn set_model(&mut self, model: &str) {
        self.model = Some(model.to_string());
    }

    /// Persist the permission mode (`"auto"` / `"ask"`) to project
    /// settings. Overwrites any existing `{allow, deny}` block — GUI
    /// and REPL only toggle the simple mode, so the complex form
    /// rewrites whenever the user flips `/permissions`.
    pub fn set_permissions_mode(&mut self, mode: &str) {
        self.permissions = Some(PermissionsConfig::Mode(mode.to_string()));
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
                    return Some(crate::mcp::McpServerConfig {
                        name: name.clone(),
                        transport,
                        command: String::new(),
                        args: Vec::new(),
                        env: std::collections::HashMap::new(),
                        url,
                        headers,
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
                Some(crate::mcp::McpServerConfig {
                    name: name.clone(),
                    transport,
                    command,
                    args,
                    env,
                    url: String::new(),
                    headers: std::collections::HashMap::new(),
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

/// Remove a server from the on-disk `mcp.json`. Returns whether anything
/// was actually removed (false when the file or the key didn't exist).
pub fn remove_mcp_server(name: &str, user: bool) -> Result<(bool, PathBuf)> {
    let path = mcp_config_path(user)?;
    if !path.exists() {
        return Ok((false, path));
    }
    let contents = std::fs::read_to_string(&path)?;
    let mut root: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|e| Error::Config(format!("parse mcp.json: {e}")))?;
    let removed = root
        .get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
        .and_then(|m| m.remove(name))
        .is_some();
    if removed {
        let pretty = serde_json::to_string_pretty(&root)
            .map_err(|e| Error::Config(format!("serialize mcp.json: {e}")))?;
        std::fs::write(&path, pretty)?;
    }
    Ok((removed, path))
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
        c.model = "gemini-2.0-flash".into();
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
}
