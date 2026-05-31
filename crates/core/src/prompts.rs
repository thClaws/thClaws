//! Configurable prompt templates.
//!
//! Every user-facing prompt used by the agent can be overridden by dropping a
//! markdown file into `.thclaws/prompt/<name>.md` (project level) or
//! `~/.config/thclaws/prompt/<name>.md` (user level). Project wins over user;
//! both win over the built-in default.
//!
//! Templates support `{variable}` substitution. Unknown placeholders are left
//! untouched so users notice typos.

use std::path::PathBuf;

const DIR: &str = "prompt";

/// Built-in default templates. These are the bytes of the markdown files under
/// `src/default_prompts/`, embedded at compile time. The same files should
/// serve as the canonical reference for authors writing overrides into
/// `.thclaws/prompt/`.
pub mod defaults {
    pub const SYSTEM: &str = include_str!("default_prompts/system.md");
    pub const LEAD: &str = include_str!("default_prompts/lead.md");
    pub const AGENT_TEAM: &str = include_str!("default_prompts/agent_team.md");
    pub const SUBAGENT: &str = include_str!("default_prompts/subagent.md");
    pub const WORKTREE: &str = include_str!("default_prompts/worktree.md");
    pub const COMPACTION: &str = include_str!("default_prompts/compaction.md");
    pub const COMPACTION_SYSTEM: &str = include_str!("default_prompts/compaction_system.md");
    /// M6.29: audit-driven goal-continue prompt fired by `/goal continue`.
    /// Variables: {{ objective }}, {{ time_used_seconds }},
    /// {{ tokens_used }}, {{ token_budget }}, {{ remaining_tokens }},
    /// {{ iterations_done }}, {{ prior_audit }}.
    pub const GOAL_CONTINUE: &str = include_str!("default_prompts/goal_continue.md");
    /// Phase B1: budget-exhausted soft-stop prompt fired by `/goal continue`
    /// when `tokens_used >= budget_tokens`. Tells the model to wrap up
    /// (summarize progress, identify blockers, give the user a next step)
    /// instead of starting new substantive work. Mirrors codex's
    /// `budget_limit.md`. Variables: {{ objective }},
    /// {{ time_used_seconds }}, {{ tokens_used }}, {{ token_budget }},
    /// {{ iterations_done }}.
    pub const GOAL_BUDGET_LIMIT: &str = include_str!("default_prompts/goal_budget_limit.md");
    /// dev-plan/32 Stage B: system prompt that teaches the model the
    /// `thclaws.*` API and sandbox limits before it authors a workflow
    /// script. Loaded by `crate::workflow::script::author`. No template
    /// variables — the user goal is passed as the user message.
    pub const WORKFLOW_AUTHOR: &str = include_str!("default_prompts/workflow_author.md");
}

fn project_path(name: &str) -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".thclaws")
        .join(DIR)
        .join(format!("{name}.md"))
}

fn user_path(name: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("thclaws").join(DIR).join(format!("{name}.md")))
}

/// Load a prompt template by name. Returns the override content (project →
/// user) if present, otherwise the built-in default string. Branding
/// placeholders (`{product}`, `{support_email}`) are substituted before
/// returning so any prompt — built-in default, project override, user
/// override — picks up the active branding without per-callsite work.
pub fn load(name: &str, default: &str) -> String {
    let raw = if let Ok(s) = std::fs::read_to_string(project_path(name)) {
        s
    } else if let Some(p) = user_path(name) {
        std::fs::read_to_string(p).unwrap_or_else(|_| default.to_string())
    } else {
        default.to_string()
    };
    crate::branding::apply_template(&raw)
}

/// Replace `{key}` occurrences with the corresponding values. Unknown
/// placeholders are left in place so typos are visible.
pub fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// Load-and-render in one call.
pub fn render_named(name: &str, default: &str, vars: &[(&str, &str)]) -> String {
    render(&load(name, default), vars)
}

// ─── Unified system-prompt assembly ─────────────────────────────────────
//
// Every entry point that builds an Agent (REPL, print-mode, GUI worker,
// `--serve`, agent_runtime HTTP) MUST go through [`build_full_system_prompt`].
// Pre-fix, four separate inline assemblies had drifted apart — the GUI worker
// included "External services" / "Documents" / "Team grounding" sections that
// CLI didn't, and CLI's skill catalog ended with a "Slash-command shortcut"
// paragraph that the GUI didn't. The model saw different text on different
// surfaces despite running over the same project. Audit: dev-plan/35 followup.
//
// Adding a new entry point that bypasses this builder is a bug; the snapshot
// tests below pin the shared body across all SurfaceHints variants.

/// Which UX surface the prompt is for. Drives only the surface-specific
/// addendum at the end of the skills section — everything else is the same.
///
/// `Repl` — interactive CLI. Gets the "Slash-command shortcut: if a user
///          message begins with `/<skill-name>`…" priming after the skill
///          catalog. (The shell_dispatch / repl rewrite layer ALSO handles
///          `/<skill-name>` deterministically; the priming is a belt-and-
///          braces nudge for model-driven calls e.g. when the user types
///          natural-language "use the foo skill".)
///
/// `Gui`  — desktop GUI worker + `--serve` web frontend. No priming — slash
///          commands have explicit autocomplete UX in the chat input; the
///          model doesn't need verbal guidance to recognise them.
///
/// `Headless` — print mode (`thclaws -p`), agent_runtime HTTP API. No
///          interactive slash UX at all; no priming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceHints {
    Repl,
    Gui,
    Headless,
}

