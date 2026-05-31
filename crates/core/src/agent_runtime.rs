//! Workspace-scoped agent runtime builder.
//!
//! Shared bootstrap for surfaces that need a fully-configured `Agent`
//! parameterized by an explicit workspace directory: the HTTP
//! `/agent/run` endpoint (see `api_v1::agent`), and — eventually —
//! `repl::run_print_mode` / `repl::run_repl` once those are refactored
//! to call through this module instead of inlining the bootstrap.
//!
//! "Workspace-scoped" means skill discovery, MCP config resolution,
//! and policy load all read from `<workspace_dir>/.claude/`,
//! `<workspace_dir>/.thclaws/`, and friends — not from the process
//! CWD. The daemon process can serve multiple concurrent agent runs
//! pointed at different workspace dirs without state crossing.
//!
//! See `dev-plan/25-thclaws-as-agent.md` for the architectural
//! rationale (treating thClaws as an agent peer to claude-code, not
//! an LLM endpoint).

use crate::agent::Agent;
use crate::config::AppConfig;
use crate::mcp::McpClient;
use crate::providers::Provider;
use crate::skills::SkillStore;
use crate::tools::ToolRegistry;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Fully-constructed agent + the keep-alive handles its tools depend
/// on. Callers run `runtime.agent.run_turn(prompt)` and must keep the
/// `AgentRuntime` value alive for the duration of the turn — dropping
/// it shuts down spawned MCP server processes mid-call.
pub struct AgentRuntime {
    pub agent: Agent,
    /// Final system prompt the agent was constructed with. Exposed for
    /// debugging / tests; the agent already owns its own copy.
    pub system_prompt: String,
    /// MCP server clients spawned during construction. The Drop on the
    /// last `Arc<McpClient>` reaps the subprocess, so keep these
    /// around until the turn completes.
    pub mcp_clients: Vec<Arc<McpClient>>,
    /// `(server_name, [tool_names])` summary for diagnostics.
    pub mcp_summary: Vec<(String, Vec<String>)>,
    /// Names of the skills discovered for this workspace. Empty when
    /// no skills are available — callers can short-circuit the catalog
    /// rendering on the front-end accordingly.
    pub skill_names: Vec<String>,
}

/// Build an `Agent` configured for a specific workspace directory.
///
/// Reads from:
///   - `<workspace_dir>/.thclaws/skills/`  (project skills, highest priority)
///   - `<workspace_dir>/.claude/skills/`   (Claude Code compat skills)
///   - `~/.config/thclaws/skills/`         (user skills)
///   - `~/.claude/skills/`                 (user Claude Code skills)
///
/// MCP servers come from `config.mcp_servers` — for now the same list
/// the REPL uses. Future work (dev-plan/25 Phase B) will let callers
/// override per-request via materialized `.thclaws/mcp.json`.
///
/// The optional `extra_system` is appended to the default system
/// prompt so callers can pass a per-request system message without
/// replacing the thClaws default scaffolding.
pub async fn build_runtime_for_workspace(
    config: &AppConfig,
    workspace_dir: &Path,
    extra_system: Option<&str>,
) -> crate::error::Result<AgentRuntime> {
    let provider = crate::repl::build_provider(config)?;
    build_runtime_with_provider(config, workspace_dir, extra_system, provider).await
}

