//! Sub-agent tool — spawn nested agents with depth tracking and
//! named agent definitions.
//!
//! Supports multi-level recursion up to `max_depth` (default 3).
//! Child agents include their own `Task` tool at `depth + 1`, so
//! they can delegate further. At max depth, the tool refuses.
//!
//! Named agents: if `agent` field is provided in the input, loads
//! the definition from `~/.config/thclaws/agents.json` and uses
//! its instructions, model override, and tool subset.

use crate::agent::{collect_agent_turn_with_cancel, Agent};
use crate::agent_defs::{AgentDef, AgentDefsConfig};
use crate::cancel::CancelToken;
use crate::error::{Error, Result};
use crate::permissions::{ApprovalSink, PermissionMode};
use crate::providers::Provider;
use crate::tools::{req_str, Tool, ToolRegistry};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};

pub const TOOL_NAME: &str = "Task";
pub const DEFAULT_MAX_DEPTH: usize = 3;

/// File-write tools that 47.2 `writePaths` scoping wraps. Each takes a
/// target path under one of the keys in [`WRITE_PATH_KEYS`]; the wrapper
/// refuses a target outside the agent def's globs before delegating.
const WRITE_TOOL_NAMES: &[&str] = &[
    "Write",
    "Edit",
    "DocxCreate",
    "DocxEdit",
    "PptxCreate",
    "PptxEdit",
    "XlsxCreate",
    "XlsxEdit",
    "PdfCreate",
    "EpubCreate",
    "NotebookEdit",
];
const WRITE_PATH_KEYS: &[&str] = &["path", "file_path", "notebook_path", "out", "output_path"];

/// Wraps a file-write tool with a glob allow-list (`AgentDef::write_paths`).
/// Delegates everything to the inner tool except that `call` first checks
/// the target path against the globs — a write outside them is refused,
/// mechanically, so a subagent can't scribble beyond its lane. Composes
/// with the existing workspace sandbox (this is a secondary, narrower scope).
struct PathScopedWriteTool {
    inner: Arc<dyn Tool>,
    globs: Arc<globset::GlobSet>,
    patterns: Vec<String>,
}

/// Where a write will actually land. Delegates to the sandbox's resolver so
/// the glob check and the sandbox check cannot disagree about the
/// destination — including for a dangling symlink, which both used to treat
/// as an ordinary non-existent file (issue #219).
fn resolve_destination(abs: &std::path::Path) -> std::path::PathBuf {
    crate::sandbox::resolve_landing(abs)
}

impl PathScopedWriteTool {
    fn check(&self, input: &Value) -> Result<()> {
        let Some(raw) = WRITE_PATH_KEYS
            .iter()
            .find_map(|k| input.get(*k).and_then(|v| v.as_str()))
        else {
            // No recognised path arg — let the inner tool validate its
            // own input rather than guessing.
            return Ok(());
        };
        let cwd = crate::workdir::current_workdir();
        let p = std::path::Path::new(raw);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        // Match where the write LANDS, not how it was spelled. Globbing
        // the raw path let `output/../escape.txt` satisfy `output/**`
        // while the file appeared at the workspace root: the glob saw
        // the `..`, the filesystem resolved it away (public issue #204).
        let resolved = resolve_destination(&abs);
        // cwd is canonicalised to match — otherwise a symlinked working
        // directory (macOS `/tmp` → `/private/tmp`) makes every strip
        // fail and every relative glob stop matching.
        let base = cwd.canonicalize().unwrap_or(cwd);
        let rel = resolved
            .strip_prefix(&base)
            .map(|r| r.to_path_buf())
            .unwrap_or(resolved);
        let cand = rel.to_string_lossy().replace('\\', "/");
        if self.globs.is_match(&cand) {
            Ok(())
        } else {
            Err(Error::Tool(format!(
                "write to '{raw}' denied — this subagent's writePaths confine it to {:?}. \
                 Write only inside those globs (the agent def scopes file writes).",
                self.patterns
            )))
        }
    }
}

#[async_trait]
impl Tool for PathScopedWriteTool {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn description(&self) -> &'static str {
        self.inner.description()
    }
    fn input_schema(&self) -> Value {
        self.inner.input_schema()
    }
    fn requires_approval(&self, input: &Value) -> bool {
        self.inner.requires_approval(input)
    }
    fn parallelizable(&self) -> bool {
        self.inner.parallelizable()
    }
    fn requires_env(&self) -> &'static [&'static str] {
        self.inner.requires_env()
    }
    fn requires_gate(&self) -> Option<&'static str> {
        self.inner.requires_gate()
    }
    async fn call(&self, input: Value) -> Result<String> {
        self.check(&input)?;
        self.inner.call(input).await
    }
    async fn call_multimodal(&self, input: Value) -> Result<crate::types::ToolResultContent> {
        self.check(&input)?;
        self.inner.call_multimodal(input).await
    }
}

/// Build a [`globset::GlobSet`] from `writePaths`. Invalid globs are
/// skipped (validate surfaces them); `None` if nothing usable compiled.
fn build_write_globset(patterns: &[String]) -> Option<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    let mut any = false;
    for p in patterns {
        if let Ok(g) = globset::Glob::new(p) {
            builder.add(g);
            any = true;
        }
    }
    if !any {
        return None;
    }
    builder.build().ok()
}

/// Mutable state shared between a `ProductionAgentFactory` and its
/// owning worker (CLI `run_repl` locals, GUI `WorkerState`).
///
/// Pre-fix the factory captured `system: String` + `base_tools:
/// ToolRegistry` at construction and never refreshed them — mid-
/// session mutators (`/mcp add`, `/skill install`, `/kms use`,
/// `/reload-prompt`, AGENTS.md / memory edits via `/reload-prompt`)
/// reached the parent agent but not the factory. Subagents spawned
/// after any of those saw the startup-time system prompt with no
/// new MCP tools.
///
/// Now the worker holds an `Arc<RwLock<FactorySnapshot>>` and the
/// factory holds a clone of that same `Arc`. Worker writes through
/// `repl::refresh_repl_system_prompt` (CLI) /
/// `WorkerState::sync_factory_snapshot` (GUI + HTTP serve); factory
/// reads on every `build()`. Child factories inherit the same `Arc`
/// (via `snapshot.clone()`) so nested subagents also pick up live
/// state.
///
/// Cheap: cloning a `ToolRegistry` is just cloning a `HashMap<String,
/// Arc<dyn Tool>>` — tool objects themselves are Arc'd, only the
/// map shape is copied.
pub struct FactorySnapshot {
    pub system: String,
    pub tools: ToolRegistry,
    /// Live session model + provider. In the snapshot (not factory
    /// fields) so a mid-session model switch (GUI ReloadConfig, CLI
    /// `/model`) reaches subagents: an unpinned `agent_def` must
    /// inherit the model the user is CURRENTLY on, not the one active
    /// at worker init. Pre-fix a deepseek→grok switch kept spawning
    /// deepseek workers for the rest of the session.
    pub model: String,
    pub provider: Arc<dyn Provider>,
}

/// How to construct a child agent. Implementations produce a brand-new
/// `Agent` with the appropriate configuration.
#[async_trait]
pub trait AgentFactory: Send + Sync {
    /// Build a child agent. `agent_def` is `Some` if the Task input
    /// specified a named agent; `None` for the default.
    async fn build(
        &self,
        prompt: &str,
        agent_def: Option<&AgentDef>,
        child_depth: usize,
    ) -> Result<Agent>;