/// Assemble the full system prompt every Agent surface should send to the
/// provider. Caller passes:
///
/// - `config`: live AppConfig. Drives `skills_listing_strategy` for the
///   skill section. `config.kms_active` drives the KMS section.
/// - `cwd`: project root the prompt should reference. Used by
///   [`crate::context::ProjectContext::discover`] for the AGENTS.md /
///   CLAUDE.md merge. Pass `&state.cwd` after any `ChangeCwd` flip, not
///   `std::env::current_dir()` directly.
/// - `skill_store`: `None` skips the "# Available skills" section
///   entirely (print mode preserves this — it does not discover skills
///   to keep one-shot startup fast). `Some(&store)` renders the section
///   per `config.skills_listing_strategy`.
/// - `mcp_instructions`: slice of `(server_name, instructions)`
///   pairs harvested from each [`crate::mcp::McpClient::instructions`]
///   after `initialize`. Empty slice → skip the section. Caller is
///   responsible for filtering out servers whose `instructions()`
///   returned `None`.
/// - `surface`: see [`SurfaceHints`].
///
/// Section order (canonical, identical across all surfaces):
///   1. base system prompt (`prompts::load("system", …)` + ProjectContext)
///   2. `# Memory` (if [`crate::memory::MemoryStore`] has any entries)
///   3. KMS attachments (if `config.kms_active` is non-empty)
///   4. `# External services` (HAL keys + WebSearch backend hint)
///   5. `# MCP server instructions` (per-server briefings from each
///      MCP `InitializeResult.instructions` — `mcp_instructions` slice)
///   6. `# Document & spreadsheet generation` (unconditional)
///   7. `# Collaboration primitives` (Subagent + Agent Teams + WorkflowRun
///      one-line catalog, with team line varying on `teamEnabled`)
///   8. Team grounding (only when `teamEnabled` or `agent/*` provider —
///      full playbook; the Collaboration section above just one-lines it)
///   9. `# Available skills` (only when `skill_store: Some`)
///  10. Repl-only slash-command-shortcut priming (only when `Surface::Repl`
///      AND `skill_store: Some(&store)` with non-empty `store.skills`)
pub fn build_full_system_prompt(
    config: &crate::config::AppConfig,
    cwd: &std::path::Path,
    skill_store: Option<&crate::skills::SkillStore>,
    mcp_instructions: &[(String, String)],
    surface: SurfaceHints,
) -> String {
    let ctx =
        crate::context::ProjectContext::discover(cwd).unwrap_or(crate::context::ProjectContext {
            cwd: cwd.to_path_buf(),
            git: None,
            project_instructions: None,
        });
    let system_fallback = if config.system_prompt.is_empty() {
        defaults::SYSTEM
    } else {
        config.system_prompt.as_str()
    };
    let base_prompt = load("system", system_fallback);
    let mut system = ctx.build_system_prompt(&base_prompt);

    // (2) Memory
    if let Some(store) =
        crate::memory::MemoryStore::default_path().map(crate::memory::MemoryStore::new)
    {
        if let Some(mem) = store.system_prompt_section() {
            system.push_str("\n\n# Memory\n");
            system.push_str(&mem);
        }
    }

    // (3) KMS
    let kms_section = crate::kms::system_prompt_section(&config.kms_active);
    if !kms_section.is_empty() {
        system.push_str("\n\n");
        system.push_str(&kms_section);
    }

    // (4) External services
    let services_section = services_prompt_section();
    if !services_section.is_empty() {
        system.push_str("\n\n");
        system.push_str(&services_section);
    }

    // (5) MCP server instructions — per-server briefings from each
    // MCP `InitializeResult.instructions`. Slot sits between
    // Services and Documents because all three describe "what tools
    // exist and when to call them" — keep capability sections
    // clustered before Team (collaboration) and Skills (workflows).
    let mcp_section = mcp_instructions_section(mcp_instructions);
    if !mcp_section.is_empty() {
        system.push_str("\n\n");
        system.push_str(&mcp_section);
    }

    // (6) Documents (unconditional — tools always registered)
    let documents_section = documents_prompt_section();
    if !documents_section.is_empty() {
        system.push_str("\n\n");
        system.push_str(&documents_section);
    }

    // (7) Collaboration primitives — Subagent + Agent Teams +
    // WorkflowRun. Names all three decomposition tools in one place
    // with their gates, since each was previously surfaced via a
    // different mechanism (Subagent: tool-schema description only,
    // never mentioned in prompt; Agent Teams: ~70-line team_grounding
    // section when enabled, silent otherwise; WorkflowRun: invisible
    // to model entirely until this fix). The Team grounding section
    // below remains the full detailed playbook when teams are on.
    let team_enabled = crate::config::ProjectConfig::load()
        .and_then(|c| c.team_enabled)
        .unwrap_or(false);
    let collaboration_section = collaboration_primitives_section(team_enabled);
    if !collaboration_section.is_empty() {
        system.push_str("\n\n");
        system.push_str(&collaboration_section);
    }

    // (8) Team grounding (config-gated inside the helper)
    let team_section = team_grounding_prompt(&config.model, team_enabled);
    if !team_section.is_empty() {
        system.push_str("\n\n");
        system.push_str(&team_section);
    }

    // (7) Skill catalog + (8) Repl-only priming
    if let Some(store) = skill_store {
        if !store.skills.is_empty() {
            append_skills_section(&mut system, store, config.skills_listing_strategy.as_str());
            if surface == SurfaceHints::Repl {
                append_repl_slash_shortcut_priming(&mut system);
            }
        }
    }

    system
}

/// REPL-only nudge that the model should treat `/<skill-name>` as an
/// explicit Skill invocation when the user types it in chat. The CLI's
/// own slash dispatcher (`repl.rs::run_repl` line ~5220) already rewrites
/// `/<known-skill>` into a model prompt before the agent runs, so this
/// text is belt-and-braces for the natural-language case ("use the foo
/// skill"). Kept Repl-only because GUI's slash chip UX makes the verbal
/// nudge redundant + distracting.
fn append_repl_slash_shortcut_priming(system: &mut String) {
    system.push_str(
        "\nReminder: if the user's request matches ANY skill trigger above, \
         call `Skill(name: \"...\")` FIRST.\n\n\
         Slash-command shortcut: if a user message begins with \
         `/<skill-name>` (matching one of the skills above), that IS \
         an explicit request to run that skill. Call \
         `Skill(name: \"<skill-name>\")` immediately, then follow its \
         instructions using any args that appeared after the name.\n",
    );
}

