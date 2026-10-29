//! Agent definitions — named agent configs for sub-agents and team members.
//!
//! Load order (later overrides earlier):
//! 1. `~/.config/thclaws/agents.json` — user global (legacy JSON format)
//! 2. `~/.claude/agents/*.md` — user Claude Code
//! 3. `~/.config/thclaws/agents/*.md` — user thClaws
//! 4. `.claude/agents/*.md` — project Claude Code
//! 5. `.thclaws/agents/*.md` — project thClaws (highest priority)
//!
//! Plus any plugin-contributed agent dirs (see [`crate::plugins`]), which
//! are merged additively and never shadow the sources above.
//!
//! Markdown format (YAML frontmatter + body as instructions):
//! ```markdown
//! ---
//! name: researcher
//! description: Researches topics thoroughly
//! model: claude-sonnet-4-5
//! tools: Read, Grep, Glob, WebSearch
//! maxTurns: 20
//! ---
//! You are a research agent. Search the codebase and web...
//! ```
//!
//! Used by both the Task tool (sub-agents) and Agent Teams (teammates).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    /// Tool names this agent can use. Empty = all built-in tools.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Tools to exclude.
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// Skill names this agent may load/list/search. Empty = inherit the
    /// parent's full skill set. When non-empty, the subagent's
    /// Skill/SkillList/SkillSearch tools are scoped to exactly these
    /// names (loading any other skill is refused).
    #[serde(default)]
    pub skills: Vec<String>,
    /// MCP server names this agent may use. Empty = inherit all of the
    /// parent's MCP tools. When non-empty, MCP tools whose server segment
    /// isn't listed are removed from the subagent's registry.
    #[serde(default)]
    pub mcp: Vec<String>,
    /// Agent terminal color.
    #[serde(default)]
    pub color: Option<String>,
    /// Isolation mode: "worktree" creates a git worktree for the agent.
    #[serde(default)]
    pub isolation: Option<String>,
    /// Permission mode override.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Declared output JSON Schema. When a workflow `thclaws.subagent({agent})`
    /// call omits a per-call `schema`, the worker's output is validated against
    /// this — one source of truth instead of duplicating the schema in the
    /// workflow JS. In frontmatter it's either single-line inline JSON or a
    /// path (relative to the def's `.md`) to a `.json` schema file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    /// Declared input JSON Schema — documentation + a hook for
    /// `thclaws agent validate`. Same frontmatter encoding as `output_schema`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
    /// Glob allow-list confining this agent's file writes. Empty = inherit
    /// (writes anywhere the parent permits). When non-empty, the file-write
    /// tools (Write/Edit/office create+edit) refuse a target outside these
    /// globs — mechanical write-scoping instead of a prompt promise. Globs
    /// are workspace-relative (e.g. `.thclaws/state/kms/**`). NOTE: this scopes
    /// the file-write tools, not `Bash`; a writer that also has Bash can
    /// still write via the shell.
    #[serde(default)]
    pub write_paths: Vec<String>,
}

fn default_max_iterations() -> usize {
    200
}

impl Default for AgentDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            instructions: String::new(),
            model: None,
            max_iterations: default_max_iterations(),
            tools: vec![],
            disallowed_tools: vec![],
            skills: vec![],
            mcp: vec![],
            color: None,
            isolation: None,
            permission_mode: None,
            output_schema: None,
            input_schema: None,
            write_paths: vec![],
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentDefsConfig {
    #[serde(default)]
    pub agents: Vec<AgentDef>,
}

impl AgentDefsConfig {
    /// Load agent definitions from all sources (JSON + markdown directories).
    pub fn load() -> Self {
        Self::load_with_extra(&[])
    }

    /// Load, additionally walking each directory in `extra` after the
    /// standard dirs. Used by the plugin system to surface agent defs
    /// contributed by installed plugins. Standard dirs still win on name
    /// collision because they're loaded first — plugin agents are merged
    /// only when they don't clash.
    pub fn load_with_extra(extra: &[PathBuf]) -> Self {
        let mut config = Self::default();

        // 0. Built-in agent defs compiled into the binary. Seeded first
        // so every other source (legacy JSON, user/project md dirs)
        // overrides by name. Surface area is intentionally small —
        // built-ins ship for first-class operations like `/dream`.
        config.seed_builtins();

        // 1. Legacy JSON config.
        let json_path = Self::default_json_path();
        if json_path.exists() {
            if let Ok(contents) = std::fs::read_to_string(&json_path) {
                if let Ok(json_config) = serde_json::from_str::<AgentDefsConfig>(&contents) {
                    config.agents.extend(json_config.agents);
                }
            }
        }

        // 2. Standard markdown agent directories. Later entries in the
        // list override earlier ones (same name), so the order here sets
        // priority: user-global < project < … any plugin dirs appended
        // below.
        for dir in Self::agent_dirs() {
            if dir.exists() {
                config.load_md_dir(&dir);
            }
        }

        // 3. Plugin-contributed dirs. Walk them via `load_md_dir_no_clobber`
        // so a plugin can't shadow a user's or project's agent by name —
        // the existing entry is kept.
        for dir in extra {
            if dir.exists() {
                config.load_md_dir_no_clobber(dir);
            }
        }

        config
    }