    /// The factory's base model id — used to attribute a subagent's
    /// token usage when its `agent_def` doesn't pin its own model.
    /// Default `"unknown"` keeps test stubs simple.
    fn base_model(&self) -> String {
        "unknown".into()
    }
}

/// M6.33: production agent factory shared by CLI (`run_repl`) and GUI
/// (`build_state`). Pre-fix the CLI had its own `ReplAgentFactory` and
/// the GUI had no factory at all (Task tool unregistered — SUB1).
/// Consolidated here so both surfaces get identical subagent behavior.
///
/// Fields capture the parent's runtime state for propagation to child
/// agents:
/// - `snapshot.provider` / `snapshot.model` — wire layer for the
///   child's LLM calls (live, follows mid-session model switches)
/// - `base_tools` — tool registry the child inherits (filtered by
///   agent_def.tools allow-list + agent_def.disallowed_tools deny-list
///   inside `build`)
/// - `system` — parent's full system prompt (CLAUDE.md + memory + KMS +
///   plan + todos), copied to the child + agent_def addendum + the
///   embedded `subagent.md` "you are a sub-agent" wording
/// - `max_iterations` — fallback when agent_def doesn't specify
/// - `max_depth` — recursion ceiling; child gets a Task tool only when
///   child_depth < max_depth
/// - `agent_defs` — registry of named agents (for nested Task calls)
/// - `approver` + `permission_mode` — M6.20 BUG H1: parent's gate
///   propagates so subagents can't silently bypass Ask mode
/// - `cancel` — M6.33 SUB4: parent's cancel token propagates so
///   ctrl-C reaches a runaway subagent. CLI passes `None` (no cancel
///   plumbing yet); GUI passes the worker's CancelToken.
pub struct ProductionAgentFactory {
    /// Live view of the parent agent's system prompt + tool registry
    /// + model/provider. Shared by Arc with the worker — see
    /// [`FactorySnapshot`] docs.
    pub snapshot: Arc<RwLock<FactorySnapshot>>,
    pub max_iterations: usize,
    pub max_depth: usize,
    /// Per-request output token budget propagated from `AppConfig::max_tokens`.
    /// Subagents inherit the parent's value so a project's `settings.json`
    /// `maxTokens` override applies uniformly. Issue #72: pre-fix subagents
    /// hit the hardcoded `Agent::new` default of 8192 even when the parent
    /// was correctly configured.
    pub max_tokens: u32,
    pub agent_defs: AgentDefsConfig,
    pub approver: Arc<dyn ApprovalSink>,
    pub permission_mode: PermissionMode,
    pub cancel: Option<CancelToken>,
    /// M6.35 HOOK1: lifecycle hooks propagate parent → subagent so a
    /// pre/post_tool_use hook fires for tool calls inside a Task spawn,
    /// not just at the top-level agent. Audit hooks would otherwise miss
    /// every subagent action — silent gap.
    pub hooks: Option<Arc<crate::hooks::HooksConfig>>,
}

/// Pick the model a subagent runs under. An unpinned def inherits the session
/// model. A pinned `model:` is HONORED whenever its provider is *usable* — the
/// same provider as the session, OR it has its own BYOK key, OR the gateway is
/// configured to route it — because `create_subagent` builds a matching
/// provider for a cross-provider pin instead of blindly reusing the parent's
/// (which would send e.g. `claude-haiku-4-5` to a deepseek endpoint → "model
/// not found"). It falls back to the session model only when the pin can't be
/// served at all, so an explicit choice degrades to "runs on the current
/// model" rather than a hard 404. Keep this decision in lockstep with the
/// provider build in `create_subagent` and the usage attribution below.
fn subagent_model<'a>(
    pinned: Option<&'a str>,
    session_model: &'a str,
    config: &crate::config::AppConfig,
) -> &'a str {
    let Some(p) = pinned else {
        return session_model;
    };
    let Some(pk) = crate::providers::ProviderKind::detect(p) else {
        // Unknown provider for this id — nothing can route it; keep session.
        return session_model;
    };
    let same_provider = crate::providers::ProviderKind::detect(session_model) == Some(pk);
    if same_provider
        || pk.has_key_available()
        || crate::providers::thclaws_gateway::for_kind(config, pk).is_some()
    {
        p
    } else {
        session_model
    }
}

/// Persist a completed sub-agent's transcript to its own child session
/// file, for audit + debugging. Best-effort — a logging failure never
/// affects the run. Shared by Task (`SubAgentTool`) and isolated
/// job-skills so every delegated run is recorded on disk, not merely
/// surfaced as a compact result in the parent conversation: the
/// intermediate steps a sub-agent keeps OUT of the parent's LLM context
/// still land here under `.thclaws/state/sessions/sub-<id>.jsonl`, so the
/// "how" (which files were read, what a script did) stays auditable.
pub(crate) fn persist_subagent_session(agent: &Agent, model: &str, role: &str) {
    let cwd = crate::workdir::current_workdir();
    let dir = cwd.join(".thclaws").join("state").join("sessions");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut sess = crate::session::Session::new(model, cwd.to_string_lossy().to_string());
    sess.title = Some(format!("subagent:{role}"));
    sess.sync(agent.history_snapshot());
    let path = dir.join(format!("sub-{}.jsonl", sess.id));
    let _ = sess.append_to(&path);
}

#[async_trait]
impl AgentFactory for ProductionAgentFactory {
    fn base_model(&self) -> String {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model
            .clone()
    }