/// "External services" section — names HAL-backed tools + the active
/// WebSearch backend so the model reaches for the structured tools
/// instead of `Bash` + `curl`. Surfaces only services whose API key is
/// in the process env at call time (paste-a-key-mid-session lights up
/// on the next [`build_full_system_prompt`]).
///
/// Pre-fix the model defaulted to `WebFetch` for everything and never
/// reached for `WebScrape` / `YouTubeTranscript` even when HAL was
/// configured, because unfamiliar tool names in the long tools-param
/// list got glossed over. Moved here from `shared_session.rs` so CLI +
/// print + agent_runtime get the same nudge.
pub(crate) fn services_prompt_section() -> String {
    let mut bullets: Vec<String> = Vec::new();

    let hal_ok = std::env::var("HAL_API_KEY")
        .ok()
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false);
    if hal_ok {
        bullets.push(
            "**HAL Public API** (key set). \
             `WebFetch` now runs **both** a HAL headless-browser scrape **and** \
             a plain HTTP GET in parallel on every call, returning a single \
             combined response with each section clearly labelled (`[via HAL …]` \
             then `[via plain HTTP GET …]`). Pick the slice that answers your \
             question — HAL for SPA / JS-rendered / docs / blog content; plain \
             GET for JSON APIs / sitemaps / robots.txt / anything where the raw \
             body matters. Set `prefer_raw: true` on `WebFetch` to skip HAL \
             entirely when you know the URL is a JSON endpoint or similar \
             (saves wall-clock + tokens). Reach directly for `WebScrape` only \
             when you need advanced HAL parameters (`wait_for` CSS selector, \
             `scroll_to_bottom`, `remove_selectors`, `output_format`). Use \
             `YouTubeTranscript` for video captions (en/th preference by default)."
                .to_string(),
        );
    }

    let tavily_ok = std::env::var("TAVILY_API_KEY")
        .ok()
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false);
    let brave_ok = std::env::var("BRAVE_SEARCH_API_KEY")
        .ok()
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false);
    let backend_hint = match (tavily_ok, brave_ok) {
        (true, _) => "currently Tavily (best quality)",
        (false, true) => "currently Brave",
        (false, false) => "currently DuckDuckGo (no key set — paste a Tavily or Brave key in Settings for better results)",
    };
    bullets.push(format!(
        "**Web search**. `WebSearch` returns titles, URLs, and snippets \
         from the live web — {backend_hint}. Auto-picks the best \
         available backend at call time: Tavily → Brave → DuckDuckGo. \
         Each result starts with a `Source: <engine>` line — mention \
         the engine when summarising so the user knows result quality. \
         Reach for this instead of `Bash` + `curl` for any web lookup."
    ));

    if bullets.is_empty() {
        return String::new();
    }

    let mut out = String::from("# External services\n\n");
    for b in bullets {
        out.push_str("- ");
        out.push_str(&b);
        out.push('\n');
    }
    out
}

/// Render the "# Collaboration primitives" section — one-line catalog
/// of the three ways to decompose work, with the gating rule for each.
///
/// Why: pre-fix, the model was told about Agent Teams (via the dense
/// team_grounding_prompt) but Subagent (always-available `Task` tool)
/// and `WorkflowRun` (new model-callable workflow author + runner)
/// were surfaced ONLY via their tool-schema descriptions. The model
/// would default to one-shot Subagent calls for tasks where parallel
/// fan-out via WorkflowRun would have been a better fit, and it had
/// no top-level priming for when to pick Teams vs Subagent vs neither.
///
/// The team line swaps based on `team_enabled` so the model doesn't
/// see Team tool names it can't actually call when the feature is off.
/// The fuller team playbook below this section (rendered by
/// `team_grounding_prompt` when teams are on) stays unchanged.
pub(crate) fn collaboration_primitives_section(team_enabled: bool) -> String {
    let teams_line = if team_enabled {
        "**Agent Teams** — `TeamCreate` + `SpawnTeammate` start \
         persistent parallel teammates with optional worktree \
         isolation; `TeamTaskCreate` / `Claim` / `Complete` for the \
         shared task queue; `SendMessage` / `CheckInbox` for async \
         coordination. See the detailed playbook below."
    } else {
        "**Agent Teams** — disabled in this workspace \
         (`teamEnabled: false`). The Team* tools are NOT registered; \
         do not try to call them. For multi-step parallel work, \
         reach for Subagent or WorkflowRun above."
    };
    format!(
        "# Collaboration primitives\n\n\
         Three ways to decompose work — pick by shape, not size:\n\n\
         1. **Subagent** — the `Task` tool launches one scoped child \
         agent that returns a transcript when done. Always available. \
         Use for a single side-quest that would clutter history, a \
         read-only sweep, or a well-defined delegation. NOT for \
         parallel fan-out (one call = one child).\n\n\
         2. {teams_line}\n\n\
         3. **WorkflowRun** — `WorkflowRun(prompt: \"…\")` authors a \
         JavaScript orchestration script and runs it in a Boa \
         sandbox. Use for deterministic fan-out across N items, \
         retry loops, multistep pipelines with budget control, or \
         anything where you'd otherwise loop over Subagent calls. \
         Requires user approval per invocation. Nested WorkflowRun \
         calls (from inside a running workflow) are rejected — \
         orchestrate via `thclaws.subagent(...)` / \
         `thclaws.parallel(...)` inside the script instead.\n"
    )
}

/// Render the per-server "MCP server instructions" section from the
/// captured [`crate::mcp::McpClient::instructions`] strings. One
/// `## <server_name>` subsection per server, in caller-supplied
/// order (server-config order; stable, no sort surprises). Returns
/// empty when no server returned an `instructions` field — the
/// caller skips the whole section in that case.
///
/// Per MCP spec, `InitializeResult.instructions` is the server's
/// opportunity to brief the model about when / how to call its
/// tools. Pre-fix thClaws threw the string away — operators had to
/// duplicate the same guidance into AGENTS.md, and updates didn't
/// propagate. Surfacing here means installing a new MCP server (or
/// upgrading one) automatically updates the system prompt on the
/// next `rebuild_system_prompt` / process restart, with no
/// per-project copy-paste.
pub(crate) fn mcp_instructions_section(servers: &[(String, String)]) -> String {
    let filtered: Vec<&(String, String)> = servers
        .iter()
        .filter(|(_, instr)| !instr.trim().is_empty())
        .collect();
    if filtered.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "# MCP server instructions\n\n\
         The following MCP servers shipped usage guidance via their \
         `InitializeResult.instructions`. Treat these as authoritative \
         when calling the matching server's tools — they override your \
         default assumptions about how the tool works.\n",
    );
    for (name, instr) in filtered {
        out.push_str(&format!("\n## {name}\n\n"));
        out.push_str(instr.trim());
        out.push('\n');
    }
    out
}