/// As [`build_runtime_for_workspace`], but with an externally-supplied
/// provider. Used by tests that inject a mock provider and by code
/// paths that want to share a provider across multiple turns.
pub async fn build_runtime_with_provider(
    config: &AppConfig,
    workspace_dir: &Path,
    extra_system: Option<&str>,
    provider: Arc<dyn Provider>,
) -> crate::error::Result<AgentRuntime> {
    let mut tool_registry = ToolRegistry::with_builtins();
    // Always-on KMS + Memory + SessionRename, matching the REPL/print
    // mode toolset. Skill / MCP tools register below.
    tool_registry.register(Arc::new(crate::tools::KmsReadTool));
    tool_registry.register(Arc::new(crate::tools::KmsSearchTool));
    tool_registry.register(Arc::new(crate::tools::KmsWriteTool));
    tool_registry.register(Arc::new(crate::tools::KmsAppendTool));
    tool_registry.register(Arc::new(crate::tools::KmsDeleteTool));
    tool_registry.register(Arc::new(crate::tools::KmsCreateTool));
    tool_registry.register(Arc::new(crate::tools::MemoryReadTool));
    tool_registry.register(Arc::new(crate::tools::MemoryWriteTool));
    tool_registry.register(Arc::new(crate::tools::MemoryAppendTool));
    tool_registry.register(Arc::new(crate::tools::SessionRenameTool));

    // Tool-parity audit fix: respect `searchEngine` override (REPL
    // had this; agent_runtime fell back to "auto"). HashMap::insert
    // in `register` overwrites the default `WebSearchTool` from
    // `with_builtins` by name.
    if config.search_engine != "auto" {
        tool_registry.register(Arc::new(crate::tools::WebSearchTool::new(
            &config.search_engine,
        )));
    }

    // Tool-parity audit fix: Task tools (TodoWrite + task queue) —
    // REPL + GUI register these; agent_runtime omitted them. HTTP
    // clients now get the same scaffolding the default system prompt
    // tells the model to use. TodoWrite is safe across surfaces (it
    // writes per-project `.thclaws/todos.md`, which dev-plan/35's
    // per-user state plumbing scopes correctly for multi-tenant
    // serve). Returned task_store handle is unused here (no
    // subagent factory wiring on the HTTP path) so dropped.
    let _task_store = crate::tools::tasks::register_task_tools(&mut tool_registry);

    // WorkflowRun: model-callable wrapper around `/workflow run`.
    // agent_runtime doesn't register SubAgentTool (no subagent
    // factory wiring on the HTTP path), so scripts that call
    // `thclaws.subagent(...)` will error at runtime — same as print
    // mode. The workflow primitive itself still works for non-
    // subagent scripts, and registering keeps the tool surface
    // consistent across all 4 entry points.
    tool_registry.register(Arc::new(crate::tools::WorkflowRunTool::new(
        provider.clone(),
        config.model.clone(),
        None,
    )));

    // Team tools deliberately NOT registered for the HTTP API
    // surface — multi-tenant agent_runtime serves untrusted callers,
    // and TeamCreate / SpawnTeammate spawn subprocesses outside the
    // request boundary. If a deployment wants team-on for HTTP, add
    // a config knob; for now stay safe-by-default.

    // dev-plan/35 followup: discover skills BEFORE the prompt build
    // so the unified `build_full_system_prompt` sees the live catalog.
    // The daemon's cwd may differ from the workspace_dir the caller
    // pointed at, so use `discover_in(workspace_dir, …)` instead of
    // the cwd-relative `discover()`.
    let plugin_skill_dirs = crate::plugins::plugin_skill_dirs();
    let skill_store = SkillStore::discover_in(workspace_dir, &plugin_skill_dirs);
    let mut skill_names: Vec<String> = skill_store.skills.keys().cloned().collect();
    skill_names.sort();

    if !skill_store.skills.is_empty() {
        let skill_tool = crate::skills::SkillTool::new(skill_store.clone());
        let store_handle = skill_tool.store_handle();
        tool_registry.register(Arc::new(skill_tool));
        tool_registry.register(Arc::new(crate::skills::SkillListTool::new_from_handle(
            store_handle.clone(),
        )));
        tool_registry.register(Arc::new(crate::skills::SkillSearchTool::new_from_handle(
            store_handle,
        )));
    }

    // MCP servers: config-level + plugin contributions, merged with
    // config winning on name clash. Spawn BEFORE the prompt build so
    // each server's `InitializeResult.instructions` lands in the
    // `# MCP server instructions` section on the first request.
    let mut merged_mcp = config.mcp_servers.clone();
    for p_mcp in crate::plugins::plugin_mcp_servers() {
        if !merged_mcp.iter().any(|s| s.name == p_mcp.name) {
            merged_mcp.push(p_mcp);
        }
    }
    let (mcp_clients, mcp_summary) = load_mcp_servers_silent(&merged_mcp, &mut tool_registry).await;
    let mcp_instructions = crate::mcp::collect_mcp_instructions(&mcp_clients);

    // System prompt via the unified builder — same shape as CLI / GUI /
    // print. SurfaceHints::Headless skips the REPL slash-command priming.
    let mut system = crate::prompts::build_full_system_prompt(
        config,
        workspace_dir,
        if skill_store.skills.is_empty() {
            None
        } else {
            Some(&skill_store)
        },
        &mcp_instructions,
        crate::prompts::SurfaceHints::Headless,
    );

    // Per-request extra system content (the OpenAI-style `system`
    // message a caller can pass). Appended so the default scaffolding
    // + skill catalog still apply.
    if let Some(extra) = extra_system {
        if !extra.trim().is_empty() {
            system.push_str("\n\n# Client-provided context\n");
            system.push_str(extra.trim());
        }
    }

    let agent = Agent::new(
        provider,
        tool_registry,
        config.model.clone(),
        system.clone(),
    )
    .with_max_iterations(config.max_iterations)
    .with_max_tokens(config.max_tokens);

    Ok(AgentRuntime {
        agent,
        system_prompt: system,
        mcp_clients,
        mcp_summary,
        skill_names,
    })
}