    async fn build(
        &self,
        _prompt: &str,
        agent_def: Option<&AgentDef>,
        child_depth: usize,
    ) -> Result<Agent> {
        // Snapshot the live parent state ONCE — system + tools +
        // model/provider are all needed and we want them to come from
        // the same instant (so a refresh between reads can't tear,
        // e.g. a model id from provider A paired with provider B).
        let (parent_system, base_tools, session_model, provider) = {
            // L2: recover from a poisoned lock instead of panicking — the
            // only writers do trivial field assignments, so the inner
            // data is always consistent even if a writer panicked.
            let snap = self.snapshot.read().unwrap_or_else(|e| e.into_inner());
            (
                snap.system.clone(),
                snap.tools.clone(),
                snap.model.clone(),
                snap.provider.clone(),
            )
        };
        // Resolve the subagent's model, then a provider that can serve it.
        // Unpinned (and same-provider-pinned) subagents reuse the parent's
        // provider. A cross-provider pin we choose to honor gets a
        // freshly-built provider so the model id routes to its own endpoint;
        // if that build unexpectedly fails we degrade to the session model on
        // the parent provider rather than misrouting the pinned id.
        let (model, provider): (String, Arc<dyn Provider>) =
            match agent_def.and_then(|d| d.model.as_deref()) {
                None => (session_model.clone(), provider),
                Some(pin) => {
                    let sub_config = crate::config::AppConfig::load().unwrap_or_default();
                    let chosen = subagent_model(Some(pin), &session_model, &sub_config).to_string();
                    let session_kind = crate::providers::ProviderKind::detect(&session_model);
                    if crate::providers::ProviderKind::detect(&chosen) == session_kind {
                        // same provider, or the pin fell back to the session model
                        (chosen, provider)
                    } else {
                        let mut c = sub_config;
                        c.model = chosen.clone();
                        match crate::repl::build_provider(&c) {
                            Ok(p) => (chosen, p),
                            Err(_) => (session_model.clone(), provider),
                        }
                    }
                }
            };

        // System prompt: parent's full prompt + (optional) agent
        // instructions + (when nested) the subagent-mode addendum.
        let mut system = agent_def
            .map(|d| {
                if d.instructions.is_empty() {
                    parent_system.clone()
                } else {
                    format!(
                        "{}\n\n# Agent instructions\n{}",
                        parent_system, d.instructions
                    )
                }
            })
            .unwrap_or_else(|| parent_system.clone());
        if child_depth > 0 {
            system.push_str(&crate::prompts::load(
                "subagent",
                crate::prompts::defaults::SUBAGENT,
            ));
        }
        let max_iter = agent_def
            .map(|d| d.max_iterations)
            .unwrap_or(self.max_iterations);

        // Tool registry: agent_def.tools allow-list (when non-empty)
        // intersects base_tools, then agent_def.disallowed_tools
        // deny-list removes anything in it. M6.33 SUB2: pre-fix
        // disallowed_tools was parsed but never applied — agent
        // definitions claiming `disallowed_tools: Bash` got Bash anyway.
        let mut tools = if let Some(def) = agent_def {
            if def.tools.is_empty() {
                base_tools.clone()
            } else {
                let mut filtered = ToolRegistry::new();
                for name in &def.tools {
                    if let Some(tool) = base_tools.get(name) {
                        filtered.register(tool);
                    }
                }
                filtered
            }
        } else {
            base_tools.clone()
        };
        if let Some(def) = agent_def {
            for name in &def.disallowed_tools {
                tools.remove(name);
            }
        }

        // A subagent that explicitly allow-lists a gated tool (e.g.
        // `FetchImages` behind the `content-extractor` gate) opens that gate
        // for itself — the declarative parallel to a skill's `tool-gate:`, so
        // a subagent is as capable as a skill at surfacing a gated tool group.
        // Only tools NAMED in the allow-list count; an inherit-all def (empty
        // `tools`) does not auto-open every gate. Process-global + session-
        // sticky (same model as skills).
        if let Some(def) = agent_def {
            open_gates_for_allowlist(def, &base_tools);
        }

        // Per-subagent MCP scoping (`AgentDef::mcp`). MCP tools are named
        // `<server>__<tool>` (see `mcp::MCP_NAME_SEPARATOR`). When the def
        // lists servers, drop every MCP tool whose server segment isn't
        // allow-listed — the subagent literally cannot call other servers.
        // Opt-in: empty list = inherit all MCP tools.
        if let Some(def) = agent_def {
            if !def.mcp.is_empty() {
                let allowed: std::collections::HashSet<String> = def
                    .mcp
                    .iter()
                    .map(|s| crate::mcp::sanitize_tool_name_segment(s))
                    .collect();
                let sep = crate::mcp::MCP_NAME_SEPARATOR;
                let drop: Vec<String> = tools
                    .names()
                    .iter()
                    .filter(|n| {
                        n.split_once(sep)
                            .map(|(server, _)| !allowed.contains(server))
                            .unwrap_or(false)
                    })
                    .map(|s| s.to_string())
                    .collect();
                for n in drop {
                    tools.remove(&n);
                }
            }
        }

        // Per-subagent skill scoping (`AgentDef::skills`). Skills are
        // reached only through the Skill/SkillList/SkillSearch tools (a
        // shared store). Recover the store handle from the inherited
        // `Skill` tool via `as_any`, then replace all three with
        // allow-list-scoped copies sharing that same handle. Opt-in:
        // empty list = inherit the full skill set.
        if let Some(def) = agent_def {
            if !def.skills.is_empty() {
                let handle = tools.get("Skill").and_then(|t| {
                    t.as_any()
                        .and_then(|a| a.downcast_ref::<crate::skills::SkillTool>())
                        .map(|s| s.store_handle())
                });
                if let Some(handle) = handle {
                    let allowed = Arc::new(
                        def.skills
                            .iter()
                            .cloned()
                            .collect::<std::collections::HashSet<String>>(),
                    );
                    tools.register(Arc::new(
                        crate::skills::SkillTool::new_from_handle(handle.clone())
                            .with_allowed(allowed.clone()),
                    ));
                    if tools.get("SkillList").is_some() {
                        tools.register(Arc::new(
                            crate::skills::SkillListTool::new_from_handle(handle.clone())
                                .with_allowed(allowed.clone()),
                        ));
                    }
                    if tools.get("SkillSearch").is_some() {
                        tools.register(Arc::new(
                            crate::skills::SkillSearchTool::new_from_handle(handle)
                                .with_allowed(allowed),
                        ));
                    }
                }
            }
        }

        // 47.2: per-subagent write-path scoping (`AgentDef::write_paths`).
        // When the def declares globs, wrap each present file-write tool so
        // a target outside the globs is refused before the inner tool runs —
        // mechanical write-scoping, not a prompt promise. Opt-in: empty list
        // = inherit (write wherever the parent allows). Scopes file-write
        // tools, not Bash.
        if let Some(def) = agent_def {
            if !def.write_paths.is_empty() {
                if let Some(set) = build_write_globset(&def.write_paths) {
                    let set = Arc::new(set);
                    for tname in WRITE_TOOL_NAMES {
                        if let Some(inner) = tools.get(tname) {
                            tools.register(Arc::new(PathScopedWriteTool {
                                inner,
                                globs: set.clone(),
                                patterns: def.write_paths.clone(),
                            }));
                        }
                    }
                }
            }
        }

        // H1/M1: `base_tools` is cloned from the shared snapshot, which —
        // after ANY mid-session refresh (`/mcp add`, `/kms use`,
        // `/reload-prompt`, or the GUI's `McpReady` fan-out) — re-clones
        // the live parent registry INCLUDING the root depth-0 `Task`
        // tool. Strip it unconditionally here, then re-register a
        // correctly-depthed one below only when allowed. Without the
        // strip, a leaf agent at `child_depth == max_depth` (where the
        // registration block is skipped) would inherit the root Task and
        // could spawn again from depth 0 — resetting the recursion
        // counter and defeating `max_depth` entirely (runaway nesting).
        // Stripping here also makes `disallowed_tools: ["Task"]` and an
        // allow-list that omits `Task` actually disable subagent spawning,
        // which the pre-strip filter could not do (Task was always added
        // afterwards).
        tools.remove(TOOL_NAME);

        // Whether this child may spawn further subagents. Honored by both
        // the allow-list (when non-empty it must list `Task`) and the
        // deny-list. Default agents (no `agent_def`) may always recurse.
        let task_allowed = match agent_def {
            Some(def) => {
                let allowed_by_list =
                    def.tools.is_empty() || def.tools.iter().any(|t| t == TOOL_NAME);
                let not_denied = !def.disallowed_tools.iter().any(|t| t == TOOL_NAME);
                allowed_by_list && not_denied
            }
            None => true,
        };

        // Add a Task tool at the next depth (multi-level recursion).
        // child_depth < max_depth AND the agent def permits Task →
        // register; otherwise the subagent has no Task tool and the chain
        // stops (either a leaf at the depth ceiling or an agent scoped to
        // not recurse).
        if task_allowed && child_depth < self.max_depth {
            let child_factory = Arc::new(ProductionAgentFactory {
                // Share the SAME snapshot Arc so nested subagents
                // also see live state updates. Cloning the Arc is
                // O(1) — just a refcount bump.
                snapshot: self.snapshot.clone(),
                max_iterations: self.max_iterations,
                max_depth: self.max_depth,
                max_tokens: self.max_tokens,
                agent_defs: self.agent_defs.clone(),
                approver: self.approver.clone(),
                permission_mode: self.permission_mode,
                cancel: self.cancel.clone(),
                hooks: self.hooks.clone(),
            });
            let mut child_tool = SubAgentTool::new(child_factory)
                .with_depth(child_depth)
                .with_max_depth(self.max_depth)
                .with_agent_defs(self.agent_defs.clone());
            if let Some(c) = self.cancel.clone() {
                child_tool = child_tool.with_cancel(c);
            }
            tools.register(Arc::new(child_tool));
        }

        // M6.33 SUB4: thread parent's cancel token into the child agent
        // so retry-backoff sleeps + collect_agent_turn observe ctrl-C.
        let mut agent = Agent::new(provider, tools, model, &system)
            .with_max_iterations(max_iter)
            .with_max_tokens(self.max_tokens)
            .with_approver(self.approver.clone())
            // The live mode, not the one captured at worker start: after
            // auto → `/permissions ask` the child would otherwise keep Auto
            // and run Write/Bash without asking.
            .with_permission_mode(crate::permissions::live_mode().unwrap_or(self.permission_mode));
        if let Some(c) = self.cancel.clone() {
            agent = agent.with_cancel(c);
        }
        // M6.35 HOOK1: subagent inherits parent's hooks so audit hooks
        // see Task-spawned tool calls too.
        if let Some(h) = self.hooks.clone() {
            agent = agent.with_hooks(h);
        }
        Ok(agent)
    }
}