    fn default_json_path() -> PathBuf {
        crate::util::home_dir()
            .map(|h| {
                h.join(format!(
                    ".config/{}/agents.json",
                    crate::profile::app_dir_name()
                ))
            })
            .unwrap_or_else(|| PathBuf::from("agents.json"))
    }

    /// Directories to scan for agent .md files, in priority order.
    /// Later entries override earlier ones (same name).
    fn agent_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(home) = crate::util::home_dir() {
            if crate::profile::reads_claude_home() {
                dirs.push(home.join(".claude/agents")); // user Claude Code
            }
            dirs.push(home.join(format!(".config/{}/agents", crate::profile::app_dir_name())));
            // user thClaws
        }
        dirs.push(PathBuf::from(".claude/agents")); // project Claude Code
        dirs.push(PathBuf::from(".thclaws/agents")); // project thClaws (highest priority)
        dirs
    }

    /// Load agent definitions from a directory of .md files.
    fn load_md_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Some(agent) = Self::parse_agent_md(&path) {
                // Override existing agent with same name.
                if let Some(existing) = self.agents.iter_mut().find(|a| a.name == agent.name) {
                    *existing = agent;
                } else {
                    self.agents.push(agent);
                }
            }
        }
    }

    /// Variant of [`load_md_dir`] that keeps the existing agent on a name
    /// collision. Used for plugin-contributed dirs so a plugin can't
    /// shadow the user's own agent defs.
    fn load_md_dir_no_clobber(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Some(agent) = Self::parse_agent_md(&path) {
                if self.agents.iter().any(|a| a.name == agent.name) {
                    continue;
                }
                self.agents.push(agent);
            }
        }
    }

    /// Seed the config with built-in agent defs compiled into the
    /// binary. Each entry pairs a fallback name (used if the markdown
    /// has no `name:` frontmatter) with the embedded source. Built-ins
    /// land at the lowest priority so any user/project agent def with
    /// the same name will override them.
    /// Apply settings.json overrides for built-in subagents' `model:`
    /// field. Each built-in that needs settings tunability gets a
    /// matching `<name>_subagent_model` field on AppConfig; this
    /// helper resolves them against the loaded AgentDefs by name and
    /// edits in place. Disk-loaded user agent files at
    /// `.thclaws/agents/<name>.md` still win because they replaced
    /// the embedded AgentDef during the prior load_md_dir pass — this
    /// only edits whatever's currently registered under that name
    /// (built-in or user, doesn't matter).
    pub fn apply_builtin_subagent_overrides(&mut self, config: &crate::config::AppConfig) {
        if let Some(ref m) = config.translator_subagent_model {
            if let Some(def) = self.agents.iter_mut().find(|d| d.name == "translator") {
                def.model = Some(m.clone());
            }
        }
        // Future built-in subagents add their override branch here.
        // Pattern: read AppConfig::<name>_subagent_model, find AgentDef
        // by name, replace `model` field. Three lines per built-in.
    }

    fn seed_builtins(&mut self) {
        const BUILTINS: &[(&str, &str)] = &[
            ("dream", include_str!("default_prompts/dream.md")),
            ("translator", include_str!("default_prompts/translator.md")),
            ("summarizer", include_str!("default_prompts/summarizer.md")),
            (
                "content-extractor",
                include_str!("default_prompts/content-extractor.md"),
            ),
            (
                "folder-indexer",
                include_str!("default_prompts/folder-indexer.md"),
            ),
            ("kms-linker", include_str!("default_prompts/kms-linker.md")),
            (
                "kms-reconcile",
                include_str!("default_prompts/kms-reconcile.md"),
            ),
            (
                "kms-maintain",
                include_str!("default_prompts/kms-maintain.md"),
            ),
        ];
        for (fallback_name, raw) in BUILTINS {
            if let Some(agent) = Self::parse_agent_md_str(raw, fallback_name, None) {
                self.agents.push(agent);
            }
        }
    }

    /// Parse an agent .md file with YAML frontmatter.
    fn parse_agent_md(path: &Path) -> Option<AgentDef> {
        let raw = std::fs::read_to_string(path).ok()?;
        let fallback = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        Self::parse_agent_md_str(&raw, fallback, path.parent())
    }

    /// Parse an agent .md body (frontmatter + instructions) from an
    /// in-memory string. `fallback_name` is used when the frontmatter
    /// has no `name:` key — for disk loads this is the file stem; for
    /// embedded built-ins it's a hard-coded name. `base_dir` is the
    /// directory the `.md` lives in, used to resolve `output_schema` /
    /// `input_schema` paths; `None` for embedded built-ins (inline JSON
    /// only).
    fn parse_agent_md_str(
        raw: &str,
        fallback_name: &str,
        base_dir: Option<&Path>,
    ) -> Option<AgentDef> {
        let (frontmatter, body) = crate::memory::parse_frontmatter(raw);

        let name = frontmatter
            .get("name")
            .cloned()
            .unwrap_or_else(|| fallback_name.to_string());

        let description = frontmatter.get("description").cloned().unwrap_or_default();
        let model = frontmatter.get("model").cloned();
        let color = frontmatter.get("color").cloned();
        let permission_mode = frontmatter
            .get("permissionMode")
            .or_else(|| frontmatter.get("permission_mode"))
            .cloned();
        let isolation = frontmatter.get("isolation").cloned();

        let max_iterations = frontmatter
            .get("maxTurns")
            .or_else(|| frontmatter.get("max_iterations"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(default_max_iterations());

        let tools = frontmatter
            .get("tools")
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let disallowed_tools = frontmatter
            .get("disallowedTools")
            .or_else(|| frontmatter.get("disallowed_tools"))
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let split_list = |s: &str| -> Vec<String> {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        };
        let skills = frontmatter
            .get("skills")
            .map(|s| split_list(s))
            .unwrap_or_default();
        let mcp = frontmatter
            .get("mcp")
            .or_else(|| frontmatter.get("mcpServers"))
            .map(|s| split_list(s))
            .unwrap_or_default();

        // `output_schema` / `input_schema`: single-line inline JSON
        // (`{...}`/`[...]`) or a path (relative to `base_dir`) to a
        // `.json` file. Unresolvable / malformed values parse to `None`
        // here; `thclaws agent validate` surfaces them as errors.
        let resolve_schema = |keys: &[&str]| -> Option<serde_json::Value> {
            let v = keys.iter().find_map(|k| frontmatter.get(*k))?.trim();
            if v.is_empty() {
                return None;
            }
            if v.starts_with('{') || v.starts_with('[') {
                serde_json::from_str(v).ok()
            } else {
                let p = base_dir?.join(v);
                serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()
            }
        };
        let output_schema = resolve_schema(&["output_schema", "outputSchema"]);
        let input_schema = resolve_schema(&["input_schema", "inputSchema"]);

        let write_paths = frontmatter
            .get("writePaths")
            .or_else(|| frontmatter.get("write_paths"))
            .map(|s| split_list(s))
            .unwrap_or_default();

        Some(AgentDef {
            name,
            description,
            instructions: body.trim().to_string(),
            model,
            max_iterations,
            tools,
            disallowed_tools,
            skills,
            mcp,
            color,
            isolation,
            permission_mode,
            output_schema,
            input_schema,
            write_paths,
        })
    }

    pub fn load_from_path(path: &PathBuf) -> Self {
        if !path.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Parse a single agent `.md` file (frontmatter + body) into an
    /// [`AgentDef`], resolving `output_schema` / `input_schema` paths
    /// relative to the file's directory. Public so `thclaws agent
    /// validate` can lint a folder's defs without loading the whole
    /// config from the standard dirs.
    pub fn parse_md_file(path: &Path) -> Option<AgentDef> {
        Self::parse_agent_md(path)
    }

    pub fn get(&self, name: &str) -> Option<&AgentDef> {
        self.agents.iter().find(|a| a.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.agents.iter().map(|a| a.name.as_str()).collect()
    }

    pub fn as_map(&self) -> HashMap<String, AgentDef> {
        self.agents
            .iter()
            .map(|a| (a.name.clone(), a.clone()))
            .collect()
    }

    /// Locate the on-disk `.md` file that backs `name`, if any. Walks
    /// the same standard dirs as [`agent_dirs`] and returns the
    /// highest-priority existing file (project wins over user, same
    /// order load uses). Returns `None` for names that only exist as a
    /// compiled-in built-in (e.g. `translator`) — those have no source
    /// file on disk until the user saves a project override.
    pub fn find_on_disk(name: &str) -> Option<PathBuf> {
        let mut found = None;
        for dir in Self::agent_dirs() {
            let candidate = dir.join(format!("{name}.md"));
            if candidate.is_file() {
                found = Some(candidate); // later dirs (higher priority) overwrite
            }
        }
        found
    }

    /// Path a project-scoped agent def is saved to: `.thclaws/agents/<name>.md`
    /// relative to the current working directory. This is the single
    /// write target for the GUI editor — edits to a user-scoped or
    /// built-in agent land here as a project override.
    pub fn project_agent_path(name: &str) -> PathBuf {
        PathBuf::from(".thclaws/agents").join(format!("{name}.md"))
    }
}

/// Validate an agent name for use as a `.md` filename. Accepts
/// non-empty strings of ASCII alphanumerics plus `-` / `_` — enough
/// for real agent names while rejecting path separators, `..`, and
/// other traversal vectors. Returns the trimmed name on success.
pub fn sanitize_agent_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    if name.is_empty() {
        return None;
    }
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Some(name.to_string())
    } else {
        None
    }
}

impl AgentDef {
    /// Render this def back to a markdown file body (YAML frontmatter +
    /// instructions). Used as the starting point when the GUI editor
    /// opens a built-in (or otherwise file-less) agent — the user edits
    /// the reconstruction and saves it as a project override. Only
    /// non-empty / non-default fields are emitted to keep the file
    /// readable.
    pub fn to_markdown(&self) -> String {
        let mut fm = String::from("---\n");
        fm.push_str(&format!("name: {}\n", self.name));
        if !self.description.is_empty() {
            fm.push_str(&format!("description: {}\n", self.description));
        }
        if let Some(m) = &self.model {
            fm.push_str(&format!("model: {m}\n"));
        }
        if !self.tools.is_empty() {
            fm.push_str(&format!("tools: {}\n", self.tools.join(", ")));
        }
        if !self.disallowed_tools.is_empty() {
            fm.push_str(&format!(
                "disallowedTools: {}\n",
                self.disallowed_tools.join(", ")
            ));
        }
        if !self.skills.is_empty() {
            fm.push_str(&format!("skills: {}\n", self.skills.join(", ")));
        }
        if !self.mcp.is_empty() {
            fm.push_str(&format!("mcp: {}\n", self.mcp.join(", ")));
        }
        if let Some(p) = &self.permission_mode {
            fm.push_str(&format!("permissionMode: {p}\n"));
        }
        if self.max_iterations != default_max_iterations() {
            fm.push_str(&format!("maxTurns: {}\n", self.max_iterations));
        }
        if let Some(c) = &self.color {
            fm.push_str(&format!("color: {c}\n"));
        }
        if let Some(i) = &self.isolation {
            fm.push_str(&format!("isolation: {i}\n"));
        }
        if let Some(s) = &self.output_schema {
            if let Ok(j) = serde_json::to_string(s) {
                fm.push_str(&format!("output_schema: {j}\n"));
            }
        }
        if let Some(s) = &self.input_schema {
            if let Ok(j) = serde_json::to_string(s) {
                fm.push_str(&format!("input_schema: {j}\n"));
            }
        }
        if !self.write_paths.is_empty() {
            fm.push_str(&format!("writePaths: {}\n", self.write_paths.join(", ")));
        }
        fm.push_str("---\n\n");
        fm.push_str(&self.instructions);
        if !self.instructions.ends_with('\n') {
            fm.push('\n');
        }
        fm
    }
}

// ── Marketplace install (single-.md subagent) ───────────────────────

/// Install target dir for agent defs: project `.thclaws/agents` or
/// user `~/.config/thclaws/agents` (default).
fn agents_target_root(project_scope: bool) -> crate::Result<PathBuf> {
    if project_scope {
        Ok(std::env::current_dir()
            .map_err(|e| crate::Error::Tool(format!("cwd: {e}")))?
            .join(".thclaws/agents"))
    } else {
        crate::util::home_dir()
            .ok_or_else(|| crate::Error::Tool("cannot locate user home directory".into()))
            .map(|h| h.join(format!(".config/{}/agents", crate::profile::app_dir_name())))
    }
}

/// Resolve a `/subagent install` argument: a bare marketplace name →
/// its `install_url` (rejecting `linked-only`), or a URL/path passed
/// through unchanged. Returns `(install_url, abort_message)`; a
/// non-`None` message means stop and show it to the user. Mirrors
/// `repl::resolve_skill_install_target`.
pub fn resolve_subagent_install_target(arg: &str) -> (String, Option<String>) {
    let arg = arg.trim();
    let looks_like_url = arg.contains("://")
        || arg.starts_with("git@")
        || arg.starts_with('/')
        || arg.starts_with("./")
        || arg.starts_with("../")
        || arg.ends_with(".zip")
        || arg.ends_with(".md");
    if looks_like_url {
        return (arg.to_string(), None);
    }
    let mp = crate::marketplace::load();
    match mp.find_subagent(arg) {
        Some(entry) => match &entry.install_url {
            Some(url) if !url.is_empty() => (url.clone(), None),
            _ => (
                String::new(),
                Some(format!(
                    "'{arg}' is linked-only — install from upstream: {}",
                    if entry.homepage.is_empty() {
                        "(no homepage)"
                    } else {
                        &entry.homepage
                    }
                )),
            ),
        },
        None => (
            String::new(),
            Some(format!(
                "no subagent named '{arg}' in marketplace and not a URL — try /subagent search <query> or pass a git URL"
            )),
        ),
    }
}

fn is_md(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}

/// Pick the single agent `.md` in `dir`, ignoring common repo docs
/// (README/LICENSE/…). Errors when zero or many candidates remain.
fn single_md_in_dir(dir: &Path) -> crate::Result<PathBuf> {
    let mut mds: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| crate::Error::Tool(format!("read {}: {e}", dir.display())))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_md(p))
        .collect();
    mds.retain(|p| {
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        !matches!(
            stem.as_str(),
            "readme" | "license" | "changelog" | "contributing"
        )
    });
    match mds.len() {
        1 => Ok(mds.pop().unwrap()),
        0 => Err(crate::Error::Tool(
            "no agent `.md` found in the source — point the install URL at the file \
             (e.g. …#main:agents/<name>.md)"
                .into(),
        )),
        _ => Err(crate::Error::Tool(format!(
            "multiple .md files found ({}) — specify the file via #<branch>:<path/to/agent.md>",
            mds.len()
        ))),
    }
}