/// Document/spreadsheet section — unconditional. Tools are always
/// registered in `ToolRegistry::with_builtins`; this section's job is
/// purely discoverability so the model picks `DocxCreate` etc. instead
/// of `Bash` + `python-docx`. Moved from `shared_session.rs`.
pub(crate) fn documents_prompt_section() -> String {
    String::from(
        "# Document & spreadsheet generation\n\n\
         When the user asks to create or read Word docs, Excel sheets, \
         PowerPoint decks, or PDFs, reach for these native tools instead \
         of shelling out to Python libraries. They are bundled (no setup \
         on the user's machine), embed Noto Sans Thai (mixed Thai/Latin \
         renders correctly), and produce predictable output.\n\n\
         - **DocxCreate** / **DocxRead** — Word `.docx`. Markdown in, \
         supports tables, inline images, H1–H4. Read extracts to text.\n\
         - **XlsxCreate** / **XlsxRead** — Excel `.xlsx`. Accepts CSV \
         string, JSON 2D array, or `[{sheet, rows}]` for multi-sheet \
         workbooks. Numeric cells stay numeric.\n\
         - **PptxCreate** / **PptxRead** — PowerPoint `.pptx`. Markdown \
         outline: `# Heading` starts a new slide, bullets become body. \
         Read extracts slide text.\n\
         - **PdfCreate** / **PdfRead** — PDF. Markdown in, supports \
         tables, inline images, embedded fonts. A4 / Letter / Legal.\n\n\
         Use these for the matching format every time. Do NOT call \
         generic `Read` on `.docx` / `.xlsx` / `.pptx` / `.pdf` — it \
         returns raw bytes the model can't parse; the dedicated `*Read` \
         tool extracts to model-readable text.\n",
    )
}

/// dev-plan/06 P2: render the skills catalog per the configured strategy
/// (`full` / `names-only` / `discover-tool-only`). Moved from
/// `shared_session.rs` so CLI + print + agent_runtime get the same
/// strategy-aware rendering. The `full` arm is the default; the other
/// two are opt-in for users with large catalogs.
pub(crate) fn append_skills_section(
    system: &mut String,
    store: &crate::skills::SkillStore,
    strategy: &str,
) {
    let mut entries: Vec<&crate::skills::SkillDef> = store.skills.values().collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    match strategy {
        "discover-tool-only" => {
            system.push_str("\n\n# Available skills (MANDATORY usage)\n");
            system.push_str(
                "Bundled skills are available but not listed inline (you have \
                 a large catalog). Discover them via `SkillList()` for the full \
                 catalog or `SkillSearch(query: \"...\")` for a substring \
                 lookup. When a user request sounds like it might match a \
                 bundled workflow (\"make a PDF\", \"scaffold a skill\", \
                 \"extract data from xlsx\", etc.), you MUST call SkillList \
                 or SkillSearch FIRST before implementing the task manually. \
                 Once you find a relevant skill, call `Skill(name: \"<name>\")` \
                 to load its expert instructions and follow them.\n",
            );
        }
        "names-only" => {
            system.push_str("\n\n# Available skills (MANDATORY usage)\n");
            system.push_str(
                "The `Skill` tool loads expert instructions for a bundled \
                 workflow. Skill names are listed below; for descriptions and \
                 trigger criteria call `SkillSearch(query: \"...\")` or \
                 `SkillList()`. If a user request might match any of these \
                 skills, you MUST call Skill (or SkillSearch first) FIRST — \
                 before any Bash, Write, Edit, or other tool calls for that \
                 task. Announce the skill at the start of your reply.\n\n",
            );
            let names: Vec<&str> = entries.iter().map(|s| s.name.as_str()).collect();
            system.push_str(&names.join(", "));
            system.push('\n');
        }
        _ => {
            // "full" (default).
            system.push_str("\n\n# Available skills (MANDATORY usage)\n");
            system.push_str(
                "The `Skill` tool loads expert instructions for a bundled workflow. \
                 If a user request matches the trigger criteria of any skill below, \
                 you MUST:\n\
                 1. Call `Skill(name: \"<skill-name>\")` FIRST — before any Bash, \
                    Write, Edit, or other tool calls for that task.\n\
                 2. Follow the instructions returned by that skill for the rest of \
                    the task. They override your default approach.\n\
                 3. Announce the skill at the start of your reply, e.g. \
                    \"Using the `pdf` skill to …\".\n\
                 Do NOT implement the task yourself when a matching skill exists — \
                 the skill encodes conventions and scripts you don't have built in.\n\n",
            );
            for skill in entries {
                system.push_str(&format!("- **{}** — {}", skill.name, skill.description));
                if !skill.when_to_use.is_empty() {
                    system.push_str(&format!("\n  Trigger: {}", skill.when_to_use));
                }
                system.push('\n');
            }
        }
    }
}