/// Server-friendly MCP loader: same shape as `repl::load_mcp_servers`
/// but routes status to stderr (or nowhere) instead of stdout. The
/// REPL's stdout-heavy version is unsafe in HTTP handlers where stdout
/// is the response transport.
async fn load_mcp_servers_silent(
    servers: &[crate::mcp::McpServerConfig],
    registry: &mut ToolRegistry,
) -> (Vec<Arc<McpClient>>, Vec<(String, Vec<String>)>) {
    let mut clients: Vec<Arc<McpClient>> = Vec::new();
    let mut summary: Vec<(String, Vec<String>)> = Vec::new();
    for cfg in servers {
        match McpClient::spawn(cfg.clone()).await {
            Ok(client) => match client.list_tools().await {
                Ok(tools) => {
                    let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
                    for info in tools {
                        let tool = crate::mcp::McpTool::new(client.clone(), info);
                        registry.register(Arc::new(tool));
                    }
                    summary.push((cfg.name.clone(), names));
                    clients.push(client);
                }
                Err(e) => {
                    eprintln!("[agent_runtime] mcp '{}' list_tools failed: {e}", cfg.name);
                }
            },
            Err(e) => {
                eprintln!("[agent_runtime] mcp '{}' spawn failed: {e}", cfg.name);
            }
        }
    }
    (clients, summary)
}

/// CLI flag value resolved at server boot: an absolute path that all
/// `workspace_dir` request fields must live underneath. `None` means
/// validation is disabled (development default — production runs of
/// the agent endpoint set this explicitly).
///
/// Stored in a process-global cell because Phase A only needs the
/// daemon-wide value; per-listener customization can come later if
/// embedders need it.
pub fn allowed_workspace_root() -> Option<PathBuf> {
    std::env::var("THCLAWS_AGENT_WORKSPACE_ROOT")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Validate a request's `workspace_dir`. Returns the canonicalized
/// path on success.
///
/// Rules:
///   - Must be absolute.
///   - Must exist and be a directory.
///   - If [`allowed_workspace_root`] is set, the canonical path must
///     live inside it. Prevents a malicious caller from pointing the
///     daemon at sensitive system paths.
pub fn validate_workspace_dir(raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(format!("workspace_dir must be absolute, got: {raw}"));
    }
    let meta =
        std::fs::metadata(&path).map_err(|e| format!("workspace_dir not accessible: {e}"))?;
    if !meta.is_dir() {
        return Err(format!("workspace_dir must be a directory: {raw}"));
    }
    let canonical = std::fs::canonicalize(&path)
        .map_err(|e| format!("workspace_dir canonicalize failed: {e}"))?;
    if let Some(root) = allowed_workspace_root() {
        let root_canonical = std::fs::canonicalize(&root).map_err(|e| {
            format!(
                "THCLAWS_AGENT_WORKSPACE_ROOT={} canonicalize failed: {e}",
                root.display()
            )
        })?;
        if !canonical.starts_with(&root_canonical) {
            return Err(format!(
                "workspace_dir {} is outside allowed root {}",
                canonical.display(),
                root_canonical.display()
            ));
        }
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn validate_rejects_relative_path() {
        let err = validate_workspace_dir("relative/path").unwrap_err();
        assert!(err.contains("must be absolute"), "got: {err}");
    }

    #[test]
    fn validate_rejects_nonexistent_path() {
        let err = validate_workspace_dir("/this/does/not/exist/probably").unwrap_err();
        assert!(err.contains("not accessible"), "got: {err}");
    }

    /// Env-mutating cases are bundled into a single sequential test so
    /// the THCLAWS_AGENT_WORKSPACE_ROOT global doesn't race against
    /// other parallel test threads.
    #[test]
    fn validate_root_scoped_cases() {
        let prior = std::env::var("THCLAWS_AGENT_WORKSPACE_ROOT").ok();

        // (a) no root set → any absolute existing dir accepted.
        std::env::remove_var("THCLAWS_AGENT_WORKSPACE_ROOT");
        let dir = tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        let resolved = validate_workspace_dir(path.to_str().unwrap()).unwrap();
        assert_eq!(resolved, path);

        // (b) root set, path inside → accepted.
        let root = tempdir().unwrap();
        let nested = root.path().join("agent-1");
        fs::create_dir(&nested).unwrap();
        std::env::set_var(
            "THCLAWS_AGENT_WORKSPACE_ROOT",
            root.path().to_str().unwrap(),
        );
        let resolved = validate_workspace_dir(nested.to_str().unwrap()).unwrap();
        assert_eq!(resolved, nested.canonicalize().unwrap());

        // (c) root set, path outside → rejected.
        let outside = tempdir().unwrap();
        let err = validate_workspace_dir(outside.path().canonicalize().unwrap().to_str().unwrap())
            .unwrap_err();
        assert!(err.contains("outside allowed root"), "got: {err}");

        match prior {
            Some(v) => std::env::set_var("THCLAWS_AGENT_WORKSPACE_ROOT", v),
            None => std::env::remove_var("THCLAWS_AGENT_WORKSPACE_ROOT"),
        }
    }
}