pub struct SubAgentTool {
    factory: Arc<dyn AgentFactory>,
    depth: usize,
    max_depth: usize,
    /// Agent definitions loaded at startup.
    agent_defs: crate::agent_defs::AgentDefsConfig,
    /// M6.33 SUB4: parent's cancel token. Observed by
    /// `collect_agent_turn_with_cancel` so ctrl-C reaches a runaway
    /// subagent. None when no parent cancel is wired (CLI today).
    cancel: Option<CancelToken>,
}

impl SubAgentTool {
    pub fn new(factory: Arc<dyn AgentFactory>) -> Self {
        Self {
            factory,
            depth: 0,
            max_depth: DEFAULT_MAX_DEPTH,
            agent_defs: crate::agent_defs::AgentDefsConfig::load_with_extra(
                &crate::plugins::plugin_agent_dirs(),
            ),
            cancel: None,
        }
    }

    pub fn with_depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    pub fn with_agent_defs(mut self, defs: crate::agent_defs::AgentDefsConfig) -> Self {
        self.agent_defs = defs;
        self
    }

    /// M6.33 SUB4: wire a cancel token. The token is observed inside
    /// `collect_agent_turn_with_cancel` so a parent ctrl-C / `/cancel`
    /// short-circuits the subagent's stream instead of waiting for it
    /// to run to completion.
    pub fn with_cancel(mut self, token: CancelToken) -> Self {
        self.cancel = Some(token);
        self
    }
}

#[async_trait]
impl Tool for SubAgentTool {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    /// Task subagents are read/research delegations with no approval gate —
    /// safe to fan out concurrently. (A subagent that internally mutates is
    /// the model's explicit parallel choice, same as parallel Bash would be.)
    fn parallelizable(&self) -> bool {
        true
    }

    /// 47.1: expose a named agent's declared `output_schema` so the
    /// workflow runtime can apply it when a `thclaws.subagent({agent})`
    /// call omits an explicit per-call `schema`.
    fn subagent_output_schema(&self, agent: &str) -> Option<serde_json::Value> {
        self.agent_defs
            .get(agent)
            .and_then(|d| d.output_schema.clone())
    }

    fn description(&self) -> &'static str {
        "Launch a sub-agent with its own history to handle a bounded subtask. \
         The sub-agent runs independently, may call tools (and spawn further \
         sub-agents up to the recursion limit), and returns its final response \
         as text. Use `agent` to pick a named agent definition from agents.json. \
         When several subtasks are independent, emit all their `Task` calls in \
         a SINGLE message — they run concurrently. Batching them with a tool \
         that isn't read-only forces the whole message to run one call at a time."
    }

    fn input_schema(&self) -> Value {
        let mut agent_names = self.agent_defs.names();
        agent_names.sort();
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Short label for the sub-task (shown in logs)."
                },
                "prompt": {
                    "type": "string",
                    "description": "The full instruction for the sub-agent."
                },
                "agent": {
                    "type": "string",
                    "description": format!(
                        "Optional named agent from agents.json. Available: {}",
                        if agent_names.is_empty() { "none configured".to_string() }
                        else { agent_names.join(", ") }
                    )
                }
            },
            "required": ["prompt"]
        })
    }

    /// Spawning a sub-agent is itself unguarded. The Task call performs
    /// no mutation directly, and the child inherits the parent's
    /// `permission_mode` + approver (see `ProductionAgentFactory::build`),
    /// so any mutating tool the sub-agent calls is gated at the child
    /// level. Returning `false` keeps behavior consistent regardless of
    /// how many Task calls the model emits in a turn: pre-fix the
    /// sequential path honored this `true` and prompted under Ask mode,
    /// but the concurrent fan-out path (≥2 parallelizable tools) never
    /// consulted the approver — so one Task asked and two did not.
    fn requires_approval(&self, _input: &Value) -> bool {
        false
    }

    async fn call(&self, input: Value) -> Result<String> {
        if self.depth >= self.max_depth {
            return Err(Error::Agent(format!(
                "sub-agent recursion limit reached (depth {}/{})",
                self.depth, self.max_depth
            )));
        }

        let prompt = req_str(&input, "prompt")?.to_string();
        let agent_name = input.get("agent").and_then(Value::as_str);

        // Look up named agent definition if specified.
        let agent_def = agent_name.and_then(|name| self.agent_defs.get(name));
        if agent_name.is_some() && agent_def.is_none() {
            let available = self.agent_defs.names().join(", ");
            return Err(Error::Agent(format!(
                "unknown agent '{}'. Available: {}",
                agent_name.unwrap(),
                if available.is_empty() {
                    "none"
                } else {
                    &available
                }
            )));
        }

        let child_depth = self.depth + 1;
        let agent = self.factory.build(&prompt, agent_def, child_depth).await?;
        let stream = agent.run_turn(prompt);
        // M6.33 SUB4: collect_agent_turn_with_cancel observes the
        // parent's cancel token between stream events. Pre-fix the
        // subagent stream ran to completion regardless of ctrl-C.
        let outcome = collect_agent_turn_with_cancel(stream, self.cancel.clone()).await?;

        // Audit: persist the sub-agent's full transcript to its own child
        // session (its intermediate steps never reached the parent context).
        persist_subagent_session(
            &agent,
            &self.factory.base_model(),
            agent_name.unwrap_or("Task"),
        );

        // dev-plan/32 Stage I: push this turn's Usage to the workflow
        // usage sink if one is active on this thread. No-op outside
        // `/workflow run` — model-driven Task calls and tests stay
        // unaffected.
        if let Some(u) = outcome.usage.as_ref() {
            crate::workflow::push_worker_usage(u.clone());

            // A subagent's tokens were otherwise DROPPED outside a
            // workflow — the orchestrator's `cumulative_usage` only
            // counts the parent's own provider calls, not the work a
            // Task spawned. Record the subagent's usage to the global
            // per-model tracker AND the per-workspace ledger so a
            // task's true cost (orchestrator + every subagent) is
            // captured. Attribute to the subagent's *effective* model
            // (its `agent_def` may pin a cheaper one than the parent) —
            // same selection as the build path, so a cross-provider pin that
            // fell back to the session model is billed to what actually ran.
            let base_model = self.factory.base_model();
            // Same selection as the build path (same config-driven usability
            // check) so a cross-provider pin is billed to what actually ran.
            let attr_config = crate::config::AppConfig::load().unwrap_or_default();
            let eff_model = subagent_model(
                agent_def.and_then(|d| d.model.as_deref()),
                &base_model,
                &attr_config,
            );
            let provider = crate::providers::ProviderKind::detect(eff_model)
                .map(|k| k.name())
                .unwrap_or("unknown");
            let role = agent_name.unwrap_or("subagent");
            crate::usage::UsageTracker::new(crate::usage::UsageTracker::default_path())
                .record(provider, eff_model, u);
            if let Ok(cwd) = std::env::current_dir() {
                crate::usage::append_usage_ledger(&cwd, role, provider, eff_model, u);
            }
        }

        if outcome.text.is_empty() {
            // L1: distinguish "did nothing" from "acted but produced no
            // final text". A sub-agent that ran tools (e.g. wrote files)
            // but hit max_iterations or stopped without a closing message
            // used to surface a spurious "empty response" error to the
            // caller even though work happened. Return a synthetic summary
            // of the tool activity instead; only error when truly nothing
            // ran.
            if outcome.tool_calls.is_empty() {
                Err(Error::Agent("sub-agent returned empty response".into()))
            } else {
                Ok(format!(
                    "(sub-agent finished without a final text message; ran {} tool call(s): {})",
                    outcome.tool_calls.len(),
                    outcome.tool_calls.join(", ")
                ))
            }
        } else {
            Ok(outcome.text)
        }
    }
}