/// Team-collaboration framing. Four states (preserved verbatim from
/// the original implementation in `shared_session.rs`):
/// - team OFF + not on Claude Agent SDK → empty (no team section at all)
/// - team ON + on Claude Agent SDK → "UNREACHABLE on this provider"
/// - team OFF + on Claude Agent SDK → "DISABLED in this workspace"
/// - team ON + not on Claude Agent SDK → "enabled" framing + AgentSdk
///   addendum when applicable
///
/// Moved from `shared_session.rs` so all surfaces (CLI, print, GUI,
/// agent_runtime) reach for it. The long literal strings are
/// intentional — they're the model-facing copy the team audit pinned
/// (the model used to hallucinate fake team creation on Claude SDK
/// when the section was missing).
pub(crate) fn team_grounding_prompt(model: &str, team_enabled: bool) -> String {
    let kind = crate::providers::ProviderKind::detect(model);
    let on_claude_sdk = matches!(kind, Some(crate::providers::ProviderKind::AgentSdk));

    if !team_enabled && !on_claude_sdk {
        return String::new();
    }

    if team_enabled && on_claude_sdk {
        return String::from(
            "# Agent Teams — UNREACHABLE on this provider\n\n\
             The user has enabled thClaws's team feature \
             (`teamEnabled: true`), but they are also running on the \
             `agent/*` provider — which shells to the local `claude` \
             CLI as a subprocess. That subprocess uses Claude Code's \
             own built-in toolset (`Agent`, `Bash`, `Edit`, `Read`, \
             `ScheduleWakeup`, `Skill`, `ToolSearch`, `Write`) and \
             does NOT see thClaws's tool registry.\n\n\
             This means thClaws's `TeamCreate`, `SpawnTeammate`, \
             `SendMessage`, `CheckInbox`, `TeamStatus`, \
             `TeamTaskCreate`/`List`/`Claim`/`Complete`, and \
             `TeamMerge` tools are REGISTERED in thClaws but are \
             unreachable from your current toolset. You literally \
             cannot call them.\n\n\
             Claude Code's own `TeamCreate` / `Agent` / `TodoWrite` / \
             `AskUserQuestion` / `ToolSearch` / `SendMessage` \
             built-ins are available to you, but they write state \
             under `~/.claude/teams/` and `~/.claude/tasks/` which is \
             invisible to the thClaws Team tab. Calling them produces \
             a fabricated success — the user sees an empty Team tab.\n\n\
             If the user asks you to \"create a team\" / \"spawn agents\":\n\
             - Explain that thClaws's team tools are unreachable from \
             the `agent/*` provider (their tool registry doesn't \
             cross the CLI subprocess boundary).\n\
             - Tell them to switch to a non-`agent/*` provider — e.g. \
             `claude-sonnet-4-6`, `claude-opus-4-7`, `gpt-4o`, etc. — \
             via `/model` or `/provider`. Once switched, thClaws's \
             team tools are directly callable.\n\
             - Offer to proceed sequentially without a team if they \
             prefer to stay on the `agent/*` model.\n\n\
             Do NOT pretend a team has been created. Do NOT call \
             Claude Code's built-in `TeamCreate` etc. as a substitute. \
             The honest answer is the only useful one.\n",
        );
    }

    if !team_enabled {
        return String::from(
            "# Agent Teams — DISABLED in this workspace\n\n\
             The user has NOT enabled thClaws's team feature \
             (`teamEnabled: true` is missing from `.thclaws/settings.json`). \
             thClaws's team tools (`TeamCreate`, `SpawnTeammate`, `SendMessage`, \
             `CheckInbox`, `TeamStatus`, `TeamTaskCreate/List/Claim/Complete`, \
             `TeamMerge`) are NOT registered in this session and you cannot \
             call them.\n\n\
             You are running under the local `claude` CLI subprocess \
             (Anthropic Agent SDK), which DOES ship its own `TeamCreate`, \
             `Agent`, `TodoWrite`, `AskUserQuestion`, `ToolSearch`, \
             `SendMessage` built-ins backed by `~/.claude/teams/` and \
             `~/.claude/tasks/`. DO NOT CALL THEM. Their state is invisible \
             to thClaws — the Team tab polls `.thclaws/team/agents/` locally \
             and will never see an SDK-created team, so the user gets a \
             fabricated success story with nothing behind it.\n\n\
             If the user asks you to \"create a team\" / \"spawn agents\" / \
             \"set up a team of subagents\", respond in plain text:\n\
             - Explain that thClaws's team feature is off in this workspace.\n\
             - Tell them to set `teamEnabled: true` in `.thclaws/settings.json` \
             (or globally in `~/.config/thclaws/settings.json`) and restart \
             the app.\n\
             - Offer to proceed WITHOUT a team by handling the task yourself \
             sequentially.\n\n\
             Do NOT claim to have created a team, spawned teammates, written \
             config, or stored state. Do NOT reference `~/.claude/teams/` or \
             `~/.claude/tasks/` paths. The only honest response is \"teams are \
             disabled\" — anything else is a hallucination.\n",
        );
    }

    let mut out = String::from(
        "# Agent Teams (thClaws native)\n\n\
         This workspace has thClaws's team feature ENABLED. When the user asks for \
         parallel work via a team, use ONLY these thClaws tools — they are the \
         canonical implementation and their state is visible in the Team tab:\n\n\
         - `TeamCreate` — define a team (name + member agents with roles/prompts). \
         Writes `.thclaws/team/config.json` in the current project root.\n\
         - `SpawnTeammate` — start one named teammate. Spawns a thClaws subprocess \
         that polls its inbox in a tmux pane (or background).\n\
         - `SendMessage` — deliver a message to a teammate's inbox.\n\
         - `CheckInbox` — read your own inbox.\n\
         - `TeamStatus` — summarise the team.\n\
         - `TeamTaskCreate` / `TeamTaskList` / `TeamTaskClaim` / `TeamTaskComplete` — \
         a shared task queue teammates can claim from.\n\
         - `TeamMerge` — (lead only) merge each teammate's git worktree back into \
         the main branch.\n\n\
         Team state lives under `.thclaws/team/` **in the current project root** — \
         NOT under `~/.claude/teams/`, NOT under `~/.claude/tasks/`. Do not reference \
         those paths; they are from a different product.\n\n\
         You are the team **lead**. After `TeamCreate`:\n\
         1. Do NOT use `Bash`/`Write`/`Edit` to build code — delegate via `SendMessage`.\n\
         2. Use `TeamTaskCreate` to queue work; teammates claim via `TeamTaskClaim`.\n\
         3. Use `Read`/`Glob`/`Grep` only for review and verification.\n\
         4. Watch `CheckInbox` / `TeamStatus` between coordination rounds.\n\
         \n\
         **Worktree isolation is declarative.** If a teammate should work on \
         an isolated branch, set `isolation: \"worktree\"` on that member when \
         you call `TeamCreate`. `SpawnTeammate` then creates \
         `.worktrees/{name}` on branch `team/{name}` automatically and \
         launches the teammate there. DO NOT write `git worktree add …` or \
         `cd ../{name}` into teammate prompts — the teammate will execute them \
         as shell and the worktree will land somewhere wrong (project root, a \
         sibling dir) and be invisible to `TeamMerge`.\n\
         \n\
         # CRITICAL: do NOT call Claude Code's Agent SDK team tools\n\n\
         Your training data contains references to an Anthropic Managed Agents \
         SDK server-side toolset (`agent_toolset_20260401`) that ships its own \
         `TeamCreate`, `Agent`, `AskUserQuestion`, `TodoWrite`, `ToolSearch`, \
         `SendMessage` tools backed by `~/.claude/teams/` and `~/.claude/tasks/`. \
         Those are a DIFFERENT SYSTEM, invisible to thClaws — if you call them \
         (or claim to have called them in your text output), the user will see \
         an empty Team tab and think nothing happened.\n\n\
         Rules that apply regardless of which provider you are running on:\n\
         - When the user asks about \"teams\" / \"agents\" / \"task queue\", use \
         the thClaws tools listed above. `TeamCreate` and `SendMessage` in this \
         workspace mean the thClaws versions — never the SDK's.\n\
         - Never reference `~/.claude/teams/`, `~/.claude/tasks/`, or \
         `~/.config/thclaws/teams/` paths in your replies. Teams live in \
         `.thclaws/team/`.\n\
         - Do not call `AskUserQuestion`, `TodoWrite`, `ToolSearch`, or a bare \
         `Agent` tool. Those belong to Claude Code's interactive flow and do \
         not exist in thClaws. If you need a task list, use `TeamTaskCreate`. \
         If you need to ask the user, just ask them in plain text.\n\
         - Do not claim to have created a team, spawned agents, or stored \
         config unless you actually called the corresponding thClaws tool and \
         got a success response back.\n",
    );

    if on_claude_sdk {
        out.push_str(
            "\n# Additional note for the Claude Agent SDK provider\n\n\
             You ARE running under the local `claude` CLI subprocess right now, \
             which ships its own `TeamCreate`, `Agent`, `AskUserQuestion`, \
             `TodoWrite`, and `ToolSearch` built-ins. Calling them will appear \
             to succeed inside Claude Code's own world, but the thClaws Team \
             tab polls `.thclaws/team/agents/` and will never see a team \
             created that way. Treat any impulse to call those tools as a bug.\n",
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tier-2 followup test: the Repl surface adds the slash-command
    /// shortcut priming after the skill catalog; Gui / Headless do
    /// not. Checks only the surface-specific marker text — not full
    /// byte equality — because the builder also reads process-global
    /// env (`HAL_API_KEY`, `TAVILY_API_KEY`, `BRAVE_SEARCH_API_KEY`)
    /// and cwd for the services / memory / KMS sections, and parallel
    /// tests in `skills::tests::with_cwd` / `agent::tests::with_cwd`
    /// can race our back-to-back builder calls. The actual user-facing
    /// regression we guard against — "Repl users see the slash hint
    /// telling the model `/<skill>` is an explicit Skill request,
    /// other surfaces don't" — is binary-present-or-absent, so the
    /// addendum-marker check is enough and is race-resilient.
    #[test]
    fn repl_surface_adds_slash_shortcut_priming() {
        let tmp = tempfile::tempdir().unwrap();
        let config = crate::config::AppConfig::default();
        let mut store = crate::skills::SkillStore::default();
        let skill = crate::skills::SkillDef::new_eager(
            "demo".to_string(),
            "demo skill for the surface-parity test".to_string(),
            "whenever the surface-parity test calls this".to_string(),
            tmp.path().join("demo"),
            "body".to_string(),
        );
        store.skills.insert("demo".to_string(), skill);

        let repl =
            build_full_system_prompt(&config, tmp.path(), Some(&store), &[], SurfaceHints::Repl);
        let gui =
            build_full_system_prompt(&config, tmp.path(), Some(&store), &[], SurfaceHints::Gui);
        let headless = build_full_system_prompt(
            &config,
            tmp.path(),
            Some(&store),
            &[],
            SurfaceHints::Headless,
        );

        const ADDENDUM_MARKER: &str = "Slash-command shortcut";
        assert!(
            repl.contains(ADDENDUM_MARKER),
            "Repl prompt must include the slash-command-shortcut priming",
        );
        assert!(
            !gui.contains(ADDENDUM_MARKER),
            "Gui prompt must NOT include the Repl-only slash-command priming",
        );
        assert!(
            !headless.contains(ADDENDUM_MARKER),
            "Headless prompt must NOT include the Repl-only slash-command priming",
        );

        // Catalog presence + the literal skill name are determined
        // entirely by the local SkillStore — env races don't touch
        // these. Pins that the builder actually renders the section
        // when the store has entries.
        for (surface_name, p) in [("Repl", &repl), ("Gui", &gui), ("Headless", &headless)] {
            assert!(
                p.contains("# Available skills"),
                "{surface_name} prompt must include skill catalog when store has entries",
            );
            assert!(
                p.contains("**demo**"),
                "{surface_name} prompt must list the `demo` skill name",
            );
        }
    }

    /// Empty skill_store → no "Available skills" section anywhere, no
    /// slash priming even in Repl mode. Pins the silent-skip the
    /// v0.24 GUI bug exhibited: empty store → no model-facing signal
    /// that any skill exists, hence the user-reported "skill works in
    /// CLI but not GUI". Catalog presence is local-state-determined,
    /// so this is race-resilient.
    #[test]
    fn empty_skill_store_skips_section_in_all_surfaces() {
        let tmp = tempfile::tempdir().unwrap();
        let config = crate::config::AppConfig::default();
        let empty = crate::skills::SkillStore::default();

        for surface in [
            SurfaceHints::Repl,
            SurfaceHints::Gui,
            SurfaceHints::Headless,
        ] {
            let p = build_full_system_prompt(&config, tmp.path(), Some(&empty), &[], surface);
            assert!(
                !p.contains("# Available skills"),
                "{surface:?}: empty store must skip skill section, got:\n{p}",
            );
            assert!(
                !p.contains("Slash-command shortcut"),
                "{surface:?}: empty store must skip Repl priming too",
            );
        }
    }

    /// `None` for skill_store also skips the section — the print-mode
    /// shape (it doesn't discover skills to keep one-shot startup
    /// fast). Section / priming presence is determined entirely by
    /// the (skill_store, surface) pair, no env dependency.
    #[test]
    fn none_skill_store_skips_section_in_all_surfaces() {
        let tmp = tempfile::tempdir().unwrap();
        let config = crate::config::AppConfig::default();

        for surface in [
            SurfaceHints::Repl,
            SurfaceHints::Gui,
            SurfaceHints::Headless,
        ] {
            let p = build_full_system_prompt(&config, tmp.path(), None, &[], surface);
            assert!(
                !p.contains("# Available skills"),
                "{surface:?}: None must skip skill section, got:\n{p}",
            );
            assert!(
                !p.contains("Slash-command shortcut"),
                "{surface:?}: None must skip Repl priming too",
            );
        }
    }

    /// Run on demand to compare the system prompts each surface
    /// would send for a target project. Writes the three prompts
    /// to `/tmp/thclaws-prompt-{repl,gui,headless}.txt` so the
    /// caller can `diff` them. Honours `THCLAWS_PROJECT_ROOT` so
    /// you can point at any project without `cd`-ing (cargo
    /// chdirs to the manifest dir before running tests, so a plain
    /// `cd <project> && cargo test ...` wouldn't actually pick up
    /// `<project>` — set the env var instead).
    ///
    /// Usage:
    ///   THCLAWS_PROJECT_ROOT=<project> \
    ///     cargo test --features gui --lib \
    ///     prompts::tests::dump_all_surface_prompts \
    ///     -- --ignored --nocapture
    ///   diff /tmp/thclaws-prompt-gui.txt /tmp/thclaws-prompt-repl.txt
    ///   diff /tmp/thclaws-prompt-gui.txt /tmp/thclaws-prompt-headless.txt
    #[test]
    #[ignore]
    fn dump_all_surface_prompts() {
        let cwd = std::env::var_os("THCLAWS_PROJECT_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        // Reload config from inside the target project — picks up
        // its .thclaws/settings.json.
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&cwd).unwrap();
        let config = crate::config::AppConfig::load().unwrap_or_default();
        let skill_store = crate::skills::SkillStore::discover();
        std::env::set_current_dir(prev).unwrap();

        let store_ref = if skill_store.skills.is_empty() {
            None
        } else {
            Some(&skill_store)
        };

        eprintln!(
            "── target project ──  cwd={}  skills={}",
            cwd.display(),
            skill_store.skills.len()
        );

        for (slug, surface) in [
            ("repl", SurfaceHints::Repl),
            ("gui", SurfaceHints::Gui),
            ("headless", SurfaceHints::Headless),
        ] {
            let prompt = build_full_system_prompt(&config, &cwd, store_ref, &[], surface);
            let path = std::path::PathBuf::from(format!("/tmp/thclaws-prompt-{slug}.txt"));
            std::fs::write(&path, &prompt).unwrap();
            eprintln!("  wrote {} bytes  →  {}", prompt.len(), path.display());
        }

        eprintln!("\nCompare with:");
        eprintln!("  diff /tmp/thclaws-prompt-gui.txt /tmp/thclaws-prompt-repl.txt");
        eprintln!("  diff /tmp/thclaws-prompt-gui.txt /tmp/thclaws-prompt-headless.txt");
    }

    /// `collaboration_primitives_section` always includes Subagent +
    /// WorkflowRun (both always-available) and a team line that
    /// swaps based on `team_enabled`. The swap matters because the
    /// model otherwise would see Team tool names it can't call.
    #[test]
    fn collaboration_section_lists_all_three_primitives() {
        let on = collaboration_primitives_section(true);
        assert!(on.starts_with("# Collaboration primitives"));
        assert!(on.contains("**Subagent**"));
        assert!(on.contains("`Task`"));
        assert!(on.contains("**Agent Teams**"));
        assert!(on.contains("`TeamCreate`"));
        assert!(on.contains("**WorkflowRun**"));
        assert!(on.contains("`WorkflowRun(prompt"));

        let off = collaboration_primitives_section(false);
        assert!(off.contains("disabled in this workspace"));
        assert!(
            !off.contains("`TeamCreate`"),
            "team-off prompt must not name Team tools"
        );
        // Subagent + WorkflowRun appear regardless of team_enabled.
        assert!(off.contains("**Subagent**"));
        assert!(off.contains("**WorkflowRun**"));
    }

    /// The unified builder slots the Collaboration section between
    /// Documents and Team grounding. Verifies the heading appears
    /// when called with any SurfaceHints (collaboration is
    /// surface-agnostic) and that the team line in the section
    /// respects the `team_enabled` ProjectConfig lookup the builder
    /// performs internally.
    #[test]
    fn build_full_system_prompt_includes_collaboration_section() {
        let tmp = tempfile::tempdir().unwrap();
        let config = crate::config::AppConfig::default();
        for surface in [
            SurfaceHints::Repl,
            SurfaceHints::Gui,
            SurfaceHints::Headless,
        ] {
            let p = build_full_system_prompt(&config, tmp.path(), None, &[], surface);
            assert!(
                p.contains("# Collaboration primitives"),
                "{surface:?}: Collaboration section missing",
            );
            assert!(
                p.contains("**WorkflowRun**"),
                "{surface:?}: WorkflowRun must be named so the model knows it can call it",
            );
        }
    }

    /// `mcp_instructions_section` renders one `## <server>`
    /// subsection per server, in input order, filters out empty
    /// strings, and returns empty when no server has anything to
    /// say (so the unified builder skips the section heading too).
    #[test]
    fn mcp_section_renders_per_server_subsections() {
        let out = mcp_instructions_section(&[
            (
                "todo".to_string(),
                "Call list_tasks before todo_add.".to_string(),
            ),
            ("noisy".to_string(), "   ".to_string()), // whitespace-only → filtered
            ("calc".to_string(), "Numbers stay numeric.".to_string()),
        ]);
        assert!(out.starts_with("# MCP server instructions"));
        assert!(out.contains("## todo\n\nCall list_tasks before todo_add."));
        assert!(out.contains("## calc\n\nNumbers stay numeric."));
        assert!(
            !out.contains("## noisy"),
            "whitespace-only instructions must be filtered out",
        );
        // Server order preserved (todo before calc, not alphabetical).
        let todo_idx = out.find("## todo").unwrap();
        let calc_idx = out.find("## calc").unwrap();
        assert!(todo_idx < calc_idx, "input order must be preserved");
    }

    #[test]
    fn mcp_section_is_empty_when_no_server_has_instructions() {
        assert!(mcp_instructions_section(&[]).is_empty());
        assert!(
            mcp_instructions_section(&[
                ("a".to_string(), "".to_string()),
                ("b".to_string(), "   ".to_string()),
            ])
            .is_empty(),
            "all-empty input → no section heading either",
        );
    }

    /// The unified builder folds MCP instructions in between Services
    /// and Documents. Verifies the placement and that the slice flows
    /// through without mutation.
    #[test]
    fn build_full_system_prompt_includes_mcp_section() {
        let tmp = tempfile::tempdir().unwrap();
        let config = crate::config::AppConfig::default();
        let mcp = vec![(
            "pinn_ai".to_string(),
            "Use text2image for SDXL; img2img needs a base64 source.".to_string(),
        )];
        let p = build_full_system_prompt(&config, tmp.path(), None, &mcp, SurfaceHints::Gui);
        assert!(p.contains("# MCP server instructions"));
        assert!(p.contains("## pinn_ai"));
        assert!(p.contains("Use text2image for SDXL"));
    }

    #[test]
    fn render_substitutes_known_keys() {
        let out = render(
            "hello {name}, you are {role}",
            &[("name", "ada"), ("role", "lead")],
        );
        assert_eq!(out, "hello ada, you are lead");
    }

    #[test]
    fn render_leaves_unknown_keys_alone() {
        let out = render("hi {name} — {missing}", &[("name", "ada")]);
        assert_eq!(out, "hi ada — {missing}");
    }

    #[test]
    fn load_falls_back_to_default_when_no_override() {
        let out = load("__nonexistent_prompt_xyz__", "DEFAULT");
        assert_eq!(out, "DEFAULT");
    }

    #[test]
    fn load_applies_branding_to_product_placeholder() {
        // The default branding (open-core, no policy active) substitutes
        // `{product}` with "thClaws". Critical for system.md, which now
        // says "You are {product}" — without this substitution the agent
        // would literally introduce itself as "{product}".
        let template = "I am {product}.";
        let out = load("__nonexistent_for_test__", template);
        assert_eq!(out, "I am thClaws.");
    }

    #[test]
    fn load_applies_branding_to_default_system_prompt() {
        // The actual built-in system.md template starts with
        // "You are {product}, …" — confirm it round-trips through `load`
        // with the placeholder substituted. Test guards against future
        // bypasses of `branding::apply_template` in the load path.
        let out = load("__nonexistent_for_test__", defaults::SYSTEM);
        assert!(
            out.starts_with("You are thClaws,"),
            "system.md substitution missing — got: {}",
            out.lines().next().unwrap_or("")
        );
        assert!(
            !out.contains("{product}"),
            "{{product}} placeholder leaked into rendered prompt"
        );
    }

    #[test]
    fn default_system_prompt_distinguishes_todowrite_from_submitplan() {
        // Both surfaces must appear in the system prompt with their
        // distinct roles spelled out, so the model picks the right
        // tool without us having to teach it from scratch every turn.
        // Regression guard for M6.6 — the casual-vs-structured
        // distinction was the load-bearing addition.
        let s = defaults::SYSTEM;
        assert!(
            s.contains("SubmitPlan"),
            "SubmitPlan not mentioned: missing in system prompt"
        );
        assert!(
            s.contains("TodoWrite"),
            "TodoWrite not mentioned: missing in system prompt"
        );
        assert!(
            s.contains("scratchpad"),
            "TodoWrite must be framed as a scratchpad in the system prompt",
        );
        assert!(
            s.contains("sidebar"),
            "SubmitPlan's sidebar/visibility property must be named so the model knows when to use it",
        );
    }

    #[test]
    fn default_system_prompt_routes_user_plan_word_correctly() {
        // Users routinely say "plan to do X" colloquially — meaning
        // "let's organize this work", NOT "enter formal plan mode".
        // The system prompt must teach the model to decide on the
        // *work*, not the literal word — small jobs → TodoWrite,
        // big jobs (real per-step actions + runnable verifications)
        // → EnterPlanMode + SubmitPlan. Regression guard for M6.6:
        // user explicitly called this out as load-bearing.
        let s = defaults::SYSTEM;
        assert!(
            s.contains("Picking the right one when the user says \"plan\""),
            "section header for plan-routing missing: should be present in system prompt",
        );
        assert!(
            s.contains("Don't reflexively enter plan mode"),
            "anti-reflex guidance missing — model must not auto-enter plan mode on every \"plan\" mention",
        );
        assert!(
            (s.contains("Small job") || s.contains("Small or medium job"))
                && s.contains("TodoWrite"),
            "small-job → TodoWrite branch missing",
        );
        assert!(
            s.contains("Big job") && s.contains("SubmitPlan"),
            "big-job → SubmitPlan branch missing",
        );
        // Concrete examples — these anchor the abstract rule.
        assert!(
            s.contains("plan to rename") || s.contains("plan to add"),
            "TodoWrite-side example missing",
        );
        assert!(
            s.contains("plan to build a webapp") || s.contains("plan to migrate"),
            "SubmitPlan-side example missing",
        );
    }

    #[test]
    fn default_system_prompt_tells_model_to_check_todos_md_at_session_start() {
        // The "resume from existing todos.md" behaviour was the user's
        // specific ask in M6.6 — the system prompt must instruct the
        // model to look for the file and surface incomplete items
        // before starting fresh work. Sharpened post-test (gpt-4.1
        // didn't follow the original conditional wording) — the
        // directive is now unconditional and front-loaded.
        let s = defaults::SYSTEM;
        assert!(
            s.contains(".thclaws/todos.md"),
            "system prompt must name the todos file path",
        );
        assert!(
            s.contains("BEFORE asking the user"),
            "must instruct to check todos.md BEFORE asking for context",
        );
        assert!(
            s.contains("ALWAYS check"),
            "must use unconditional ALWAYS framing",
        );
        assert!(
            s.to_lowercase().contains("incomplete"),
            "must mention incomplete items as the resume target",
        );
        assert!(
            s.contains("resume"),
            "must offer resume as the option for existing todos",
        );
        assert!(
            s.contains("Don't ask"),
            "must explicitly forbid asking when a todo file already has answers",
        );
    }
}