/// Resolve the agent `.md` inside a staged checkout/extract. With a
/// `subpath`, resolve it (a `.md` file, or a dir holding one). Without
/// one, accept a single `.md` at the root.
fn locate_agent_md(root: &Path, subpath: Option<&str>) -> crate::Result<PathBuf> {
    if let Some(sub) = subpath {
        let cand = root.join(sub);
        if cand.is_file() && is_md(&cand) {
            return Ok(cand);
        }
        if cand.is_dir() {
            return single_md_in_dir(&cand);
        }
        return Err(crate::Error::Tool(format!(
            "subpath '{sub}' not found (or not a .md / dir) in the source"
        )));
    }
    single_md_in_dir(root)
}

/// Install a single agent-def `.md` from a git URL or `.zip`. Mirrors
/// [`crate::skills::install_from_url`] but lands one file in the agents
/// dir (project or user scope). Reuses the skills zip/git helpers.
/// Returns human-readable report lines.
pub async fn install_subagent_from_url(
    url: &str,
    override_name: Option<&str>,
    project_scope: bool,
) -> crate::Result<Vec<String>> {
    crate::marketplace::ensure_fresh().await;
    if !crate::marketplace::install_url_allowed(url) {
        return Err(crate::Error::Tool(crate::marketplace::install_refusal(
            "agent",
        )));
    }
    if let crate::policy::AllowDecision::Denied { reason } = crate::policy::check_url(url) {
        return Err(crate::Error::Tool(format!(
            "subagent install blocked by org policy: {reason}"
        )));
    }
    let target_root = agents_target_root(project_scope)?;
    std::fs::create_dir_all(&target_root)
        .map_err(|e| crate::Error::Tool(format!("mkdir {}: {e}", target_root.display())))?;

    // Stage under the target root, then copy just the resolved `.md`.
    let stage = target_root.join(format!(
        ".thclaws-install-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let (md_src, source_label) = if crate::skills::is_zip_url(url) {
        let bytes = crate::skills::download_zip(url).await?;
        if let Err(e) = std::fs::create_dir_all(&stage)
            .map_err(|e| crate::Error::Tool(format!("mkdir stage: {e}")))
        {
            return Err(e);
        }
        if let Err(e) = crate::skills::extract_zip(&bytes, &stage) {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e);
        }
        let root = crate::skills::single_wrapper_subdir(&stage).unwrap_or_else(|| stage.clone());
        (locate_agent_md(&root, None), url.to_string())
    } else {
        let (base_url, branch, subpath) = crate::skills::parse_git_subpath(url);
        let mut args: Vec<String> = vec!["clone".into(), "--depth".into(), "1".into()];
        if let Some(b) = &branch {
            args.push("--branch".into());
            args.push(b.clone());
        }
        args.push(base_url.clone());
        args.push(stage.to_string_lossy().into_owned());
        let out = std::process::Command::new("git")
            .args(&args)
            .output()
            .map_err(|e| crate::Error::Tool(format!("spawn git: {e}")))?;
        if !out.status.success() {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(crate::Error::Tool(format!(
                "git clone failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        (locate_agent_md(&stage, subpath.as_deref()), base_url)
    };

    let md_src = match md_src {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e);
        }
    };

    // Name precedence: override > frontmatter `name:` > file stem.
    let raw = match std::fs::read_to_string(&md_src) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(crate::Error::Tool(format!(
                "read {}: {e}",
                md_src.display()
            )));
        }
    };
    let (fm, _body) = crate::memory::parse_frontmatter(&raw);
    let derived = override_name
        .map(str::to_string)
        .or_else(|| fm.get("name").cloned())
        .or_else(|| md_src.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let Some(name) = sanitize_agent_name(&derived) else {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(crate::Error::Tool(format!(
            "could not derive a valid agent name from '{source_label}' — pass one explicitly: \
             /subagent install {url} <name>"
        )));
    };

    let dest = target_root.join(format!("{name}.md"));
    if dest.exists() {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(crate::Error::Tool(format!(
            "'{}' already exists — remove it first or pick a different name",
            dest.display()
        )));
    }
    if let Err(e) = std::fs::copy(&md_src, &dest) {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(crate::Error::Tool(format!("copy into place: {e}")));
    }
    let _ = std::fs::remove_dir_all(&stage);
    Ok(vec![
        format!("fetched {source_label} → {}", dest.display()),
        format!("installed subagent '{name}'"),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_from_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("agents.json");
        std::fs::write(
            &path,
            r#"{"agents": [
                {"name": "researcher", "instructions": "Research things", "max_iterations": 5},
                {"name": "coder", "instructions": "Write code", "tools": ["Read", "Write", "Edit"]}
            ]}"#,
        )
        .unwrap();

        let config = AgentDefsConfig::load_from_path(&path);
        assert_eq!(config.agents.len(), 2);
        assert_eq!(config.get("researcher").unwrap().max_iterations, 5);
        assert_eq!(
            config.get("coder").unwrap().tools,
            vec!["Read", "Write", "Edit"]
        );
        assert!(config.get("nonexistent").is_none());
    }

    #[test]
    fn missing_file_returns_default() {
        let config = AgentDefsConfig::load_from_path(&PathBuf::from("/nonexistent/agents.json"));
        assert!(config.agents.is_empty());
    }

    #[test]
    fn names_lists_all() {
        let config = AgentDefsConfig {
            agents: vec![
                AgentDef {
                    name: "a".into(),
                    ..Default::default()
                },
                AgentDef {
                    name: "b".into(),
                    ..Default::default()
                },
            ],
        };
        assert_eq!(config.names(), vec!["a", "b"]);
    }

    #[test]
    fn parse_agent_md_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("researcher.md");
        std::fs::write(
            &path,
            "\
---
name: researcher
description: Researches topics
model: claude-sonnet-4-5
tools: Read, Grep, Glob, WebSearch
maxTurns: 20
color: blue
---
You are a research agent. Search thoroughly and report findings.
",
        )
        .unwrap();

        let agent = AgentDefsConfig::parse_agent_md(&path).unwrap();
        assert_eq!(agent.name, "researcher");
        assert_eq!(agent.description, "Researches topics");
        assert_eq!(agent.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(agent.tools, vec!["Read", "Grep", "Glob", "WebSearch"]);
        assert_eq!(agent.max_iterations, 20);
        assert_eq!(agent.color.as_deref(), Some("blue"));
        assert!(agent.instructions.contains("research agent"));
    }

    #[test]
    fn parse_agent_md_name_from_filename() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("backend.md");
        std::fs::write(
            &path,
            "\
---
description: Backend developer
---
Build REST APIs.
",
        )
        .unwrap();

        let agent = AgentDefsConfig::parse_agent_md(&path).unwrap();
        assert_eq!(agent.name, "backend");
        assert_eq!(agent.instructions, "Build REST APIs.");
    }

    #[test]
    fn load_md_dir_no_clobber_keeps_existing() {
        let dir = tempdir().unwrap();

        // A project-level agent already in the config.
        let mut config = AgentDefsConfig {
            agents: vec![AgentDef {
                name: "coder".into(),
                instructions: "project version".into(),
                ..Default::default()
            }],
        };

        // A plugin dir with an agent of the same name PLUS a new one.
        let plugin_dir = dir.path().join("plugin-agents");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("coder.md"),
            "\
---
name: coder
---
plugin version (should NOT win)
",
        )
        .unwrap();
        std::fs::write(
            plugin_dir.join("reviewer.md"),
            "\
---
name: reviewer
---
plugin-only reviewer
",
        )
        .unwrap();

        config.load_md_dir_no_clobber(&plugin_dir);
        assert_eq!(config.get("coder").unwrap().instructions, "project version");
        assert_eq!(
            config.get("reviewer").unwrap().instructions,
            "plugin-only reviewer"
        );
    }

    #[test]
    fn seed_builtins_includes_translator() {
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();
        let translator = config
            .get("translator")
            .expect("built-in translator agent should be seeded");
        assert_eq!(translator.name, "translator");
        assert!(!translator.instructions.is_empty());
        // No `model:` in frontmatter — translator inherits the
        // session's active model (so cross-provider users don't
        // hit "model not found" 404s when the hard-coded model
        // doesn't match their session's provider).
        assert_eq!(translator.model.as_deref(), None);
        // Tool whitelist captured — translator has no Bash, no KMS,
        // no Task. Just file I/O.
        assert!(translator.tools.iter().any(|t| t == "Read"));
        assert!(translator.tools.iter().any(|t| t == "Write"));
        assert!(!translator.tools.iter().any(|t| t == "Bash"));
    }

    /// settings.json `translator_subagent_model` swaps the embedded
    /// `gpt-4.1` for the override value before the AgentDef reaches
    /// the factory. Disk-resident user agents at
    /// `.thclaws/agents/translator.md` would have replaced the
    /// AgentDef during the prior load_md_dir pass, so this only
    /// runs against the embedded built-in.
    #[test]
    fn apply_builtin_subagent_overrides_replaces_translator_model() {
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();

        let mut app_config = crate::config::AppConfig::default();
        app_config.translator_subagent_model = Some("claude-sonnet-4-6".into());

        config.apply_builtin_subagent_overrides(&app_config);
        let translator = config.get("translator").unwrap();
        assert_eq!(translator.model.as_deref(), Some("claude-sonnet-4-6"));
    }

    /// Absent override leaves the embedded default in place.
    #[test]
    fn apply_builtin_subagent_overrides_no_op_when_absent() {
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();

        let app_config = crate::config::AppConfig::default();
        config.apply_builtin_subagent_overrides(&app_config);
        let translator = config.get("translator").unwrap();
        // No override + no frontmatter `model:` → None (inherits
        // session model at build time).
        assert_eq!(translator.model.as_deref(), None);
    }

    #[test]
    fn seed_builtins_includes_folder_indexer() {
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();
        let indexer = config
            .get("folder-indexer")
            .expect("built-in folder-indexer agent should be seeded");
        assert_eq!(indexer.name, "folder-indexer");
        assert!(!indexer.instructions.is_empty());
        // Reads + the deterministic index writer. No Write/Edit: index.md and
        // the fingerprint cache are FolderIndex's alone to write, and the
        // indexed files must come back unchanged.
        assert!(indexer.tools.iter().any(|t| t == "FolderIndex"));
        assert!(indexer.tools.iter().any(|t| t == "Read"));
        assert!(indexer.tools.iter().any(|t| t == "PdfRead"));
        assert!(!indexer.tools.iter().any(|t| t == "Write"));
        assert!(!indexer.tools.iter().any(|t| t == "Edit"));
        assert!(!indexer.tools.iter().any(|t| t == "Bash"));
    }

    #[test]
    fn seed_builtins_includes_kms_linker() {
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();
        let linker = config
            .get("kms-linker")
            .expect("built-in kms-linker agent should be seeded");
        assert_eq!(linker.name, "kms-linker");
        assert!(!linker.instructions.is_empty());
        // Tool whitelist: KMS read/write surface only — no Bash, no
        // KmsDelete (the operating procedure forbids deletion).
        assert!(linker.tools.iter().any(|t| t == "KmsRead"));
        assert!(linker.tools.iter().any(|t| t == "KmsSearch"));
        assert!(linker.tools.iter().any(|t| t == "KmsWrite"));
        assert!(linker.tools.iter().any(|t| t == "KmsAppend"));
        assert!(!linker.tools.iter().any(|t| t == "KmsDelete"));
        assert!(!linker.tools.iter().any(|t| t == "Bash"));
    }

    #[test]
    fn seed_builtins_includes_kms_reconcile() {
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();
        let reconcile = config
            .get("kms-reconcile")
            .expect("built-in kms-reconcile agent should be seeded");
        assert_eq!(reconcile.name, "kms-reconcile");
        assert!(!reconcile.instructions.is_empty());
        // Tool whitelist: same shape as kms-linker — KMS surface only,
        // no KmsDelete (reconcile preserves history; rewrites with
        // History sections, never silently drops claims), no Bash.
        assert!(reconcile.tools.iter().any(|t| t == "KmsRead"));
        assert!(reconcile.tools.iter().any(|t| t == "KmsSearch"));
        assert!(reconcile.tools.iter().any(|t| t == "KmsWrite"));
        assert!(reconcile.tools.iter().any(|t| t == "KmsAppend"));
        assert!(reconcile.tools.iter().any(|t| t == "TodoWrite"));
        assert!(!reconcile.tools.iter().any(|t| t == "KmsDelete"));
        assert!(!reconcile.tools.iter().any(|t| t == "Bash"));
        // Procedure-defining keywords from the body.
        assert!(reconcile.instructions.contains("History"));
        assert!(reconcile.instructions.contains("Conflict"));
    }

    #[test]
    fn seed_builtins_includes_kms_maintain() {
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();
        let maintain = config
            .get("kms-maintain")
            .expect("built-in kms-maintain agent should be seeded");
        assert_eq!(maintain.name, "kms-maintain");
        assert!(!maintain.instructions.is_empty());
        // KMS surface + Glob (needed to read the live session set for the
        // source-reconciliation stage). Still no KmsDelete — maintain never
        // deletes a page, only scrubs dead refs inside one.
        assert!(maintain.tools.iter().any(|t| t == "KmsRead"));
        assert!(maintain.tools.iter().any(|t| t == "KmsWrite"));
        assert!(maintain.tools.iter().any(|t| t == "Glob"));
        assert!(maintain.tools.iter().any(|t| t == "TodoWrite"));
        assert!(!maintain.tools.iter().any(|t| t == "KmsDelete"));
        assert!(!maintain.tools.iter().any(|t| t == "Bash"));
    }

    #[test]
    fn seed_builtins_includes_dream() {
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();
        let dream = config
            .get("dream")
            .expect("built-in dream agent should be seeded");
        assert_eq!(dream.name, "dream");
        assert!(!dream.instructions.is_empty());
        // Tool whitelist must be wired up so the dream agent can mutate
        // KMS — bare-bones smoke check.
        assert!(dream.tools.iter().any(|t| t == "KmsDelete"));
    }

    #[test]
    fn user_dream_md_overrides_builtin() {
        let dir = tempdir().unwrap();
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();
        let builtin_instructions = config.get("dream").unwrap().instructions.clone();

        let md_dir = dir.path().join("agents");
        std::fs::create_dir_all(&md_dir).unwrap();
        std::fs::write(
            md_dir.join("dream.md"),
            "\
---
name: dream
---
custom user dream prompt
",
        )
        .unwrap();

        config.load_md_dir(&md_dir);
        let dream = config.get("dream").unwrap();
        assert_eq!(dream.instructions, "custom user dream prompt");
        assert_ne!(dream.instructions, builtin_instructions);
    }

    #[test]
    fn load_md_dir_overrides_json() {
        let dir = tempdir().unwrap();

        // JSON agent.
        let mut config = AgentDefsConfig {
            agents: vec![AgentDef {
                name: "coder".into(),
                instructions: "old instructions".into(),
                ..Default::default()
            }],
        };

        // MD agent with same name overrides.
        let md_dir = dir.path().join("agents");
        std::fs::create_dir_all(&md_dir).unwrap();
        std::fs::write(
            md_dir.join("coder.md"),
            "\
---
name: coder
---
new instructions
",
        )
        .unwrap();

        config.load_md_dir(&md_dir);
        assert_eq!(
            config.get("coder").unwrap().instructions,
            "new instructions"
        );
    }

    #[test]
    fn sanitize_agent_name_rejects_traversal() {
        assert_eq!(
            sanitize_agent_name("researcher").as_deref(),
            Some("researcher")
        );
        assert_eq!(
            sanitize_agent_name("  kms-linker ").as_deref(),
            Some("kms-linker")
        );
        assert_eq!(sanitize_agent_name("a_b-c1").as_deref(), Some("a_b-c1"));
        assert!(sanitize_agent_name("").is_none());
        assert!(sanitize_agent_name("   ").is_none());
        assert!(sanitize_agent_name("../etc/passwd").is_none());
        assert!(sanitize_agent_name("a/b").is_none());
        assert!(sanitize_agent_name("a.md").is_none());
    }

    #[test]
    fn to_markdown_roundtrips_through_parser() {
        let def = AgentDef {
            name: "reviewer".into(),
            description: "Read-only review".into(),
            instructions: "You are a reviewer.\nFlag issues.".into(),
            model: Some("claude-haiku-4-5".into()),
            max_iterations: 20,
            tools: vec!["Read".into(), "Glob".into(), "Grep".into()],
            disallowed_tools: vec!["Bash".into()],
            skills: vec!["pdf".into(), "xlsx".into()],
            mcp: vec!["pinn-ai".into()],
            color: Some("cyan".into()),
            isolation: None,
            permission_mode: Some("auto".into()),
            output_schema: Some(serde_json::json!({"type": "object"})),
            input_schema: None,
            write_paths: vec![".thclaws/state/kms/**".into()],
        };
        let md = def.to_markdown();
        let parsed = AgentDefsConfig::parse_agent_md_str(&md, "fallback", None).unwrap();
        assert_eq!(parsed.name, "reviewer");
        assert_eq!(parsed.description, "Read-only review");
        assert_eq!(parsed.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(parsed.tools, vec!["Read", "Glob", "Grep"]);
        assert_eq!(parsed.disallowed_tools, vec!["Bash"]);
        assert_eq!(parsed.skills, vec!["pdf", "xlsx"]);
        assert_eq!(parsed.mcp, vec!["pinn-ai"]);
        assert_eq!(parsed.max_iterations, 20);
        assert_eq!(parsed.color.as_deref(), Some("cyan"));
        assert_eq!(parsed.permission_mode.as_deref(), Some("auto"));
        // 47.1: output_schema round-trips as single-line inline JSON.
        assert_eq!(
            parsed.output_schema,
            Some(serde_json::json!({"type": "object"}))
        );
        // 47.2: writePaths round-trips.
        assert_eq!(
            parsed.write_paths,
            vec![".thclaws/state/kms/**".to_string()]
        );
        assert!(parsed.instructions.contains("Flag issues."));
    }

    /// 47.1: a path-based `output_schema` is resolved relative to the
    /// def's `.md` directory and parsed into the AgentDef.
    #[test]
    fn output_schema_path_resolves_relative_to_md_dir() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents");
        let schemas = dir.path().join("schemas");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::create_dir_all(&schemas).unwrap();
        std::fs::write(
            schemas.join("planner.json"),
            r#"{"type":"object","required":["subtopics"]}"#,
        )
        .unwrap();
        std::fs::write(
            agents.join("planner.md"),
            "---\nname: planner\noutput_schema: ../schemas/planner.json\n---\nplan\n",
        )
        .unwrap();
        let def = AgentDefsConfig::parse_md_file(&agents.join("planner.md")).unwrap();
        assert_eq!(
            def.output_schema,
            Some(serde_json::json!({"type": "object", "required": ["subtopics"]}))
        );
    }

    #[test]
    fn to_markdown_omits_default_max_iterations() {
        let def = AgentDef {
            name: "x".into(),
            instructions: "body".into(),
            ..Default::default()
        };
        let md = def.to_markdown();
        assert!(
            !md.contains("maxTurns"),
            "default maxTurns should be omitted: {md}"
        );
    }

    #[test]
    fn project_agent_path_is_under_thclaws() {
        let p = AgentDefsConfig::project_agent_path("reviewer");
        assert_eq!(p, PathBuf::from(".thclaws/agents/reviewer.md"));
    }

    #[test]
    fn resolve_subagent_target_passes_through_urls() {
        // URL-ish args short-circuit (no marketplace lookup).
        for arg in [
            "https://x.com/r.git#main:agents/reviewer.md",
            "git@github.com:o/r.git",
            "./local/reviewer.md",
            "https://x.com/pack.zip",
        ] {
            let (url, abort) = resolve_subagent_install_target(arg);
            assert!(abort.is_none(), "{arg} should pass through");
            assert_eq!(url, arg);
        }
    }

    #[test]
    fn locate_agent_md_picks_single_md_ignoring_readme() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "# repo").unwrap();
        std::fs::write(
            dir.path().join("reviewer.md"),
            "---\nname: reviewer\n---\nbody",
        )
        .unwrap();
        let got = locate_agent_md(dir.path(), None).unwrap();
        assert_eq!(got.file_name().unwrap(), "reviewer.md");
    }

    #[test]
    fn locate_agent_md_subpath_file_and_errors() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("reviewer.md"), "x").unwrap();
        // Subpath pointing straight at the file resolves.
        let got = locate_agent_md(dir.path(), Some("agents/reviewer.md")).unwrap();
        assert_eq!(got.file_name().unwrap(), "reviewer.md");
        // Missing subpath errors.
        assert!(locate_agent_md(dir.path(), Some("nope/x.md")).is_err());
        // Bare root with no .md errors.
        assert!(single_md_in_dir(dir.path()).is_err());
    }
}