/// Open the tool gate for every gated tool this def explicitly allow-lists.
/// Looks each name up in `base_tools` (gate membership is a property of the
/// registered tool, independent of whether the gate is open), and activates
/// its gate. Empty `def.tools` (inherit-all) is intentionally skipped by the
/// caller so an unrestricted subagent doesn't fling every gate open.
fn open_gates_for_allowlist(def: &crate::agent_defs::AgentDef, base_tools: &ToolRegistry) {
    for name in &def.tools {
        if let Some(gate) = base_tools.get(name).and_then(|t| t.requires_gate()) {
            crate::tools::activate_gate(gate);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::providers::{EventStream, Provider, ProviderEvent, StreamRequest};
    use crate::tools::ToolRegistry;
    use async_trait::async_trait;
    use futures::stream;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[test]
    fn allowlisting_a_gated_tool_opens_its_gate() {
        crate::tools::reset_gates();
        let mut base = ToolRegistry::new();
        base.register(std::sync::Arc::new(crate::tools::FetchImagesTool::new())); // gated "content-extractor"

        // Def that does NOT list it → gate stays closed.
        let mut def = crate::agent_defs::AgentDef {
            tools: vec!["Read".into()],
            ..Default::default()
        };
        open_gates_for_allowlist(&def, &base);
        assert!(!crate::tools::gate_is_active("content-extractor"));

        // Def that explicitly allow-lists FetchImages → gate opens.
        def.tools = vec!["Read".into(), "FetchImages".into()];
        open_gates_for_allowlist(&def, &base);
        assert!(crate::tools::gate_is_active("content-extractor"));
        crate::tools::reset_gates();
    }

    #[test]
    fn subagent_model_honors_pin_only_when_provider_is_usable() {
        use std::sync::Mutex;
        // Serialise this test's env mutation (cargo runs lib tests in
        // parallel), mirroring the gateway module's env-touching tests.
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("THCLAWS_DISABLE_KEYCHAIN", "1");
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("THCLAWS_GATEWAY_API_KEY");

        let empty = crate::config::AppConfig::default();

        // Same provider as the session → honored regardless of creds.
        assert_eq!(
            subagent_model(Some("claude-haiku-4-5"), "claude-sonnet-4-6", &empty),
            "claude-haiku-4-5"
        );
        // No pin → session model.
        assert_eq!(
            subagent_model(None, "deepseek-v4-pro", &empty),
            "deepseek-v4-pro"
        );
        // Unrecognized pin → session model (can't confirm it routes).
        assert_eq!(
            subagent_model(Some("totally-unknown-model"), "gpt-5.5", &empty),
            "gpt-5.5"
        );
        // Cross-provider pin with no BYOK key and no gateway route → falls back.
        assert_eq!(
            subagent_model(Some("claude-haiku-4-5"), "deepseek-v4-pro", &empty),
            "deepseek-v4-pro"
        );
        // Cross-provider pin the GATEWAY is configured to route → honored,
        // even on a deepseek session (create_subagent builds a matching
        // provider). Access key present so the overlay resolves; native key
        // absent so BYOK doesn't suppress the overlay.
        std::env::set_var("THCLAWS_GATEWAY_API_KEY", "gw-test");
        let mut routed = crate::config::AppConfig::default();
        routed.gateway_use_for = vec!["anthropic".to_string()];
        assert_eq!(
            subagent_model(Some("claude-haiku-4-5"), "deepseek-v4-pro", &routed),
            "claude-haiku-4-5"
        );

        std::env::remove_var("THCLAWS_GATEWAY_API_KEY");
        std::env::remove_var("THCLAWS_DISABLE_KEYCHAIN");
    }

    struct ScriptedProvider {
        scripts: Arc<Mutex<VecDeque<Vec<ProviderEvent>>>>,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Vec<ProviderEvent>>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Arc::new(Mutex::new(VecDeque::from(scripts))),
            })
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn stream(&self, _req: StreamRequest) -> Result<EventStream> {
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::Provider("no more scripts".into()))?;
            let events: Vec<Result<ProviderEvent>> = script.into_iter().map(Ok).collect();
            Ok(Box::pin(stream::iter(events)))
        }
    }

    fn text_script(chunks: &[&str]) -> Vec<ProviderEvent> {
        let mut out = vec![ProviderEvent::MessageStart {
            model: "test".into(),
        }];
        for c in chunks {
            out.push(ProviderEvent::TextDelta((*c).to_string()));
        }
        out.push(ProviderEvent::ContentBlockStop);
        out.push(ProviderEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        });
        out
    }

    struct SimpleFactory {
        scripts: Arc<Mutex<Vec<Vec<Vec<ProviderEvent>>>>>,
    }

    impl SimpleFactory {
        fn new(scripts: Vec<Vec<Vec<ProviderEvent>>>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Arc::new(Mutex::new(scripts)),
            })
        }
    }

    #[async_trait]
    impl AgentFactory for SimpleFactory {
        async fn build(
            &self,
            _prompt: &str,
            _def: Option<&AgentDef>,
            _depth: usize,
        ) -> Result<Agent> {
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| Error::Agent("factory exhausted".into()))?;
            let provider = ScriptedProvider::new(script);
            Ok(Agent::new(provider, ToolRegistry::new(), "test", ""))
        }
    }

    #[tokio::test]
    async fn sub_agent_returns_text() {
        let factory = SimpleFactory::new(vec![vec![text_script(&["done"])]]);
        let tool = SubAgentTool::new(factory);
        let out = tool.call(json!({"prompt": "go"})).await.unwrap();
        assert_eq!(out, "done");
    }

    #[tokio::test]
    async fn depth_limit_enforced() {
        let factory = SimpleFactory::new(vec![]);
        let tool = SubAgentTool::new(factory).with_depth(3).with_max_depth(3);
        let err = tool.call(json!({"prompt": "go"})).await.unwrap_err();
        assert!(format!("{err}").contains("recursion limit"));
    }

    #[tokio::test]
    async fn unknown_agent_errors() {
        let factory = SimpleFactory::new(vec![]);
        let tool = SubAgentTool::new(factory);
        let err = tool
            .call(json!({"prompt": "go", "agent": "nonexistent"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("unknown agent"));
    }

    struct EchoTool {
        name: &'static str,
    }
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "echo"
        }
        fn input_schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn call(&self, _input: Value) -> Result<String> {
            Ok(String::new())
        }
    }

    struct StubProvider;
    #[async_trait]
    impl Provider for StubProvider {
        async fn stream(&self, _r: StreamRequest) -> Result<EventStream> {
            Ok(Box::pin(stream::iter(vec![Ok(
                ProviderEvent::MessageStart {
                    model: "test".into(),
                },
            )])))
        }
    }

    /// M6.33 SUB2: agent_def.disallowed_tools must be honored. Pre-fix
    /// the field was parsed but never applied — agent definitions
    /// claiming `disallowed_tools: Bash` got Bash anyway.
    #[tokio::test]
    async fn production_factory_applies_agent_def_disallowed_tools() {
        let mut base = ToolRegistry::new();
        base.register(Arc::new(EchoTool { name: "Bash" }));
        base.register(Arc::new(EchoTool { name: "Read" }));

        let factory = ProductionAgentFactory {
            snapshot: Arc::new(RwLock::new(FactorySnapshot {
                system: String::new(),
                tools: base,
                model: "test".into(),
                provider: Arc::new(StubProvider),
            })),
            max_iterations: 1,
            max_depth: 3,
            max_tokens: 8192,
            agent_defs: AgentDefsConfig::default(),
            approver: Arc::new(crate::permissions::DenyApprover),
            permission_mode: PermissionMode::Auto,
            cancel: None,
            hooks: None,
        };
        let def = AgentDef {
            name: "restricted".into(),
            disallowed_tools: vec!["Bash".into()],
            ..Default::default()
        };
        let child = factory.build("go", Some(&def), 1).await.unwrap();
        let names = child.tools.names();
        assert!(
            !names.contains(&"Bash"),
            "Bash should be removed by disallowed_tools, got {names:?}"
        );
        assert!(names.contains(&"Read"), "Read should remain, got {names:?}");
    }

    /// M6.33 SUB4: parent's cancel token propagates into the built
    /// child agent so retry-backoff sleeps + the streaming collector
    /// observe ctrl-C. Pre-fix the subagent ran to completion.
    #[tokio::test]
    async fn production_factory_propagates_cancel_token() {
        let cancel = CancelToken::new();
        let factory = ProductionAgentFactory {
            snapshot: Arc::new(RwLock::new(FactorySnapshot {
                system: String::new(),
                tools: ToolRegistry::new(),
                model: "test".into(),
                provider: Arc::new(StubProvider),
            })),
            max_iterations: 1,
            max_depth: 3,
            max_tokens: 8192,
            agent_defs: AgentDefsConfig::default(),
            approver: Arc::new(crate::permissions::DenyApprover),
            permission_mode: PermissionMode::Auto,
            cancel: Some(cancel.clone()),
            hooks: None,
        };
        let child = factory.build("go", None, 1).await.unwrap();
        cancel.cancel();
        assert!(
            child
                .cancel
                .as_ref()
                .map(|c| c.is_cancelled())
                .unwrap_or(false),
            "child agent should observe parent's cancel token"
        );
    }

    /// Regression: the factory must see the LIVE system prompt + tool
    /// registry — not a snapshot frozen at construction time. Pre-fix
    /// (everything before this commit) ProductionAgentFactory held
    /// `system: String` + `base_tools: ToolRegistry` as owned fields
    /// populated once at worker init. Mid-session `/mcp add` /
    /// `/skill install` / `/kms use` / `/reload-prompt` updated the
    /// PARENT agent's system + tool_registry but never reached the
    /// factory — subagents spawned post-mutator saw the startup-time
    /// snapshot, missing newly-attached MCP tools and stale on the
    /// `# MCP server instructions` / KMS / Memory sections.
    #[tokio::test]
    async fn production_factory_reads_live_snapshot() {
        let mut initial_tools = ToolRegistry::new();
        initial_tools.register(Arc::new(EchoTool { name: "OldTool" }));
        let snapshot = Arc::new(RwLock::new(FactorySnapshot {
            system: "INITIAL_SYSTEM".into(),
            tools: initial_tools,
            model: "test".into(),
            provider: Arc::new(StubProvider),
        }));
        let factory = ProductionAgentFactory {
            snapshot: snapshot.clone(),
            max_iterations: 1,
            max_depth: 3,
            max_tokens: 8192,
            agent_defs: AgentDefsConfig::default(),
            approver: Arc::new(crate::permissions::DenyApprover),
            permission_mode: PermissionMode::Auto,
            cancel: None,
            hooks: None,
        };

        // Build once with the initial snapshot — child sees OldTool
        // and the initial system.
        let child1 = factory.build("go", None, 1).await.unwrap();
        let names1 = child1.tools.names();
        assert!(
            names1.contains(&"OldTool"),
            "child should see initial tools, got {names1:?}"
        );
        assert!(
            child1.system_text().contains("INITIAL_SYSTEM"),
            "child should see initial system; got: {:?}",
            child1.system_text()
        );

        // Worker-side mutation: a `/mcp add` would do this — update
        // tool registry, refresh system prompt, then push both into
        // the shared snapshot.
        assert_eq!(child1.model(), "test");

        let mut updated_tools = ToolRegistry::new();
        updated_tools.register(Arc::new(EchoTool { name: "NewTool" }));
        {
            let mut snap = snapshot.write().unwrap();
            snap.system = "REFRESHED_SYSTEM".into();
            snap.tools = updated_tools;
            // Mid-session model switch (GUI ReloadConfig / CLI `/model`)
            // writes the new pair the same way.
            snap.model = "switched-model".into();
        }

        // Build AGAIN with the same factory — the new child must see
        // the refreshed state, NOT the initial snapshot.
        let child2 = factory.build("go", None, 1).await.unwrap();
        let names2 = child2.tools.names();
        assert!(
            names2.contains(&"NewTool"),
            "child built after refresh must see new tool, got {names2:?}"
        );
        assert!(
            !names2.contains(&"OldTool"),
            "child built after refresh must NOT see old tool, got {names2:?}"
        );
        assert!(
            child2.system_text().contains("REFRESHED_SYSTEM"),
            "child built after refresh must see fresh system; got: {:?}",
            child2.system_text()
        );
        assert!(
            !child2.system_text().contains("INITIAL_SYSTEM"),
            "child built after refresh must NOT see stale system; got: {:?}",
            child2.system_text()
        );
        // The grok-main/deepseek-workers bug: an unpinned subagent must
        // inherit the model the session is CURRENTLY on.
        assert_eq!(
            child2.model(),
            "switched-model",
            "child built after a model switch must run the new model"
        );
        assert_eq!(factory.base_model(), "switched-model");
    }

    #[tokio::test]
    async fn named_agent_passed_to_factory() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let saw_def = Arc::new(AtomicBool::new(false));
        let saw_def_clone = saw_def.clone();

        struct DefCheckFactory(Arc<AtomicBool>);
        #[async_trait]
        impl AgentFactory for DefCheckFactory {
            async fn build(&self, _p: &str, def: Option<&AgentDef>, _d: usize) -> Result<Agent> {
                if let Some(d) = def {
                    assert_eq!(d.name, "researcher");
                    self.0.store(true, Ordering::Relaxed);
                }
                let provider = ScriptedProvider::new(vec![text_script(&["found it"])]);
                Ok(Agent::new(provider, ToolRegistry::new(), "test", ""))
            }
        }

        let defs = crate::agent_defs::AgentDefsConfig {
            agents: vec![AgentDef {
                name: "researcher".into(),
                instructions: "Research things".into(),
                max_iterations: 5,
                ..Default::default()
            }],
        };

        let factory = Arc::new(DefCheckFactory(saw_def_clone));
        let tool = SubAgentTool::new(factory).with_agent_defs(defs);
        let out = tool
            .call(json!({"prompt": "find X", "agent": "researcher"}))
            .await
            .unwrap();
        assert_eq!(out, "found it");
        assert!(saw_def.load(Ordering::Relaxed));
    }

    /// Build a production factory over `base` with max_depth 3 — shared
    /// by the H1/M1 recursion-scoping tests below.
    fn factory_with(base: ToolRegistry) -> ProductionAgentFactory {
        ProductionAgentFactory {
            snapshot: Arc::new(RwLock::new(FactorySnapshot {
                system: String::new(),
                tools: base,
                model: "test".into(),
                provider: Arc::new(StubProvider),
            })),
            max_iterations: 1,
            max_depth: 3,
            max_tokens: 8192,
            agent_defs: AgentDefsConfig::default(),
            approver: Arc::new(crate::permissions::DenyApprover),
            permission_mode: PermissionMode::Auto,
            cancel: None,
            hooks: None,
        }
    }

    /// H1 regression: the snapshot's base registry can already hold the
    /// ROOT depth-0 `Task` tool (any mid-session refresh re-clones the
    /// live parent registry, which includes it). A child built at the
    /// depth ceiling (`child_depth == max_depth`, where the registration
    /// block is skipped) must NOT inherit that root Task — otherwise it
    /// could spawn from depth 0 again and defeat `max_depth` entirely.
    #[tokio::test]
    async fn production_factory_strips_inherited_task_at_leaf() {
        let mut base = ToolRegistry::new();
        base.register(Arc::new(EchoTool { name: TOOL_NAME })); // root Task baked into snapshot
        base.register(Arc::new(EchoTool { name: "Read" }));

        let child = factory_with(base).build("go", None, 3).await.unwrap();
        let names = child.tools.names();
        assert!(
            !names.contains(&TOOL_NAME),
            "leaf at max_depth must not inherit the root Task, got {names:?}"
        );
        assert!(
            names.contains(&"Read"),
            "non-Task tools must survive the strip, got {names:?}"
        );
    }

    /// Below the depth ceiling, a correctly-depthed Task IS registered —
    /// the strip removes the inherited root Task, then a fresh child Task
    /// replaces it so recursion continues normally.
    #[tokio::test]
    async fn production_factory_registers_task_below_max_depth() {
        let mut base = ToolRegistry::new();
        base.register(Arc::new(EchoTool { name: TOOL_NAME }));

        let child = factory_with(base).build("go", None, 1).await.unwrap();
        assert!(
            child.tools.names().contains(&TOOL_NAME),
            "child below max_depth should have a Task tool"
        );
    }

    /// M1: `disallowed_tools: ["Task"]` must actually prevent the child
    /// from spawning further subagents, even below the depth ceiling.
    /// Pre-fix the deny-list could not remove Task (it was registered
    /// after the filter ran).
    #[tokio::test]
    async fn production_factory_disallowed_task_blocks_recursion() {
        let mut base = ToolRegistry::new();
        base.register(Arc::new(EchoTool { name: TOOL_NAME }));
        base.register(Arc::new(EchoTool { name: "Read" }));

        let def = AgentDef {
            name: "no-recurse".into(),
            disallowed_tools: vec![TOOL_NAME.into()],
            ..Default::default()
        };
        let child = factory_with(base).build("go", Some(&def), 1).await.unwrap();
        let names = child.tools.names();
        assert!(
            !names.contains(&TOOL_NAME),
            "disallowed_tools must remove Task, got {names:?}"
        );
        assert!(names.contains(&"Read"), "Read should remain, got {names:?}");
    }

    /// M1: a non-empty tools allow-list that omits `Task` must also
    /// disable recursion — the allow-list is exhaustive.
    #[tokio::test]
    async fn production_factory_allowlist_omitting_task_blocks_recursion() {
        let mut base = ToolRegistry::new();
        base.register(Arc::new(EchoTool { name: TOOL_NAME }));
        base.register(Arc::new(EchoTool { name: "Read" }));

        let def = AgentDef {
            name: "reader".into(),
            tools: vec!["Read".into()],
            ..Default::default()
        };
        let child = factory_with(base).build("go", Some(&def), 1).await.unwrap();
        let names = child.tools.names();
        assert!(
            !names.contains(&TOOL_NAME),
            "allow-list omitting Task must not grant it, got {names:?}"
        );
        assert!(
            names.contains(&"Read"),
            "Read should be granted, got {names:?}"
        );
    }

    /// M1: an allow-list that explicitly lists `Task` keeps recursion
    /// (and the registered tool is the real correctly-depthed one).
    #[tokio::test]
    async fn production_factory_allowlist_with_task_keeps_recursion() {
        let mut base = ToolRegistry::new();
        base.register(Arc::new(EchoTool { name: TOOL_NAME }));
        base.register(Arc::new(EchoTool { name: "Read" }));

        let def = AgentDef {
            name: "lead".into(),
            tools: vec!["Read".into(), TOOL_NAME.into()],
            ..Default::default()
        };
        let child = factory_with(base).build("go", Some(&def), 1).await.unwrap();
        assert!(
            child.tools.names().contains(&TOOL_NAME),
            "allow-list including Task should keep recursion"
        );
    }

    /// `AgentDef::mcp` scopes MCP tools (`<server>__<tool>`) by server:
    /// listed servers' tools survive, others are dropped, non-MCP tools
    /// are untouched.
    #[tokio::test]
    async fn production_factory_scopes_mcp_by_server() {
        let mut base = ToolRegistry::new();
        base.register(Arc::new(EchoTool { name: "Read" }));
        base.register(Arc::new(EchoTool {
            name: "weather__forecast",
        }));
        base.register(Arc::new(EchoTool {
            name: "pinn-ai__text2image",
        }));

        let def = AgentDef {
            name: "img".into(),
            mcp: vec!["pinn-ai".into()],
            ..Default::default()
        };
        let child = factory_with(base).build("go", Some(&def), 1).await.unwrap();
        let names = child.tools.names();
        assert!(
            names.contains(&"pinn-ai__text2image"),
            "allowed MCP server kept, got {names:?}"
        );
        assert!(
            !names.contains(&"weather__forecast"),
            "non-listed MCP server dropped, got {names:?}"
        );
        assert!(names.contains(&"Read"), "non-MCP tools kept, got {names:?}");
    }

    /// `AgentDef::skills` replaces the inherited Skill tool with an
    /// allow-list-scoped copy (same store handle): listed skills load,
    /// others are refused.
    #[tokio::test]
    async fn production_factory_scopes_skills() {
        use crate::skills::{SkillDef, SkillStore, SkillTool};
        let mut store = SkillStore::default();
        store.skills.insert(
            "pdf".into(),
            SkillDef::new_eager(
                "pdf".into(),
                "d".into(),
                "".into(),
                std::path::PathBuf::from("/tmp"),
                "PDF BODY".into(),
            ),
        );
        store.skills.insert(
            "xlsx".into(),
            SkillDef::new_eager(
                "xlsx".into(),
                "d".into(),
                "".into(),
                std::path::PathBuf::from("/tmp"),
                "XLSX BODY".into(),
            ),
        );
        let mut base = ToolRegistry::new();
        base.register(Arc::new(SkillTool::new(store)));

        let def = AgentDef {
            name: "pdf-only".into(),
            skills: vec!["pdf".into()],
            ..Default::default()
        };
        let child = factory_with(base).build("go", Some(&def), 1).await.unwrap();
        let skill = child.tools.get("Skill").expect("Skill tool present");
        let err = skill.call(json!({"name": "xlsx"})).await.unwrap_err();
        assert!(
            format!("{err}").contains("not available to this agent"),
            "scoped Skill must refuse non-listed skill, got: {err}"
        );
        assert!(skill
            .call(json!({"name": "pdf"}))
            .await
            .unwrap()
            .contains("PDF BODY"));
    }

    /// M2: spawning a sub-agent is unguarded so behavior is consistent
    /// across the sequential (1 call) and concurrent (≥2 calls) paths.
    #[test]
    fn subagent_tool_does_not_require_approval() {
        let tool = SubAgentTool::new(SimpleFactory::new(vec![]));
        assert!(!tool.requires_approval(&json!({"prompt": "go"})));
    }

    /// `parallelizable() == true` only pays off if the model actually
    /// emits ≥2 `Task` blocks in one message, so the schema description
    /// has to say so. Pre-fix it never mentioned batching and the
    /// prompt's Collaboration section actively forbade it — subagents
    /// ran one at a time despite the concurrent fast-path in
    /// `agent::run_turn`.
    #[test]
    fn subagent_tool_description_advertises_concurrent_fan_out() {
        let tool = SubAgentTool::new(SimpleFactory::new(vec![]));
        assert!(tool.parallelizable());
        let d = tool.description();
        assert!(
            d.contains("SINGLE message"),
            "description must tell the model to batch Task calls, got: {d}"
        );
        assert!(
            d.contains("run concurrently"),
            "description must say a batch runs concurrently, got: {d}"
        );
    }

    fn tool_call_script(id: &str, name: &str, args: &str) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::MessageStart {
                model: "test".into(),
            },
            ProviderEvent::ToolUseStart {
                id: id.into(),
                name: name.into(),
                thought_signature: None,
            },
            ProviderEvent::ToolUseDelta {
                partial_json: args.into(),
            },
            ProviderEvent::ContentBlockStop,
            ProviderEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ]
    }

    fn empty_end_script() -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::MessageStart {
                model: "test".into(),
            },
            ProviderEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ]
    }

    /// L1: a sub-agent that runs a tool but emits no final text must
    /// return a synthetic summary of its tool activity rather than the
    /// spurious "empty response" error it produced pre-fix.
    #[tokio::test]
    async fn empty_text_with_tool_activity_returns_summary() {
        struct ToolRunFactory;
        #[async_trait]
        impl AgentFactory for ToolRunFactory {
            async fn build(&self, _p: &str, _d: Option<&AgentDef>, _depth: usize) -> Result<Agent> {
                let mut reg = ToolRegistry::new();
                reg.register(Arc::new(EchoTool { name: "Echo" }));
                // Turn 1: call Echo. Turn 2: stop with no text.
                let provider = ScriptedProvider::new(vec![
                    tool_call_script("call-1", "Echo", "{}"),
                    empty_end_script(),
                ]);
                Ok(Agent::new(provider, reg, "test", ""))
            }
        }

        let tool = SubAgentTool::new(Arc::new(ToolRunFactory));
        let out = tool.call(json!({"prompt": "go"})).await.unwrap();
        assert!(
            out.contains("Echo"),
            "summary should name the tool that ran, got: {out}"
        );
        assert!(
            out.contains("without a final text"),
            "should be the synthetic summary, got: {out}"
        );
    }

    /// L1: a sub-agent that does literally nothing (no text, no tools)
    /// still errors — there's nothing to report.
    #[tokio::test]
    async fn empty_text_no_tools_still_errors() {
        let factory = SimpleFactory::new(vec![vec![empty_end_script()]]);
        let tool = SubAgentTool::new(factory);
        let err = tool.call(json!({"prompt": "go"})).await.unwrap_err();
        assert!(format!("{err}").contains("empty response"));
    }

    /// 47.2: a `PathScopedWriteTool` allows writes inside its globs and
    /// refuses writes outside them, before the inner tool ever runs.
    #[tokio::test]
    async fn write_paths_scoping_allows_inside_and_refuses_outside() {
        struct OkWrite;
        #[async_trait]
        impl Tool for OkWrite {
            fn name(&self) -> &'static str {
                "Write"
            }
            fn description(&self) -> &'static str {
                "mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> Result<String> {
                Ok("wrote".into())
            }
        }
        let set = build_write_globset(&[".thclaws/state/kms/**".to_string()]).unwrap();
        let scoped = PathScopedWriteTool {
            inner: Arc::new(OkWrite),
            globs: Arc::new(set),
            patterns: vec![".thclaws/state/kms/**".into()],
        };
        // Inside the allow-list → delegates to the inner tool.
        let ok = scoped
            .call(json!({"path": ".thclaws/state/kms/ai/page.md", "content": "x"}))
            .await
            .unwrap();
        assert_eq!(ok, "wrote");
        // Outside → refused before the inner tool runs.
        let err = scoped
            .call(json!({"path": "src/main.rs", "content": "x"}))
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("writePaths"),
            "expected writePaths denial, got: {err}"
        );
    }

    /// Public issue #204: `output/../traversal.txt` matched the glob
    /// `output/**` as a string while the write landed at the workspace
    /// root. The three rows are the reporter's own control table.
    #[tokio::test]
    async fn write_paths_scoping_resolves_parent_traversal() {
        struct OkWrite;
        #[async_trait]
        impl Tool for OkWrite {
            fn name(&self) -> &'static str {
                "Write"
            }
            fn description(&self) -> &'static str {
                "mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> Result<String> {
                Ok("wrote".into())
            }
        }
        let scoped = PathScopedWriteTool {
            inner: Arc::new(OkWrite),
            globs: Arc::new(build_write_globset(&["output/**".to_string()]).unwrap()),
            patterns: vec!["output/**".into()],
        };

        assert_eq!(
            scoped
                .call(json!({"path": "output/allowed.txt", "content": "x"}))
                .await
                .unwrap(),
            "wrote"
        );

        for escape in [
            "outside.txt",
            "output/../traversal.txt",
            // Deeper and doubled — one `..` stripped is not a fix.
            "output/a/b/../../../traversal.txt",
            "output/./../traversal.txt",
        ] {
            let err = scoped
                .call(json!({"path": escape, "content": "x"}))
                .await
                .unwrap_err();
            assert!(
                format!("{err}").contains("writePaths"),
                "{escape} should be refused, got: {err}"
            );
        }

        // `..` that stays inside the scope is still legal — the fix
        // resolves paths, it does not ban a character.
        assert_eq!(
            scoped
                .call(json!({"path": "output/sub/../kept.txt", "content": "x"}))
                .await
                .unwrap(),
            "wrote"
        );
    }

    /// The other half of #204's suggested fix: a symlink must be followed
    /// before the globs see the path, or `output/link/escape.txt` walks
    /// out the same way `..` did.
    #[test]
    fn resolve_destination_follows_symlinks_and_new_tails() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("output")).unwrap();
        std::fs::create_dir(root.join("elsewhere")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("output/link")).unwrap();

        // A file that does not exist yet still resolves, via its parent.
        assert_eq!(
            resolve_destination(&root.join("output/new.txt")),
            root.join("output/new.txt")
        );
        // `..` collapses even when nothing along the way exists.
        assert_eq!(
            resolve_destination(&root.join("output/../up.txt")),
            root.join("up.txt")
        );
        #[cfg(unix)]
        assert_eq!(
            resolve_destination(&root.join("output/link/escape.txt")),
            root.join("elsewhere/escape.txt"),
            "a symlinked directory must resolve before the globs match"
        );
    }
}
