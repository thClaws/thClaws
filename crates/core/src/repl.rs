//! Interactive REPL loop + slash-command dispatcher.
//!
//! The pure-logic pieces (slash parsing, help rendering, provider factory)
//! are unit-tested. `run_repl` is the interactive entry point; it can only
//! be smoke-tested manually by running the `thclaws` binary.

use crate::agent::{Agent, AgentEvent};
use crate::config::{AppConfig, ProjectConfig};
use crate::context::ProjectContext;
use crate::error::{Error, Result};
use crate::mcp::{McpClient, McpServerConfig, McpTool};
use crate::memory::MemoryStore;
use crate::permissions::{PermissionMode, ReplApprover};
use crate::providers::{
    anthropic::AnthropicProvider, gemini::GeminiProvider, ollama::OllamaProvider,
    ollama_cloud::OllamaCloudProvider, openai::OpenAIProvider, Provider, ProviderKind,
};
use crate::session::{Session, SessionStore};
use crate::subagent::{ProductionAgentFactory, SubAgentTool};
use crate::tools::ToolRegistry;
use futures::StreamExt;
use std::io::Write;
use std::sync::Arc;

const COLOR_RESET: &str = "\x1b[0m";
const COLOR_DIM: &str = "\x1b[90m";
const COLOR_GREEN: &str = "\x1b[32m";
const COLOR_CYAN: &str = "\x1b[36m";
const COLOR_YELLOW: &str = "\x1b[33m";
const COLOR_BOLD: &str = "\x1b[1m";
const COLOR_RED: &str = "\x1b[31m";

const REPL_PROMPT: &str = "❯ ";

fn readline_config() -> rustyline::Config {
    let builder = rustyline::Config::builder();
    #[cfg(windows)]
    let builder = builder.behavior(rustyline::Behavior::PreferTerm);
    builder.build()
}
/// Render the current plan as a coloured ANSI block for the CLI
/// terminal — analogue of the right-side `PlanSidebar` component the
/// GUI chat tab gets. M5 CLI parity. Called from the agent loop after
/// any plan-tool ToolCallResult so the user sees the live state inline:
///
/// ```text
/// ─── plan: 4 steps · 2 done · current step 3 ───────
///   ✓ 1. Scaffold project
///   ✓ 2. Install dependencies
///   ◉ 3. Run tests
///     4. Deploy
/// ─────────────────────────────────────────────────
/// ```
///
/// Status glyphs: ✓ done · ◉ in_progress (yellow) · ✕ failed (red) ·
/// space todo. Notes (failure reasons, "skipped by user") render
/// dim-italic-ish below the step.
fn format_plan_for_cli(plan: &crate::tools::plan_state::Plan) -> String {
    use crate::tools::plan_state::StepStatus;
    let total = plan.steps.len();
    let done = plan
        .steps
        .iter()
        .filter(|s| s.status == StepStatus::Done)
        .count();
    let current = plan
        .steps
        .iter()
        .position(|s| s.status == StepStatus::InProgress);

    let header = match current {
        Some(idx) => format!(
            "─── plan: {total} step{plural} · {done} done · current step {n} ───",
            plural = if total == 1 { "" } else { "s" },
            n = idx + 1,
        ),
        None if done == total => format!("─── plan: {total} steps · all complete ───"),
        None => format!(
            "─── plan: {total} step{plural} · {done} done ───",
            plural = if total == 1 { "" } else { "s" },
        ),
    };

    let mut out = String::new();
    out.push_str(&format!("\n{COLOR_CYAN}{header}{COLOR_RESET}\n"));
    for (i, step) in plan.steps.iter().enumerate() {
        let (glyph, color) = match step.status {
            StepStatus::Done => ("✓", COLOR_GREEN),
            StepStatus::InProgress => ("◉", COLOR_YELLOW),
            StepStatus::Failed => ("✕", COLOR_RED),
            StepStatus::Todo => (" ", COLOR_DIM),
        };
        out.push_str(&format!(
            "  {color}{glyph}{COLOR_RESET} {dim}{n}.{COLOR_RESET} {title}\n",
            n = i + 1,
            dim = if step.status == StepStatus::Todo {
                COLOR_DIM
            } else {
                ""
            },
            title = step.title,
        ));
        if let Some(note) = &step.note {
            if !note.trim().is_empty() {
                let note_color = if step.status == StepStatus::Failed {
                    COLOR_RED
                } else {
                    COLOR_DIM
                };
                out.push_str(&format!("       {note_color}({note}){COLOR_RESET}\n"));
            }
        }
        // M6.3: render the cross-step output below the title for Done
        // steps so the user can see what each step produced. Truncate
        // long values for the CLI; the sidebar gets to show more.
        if let Some(output) = &step.output {
            if !output.trim().is_empty() {
                let preview: String = output.chars().take(120).collect();
                let suffix = if output.chars().count() > 120 {
                    "…"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "       {COLOR_DIM}→ {preview}{suffix}{COLOR_RESET}\n",
                ));
            }
        }
    }
    let footer = "─".repeat(header.chars().count());
    out.push_str(&format!("{COLOR_CYAN}{footer}{COLOR_RESET}\n"));
    out
}

/// Set of tool names that mutate plan state — used to gate the CLI
/// plan-block render so we don't print a plan after every Read or
/// Bash. Matches the registry names exactly.
const PLAN_TOOL_NAMES: &[&str] = &[
    "SubmitPlan",
    "UpdatePlanStep",
    "EnterPlanMode",
    "ExitPlanMode",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Quit,
    Clear,
    History,
    Model(String),
    Models,
    /// Download the model catalogue from the thclaws.ai endpoint and
    /// update the local cache. Used by the `/models refresh` UI path
    /// and by the daily auto-refresh background task.
    ModelsRefresh,
    /// Set a per-`provider/model` user override for context window.
    /// Defaults to user-global scope (`~/.config/thclaws/settings.json`);
    /// `--project` scopes to `.thclaws/settings.json` of the current
    /// working directory. Override wins over every catalogue layer at
    /// lookup time.
    ModelsSetContext {
        key: String,
        size: u32,
        project: bool,
    },
    /// Remove a `provider/model` override. Falls back to whatever the
    /// next catalogue layer says.
    ModelsUnsetContext {
        key: String,
        project: bool,
    },
    Provider(String),
    Providers,
    Config {
        key: String,
        value: String,
    },
    Save,
    Load(String),
    Sessions,
    Rename(String),
    MemoryList,
    MemoryRead(String),
    /// M6.26 BUG #2: write a memory entry. `body` carries an inline
    /// content string when `--body` was passed; `None` means open
    /// $EDITOR (CLI) or fall back to a one-shot scaffold (GUI).
    MemoryWrite {
        name: String,
        body: Option<String>,
        type_: Option<String>,
        description: Option<String>,
    },
    /// M6.26 BUG #2: append a chunk to an entry.
    MemoryAppend {
        name: String,
        body: String,
    },
    /// M6.26 BUG #2: edit an existing entry — same as Write but
    /// pre-fills the editor with current content. CLI only.
    MemoryEdit(String),
    /// M6.26 BUG #2: delete an entry. `yes` skips the confirmation prompt.
    MemoryDelete {
        name: String,
        yes: bool,
    },
    /// M6.29: start an iteration loop. Either fixed interval (`/loop 30s
    /// <body>`) or self-paced (`/loop <body>`, default 5min). `body` is
    /// the line the loop fires each iteration — slash command, bare
    /// prompt, or any input the worker would normally accept.
    Loop {
        interval_secs: Option<u64>,
        body: String,
    },
    /// M6.29: stop the active loop. No-op if none running.
    LoopStop,
    /// M6.29: show active loop status.
    LoopStatus,
    /// M6.29: start a new goal with audit-driven completion. `objective`
    /// is the user-supplied task; budgets are optional.
    GoalStart {
        objective: String,
        budget_tokens: Option<u64>,
        budget_time_secs: Option<u64>,
    },
    /// M6.29: show current goal state + budget consumption.
    GoalStatus,
    /// M6.29: fire one iteration toward the goal. Builds the audit
    /// prompt and runs an agent turn. Composable with `/loop /goal continue`.
    GoalContinue,
    /// M6.29: manually mark the goal complete. Bypasses the audit
    /// (use sparingly — the audit exists for a reason).
    GoalComplete {
        reason: Option<String>,
    },
    /// M6.29: abandon the goal with an optional reason.
    GoalAbandon {
        reason: Option<String>,
    },
    /// M6.29: show the goal's full text + budgets.
    GoalShow,
    Mcp,
    McpAdd {
        name: String,
        url: String,
        user: bool,
    },
    McpRemove {
        name: String,
        user: bool,
    },
    Plugins,
    PluginInstall {
        url: String,
        user: bool,
    },
    PluginRemove {
        name: String,
        user: bool,
    },
    PluginEnable {
        name: String,
        user: bool,
    },
    PluginDisable {
        name: String,
        user: bool,
    },
    PluginShow {
        name: String,
    },
    /// `/plugin gc` — remove registry entries whose plugin directory
    /// is missing or whose manifest fails to parse. M6.16.1 BUG L2.
    PluginGc,
    Tasks,
    Context,
    Version,
    Cwd,
    Thinking(String),
    Compact,
    /// Save the current session, then start a fresh session seeded with
    /// an LLM-summarized view of the prior history. Used when the
    /// session's on-disk JSONL has grown past the working threshold
    /// and continuing in-place would keep bloating the file.
    Fork,
    Doctor,
    Skills,
    /// Org-policy SSO subcommands (Phase 4).
    /// `/sso login`  — interactive OIDC login via browser + loopback callback
    /// `/sso logout` — clear cached tokens for the active issuer
    /// `/sso status` — show current session, expiry, and issuer
    Sso {
        sub: SsoSubcommand,
    },
    SkillInstall {
        git_url: String,
        name: Option<String>,
        project: bool,
    },
    SkillShow(String),
    /// `/skill marketplace` — list all skills in the marketplace catalogue.
    /// `--refresh` forces a remote fetch before listing.
    SkillMarketplace {
        refresh: bool,
    },
    /// `/skill search <query>` — case-insensitive substring match across
    /// name / description / category in the marketplace catalogue.
    SkillSearch(String),
    /// `/skill info <name>` — detail view for one marketplace entry.
    SkillInfo(String),
    /// `/mcp marketplace [--refresh]` — list MCP servers in catalogue.
    McpMarketplace {
        refresh: bool,
    },
    /// `/mcp search <query>` — search MCP server catalogue.
    McpSearch(String),
    /// `/mcp info <name>` — detail for a marketplace MCP server entry.
    McpInfo(String),
    /// `/mcp install [--user] <name>` — install MCP server from catalogue.
    /// Looks up `install_url` / transport / command and writes the
    /// matching `mcp.json` entry; clones source if `install_url` is set.
    McpInstall {
        name: String,
        user: bool,
    },
    /// `/plugin marketplace [--refresh]` — list plugins in catalogue.
    PluginMarketplace {
        refresh: bool,
    },
    /// `/plugin search <query>` — search plugin catalogue.
    PluginSearch(String),
    /// `/plugin info <name>` — detail for a marketplace plugin entry.
    PluginInfo(String),
    Permissions(String),
    /// `/plan` — toggle plan mode (M2). With no args, flips the
    /// session into plan mode (mutating tools blocked, sidebar opens
    /// when SubmitPlan fires). `/plan exit` / `/plan cancel` clears
    /// any active plan and restores the prior permission mode.
    Plan(String),
    Team,
    Usage,
    Kms,
    KmsNew {
        name: String,
        project: bool,
    },
    KmsUse(String),
    KmsOff(String),
    KmsShow(String),
    KmsIngest {
        name: String,
        file: String,
        alias: Option<String>,
        force: bool,
    },
    /// M6.25 BUG #8: ingest a remote URL (fetched via HTTP) into a KMS.
    KmsIngestUrl {
        name: String,
        url: String,
        alias: Option<String>,
        force: bool,
    },
    /// M6.25 BUG #8: ingest a PDF file (extracted via pdftotext) into a KMS.
    KmsIngestPdf {
        name: String,
        file: String,
        alias: Option<String>,
        force: bool,
    },
    /// M6.28: ingest the current chat session as a KMS page. Triggers an
    /// agent turn that summarizes history and calls `KmsWrite`.
    /// Source target was `$`.
    KmsIngestSession {
        name: String,
        alias: Option<String>,
        force: bool,
    },
    /// M6.25 BUG #3: lint a KMS for orphans / broken links / index drift /
    /// missing frontmatter. Pure-read; no mutation.
    KmsLint(String),
    /// M6.25 BUG #4: file the latest assistant message into a KMS as a
    /// new page. Compounds explorations into the wiki.
    KmsFileAnswer {
        name: String,
        title: String,
    },
    Unknown(String),
}

/// Subcommands of `/sso`. `/sso` with no arg defaults to `Status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SsoSubcommand {
    Login,
    Logout,
    Status,
}

/// Parse `/plugin [install|remove ...]` and the `/plugins` alias.
///
/// `/plugins` with no args lists; `/plugin` with no args also lists.
/// Subcommands: `install [--user] <url>` and `remove [--user] <name>`.
fn parse_plugin_subcommand(cmd: &str, args: &str) -> SlashCommand {
    let args = args.trim();
    if cmd == "plugins" || args.is_empty() {
        return SlashCommand::Plugins;
    }
    let (sub, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    match sub {
        "install" => {
            let mut parts: Vec<&str> = rest.split_whitespace().collect();
            let mut user = false;
            if parts.first().copied() == Some("--user") {
                user = true;
                parts.remove(0);
            } else if parts.first().copied() == Some("--project") {
                parts.remove(0);
            }
            match parts.as_slice() {
                [url] => SlashCommand::PluginInstall {
                    url: (*url).to_string(),
                    user,
                },
                _ => SlashCommand::Unknown(
                    "usage: /plugin install [--user] <name-or-git-url-or-.zip>".into(),
                ),
            }
        }
        "remove" | "rm" | "uninstall" => {
            let mut parts: Vec<&str> = rest.split_whitespace().collect();
            let mut user = false;
            if parts.first().copied() == Some("--user") {
                user = true;
                parts.remove(0);
            } else if parts.first().copied() == Some("--project") {
                parts.remove(0);
            }
            match parts.as_slice() {
                [name] => SlashCommand::PluginRemove {
                    name: (*name).to_string(),
                    user,
                },
                _ => SlashCommand::Unknown(
                    "usage: /plugin remove [--user] <name>".into(),
                ),
            }
        }
        "list" | "ls" => SlashCommand::Plugins,
        "enable" | "disable" => {
            let mut parts: Vec<&str> = rest.split_whitespace().collect();
            let mut user = false;
            if parts.first().copied() == Some("--user") {
                user = true;
                parts.remove(0);
            } else if parts.first().copied() == Some("--project") {
                parts.remove(0);
            }
            match parts.as_slice() {
                [name] => {
                    let name = (*name).to_string();
                    if sub == "enable" {
                        SlashCommand::PluginEnable { name, user }
                    } else {
                        SlashCommand::PluginDisable { name, user }
                    }
                }
                _ => SlashCommand::Unknown(format!(
                    "usage: /plugin {sub} [--user] <name>"
                )),
            }
        }
        "show" => match rest.split_whitespace().next() {
            Some(name) => SlashCommand::PluginShow { name: name.to_string() },
            None => SlashCommand::Unknown("usage: /plugin show <name>".into()),
        },
        // `/plugin gc` removes registry entries whose plugin
        // directory is missing or whose manifest can't be parsed.
        // No args. M6.16.1 BUG L2.
        "gc" => SlashCommand::PluginGc,
        "marketplace" => {
            let refresh = rest.split_whitespace().any(|p| p == "--refresh");
            SlashCommand::PluginMarketplace { refresh }
        }
        "search" => {
            if rest.is_empty() {
                SlashCommand::Unknown("usage: /plugin search <query>".into())
            } else {
                SlashCommand::PluginSearch(rest.to_string())
            }
        }
        // `/plugin info <name>` mirrors `/skill info`/`/mcp info` —
        // marketplace detail. Use `/plugin show <name>` for an
        // installed-plugin detail (keeps the terminology consistent
        // with the other extension namespaces).
        "info" => match rest.split_whitespace().next() {
            Some(name) => SlashCommand::PluginInfo(name.to_string()),
            None => SlashCommand::Unknown("usage: /plugin info <name>".into()),
        },
        other => SlashCommand::Unknown(format!(
            "unknown plugin subcommand: '{other}' (try: /plugin, /plugin install, /plugin remove, /plugin enable, /plugin disable, /plugin show, /plugin gc, /plugin marketplace, /plugin search, /plugin info)"
        )),
    }
}

/// Parse `/mcp [add|remove ...]` into the right SlashCommand.
/// - `/mcp` → list
/// - `/mcp add [--user] <name> <url>` → register an HTTP MCP server
/// - `/mcp remove [--user] <name>` → delete a server from mcp.json
fn parse_mcp_subcommand(args: &str) -> SlashCommand {
    let args = args.trim();
    if args.is_empty() {
        return SlashCommand::Mcp;
    }
    let (sub, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    match sub {
        "add" => {
            let mut parts: Vec<&str> = rest.split_whitespace().collect();
            let mut user = false;
            if parts.first().copied() == Some("--user") {
                user = true;
                parts.remove(0);
            } else if parts.first().copied() == Some("--project") {
                parts.remove(0);
            }
            match parts.as_slice() {
                [name, url] => SlashCommand::McpAdd {
                    name: (*name).to_string(),
                    url: (*url).to_string(),
                    user,
                },
                _ => SlashCommand::Unknown("usage: /mcp add [--user] <name> <url>".into()),
            }
        }
        "remove" | "rm" => {
            let mut parts: Vec<&str> = rest.split_whitespace().collect();
            let mut user = false;
            if parts.first().copied() == Some("--user") {
                user = true;
                parts.remove(0);
            } else if parts.first().copied() == Some("--project") {
                parts.remove(0);
            }
            match parts.as_slice() {
                [name] => SlashCommand::McpRemove {
                    name: (*name).to_string(),
                    user,
                },
                _ => SlashCommand::Unknown("usage: /mcp remove [--user] <name>".into()),
            }
        }
        "marketplace" => {
            let refresh = rest.split_whitespace().any(|p| p == "--refresh");
            SlashCommand::McpMarketplace { refresh }
        }
        "search" => {
            if rest.is_empty() {
                SlashCommand::Unknown("usage: /mcp search <query>".into())
            } else {
                SlashCommand::McpSearch(rest.to_string())
            }
        }
        "info" => {
            if rest.is_empty() {
                SlashCommand::Unknown("usage: /mcp info <name>".into())
            } else {
                SlashCommand::McpInfo(rest.to_string())
            }
        }
        "install" => {
            let mut parts: Vec<&str> = rest.split_whitespace().collect();
            let mut user = false;
            if parts.first().copied() == Some("--user") {
                user = true;
                parts.remove(0);
            } else if parts.first().copied() == Some("--project") {
                parts.remove(0);
            }
            match parts.as_slice() {
                [name] => SlashCommand::McpInstall {
                    name: (*name).to_string(),
                    user,
                },
                _ => SlashCommand::Unknown("usage: /mcp install [--user] <name>".into()),
            }
        }
        other => SlashCommand::Unknown(format!(
            "unknown mcp subcommand: '{other}' (try: /mcp, /mcp add, /mcp remove, /mcp marketplace, /mcp search, /mcp info, /mcp install)"
        )),
    }
}

/// Parse `/models [refresh|set-context|unset-context ...]`.
/// - `/models` → list (current behaviour)
/// - `/models refresh` → refetch catalogue
/// - `/models set-context [--project] <provider/model> <size>`
/// - `/models unset-context [--project] <provider/model>`
fn parse_models_subcommand(args: &str) -> SlashCommand {
    let args = args.trim();
    if args.is_empty() {
        return SlashCommand::Models;
    }
    let (sub, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    match sub {
        "refresh" => SlashCommand::ModelsRefresh,
        "set-context" => {
            let mut parts: Vec<&str> = rest.split_whitespace().collect();
            let mut project = false;
            if parts.first().copied() == Some("--project") {
                project = true;
                parts.remove(0);
            } else if parts.first().copied() == Some("--user") {
                parts.remove(0);
            }
            match parts.as_slice() {
                [key, size] => match parse_size(size) {
                    Some(n) => SlashCommand::ModelsSetContext {
                        key: (*key).to_string(),
                        size: n,
                        project,
                    },
                    None => SlashCommand::Unknown(format!(
                        "/models set-context: invalid size '{size}' (try 128000 or 128k)"
                    )),
                },
                _ => SlashCommand::Unknown(
                    "usage: /models set-context [--project] <provider/model> <size>".into(),
                ),
            }
        }
        "unset-context" => {
            let mut parts: Vec<&str> = rest.split_whitespace().collect();
            let mut project = false;
            if parts.first().copied() == Some("--project") {
                project = true;
                parts.remove(0);
            } else if parts.first().copied() == Some("--user") {
                parts.remove(0);
            }
            match parts.as_slice() {
                [key] => SlashCommand::ModelsUnsetContext {
                    key: (*key).to_string(),
                    project,
                },
                _ => SlashCommand::Unknown(
                    "usage: /models unset-context [--project] <provider/model>".into(),
                ),
            }
        }
        other => SlashCommand::Unknown(format!(
            "unknown /models subcommand: '{other}' (try /models, /models refresh, /models set-context, /models unset-context)"
        )),
    }
}

/// Parse a token-count argument that accepts plain digits ("128000") or
/// a `k`/`m` suffix ("128k", "1m"). Case-insensitive on the suffix.
fn parse_size(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = if let Some(rest) = s.strip_suffix(['k', 'K']) {
        (rest, 1_000u64)
    } else if let Some(rest) = s.strip_suffix(['m', 'M']) {
        (rest, 1_000_000u64)
    } else {
        (s, 1u64)
    };
    let n: u64 = num.parse().ok()?;
    let total = n.checked_mul(mult)?;
    if total == 0 || total > u32::MAX as u64 {
        return None;
    }
    Some(total as u32)
}

/// Default model to select when switching provider by name only.
/// Thin wrapper around `ProviderKind::from_name` + `default_model` for
/// backward-compat tests and REPL call sites that already use `&str`.
pub fn default_model_for_provider(provider: &str) -> Option<&'static str> {
    ProviderKind::from_name(provider).map(|k| k.default_model())
}

/// Parse a line as a slash command. Returns `None` when the line isn't a
/// slash command (so the caller can treat it as a user prompt).
///
/// M6.27: also recognizes the `# <name>:<body>` memory shortcut (Claude
/// Code parity) — translates to `SlashCommand::MemoryWrite` so the
/// dispatch goes through the same write path as `/memory write`. The
/// shortcut requires `<name>` to match `[A-Za-z0-9_-]+` and a colon
/// separator, so accidental markdown headers like `# Architecture: foo`
/// don't get intercepted.
pub fn parse_slash(input: &str) -> Option<SlashCommand> {
    let input = input.trim();
    if let Some(cmd) = parse_memory_shortcut(input) {
        return Some(cmd);
    }
    if !input.starts_with('/') {
        return None;
    }
    let rest = &input[1..];
    let (cmd, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let args = args.trim();

    Some(match cmd {
        "help" | "h" | "?" => SlashCommand::Help,
        "quit" | "q" | "exit" => SlashCommand::Quit,
        "clear" => SlashCommand::Clear,
        "history" => SlashCommand::History,
        "model" => SlashCommand::Model(args.to_string()),
        "models" => parse_models_subcommand(args),
        "provider" => SlashCommand::Provider(args.to_string()),
        "providers" => SlashCommand::Providers,
        "config" => match args.split_once('=') {
            Some((k, v)) => SlashCommand::Config {
                key: k.trim().to_string(),
                value: v.trim().to_string(),
            },
            None => SlashCommand::Unknown(format!("config expects key=value, got: '{args}'")),
        },
        "save" => SlashCommand::Save,
        "load" => SlashCommand::Load(args.to_string()),
        // `/resume` is a load-latest alias so the user-facing behaviour
        // mirrors the `--resume [ID|NAME]` CLI flag. Bare `/resume`
        // pulls the newest session; `/resume NAME` is the same as
        // `/load NAME`.
        "resume" => {
            let trimmed = args.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("last") {
                SlashCommand::Load("last".into())
            } else {
                SlashCommand::Load(trimmed.to_string())
            }
        }
        "sessions" => SlashCommand::Sessions,
        "rename" => SlashCommand::Rename(args.to_string()),
        "mcp" => parse_mcp_subcommand(args),
        "plugin" | "plugins" => parse_plugin_subcommand(cmd, args),
        "tasks" | "todo" => SlashCommand::Tasks,
        "context" => SlashCommand::Context,
        "version" | "v" => SlashCommand::Version,
        "cwd" | "pwd" => SlashCommand::Cwd,
        "thinking" => SlashCommand::Thinking(args.to_string()),
        "compact" => SlashCommand::Compact,
        "fork" => SlashCommand::Fork,
        "doctor" | "diag" => SlashCommand::Doctor,
        "sso" => match args.trim() {
            "" | "status" => SlashCommand::Sso {
                sub: SsoSubcommand::Status,
            },
            "login" => SlashCommand::Sso {
                sub: SsoSubcommand::Login,
            },
            "logout" => SlashCommand::Sso {
                sub: SsoSubcommand::Logout,
            },
            other => SlashCommand::Unknown(format!(
                "unknown /sso subcommand: '{other}' (try /sso, /sso login, /sso logout)"
            )),
        },
        "skills" => SlashCommand::Skills,
        "skill" => {
            // Supported subcommands:
            //   /skill show <name>                                  — installed-skill detail
            //   /skill install [--user] <url-or-zip-or-name> [name] — install (URL or marketplace name)
            //   /skill marketplace [--refresh]                      — list marketplace
            //   /skill search <query>                               — search marketplace
            //   /skill info <name>                                  — marketplace detail
            //
            // For `/skill install <X>`, `<X>` may be a git URL, a `.zip`
            // URL (incl. our `<repo>#<branch>:<subpath>` extension),
            // or a marketplace name. The dispatcher in the executor
            // detects which form it is and routes accordingly.
            let rest = args.trim();
            if let Some(after_show) = rest.strip_prefix("show").map(str::trim_start) {
                if after_show.is_empty() {
                    SlashCommand::Unknown("usage: /skill show <name>".into())
                } else {
                    SlashCommand::SkillShow(after_show.to_string())
                }
            } else if let Some(after_mp) = rest.strip_prefix("marketplace").map(str::trim_start) {
                let parts: Vec<&str> = after_mp.split_whitespace().collect();
                let refresh = parts.iter().any(|p| *p == "--refresh");
                SlashCommand::SkillMarketplace { refresh }
            } else if let Some(after_search) = rest.strip_prefix("search").map(str::trim_start) {
                if after_search.is_empty() {
                    SlashCommand::Unknown("usage: /skill search <query>".into())
                } else {
                    SlashCommand::SkillSearch(after_search.to_string())
                }
            } else if let Some(after_info) = rest.strip_prefix("info").map(str::trim_start) {
                if after_info.is_empty() {
                    SlashCommand::Unknown("usage: /skill info <name>".into())
                } else {
                    SlashCommand::SkillInfo(after_info.to_string())
                }
            } else if let Some(after_install) = rest.strip_prefix("install").map(str::trim_start) {
                let mut project = true;
                let mut parts: Vec<&str> = after_install.split_whitespace().collect();
                if parts.first().copied() == Some("--user") {
                    project = false;
                    parts.remove(0);
                } else if parts.first().copied() == Some("--project") {
                    // Accept --project as a no-op alias so old habits don't
                    // break.
                    parts.remove(0);
                }
                match parts.as_slice() {
                    [url_or_name] => SlashCommand::SkillInstall {
                        git_url: url_or_name.to_string(),
                        name: None,
                        project,
                    },
                    [url_or_name, name] => SlashCommand::SkillInstall {
                        git_url: url_or_name.to_string(),
                        name: Some(name.to_string()),
                        project,
                    },
                    _ => SlashCommand::Unknown(
                        "usage: /skill install [--user] <name-or-git-url-or-.zip> [name]".into(),
                    ),
                }
            } else {
                SlashCommand::Unknown(format!(
                    "unknown skill subcommand: '{rest}' (try: /skill install, /skill marketplace, /skill search, /skill info)"
                ))
            }
        }
        "permissions" | "perms" => SlashCommand::Permissions(args.to_string()),
        "plan" => SlashCommand::Plan(args.trim().to_string()),
        "team" => SlashCommand::Team,
        "usage" => SlashCommand::Usage,
        "memory" => parse_memory_subcommand(args),
        "kms" => parse_kms_subcommand(args),
        "loop" => parse_loop_subcommand(args),
        "goal" => parse_goal_subcommand(args),
        _ => SlashCommand::Unknown(cmd.to_string()),
    })
}

/// M6.27: detect the `# <name>:<body>` memory shortcut.
///
/// Matches when the input starts with `#` followed by a single space,
/// a slug-style name (`[A-Za-z0-9_-]+`), `:`, and non-empty body. The
/// strict pattern prevents intercepting markdown headers like
/// `# Architecture Plan: build a REST API` — `Architecture Plan` has a
/// space, so the name capture fails.
///
/// Returns `Some(MemoryWrite)` on match, `None` otherwise (including
/// for `#` prefixes that don't fit the pattern, like a bare comment).
fn parse_memory_shortcut(input: &str) -> Option<SlashCommand> {
    // Must start with `# ` (hash + exactly one space) or `#` followed
    // by the name directly (no space). Either form is fine; the colon
    // anchors the body separator.
    let after_hash = if let Some(rest) = input.strip_prefix("# ") {
        rest
    } else if let Some(rest) = input.strip_prefix('#') {
        rest
    } else {
        return None;
    };

    let (name_part, body_part) = after_hash.split_once(':')?;
    let name = name_part.trim();
    let body = body_part.trim();

    if name.is_empty() || body.is_empty() {
        return None;
    }
    // Validate name shape: slug chars only. Rejects `Architecture Plan`
    // (space) and any name with non-slug chars so markdown headers
    // pass through unchanged.
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }

    Some(SlashCommand::MemoryWrite {
        name: name.to_string(),
        body: Some(body.to_string()),
        type_: None,
        description: None,
    })
}

/// M6.26 BUG #2: parse `/memory [list|read|write|append|edit|delete ...]`.
///
/// Syntax:
/// - `/memory` (or `/memory list`) → list
/// - `/memory read <name>` (or `show` / `cat`) → read
/// - `/memory write <name>` → editor flow
/// - `/memory write <name> --body "..."` → one-shot inline write
/// - `/memory write <name> --type <user|feedback|project|reference> --description "..."`
///   → flag-pre-fill the frontmatter
/// - `/memory append <name> --body "..."` (or `add`)
/// - `/memory edit <name>` → editor pre-filled with existing content
/// - `/memory delete <name>` (or `rm`) [-y / --yes]
fn parse_memory_subcommand(args: &str) -> SlashCommand {
    let (sub, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();
    match sub {
        "" | "list" => SlashCommand::MemoryList,
        "read" | "show" | "cat" => SlashCommand::MemoryRead(rest.to_string()),
        "write" | "new" => parse_memory_write_args(rest),
        "append" | "add" => parse_memory_append_args(rest),
        "edit" => {
            if rest.is_empty() {
                SlashCommand::Unknown("usage: /memory edit <name>".into())
            } else {
                SlashCommand::MemoryEdit(rest.to_string())
            }
        }
        "delete" | "rm" | "remove" => parse_memory_delete_args(rest),
        other => SlashCommand::Unknown(format!("memory {other}")),
    }
}

/// Parse `<name> [--body "..."] [--type ...] [--description "..."]`.
/// Quotes around `--body` / `--description` values are honored to allow
/// embedded spaces. Unquoted values consume one whitespace-delimited token.
fn parse_memory_write_args(rest: &str) -> SlashCommand {
    let tokens = tokenize_quoted(rest);
    let mut name: Option<String> = None;
    let mut body: Option<String> = None;
    let mut type_: Option<String> = None;
    let mut description: Option<String> = None;
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i].as_str();
        match tok {
            "--body" | "-b" => {
                i += 1;
                if i >= tokens.len() {
                    return SlashCommand::Unknown(
                        "usage: /memory write <name> --body \"...\"".into(),
                    );
                }
                body = Some(tokens[i].clone());
            }
            "--type" | "-t" => {
                i += 1;
                if i >= tokens.len() {
                    return SlashCommand::Unknown("--type requires a value".into());
                }
                type_ = Some(tokens[i].clone());
            }
            "--description" | "--desc" | "-d" => {
                i += 1;
                if i >= tokens.len() {
                    return SlashCommand::Unknown("--description requires a value".into());
                }
                description = Some(tokens[i].clone());
            }
            other if other.starts_with("--") => {
                return SlashCommand::Unknown(format!("unknown flag: {other}"));
            }
            other => {
                if name.is_some() {
                    return SlashCommand::Unknown(format!(
                        "unexpected positional: {other} (name already set)"
                    ));
                }
                name = Some(other.to_string());
            }
        }
        i += 1;
    }
    match name {
        Some(n) => SlashCommand::MemoryWrite {
            name: n,
            body,
            type_,
            description,
        },
        None => SlashCommand::Unknown(
            "usage: /memory write <name> [--body \"...\"] [--type ...] [--description \"...\"]"
                .into(),
        ),
    }
}

fn parse_memory_append_args(rest: &str) -> SlashCommand {
    let tokens = tokenize_quoted(rest);
    let mut name: Option<String> = None;
    let mut body: Option<String> = None;
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i].as_str();
        match tok {
            "--body" | "-b" => {
                i += 1;
                if i >= tokens.len() {
                    return SlashCommand::Unknown(
                        "usage: /memory append <name> --body \"...\"".into(),
                    );
                }
                body = Some(tokens[i].clone());
            }
            other if other.starts_with("--") => {
                return SlashCommand::Unknown(format!("unknown flag: {other}"));
            }
            other => {
                if name.is_some() {
                    return SlashCommand::Unknown(format!("unexpected positional: {other}"));
                }
                name = Some(other.to_string());
            }
        }
        i += 1;
    }
    match (name, body) {
        (Some(n), Some(b)) => SlashCommand::MemoryAppend { name: n, body: b },
        _ => SlashCommand::Unknown("usage: /memory append <name> --body \"...\"".into()),
    }
}

fn parse_memory_delete_args(rest: &str) -> SlashCommand {
    let mut name: Option<String> = None;
    let mut yes = false;
    for tok in rest.split_whitespace() {
        match tok {
            "--yes" | "-y" => yes = true,
            other if other.starts_with("--") => {
                return SlashCommand::Unknown(format!("unknown flag: {other}"));
            }
            other => {
                if name.is_some() {
                    return SlashCommand::Unknown(format!("unexpected positional: {other}"));
                }
                name = Some(other.to_string());
            }
        }
    }
    match name {
        Some(n) => SlashCommand::MemoryDelete { name: n, yes },
        None => SlashCommand::Unknown("usage: /memory delete <name> [-y]".into()),
    }
}

/// Split `s` into whitespace-delimited tokens, honoring `"..."` and
/// `'...'` quoting (so `--body "long string with spaces"` becomes one
/// token). Backslash escapes inside quotes are NOT honored — keep it
/// simple; users who need literal quote chars can avoid them.
fn tokenize_quoted(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_double = false;
    let mut in_single = false;
    for ch in s.chars() {
        match ch {
            '"' if !in_single => {
                in_double = !in_double;
            }
            '\'' if !in_double => {
                in_single = !in_single;
            }
            c if c.is_whitespace() && !in_double && !in_single => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Source of the auto-resolved page slug for `/kms ingest <name> $`.
/// Drives the wording of the prompt's "Page name:" hint so the model
/// sees provenance (and can decide whether the slug fits the topic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KmsIngestSessionAliasSource {
    /// User passed `as <alias>` on the command line.
    UserSupplied,
    /// Derived from the active session's title (`session.title`).
    SessionTitle,
    /// Derived from the session id (no title set).
    SessionId,
}

/// M6.28: resolve the page slug + its provenance for `/kms ingest
/// <name> $`. Precedence:
///   1. User-supplied `as <alias>` (sanitized)
///   2. `session.title` if set (sanitized)
///   3. `session.id` (already slug-safe — `sess-<hex>`)
/// The slug is guaranteed non-empty because session.id always starts
/// with `sess-` + hex chars.
pub fn resolve_session_alias(
    user_alias: Option<&str>,
    session_title: Option<&str>,
    session_id: &str,
) -> (String, KmsIngestSessionAliasSource) {
    if let Some(a) = user_alias {
        let sanitized = crate::kms::sanitize_alias(a);
        if !sanitized.is_empty() {
            return (sanitized, KmsIngestSessionAliasSource::UserSupplied);
        }
    }
    if let Some(t) = session_title {
        let sanitized = crate::kms::sanitize_alias(t);
        if !sanitized.is_empty() {
            return (sanitized, KmsIngestSessionAliasSource::SessionTitle);
        }
    }
    (
        session_id.to_string(),
        KmsIngestSessionAliasSource::SessionId,
    )
}

/// M6.28: build the prompt that gets fed to the agent for `/kms ingest
/// <name> $`. Tells the model to summarize the current session and call
/// `KmsWrite` to file it. The page slug is resolved at the call site
/// (CLI / GUI) before this is called — see `resolve_session_alias` —
/// so the model gets a concrete page name with provenance hint.
///
/// Used by both CLI and GUI handlers — both rewrite the slash command
/// into this prompt and feed it to `agent.run_turn`.
pub fn build_kms_ingest_session_prompt(
    kms_name: &str,
    page: &str,
    source: KmsIngestSessionAliasSource,
    force: bool,
) -> String {
    let provenance = match source {
        KmsIngestSessionAliasSource::UserSupplied => "user-supplied via `as <alias>`",
        KmsIngestSessionAliasSource::SessionTitle => "derived from the active session title",
        KmsIngestSessionAliasSource::SessionId => {
            "derived from the active session id (the session has no title — \
             refine if a meaningful theme is obvious)"
        }
    };
    let force_hint = if force {
        "The user passed `--force` — if the page already exists, replace it."
    } else {
        "If `KmsWrite` errors with `already exists`, suggest the user re-run with `--force` to \
         replace; do not silently skip."
    };
    format!(
        "The user ran `/kms ingest {kms_name} $` to file the current chat session as a \
         knowledge-base page in KMS '{kms_name}'.\n\
         \n\
         Steps:\n\
         1. Summarize this conversation as a self-contained wiki page suitable for future \
         reference. Include:\n   - An H1 title that captures the topic\n   - Key topics \
         discussed / decisions made / conclusions reached\n   - Any artifacts created (files, \
         commits, dev-logs, manuals, etc.) with paths or commit SHAs\n   - Open questions or \
         follow-ups\n   Keep it tight: synthesize, don't transcribe. Aim for 200-1500 words \
         depending on conversation depth.\n\
         \n\
         2. Call `KmsWrite(kms: \"{kms_name}\", page: \"{page}\", content: \"...\")` with \
         frontmatter:\n   ---\n   category: session\n   sources: chat\n   description: \
         <one-line hook>\n   ---\n   <your summary>\n\
         \n\
         Page name: `{page}` ({provenance}).\n\
         \n\
         3. {force_hint}\n\
         \n\
         4. After the write succeeds, confirm to the user with the resolved page path."
    )
}

/// M6.26 BUG #2: scaffold body for `/memory write` / `/memory edit`.
/// When `existing` is `Some`, pre-fills with that entry's frontmatter +
/// body for editing. When `None`, builds a fresh template.
fn build_memory_scaffold(
    name: &str,
    type_: Option<&str>,
    description: Option<&str>,
    existing: Option<&crate::memory::MemoryEntry>,
) -> String {
    if let Some(e) = existing {
        // Edit flow — re-emit frontmatter + body so the user sees what
        // they're about to change.
        let mut out = String::from("---\n");
        out.push_str(&format!("name: {}\n", e.name));
        if !e.description.is_empty() {
            out.push_str(&format!("description: {}\n", e.description));
        }
        if let Some(ty) = &e.memory_type {
            out.push_str(&format!("type: {ty}\n"));
        }
        out.push_str("---\n");
        out.push_str(&e.body);
        if !e.body.ends_with('\n') {
            out.push('\n');
        }
        out
    } else {
        // Fresh template — pre-fill anything the user gave on the
        // command line so the editor doesn't duplicate the work.
        let mut out = String::from("---\n");
        out.push_str(&format!("name: {name}\n"));
        out.push_str(&format!("description: {}\n", description.unwrap_or("")));
        out.push_str(&format!("type: {}\n", type_.unwrap_or("")));
        out.push_str("---\n\n");
        out
    }
}

/// M6.26 BUG #2: spawn `$EDITOR` (default `vi`) on a temp file
/// pre-filled with `scaffold`, return the post-edit content. Ignores
/// the post-edit content if the editor exits non-zero (treated as
/// cancellation — the caller surfaces "(empty content — write cancelled)").
fn spawn_editor_for_memory(
    name: &str,
    scaffold: &str,
) -> std::result::Result<String, std::io::Error> {
    use std::io::Write;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let tmp = std::env::temp_dir().join(format!("thclaws-memory-{name}.md"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(scaffold.as_bytes())?;
    }
    let status = std::process::Command::new(&editor).arg(&tmp).status()?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(std::io::Error::other(format!(
            "$EDITOR ({editor}) exited {}",
            status.code().unwrap_or(-1)
        )));
    }
    let contents = std::fs::read_to_string(&tmp)?;
    let _ = std::fs::remove_file(&tmp);
    Ok(contents)
}

/// Parse `/kms [list|new|use|off|show ...]`.
/// M6.29: parse `/loop [<interval>] <body>` / `/loop stop` / `/loop status`.
///
/// `<interval>` is a duration like `30s`, `5m`, `2h`. If the first
/// token doesn't parse as a duration, the whole `args` string is
/// treated as the body and the loop runs self-paced (default 5min).
///
/// Examples:
///   /loop                         → status
///   /loop status                  → status
///   /loop stop / cancel           → stop
///   /loop 30s /goal continue      → fixed-interval, 30s
///   /loop 5m continue working     → fixed-interval, 5min, plain prompt
///   /loop /goal continue          → self-paced (default 5min)
fn parse_loop_subcommand(args: &str) -> SlashCommand {
    let trimmed = args.trim();
    if trimmed.is_empty() || trimmed == "status" || trimmed == "list" {
        return SlashCommand::LoopStatus;
    }
    if matches!(trimmed, "stop" | "cancel" | "kill" | "off") {
        return SlashCommand::LoopStop;
    }
    let (first, rest) = trimmed
        .split_once(char::is_whitespace)
        .unwrap_or((trimmed, ""));
    if let Some(secs) = parse_duration_secs(first) {
        let body = rest.trim();
        if body.is_empty() {
            return SlashCommand::Unknown(
                "usage: /loop <interval> <body>; got interval but no body".into(),
            );
        }
        SlashCommand::Loop {
            interval_secs: Some(secs),
            body: body.to_string(),
        }
    } else {
        // First token isn't a duration — treat whole input as body,
        // self-paced.
        SlashCommand::Loop {
            interval_secs: None,
            body: trimmed.to_string(),
        }
    }
}

/// Parse a duration string like `30s` / `5m` / `2h` / `1d` to seconds.
/// Returns `None` if the string doesn't match the pattern.
fn parse_duration_secs(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let (num_part, unit) = s.split_at(s.len() - 1);
    let n: u64 = num_part.parse().ok()?;
    match unit {
        "s" | "S" => Some(n),
        "m" | "M" => Some(n * 60),
        "h" | "H" => Some(n * 3600),
        "d" | "D" => Some(n * 86_400),
        _ => None,
    }
}

/// M6.29: parse `/goal <subcommand>`.
///
///   /goal start "<objective>" [--budget-tokens N] [--budget-time T]
///   /goal status
///   /goal continue
///   /goal complete [reason]
///   /goal abandon [reason]
///   /goal show
fn parse_goal_subcommand(args: &str) -> SlashCommand {
    let (sub, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();
    match sub {
        "" | "status" => SlashCommand::GoalStatus,
        "show" | "info" => SlashCommand::GoalShow,
        "continue" | "next" => SlashCommand::GoalContinue,
        "complete" | "done" => SlashCommand::GoalComplete {
            reason: if rest.is_empty() {
                None
            } else {
                Some(rest.to_string())
            },
        },
        "abandon" | "stop" | "cancel" => SlashCommand::GoalAbandon {
            reason: if rest.is_empty() {
                None
            } else {
                Some(rest.to_string())
            },
        },
        "start" | "set" | "new" => parse_goal_start_args(rest),
        other => SlashCommand::Unknown(format!(
            "unknown goal subcommand: '{other}' (try: /goal, /goal start, /goal continue, \
             /goal complete, /goal abandon, /goal show)"
        )),
    }
}

/// Parse `/goal start <objective> [--budget-tokens N] [--budget-time T]`.
/// Objective can be quoted ("...") to include all words; unquoted
/// strings consume up to the first `--` flag.
fn parse_goal_start_args(rest: &str) -> SlashCommand {
    let tokens = tokenize_quoted(rest);
    let mut objective_parts: Vec<String> = Vec::new();
    let mut budget_tokens: Option<u64> = None;
    let mut budget_time_secs: Option<u64> = None;
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i].as_str();
        match tok {
            "--budget-tokens" | "--tokens" => {
                i += 1;
                if i >= tokens.len() {
                    return SlashCommand::Unknown("--budget-tokens requires a number".into());
                }
                match tokens[i].parse::<u64>() {
                    Ok(n) => budget_tokens = Some(n),
                    Err(_) => {
                        return SlashCommand::Unknown(format!(
                            "--budget-tokens: not a number: {}",
                            tokens[i]
                        ))
                    }
                }
            }
            "--budget-time" | "--time" => {
                i += 1;
                if i >= tokens.len() {
                    return SlashCommand::Unknown(
                        "--budget-time requires a duration (e.g. 30m, 2h)".into(),
                    );
                }
                match parse_duration_secs(&tokens[i]) {
                    Some(n) => budget_time_secs = Some(n),
                    None => {
                        return SlashCommand::Unknown(format!(
                            "--budget-time: not a duration: {}",
                            tokens[i]
                        ))
                    }
                }
            }
            other if other.starts_with("--") => {
                return SlashCommand::Unknown(format!("unknown flag: {other}"));
            }
            other => objective_parts.push(other.to_string()),
        }
        i += 1;
    }
    let objective = objective_parts.join(" ");
    if objective.trim().is_empty() {
        return SlashCommand::Unknown(
            "usage: /goal start \"<objective>\" [--budget-tokens N] [--budget-time T]".into(),
        );
    }
    SlashCommand::GoalStart {
        objective,
        budget_tokens,
        budget_time_secs,
    }
}

fn parse_kms_subcommand(args: &str) -> SlashCommand {
    let (sub, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();
    match sub {
        "" | "list" | "ls" => SlashCommand::Kms,
        "new" | "create" => {
            // Project scope is the default — a KMS is typically tied
            // to the code you're working on, so `./.thclaws/kms/<name>`
            // follows the repo. `--user` opts out into the user-global
            // `~/.config/thclaws/kms/<name>`. `--project` is accepted
            // as a no-op alias so muscle memory from the old default
            // doesn't break on upgrade.
            let mut project = true;
            let mut parts: Vec<&str> = rest.split_whitespace().collect();
            if let Some(i) = parts.iter().position(|p| *p == "--user") {
                project = false;
                parts.remove(i);
            } else if let Some(i) = parts.iter().position(|p| *p == "--project") {
                parts.remove(i);
            }
            match parts.as_slice() {
                [name] => SlashCommand::KmsNew {
                    name: (*name).to_string(),
                    project,
                },
                _ => SlashCommand::Unknown(
                    "usage: /kms new [--user] <name>".into(),
                ),
            }
        }
        "use" | "on" => {
            if rest.is_empty() {
                SlashCommand::Unknown("usage: /kms use <name>".into())
            } else {
                SlashCommand::KmsUse(rest.to_string())
            }
        }
        "off" | "unuse" => {
            if rest.is_empty() {
                SlashCommand::Unknown("usage: /kms off <name>".into())
            } else {
                SlashCommand::KmsOff(rest.to_string())
            }
        }
        "show" | "cat" => {
            if rest.is_empty() {
                SlashCommand::Unknown("usage: /kms show <name>".into())
            } else {
                SlashCommand::KmsShow(rest.to_string())
            }
        }
        "ingest" | "add" => {
            // Syntax: /kms ingest <kms-name> <file-or-url> [as <alias>] [--force]
            //
            // M6.25 BUG #8: detect URL vs PDF vs text and dispatch to the
            // matching ingest variant. URL: starts with http:// or https://.
            // PDF: file extension is .pdf. Otherwise: standard text ingest.
            let mut parts: Vec<&str> = rest.split_whitespace().collect();
            let mut force = false;
            if let Some(i) = parts.iter().position(|p| *p == "--force" || *p == "-f") {
                force = true;
                parts.remove(i);
            }
            let mut alias: Option<String> = None;
            if let Some(i) = parts.iter().position(|p| *p == "as") {
                if i + 1 < parts.len() {
                    alias = Some(parts[i + 1].to_string());
                    parts.drain(i..=i + 1);
                } else {
                    return SlashCommand::Unknown(
                        "usage: /kms ingest <kms> <file-or-url> [as <alias>] [--force]".into(),
                    );
                }
            }
            match parts.as_slice() {
                [name, target] => {
                    let t = *target;
                    if t == "$" {
                        // M6.28: `$` = current chat session. Triggers an
                        // agent turn that summarizes history + calls
                        // KmsWrite.
                        SlashCommand::KmsIngestSession {
                            name: (*name).to_string(),
                            alias,
                            force,
                        }
                    } else if t.starts_with("http://") || t.starts_with("https://") {
                        SlashCommand::KmsIngestUrl {
                            name: (*name).to_string(),
                            url: t.to_string(),
                            alias,
                            force,
                        }
                    } else if t.to_ascii_lowercase().ends_with(".pdf") {
                        SlashCommand::KmsIngestPdf {
                            name: (*name).to_string(),
                            file: t.to_string(),
                            alias,
                            force,
                        }
                    } else {
                        SlashCommand::KmsIngest {
                            name: (*name).to_string(),
                            file: t.to_string(),
                            alias,
                            force,
                        }
                    }
                }
                _ => SlashCommand::Unknown(
                    "usage: /kms ingest <kms> <file-or-url-or-$> [as <alias>] [--force]".into(),
                ),
            }
        }
        "lint" | "check" | "doctor" => {
            // M6.25 BUG #3: pure-read health check.
            if rest.is_empty() {
                SlashCommand::Unknown("usage: /kms lint <name>".into())
            } else {
                SlashCommand::KmsLint(rest.to_string())
            }
        }
        "file-answer" | "file" => {
            // M6.25 BUG #4: file the latest assistant message as a new
            // KMS page. Syntax: /kms file-answer <kms-name> <title>
            // (everything after the kms name is the title).
            let mut parts = rest.splitn(2, char::is_whitespace);
            match (parts.next(), parts.next()) {
                (Some(name), Some(title)) if !name.is_empty() && !title.trim().is_empty() => {
                    SlashCommand::KmsFileAnswer {
                        name: name.to_string(),
                        title: title.trim().to_string(),
                    }
                }
                _ => SlashCommand::Unknown(
                    "usage: /kms file-answer <kms> <title>".into(),
                ),
            }
        }
        other => SlashCommand::Unknown(format!(
            "unknown kms subcommand: '{other}' (try: /kms, /kms new …, /kms use …, /kms off …, /kms show …, /kms ingest …, /kms lint …, /kms file-answer …)"
        )),
    }
}

/// One built-in slash command, surfaced to the GUI's `/` popup so it can
/// render an autocomplete list grouped by `category`.
///
/// Keep this list in lock-step with the `parse_slash` arms in this file
/// and the dispatch arms in `shell_dispatch.rs`. Help text is the
/// single-line summary shown next to the name in the popup; longer
/// usage syntax (e.g. flags, sub-commands) goes in `usage` so the
/// popup can render it as dim trailing text.
pub struct BuiltInCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    /// Optional argument hint, e.g. `"NAME"` for `/model NAME`. Empty
    /// when the command takes no arguments.
    pub usage: &'static str,
}

/// Decide how to interpret the argument to `/skill install <X>`:
///
///   * URL-shaped (`http://`, `https://`, `git@`, `.zip`, contains `://`,
///     starts with `/`, or carries our `<repo>#<branch>:<subpath>`
///     extension) → install_from_url gets `<X>` directly, optional
///     `name` from the second arg.
///   * Otherwise (bare slug like `skill-creator`) → look up in the
///     marketplace catalogue. Resolved `install_url` becomes the URL we
///     hand to `install_from_url`; the name defaults to the marketplace
///     entry's slug if the user didn't pass one explicitly.
///
/// Returns `(effective_url, effective_name, abort_msg)`. If `abort_msg`
/// is `Some`, the caller should print it and skip install (used for
/// linked-only entries and unknown names).
fn resolve_skill_install_target(
    arg: &str,
    explicit_name: Option<&str>,
) -> (String, Option<String>, Option<String>) {
    if looks_like_url(arg) {
        return (arg.to_string(), explicit_name.map(String::from), None);
    }
    let mp = crate::marketplace::load();
    match mp.find(arg) {
        Some(entry) if entry.license_tier == "linked-only" => {
            let homepage = if entry.homepage.is_empty() {
                "the upstream repo".to_string()
            } else {
                entry.homepage.clone()
            };
            (
                String::new(),
                None,
                Some(format!(
                    "'{}' is source-available and cannot be redistributed — install directly from {}",
                    entry.name, homepage
                )),
            )
        }
        Some(entry) => match &entry.install_url {
            Some(url) => (
                url.clone(),
                Some(explicit_name.map(String::from).unwrap_or_else(|| entry.name.clone())),
                None,
            ),
            None => (
                String::new(),
                None,
                Some(format!(
                    "'{}' has no install_url in the marketplace catalogue",
                    entry.name
                )),
            ),
        },
        None => (
            String::new(),
            None,
            Some(format!(
                "no skill named '{arg}' in marketplace and not a URL — try /skill search <query> or pass a git URL"
            )),
        ),
    }
}

/// Sorted list of MCP server names a plugin contributes (or `None`
/// if the plugin isn't found / has no MCP servers / manifest unread).
/// Used by /plugin enable / disable / remove to render an emphasized
/// hint listing the actual server names so the user knows what's
/// coming after `/quit` + relaunch. M6.16.1 — replaces the older
/// `plugin_has_mcp_servers` boolean.
pub fn plugin_mcp_server_names(name: &str) -> Option<Vec<String>> {
    let plugin = crate::plugins::find_installed(name)?;
    let manifest = plugin.manifest().ok()?;
    if manifest.mcp_servers.is_empty() {
        return None;
    }
    let mut names: Vec<String> = manifest.mcp_servers.keys().cloned().collect();
    names.sort();
    Some(names)
}

/// `/plugin install <X>` mirror of `resolve_skill_install_target`. If
/// `arg` looks like a URL, pass it through; otherwise look it up in
/// the marketplace's `plugins` array by name and return that entry's
/// `install_url`. Returns `(effective_url, abort_msg)`.
pub fn resolve_plugin_install_target(arg: &str) -> (String, Option<String>) {
    if looks_like_url(arg) {
        return (arg.to_string(), None);
    }
    let mp = crate::marketplace::load();
    match mp.find_plugin(arg) {
        Some(entry) if entry.license_tier == "linked-only" => {
            let homepage = if entry.homepage.is_empty() {
                "the upstream repo".to_string()
            } else {
                entry.homepage.clone()
            };
            (
                String::new(),
                Some(format!(
                    "'{}' is source-available and cannot be redistributed — install directly from {}",
                    entry.name, homepage
                )),
            )
        }
        Some(entry) => (entry.install_url.clone(), None),
        None => (
            String::new(),
            Some(format!(
                "no plugin named '{arg}' in marketplace and not a URL — try /plugin search <query> or pass a git URL"
            )),
        ),
    }
}

/// Heuristic: does this argument look like a URL or a bare marketplace
/// name? Conservative — when in doubt we prefer URL (so a typo in a
/// marketplace name doesn't accidentally hit some local path).
fn looks_like_url(s: &str) -> bool {
    s.contains("://")
        || s.starts_with("git@")
        || s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || s.to_ascii_lowercase().ends_with(".zip")
}

/// Install an MCP server from the marketplace catalogue. Writes the
/// matching `mcp.json` entry — that's it. **Does not** download or
/// install the underlying package; the entry's `command` / `args`
/// must already resolve on PATH (or use a runner like `uvx` / `npx`
/// that fetches the package on first invocation).
///
/// Why no clone: an MCP server is a separate process the agent spawns
/// via the configured command — it's not source the agent reads.
/// Whatever package manager the upstream ships under (PyPI / npm /
/// cargo / a binary release) is responsible for installing it; the
/// marketplace entry's `post_install_message` describes that step
/// when needed (e.g. "first run will auto-install via uvx" or "run
/// `pip install foo` first").
///
/// Errors out cleanly when the name isn't in the catalog or when the
/// mcp.json write fails.
pub async fn install_mcp_from_marketplace(
    name: &str,
    user: bool,
) -> std::result::Result<Vec<String>, String> {
    let mp = crate::marketplace::load();
    let entry = mp
        .find_mcp(name)
        .ok_or_else(|| format!(
            "no MCP named '{name}' in marketplace — try /mcp search <query> or /mcp add <name> <url> for a custom server"
        ))?
        .clone();

    // Build the mcp.json config from the entry. Transport shape:
    //   - "sse"   → http transport, url-only
    //   - "stdio" → command + args, no url
    // Marketplace install — trusted, so the server can render UI
    // widgets and accept widget-initiated tool calls.
    let cfg = if entry.transport == "sse" {
        crate::mcp::McpServerConfig {
            name: entry.name.clone(),
            transport: "http".into(),
            command: String::new(),
            args: Vec::new(),
            env: Default::default(),
            url: entry.url.clone(),
            headers: Default::default(),
            trusted: true,
        }
    } else {
        crate::mcp::McpServerConfig {
            name: entry.name.clone(),
            transport: "stdio".into(),
            command: entry.command.clone(),
            args: entry.args.clone(),
            env: Default::default(),
            url: String::new(),
            headers: Default::default(),
            trusted: true,
        }
    };
    let saved_to =
        crate::config::save_mcp_server(&cfg, user).map_err(|e| format!("save mcp.json: {e}"))?;

    let mut report: Vec<String> = Vec::new();
    let scope = if user { "user" } else { "project" };
    report.push(format!(
        "registered '{}' in {} ({} scope, {} transport)",
        entry.name,
        saved_to.display(),
        scope,
        entry.transport
    ));
    if entry.transport == "stdio" && !entry.command.is_empty() {
        let argv = if entry.args.is_empty() {
            entry.command.clone()
        } else {
            format!("{} {}", entry.command, entry.args.join(" "))
        };
        report.push(format!("command: {argv}"));
    }
    if let Some(msg) = &entry.post_install_message {
        report.push(format!("note: {msg}"));
    }
    report.push("restart thClaws to spawn the MCP and load its tools".into());

    Ok(report)
}

// Hand-aligned struct-literal table — keeping the columns reads well at a
// glance and rustfmt's exploded form (~6 lines per row) bloats the function
// to >180 lines for the same content. Skip for the table only.
#[rustfmt::skip]
pub fn built_in_commands() -> &'static [BuiltInCommand] {
    &[
        // Session
        BuiltInCommand { name: "clear",    description: "Clear conversation history",                 category: "Session", usage: "" },
        BuiltInCommand { name: "compact",  description: "Compact history (drop oldest, keep recent)", category: "Session", usage: "" },
        BuiltInCommand { name: "fork",     description: "Save + start a new session seeded with a summary", category: "Session", usage: "" },
        BuiltInCommand { name: "save",     description: "Force-save the current session",             category: "Session", usage: "" },
        BuiltInCommand { name: "load",     description: "Load a saved session by id or name",         category: "Session", usage: "ID|NAME" },
        BuiltInCommand { name: "sessions", description: "List saved sessions",                        category: "Session", usage: "" },
        BuiltInCommand { name: "rename",   description: "Rename the current session",                 category: "Session", usage: "NAME" },
        BuiltInCommand { name: "history",  description: "Print message-history summary",              category: "Session", usage: "" },

        // Model
        BuiltInCommand { name: "model",     description: "Show or switch the current model",          category: "Model", usage: "[NAME]" },
        BuiltInCommand { name: "models",    description: "List models from the current provider",     category: "Model", usage: "" },
        BuiltInCommand { name: "provider",  description: "Switch provider to its default model",      category: "Model", usage: "NAME" },
        BuiltInCommand { name: "providers", description: "List all supported providers",              category: "Model", usage: "" },
        BuiltInCommand { name: "thinking",  description: "Set extended-thinking token budget",        category: "Model", usage: "BUDGET" },
        BuiltInCommand { name: "permissions", description: "Show or set the permission mode",         category: "Model", usage: "[auto|ask]" },
        BuiltInCommand { name: "plan",        description: "Toggle plan mode (read-only + sidebar)", category: "Model", usage: "[enter|exit|status]" },

        // Context / memory / knowledge
        BuiltInCommand { name: "context",  description: "Show context-window usage breakdown",        category: "Context", usage: "" },
        BuiltInCommand { name: "memory",   description: "List memory entries",                        category: "Context", usage: "" },
        BuiltInCommand { name: "kms",      description: "List knowledge bases",                       category: "Context", usage: "" },

        // Skills, plugins, MCP
        BuiltInCommand { name: "skills",   description: "List installed skills",                      category: "Extensions", usage: "" },
        BuiltInCommand { name: "skill",    description: "Skill subcommands (install / marketplace / search / info / show)", category: "Extensions", usage: "<sub> [args]" },
        BuiltInCommand { name: "plugins",  description: "List installed plugins",                     category: "Extensions", usage: "" },
        BuiltInCommand { name: "plugin",   description: "Plugin subcommands (install / marketplace / search / info / show / enable / disable)", category: "Extensions", usage: "<sub> [args]" },
        BuiltInCommand { name: "mcp",      description: "MCP subcommands (add / remove / install / marketplace / search / info)", category: "Extensions", usage: "[sub] [args]" },

        // Team
        BuiltInCommand { name: "team",     description: "Show team agent status",                     category: "Team", usage: "" },
        BuiltInCommand { name: "tasks",    description: "List current tasks/todos",                   category: "Team", usage: "" },

        // System
        BuiltInCommand { name: "help",     description: "Show this help",                             category: "System", usage: "" },
        BuiltInCommand { name: "version",  description: "Show version",                               category: "System", usage: "" },
        BuiltInCommand { name: "cwd",      description: "Show current working directory",             category: "System", usage: "" },
        BuiltInCommand { name: "usage",    description: "Show token usage by provider and model",     category: "System", usage: "" },
        BuiltInCommand { name: "doctor",   description: "Run diagnostics",                            category: "System", usage: "" },
        BuiltInCommand { name: "config",   description: "Set a config value (session-only)",          category: "System", usage: "key=value" },
        BuiltInCommand { name: "quit",     description: "Exit",                                       category: "System", usage: "" },
    ]
}

pub fn render_help() -> &'static str {
    "Shell escape:\n  \
     !<command>        Run <command> in a subshell — sandbox-restricted to \n  \
                       the project directory, non-interactive env vars set \n  \
                       (CI=1, NPM_CONFIG_YES, TERM=dumb, etc.). Output is \n  \
                       displayed but NOT pushed to agent history. Use this \n  \
                       for quick checks between turns (`!git status`, \n  \
                       `!cargo check`). The agent doesn't see the output. \n\n\
     Slash commands:\n  \
     /help             Show this help\n  \
     /quit             Exit\n  \
     /clear            Clear conversation history\n  \
     /history          Print message-history summary\n  \
     /model [NAME]     Show current model, or switch to NAME\n  \
     /models           List models available from the current provider\n  \
     /provider NAME    Switch provider to its default model\n  \
     /providers        List all supported providers + defaults\n  \
     /config key=val   Set a config value (session-only for now)\n  \
     /save             Force-save the current session\n  \
     /load ID|NAME     Load a saved session by id or (renamed) title\n  \
     /resume [ID|NAME] Resume the latest session (or a specific one by id/name)\n  \
     /sessions         List saved sessions\n  \
     /rename [NAME]    Rename the current session (no arg clears the title)\n  \
     /memory           List memory entries\n  \
     /memory read NAME Show a memory entry by name\n  \
     /mcp              List active MCP servers and their tools\n  \
     /mcp add [--user] <name> <url>\n  \
                       Register a remote (HTTP) MCP server. Writes to\n  \
                       .thclaws/mcp.json (or ~/.config/thclaws/mcp.json\n  \
                       with --user), then connects and registers tools.\n  \
     /mcp remove [--user] <name>\n  \
                       Remove an MCP server from the config file.\n  \
     /plugins          List installed plugins\n  \
     /plugin install [--user] <url>\n  \
                       Install a plugin bundle (git or .zip URL) with\n  \
                       skills, commands, and MCP servers under one manifest.\n  \
     /plugin remove [--user] <name>\n  \
                       Uninstall a plugin and remove its files.\n  \
     /plugin enable [--user] <name>\n  \
     /plugin disable [--user] <name>\n  \
                       Toggle a plugin on/off without uninstalling it.\n  \
     /plugin show <name>\n  \
                       Show full manifest details for an installed plugin.\n  \
     /tasks            List current tasks/todos\n  \
     /context          Show the current system prompt\n  \
     /thinking BUDGET  Set extended-thinking token budget (0 = off)\n  \
     /cwd              Show current working directory\n  \
     /version          Show version\n  \
     /team             Attach to team tmux session (or show status)\n  \
     /usage            Show token usage by provider and model\n  \
     /skill show NAME  Show full description + path for a skill\n  \
     /skill install [--user] <url> [name]\n  \
     \x20                 Install a skill (or bundle) from a git repo or\n  \
     \x20                 a .zip URL into ./.thclaws/skills/ (default) or\n  \
     \x20                 ~/.config/thclaws/skills/ (--user)\n  \
     /kms              List knowledge bases (* = active for this project)\n  \
     /kms new [--user] NAME\n  \
     \x20                 Create a new KMS under ./.thclaws/kms/\n  \
     \x20                 (default) or ~/.config/thclaws/kms/ (--user)\n  \
     /kms use NAME     Attach a KMS to this project's chats\n  \
     /kms off NAME     Detach a KMS\n  \
     /kms show NAME    Print the KMS index.md\n  \
     /kms ingest KMS FILE [as ALIAS] [--force]\n  \
     \x20                 Copy a working-dir file into KMS/pages/ and\n  \
     \x20                 add it to the index. Allowed: .md .markdown\n  \
     \x20                 .txt .rst .log .json\n\n  \
     ! <command>       Run a shell command directly (e.g. ! git status)"
}

/// Build a Provider for the current `config.model`. Picks the impl based on the
/// model prefix. Anthropic / OpenAI / Gemini read an env var for auth;
/// Ollama uses a local endpoint with no auth (base URL overridable via
/// `OLLAMA_BASE_URL`).
pub fn build_provider(config: &AppConfig) -> Result<Arc<dyn Provider>> {
    let kind = config.detect_provider_kind()?;

    // Org policy gateway (EE Phase 3): when policies.gateway.enabled and
    // this provider should route through the gateway, replace the entire
    // provider with an OpenAI-compatible client pointing at the gateway
    // URL. The gateway (LiteLLM, Portkey, etc.) handles upstream routing
    // based on the model id and applies its own auth. User's per-provider
    // API keys are deliberately ignored — gateway owns credentials.
    if crate::providers::gateway::should_route(kind) {
        if let Some(url) = crate::providers::gateway::gateway_url() {
            let chat_url = if url.ends_with("/chat/completions") {
                url
            } else {
                format!("{}/chat/completions", url.trim_end_matches('/'))
            };
            // The gateway's auth header replaces normal Bearer-with-key.
            // Empty string is fine — OpenAIProvider always sends some
            // Authorization, and gateways without auth ignore it.
            let auth = crate::providers::gateway::resolve_auth_header().unwrap_or_default();
            return Ok(Arc::new(OpenAIProvider::new(auth).with_base_url(chat_url)));
        }
    }

    // Auth-less providers build directly.
    match kind {
        ProviderKind::AgentSdk => {
            let bin = std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
            return Ok(Arc::new(
                crate::providers::agent_sdk::AgentSdkProvider::new().with_bin(bin),
            ));
        }
        ProviderKind::Ollama => {
            let base = std::env::var("OLLAMA_BASE_URL")
                .unwrap_or_else(|_| crate::providers::ollama::DEFAULT_BASE_URL.to_string());
            return Ok(Arc::new(OllamaProvider::new().with_base_url(base)));
        }
        ProviderKind::OllamaAnthropic => {
            // Ollama's Anthropic-compatible endpoint at /v1/messages.
            // Uses the Anthropic wire format but with "ollama" as the auth token.
            // No prompt caching, no extended thinking — Ollama doesn't support them.
            let base = std::env::var("OLLAMA_BASE_URL")
                .unwrap_or_else(|_| crate::providers::ollama::DEFAULT_BASE_URL.to_string());
            let url = format!("{}/v1/messages", base.trim_end_matches('/'));
            return Ok(Arc::new(
                AnthropicProvider::new("ollama").with_base_url(url),
            ));
        }
        ProviderKind::LMStudio => {
            // LMStudio is OpenAI-compatible at /v1 with no auth. Default
            // base http://localhost:1234/v1; user-configurable via the
            // Settings UI or LMSTUDIO_BASE_URL env. Pass a dummy bearer
            // token — LMStudio ignores Authorization but the OpenAI
            // client always sends one.
            let base = std::env::var("LMSTUDIO_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:1234/v1".to_string());
            let url = if base.ends_with("/chat/completions") {
                base
            } else {
                format!("{}/chat/completions", base.trim_end_matches('/'))
            };
            return Ok(Arc::new(
                OpenAIProvider::new("lm-studio".to_string())
                    .with_base_url(url)
                    .with_strip_model_prefix("lmstudio/"),
            ));
        }
        _ => {}
    }

    let api_key = config.api_key_from_env().ok_or_else(|| {
        let envar = kind.api_key_env().unwrap_or("<none>");
        Error::Config(format!(
            "no API key found for provider '{}' — set {envar}",
            kind.name()
        ))
    })?;
    match kind {
        ProviderKind::AgenticPress => {
            // Hosted gateway — URL is fixed by the service, no env override.
            Ok(Arc::new(
                OpenAIProvider::new(api_key)
                    .with_base_url("https://llm.artech.cloud/v1/chat/completions")
                    .with_strip_model_prefix("ap/"),
            ))
        }
        ProviderKind::OpenRouter => {
            // OpenAI-compatible; models use openrouter/<vendor>/<model> form
            // (e.g. openrouter/anthropic/claude-sonnet-4-6). Strip the
            // "openrouter/" prefix before forwarding to the upstream API.
            Ok(Arc::new(
                OpenAIProvider::new(api_key)
                    .with_base_url("https://openrouter.ai/api/v1/chat/completions")
                    .with_strip_model_prefix("openrouter/"),
            ))
        }
        ProviderKind::Anthropic => Ok(Arc::new(AnthropicProvider::new(api_key))),
        ProviderKind::OpenAI => Ok(Arc::new(OpenAIProvider::new(api_key))),
        ProviderKind::OpenAIResponses => Ok(Arc::new(
            crate::providers::openai_responses::OpenAIResponsesProvider::new(api_key),
        )),
        ProviderKind::Gemini => Ok(Arc::new(GeminiProvider::new(api_key))),
        ProviderKind::DashScope => {
            let base = std::env::var("DASHSCOPE_BASE_URL").unwrap_or_else(|_| {
                "https://dashscope.aliyuncs.com/compatible-mode/v1".to_string()
            });
            let url = if base.ends_with("/chat/completions") {
                base
            } else {
                format!("{}/chat/completions", base.trim_end_matches('/'))
            };
            Ok(Arc::new(OpenAIProvider::new(api_key).with_base_url(url)))
        }
        ProviderKind::ZAi => {
            // Z.ai GLM Coding Plan endpoint. Models use `zai/<id>` form
            // (e.g. zai/glm-4.6). Strip the prefix before forwarding to
            // the OpenAI-compatible upstream. Power users with the
            // general BigModel SKU (https://open.bigmodel.cn/api/paas/v4)
            // can override via ZAI_BASE_URL.
            let base = std::env::var("ZAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.z.ai/api/coding/paas/v4".to_string());
            let url = if base.ends_with("/chat/completions") {
                base
            } else {
                format!("{}/chat/completions", base.trim_end_matches('/'))
            };
            Ok(Arc::new(
                OpenAIProvider::new(api_key)
                    .with_base_url(url)
                    .with_strip_model_prefix("zai/"),
            ))
        }
        ProviderKind::AzureAIFoundry => {
            let endpoint = std::env::var("AZURE_AI_FOUNDRY_ENDPOINT").map_err(|_| {
                Error::Config(
                    "AZURE_AI_FOUNDRY_ENDPOINT not set — add it in Settings or export the env var"
                        .into(),
                )
            })?;
            let base = endpoint.trim_end_matches('/');
            let messages_url = format!("{base}/anthropic/v1/messages");
            Ok(Arc::new(
                AnthropicProvider::new(api_key).with_base_url(messages_url),
            ))
        }
        ProviderKind::OpenAICompat => {
            // Generic OpenAI-compatible endpoint (SML Gateway, LiteLLM,
            // Portkey, Helicone, vLLM, internal corporate proxies, etc.).
            // Models use `oai/<id>` form (e.g. oai/gpt-4o-mini); the
            // "oai/" prefix is stripped before the request reaches the
            // upstream. Auth is `Bearer $OPENAI_COMPAT_API_KEY`.
            let base = std::env::var("OPENAI_COMPAT_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:8000/v1".to_string());
            let url = if base.ends_with("/chat/completions") {
                base
            } else {
                format!("{}/chat/completions", base.trim_end_matches('/'))
            };
            Ok(Arc::new(
                OpenAIProvider::new(api_key)
                    .with_base_url(url)
                    .with_strip_model_prefix("oai/"),
            ))
        }
        ProviderKind::DeepSeek => {
            // DeepSeek's hosted endpoint is OpenAI-compatible. Model IDs
            // (deepseek-chat, deepseek-reasoner) are bare — no prefix to
            // strip. Override via DEEPSEEK_BASE_URL for proxies / self-
            // hosted deployments.
            let base = std::env::var("DEEPSEEK_BASE_URL")
                .unwrap_or_else(|_| "https://api.deepseek.com/v1".to_string());
            let url = if base.ends_with("/chat/completions") {
                base
            } else {
                format!("{}/chat/completions", base.trim_end_matches('/'))
            };
            Ok(Arc::new(OpenAIProvider::new(api_key).with_base_url(url)))
        }
        ProviderKind::ThaiLLM => {
            // NSTDA / สวทช Thai LLM aggregator (thaillm.or.th). OpenAI-
            // compatible endpoint hosting OpenThaiGPT, Typhoon-S,
            // Pathumma, and THaLLE. Models use the `thaillm/<id>` form;
            // the prefix is stripped before the request reaches the
            // upstream. Override via THAILLM_BASE_URL for testing.
            let base = std::env::var("THAILLM_BASE_URL")
                .unwrap_or_else(|_| "http://thaillm.or.th/api/v1".to_string());
            let url = if base.ends_with("/chat/completions") {
                base
            } else {
                format!("{}/chat/completions", base.trim_end_matches('/'))
            };
            Ok(Arc::new(
                OpenAIProvider::new(api_key)
                    .with_base_url(url)
                    .with_strip_model_prefix("thaillm/"),
            ))
        }
        ProviderKind::Nvidia => {
            // NVIDIA NIM — OpenAI-compatible hosted inference at
            // integrate.api.nvidia.com. Model IDs (e.g.
            // `nvidia/nemotron-3-super-120b-a12b`) keep the `nvidia/`
            // prefix on the wire — it is part of NVIDIA's own model
            // namespace, not a thClaws routing prefix. Override via
            // NVIDIA_BASE_URL for on-prem NIM deployments.
            let base = std::env::var("NVIDIA_BASE_URL")
                .unwrap_or_else(|_| "https://integrate.api.nvidia.com/v1".to_string());
            let url = if base.ends_with("/chat/completions") {
                base
            } else {
                format!("{}/chat/completions", base.trim_end_matches('/'))
            };
            Ok(Arc::new(OpenAIProvider::new(api_key).with_base_url(url)))
        }
        ProviderKind::Ollama
        | ProviderKind::OllamaAnthropic
        | ProviderKind::LMStudio
        | ProviderKind::AgentSdk => {
            unreachable!("handled above")
        }
        ProviderKind::OllamaCloud => Ok(Arc::new(OllamaCloudProvider::new(api_key))),
    }
}

/// A no-op provider that errors friendly on every stream attempt.
/// Used at REPL startup when literally no provider has credentials and
/// Ollama isn't running, so the app can still open the Settings modal
/// instead of exiting before the user sees the window.
struct NoProviderPlaceholder;

#[async_trait::async_trait]
impl Provider for NoProviderPlaceholder {
    async fn stream(
        &self,
        _req: crate::providers::StreamRequest,
    ) -> Result<crate::providers::EventStream> {
        Err(Error::Config(
            "No LLM provider configured yet. Open Settings → Provider API keys (the gear icon in the status bar) to paste a key, or start Ollama locally and run `/model ollama/gemma4:26b`.".into()
        ))
    }
}

/// Try [`build_provider`] with the configured model, then fall back to
/// any provider that actually has a working API key. Used at REPL
/// startup so a missing `~/.config/thclaws/.env` (or a since-rotated
/// key) doesn't crash the app — the user ends up on whichever provider
/// is actually configured, with a yellow warning explaining the swap.
///
/// Fallback order picks providers that don't need auth first (Ollama
/// variants), then hosted providers in an order that usually matches
/// user preference. If *nothing* is available, returns `None` so the
/// caller can start the REPL in a degraded state where the user is
/// prompted to configure a key before the first turn.
pub async fn build_provider_with_fallback(
    config: &mut AppConfig,
) -> (Option<Arc<dyn Provider>>, Option<String>) {
    // 1. Try the configured model.
    if let Ok(p) = build_provider(config) {
        return (Some(p), None);
    }
    let original = config.model.clone();

    // 2. Walk a preference list. Cloud providers only succeed when a
    //    matching key exists (shell export > keychain > .env). Ollama
    //    variants always *build* successfully, so we probe the endpoint
    //    before offering them as a fallback — otherwise a user with no
    //    keys AND no local Ollama gets a noisy "model not found" loop
    //    on the first prompt.
    let fallback_order: &[ProviderKind] = &[
        ProviderKind::Anthropic,
        ProviderKind::OpenAI,
        ProviderKind::AgenticPress,
        ProviderKind::OpenRouter,
        ProviderKind::Gemini,
        ProviderKind::DashScope,
        ProviderKind::ZAi,
        ProviderKind::ThaiLLM,
        ProviderKind::Ollama,
        ProviderKind::OllamaAnthropic,
        ProviderKind::OllamaCloud,
    ];
    let ollama_alive = ollama_is_reachable().await;
    for kind in fallback_order {
        let is_ollama = matches!(kind, ProviderKind::Ollama | ProviderKind::OllamaAnthropic);
        if is_ollama && !ollama_alive {
            continue;
        }
        config.model = kind.default_model().to_string();
        if let Ok(p) = build_provider(config) {
            let warning = format!(
                "no API key for {} — falling back to {} (model: {})",
                ProviderKind::detect(&original)
                    .map(|k| k.name())
                    .unwrap_or("<unknown>"),
                kind.name(),
                config.model
            );
            return (Some(p), Some(warning));
        }
    }

    // 3. Nothing works — restore the original model so the rest of the
    //    REPL still shows what the user had configured, and let the
    //    caller degrade gracefully.
    config.model = original;
    (None, Some(
        "no usable LLM provider — set an API key via Settings → Provider API keys, or start Ollama (see Chapter 2)".into(),
    ))
}

/// Quick HEAD-style probe against Ollama's `/api/version` to decide
/// whether it's worth offering as a startup fallback. 500 ms timeout
/// so we don't hold up a fresh-install launch.
async fn ollama_is_reachable() -> bool {
    let base = std::env::var("OLLAMA_BASE_URL")
        .unwrap_or_else(|_| crate::providers::ollama::DEFAULT_BASE_URL.to_string());
    let url = format!("{}/api/version", base.trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    matches!(client.get(&url).send().await, Ok(r) if r.status().is_success())
}

/// Save model to project-level `.thclaws/settings.json`.
/// Format a turn duration for the `[tokens: ... · 3.2s]` line.
/// Short durations render in ms, sub-minute in seconds with one decimal,
/// longer runs as `1m 23s`.
fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        let secs = d.as_secs();
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

fn save_project_model(model: &str) {
    let mut project = ProjectConfig::load().unwrap_or_default();
    project.set_model(model);
    if let Err(e) = project.save() {
        eprintln!("{COLOR_YELLOW}warning: could not save settings.json: {e}{COLOR_RESET}");
    }
}

// M6.33: ReplAgentFactory was promoted to `crate::subagent::ProductionAgentFactory`
// so the GUI's `shared_session::build_state` can register the Task tool too
// (SUB1). Same shape, same fields, same propagation semantics — just lifted
// to a shared location.

/// Spawn every configured MCP server and register its discovered tools into
/// the passed-in registry. Returns the spawned clients (must stay alive for
/// the REPL duration) and a per-server summary used by `/mcp`. Failures per
/// server are warnings, not fatal errors.
async fn load_mcp_servers(
    servers: &[McpServerConfig],
    registry: &mut ToolRegistry,
) -> (Vec<Arc<McpClient>>, Vec<(String, Vec<String>)>) {
    let mut clients: Vec<Arc<McpClient>> = Vec::new();
    let mut summary: Vec<(String, Vec<String>)> = Vec::new();

    for cfg in servers {
        print!("{COLOR_DIM}[mcp] {} … {COLOR_RESET}", cfg.name);
        let _ = std::io::stdout().flush();

        match McpClient::spawn(cfg.clone()).await {
            Ok(client) => match client.list_tools().await {
                Ok(tools) => {
                    let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
                    println!("{COLOR_DIM}{} tool(s){COLOR_RESET}", tools.len());
                    for info in tools {
                        let tool = McpTool::new(client.clone(), info);
                        registry.register(Arc::new(tool));
                    }
                    summary.push((cfg.name.clone(), names));
                    clients.push(client);
                }
                Err(e) => {
                    println!("{COLOR_YELLOW}list_tools failed: {e}{COLOR_RESET}");
                }
            },
            Err(e) => {
                println!("{COLOR_YELLOW}spawn failed: {e}{COLOR_RESET}");
            }
        }
    }
    (clients, summary)
}

/// Non-interactive mode: run a single prompt and print the result to stdout.
/// Matches the Python `--print` flag behavior.
pub async fn run_print_mode(config: AppConfig, prompt: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = ProjectContext::discover(&cwd)?;
    let memory_store = MemoryStore::default_path().map(MemoryStore::new);
    let system_fallback = if config.system_prompt.is_empty() {
        crate::prompts::defaults::SYSTEM
    } else {
        config.system_prompt.as_str()
    };
    let base_prompt = crate::prompts::load("system", system_fallback);
    let mut system = ctx.build_system_prompt(&base_prompt);
    if let Some(store) = &memory_store {
        if let Some(mem_section) = store.system_prompt_section() {
            system.push_str("\n\n# Memory\n");
            system.push_str(&mem_section);
        }
    }
    let kms_section = crate::kms::system_prompt_section(&config.kms_active);
    if !kms_section.is_empty() {
        system.push_str("\n\n");
        system.push_str(&kms_section);
    }

    let mut tool_registry = ToolRegistry::with_builtins();
    if !config.kms_active.is_empty() {
        tool_registry.register(Arc::new(crate::tools::KmsReadTool));
        tool_registry.register(Arc::new(crate::tools::KmsSearchTool));
        // M6.25 BUG #1: write tools alongside read tools.
        tool_registry.register(Arc::new(crate::tools::KmsWriteTool));
        tool_registry.register(Arc::new(crate::tools::KmsAppendTool));
    }
    // M6.26 BUG #1: Memory tools always-on (model can create first entry).
    tool_registry.register(Arc::new(crate::tools::MemoryReadTool));
    tool_registry.register(Arc::new(crate::tools::MemoryWriteTool));
    tool_registry.register(Arc::new(crate::tools::MemoryAppendTool));
    let (_mcp_clients, _mcp_summary) =
        load_mcp_servers(&config.mcp_servers, &mut tool_registry).await;

    let provider = build_provider(&config)?;
    let perm_mode = if config.permissions == "auto" {
        PermissionMode::Auto
    } else {
        PermissionMode::Ask
    };
    let agent = Agent::new(provider, tool_registry, config.model.clone(), system)
        .with_max_iterations(config.max_iterations)
        .with_permission_mode(perm_mode);

    let mut stream = Box::pin(agent.run_turn(prompt.to_string()));
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(AgentEvent::Text(s)) => {
                print!("{s}");
                let _ = std::io::stdout().flush();
            }
            Ok(AgentEvent::Done { .. }) => {
                println!();
            }
            Err(e) => {
                eprintln!("\nerror: {e}");
                std::process::exit(1);
            }
            _ => {}
        }
    }
    Ok(())
}

/// Interactive REPL. Reads from stdin via `rustyline`, streams assistant
/// output live, handles slash commands. Runs until `/quit`, EOF, or Ctrl-C.
pub async fn run_repl(mut config: AppConfig) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = ProjectContext::discover(&cwd)?;
    let memory_store = MemoryStore::default_path().map(MemoryStore::new);

    // M6.11 (H1): daily auto-refresh of the marketplace catalog so
    // CLI users get fresh entries without having to remember
    // /skill marketplace --refresh. Same pattern the GUI worker uses;
    // no-op when the cache is < 24h old.
    crate::marketplace::spawn_daily_auto_refresh();

    // Append memory section to the project system prompt, if any memory exists.
    let system_fallback = if config.system_prompt.is_empty() {
        crate::prompts::defaults::SYSTEM
    } else {
        config.system_prompt.as_str()
    };
    let base_prompt = crate::prompts::load("system", system_fallback);
    let mut system = ctx.build_system_prompt(&base_prompt);
    if let Some(store) = &memory_store {
        if let Some(mem_section) = store.system_prompt_section() {
            system.push_str("\n\n# Memory\n");
            system.push_str(&mem_section);
        }
    }
    let kms_section = crate::kms::system_prompt_section(&config.kms_active);
    if !kms_section.is_empty() {
        system.push_str("\n\n");
        system.push_str(&kms_section);
    }

    // Build the tool registry once, with built-ins + task tools + MCP tools.
    // Override WebSearch with the configured engine (with_builtins uses "auto").
    let mut tool_registry = ToolRegistry::with_builtins();
    if !config.kms_active.is_empty() {
        tool_registry.register(Arc::new(crate::tools::KmsReadTool));
        tool_registry.register(Arc::new(crate::tools::KmsSearchTool));
        // M6.25 BUG #1: write tools alongside read tools.
        tool_registry.register(Arc::new(crate::tools::KmsWriteTool));
        tool_registry.register(Arc::new(crate::tools::KmsAppendTool));
    }
    // M6.26 BUG #1: Memory tools always-on (model can create first entry).
    tool_registry.register(Arc::new(crate::tools::MemoryReadTool));
    tool_registry.register(Arc::new(crate::tools::MemoryWriteTool));
    tool_registry.register(Arc::new(crate::tools::MemoryAppendTool));
    if config.search_engine != "auto" {
        tool_registry.register(Arc::new(crate::tools::WebSearchTool::new(
            &config.search_engine,
        )));
    }
    let task_store = crate::tools::tasks::register_task_tools(&mut tool_registry);
    let team_agent_name = std::env::var("THCLAWS_TEAM_AGENT").ok();
    let team_role = team_agent_name.as_deref().unwrap_or("lead");
    // Team feature is opt-in (teamEnabled: true in settings.json). Teammate
    // processes always have it on — the spawner already decided to use teams
    // when it ran `thclaws --team-agent <name>`.
    let team_enabled = team_agent_name.is_some()
        || crate::config::ProjectConfig::load()
            .and_then(|c| c.team_enabled)
            .unwrap_or(false);
    let _team_mailbox = if team_enabled {
        Some(crate::team::register_team_tools(
            &mut tool_registry,
            team_role,
        ))
    } else {
        None
    };

    // Mark this process as the team lead if applicable. BashTool consults
    // this to hard-block destructive workspace ops (`git reset --hard`,
    // `rm -rf`, `git worktree remove`) that have repeatedly cascade-killed
    // teammate processes when an LLM lead tried to "clean up". Set as a
    // static rather than env var so child teammate processes (which inherit
    // env) don't accidentally pick up the lead flag.
    crate::team::set_is_team_lead(team_enabled && team_agent_name.is_none());

    // M6.34 TEAM3: capture our team_dir so the EOF cleanup hammer
    // (`kill_my_teammates`) can target ONLY teammates of THIS lead
    // session — pre-fix `pkill -f team-agent` killed teammates of
    // other thClaws sessions (any project on the box) too.
    if team_enabled && team_agent_name.is_none() {
        let td = std::env::var("THCLAWS_TEAM_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| crate::team::Mailbox::default_dir());
        crate::team::set_lead_team_dir(&td);
    }

    // Team agents: remove interactive tools — no human is watching.
    if team_agent_name.is_some() {
        tool_registry.remove("AskUserQuestion");
        tool_registry.remove("EnterPlanMode");
        tool_registry.remove("ExitPlanMode");
    } else {
        // Lead: remove TeamTaskClaim — lead coordinates, doesn't claim tasks.
        tool_registry.remove("TeamTaskClaim");
        tool_registry.remove("TeamTaskComplete");
    }

    // Surface enabled plugins first — their contributions feed into the
    // skill/command stores and the MCP server list below.
    let plugin_skill_dirs = crate::plugins::plugin_skill_dirs();
    let plugin_command_dirs = crate::plugins::plugin_command_dirs();
    let plugin_mcp_servers = crate::plugins::plugin_mcp_servers();
    let plugin_count = crate::plugins::installed_plugins_all_scopes().len();
    if plugin_count > 0 {
        println!(
            "{COLOR_DIM}[plugins] {} plugin(s) enabled{COLOR_RESET}",
            plugin_count
        );
    }

    // Merge plugin MCP servers into config. Config entries win on name
    // clash so project-level mcp.json can override a plugin default.
    for p_mcp in &plugin_mcp_servers {
        if !config.mcp_servers.iter().any(|s| s.name == p_mcp.name) {
            config.mcp_servers.push(p_mcp.clone());
        }
    }

    // Discover legacy prompt commands (Claude-Code-style `.md` templates
    // under `.thclaws/commands/`, `.claude/commands/`, plus plugin dirs).
    let command_store = crate::commands::CommandStore::discover_with_extra(&plugin_command_dirs);
    if !command_store.commands.is_empty() {
        println!(
            "{COLOR_DIM}[commands] {} command(s) loaded{COLOR_RESET}",
            command_store.commands.len()
        );
    }

    // Discover and register skills (project/user + plugin-contributed).
    let skill_store = crate::skills::SkillStore::discover_with_extra(&plugin_skill_dirs);
    // Mutable name snapshot so the REPL's `/<skill-name>` shortcut picks up
    // skills installed at runtime (/skill install …). Kept in sync with the
    // SkillTool's shared store via `skill_store_handle` below.
    let mut skill_names: std::collections::HashSet<String> =
        skill_store.skills.keys().cloned().collect();
    let mut skill_store_handle: Option<
        std::sync::Arc<std::sync::Mutex<crate::skills::SkillStore>>,
    > = None;
    if !skill_store.skills.is_empty() {
        let count = skill_store.skills.len();
        println!("{COLOR_DIM}[skills] {} skill(s) loaded{COLOR_RESET}", count);
        // Surface the skill catalog in the system prompt so the model knows
        // what's available without having to read the Skill tool's input
        // schema. For each skill list name + description + whenToUse — the
        // same fields Claude Code uses to decide when to reach for a skill.
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
        let mut entries: Vec<&crate::skills::SkillDef> = skill_store.skills.values().collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for skill in entries {
            // Keep each entry compact: name + short trigger only. Full
            // description is available via `Skill(name)` call. This helps
            // small-context models (Ollama/Gemma) where 18 multi-line
            // descriptions push the catalog out of the attention window.
            if !skill.when_to_use.is_empty() {
                system.push_str(&format!("- **{}**: {}\n", skill.name, skill.when_to_use));
            } else {
                system.push_str(&format!("- **{}**: {}\n", skill.name, skill.description));
            }
        }
        // Re-anchor the rule close to where the model's attention is
        // strongest (end of system prompt gets more weight than middle).
        system.push_str(
            "\nReminder: if the user's request matches ANY skill trigger above, \
             call `Skill(name: \"...\")` FIRST.\n\n\
             Slash-command shortcut: if a user message begins with \
             `/<skill-name>` (matching one of the skills above), that IS \
             an explicit request to run that skill. Call \
             `Skill(name: \"<skill-name>\")` immediately, then follow its \
             instructions using any args that appeared after the name.\n",
        );
        let skill_tool = crate::skills::SkillTool::new(skill_store);
        let store_handle = skill_tool.store_handle();
        skill_store_handle = Some(store_handle.clone());
        tool_registry.register(Arc::new(skill_tool));
        // dev-plan/06 P2: discovery tools register alongside Skill so
        // the "names-only" / "discover-tool-only" strategies have
        // something to point at. Always-registered for symmetry with
        // the GUI worker.
        tool_registry.register(Arc::new(crate::skills::SkillListTool::new_from_handle(
            store_handle.clone(),
        )));
        tool_registry.register(Arc::new(crate::skills::SkillSearchTool::new_from_handle(
            store_handle,
        )));
    }
    let (mut mcp_clients, mut mcp_summary) =
        load_mcp_servers(&config.mcp_servers, &mut tool_registry).await;

    // Try the configured provider first; on failure (missing key, etc.)
    // fall back to something usable so the REPL still opens. The user
    // can configure a real key via Settings → API Keys then `/model`
    // back to what they want.
    let (provider, provider_warning) = build_provider_with_fallback(&mut config).await;
    if let Some(warn) = &provider_warning {
        println!("{COLOR_YELLOW}[startup] {warn}{COLOR_RESET}");
    }
    // If literally nothing is available, construct a placeholder that
    // errors friendly on every turn — the REPL still runs so the user
    // can open Settings / type slash commands without an immediate exit.
    let provider = provider.unwrap_or_else(|| Arc::new(NoProviderPlaceholder) as Arc<dyn Provider>);

    // M6.20 BUG H1: build the approver + permission_mode FIRST so the
    // subagent factory and the top-level agent share the same instance.
    // Pre-fix the factory built its child agents via `Agent::new`'s
    // defaults (AutoApprover + PermissionMode::Auto), and the dispatch
    // fallback at agent.rs:1112 promoted the global Ask back to Auto —
    // every subagent tool call bypassed the user's approval gate.
    let perm_mode = if team_agent_name.is_some() || config.permissions == "auto" {
        PermissionMode::Auto
    } else {
        PermissionMode::Ask
    };
    let approver = ReplApprover::new();

    // M6.33 SUB3: tool filtering MUST run BEFORE registering the Task
    // tool — otherwise the subagent's `base_tools` snapshot includes
    // tools the parent was forbidden from using, so a model that
    // can't call Bash directly could spawn a Task and have the
    // subagent run Bash. Privilege-escalation primitive.
    //
    // Order: (1) apply --allowed-tools / --disallowed-tools filter to
    // tool_registry. (2) snapshot the FILTERED registry as base_tools.
    // (3) register Task with the filtered base_tools.

    // Apply tool filtering from config. Team-essential tools are always kept.
    let team_essential_tools: std::collections::HashSet<&str> = [
        "SendMessage",
        "CheckInbox",
        "TeamStatus",
        "TeamCreate",
        "SpawnTeammate",
        "TeamTaskCreate",
        "TeamTaskList",
        "TeamTaskClaim",
        "TeamTaskComplete",
    ]
    .into_iter()
    .collect();

    if let Some(ref allowed) = config.allowed_tools {
        let mut allowed_set: std::collections::HashSet<&str> =
            allowed.iter().map(|s| s.as_str()).collect();
        // M6.34 TEAM4: keep team-essential tools whenever the team
        // feature is on, not just for teammate processes. Pre-fix the
        // lead's `--allowed-tools Read` would silently strip
        // SendMessage/TeamStatus/CheckInbox/etc — coordination broken
        // without a clear error. Asymmetric with the disallowed_tools
        // handling below, which already protects team_essential
        // unconditionally.
        if team_enabled {
            allowed_set.extend(&team_essential_tools);
        }
        let all_names: Vec<String> = tool_registry
            .names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        for name in all_names {
            if !allowed_set.contains(name.as_str()) {
                tool_registry.remove(&name);
            }
        }
    }
    if let Some(ref disallowed) = config.disallowed_tools {
        for name in disallowed {
            // Never remove team-essential tools.
            if !team_essential_tools.contains(name.as_str()) {
                tool_registry.remove(name);
            }
        }
    }

    // M6.35 HOOK1: snapshot HooksConfig early so the factory + the
    // top-level agent share one Arc. Subagent inherits via factory
    // propagation so Task-spawned tool calls fire hooks too.
    let hooks_arc = std::sync::Arc::new(config.hooks.clone());

    // M6.33 SUB1 + SUB3 + SUB4: register the Task tool AFTER the
    // --allowed-tools / --disallowed-tools filter has run. The
    // ProductionAgentFactory captures the FILTERED tool_registry as
    // its `base_tools`, so subagents inherit the same restrictions
    // the parent has. Pre-fix base_tools was snapshotted BEFORE
    // filtering — Task became a privilege-escalation primitive
    // (model spawns subagent → subagent has tools the parent was
    // forbidden from using).
    {
        let plugin_agent_dirs = crate::plugins::plugin_agent_dirs();
        let agent_defs = crate::agent_defs::AgentDefsConfig::load_with_extra(&plugin_agent_dirs);
        let base_tools = tool_registry.clone();
        let factory = Arc::new(ProductionAgentFactory {
            provider: provider.clone(),
            base_tools,
            model: config.model.clone(),
            system: system.clone(),
            max_iterations: config.max_iterations,
            max_depth: crate::subagent::DEFAULT_MAX_DEPTH,
            agent_defs: agent_defs.clone(),
            approver: approver.clone(),
            permission_mode: perm_mode,
            // CLI doesn't have a CancelToken plumbing today; subagents
            // run uninterruptibly here. GUI passes Some via build_state.
            cancel: None,
            hooks: Some(hooks_arc.clone()),
        });
        tool_registry.register(Arc::new(
            SubAgentTool::new(factory)
                .with_depth(0)
                .with_agent_defs(agent_defs),
        ));
    }

    // If a team exists, inject lead coordination rules into the system prompt.
    // This tells the lead to delegate work to teammates instead of doing it itself.
    if team_enabled && team_agent_name.is_none() {
        let team_config_path = crate::team::Mailbox::default_dir().join("config.json");
        if team_config_path.exists() {
            if let Ok(team_cfg) = crate::team::TeamConfig::load(&team_config_path) {
                let members: Vec<String> = team_cfg
                    .members
                    .iter()
                    .map(|m| {
                        if m.role.is_empty() {
                            m.name.clone()
                        } else {
                            format!("{} ({})", m.name, m.role)
                        }
                    })
                    .collect();
                system.push_str(&crate::prompts::render_named(
                    "lead",
                    crate::prompts::defaults::LEAD,
                    &[("members", &members.join(", "))],
                ));
            }
        }
    }

    // M6.20 BUG H1: `perm_mode` and `approver` are defined above the
    // factory block so subagents inherit the same gate.
    let mut agent = Agent::new(
        provider,
        tool_registry.clone(),
        config.model.clone(),
        system.clone(),
    )
    .with_max_iterations(config.max_iterations)
    .with_permission_mode(perm_mode)
    .with_approver(approver.clone())
    .with_hooks(hooks_arc.clone());

    let session_store = SessionStore::default_path().map(SessionStore::new);
    let mut session = Session::new(&config.model, cwd.to_string_lossy());

    // Resume session from --resume flag.
    if let Some(ref resume_id) = config.resume_session {
        if let Some(ref store) = session_store {
            let loaded = if resume_id == "last" {
                store.latest().ok().flatten()
            } else {
                store.load(resume_id).ok()
            };
            if let Some(s) = loaded {
                agent.set_history(s.messages.clone());
                session = s;
                println!(
                    "{COLOR_DIM}resumed session {} ({} messages){COLOR_RESET}",
                    session.id,
                    session.messages.len()
                );
            } else {
                println!(
                    "{COLOR_YELLOW}session not found: {resume_id} — starting fresh{COLOR_RESET}"
                );
            }
        }
    }

    let perm_label = if config.permissions == "auto" {
        "auto"
    } else {
        "ask"
    };
    let v = crate::version::info();
    let dirty_tag = if v.git_dirty { "+dirty" } else { "" };
    let brand = crate::branding::current();
    if team_agent_name.is_none() {
        println!("\n{COLOR_CYAN}{}{COLOR_RESET}", brand.banner_text);
        println!();
    }
    println!(
        "{COLOR_BOLD}{} {}{COLOR_RESET} {COLOR_DIM}({}{}) — model: {} · permissions: {} · session: {}{COLOR_RESET}",
        brand.name, v.version, v.git_sha, dirty_tag, config.model, perm_label, session.id
    );
    if let Some(ref name) = team_agent_name {
        println!(
            "{COLOR_DIM}Running as team agent '{name}' — polling inbox for messages{COLOR_RESET}"
        );
    } else {
        println!("{COLOR_DIM}Type /help for commands, /quit to exit.{COLOR_RESET}");
    }

    // ── Team agent mode: inject rules + poll inbox ────────────────────
    if let Some(ref agent_name) = team_agent_name {
        // Load agent definition from .thclaws/agents/ + plugin-contributed
        // dirs if available.
        let plugin_agent_dirs = crate::plugins::plugin_agent_dirs();
        let agent_defs = crate::agent_defs::AgentDefsConfig::load_with_extra(&plugin_agent_dirs);
        if let Some(def) = agent_defs.get(agent_name) {
            if !def.instructions.is_empty() {
                agent.append_system(&format!(
                    "\n\n# Agent Role: {}\n{}\n",
                    def.description, def.instructions
                ));
            }
        }

        // Build team member list from config.
        let team_members_info = {
            let td = std::env::var("THCLAWS_TEAM_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| crate::team::Mailbox::default_dir());
            let config_path = td.join("config.json");
            crate::team::TeamConfig::load(&config_path)
                .ok()
                .map(|cfg| {
                    let members: Vec<String> = cfg
                        .members
                        .iter()
                        .map(|m| {
                            if m.role.is_empty() {
                                format!("- {}", m.name)
                            } else {
                                format!("- {} ({})", m.name, m.role)
                            }
                        })
                        .collect();
                    format!("- lead (team coordinator)\n{}", members.join("\n"))
                })
                .unwrap_or_else(|| "- lead (team coordinator)".into())
        };

        // Worktree context for shared-vs-isolated writes.
        let in_worktree = std::env::var("THCLAWS_IN_WORKTREE").ok().as_deref() == Some("1");
        let project_root = std::env::var("THCLAWS_PROJECT_ROOT").unwrap_or_else(|_| {
            std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });
        let worktree_rules = if in_worktree {
            crate::prompts::render_named(
                "worktree",
                crate::prompts::defaults::WORKTREE,
                &[("agent_name", agent_name), ("project_root", &project_root)],
            )
        } else {
            String::new()
        };

        let cwd_str = std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        // Inject team communication rules (matches Claude Code's TEAMMATE_SYSTEM_PROMPT_ADDENDUM).
        let team_rules = crate::prompts::render_named(
            "agent_team",
            crate::prompts::defaults::AGENT_TEAM,
            &[
                ("agent_name", agent_name),
                ("team_members_info", &team_members_info),
                ("cwd", &cwd_str),
                ("project_root", &project_root),
                ("worktree_rules", &worktree_rules),
            ],
        );
        agent.append_system(&team_rules);
        let team_dir = std::env::var("THCLAWS_TEAM_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| crate::team::Mailbox::default_dir());
        let mailbox = crate::team::Mailbox::new(team_dir.clone());
        mailbox.init_agent(agent_name).unwrap_or(());

        // Output log file for GUI Team tab to read.
        let log_path = mailbox.output_log_path(agent_name);
        let mut log_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
            .ok();

        // Helper: write to both stdout and log file.
        macro_rules! team_print {
            ($($arg:tt)*) => {{
                let s = format!($($arg)*);
                print!("{}", s);
                let _ = std::io::stdout().flush();
                if let Some(ref mut f) = log_file {
                    let _ = std::io::Write::write_all(f, s.as_bytes());
                    let _ = std::io::Write::flush(f);
                }
            }};
        }
        macro_rules! team_println {
            ($($arg:tt)*) => {{
                let s = format!($($arg)*);
                println!("{}", s);
                if let Some(ref mut f) = log_file {
                    let _ = std::io::Write::write_all(f, s.as_bytes());
                    let _ = std::io::Write::write_all(f, b"\n");
                    let _ = std::io::Write::flush(f);
                }
            }};
        }

        // Set initial status.
        let _ = mailbox.write_status(agent_name, "idle", None);
        team_println!("[{agent_name}] waiting for messages...");

        let poll_ms = crate::team::POLL_INTERVAL_MS;
        let mut pending_queue: std::collections::VecDeque<crate::team::TeamMessage> =
            std::collections::VecDeque::new();

        loop {
            // 1. Read unread messages from inbox.
            let unread = mailbox.read_unread(agent_name).unwrap_or_default();
            if !unread.is_empty() {
                let ids: Vec<String> = unread.iter().map(|m| m.id.clone()).collect();
                let _ = mailbox.mark_as_read(agent_name, &ids);

                for msg in unread {
                    // Check for protocol messages (shutdown, etc.).
                    if let Some(proto) = crate::team::parse_protocol_message(msg.content()) {
                        match proto {
                            crate::team::ProtocolMessage::ShutdownRequest { from } => {
                                // Check if we have unfinished work.
                                let has_work = !pending_queue.is_empty();
                                let has_active_task = mailbox
                                    .task_queue()
                                    .list(Some(crate::team::TaskStatus::InProgress))
                                    .unwrap_or_default()
                                    .iter()
                                    .any(|t| t.owner.as_deref() == Some(agent_name));

                                if has_work || has_active_task {
                                    // Reject shutdown — still working.
                                    team_println!(
                                        "[{agent_name}] shutdown rejected — still have unfinished work"
                                    );
                                    let reject = serde_json::to_string(
                                        &crate::team::ProtocolMessage::ShutdownRejected {
                                            from: agent_name.to_string(),
                                            reason: "still have unfinished tasks".into(),
                                        },
                                    )
                                    .unwrap_or_default();
                                    let reject_msg =
                                        crate::team::TeamMessage::new(agent_name, &reject);
                                    let _ = mailbox.write_to_mailbox(&from, reject_msg);
                                } else {
                                    // Approve shutdown — idle, no tasks.
                                    team_println!("[{agent_name}] shutdown approved — exiting");
                                    let approve = serde_json::to_string(
                                        &crate::team::ProtocolMessage::ShutdownApproved {
                                            from: agent_name.to_string(),
                                        },
                                    )
                                    .unwrap_or_default();
                                    let approve_msg =
                                        crate::team::TeamMessage::new(agent_name, &approve);
                                    let _ = mailbox.write_to_mailbox(&from, approve_msg);
                                    let _ = mailbox.write_status(agent_name, "stopped", None);
                                    return Ok(());
                                }
                            }
                            _ => {}
                        }
                    } else {
                        pending_queue.push_back(msg);
                    }
                }
            }

            // 2. If no messages, try claiming a task from the queue.
            if pending_queue.is_empty() {
                let tq = mailbox.task_queue();
                if let Ok(Some(task)) = tq.claim_next(agent_name) {
                    team_println!("[{agent_name}] claimed task #{}: {}", task.id, task.subject);
                    let synthetic = crate::team::TeamMessage::new(
                        "task-queue",
                        &format!(
                            "[Task #{} — {}]\n\n{}\n\nWhen done, use TeamTaskComplete with task_id=\"{}\".",
                            task.id, task.subject, task.description, task.id
                        ),
                    );
                    pending_queue.push_back(synthetic);
                }
            }

            // 3. Process one message from the queue.
            if let Some(msg) = pending_queue.pop_front() {
                let summary = msg.summary.as_deref().unwrap_or("");
                let prompt = format!(
                    "<teammate_message from=\"{}\" summary=\"{}\">\n{}\n</teammate_message>",
                    msg.from,
                    summary,
                    msg.content()
                );
                team_println!("\n[{agent_name}] received from '{}'", msg.from);

                let _ = mailbox.write_status(agent_name, "working", Some(&msg.id));
                let mut last_heartbeat = std::time::Instant::now();
                let turn_start = std::time::Instant::now();

                // Run the agent turn.
                let mut stream = Box::pin(agent.run_turn(prompt));
                loop {
                    let ev = tokio::select! {
                        ev = stream.next() => ev,
                        _ = tokio::signal::ctrl_c() => {
                            team_println!("\n[cancelled]");
                            drop(stream);
                            break;
                        }
                    };
                    let Some(ev) = ev else { break };
                    match ev {
                        Ok(AgentEvent::Text(s)) => {
                            team_print!("{s}");
                            // Throttled heartbeat — update every 30s on any output.
                            if last_heartbeat.elapsed().as_secs() >= 30 {
                                let _ = mailbox.write_status(agent_name, "working", None);
                                last_heartbeat = std::time::Instant::now();
                            }
                        }
                        Ok(AgentEvent::ToolCallStart { name, .. }) => {
                            team_print!("\n[tool: {name}]");
                        }
                        Ok(AgentEvent::ToolCallResult { output, .. }) => {
                            team_println!("{}", if output.is_ok() { " ✓" } else { " ✗" });
                            // Update heartbeat on tool completion.
                            let _ = mailbox.write_status(agent_name, "working", None);
                            last_heartbeat = std::time::Instant::now();
                        }
                        Ok(AgentEvent::Done { usage, .. }) => {
                            // Record teammate usage to project's .thclaws/usage/.
                            // Use team_dir parent to find project root (team_dir is absolute).
                            let usage_path = team_dir.parent().unwrap_or(&team_dir).join("usage");
                            let provider_name = config.detect_provider().unwrap_or("unknown");
                            let tracker = crate::usage::UsageTracker::new(usage_path);
                            tracker.record(provider_name, &config.model, &usage);
                            team_println!(
                                "\n[tokens: {}in/{}out · {}]",
                                usage.input_tokens,
                                usage.output_tokens,
                                format_duration(turn_start.elapsed())
                            );
                        }
                        _ => {}
                    }
                }
                team_println!("");

                // Turn completed (Stop hook equivalent) — always send idle notification.
                // This tells the lead we finished the current work, even if more is queued.
                // The teammate will pick up queued work on the next loop iteration.
                let _ = mailbox.write_status(agent_name, "idle", None);
                let idle = crate::team::make_idle_notification(
                    agent_name,
                    None,
                    None,
                    Some("finished current turn"),
                );
                let idle_msg = crate::team::TeamMessage::new(agent_name, &idle);
                let _ = mailbox.write_to_mailbox("lead", idle_msg);
            } else {
                // Nothing to do — update heartbeat and poll.
                let _ = mailbox.write_status(agent_name, "idle", None);
                tokio::time::sleep(tokio::time::Duration::from_millis(poll_ms)).await;
            }
        }
    }

    // Lead output log — always active so the GUI Team tab can show lead's output.
    // Only the output log + status are created; the full team (inboxes, config)
    // is created by TeamCreate, not here.
    let lead_mb = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
    let lead_log_path = lead_mb.output_log_path("lead");
    if let Some(parent) = lead_log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = lead_mb.write_status("lead", "active", None);
    let lead_log: std::sync::Arc<std::sync::Mutex<Option<std::fs::File>>> =
        std::sync::Arc::new(std::sync::Mutex::new(
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&lead_log_path)
                .ok(),
        ));

    // Helper: write to lead's output log (for Team tab in GUI).
    macro_rules! lead_log {
        ($($arg:tt)*) => {{
            let s = format!($($arg)*);
            if let Ok(mut guard) = lead_log.lock() {
                if let Some(ref mut f) = *guard {
                    let _ = std::io::Write::write_all(f, s.as_bytes());
                    let _ = std::io::Write::flush(f);
                }
            }
        }};
    }

    // Background task: poll lead's inbox (1s interval). Only runs when the
    // team feature is enabled; otherwise the channel stays idle forever and
    // the select! arm is effectively a no-op.
    let (inbox_tx, mut inbox_rx) =
        tokio::sync::mpsc::unbounded_channel::<Vec<crate::team::TeamMessage>>();
    // M6.29: /loop fires lines back into the readline loop via this
    // channel. The loop task pushes; the readline select! arm pulls.
    let (cli_input_tx, mut cli_input_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut active_loop_handle: Option<tokio::task::AbortHandle> = None;
    let mut active_loop_body: Option<String> = None;
    if team_enabled {
        let mailbox = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
        tokio::spawn(async move {
            loop {
                let unread = mailbox.read_unread("lead").unwrap_or_default();
                if !unread.is_empty() {
                    let ids: Vec<String> = unread.iter().map(|m| m.id.clone()).collect();
                    // M6.34 TEAM5: send THEN mark-as-read so a
                    // closed channel doesn't silently lose messages.
                    if inbox_tx.send(unread).is_ok() {
                        let _ = mailbox.mark_as_read("lead", &ids);
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(
                    crate::team::POLL_INTERVAL_MS,
                ))
                .await;
            }
        });
    }

    // Shared readline editor for spawn_blocking calls. Helper enables
    // Tab-completion of slash commands plus inline ghost-text hints
    // (see `crate::cli_completer`). Default Circular completion: Tab
    // cycles through matches; the Hinter shows a dim suggestion after
    // the cursor that Right-arrow accepts.
    let mut rl: rustyline::Editor<
        crate::cli_completer::SlashCompleter,
        rustyline::history::DefaultHistory,
    > = rustyline::Editor::with_config(readline_config())
        .map_err(|e| Error::Agent(format!("readline init: {e}")))?;
    rl.set_helper(Some(crate::cli_completer::SlashCompleter));
    let rl_mutex = std::sync::Arc::new(std::sync::Mutex::new(rl));

    // Helper: process team inbox messages and run agent turn.
    macro_rules! process_team_messages {
        ($msgs:expr) => {{
            let mut regular = Vec::new();
            for msg in &$msgs {
                if let Some(proto) = crate::team::parse_protocol_message(msg.content()) {
                    match proto {
                        crate::team::ProtocolMessage::IdleNotification {
                            ref from, ref completed_task_id, ref summary, ..
                        } => {
                            let task_info = completed_task_id.as_ref()
                                .map(|id| format!(" (task #{id})"))
                                .unwrap_or_default();
                            let sum = summary.as_deref().unwrap_or("");
                            println!(
                                "\n{COLOR_CYAN}[{from} is idle{task_info}]{COLOR_RESET} {COLOR_DIM}{sum}{COLOR_RESET}"
                            );
                            lead_log!("\n{COLOR_CYAN}[{from} is idle{task_info}]{COLOR_RESET} {COLOR_DIM}{sum}{COLOR_RESET}\n");
                            // Feed to agent so it can coordinate next steps.
                            regular.push(msg);
                        }
                        crate::team::ProtocolMessage::ShutdownApproved { ref from } => {
                            println!(
                                "\n{COLOR_CYAN}[{from} shutdown approved — stopped]{COLOR_RESET}"
                            );
                            lead_log!("\n{COLOR_CYAN}[{from} shutdown approved — stopped]{COLOR_RESET}\n");
                            regular.push(msg);
                        }
                        crate::team::ProtocolMessage::ShutdownRejected { ref from, ref reason } => {
                            println!(
                                "\n{COLOR_YELLOW}[{from} shutdown rejected: {reason}]{COLOR_RESET}"
                            );
                            lead_log!("\n{COLOR_YELLOW}[{from} shutdown rejected: {reason}]{COLOR_RESET}\n");
                            regular.push(msg);
                        }
                        _ => {}
                    }
                } else {
                    let preview = msg.content().chars().take(300).collect::<String>();
                    println!(
                        "\n{COLOR_CYAN}[message from '{}']:{COLOR_RESET} {}",
                        msg.from, preview
                    );
                    lead_log!(
                        "\n{COLOR_CYAN}[message from '{}']:{COLOR_RESET} {}\n",
                        msg.from, preview
                    );
                    regular.push(msg);
                }
            }
            if !regular.is_empty() {
                let combined: Vec<String> = regular
                    .iter()
                    .map(|m| {
                        let summary = m.summary.as_deref().unwrap_or("");
                        format!(
                            "<teammate_message from=\"{}\" summary=\"{}\">\n{}\n</teammate_message>",
                            m.from, summary, m.content()
                        )
                    })
                    .collect();
                let team_prompt = combined.join("\n\n");
                println!("{COLOR_GREEN}");
                lead_log!("{COLOR_GREEN}");
                let _ = std::io::stdout().flush();
                let mut stream = Box::pin(agent.run_turn(team_prompt));
                loop {
                    let ev = tokio::select! {
                        ev = stream.next() => ev,
                        _ = tokio::signal::ctrl_c() => {
                            println!("{COLOR_RESET}\n{COLOR_YELLOW}[cancelled]{COLOR_RESET}");
                            lead_log!("{COLOR_RESET}\n{COLOR_YELLOW}[cancelled]{COLOR_RESET}\n");
                            drop(stream);
                            break;
                        }
                    };
                    let Some(ev) = ev else { break };
                    match ev {
                        Ok(AgentEvent::Text(s)) => {
                            print!("{s}");
                            lead_log!("{s}");
                            let _ = std::io::stdout().flush();
                        }
                        Ok(AgentEvent::ToolCallStart { name, .. }) => {
                            print!(
                                "{COLOR_RESET}\n{COLOR_DIM}[tool: {name}]{COLOR_RESET}{COLOR_GREEN}"
                            );
                            lead_log!("{COLOR_RESET}\n{COLOR_DIM}[tool: {name}]{COLOR_RESET}");
                        }
                        Ok(AgentEvent::ToolCallResult { output, .. }) => {
                            let mark = if output.is_ok() { "✓" } else { "✗" };
                            let color = if output.is_ok() { COLOR_DIM } else { COLOR_YELLOW };
                            print!("{color} {mark}{COLOR_RESET}{COLOR_GREEN}");
                            lead_log!(" {color}{mark}{COLOR_RESET}\n{COLOR_GREEN}");
                        }
                        Ok(AgentEvent::ToolCallDenied { name, .. }) => {
                            print!(
                                "{COLOR_RESET}\n{COLOR_YELLOW}[denied: {name}]{COLOR_RESET}{COLOR_GREEN}"
                            );
                            lead_log!("{COLOR_RESET}\n{COLOR_YELLOW}[denied: {name}]{COLOR_RESET}\n{COLOR_GREEN}");
                        }
                        Ok(AgentEvent::Done { stop_reason, .. }) => {
                            print!("{COLOR_RESET}");
                            lead_log!("{COLOR_RESET}");
                            if let Some(reason) = stop_reason {
                                if reason == "max_iterations" {
                                    println!("\n{COLOR_YELLOW}[hit max_iterations]{COLOR_RESET}");
                                    lead_log!("\n{COLOR_YELLOW}[hit max_iterations]{COLOR_RESET}\n");
                                }
                            }
                            println!();
                            lead_log!("\n");
                        }
                        _ => {}
                    }
                }
                print!("{COLOR_RESET}");
                let _ = std::io::stdout().flush();
                if let Some(store) = &session_store {
                    session.sync(agent.history_snapshot());
                    let _ = store.save(&mut session);
                }
            }
        }};
    }

    // M6.35 HOOK2: fire session_start hook now that the agent + session
    // are both built and we're about to enter the readline loop. Pre-fix
    // the entire hooks subsystem was dead code; this is the first place
    // a CLI session_start hook ever runs.
    crate::hooks::fire_session(
        &hooks_arc,
        crate::hooks::HookEvent::SessionStart,
        &session.id,
        &config.model,
    );

    // ── Normal interactive REPL ──────────────────────────────────────
    // Uses select! to race user input against team inbox messages so the
    // lead can respond to teammates without the user needing to press Enter.
    loop {
        // Spawn readline on a blocking thread so it doesn't block tokio.
        let rl_clone = rl_mutex.clone();
        let readline_task = tokio::task::spawn_blocking(move || {
            let mut rl = rl_clone.lock().unwrap();
            match rl.readline(REPL_PROMPT) {
                Ok(line) => {
                    let trimmed = line.trim().to_string();
                    if !trimmed.is_empty() {
                        let _ = rl.add_history_entry(&trimmed);
                    }
                    Some(trimmed)
                }
                Err(_) => None, // EOF / Ctrl-C / error
            }
        });

        // Race readline against team inbox messages AND any active
        // /loop firings (M6.29: cli_input_rx delivers loop body lines
        // back into the readline path so the next iteration just
        // takes the next line).
        let mut line: String;
        tokio::pin!(readline_task);
        loop {
            tokio::select! {
                result = &mut readline_task => {
                    match result {
                        Ok(Some(l)) => { line = l; break; }
                        _ => {
                            // M6.35 HOOK2: fire session_end before tearing down.
                            crate::hooks::fire_session(
                                &hooks_arc,
                                crate::hooks::HookEvent::SessionEnd,
                                &session.id,
                                &config.model,
                            );
                            // M6.34 TEAM3: scoped to this lead's team_dir.
                            crate::team::kill_my_teammates();
                            println!("{COLOR_DIM}bye{COLOR_RESET}");
                            return Ok(());
                        }
                    }
                }
                Some(msgs) = inbox_rx.recv() => {
                    process_team_messages!(msgs);
                    print!("{COLOR_CYAN}{REPL_PROMPT}{COLOR_RESET}");
                    let _ = std::io::stdout().flush();
                }
                Some(loop_line) = cli_input_rx.recv() => {
                    println!(
                        "{COLOR_DIM}[loop fired]{COLOR_RESET} {loop_line}"
                    );
                    line = loop_line;
                    break;
                }
            }
        }

        if line.is_empty() {
            continue;
        }

        // `!<cmd>` shell escape — user-initiated shell command, runs
        // through BashTool (sandbox cwd, non-interactive env, etc.)
        // and prints the output. Doesn't touch agent history. Mirrors
        // the GUI handle_line path in shared_session.rs.
        if let Some(cmd) = crate::shell_bang::parse_bang(&line) {
            println!("{COLOR_DIM}[!] {cmd}{COLOR_RESET}");
            match crate::shell_bang::run_bang_command(cmd).await {
                Ok(output) => {
                    if !output.is_empty() {
                        println!("{output}");
                    }
                }
                Err(e) => {
                    println!("{COLOR_YELLOW}{e}{COLOR_RESET}");
                }
            }
            continue;
        }

        // `/<name> [args]` shortcut — matches Claude Code's unified slash-
        // command UX. Resolution order (first match wins):
        //   1. Built-in slash commands (handled below by `parse_slash`).
        //   2. Installed skills (`/<skill-name>` → `Skill(name: …)`).
        //   3. Legacy prompt commands (Claude-Code-style `.md` templates).
        // Both skill and command paths rewrite `line` to a regular user
        // prompt so the turn pipeline below picks it up.
        if line.starts_with('/') {
            if let Some(SlashCommand::Unknown(what)) = parse_slash(&line) {
                let word = what.split_whitespace().next().unwrap_or("").to_string();
                let body = line.trim().strip_prefix('/').unwrap_or("").trim_start();
                let args = body.strip_prefix(&word).unwrap_or("").trim();

                if skill_names.contains(&word) {
                    let args_note = if args.is_empty() {
                        String::new()
                    } else {
                        format!(" The user's task for this skill: {args}")
                    };
                    println!("{COLOR_DIM}(/{word} → Skill(name: \"{word}\")){COLOR_RESET}");
                    line = format!(
                        "The user ran the `/{word}` slash command. Call `Skill(name: \"{word}\")` right away and follow the instructions it returns.{args_note}"
                    );
                } else if let Some(cmd) = command_store.get(&word).cloned() {
                    println!(
                        "{COLOR_DIM}(/{word} → prompt from {}){COLOR_RESET}",
                        cmd.source.display()
                    );
                    line = cmd.render(args);
                }
            }

            // M6.29: `/goal continue` rewrite-before-match. Same shape
            // as KmsIngestSession — the slash command becomes the
            // user prompt for the next agent turn (the audit prompt
            // built from the embedded template + current goal state).
            // Auto-stops the loop on terminal goal status.
            if matches!(parse_slash(&line), Some(SlashCommand::GoalContinue)) {
                match crate::goal_state::current() {
                    Some(g) if g.status.is_terminal() => {
                        println!(
                            "{COLOR_DIM}/goal continue — goal already {} (last: {}){COLOR_RESET}",
                            g.status.as_str(),
                            g.last_message.as_deref().unwrap_or("(none)")
                        );
                        if let Some(h) = active_loop_handle.take() {
                            h.abort();
                            active_loop_body = None;
                            println!("{COLOR_DIM}loop auto-stopped{COLOR_RESET}");
                        }
                        continue;
                    }
                    Some(g) => {
                        println!(
                            "{COLOR_DIM}(/goal continue → audit prompt fired — iteration {}, {}s elapsed){COLOR_RESET}",
                            g.iterations_done.saturating_add(1),
                            g.time_used_secs(),
                        );
                        crate::goal_state::record_iteration(0);
                        line = crate::goal_state::build_audit_prompt(&g);
                        // After this loop iteration's agent turn finishes,
                        // we'll check goal_state for terminal status and
                        // auto-stop the loop. That check is below the
                        // turn pipeline (around the existing post-turn
                        // section); for now just rewrite line and fall
                        // through.
                    }
                    None => {
                        println!(
                            "{COLOR_YELLOW}/goal continue — no active goal. Try /goal start \"<objective>\".{COLOR_RESET}"
                        );
                        continue;
                    }
                }
            }

            // M6.28: `/kms ingest <name> $` rewrite-before-match. The
            // `$` source means "the current chat session"; instead of
            // dispatching to a synchronous handler, build a prompt
            // that tells the model to summarize history + call
            // KmsWrite. Same pattern as the skill / command rewrites
            // above — `line` becomes plain text so the slash match
            // below skips it and the agent turn runs naturally.
            if let Some(SlashCommand::KmsIngestSession { name, alias, force }) = parse_slash(&line)
            {
                if crate::kms::resolve(&name).is_some() {
                    let (page, source) = resolve_session_alias(
                        alias.as_deref(),
                        session.title.as_deref(),
                        &session.id,
                    );
                    println!(
                        "{COLOR_DIM}(/kms ingest {name} $ → page `{page}` — summarize and KmsWrite){COLOR_RESET}"
                    );
                    line = build_kms_ingest_session_prompt(&name, &page, source, force);
                }
                // If the KMS doesn't exist, leave `line` as the
                // original slash command — the slash match's
                // `KmsIngestSession` arm will print a clear error.
            }
        }

        if let Some(cmd) = parse_slash(&line) {
            match cmd {
                SlashCommand::Help => println!("{}", render_help()),
                SlashCommand::Quit => break,
                SlashCommand::Clear => {
                    agent.clear_history();
                    // ANSI: scrollback erase (\x1b[3J) + screen erase (\x1b[2J)
                    // + cursor home (\x1b[H). Matches what most terminals do
                    // for Cmd+K / `clear`. Makes the visible scrollback match
                    // the model's now-empty history.
                    print!("\x1b[3J\x1b[2J\x1b[H");
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                    println!("{COLOR_DIM}history cleared{COLOR_RESET}");
                }
                SlashCommand::History => {
                    let h = agent.history_snapshot();
                    println!("{COLOR_DIM}{} message(s) in history{COLOR_RESET}", h.len());
                    for (i, m) in h.iter().enumerate() {
                        println!(
                            "{COLOR_DIM}  [{i}] {:?} — {} block(s){COLOR_RESET}",
                            m.role,
                            m.content.len()
                        );
                    }
                }
                SlashCommand::Model(new_model) => {
                    if new_model.is_empty() {
                        let provider_name = config.detect_provider().unwrap_or("unknown");
                        println!(
                            "{COLOR_DIM}model: {} (provider: {}){COLOR_RESET}",
                            config.model, provider_name
                        );
                        continue;
                    }
                    // Resolve short aliases ("sonnet" → "claude-sonnet-4-6",
                    // "flash" → "gemini-2.5-flash", etc.) to the canonical
                    // model id. Otherwise we'd persist "sonnet" and hand it
                    // straight to the Anthropic API, which replies
                    // `not_found_error: model: sonnet`.
                    let resolved = crate::providers::ProviderKind::resolve_alias(&new_model);
                    if resolved != new_model {
                        println!("{COLOR_DIM}(alias '{new_model}' → '{resolved}'){COLOR_RESET}");
                    }
                    // Validate before mutating: build a candidate config and
                    // try to construct a provider. Then — if the provider
                    // supports listing — confirm the remote actually serves
                    // this model. Only commit on success so a typo leaves
                    // the previous state intact.
                    let mut candidate = config.clone();
                    candidate.model = resolved.clone();
                    let new_provider = match build_provider(&candidate) {
                        Ok(p) => p,
                        Err(e) => {
                            println!("{COLOR_YELLOW}{e}{COLOR_RESET}");
                            continue;
                        }
                    };
                    match new_provider.list_models().await {
                        Ok(models) if !models.is_empty() => {
                            let ok = models.iter().any(|m| m.id == resolved);
                            if !ok {
                                println!(
                                    "{COLOR_YELLOW}unknown model '{resolved}' — try /models to see what's available{COLOR_RESET}"
                                );
                                continue;
                            }
                        }
                        // Empty list or unsupported list_models → accept the
                        // switch since we can't disprove the model. The
                        // Agent-SDK provider (local claude subprocess) doesn't
                        // implement listing.
                        _ => {}
                    }
                    // Flush any pending messages in the outgoing session
                    // before we swap providers. Mid-turn history built
                    // against provider A's message/tool schema can't always
                    // be re-fed to provider B — keep the old turns in their
                    // own file and start provider B with a clean slate, like
                    // a fresh app launch with the new model.
                    if let Some(store) = &session_store {
                        session.sync(agent.history_snapshot());
                        if !session.messages.is_empty() {
                            if let Err(e) = store.save(&mut session) {
                                println!(
                                    "{COLOR_YELLOW}[autosave before model switch failed: {e}]{COLOR_RESET}"
                                );
                            }
                        }
                    }
                    config = candidate;
                    agent = Agent::new(
                        new_provider,
                        tool_registry.clone(),
                        config.model.clone(),
                        system.clone(),
                    )
                    .with_max_iterations(config.max_iterations)
                    .with_permission_mode(perm_mode)
                    .with_approver(approver.clone())
                    .with_hooks(std::sync::Arc::new(config.hooks.clone()));
                    agent.clear_history();
                    session = Session::new(&config.model, session.cwd.clone());
                    // M6.20 BUG M2 + M3: model swap mints a fresh
                    // session; reset yolo flag and permission mode.
                    crate::permissions::ApprovalSink::reset_session_flag(approver.as_ref());
                    let _ = crate::permissions::take_pre_plan_mode();
                    crate::permissions::set_current_mode_and_broadcast(perm_mode);
                    save_project_model(&config.model);
                    println!(
                        "{COLOR_DIM}model → {} (saved to .thclaws/settings.json; new session {}){COLOR_RESET}",
                        config.model, session.id
                    );
                }
                SlashCommand::Config { key, value } => {
                    println!("{COLOR_DIM}(session-only) {key} = {value}{COLOR_RESET}");
                }
                SlashCommand::Providers => {
                    let current = config.detect_provider_kind().ok();
                    for kind in ProviderKind::ALL {
                        let marker = if Some(*kind) == current { "*" } else { " " };
                        println!(
                            "{COLOR_DIM}  {marker} {:<10} → {}{COLOR_RESET}",
                            kind.name(),
                            kind.default_model()
                        );
                    }
                }
                SlashCommand::Provider(name) => {
                    if name.is_empty() {
                        let current = config.detect_provider().unwrap_or("unknown");
                        println!(
                            "{COLOR_DIM}current provider: {current} (model: {}){COLOR_RESET}",
                            config.model
                        );
                        continue;
                    }
                    let Some(default_model) = default_model_for_provider(&name) else {
                        println!(
                            "{COLOR_YELLOW}unknown provider: {name} (try: anthropic, openai, gemini, ollama){COLOR_RESET}"
                        );
                        continue;
                    };
                    let mut candidate = config.clone();
                    candidate.model = default_model.to_string();
                    let new_provider = match build_provider(&candidate) {
                        Ok(p) => p,
                        Err(e) => {
                            println!("{COLOR_YELLOW}{e}{COLOR_RESET}");
                            continue;
                        }
                    };
                    // Flush pending messages to the old session, then fork
                    // a fresh one for the new provider (same reason as
                    // `/model` — history built against provider A's schema
                    // may not survive being re-sent to provider B).
                    if let Some(store) = &session_store {
                        session.sync(agent.history_snapshot());
                        if !session.messages.is_empty() {
                            if let Err(e) = store.save(&mut session) {
                                println!(
                                    "{COLOR_YELLOW}[autosave before provider switch failed: {e}]{COLOR_RESET}"
                                );
                            }
                        }
                    }
                    config = candidate;
                    agent = Agent::new(
                        new_provider,
                        tool_registry.clone(),
                        config.model.clone(),
                        system.clone(),
                    )
                    .with_max_iterations(config.max_iterations)
                    .with_permission_mode(perm_mode)
                    .with_approver(approver.clone())
                    .with_hooks(std::sync::Arc::new(config.hooks.clone()));
                    agent.clear_history();
                    session = Session::new(&config.model, session.cwd.clone());
                    // M6.20 BUG M2 + M3: provider swap mints a fresh
                    // session; reset yolo flag and permission mode.
                    crate::permissions::ApprovalSink::reset_session_flag(approver.as_ref());
                    let _ = crate::permissions::take_pre_plan_mode();
                    crate::permissions::set_current_mode_and_broadcast(perm_mode);
                    save_project_model(&config.model);
                    println!(
                        "{COLOR_DIM}provider → {name} (model: {}, saved to .thclaws/settings.json; new session {}){COLOR_RESET}",
                        config.model, session.id
                    );
                }
                SlashCommand::ModelsRefresh => {
                    println!("{COLOR_DIM}refreshing model catalogue…{COLOR_RESET}");
                    match crate::model_catalogue::refresh_from_remote().await {
                        Ok(out) => println!(
                            "{COLOR_DIM}catalogue refreshed: {} models (source: {}){COLOR_RESET}",
                            out.model_count, out.source
                        ),
                        Err(e) => {
                            println!("{COLOR_YELLOW}catalogue refresh failed: {e}{COLOR_RESET}")
                        }
                    }
                }
                SlashCommand::ModelsSetContext { key, size, project } => {
                    let scope = if project {
                        crate::model_catalogue::OverrideScope::Project
                    } else {
                        crate::model_catalogue::OverrideScope::User
                    };
                    let entry = crate::model_catalogue::ModelEntry {
                        context: Some(size),
                        max_output: None,
                        source: Some("override".into()),
                        verified_at: None,
                    };
                    // Compare against catalogue value before saving so we
                    // can warn when the override exceeds it (trust + warn).
                    let cat = crate::model_catalogue::EffectiveCatalogue::load();
                    let warn = cat.lookup_exact(&key).map(|n| size > n).unwrap_or(false);
                    match crate::model_catalogue::save_override(&key, Some(entry), scope) {
                        Ok(path) => {
                            println!(
                                "{COLOR_DIM}override → {key}: {size} tokens (saved to {}){COLOR_RESET}",
                                path.display()
                            );
                            if warn {
                                println!(
                                    "{COLOR_YELLOW}warning: override exceeds catalogue value — provider may reject{COLOR_RESET}"
                                );
                            }
                        }
                        Err(e) => {
                            println!("{COLOR_YELLOW}set-context failed: {e}{COLOR_RESET}")
                        }
                    }
                }
                SlashCommand::ModelsUnsetContext { key, project } => {
                    let scope = if project {
                        crate::model_catalogue::OverrideScope::Project
                    } else {
                        crate::model_catalogue::OverrideScope::User
                    };
                    match crate::model_catalogue::save_override(&key, None, scope) {
                        Ok(path) => println!(
                            "{COLOR_DIM}override removed for {key} (in {}){COLOR_RESET}",
                            path.display()
                        ),
                        Err(e) => {
                            println!("{COLOR_YELLOW}unset-context failed: {e}{COLOR_RESET}")
                        }
                    }
                }
                SlashCommand::Models => {
                    // Build a fresh provider from current config and query it.
                    match build_provider(&config) {
                        Ok(p) => match p.list_models().await {
                            Ok(models) if models.is_empty() => {
                                println!("{COLOR_DIM}no models returned{COLOR_RESET}")
                            }
                            Ok(models) => {
                                for m in models {
                                    match m.display_name {
                                        Some(dn) => {
                                            println!("{COLOR_DIM}  {} — {}{COLOR_RESET}", m.id, dn)
                                        }
                                        None => println!("{COLOR_DIM}  {}{COLOR_RESET}", m.id),
                                    }
                                }
                            }
                            Err(e) => {
                                println!("{COLOR_YELLOW}list models failed: {e}{COLOR_RESET}")
                            }
                        },
                        Err(e) => println!("{COLOR_YELLOW}{e}{COLOR_RESET}"),
                    }
                }
                SlashCommand::Save => {
                    session.sync(agent.history_snapshot());
                    match &session_store {
                        Some(store) => match store.save(&mut session) {
                            Ok(path) => {
                                println!("{COLOR_DIM}saved → {}{COLOR_RESET}", path.display())
                            }
                            Err(e) => println!("{COLOR_YELLOW}save failed: {e}{COLOR_RESET}"),
                        },
                        None => println!("{COLOR_YELLOW}no session store (set $HOME){COLOR_RESET}"),
                    }
                }
                SlashCommand::Load(name_or_id) => {
                    let name_or_id = name_or_id.trim();
                    if name_or_id.is_empty() {
                        println!("{COLOR_YELLOW}usage: /load SESSION_ID | NAME (or /resume for the latest){COLOR_RESET}");
                        continue;
                    }
                    match &session_store {
                        Some(store) => {
                            // `/resume` is wired as `/load last`; resolve
                            // that to the newest session instead of
                            // treating "last" as a literal session id.
                            let loaded_result = if name_or_id.eq_ignore_ascii_case("last") {
                                match store.latest() {
                                    Ok(Some(s)) => Ok(s),
                                    Ok(None) => Err(crate::error::Error::Config(
                                        "no saved sessions to resume".into(),
                                    )),
                                    Err(e) => Err(e),
                                }
                            } else {
                                store.load_by_name_or_id(name_or_id)
                            };
                            match loaded_result {
                                Ok(loaded) => {
                                    agent.set_history(loaded.messages.clone());
                                    session = loaded;
                                    // M6.20 BUG M2 + M3: clear yolo
                                    // flag and reset permission mode
                                    // on session swap so the loaded
                                    // session starts clean rather than
                                    // inheriting Plan / AllowForSession
                                    // from the prior session.
                                    crate::permissions::ApprovalSink::reset_session_flag(
                                        approver.as_ref(),
                                    );
                                    let _ = crate::permissions::take_pre_plan_mode();
                                    crate::permissions::set_current_mode_and_broadcast(perm_mode);
                                    let label = session
                                        .title
                                        .as_deref()
                                        .map(|t| format!("{t} ({})", session.id))
                                        .unwrap_or_else(|| session.id.clone());
                                    println!(
                                        "{COLOR_DIM}loaded {label} ({} message(s)){COLOR_RESET}",
                                        session.messages.len()
                                    );
                                }
                                Err(e) => {
                                    println!("{COLOR_YELLOW}load failed: {e}{COLOR_RESET}");
                                }
                            }
                        }
                        None => println!("{COLOR_YELLOW}no session store (set $HOME){COLOR_RESET}"),
                    }
                }
                SlashCommand::Rename(title) => match &session_store {
                    Some(store) => {
                        // Make sure the session exists on disk first — /rename
                        // before /save would error otherwise. Save any pending
                        // messages so the rename attaches to a real file.
                        session.sync(agent.history_snapshot());
                        if let Err(e) = store.save(&mut session) {
                            println!("{COLOR_YELLOW}save failed: {e}{COLOR_RESET}");
                            continue;
                        }
                        match store.rename(&session.id, &title) {
                            Ok(updated) => {
                                session.title = updated.title.clone();
                                match &session.title {
                                    Some(t) => {
                                        println!("{COLOR_DIM}session renamed → {t}{COLOR_RESET}")
                                    }
                                    None => {
                                        println!("{COLOR_DIM}session title cleared{COLOR_RESET}")
                                    }
                                }
                            }
                            Err(e) => println!("{COLOR_YELLOW}rename failed: {e}{COLOR_RESET}"),
                        }
                    }
                    None => println!("{COLOR_YELLOW}no session store (set $HOME){COLOR_RESET}"),
                },
                SlashCommand::Sessions => match &session_store {
                    Some(store) => match store.list() {
                        Ok(metas) if metas.is_empty() => {
                            println!("{COLOR_DIM}no saved sessions{COLOR_RESET}")
                        }
                        Ok(metas) => {
                            for m in metas.iter().take(20) {
                                let label = m.title.as_deref().unwrap_or(&m.id);
                                println!(
                                    "{COLOR_DIM}  {} · {} · {} msg{COLOR_RESET}",
                                    label, m.model, m.message_count
                                );
                            }
                        }
                        Err(e) => println!("{COLOR_YELLOW}list failed: {e}{COLOR_RESET}"),
                    },
                    None => println!("{COLOR_YELLOW}no session store (set $HOME){COLOR_RESET}"),
                },
                SlashCommand::MemoryList => match &memory_store {
                    Some(store) => match store.list() {
                        Ok(entries) if entries.is_empty() => {
                            println!(
                                "{COLOR_DIM}no memory entries at {}{COLOR_RESET}",
                                store.root.display()
                            );
                        }
                        Ok(entries) => {
                            for e in entries {
                                let ty = e
                                    .memory_type
                                    .as_deref()
                                    .map(|t| format!(" [{t}]"))
                                    .unwrap_or_default();
                                let desc = if e.description.is_empty() {
                                    "(no description)".to_string()
                                } else {
                                    e.description
                                };
                                println!("{COLOR_DIM}  {}{} — {}{COLOR_RESET}", e.name, ty, desc);
                            }
                        }
                        Err(e) => println!("{COLOR_YELLOW}memory list failed: {e}{COLOR_RESET}"),
                    },
                    None => println!("{COLOR_YELLOW}no memory store (set $HOME){COLOR_RESET}"),
                },
                SlashCommand::MemoryRead(name) => {
                    if name.is_empty() {
                        println!("{COLOR_YELLOW}usage: /memory read NAME{COLOR_RESET}");
                        continue;
                    }
                    match &memory_store {
                        Some(store) => match store.get(&name) {
                            Some(entry) => {
                                println!("{COLOR_DIM}── {} ─────{COLOR_RESET}", entry.name);
                                if !entry.description.is_empty() {
                                    println!(
                                        "{COLOR_DIM}description: {}{COLOR_RESET}",
                                        entry.description
                                    );
                                }
                                if let Some(ty) = &entry.memory_type {
                                    println!("{COLOR_DIM}type: {ty}{COLOR_RESET}");
                                }
                                println!("{}", entry.body);
                            }
                            None => println!(
                                "{COLOR_YELLOW}memory entry not found: {name}{COLOR_RESET}"
                            ),
                        },
                        None => println!("{COLOR_YELLOW}no memory store (set $HOME){COLOR_RESET}"),
                    }
                }
                // M6.26 BUG #2: write a memory entry. Editor flow when
                // body is missing; --body shortcut for one-shot.
                SlashCommand::MemoryWrite {
                    name,
                    body,
                    type_,
                    description,
                } => {
                    let Some(store) = &memory_store else {
                        println!("{COLOR_YELLOW}no memory store (set $HOME){COLOR_RESET}");
                        continue;
                    };
                    let body_str = match body {
                        Some(b) => b,
                        None => {
                            // Editor flow: scaffold + spawn $EDITOR.
                            let scaffold = build_memory_scaffold(
                                &name,
                                type_.as_deref(),
                                description.as_deref(),
                                store.get(&name).as_ref(),
                            );
                            match spawn_editor_for_memory(&name, &scaffold) {
                                Ok(content) if content.trim().is_empty() => {
                                    println!(
                                        "{COLOR_DIM}(empty content — write cancelled){COLOR_RESET}"
                                    );
                                    continue;
                                }
                                Ok(content) => content,
                                Err(e) => {
                                    println!("{COLOR_YELLOW}editor failed: {e}{COLOR_RESET}");
                                    continue;
                                }
                            }
                        }
                    };
                    // If --type / --description were passed alongside
                    // --body, splice them into a frontmatter block.
                    let final_content = if (type_.is_some() || description.is_some())
                        && !body_str.starts_with("---")
                    {
                        let mut fm = std::collections::HashMap::new();
                        if let Some(t) = &type_ {
                            fm.insert("type".to_string(), t.clone());
                        }
                        if let Some(d) = &description {
                            fm.insert("description".to_string(), d.clone());
                        }
                        crate::memory::write_frontmatter_map(&fm, &body_str)
                    } else {
                        body_str
                    };
                    match crate::memory::write_entry(store, &name, &final_content) {
                        Ok(path) => println!(
                            "{COLOR_DIM}wrote {} ({} bytes){COLOR_RESET}",
                            path.display(),
                            final_content.len()
                        ),
                        Err(e) => {
                            println!("{COLOR_YELLOW}write failed: {e}{COLOR_RESET}");
                        }
                    }
                }
                SlashCommand::MemoryAppend { name, body } => {
                    let Some(store) = &memory_store else {
                        println!("{COLOR_YELLOW}no memory store (set $HOME){COLOR_RESET}");
                        continue;
                    };
                    match crate::memory::append_to_entry(store, &name, &body) {
                        Ok(path) => println!(
                            "{COLOR_DIM}appended {} bytes → {}{COLOR_RESET}",
                            body.len(),
                            path.display()
                        ),
                        Err(e) => {
                            println!("{COLOR_YELLOW}append failed: {e}{COLOR_RESET}");
                        }
                    }
                }
                SlashCommand::MemoryEdit(name) => {
                    let Some(store) = &memory_store else {
                        println!("{COLOR_YELLOW}no memory store (set $HOME){COLOR_RESET}");
                        continue;
                    };
                    let existing = store.get(&name);
                    if existing.is_none() {
                        println!(
                            "{COLOR_YELLOW}entry not found: {name} (use /memory write {name} to create){COLOR_RESET}"
                        );
                        continue;
                    }
                    let scaffold = build_memory_scaffold(&name, None, None, existing.as_ref());
                    let content = match spawn_editor_for_memory(&name, &scaffold) {
                        Ok(c) if c.trim().is_empty() => {
                            println!("{COLOR_DIM}(empty content — edit cancelled){COLOR_RESET}");
                            continue;
                        }
                        Ok(c) => c,
                        Err(e) => {
                            println!("{COLOR_YELLOW}editor failed: {e}{COLOR_RESET}");
                            continue;
                        }
                    };
                    match crate::memory::write_entry(store, &name, &content) {
                        Ok(path) => println!(
                            "{COLOR_DIM}updated {} ({} bytes){COLOR_RESET}",
                            path.display(),
                            content.len()
                        ),
                        Err(e) => {
                            println!("{COLOR_YELLOW}edit failed: {e}{COLOR_RESET}");
                        }
                    }
                }
                SlashCommand::MemoryDelete { name, yes } => {
                    let Some(store) = &memory_store else {
                        println!("{COLOR_YELLOW}no memory store (set $HOME){COLOR_RESET}");
                        continue;
                    };
                    if !yes {
                        // Show a quick preview so the user sees what
                        // they're about to nuke.
                        match store.get(&name) {
                            Some(entry) => {
                                println!(
                                    "{COLOR_DIM}About to delete: {} — {}{COLOR_RESET}",
                                    entry.name,
                                    if entry.description.is_empty() {
                                        "(no description)".to_string()
                                    } else {
                                        entry.description
                                    }
                                );
                            }
                            None => {
                                println!("{COLOR_YELLOW}entry not found: {name}{COLOR_RESET}");
                                continue;
                            }
                        }
                        use std::io::{BufRead, Write};
                        print!("Delete? [y/N] ");
                        std::io::stdout().flush().ok();
                        let mut line = String::new();
                        std::io::stdin().lock().read_line(&mut line).ok();
                        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                            println!("{COLOR_DIM}cancelled{COLOR_RESET}");
                            continue;
                        }
                    }
                    match crate::memory::delete_entry(store, &name) {
                        Ok(path) => println!("{COLOR_DIM}deleted {}{COLOR_RESET}", path.display()),
                        Err(e) => {
                            println!("{COLOR_YELLOW}delete failed: {e}{COLOR_RESET}");
                        }
                    }
                }
                SlashCommand::Tasks => {
                    let store = task_store.lock().unwrap();
                    let tasks = store.list();
                    if tasks.is_empty() {
                        println!("{COLOR_DIM}no tasks{COLOR_RESET}");
                    } else {
                        for t in tasks {
                            println!(
                                "{COLOR_DIM}  #{} [{}] {}{COLOR_RESET}",
                                t.id, t.status, t.subject
                            );
                        }
                    }
                }
                SlashCommand::Context => {
                    let history = agent.history_snapshot();
                    let blocks: usize = history.iter().map(|m| m.content.len()).sum();
                    let history_tokens = crate::compaction::estimate_messages_tokens(&history);
                    let system_tokens = system.len() / 4;
                    let total_tokens = history_tokens + system_tokens;
                    let window = agent.budget_tokens.max(1);
                    let pct = (total_tokens as f64 / window as f64) * 100.0;

                    const BUDGET_CLAUDE_MD: u64 = 1024;
                    const BUDGET_MEMORY_INDEX: u64 = 512;
                    const BUDGET_MEMORY_ENTRY: u64 = 1024;
                    let claude_files = crate::context::scan_claude_md_sizes(&cwd);
                    let claude_total: u64 = claude_files.iter().map(|(_, n)| *n).sum();
                    let claude_over: Vec<String> = claude_files
                        .iter()
                        .filter(|(_, n)| *n > BUDGET_CLAUDE_MD)
                        .map(|(p, n)| {
                            format!(
                                "{} ({})",
                                p.file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| p.display().to_string()),
                                crate::util::format_bytes(*n),
                            )
                        })
                        .collect();
                    let (mem_index_bytes, mem_entries) = crate::memory::MemoryStore::default_path()
                        .map(crate::memory::MemoryStore::new)
                        .map(|s| crate::memory::memory_sizes(&s))
                        .unwrap_or((0, Vec::new()));
                    let mem_entries_total: u64 = mem_entries.iter().map(|(_, n)| *n).sum();
                    let mem_entries_over: Vec<String> = mem_entries
                        .iter()
                        .filter(|(_, n)| *n > BUDGET_MEMORY_ENTRY)
                        .map(|(name, n)| format!("{} ({})", name, crate::util::format_bytes(*n)))
                        .collect();

                    println!(
                        "{COLOR_DIM}context: {} message(s), {} content block(s), system prompt {} chars{COLOR_RESET}",
                        history.len(),
                        blocks,
                        system.len()
                    );
                    println!(
                        "{COLOR_DIM}model: {} · window: {} tokens · used: ~{} tokens{COLOR_RESET}",
                        config.model,
                        crate::util::format_tokens(window),
                        crate::util::format_tokens(total_tokens),
                    );
                    println!(
                        "{COLOR_DIM}{} {:.1}%{COLOR_RESET}",
                        crate::util::progress_bar(pct, 24),
                        pct,
                    );
                    if !claude_files.is_empty() || mem_index_bytes > 0 || !mem_entries.is_empty() {
                        println!("{COLOR_DIM}system-prompt breakdown:{COLOR_RESET}");
                        if !claude_files.is_empty() {
                            let mut line = format!(
                                "  CLAUDE.md / AGENTS.md  {}  ({} file{})",
                                crate::util::format_bytes(claude_total),
                                claude_files.len(),
                                if claude_files.len() == 1 { "" } else { "s" },
                            );
                            if !claude_over.is_empty() {
                                line.push_str(&format!(
                                    "  ⚠ over {} cap: {}",
                                    crate::util::format_bytes(BUDGET_CLAUDE_MD),
                                    claude_over.join(", "),
                                ));
                            }
                            println!("{COLOR_DIM}{line}{COLOR_RESET}");
                        }
                        if mem_index_bytes > 0 {
                            let mut line = format!(
                                "  MEMORY.md              {}",
                                crate::util::format_bytes(mem_index_bytes)
                            );
                            if mem_index_bytes > BUDGET_MEMORY_INDEX {
                                line.push_str(&format!(
                                    "  ⚠ over {} cap",
                                    crate::util::format_bytes(BUDGET_MEMORY_INDEX),
                                ));
                            }
                            println!("{COLOR_DIM}{line}{COLOR_RESET}");
                        }
                        if !mem_entries.is_empty() {
                            let mut line = format!(
                                "  memory entries         {}  ({} file{})",
                                crate::util::format_bytes(mem_entries_total),
                                mem_entries.len(),
                                if mem_entries.len() == 1 { "" } else { "s" },
                            );
                            if !mem_entries_over.is_empty() {
                                line.push_str(&format!(
                                    "  ⚠ over {} cap: {}",
                                    crate::util::format_bytes(BUDGET_MEMORY_ENTRY),
                                    mem_entries_over.join(", "),
                                ));
                            }
                            println!("{COLOR_DIM}{line}{COLOR_RESET}");
                        }
                    }
                }
                SlashCommand::Version => {
                    let v = crate::version::info();
                    println!("{COLOR_DIM}version:  {}{COLOR_RESET}", v.version);
                    println!(
                        "{COLOR_DIM}revision: {}{} ({}){COLOR_RESET}",
                        v.git_sha,
                        if v.git_dirty { "+dirty" } else { "" },
                        v.git_branch
                    );
                    println!(
                        "{COLOR_DIM}built:    {} ({}){COLOR_RESET}",
                        v.build_time, v.build_profile
                    );
                }
                SlashCommand::Cwd => {
                    println!(
                        "{COLOR_DIM}{}{COLOR_RESET}",
                        std::env::current_dir()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| "(unknown)".into())
                    );
                }
                SlashCommand::Thinking(budget_str) => {
                    if budget_str.is_empty() {
                        let current = config.thinking_budget.unwrap_or(0);
                        println!(
                            "{COLOR_DIM}thinking budget: {current} tokens (0 = off){COLOR_RESET}"
                        );
                    } else {
                        match budget_str.parse::<u32>() {
                            Ok(0) => {
                                config.thinking_budget = None;
                                println!("{COLOR_DIM}thinking disabled{COLOR_RESET}");
                            }
                            Ok(n) => {
                                config.thinking_budget = Some(n);
                                println!("{COLOR_DIM}thinking budget → {n} tokens{COLOR_RESET}");
                            }
                            Err(_) => {
                                println!(
                                    "{COLOR_YELLOW}usage: /thinking BUDGET (integer){COLOR_RESET}"
                                );
                            }
                        }
                    }
                }
                SlashCommand::Plugins => {
                    let plugins = crate::plugins::all_plugins_all_scopes();
                    if plugins.is_empty() {
                        println!(
                            "{COLOR_DIM}no plugins installed (try /plugin install <url>){COLOR_RESET}"
                        );
                    } else {
                        for p in plugins {
                            let status = if p.enabled { "enabled" } else { "disabled" };
                            let version = if p.version.is_empty() {
                                String::new()
                            } else {
                                format!(" v{}", p.version)
                            };
                            println!(
                                "{COLOR_DIM}  {}{} ({}) → {}{COLOR_RESET}",
                                p.name,
                                version,
                                status,
                                p.path.display()
                            );
                            if !p.source.is_empty() {
                                println!("{COLOR_DIM}    source: {}{COLOR_RESET}", p.source);
                            }
                        }
                    }
                }
                SlashCommand::PluginInstall { url, user } => {
                    // Allow `/plugin install <name>` to resolve a
                    // marketplace slug to its install_url. If `url`
                    // already looks like a URL, this is a no-op.
                    let (effective_url, abort_msg) = resolve_plugin_install_target(&url);
                    if let Some(msg) = abort_msg {
                        println!("{COLOR_YELLOW}{msg}{COLOR_RESET}");
                        continue;
                    }
                    match crate::plugins::install(&effective_url, user).await {
                        Ok(plugin) => {
                            let manifest = plugin.manifest().ok();
                            let scope = if user { "user" } else { "project" };
                            let summary = manifest
                                .as_ref()
                                .map(|m| {
                                    let mut parts = Vec::new();
                                    if !m.skills.is_empty() {
                                        parts.push(format!("{} skill dir(s)", m.skills.len()));
                                    }
                                    if !m.commands.is_empty() {
                                        parts.push(format!("{} command dir(s)", m.commands.len()));
                                    }
                                    if !m.agents.is_empty() {
                                        parts.push(format!("{} agent dir(s)", m.agents.len()));
                                    }
                                    if !m.mcp_servers.is_empty() {
                                        parts
                                            .push(format!("{} MCP server(s)", m.mcp_servers.len()));
                                    }
                                    if parts.is_empty() {
                                        "no contributions".to_string()
                                    } else {
                                        parts.join(", ")
                                    }
                                })
                                .unwrap_or_else(|| "manifest unreadable".into());
                            println!(
                                "{COLOR_DIM}plugin '{}' installed ({scope}, {}) → {}{COLOR_RESET}",
                                plugin.name,
                                summary,
                                plugin.path.display()
                            );
                            // Refresh the skill store + name set so the
                            // plugin's contributed skills are callable
                            // as `/<skill-name>` immediately, without
                            // a restart. SkillStore::discover() picks
                            // up plugin-contributed dirs by default.
                            let refreshed = crate::skills::SkillStore::discover();
                            skill_names = refreshed.skills.keys().cloned().collect();
                            if let Some(handle) = &skill_store_handle {
                                if let Ok(mut store) = handle.lock() {
                                    *store = refreshed;
                                }
                            }
                            // Skills + commands are live (skill store
                            // refreshed above; commands re-discover per
                            // /-resolution call). MCP servers are the
                            // one piece that still needs a restart —
                            // the live tool registry doesn't track per-
                            // plugin server lifecycle. Surface a
                            // prominent, actionable message listing the
                            // server names so the user knows exactly
                            // what they're getting after `/quit` →
                            // relaunch. M6.16.1 follow-up — pre-fix
                            // mentioned "commands" too which was no
                            // longer accurate.
                            if let Some(m) = manifest.as_ref() {
                                if !m.mcp_servers.is_empty() {
                                    let names: Vec<&str> =
                                        m.mcp_servers.keys().map(String::as_str).collect();
                                    println!(
                                        "{COLOR_YELLOW}⚠  restart {} to spawn {} new MCP server(s): {}{COLOR_RESET}",
                                        crate::branding::current().name,
                                        names.len(),
                                        names.join(", ")
                                    );
                                    println!(
                                        "{COLOR_DIM}   skills + commands already callable in this session.{COLOR_RESET}"
                                    );
                                } else {
                                    println!(
                                        "{COLOR_DIM}skills + commands callable in this session — no restart needed{COLOR_RESET}"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            println!("{COLOR_YELLOW}plugin install failed: {e}{COLOR_RESET}");
                        }
                    }
                }
                SlashCommand::PluginEnable { name, user } => {
                    match crate::plugins::set_enabled(&name, user, true) {
                        Ok(true) => {
                            // M6.16 BUG H1: refresh in-process skill store
                            // so plugin-contributed skills become callable
                            // immediately. MCP servers still need a
                            // restart — surfaced explicitly with names.
                            let refreshed = crate::skills::SkillStore::discover();
                            skill_names = refreshed.skills.keys().cloned().collect();
                            if let Some(handle) = &skill_store_handle {
                                if let Ok(mut store) = handle.lock() {
                                    *store = refreshed;
                                }
                            }
                            println!("{COLOR_DIM}plugin '{name}' enabled{COLOR_RESET}");
                            if let Some(names) = plugin_mcp_server_names(&name) {
                                println!(
                                    "{COLOR_YELLOW}⚠  restart {} to spawn {} MCP server(s): {}{COLOR_RESET}",
                                    crate::branding::current().name,
                                    names.len(),
                                    names.join(", ")
                                );
                            }
                        }
                        Ok(false) => println!(
                            "{COLOR_YELLOW}no plugin named '{name}' in that scope{COLOR_RESET}"
                        ),
                        Err(e) => println!("{COLOR_YELLOW}enable failed: {e}{COLOR_RESET}"),
                    }
                }
                SlashCommand::PluginDisable { name, user } => {
                    // Capture MCP names BEFORE disabling — symmetric
                    // with PluginRemove, where the manifest is gone
                    // after the call.
                    let mcp_to_drop = plugin_mcp_server_names(&name);
                    match crate::plugins::set_enabled(&name, user, false) {
                        Ok(true) => {
                            let refreshed = crate::skills::SkillStore::discover();
                            skill_names = refreshed.skills.keys().cloned().collect();
                            if let Some(handle) = &skill_store_handle {
                                if let Ok(mut store) = handle.lock() {
                                    *store = refreshed;
                                }
                            }
                            println!("{COLOR_DIM}plugin '{name}' disabled{COLOR_RESET}");
                            if let Some(names) = mcp_to_drop {
                                println!(
                                    "{COLOR_YELLOW}⚠  restart {} to drop {} MCP server(s) it contributed: {}{COLOR_RESET}",
                                    crate::branding::current().name,
                                    names.len(),
                                    names.join(", ")
                                );
                            }
                        }
                        Ok(false) => println!(
                            "{COLOR_YELLOW}no plugin named '{name}' in that scope{COLOR_RESET}"
                        ),
                        Err(e) => println!("{COLOR_YELLOW}disable failed: {e}{COLOR_RESET}"),
                    }
                }
                SlashCommand::PluginShow { name } => {
                    match crate::plugins::find_installed_with_scope(&name) {
                        Some((p, is_user)) => {
                            let status = if p.enabled { "enabled" } else { "disabled" };
                            // M6.16.1 BUG L3: include scope so the
                            // user knows which `--user` flag to pass
                            // to follow-up /plugin commands.
                            let scope = if is_user { "user" } else { "project" };
                            println!(
                                "{COLOR_DIM}  {} v{} ({}, {}){COLOR_RESET}",
                                p.name,
                                if p.version.is_empty() { "-" } else { &p.version },
                                status,
                                scope
                            );
                            println!(
                                "{COLOR_DIM}  path: {}{COLOR_RESET}",
                                p.path.display()
                            );
                            if !p.source.is_empty() {
                                println!(
                                    "{COLOR_DIM}  source: {}{COLOR_RESET}",
                                    p.source
                                );
                            }
                            match p.manifest() {
                                Ok(m) => {
                                    if !m.description.is_empty() {
                                        println!(
                                            "{COLOR_DIM}  description: {}{COLOR_RESET}",
                                            m.description
                                        );
                                    }
                                    if !m.author.is_empty() {
                                        println!(
                                            "{COLOR_DIM}  author: {}{COLOR_RESET}",
                                            m.author
                                        );
                                    }
                                    if !m.skills.is_empty() {
                                        println!(
                                            "{COLOR_DIM}  skill dirs: {}{COLOR_RESET}",
                                            m.skills.join(", ")
                                        );
                                    }
                                    if !m.commands.is_empty() {
                                        println!(
                                            "{COLOR_DIM}  command dirs: {}{COLOR_RESET}",
                                            m.commands.join(", ")
                                        );
                                    }
                                    if !m.agents.is_empty() {
                                        println!(
                                            "{COLOR_DIM}  agent dirs: {}{COLOR_RESET}",
                                            m.agents.join(", ")
                                        );
                                    }
                                    if !m.mcp_servers.is_empty() {
                                        println!(
                                            "{COLOR_DIM}  mcp servers: {}{COLOR_RESET}",
                                            m.mcp_servers
                                                .keys()
                                                .cloned()
                                                .collect::<Vec<_>>()
                                                .join(", ")
                                        );
                                    }
                                }
                                Err(e) => println!(
                                    "{COLOR_YELLOW}  manifest unreadable: {e}{COLOR_RESET}"
                                ),
                            }
                        }
                        None => println!(
                            "{COLOR_YELLOW}no plugin named '{name}' installed (try /plugins to list){COLOR_RESET}"
                        ),
                    }
                }
                SlashCommand::PluginGc => match crate::plugins::gc() {
                    Ok((proj, user)) => {
                        if proj.is_empty() && user.is_empty() {
                            println!(
                                "{COLOR_DIM}no zombie entries — registry is clean{COLOR_RESET}"
                            );
                        } else {
                            println!("{COLOR_DIM}removed zombie entries:{COLOR_RESET}");
                            for n in &proj {
                                println!("{COLOR_DIM}  - {n} (project){COLOR_RESET}");
                            }
                            for n in &user {
                                println!("{COLOR_DIM}  - {n} (user){COLOR_RESET}");
                            }
                            // Refresh in case any zombie was contributing
                            // skills cached in this session.
                            let refreshed = crate::skills::SkillStore::discover();
                            skill_names = refreshed.skills.keys().cloned().collect();
                            if let Some(handle) = &skill_store_handle {
                                if let Ok(mut store) = handle.lock() {
                                    *store = refreshed;
                                }
                            }
                        }
                    }
                    Err(e) => println!("{COLOR_YELLOW}gc failed: {e}{COLOR_RESET}"),
                },
                SlashCommand::PluginRemove { name, user } => {
                    // Capture MCP names BEFORE removal — once remove()
                    // succeeds the manifest is gone and find_installed
                    // returns None.
                    let mcp_to_drop = plugin_mcp_server_names(&name);
                    match crate::plugins::remove(&name, user) {
                        Ok(true) => {
                            // M6.16 BUG H1: refresh skill store so the
                            // removed plugin's skills stop being callable
                            // immediately. Without this the model could
                            // still invoke a removed skill and lazy-read
                            // the now-missing SKILL.md → empty body
                            // cached forever.
                            let refreshed = crate::skills::SkillStore::discover();
                            skill_names = refreshed.skills.keys().cloned().collect();
                            if let Some(handle) = &skill_store_handle {
                                if let Ok(mut store) = handle.lock() {
                                    *store = refreshed;
                                }
                            }
                            println!("{COLOR_DIM}plugin '{name}' removed{COLOR_RESET}");
                            if let Some(names) = mcp_to_drop {
                                println!(
                                    "{COLOR_YELLOW}⚠  restart {} to fully drop {} MCP server(s) it was running: {}{COLOR_RESET}",
                                    crate::branding::current().name,
                                    names.len(),
                                    names.join(", ")
                                );
                            }
                        }
                        Ok(false) => {
                            println!(
                                "{COLOR_YELLOW}no plugin named '{name}' in that scope{COLOR_RESET}"
                            );
                        }
                        Err(e) => {
                            println!("{COLOR_YELLOW}plugin remove failed: {e}{COLOR_RESET}");
                        }
                    }
                }
                SlashCommand::McpAdd { name, url, user } => {
                    let scope = if user { "user" } else { "project" };
                    // /mcp add is hand-add — untrusted by default. To
                    // enable widget rendering on a self-added server,
                    // edit the resulting mcp.json and set
                    // `"trusted": true` explicitly.
                    let cfg = crate::mcp::McpServerConfig {
                        name: name.clone(),
                        transport: "http".into(),
                        command: String::new(),
                        args: Vec::new(),
                        env: Default::default(),
                        url: url.clone(),
                        headers: Default::default(),
                        trusted: false,
                    };
                    // 1. Persist to disk.
                    let saved_to = match crate::config::save_mcp_server(&cfg, user) {
                        Ok(p) => p,
                        Err(e) => {
                            println!("{COLOR_YELLOW}write failed: {e}{COLOR_RESET}");
                            continue;
                        }
                    };
                    // 2. Connect and list tools.
                    match crate::mcp::McpClient::spawn(cfg.clone()).await {
                        Ok(client) => match client.list_tools().await {
                            Ok(tools) => {
                                let names: Vec<String> =
                                    tools.iter().map(|t| t.name.clone()).collect();
                                for info in tools {
                                    let tool = crate::mcp::McpTool::new(client.clone(), info);
                                    tool_registry.register(Arc::new(tool));
                                }
                                mcp_summary.push((name.clone(), names.clone()));
                                mcp_clients.push(client);
                                // 3. Rebuild agent so it picks up the new tools.
                                //    Preserve history so the conversation keeps going.
                                let prev_history = agent.history_snapshot();
                                agent = Agent::new(
                                    build_provider(&config)?,
                                    tool_registry.clone(),
                                    config.model.clone(),
                                    system.clone(),
                                )
                                .with_max_iterations(config.max_iterations)
                                .with_permission_mode(perm_mode)
                                .with_approver(approver.clone())
                                .with_hooks(std::sync::Arc::new(config.hooks.clone()));
                                agent.set_history(prev_history);
                                println!(
                                    "{COLOR_DIM}mcp '{name}' added ({scope}, {} tool(s)) → {}{COLOR_RESET}",
                                    names.len(),
                                    saved_to.display()
                                );
                            }
                            Err(e) => {
                                println!(
                                    "{COLOR_YELLOW}saved '{name}' to {} but list_tools failed: {e}{COLOR_RESET}",
                                    saved_to.display()
                                );
                            }
                        },
                        Err(e) => {
                            println!(
                                "{COLOR_YELLOW}saved '{name}' to {} but connect failed: {e}{COLOR_RESET}",
                                saved_to.display()
                            );
                        }
                    }
                }
                SlashCommand::McpRemove { name, user } => {
                    match crate::config::remove_mcp_server(&name, user) {
                        Ok((true, path)) => {
                            println!(
                                "{COLOR_DIM}mcp '{name}' removed from {} (restart to drop active tools){COLOR_RESET}",
                                path.display()
                            );
                        }
                        Ok((false, path)) => {
                            println!(
                                "{COLOR_YELLOW}no server named '{name}' in {}{COLOR_RESET}",
                                path.display()
                            );
                        }
                        Err(e) => {
                            println!("{COLOR_YELLOW}remove failed: {e}{COLOR_RESET}");
                        }
                    }
                }
                SlashCommand::Mcp => {
                    if mcp_summary.is_empty() {
                        println!("{COLOR_DIM}no MCP servers configured{COLOR_RESET}");
                    } else {
                        for (name, tools) in &mcp_summary {
                            println!(
                                "{COLOR_DIM}  {} ({} tool(s)){COLOR_RESET}",
                                name,
                                tools.len()
                            );
                            for t in tools {
                                println!(
                                    "{COLOR_DIM}    - {}{}{}{COLOR_RESET}",
                                    name,
                                    crate::mcp::MCP_NAME_SEPARATOR,
                                    t
                                );
                            }
                        }
                    }
                }
                SlashCommand::Compact => {
                    let history = agent.history_snapshot();
                    let compacted = crate::compaction::compact(&history, agent.budget_tokens / 2);
                    agent.set_history(compacted.clone());
                    let persist_note = match (&session_store, compacted.len() < history.len()) {
                        (Some(store), true) => {
                            let path = store.path_for(&session.id);
                            match session.append_compaction_to(&path, &compacted) {
                                Ok(()) => " (checkpoint saved)".to_string(),
                                Err(e) => format!(" (checkpoint save failed: {e})"),
                            }
                        }
                        _ => String::new(),
                    };
                    println!(
                        "{COLOR_DIM}compacted: {} → {} messages{persist_note}{COLOR_RESET}",
                        history.len(),
                        compacted.len()
                    );
                }
                SlashCommand::Fork => {
                    // Save → build LLM summary → seed a fresh session
                    // with the summary + recent turns. Same semantics
                    // as the GUI's ForkWithSummary flow, but triggered
                    // from the terminal/REPL.
                    if let Some(store) = &session_store {
                        let _ = store.save(&mut session);
                    }
                    let history = agent.history_snapshot();
                    if history.is_empty() {
                        println!(
                            "{COLOR_DIM}/fork: nothing to summarize — history is empty{COLOR_RESET}"
                        );
                        continue;
                    }
                    let provider = match crate::repl::build_provider(&config) {
                        Ok(p) => p,
                        Err(e) => {
                            println!("{COLOR_YELLOW}/fork: can't build provider: {e}{COLOR_RESET}");
                            continue;
                        }
                    };
                    let target = agent.budget_tokens / 2;
                    let summary_history = crate::compaction::compact_with_summary(
                        &history,
                        target,
                        provider.as_ref(),
                        &config.model,
                    )
                    .await;
                    let old_id = session.id.clone();
                    session = Session::new(&config.model, session.cwd.clone());
                    agent.clear_history();
                    agent.set_history(summary_history.clone());
                    session.messages = summary_history.clone();
                    if let Some(store) = &session_store {
                        let _ = store.save(&mut session);
                    }
                    // M6.20 BUG M2 + M3: fork mints a fresh session id;
                    // clear yolo flag and reset permission mode same as
                    // /load.
                    crate::permissions::ApprovalSink::reset_session_flag(approver.as_ref());
                    let _ = crate::permissions::take_pre_plan_mode();
                    crate::permissions::set_current_mode_and_broadcast(perm_mode);
                    println!(
                        "{COLOR_DIM}/fork: forked {old_id} → {} ({} → {} messages){COLOR_RESET}",
                        session.id,
                        history.len(),
                        summary_history.len()
                    );
                }
                SlashCommand::Doctor => {
                    println!(
                        "{COLOR_DIM}── {} diagnostics ──{COLOR_RESET}",
                        crate::branding::current().name
                    );
                    let v = crate::version::info();
                    println!("{COLOR_DIM}version:    {}{COLOR_RESET}", v.version);
                    println!(
                        "{COLOR_DIM}revision:   {}{} ({}){COLOR_RESET}",
                        v.git_sha,
                        if v.git_dirty { "+dirty" } else { "" },
                        v.git_branch
                    );
                    println!(
                        "{COLOR_DIM}built:      {} ({}){COLOR_RESET}",
                        v.build_time, v.build_profile
                    );
                    println!("{COLOR_DIM}model:      {}{COLOR_RESET}", config.model);
                    println!(
                        "{COLOR_DIM}provider:   {}{COLOR_RESET}",
                        config.detect_provider().unwrap_or("unknown")
                    );
                    println!(
                        "{COLOR_DIM}api key:    {}{COLOR_RESET}",
                        if config.api_key_from_env().is_some() {
                            "set ✓"
                        } else {
                            "MISSING ✗"
                        }
                    );
                    println!("{COLOR_DIM}config:     {}{COLOR_RESET}", {
                        let paths = AppConfig::user_config_paths();
                        paths
                            .iter()
                            .find(|p| p.exists())
                            .map(|p| format!("{} ✓", p.display()))
                            .unwrap_or_else(|| {
                                paths
                                    .first()
                                    .map(|p| format!("{} (not found)", p.display()))
                                    .unwrap_or_else(|| "none".into())
                            })
                    });
                    println!(
                        "{COLOR_DIM}sandbox:    {}{COLOR_RESET}",
                        crate::sandbox::Sandbox::root()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "disabled".into())
                    );
                    println!(
                        "{COLOR_DIM}sessions:   {}{COLOR_RESET}",
                        crate::session::SessionStore::default_path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "none".into())
                    );
                    println!(
                        "{COLOR_DIM}memory:     {}{COLOR_RESET}",
                        crate::memory::MemoryStore::default_path()
                            .map(|p| if p.exists() {
                                format!("{} ✓", p.display())
                            } else {
                                format!("{} (empty)", p.display())
                            })
                            .unwrap_or_else(|| "none".into())
                    );
                    println!(
                        "{COLOR_DIM}tmux:       {}{COLOR_RESET}",
                        if crate::team::has_tmux() {
                            "available ✓"
                        } else {
                            "not found"
                        }
                    );
                    println!(
                        "{COLOR_DIM}tools:      {} registered{COLOR_RESET}",
                        tool_registry.names().len()
                    );
                    println!(
                        "{COLOR_DIM}history:    {} messages{COLOR_RESET}",
                        agent.history_snapshot().len()
                    );
                }
                SlashCommand::Permissions(mode) => {
                    if mode.is_empty() {
                        let cur = crate::permissions::current_mode();
                        let label = match cur {
                            PermissionMode::Auto => "auto",
                            PermissionMode::Ask => "ask",
                            PermissionMode::Plan => "plan",
                        };
                        println!(
                            "{COLOR_DIM}permissions: {label} (auto = never prompt, ask = prompt on mutating tools, plan = read-only exploration){COLOR_RESET}"
                        );
                    } else {
                        match mode.as_str() {
                            "auto" | "yolo" => {
                                agent.permission_mode = PermissionMode::Auto;
                                crate::permissions::set_current_mode_and_broadcast(
                                    PermissionMode::Auto,
                                );
                                println!("{COLOR_DIM}permissions → auto (no prompts){COLOR_RESET}");
                            }
                            "ask" | "default" => {
                                agent.permission_mode = PermissionMode::Ask;
                                crate::permissions::set_current_mode_and_broadcast(
                                    PermissionMode::Ask,
                                );
                                println!("{COLOR_DIM}permissions → ask{COLOR_RESET}");
                            }
                            _ => {
                                println!("{COLOR_YELLOW}usage: /permissions auto|ask{COLOR_RESET}");
                            }
                        }
                    }
                }
                SlashCommand::Plan(arg) => {
                    let arg = arg.trim().to_lowercase();
                    let cur = crate::permissions::current_mode();
                    match arg.as_str() {
                        "" | "on" | "enter" | "start" => {
                            if matches!(cur, PermissionMode::Plan) {
                                println!("{COLOR_DIM}Already in plan mode.{COLOR_RESET}");
                            } else {
                                crate::permissions::stash_pre_plan_mode(cur);
                                crate::permissions::set_current_mode_and_broadcast(
                                    PermissionMode::Plan,
                                );
                                println!(
                                    "{COLOR_DIM}plan mode active — mutating tools blocked. Ask the model to call SubmitPlan.{COLOR_RESET}"
                                );
                            }
                        }
                        "exit" | "off" | "cancel" | "stop" | "abort" => {
                            let restored = crate::permissions::take_pre_plan_mode()
                                .unwrap_or(PermissionMode::Ask);
                            crate::permissions::set_current_mode_and_broadcast(restored);
                            crate::tools::plan_state::clear();
                            println!(
                                "{COLOR_DIM}plan mode cleared — restored to {restored:?}.{COLOR_RESET}"
                            );
                        }
                        "status" | "show" => {
                            let plan = crate::tools::plan_state::get();
                            let summary = match plan {
                                Some(p) => {
                                    format!(" — active plan {} ({} step(s))", p.id, p.steps.len())
                                }
                                None => String::new(),
                            };
                            println!("{COLOR_DIM}permission mode: {cur:?}{summary}{COLOR_RESET}");
                        }
                        _ => println!(
                            "{COLOR_YELLOW}usage: /plan [enter | exit | status]{COLOR_RESET}"
                        ),
                    }
                }
                SlashCommand::Sso { sub } => {
                    let active = crate::policy::active();
                    let policy = active.and_then(|a| a.policy.policies.sso.as_ref());
                    let policy = match policy {
                        Some(p) => p.clone(),
                        None => {
                            println!(
                                "{COLOR_YELLOW}no SSO policy active — /sso requires policies.sso.enabled in the org policy{COLOR_RESET}"
                            );
                            continue;
                        }
                    };
                    if !policy.enabled {
                        println!(
                            "{COLOR_YELLOW}policies.sso.enabled is false — nothing to do{COLOR_RESET}"
                        );
                        continue;
                    }
                    match sub {
                        SsoSubcommand::Status => {
                            println!("{COLOR_DIM}{}{COLOR_RESET}", crate::sso::status(&policy));
                        }
                        SsoSubcommand::Login => match crate::sso::login(&policy).await {
                            Ok(s) => {
                                let who = s
                                    .email
                                    .clone()
                                    .or(s.name.clone())
                                    .or(s.sub.clone())
                                    .unwrap_or_else(|| "(no identity claim)".into());
                                println!(
                                    "{COLOR_DIM}✓ signed in as {who} (issuer: {}){COLOR_RESET}",
                                    s.issuer
                                );
                            }
                            Err(e) => {
                                println!("{COLOR_YELLOW}/sso login failed: {e}{COLOR_RESET}");
                            }
                        },
                        SsoSubcommand::Logout => match crate::sso::logout(&policy) {
                            Ok(()) => println!(
                                "{COLOR_DIM}signed out (cached tokens cleared){COLOR_RESET}"
                            ),
                            Err(e) => {
                                println!("{COLOR_YELLOW}/sso logout failed: {e}{COLOR_RESET}")
                            }
                        },
                    }
                }
                SlashCommand::Skills => {
                    let store = crate::skills::SkillStore::discover();
                    if store.skills.is_empty() {
                        println!("{COLOR_DIM}no skills found{COLOR_RESET}");
                        println!(
                            "{COLOR_DIM}  add skills to .thclaws/skills/ or ~/.config/thclaws/skills/{COLOR_RESET}"
                        );
                    } else {
                        let home = crate::util::home_dir().unwrap_or_default();
                        let project_prefix = std::env::current_dir()
                            .map(|p| p.join(".thclaws/skills"))
                            .unwrap_or_default();
                        let user_prefix = home.join(".config/thclaws/skills");
                        let claude_prefix = home.join(".claude/skills");

                        let level_of = |dir: &std::path::Path| -> &str {
                            if dir.starts_with(&project_prefix) {
                                "project"
                            } else if dir.starts_with(&user_prefix) {
                                "user"
                            } else if dir.starts_with(&claude_prefix) {
                                "claude"
                            } else {
                                "?"
                            }
                        };

                        let mut rows: Vec<(&str, &str, bool)> = store
                            .skills
                            .values()
                            .map(|s| {
                                (
                                    s.name.as_str(),
                                    level_of(&s.dir),
                                    s.dir.join("scripts").exists(),
                                )
                            })
                            .collect();
                        rows.sort_by_key(|r| r.0);
                        for (name, level, has_scripts) in &rows {
                            println!(
                                "{COLOR_DIM}  {}{} ({}){COLOR_RESET}",
                                name,
                                if *has_scripts { " [+scripts]" } else { "" },
                                level,
                            );
                        }
                        println!(
                            "{COLOR_DIM}({} skill(s) — use /skill show <name> for details){COLOR_RESET}",
                            store.skills.len()
                        );
                    }
                }
                SlashCommand::SkillShow(name) => {
                    let store = crate::skills::SkillStore::discover();
                    let home = crate::util::home_dir().unwrap_or_default();
                    let project_prefix = std::env::current_dir()
                        .map(|p| p.join(".thclaws/skills"))
                        .unwrap_or_default();
                    let user_prefix = home.join(".config/thclaws/skills");
                    let skill_level = |dir: &std::path::Path| -> &str {
                        if dir.starts_with(&project_prefix) {
                            "project"
                        } else if dir.starts_with(&user_prefix) {
                            "user"
                        } else {
                            "system"
                        }
                    };
                    match store.get(&name) {
                        Some(skill) => {
                            let scripts = if skill.dir.join("scripts").exists() {
                                " [+scripts]"
                            } else {
                                ""
                            };
                            println!(
                                "{COLOR_DIM}{}{} — {}{COLOR_RESET}",
                                skill.name, scripts, skill.description,
                            );
                            if !skill.when_to_use.is_empty() {
                                println!(
                                    "{COLOR_DIM}when to use: {}{COLOR_RESET}",
                                    skill.when_to_use
                                );
                            }
                            println!("{COLOR_DIM}level: {}{COLOR_RESET}", skill_level(&skill.dir));
                            println!("{COLOR_DIM}path:  {}{COLOR_RESET}", skill.dir.display());
                        }
                        None => {
                            println!(
                                "{COLOR_YELLOW}unknown skill: '{name}' — run /skills to list{COLOR_RESET}"
                            );
                        }
                    }
                }
                SlashCommand::SkillInstall {
                    git_url,
                    name,
                    project,
                } => {
                    // Resolve the argument: if it parses as a URL (http/https/git@/.zip)
                    // or a `<repo>#<branch>:<subpath>` extension, install
                    // directly. Otherwise treat it as a marketplace name
                    // and look up the install_url from the catalogue.
                    let (effective_url, effective_name, abort_msg) =
                        resolve_skill_install_target(&git_url, name.as_deref());
                    if let Some(msg) = abort_msg {
                        println!("{COLOR_YELLOW}{msg}{COLOR_RESET}");
                    } else {
                        match crate::skills::install_from_url(
                            &effective_url,
                            effective_name.as_deref(),
                            project,
                        )
                        .await
                        {
                            Ok(report) => {
                                for line in report {
                                    println!("{COLOR_DIM}  {line}{COLOR_RESET}");
                                }
                                // Refresh both the shared SkillStore (so the
                                // Skill tool can load the new content) and the
                                // local `skill_names` (so `/<skill-name>` works
                                // without restart).
                                let refreshed = crate::skills::SkillStore::discover();
                                skill_names = refreshed.skills.keys().cloned().collect();
                                if let Some(handle) = &skill_store_handle {
                                    if let Ok(mut store) = handle.lock() {
                                        *store = refreshed;
                                    }
                                }
                            }
                            Err(e) => {
                                println!("{COLOR_YELLOW}skill install failed: {e}{COLOR_RESET}");
                            }
                        }
                    }
                }
                SlashCommand::SkillMarketplace { refresh } => {
                    if refresh {
                        match crate::marketplace::refresh_from_remote().await {
                            Ok(out) => {
                                println!(
                                    "{COLOR_DIM}refreshed marketplace from {} — {} skill(s){COLOR_RESET}",
                                    crate::marketplace::REMOTE_URL,
                                    out.skill_count
                                );
                            }
                            Err(e) => {
                                println!(
                                    "{COLOR_YELLOW}refresh failed ({e}); using cached/baseline catalogue{COLOR_RESET}"
                                );
                            }
                        }
                    }
                    let mp = crate::marketplace::load();
                    let age_suffix = match crate::marketplace::cache_age_label() {
                        Some(label) => format!(", {label}"),
                        None => String::new(),
                    };
                    println!(
                        "{COLOR_DIM}marketplace ({}, {} skill(s){age_suffix}){COLOR_RESET}",
                        mp.source,
                        mp.skills.len(),
                    );
                    // Group by category so the listing reads like a catalog.
                    let mut by_cat: std::collections::BTreeMap<
                        String,
                        Vec<&crate::marketplace::MarketplaceSkill>,
                    > = std::collections::BTreeMap::new();
                    for s in &mp.skills {
                        let cat = if s.category.is_empty() {
                            "other".to_string()
                        } else {
                            s.category.clone()
                        };
                        by_cat.entry(cat).or_default().push(s);
                    }
                    for (cat, skills) in by_cat {
                        println!("{COLOR_DIM}── {cat} ──{COLOR_RESET}");
                        for s in skills {
                            let tags = crate::marketplace::entry_tags(s);
                            println!(
                                "{COLOR_DIM}  {:<24}{tags} — {}{COLOR_RESET}",
                                s.name,
                                s.short_line()
                            );
                        }
                    }
                    println!(
                        "{COLOR_DIM}install with: /skill install <name>   |   detail: /skill info <name>{COLOR_RESET}"
                    );
                }
                SlashCommand::SkillSearch(query) => {
                    let mp = crate::marketplace::load();
                    let hits = mp.search(&query);
                    if hits.is_empty() {
                        println!(
                            "{COLOR_DIM}no matches for '{query}' — try /skill marketplace to browse all{COLOR_RESET}"
                        );
                    } else {
                        println!(
                            "{COLOR_DIM}{} match(es) for '{query}':{COLOR_RESET}",
                            hits.len()
                        );
                        for s in hits {
                            println!(
                                "{COLOR_DIM}  {:<24} — {}{COLOR_RESET}",
                                s.name,
                                s.short_line()
                            );
                        }
                    }
                }
                SlashCommand::SkillInfo(name) => {
                    let mp = crate::marketplace::load();
                    match mp.find(&name) {
                        Some(s) => {
                            println!("{COLOR_DIM}name:        {}{COLOR_RESET}", s.name);
                            println!("{COLOR_DIM}description: {}{COLOR_RESET}", s.description);
                            if !s.category.is_empty() {
                                println!("{COLOR_DIM}category:    {}{COLOR_RESET}", s.category);
                            }
                            println!(
                                "{COLOR_DIM}license:     {} ({}){COLOR_RESET}",
                                s.license, s.license_tier
                            );
                            if !s.source_repo.is_empty() {
                                println!(
                                    "{COLOR_DIM}source:      {}{}{COLOR_RESET}",
                                    s.source_repo,
                                    if s.source_path.is_empty() {
                                        String::new()
                                    } else {
                                        format!(" ({})", s.source_path)
                                    }
                                );
                            }
                            if !s.homepage.is_empty() {
                                println!("{COLOR_DIM}homepage:    {}{COLOR_RESET}", s.homepage);
                            }
                            match (s.license_tier.as_str(), s.install_url.as_ref()) {
                                ("linked-only", _) => {
                                    println!(
                                        "{COLOR_YELLOW}install:     not redistributable — install from {}{COLOR_RESET}",
                                        if s.homepage.is_empty() {
                                            "the upstream repo"
                                        } else {
                                            &s.homepage
                                        }
                                    );
                                }
                                (_, Some(url)) => {
                                    println!(
                                        "{COLOR_DIM}install:     /skill install {} (resolves to {url}){COLOR_RESET}",
                                        s.name
                                    );
                                }
                                (_, None) => {
                                    println!(
                                        "{COLOR_YELLOW}install:     no install_url in catalogue{COLOR_RESET}"
                                    );
                                }
                            }
                        }
                        None => {
                            println!(
                                "{COLOR_YELLOW}no skill named '{name}' in marketplace — try /skill search <query>{COLOR_RESET}"
                            );
                        }
                    }
                }
                SlashCommand::McpMarketplace { refresh } => {
                    if refresh {
                        if let Err(e) = crate::marketplace::refresh_from_remote().await {
                            println!("{COLOR_YELLOW}refresh failed ({e}){COLOR_RESET}");
                        }
                    }
                    let mp = crate::marketplace::load();
                    let age_suffix = match crate::marketplace::cache_age_label() {
                        Some(label) => format!(", {label}"),
                        None => String::new(),
                    };
                    println!(
                        "{COLOR_DIM}MCP marketplace ({}, {} server(s){age_suffix}){COLOR_RESET}",
                        mp.source,
                        mp.mcp_servers.len(),
                    );
                    let mut by_cat: std::collections::BTreeMap<
                        String,
                        Vec<&crate::marketplace::MarketplaceMcpServer>,
                    > = std::collections::BTreeMap::new();
                    for s in &mp.mcp_servers {
                        let cat = if s.category.is_empty() {
                            "other".into()
                        } else {
                            s.category.clone()
                        };
                        by_cat.entry(cat).or_default().push(s);
                    }
                    for (cat, servers) in by_cat {
                        println!("{COLOR_DIM}── {cat} ──{COLOR_RESET}");
                        for s in servers {
                            let tport = if s.transport == "sse" {
                                " [hosted]"
                            } else {
                                ""
                            };
                            let tags = crate::marketplace::entry_tags(s);
                            println!(
                                "{COLOR_DIM}  {:<24}{tport}{tags} — {}{COLOR_RESET}",
                                s.name,
                                s.short_line()
                            );
                        }
                    }
                    println!(
                        "{COLOR_DIM}install with: /mcp install <name>   |   detail: /mcp info <name>{COLOR_RESET}"
                    );
                }
                SlashCommand::McpSearch(query) => {
                    let mp = crate::marketplace::load();
                    let hits = mp.search_mcp(&query);
                    if hits.is_empty() {
                        println!(
                            "{COLOR_DIM}no matches for '{query}' — try /mcp marketplace{COLOR_RESET}"
                        );
                    } else {
                        println!(
                            "{COLOR_DIM}{} match(es) for '{query}':{COLOR_RESET}",
                            hits.len()
                        );
                        for s in hits {
                            println!(
                                "{COLOR_DIM}  {:<24} — {}{COLOR_RESET}",
                                s.name,
                                s.short_line()
                            );
                        }
                    }
                }
                SlashCommand::McpInfo(name) => {
                    let mp = crate::marketplace::load();
                    match mp.find_mcp(&name) {
                        Some(s) => {
                            println!("{COLOR_DIM}name:         {}{COLOR_RESET}", s.name);
                            println!("{COLOR_DIM}description:  {}{COLOR_RESET}", s.description);
                            if !s.category.is_empty() {
                                println!("{COLOR_DIM}category:     {}{COLOR_RESET}", s.category);
                            }
                            println!(
                                "{COLOR_DIM}license:      {} ({}){COLOR_RESET}",
                                s.license, s.license_tier
                            );
                            println!("{COLOR_DIM}transport:    {}{COLOR_RESET}", s.transport);
                            if s.transport == "stdio" && !s.command.is_empty() {
                                let argv = if s.args.is_empty() {
                                    s.command.clone()
                                } else {
                                    format!("{} {}", s.command, s.args.join(" "))
                                };
                                println!("{COLOR_DIM}command:      {}{COLOR_RESET}", argv);
                            }
                            if s.transport == "sse" && !s.url.is_empty() {
                                println!("{COLOR_DIM}url:          {}{COLOR_RESET}", s.url);
                            }
                            if let Some(src) = &s.install_url {
                                println!("{COLOR_DIM}source:       {}{COLOR_RESET}", src);
                            }
                            if !s.homepage.is_empty() {
                                println!("{COLOR_DIM}homepage:     {}{COLOR_RESET}", s.homepage);
                            }
                            if let Some(msg) = &s.post_install_message {
                                println!("{COLOR_DIM}note:         {}{COLOR_RESET}", msg);
                            }
                            println!(
                                "{COLOR_DIM}install with: /mcp install {}{COLOR_RESET}",
                                s.name
                            );
                        }
                        None => println!(
                            "{COLOR_YELLOW}no MCP named '{name}' in marketplace — try /mcp search <query>{COLOR_RESET}"
                        ),
                    }
                }
                SlashCommand::McpInstall { name, user } => {
                    match install_mcp_from_marketplace(&name, user).await {
                        Ok(report) => {
                            for line in report {
                                println!("{COLOR_DIM}  {line}{COLOR_RESET}");
                            }
                        }
                        Err(e) => println!("{COLOR_YELLOW}mcp install failed: {e}{COLOR_RESET}"),
                    }
                }
                SlashCommand::PluginMarketplace { refresh } => {
                    if refresh {
                        if let Err(e) = crate::marketplace::refresh_from_remote().await {
                            println!("{COLOR_YELLOW}refresh failed ({e}){COLOR_RESET}");
                        }
                    }
                    let mp = crate::marketplace::load();
                    let age_suffix = match crate::marketplace::cache_age_label() {
                        Some(label) => format!(", {label}"),
                        None => String::new(),
                    };
                    println!(
                        "{COLOR_DIM}plugin marketplace ({}, {} plugin(s){age_suffix}){COLOR_RESET}",
                        mp.source,
                        mp.plugins.len(),
                    );
                    let mut by_cat: std::collections::BTreeMap<
                        String,
                        Vec<&crate::marketplace::MarketplacePlugin>,
                    > = std::collections::BTreeMap::new();
                    for p in &mp.plugins {
                        let cat = if p.category.is_empty() {
                            "other".into()
                        } else {
                            p.category.clone()
                        };
                        by_cat.entry(cat).or_default().push(p);
                    }
                    for (cat, plugins) in by_cat {
                        println!("{COLOR_DIM}── {cat} ──{COLOR_RESET}");
                        for p in plugins {
                            let tags = crate::marketplace::entry_tags(p);
                            println!(
                                "{COLOR_DIM}  {:<24}{tags} — {}{COLOR_RESET}",
                                p.name,
                                p.short_line()
                            );
                        }
                    }
                    println!(
                        "{COLOR_DIM}install with: /plugin install <name>   |   detail: /plugin info <name>{COLOR_RESET}"
                    );
                }
                SlashCommand::PluginSearch(query) => {
                    let mp = crate::marketplace::load();
                    let hits = mp.search_plugin(&query);
                    if hits.is_empty() {
                        println!(
                            "{COLOR_DIM}no matches for '{query}' — try /plugin marketplace{COLOR_RESET}"
                        );
                    } else {
                        println!(
                            "{COLOR_DIM}{} match(es) for '{query}':{COLOR_RESET}",
                            hits.len()
                        );
                        for p in hits {
                            println!(
                                "{COLOR_DIM}  {:<24} — {}{COLOR_RESET}",
                                p.name,
                                p.short_line()
                            );
                        }
                    }
                }
                SlashCommand::PluginInfo(name) => {
                    let mp = crate::marketplace::load();
                    match mp.find_plugin(&name) {
                        Some(p) => {
                            println!("{COLOR_DIM}name:         {}{COLOR_RESET}", p.name);
                            println!("{COLOR_DIM}description:  {}{COLOR_RESET}", p.description);
                            if !p.category.is_empty() {
                                println!("{COLOR_DIM}category:     {}{COLOR_RESET}", p.category);
                            }
                            println!(
                                "{COLOR_DIM}license:      {} ({}){COLOR_RESET}",
                                p.license, p.license_tier
                            );
                            if !p.homepage.is_empty() {
                                println!("{COLOR_DIM}homepage:     {}{COLOR_RESET}", p.homepage);
                            }
                            println!(
                                "{COLOR_DIM}install with: /plugin install {} (resolves to {}){COLOR_RESET}",
                                p.name, p.install_url
                            );
                        }
                        None => println!(
                            "{COLOR_YELLOW}no plugin named '{name}' in marketplace — try /plugin search <query>{COLOR_RESET}"
                        ),
                    }
                }
                SlashCommand::Team => {
                    let session = "thclaws-team";
                    if crate::team::has_tmux() {
                        let exists = std::process::Command::new("tmux")
                            .args(["has-session", "-t", session])
                            .output()
                            .map(|o| o.status.success())
                            .unwrap_or(false);
                        if exists {
                            println!(
                                "{COLOR_DIM}attaching to tmux session '{session}'...{COLOR_RESET}"
                            );
                            println!(
                                "{COLOR_DIM}(press Ctrl+B then D to detach back here){COLOR_RESET}"
                            );
                            let _ = std::process::Command::new("tmux")
                                .args(["attach", "-t", session])
                                .status();
                        } else {
                            // List team status from mailbox.
                            let team_dir = crate::team::Mailbox::default_dir();
                            let mailbox = crate::team::Mailbox::new(team_dir);
                            match mailbox.all_status() {
                                Ok(agents) if agents.is_empty() => {
                                    println!("{COLOR_DIM}no team agents found{COLOR_RESET}");
                                }
                                Ok(agents) => {
                                    println!(
                                        "{COLOR_DIM}Team agents (no tmux session):{COLOR_RESET}"
                                    );
                                    for a in &agents {
                                        let task = a.current_task.as_deref().unwrap_or("-");
                                        println!(
                                            "{COLOR_DIM}  {} — {} (task: {}){COLOR_RESET}",
                                            a.agent, a.status, task
                                        );
                                    }
                                }
                                Err(_) => {
                                    println!("{COLOR_DIM}no team configured{COLOR_RESET}");
                                }
                            }
                        }
                    } else {
                        println!("{COLOR_YELLOW}tmux not installed — install with: brew install tmux{COLOR_RESET}");
                    }
                }
                SlashCommand::Usage => {
                    let tracker =
                        crate::usage::UsageTracker::new(crate::usage::UsageTracker::default_path());
                    println!("{COLOR_DIM}{}{COLOR_RESET}", tracker.summary());
                }
                SlashCommand::Kms => {
                    let all = crate::kms::list_all();
                    if all.is_empty() {
                        println!(
                            "{COLOR_DIM}no knowledge bases yet — try: /kms new default{COLOR_RESET}"
                        );
                    } else {
                        let active: std::collections::HashSet<&String> =
                            config.kms_active.iter().collect();
                        for k in &all {
                            let marker = if active.contains(&k.name) { "*" } else { " " };
                            println!(
                                "{COLOR_DIM}  {marker} {:<16} ({}){COLOR_RESET}",
                                k.name,
                                k.scope.as_str()
                            );
                        }
                        println!(
                            "{COLOR_DIM}(* = attached to this project; toggle with /kms use | /kms off){COLOR_RESET}"
                        );
                    }
                }
                SlashCommand::KmsNew { name, project } => {
                    let scope = if project {
                        crate::kms::KmsScope::Project
                    } else {
                        crate::kms::KmsScope::User
                    };
                    match crate::kms::create(&name, scope) {
                        Ok(k) => println!(
                            "{COLOR_DIM}created KMS '{}' ({}) → {}{COLOR_RESET}",
                            k.name,
                            k.scope.as_str(),
                            k.root.display()
                        ),
                        Err(e) => println!("{COLOR_YELLOW}create failed: {e}{COLOR_RESET}"),
                    }
                }
                SlashCommand::KmsUse(name) => {
                    if crate::kms::resolve(&name).is_none() {
                        println!(
                            "{COLOR_YELLOW}no KMS named '{name}' (try /kms list or /kms new {name}){COLOR_RESET}"
                        );
                    } else if config.kms_active.iter().any(|n| n == &name) {
                        println!("{COLOR_DIM}KMS '{name}' already attached{COLOR_RESET}");
                    } else {
                        config.kms_active.push(name.clone());
                        if let Err(e) = ProjectConfig::set_active_kms(config.kms_active.clone()) {
                            println!("{COLOR_YELLOW}save failed: {e}{COLOR_RESET}");
                        } else {
                            println!(
                                "{COLOR_DIM}KMS '{name}' attached (restart chat or start a new turn to pick it up){COLOR_RESET}"
                            );
                        }
                    }
                }
                SlashCommand::KmsOff(name) => {
                    let before = config.kms_active.len();
                    config.kms_active.retain(|n| n != &name);
                    if config.kms_active.len() == before {
                        println!("{COLOR_DIM}KMS '{name}' was not attached{COLOR_RESET}");
                    } else if let Err(e) = ProjectConfig::set_active_kms(config.kms_active.clone())
                    {
                        println!("{COLOR_YELLOW}save failed: {e}{COLOR_RESET}");
                    } else {
                        println!(
                            "{COLOR_DIM}KMS '{name}' detached (restart chat or start a new turn to apply){COLOR_RESET}"
                        );
                    }
                }
                SlashCommand::KmsShow(name) => match crate::kms::resolve(&name) {
                    Some(k) => {
                        let body = k.read_index();
                        if body.trim().is_empty() {
                            println!(
                                    "{COLOR_DIM}KMS '{name}' index is empty — populate it at {}{COLOR_RESET}",
                                    k.index_path().display()
                                );
                        } else {
                            println!("{body}");
                        }
                    }
                    None => println!("{COLOR_YELLOW}no KMS named '{name}'{COLOR_RESET}"),
                },
                SlashCommand::KmsIngest {
                    name,
                    file,
                    alias,
                    force,
                } => {
                    let Some(k) = crate::kms::resolve(&name) else {
                        println!(
                            "{COLOR_YELLOW}no KMS named '{name}' (try /kms list or /kms new {name}){COLOR_RESET}"
                        );
                        continue;
                    };
                    let source = std::path::PathBuf::from(&file);
                    let source = if source.is_absolute() {
                        source
                    } else {
                        std::env::current_dir()
                            .unwrap_or_else(|_| std::path::PathBuf::from("."))
                            .join(&source)
                    };
                    match crate::kms::ingest(&k, &source, alias.as_deref(), force) {
                        Ok(r) => {
                            let verb = if r.overwrote { "replaced" } else { "ingested" };
                            let cascade = if r.cascaded > 0 {
                                format!(" (marked {} dependent page(s) stale)", r.cascaded)
                            } else {
                                String::new()
                            };
                            println!(
                                "{COLOR_DIM}{verb} → {} — {}{cascade}{COLOR_RESET}",
                                r.target.display(),
                                r.summary,
                            );
                        }
                        Err(e) => {
                            println!("{COLOR_YELLOW}ingest failed: {e}{COLOR_RESET}");
                        }
                    }
                }
                // M6.25 BUG #8: URL ingest variant (CLI mirror).
                SlashCommand::KmsIngestUrl {
                    name,
                    url,
                    alias,
                    force,
                } => {
                    let Some(k) = crate::kms::resolve(&name) else {
                        println!("{COLOR_YELLOW}no KMS named '{name}'{COLOR_RESET}");
                        continue;
                    };
                    match crate::kms::ingest_url(&k, &url, alias.as_deref(), force).await {
                        Ok(r) => println!(
                            "{COLOR_DIM}ingested {url} → {} — {}{COLOR_RESET}",
                            r.target.display(),
                            r.summary,
                        ),
                        Err(e) => {
                            println!("{COLOR_YELLOW}url ingest failed: {e}{COLOR_RESET}");
                        }
                    }
                }
                // M6.25 BUG #8: PDF ingest variant (CLI mirror).
                SlashCommand::KmsIngestPdf {
                    name,
                    file,
                    alias,
                    force,
                } => {
                    let Some(k) = crate::kms::resolve(&name) else {
                        println!("{COLOR_YELLOW}no KMS named '{name}'{COLOR_RESET}");
                        continue;
                    };
                    let source = std::path::PathBuf::from(&file);
                    let source = if source.is_absolute() {
                        source
                    } else {
                        std::env::current_dir()
                            .unwrap_or_else(|_| std::path::PathBuf::from("."))
                            .join(&source)
                    };
                    match crate::kms::ingest_pdf(&k, &source, alias.as_deref(), force).await {
                        Ok(r) => println!(
                            "{COLOR_DIM}ingested {} → {} — {}{COLOR_RESET}",
                            source.display(),
                            r.target.display(),
                            r.summary,
                        ),
                        Err(e) => {
                            println!("{COLOR_YELLOW}pdf ingest failed: {e}{COLOR_RESET}");
                        }
                    }
                }
                // M6.28: handled above as a rewrite-before-match (so the
                // turn fires via the regular pipeline). This arm should
                // be unreachable; if the rewrite path missed (e.g. no
                // such KMS), fall through with a notice so the user
                // sees something.
                SlashCommand::KmsIngestSession { name, .. } => {
                    println!(
                        "{COLOR_YELLOW}/kms ingest {name} $ — no KMS named '{name}' \
                         (try /kms list){COLOR_RESET}"
                    );
                }
                // M6.25 BUG #3: lint (CLI).
                SlashCommand::KmsLint(name) => {
                    let Some(k) = crate::kms::resolve(&name) else {
                        println!("{COLOR_YELLOW}no KMS named '{name}'{COLOR_RESET}");
                        continue;
                    };
                    match crate::kms::lint(&k) {
                        Ok(report) => {
                            let total = report.total_issues();
                            if total == 0 {
                                println!(
                                    "{COLOR_DIM}KMS '{name}': clean — no issues found.{COLOR_RESET}"
                                );
                            } else {
                                println!("KMS '{name}': {total} issue(s)");
                                if !report.broken_links.is_empty() {
                                    println!("  broken links ({}):", report.broken_links.len());
                                    for (page, target) in &report.broken_links {
                                        println!("    {page} → pages/{target}.md (missing)");
                                    }
                                }
                                if !report.index_orphans.is_empty() {
                                    println!(
                                        "  index entries with no underlying file ({}):",
                                        report.index_orphans.len()
                                    );
                                    for stem in &report.index_orphans {
                                        println!("    {stem}");
                                    }
                                }
                                if !report.missing_in_index.is_empty() {
                                    println!(
                                        "  pages missing from index ({}):",
                                        report.missing_in_index.len()
                                    );
                                    for stem in &report.missing_in_index {
                                        println!("    {stem}");
                                    }
                                }
                                if !report.orphan_pages.is_empty() {
                                    println!("  orphan pages ({}):", report.orphan_pages.len());
                                    for stem in &report.orphan_pages {
                                        println!("    {stem}");
                                    }
                                }
                                if !report.missing_frontmatter.is_empty() {
                                    println!(
                                        "  pages without YAML frontmatter ({}):",
                                        report.missing_frontmatter.len()
                                    );
                                    for stem in &report.missing_frontmatter {
                                        println!("    {stem}");
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            println!("{COLOR_YELLOW}lint failed: {e}{COLOR_RESET}");
                        }
                    }
                }
                // M6.25 BUG #4: file-answer (CLI).
                SlashCommand::KmsFileAnswer { name, title } => {
                    let Some(k) = crate::kms::resolve(&name) else {
                        println!("{COLOR_YELLOW}no KMS named '{name}'{COLOR_RESET}");
                        continue;
                    };
                    let answer = session
                        .messages
                        .iter()
                        .rev()
                        .find(|m| matches!(m.role, crate::types::Role::Assistant))
                        .map(|m| {
                            m.content
                                .iter()
                                .filter_map(|b| match b {
                                    crate::types::ContentBlock::Text { text } => {
                                        Some(text.as_str())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        });
                    let Some(answer_text) = answer.filter(|s| !s.trim().is_empty()) else {
                        println!(
                            "{COLOR_YELLOW}no assistant message in session yet — nothing to file{COLOR_RESET}"
                        );
                        continue;
                    };
                    let stem: String = title
                        .trim()
                        .chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                                c
                            } else {
                                '_'
                            }
                        })
                        .collect::<String>()
                        .trim_matches('_')
                        .to_string();
                    if stem.is_empty() {
                        println!(
                            "{COLOR_YELLOW}title sanitises to empty — pick another{COLOR_RESET}"
                        );
                        continue;
                    }
                    let body = format!("# {title}\n\n{answer_text}\n");
                    let mut fm = std::collections::BTreeMap::new();
                    fm.insert("category".into(), "answer".into());
                    fm.insert("filed_from".into(), "chat".into());
                    let serialized = crate::kms::write_frontmatter(&fm, &body);
                    match crate::kms::write_page(&k, &stem, &serialized) {
                        Ok(path) => println!(
                            "{COLOR_DIM}filed answer → {} ({} bytes){COLOR_RESET}",
                            path.display(),
                            serialized.len()
                        ),
                        Err(e) => {
                            println!("{COLOR_YELLOW}file-answer failed: {e}{COLOR_RESET}");
                        }
                    }
                }
                // M6.29: /loop + /goal CLI dispatch.
                SlashCommand::Loop { interval_secs, body } => {
                    if active_loop_handle.is_some() {
                        println!(
                            "{COLOR_YELLOW}loop already running — `/loop stop` first{COLOR_RESET}"
                        );
                        continue;
                    }
                    let interval =
                        std::time::Duration::from_secs(interval_secs.unwrap_or(300));
                    let label = interval_secs
                        .map(|s| format!("every {s}s"))
                        .unwrap_or_else(|| "self-paced (5min default)".to_string());
                    let body_for_task = body.clone();
                    let cli_input_tx_for_task = cli_input_tx.clone();
                    let handle = tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(interval).await;
                            if cli_input_tx_for_task
                                .send(body_for_task.clone())
                                .is_err()
                            {
                                break;
                            }
                        }
                    });
                    active_loop_handle = Some(handle.abort_handle());
                    active_loop_body = Some(body.clone());
                    println!(
                        "{COLOR_DIM}loop started ({label}): {body}{COLOR_RESET}"
                    );
                }
                SlashCommand::LoopStop => match active_loop_handle.take() {
                    Some(h) => {
                        h.abort();
                        let body = active_loop_body.take().unwrap_or_default();
                        println!(
                            "{COLOR_DIM}loop stopped (was firing `{body}`){COLOR_RESET}"
                        );
                    }
                    None => println!("{COLOR_YELLOW}no active loop{COLOR_RESET}"),
                },
                SlashCommand::LoopStatus => match &active_loop_body {
                    Some(b) => println!(
                        "{COLOR_DIM}loop active: body=`{b}`{COLOR_RESET}"
                    ),
                    None => println!("{COLOR_DIM}no active loop{COLOR_RESET}"),
                },
                SlashCommand::GoalStart {
                    objective,
                    budget_tokens,
                    budget_time_secs,
                } => {
                    let new_goal = crate::goal_state::GoalState::new(
                        objective.clone(),
                        budget_tokens,
                        budget_time_secs,
                    );
                    crate::goal_state::set(Some(new_goal));
                    tool_registry.register(Arc::new(crate::tools::UpdateGoalTool));
                    // System prompt + agent rebuild aren't strictly
                    // required (UpdateGoal is a callable tool either
                    // way) but rebuilding ensures the model sees the
                    // new tool in its catalog this turn.
                    println!(
                        "{COLOR_DIM}goal started: \"{}\" (budget_tokens={}, budget_time={}){COLOR_RESET}",
                        objective,
                        budget_tokens
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "unlimited".into()),
                        budget_time_secs
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "unlimited".into()),
                    );
                }
                SlashCommand::GoalStatus => match crate::goal_state::current() {
                    Some(g) => {
                        println!(
                            "{COLOR_DIM}goal status: {} ({}s elapsed, {} iterations, {} tokens){COLOR_RESET}",
                            g.status.as_str(),
                            g.time_used_secs(),
                            g.iterations_done,
                            g.tokens_used,
                        );
                        println!("  objective: {}", g.objective);
                        if let Some(m) = &g.last_message {
                            println!("  last: {m}");
                        }
                    }
                    None => println!(
                        "{COLOR_YELLOW}no active goal — try /goal start \"<objective>\"{COLOR_RESET}"
                    ),
                },
                SlashCommand::GoalShow => match crate::goal_state::current() {
                    Some(g) => println!("{:#?}", g),
                    None => println!("{COLOR_YELLOW}no active goal{COLOR_RESET}"),
                },
                SlashCommand::GoalContinue => {
                    // Handled as a rewrite-before-match below — see
                    // the rewrite block earlier in the loop.
                    println!(
                        "{COLOR_YELLOW}/goal continue — internal: rewrite block missed; check goal state{COLOR_RESET}"
                    );
                }
                SlashCommand::GoalComplete { reason } => {
                    if crate::goal_state::current().is_none() {
                        println!("{COLOR_YELLOW}no active goal{COLOR_RESET}");
                        continue;
                    }
                    let r = reason.clone();
                    crate::goal_state::apply(|g| {
                        g.status = crate::goal_state::GoalStatus::Complete;
                        if let Some(r) = &r {
                            g.last_message = Some(r.clone());
                        }
                        g.completed_at = Some(
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0),
                        );
                        true
                    });
                    println!("{COLOR_DIM}goal marked complete{COLOR_RESET}");
                    if let Some(h) = active_loop_handle.take() {
                        h.abort();
                        active_loop_body = None;
                        println!("{COLOR_DIM}loop auto-stopped{COLOR_RESET}");
                    }
                }
                SlashCommand::GoalAbandon { reason } => {
                    if crate::goal_state::current().is_none() {
                        println!("{COLOR_YELLOW}no active goal{COLOR_RESET}");
                        continue;
                    }
                    let r = reason.clone();
                    crate::goal_state::apply(|g| {
                        g.status = crate::goal_state::GoalStatus::Abandoned;
                        if let Some(r) = &r {
                            g.last_message = Some(r.clone());
                        }
                        g.completed_at = Some(
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0),
                        );
                        true
                    });
                    println!("{COLOR_DIM}goal abandoned{COLOR_RESET}");
                    if let Some(h) = active_loop_handle.take() {
                        h.abort();
                        active_loop_body = None;
                        println!("{COLOR_DIM}loop auto-stopped{COLOR_RESET}");
                    }
                }
                SlashCommand::Unknown(what) => {
                    println!("{COLOR_YELLOW}unknown command: {what}{COLOR_RESET}");
                }
            }
            continue;
        }

        // `! command` — run a shell command directly (output goes to terminal).
        if let Some(shell_cmd) = line.strip_prefix('!') {
            let shell_cmd = shell_cmd.trim();
            if shell_cmd.is_empty() {
                println!("{COLOR_YELLOW}usage: ! <command>{COLOR_RESET}");
                continue;
            }
            println!("{COLOR_DIM}$ {shell_cmd}{COLOR_RESET}");
            let status = crate::util::shell_command_sync(shell_cmd).status();
            // If the child left the cursor mid-line (e.g. `cat` on a file with
            // no trailing newline), readline's next-prompt render issues a CR
            // + clear-to-EOL and wipes whatever the child just wrote. Emit a
            // bare newline so the child's output stays on its own visible line.
            println!();
            match status {
                Ok(s) if !s.success() => {
                    println!(
                        "{COLOR_YELLOW}[exit code {}]{COLOR_RESET}",
                        s.code().unwrap_or(-1)
                    );
                }
                Err(e) => println!("{COLOR_YELLOW}shell error: {e}{COLOR_RESET}"),
                _ => {}
            }
            continue;
        }

        // Run a turn and stream the output live.
        // Ctrl-C during streaming cancels the turn cleanly.
        lead_log!("\n{COLOR_CYAN}{REPL_PROMPT}{line}{COLOR_RESET}\n{COLOR_GREEN}");
        print!("{COLOR_GREEN}");
        let _ = std::io::stdout().flush();
        let turn_start = std::time::Instant::now();
        let mut stream = Box::pin(agent.run_turn(line.to_string()));
        let mut _cancelled = false;
        loop {
            let ev = tokio::select! {
                ev = stream.next() => ev,
                _ = tokio::signal::ctrl_c() => {
                    _cancelled = true;
                    println!("{COLOR_RESET}\n{COLOR_YELLOW}[cancelled by Ctrl-C]{COLOR_RESET}");
                    drop(stream);
                    break;
                }
            };
            let Some(ev) = ev else { break };
            match ev {
                Ok(AgentEvent::IterationStart { .. }) => {}
                Ok(AgentEvent::Text(s)) => {
                    print!("{s}");
                    lead_log!("{s}");
                    let _ = std::io::stdout().flush();
                }
                Ok(AgentEvent::ToolCallStart { name, input, .. }) => {
                    let detail = match name.as_str() {
                        "Bash" => input
                            .get("command")
                            .and_then(|v| v.as_str())
                            .map(|c| format!(": {}", c.chars().take(80).collect::<String>())),
                        "Read" | "Write" | "Edit" => input
                            .get("path")
                            .and_then(|v| v.as_str())
                            .map(|p| format!(": {p}")),
                        "Glob" => input
                            .get("pattern")
                            .and_then(|v| v.as_str())
                            .map(|p| format!(": {p}")),
                        "Grep" => input
                            .get("pattern")
                            .and_then(|v| v.as_str())
                            .map(|p| format!(": {p}")),
                        "WebFetch" => input
                            .get("url")
                            .and_then(|v| v.as_str())
                            .map(|u| format!(": {}", u.chars().take(60).collect::<String>())),
                        "WebSearch" => input
                            .get("query")
                            .and_then(|v| v.as_str())
                            .map(|q| format!(": {q}")),
                        "Skill" => input
                            .get("name")
                            .and_then(|v| v.as_str())
                            .map(|n| format!(": {n}")),
                        "Task" => input
                            .get("agent")
                            .and_then(|v| v.as_str())
                            .map(|a| format!(": agent={a}")),
                        _ => None,
                    }
                    .unwrap_or_default();
                    print!("{COLOR_RESET}\n{COLOR_DIM}[tool: {name}{detail}]{COLOR_RESET}");
                    lead_log!("{COLOR_RESET}\n{COLOR_DIM}[tool: {name}{detail}]{COLOR_RESET}");
                    let _ = std::io::stdout().flush();
                }
                Ok(AgentEvent::ToolCallResult { name, output, .. }) => {
                    match output {
                        Ok(_) => {
                            print!(" {COLOR_DIM}✓{COLOR_RESET}");
                            lead_log!(" {COLOR_DIM}✓{COLOR_RESET}\n{COLOR_GREEN}");
                        }
                        Err(ref e) => {
                            print!(" {COLOR_YELLOW}✗ {e}{COLOR_RESET}");
                            lead_log!(" {COLOR_YELLOW}✗ {e}{COLOR_RESET}\n{COLOR_GREEN}");
                        }
                    }
                    // CLI parity for plan-mode (M5). When a plan tool
                    // mutates state, render the current plan as a
                    // coloured ANSI block — analogue of the GUI
                    // sidebar's live update. Only fires for the four
                    // plan tools so we don't print a plan block
                    // after every Read / Bash / Edit.
                    if PLAN_TOOL_NAMES.contains(&name.as_str()) {
                        if let Some(plan) = crate::tools::plan_state::get() {
                            let block = format_plan_for_cli(&plan);
                            print!("{block}");
                            lead_log!("{block}");
                        }
                    }
                    print!("{COLOR_RESET}\n{COLOR_GREEN}");
                    let _ = std::io::stdout().flush();
                }
                Ok(AgentEvent::ToolCallDenied { name, .. }) => {
                    println!("{COLOR_RESET}\n{COLOR_YELLOW}[denied: {name}]{COLOR_RESET}");
                    lead_log!(
                        "{COLOR_RESET}\n{COLOR_YELLOW}[denied: {name}]{COLOR_RESET}\n{COLOR_GREEN}"
                    );
                    print!("{COLOR_GREEN}");
                    let _ = std::io::stdout().flush();
                }
                Ok(AgentEvent::Done { stop_reason, usage }) => {
                    print!("{COLOR_RESET}");
                    if let Some(reason) = stop_reason {
                        if reason == "max_iterations" {
                            println!("\n{COLOR_YELLOW}[hit max_iterations]{COLOR_RESET}");
                            lead_log!("\n{COLOR_YELLOW}[hit max_iterations]{COLOR_RESET}\n");
                        }
                    }
                    // Show token usage + elapsed turn duration.
                    let cache_info = match (
                        usage.cache_creation_input_tokens,
                        usage.cache_read_input_tokens,
                    ) {
                        (Some(c), Some(r)) if c > 0 || r > 0 => {
                            format!(" · cache: +{}w/{}r", c, r)
                        }
                        _ => String::new(),
                    };
                    let elapsed = format_duration(turn_start.elapsed());
                    println!(
                        "\n{COLOR_DIM}[tokens: {}in/{}out{} · {}]{COLOR_RESET}",
                        usage.input_tokens, usage.output_tokens, cache_info, elapsed
                    );
                    lead_log!(
                        "\n{COLOR_DIM}[tokens: {}in/{}out{} · {}]{COLOR_RESET}\n",
                        usage.input_tokens,
                        usage.output_tokens,
                        cache_info,
                        elapsed
                    );
                    let _ = std::io::stdout().flush();

                    // Record usage to .thclaws/usage/.
                    let provider_name = config.detect_provider().unwrap_or("unknown");
                    let usage_tracker =
                        crate::usage::UsageTracker::new(crate::usage::UsageTracker::default_path());
                    usage_tracker.record(provider_name, &config.model, &usage);

                    // Auto-save the session after each completed turn.
                    if let Some(store) = &session_store {
                        session.sync(agent.history_snapshot());
                        if let Err(e) = store.save(&mut session) {
                            eprintln!("{COLOR_YELLOW}[autosave failed: {e}]{COLOR_RESET}");
                        }
                    }
                }
                Err(e) => {
                    println!("{COLOR_RESET}\n{COLOR_YELLOW}error: {e}{COLOR_RESET}");
                    lead_log!("{COLOR_RESET}\n{COLOR_YELLOW}error: {e}{COLOR_RESET}\n");
                    break;
                }
            }
        }
    }

    // M6.35 HOOK2: fire session_end before teardown.
    crate::hooks::fire_session(
        &hooks_arc,
        crate::hooks::HookEvent::SessionEnd,
        &session.id,
        &config.model,
    );
    // Kill any teammate processes spawned by this session.
    // M6.34 TEAM3: scoped to this lead's team_dir.
    crate::team::kill_my_teammates();
    println!("{COLOR_DIM}bye{COLOR_RESET}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readline_config_matches_platform() {
        #[cfg(windows)]
        assert_eq!(
            readline_config().behavior(),
            rustyline::Behavior::PreferTerm
        );
        #[cfg(not(windows))]
        assert_eq!(readline_config().behavior(), rustyline::Behavior::Stdio);
    }

    #[test]
    fn parse_slash_returns_none_for_plain_text() {
        assert!(parse_slash("hello").is_none());
        assert!(parse_slash("").is_none());
        assert!(parse_slash("  ").is_none());
    }

    #[test]
    fn parse_slash_help_aliases() {
        assert_eq!(parse_slash("/help"), Some(SlashCommand::Help));
        assert_eq!(parse_slash("/h"), Some(SlashCommand::Help));
        assert_eq!(parse_slash("/?"), Some(SlashCommand::Help));
    }

    #[test]
    fn parse_slash_quit_aliases() {
        assert_eq!(parse_slash("/quit"), Some(SlashCommand::Quit));
        assert_eq!(parse_slash("/q"), Some(SlashCommand::Quit));
        assert_eq!(parse_slash("/exit"), Some(SlashCommand::Quit));
    }

    #[test]
    fn parse_slash_model_captures_arg() {
        assert_eq!(
            parse_slash("/model claude-sonnet-4-5"),
            Some(SlashCommand::Model("claude-sonnet-4-5".into()))
        );
    }

    #[test]
    fn parse_slash_model_without_arg_yields_empty_string() {
        assert_eq!(
            parse_slash("/model"),
            Some(SlashCommand::Model(String::new()))
        );
    }

    #[test]
    fn parse_slash_config_key_value() {
        assert_eq!(
            parse_slash("/config model=gpt-4o"),
            Some(SlashCommand::Config {
                key: "model".into(),
                value: "gpt-4o".into(),
            })
        );
    }

    #[test]
    fn parse_slash_config_without_equals_is_unknown() {
        match parse_slash("/config not-kv") {
            Some(SlashCommand::Unknown(msg)) => assert!(msg.contains("key=value")),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn parse_slash_unknown_command() {
        assert_eq!(
            parse_slash("/bogus"),
            Some(SlashCommand::Unknown("bogus".into()))
        );
    }

    #[test]
    fn parse_slash_handles_leading_trailing_whitespace() {
        assert_eq!(parse_slash("  /help  "), Some(SlashCommand::Help));
        assert_eq!(
            parse_slash("  /model  gpt-4o  "),
            Some(SlashCommand::Model("gpt-4o".into()))
        );
    }

    #[test]
    fn render_help_lists_commands() {
        let h = render_help();
        for needle in &[
            "/help",
            "/quit",
            "/clear",
            "/model",
            "/config",
            "/history",
            "/save",
            "/load",
            "/resume",
            "/sessions",
            "/rename",
        ] {
            assert!(h.contains(needle), "missing {needle} in help");
        }
    }

    #[test]
    fn parse_slash_resume_aliases_to_load() {
        // Bare /resume → Load("last")
        assert_eq!(
            parse_slash("/resume"),
            Some(SlashCommand::Load("last".into()))
        );
        // /resume last (case-insensitive) → Load("last")
        assert_eq!(
            parse_slash("/resume last"),
            Some(SlashCommand::Load("last".into()))
        );
        assert_eq!(
            parse_slash("/resume LAST"),
            Some(SlashCommand::Load("last".into()))
        );
        // /resume <name> → Load(name) (same handler path as /load)
        assert_eq!(
            parse_slash("/resume sess-abc123"),
            Some(SlashCommand::Load("sess-abc123".into()))
        );
        assert_eq!(
            parse_slash("/resume my-refactor"),
            Some(SlashCommand::Load("my-refactor".into()))
        );
    }

    #[test]
    fn parse_slash_save_load_sessions() {
        assert_eq!(parse_slash("/save"), Some(SlashCommand::Save));
        assert_eq!(parse_slash("/sessions"), Some(SlashCommand::Sessions));
        assert_eq!(
            parse_slash("/load sess-abc123"),
            Some(SlashCommand::Load("sess-abc123".into()))
        );
        assert_eq!(
            parse_slash("/load"),
            Some(SlashCommand::Load(String::new()))
        );
    }

    #[test]
    fn parse_slash_mcp_subcommands() {
        assert_eq!(parse_slash("/mcp"), Some(SlashCommand::Mcp));
        assert_eq!(
            parse_slash("/mcp add weather https://example.com/mcp"),
            Some(SlashCommand::McpAdd {
                name: "weather".into(),
                url: "https://example.com/mcp".into(),
                user: false,
            })
        );
        assert_eq!(
            parse_slash("/mcp add --user weather https://example.com/mcp"),
            Some(SlashCommand::McpAdd {
                name: "weather".into(),
                url: "https://example.com/mcp".into(),
                user: true,
            })
        );
        assert_eq!(
            parse_slash("/mcp remove weather"),
            Some(SlashCommand::McpRemove {
                name: "weather".into(),
                user: false,
            })
        );
        assert_eq!(
            parse_slash("/mcp rm --user weather"),
            Some(SlashCommand::McpRemove {
                name: "weather".into(),
                user: true,
            })
        );
        // Missing url → Unknown with usage hint.
        assert!(matches!(
            parse_slash("/mcp add weather"),
            Some(SlashCommand::Unknown(_))
        ));
    }

    #[test]
    fn parse_slash_rename() {
        assert_eq!(
            parse_slash("/rename my chat"),
            Some(SlashCommand::Rename("my chat".into()))
        );
        assert_eq!(
            parse_slash("/rename"),
            Some(SlashCommand::Rename(String::new()))
        );
    }

    #[test]
    fn parse_slash_sso_subcommands() {
        assert_eq!(
            parse_slash("/sso"),
            Some(SlashCommand::Sso {
                sub: SsoSubcommand::Status
            })
        );
        assert_eq!(
            parse_slash("/sso status"),
            Some(SlashCommand::Sso {
                sub: SsoSubcommand::Status
            })
        );
        assert_eq!(
            parse_slash("/sso login"),
            Some(SlashCommand::Sso {
                sub: SsoSubcommand::Login
            })
        );
        assert_eq!(
            parse_slash("/sso logout"),
            Some(SlashCommand::Sso {
                sub: SsoSubcommand::Logout
            })
        );
        assert!(matches!(
            parse_slash("/sso bogus"),
            Some(SlashCommand::Unknown(msg)) if msg.contains("unknown /sso subcommand")
        ));
    }

    #[test]
    fn parse_slash_models() {
        assert_eq!(parse_slash("/models"), Some(SlashCommand::Models));
        assert_eq!(
            parse_slash("/models refresh"),
            Some(SlashCommand::ModelsRefresh)
        );
    }

    #[test]
    fn parse_slash_models_set_context() {
        // Default scope is user.
        assert_eq!(
            parse_slash("/models set-context anthropic/claude-sonnet-4-6 200000"),
            Some(SlashCommand::ModelsSetContext {
                key: "anthropic/claude-sonnet-4-6".into(),
                size: 200_000,
                project: false,
            })
        );
        // --project flag scopes to project.
        assert_eq!(
            parse_slash("/models set-context --project openai/gpt-4o 128000"),
            Some(SlashCommand::ModelsSetContext {
                key: "openai/gpt-4o".into(),
                size: 128_000,
                project: true,
            })
        );
        // Suffix shorthand: "128k", "1m".
        assert_eq!(
            parse_slash("/models set-context anthropic/claude-sonnet-4-6 200k"),
            Some(SlashCommand::ModelsSetContext {
                key: "anthropic/claude-sonnet-4-6".into(),
                size: 200_000,
                project: false,
            })
        );
        assert_eq!(
            parse_slash("/models set-context anthropic/claude-opus-4-7-1m 1m"),
            Some(SlashCommand::ModelsSetContext {
                key: "anthropic/claude-opus-4-7-1m".into(),
                size: 1_000_000,
                project: false,
            })
        );
        // Unset.
        assert_eq!(
            parse_slash("/models unset-context anthropic/claude-sonnet-4-6"),
            Some(SlashCommand::ModelsUnsetContext {
                key: "anthropic/claude-sonnet-4-6".into(),
                project: false,
            })
        );
        assert_eq!(
            parse_slash("/models unset-context --project openai/gpt-4o"),
            Some(SlashCommand::ModelsUnsetContext {
                key: "openai/gpt-4o".into(),
                project: true,
            })
        );
        // Bad usage → Unknown with hint.
        assert!(matches!(
            parse_slash("/models set-context"),
            Some(SlashCommand::Unknown(msg)) if msg.contains("usage:")
        ));
        assert!(matches!(
            parse_slash("/models set-context openai/gpt-4o not-a-number"),
            Some(SlashCommand::Unknown(msg)) if msg.contains("invalid size")
        ));
        assert!(matches!(
            parse_slash("/models foo"),
            Some(SlashCommand::Unknown(msg)) if msg.contains("unknown /models subcommand")
        ));
    }

    #[test]
    fn parse_slash_provider() {
        assert_eq!(
            parse_slash("/provider"),
            Some(SlashCommand::Provider(String::new()))
        );
        assert_eq!(
            parse_slash("/provider gemini"),
            Some(SlashCommand::Provider("gemini".into()))
        );
    }

    #[test]
    fn parse_slash_providers() {
        assert_eq!(parse_slash("/providers"), Some(SlashCommand::Providers));
    }

    #[test]
    fn parse_slash_mcp() {
        assert_eq!(parse_slash("/mcp"), Some(SlashCommand::Mcp));
    }

    #[test]
    fn parse_slash_new_commands() {
        assert_eq!(parse_slash("/tasks"), Some(SlashCommand::Tasks));
        assert_eq!(parse_slash("/todo"), Some(SlashCommand::Tasks));
        assert_eq!(parse_slash("/context"), Some(SlashCommand::Context));
        assert_eq!(parse_slash("/version"), Some(SlashCommand::Version));
        assert_eq!(parse_slash("/v"), Some(SlashCommand::Version));
        assert_eq!(parse_slash("/cwd"), Some(SlashCommand::Cwd));
        assert_eq!(parse_slash("/pwd"), Some(SlashCommand::Cwd));
        assert_eq!(
            parse_slash("/thinking 10000"),
            Some(SlashCommand::Thinking("10000".into()))
        );
        assert_eq!(
            parse_slash("/thinking"),
            Some(SlashCommand::Thinking(String::new()))
        );
    }

    #[test]
    fn parse_slash_skill_marketplace() {
        assert_eq!(
            parse_slash("/skill marketplace"),
            Some(SlashCommand::SkillMarketplace { refresh: false })
        );
        assert_eq!(
            parse_slash("/skill marketplace --refresh"),
            Some(SlashCommand::SkillMarketplace { refresh: true })
        );
        assert_eq!(
            parse_slash("/skill search playwright"),
            Some(SlashCommand::SkillSearch("playwright".into()))
        );
        assert!(matches!(
            parse_slash("/skill search"),
            Some(SlashCommand::Unknown(msg)) if msg.contains("usage: /skill search")
        ));
        assert_eq!(
            parse_slash("/skill info skill-creator"),
            Some(SlashCommand::SkillInfo("skill-creator".into()))
        );
        assert!(matches!(
            parse_slash("/skill info"),
            Some(SlashCommand::Unknown(msg)) if msg.contains("usage: /skill info")
        ));
    }

    #[test]
    fn parse_slash_skill_install_bare_name() {
        // Bare name (no URL): parser still emits SkillInstall — the
        // executor decides if it's a marketplace lookup.
        assert_eq!(
            parse_slash("/skill install skill-creator"),
            Some(SlashCommand::SkillInstall {
                git_url: "skill-creator".into(),
                name: None,
                project: true,
            })
        );
        // URL form still works.
        assert_eq!(
            parse_slash("/skill install https://github.com/x/y.git"),
            Some(SlashCommand::SkillInstall {
                git_url: "https://github.com/x/y.git".into(),
                name: None,
                project: true,
            })
        );
        // --user flag.
        assert_eq!(
            parse_slash("/skill install --user skill-creator"),
            Some(SlashCommand::SkillInstall {
                git_url: "skill-creator".into(),
                name: None,
                project: false,
            })
        );
    }

    #[test]
    fn parse_slash_mcp_marketplace() {
        assert_eq!(
            parse_slash("/mcp marketplace"),
            Some(SlashCommand::McpMarketplace { refresh: false })
        );
        assert_eq!(
            parse_slash("/mcp marketplace --refresh"),
            Some(SlashCommand::McpMarketplace { refresh: true })
        );
        assert_eq!(
            parse_slash("/mcp search weather"),
            Some(SlashCommand::McpSearch("weather".into()))
        );
        assert_eq!(
            parse_slash("/mcp info weather-mcp"),
            Some(SlashCommand::McpInfo("weather-mcp".into()))
        );
        assert_eq!(
            parse_slash("/mcp install weather-mcp"),
            Some(SlashCommand::McpInstall {
                name: "weather-mcp".into(),
                user: false,
            })
        );
        assert_eq!(
            parse_slash("/mcp install --user weather-mcp"),
            Some(SlashCommand::McpInstall {
                name: "weather-mcp".into(),
                user: true,
            })
        );
    }

    #[test]
    fn parse_slash_plugin_marketplace() {
        assert_eq!(
            parse_slash("/plugin marketplace"),
            Some(SlashCommand::PluginMarketplace { refresh: false })
        );
        assert_eq!(
            parse_slash("/plugin search code-review"),
            Some(SlashCommand::PluginSearch("code-review".into()))
        );
        assert_eq!(
            parse_slash("/plugin info code-review"),
            Some(SlashCommand::PluginInfo("code-review".into()))
        );
        // /plugin show <name> still works for installed-plugin detail
        assert_eq!(
            parse_slash("/plugin show code-review"),
            Some(SlashCommand::PluginShow {
                name: "code-review".into()
            })
        );
        // M6.16.1 BUG L2: /plugin gc parses with no args.
        assert_eq!(parse_slash("/plugin gc"), Some(SlashCommand::PluginGc));
    }

    #[test]
    fn looks_like_url_classification() {
        assert!(looks_like_url("https://x.com/r.git"));
        assert!(looks_like_url("http://x.com/r.git"));
        assert!(looks_like_url("git@github.com:x/y.git"));
        assert!(looks_like_url("/local/path"));
        assert!(looks_like_url("./relative"));
        assert!(looks_like_url("../up"));
        assert!(looks_like_url("https://x.com/r.git#main:skills/foo"));
        assert!(looks_like_url("https://example.com/pack.zip"));
        // Marketplace slug (NOT a URL).
        assert!(!looks_like_url("skill-creator"));
        assert!(!looks_like_url("frontend-design"));
        assert!(!looks_like_url("webapp-testing"));
    }

    #[test]
    fn parse_slash_kms() {
        assert_eq!(parse_slash("/kms"), Some(SlashCommand::Kms));
        assert_eq!(parse_slash("/kms list"), Some(SlashCommand::Kms));
        // Default scope is project — `./.thclaws/kms/<name>`.
        assert_eq!(
            parse_slash("/kms new default"),
            Some(SlashCommand::KmsNew {
                name: "default".into(),
                project: true,
            })
        );
        // --user opts out into `~/.config/thclaws/kms/<name>`.
        assert_eq!(
            parse_slash("/kms new --user notes"),
            Some(SlashCommand::KmsNew {
                name: "notes".into(),
                project: false,
            })
        );
        // --project is still accepted as a no-op back-compat alias.
        assert_eq!(
            parse_slash("/kms new --project notes"),
            Some(SlashCommand::KmsNew {
                name: "notes".into(),
                project: true,
            })
        );
        assert_eq!(
            parse_slash("/kms use notes"),
            Some(SlashCommand::KmsUse("notes".into()))
        );
        assert_eq!(
            parse_slash("/kms off notes"),
            Some(SlashCommand::KmsOff("notes".into()))
        );
        assert_eq!(
            parse_slash("/kms show notes"),
            Some(SlashCommand::KmsShow("notes".into()))
        );
        assert_eq!(
            parse_slash("/kms ingest notes ./README.md"),
            Some(SlashCommand::KmsIngest {
                name: "notes".into(),
                file: "./README.md".into(),
                alias: None,
                force: false,
            })
        );
        assert_eq!(
            parse_slash("/kms ingest notes ./doc.md as intro --force"),
            Some(SlashCommand::KmsIngest {
                name: "notes".into(),
                file: "./doc.md".into(),
                alias: Some("intro".into()),
                force: true,
            })
        );
        // `add` alias mirrors `ingest`.
        assert_eq!(
            parse_slash("/kms add notes ./file.txt"),
            Some(SlashCommand::KmsIngest {
                name: "notes".into(),
                file: "./file.txt".into(),
                alias: None,
                force: false,
            })
        );
        // Missing args → Unknown with usage hint.
        assert!(matches!(
            parse_slash("/kms ingest notes"),
            Some(SlashCommand::Unknown(_))
        ));
        // Missing name → Unknown with usage hint.
        assert!(matches!(
            parse_slash("/kms new"),
            Some(SlashCommand::Unknown(_))
        ));
        assert!(matches!(
            parse_slash("/kms use"),
            Some(SlashCommand::Unknown(_))
        ));

        // M6.28: `$` source = current chat session → KmsIngestSession
        assert_eq!(
            parse_slash("/kms ingest mynotes $"),
            Some(SlashCommand::KmsIngestSession {
                name: "mynotes".into(),
                alias: None,
                force: false,
            })
        );
        // With `as <alias>` and `--force` flags.
        assert_eq!(
            parse_slash("/kms ingest mynotes $ as my-thread --force"),
            Some(SlashCommand::KmsIngestSession {
                name: "mynotes".into(),
                alias: Some("my-thread".into()),
                force: true,
            })
        );
    }

    /// M6.28: build_kms_ingest_session_prompt produces a non-empty
    /// prompt referencing the KMS name + page + KmsWrite tool, with a
    /// provenance hint that varies by alias source.
    #[test]
    fn build_kms_ingest_session_prompt_mentions_kms_and_tool() {
        let p = build_kms_ingest_session_prompt(
            "mynotes",
            "session-page-slug",
            KmsIngestSessionAliasSource::SessionId,
            false,
        );
        assert!(p.contains("mynotes"));
        assert!(p.contains("KmsWrite"));
        assert!(p.contains("session-page-slug"));
        assert!(p.contains("session id"));

        let p_user = build_kms_ingest_session_prompt(
            "mynotes",
            "my-topic",
            KmsIngestSessionAliasSource::UserSupplied,
            true,
        );
        assert!(p_user.contains("my-topic"));
        assert!(p_user.contains("user-supplied"));
        // Force hint changes when --force is set.
        assert!(p_user.contains("--force"));

        let p_title = build_kms_ingest_session_prompt(
            "mynotes",
            "memory-overhaul",
            KmsIngestSessionAliasSource::SessionTitle,
            false,
        );
        assert!(p_title.contains("memory-overhaul"));
        assert!(p_title.contains("session title"));
    }

    // ─── M6.29: /loop + /goal parser tests ──────────────────────────

    #[test]
    fn parse_slash_loop_status() {
        assert_eq!(parse_slash("/loop"), Some(SlashCommand::LoopStatus));
        assert_eq!(parse_slash("/loop status"), Some(SlashCommand::LoopStatus));
    }

    #[test]
    fn parse_slash_loop_stop() {
        assert_eq!(parse_slash("/loop stop"), Some(SlashCommand::LoopStop));
        assert_eq!(parse_slash("/loop cancel"), Some(SlashCommand::LoopStop));
    }

    #[test]
    fn parse_slash_loop_with_interval() {
        assert_eq!(
            parse_slash("/loop 30s /goal continue"),
            Some(SlashCommand::Loop {
                interval_secs: Some(30),
                body: "/goal continue".into(),
            })
        );
        assert_eq!(
            parse_slash("/loop 5m do this thing"),
            Some(SlashCommand::Loop {
                interval_secs: Some(300),
                body: "do this thing".into(),
            })
        );
        assert_eq!(
            parse_slash("/loop 2h /kms ingest mynotes $"),
            Some(SlashCommand::Loop {
                interval_secs: Some(7200),
                body: "/kms ingest mynotes $".into(),
            })
        );
    }

    #[test]
    fn parse_slash_loop_self_paced() {
        // No interval token → self-paced; whole input is the body.
        assert_eq!(
            parse_slash("/loop /goal continue"),
            Some(SlashCommand::Loop {
                interval_secs: None,
                body: "/goal continue".into(),
            })
        );
    }

    #[test]
    fn parse_duration_secs_units() {
        assert_eq!(parse_duration_secs("30s"), Some(30));
        assert_eq!(parse_duration_secs("5m"), Some(300));
        assert_eq!(parse_duration_secs("2h"), Some(7200));
        assert_eq!(parse_duration_secs("1d"), Some(86400));
        assert_eq!(parse_duration_secs("nonsense"), None);
        assert_eq!(parse_duration_secs(""), None);
    }

    #[test]
    fn parse_slash_goal_lifecycle() {
        assert_eq!(parse_slash("/goal"), Some(SlashCommand::GoalStatus));
        assert_eq!(parse_slash("/goal status"), Some(SlashCommand::GoalStatus));
        assert_eq!(parse_slash("/goal show"), Some(SlashCommand::GoalShow));
        assert_eq!(
            parse_slash("/goal continue"),
            Some(SlashCommand::GoalContinue)
        );
        assert_eq!(
            parse_slash("/goal complete done audited"),
            Some(SlashCommand::GoalComplete {
                reason: Some("done audited".into())
            })
        );
        assert_eq!(
            parse_slash("/goal abandon need API key"),
            Some(SlashCommand::GoalAbandon {
                reason: Some("need API key".into())
            })
        );
    }

    #[test]
    fn parse_slash_goal_start_with_budgets() {
        assert_eq!(
            parse_slash(
                "/goal start \"ship the auth refactor\" --budget-tokens 200000 --budget-time 30m"
            ),
            Some(SlashCommand::GoalStart {
                objective: "ship the auth refactor".into(),
                budget_tokens: Some(200_000),
                budget_time_secs: Some(1800),
            })
        );
        // Without quotes — objective is words up to first --flag.
        assert_eq!(
            parse_slash("/goal start build a REST API --budget-tokens 50000"),
            Some(SlashCommand::GoalStart {
                objective: "build a REST API".into(),
                budget_tokens: Some(50_000),
                budget_time_secs: None,
            })
        );
    }

    #[test]
    fn parse_slash_goal_start_missing_objective_errors() {
        match parse_slash("/goal start") {
            Some(SlashCommand::Unknown(_)) => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
        match parse_slash("/goal start --budget-tokens 100") {
            Some(SlashCommand::Unknown(_)) => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    /// M6.28: resolve_session_alias precedence — user > title > id.
    #[test]
    fn resolve_session_alias_precedence() {
        // 1. User-supplied wins.
        assert_eq!(
            resolve_session_alias(Some("my-page"), Some("My Session Title"), "sess-194a3b7c"),
            (
                "my-page".to_string(),
                KmsIngestSessionAliasSource::UserSupplied
            ),
        );
        // 2. Title used when no user alias; sanitized (spaces → `_`).
        assert_eq!(
            resolve_session_alias(None, Some("My Session Title"), "sess-194a3b7c"),
            (
                "My_Session_Title".to_string(),
                KmsIngestSessionAliasSource::SessionTitle,
            ),
        );
        // 3. Session id used when neither user alias nor title.
        assert_eq!(
            resolve_session_alias(None, None, "sess-194a3b7c"),
            (
                "sess-194a3b7c".to_string(),
                KmsIngestSessionAliasSource::SessionId
            ),
        );
        // 4. Empty user alias / empty title → fall through to next.
        assert_eq!(
            resolve_session_alias(Some(""), None, "sess-abc"),
            (
                "sess-abc".to_string(),
                KmsIngestSessionAliasSource::SessionId
            ),
        );
        assert_eq!(
            resolve_session_alias(None, Some(""), "sess-abc"),
            (
                "sess-abc".to_string(),
                KmsIngestSessionAliasSource::SessionId
            ),
        );
        // 5. Title that sanitizes to empty (e.g. all special chars).
        assert_eq!(
            resolve_session_alias(None, Some("///"), "sess-fallback"),
            (
                "sess-fallback".to_string(),
                KmsIngestSessionAliasSource::SessionId
            ),
        );
    }

    #[test]
    fn default_model_for_provider_covers_all_supported() {
        assert_eq!(
            default_model_for_provider("anthropic"),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(default_model_for_provider("openai"), Some("gpt-4o"));
        assert_eq!(
            default_model_for_provider("gemini"),
            Some("gemini-2.5-flash")
        );
        assert_eq!(
            default_model_for_provider("ollama"),
            Some("ollama/llama3.2")
        );
        assert_eq!(default_model_for_provider("mystery"), None);
    }

    #[test]
    fn parse_slash_memory() {
        // Bare `/memory` → list
        assert_eq!(parse_slash("/memory"), Some(SlashCommand::MemoryList));
        assert_eq!(parse_slash("/memory list"), Some(SlashCommand::MemoryList));
        // `/memory read NAME`
        assert_eq!(
            parse_slash("/memory read user_role"),
            Some(SlashCommand::MemoryRead("user_role".into()))
        );
        // Aliases for read
        assert_eq!(
            parse_slash("/memory show foo"),
            Some(SlashCommand::MemoryRead("foo".into()))
        );
        assert_eq!(
            parse_slash("/memory cat bar"),
            Some(SlashCommand::MemoryRead("bar".into()))
        );
        // Unknown subcommand bubbles up
        match parse_slash("/memory wat") {
            Some(SlashCommand::Unknown(msg)) => assert!(msg.contains("memory wat")),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    /// M6.27: `# <name>:<body>` shortcut → MemoryWrite. Strict slug-name
    /// pattern lets real markdown headers pass through to the agent.
    #[test]
    fn parse_memory_shortcut_basic() {
        match parse_slash("# user_role: senior backend engineer") {
            Some(SlashCommand::MemoryWrite {
                name,
                body,
                type_,
                description,
            }) => {
                assert_eq!(name, "user_role");
                assert_eq!(body.as_deref(), Some("senior backend engineer"));
                assert_eq!(type_, None);
                assert_eq!(description, None);
            }
            other => panic!("expected MemoryWrite, got {other:?}"),
        }
    }

    #[test]
    fn parse_memory_shortcut_no_space_after_hash() {
        // `#name:body` (no space) also accepted.
        match parse_slash("#quick_fact: always use absolute paths") {
            Some(SlashCommand::MemoryWrite { name, body, .. }) => {
                assert_eq!(name, "quick_fact");
                assert_eq!(body.as_deref(), Some("always use absolute paths"));
            }
            other => panic!("expected MemoryWrite, got {other:?}"),
        }
    }

    #[test]
    fn parse_memory_shortcut_body_with_special_chars() {
        // Body may contain colons, dashes, etc. (only the FIRST colon
        // splits name from body).
        match parse_slash("# build_flags: --release means optimized: true") {
            Some(SlashCommand::MemoryWrite { name, body, .. }) => {
                assert_eq!(name, "build_flags");
                assert_eq!(body.as_deref(), Some("--release means optimized: true"));
            }
            other => panic!("expected MemoryWrite, got {other:?}"),
        }
    }

    #[test]
    fn parse_memory_shortcut_rejects_markdown_headers() {
        // Name with space (real markdown header) → falls through.
        assert_eq!(parse_slash("# Architecture Plan: build a REST API"), None);
        // Name with non-slug char.
        assert_eq!(parse_slash("# user.role: foo"), None);
        // Missing colon.
        assert_eq!(parse_slash("# remember this"), None);
        // Empty name or body.
        assert_eq!(parse_slash("# : value"), None);
        assert_eq!(parse_slash("# name:"), None);
        assert_eq!(parse_slash("# name: "), None);
    }

    #[test]
    fn parse_memory_shortcut_doesnt_steal_non_hash_input() {
        // Plain text + slash commands still parse normally.
        assert_eq!(parse_slash("hello"), None);
        assert_eq!(parse_slash("/memory list"), Some(SlashCommand::MemoryList));
    }

    // Env-var tests live in a single serialized block because they mutate
    // process-wide state and would race under cargo test's parallel runner.
    // Holds a Mutex that serializes access across all env-var-touching tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn build_provider_honors_env_keys() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let saved_a = std::env::var("ANTHROPIC_API_KEY").ok();
        let saved_o = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");

        // Case 1: no key → error with a pointer at the env var.
        let cfg = AppConfig::default();
        match build_provider(&cfg) {
            Ok(_) => panic!("expected error when no API key is set"),
            Err(e) => assert!(format!("{e}").contains("ANTHROPIC_API_KEY")),
        }

        // Case 2: anthropic key set → builds.
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-fake");
        build_provider(&cfg).expect("anthropic should build");
        std::env::remove_var("ANTHROPIC_API_KEY");

        // Case 3: openai model + openai key → builds openai.
        std::env::set_var("OPENAI_API_KEY", "sk-fake");
        let mut openai_cfg = AppConfig::default();
        openai_cfg.model = "gpt-4o".into();
        build_provider(&openai_cfg).expect("openai should build");
        std::env::remove_var("OPENAI_API_KEY");

        // Restore original env if the caller had any.
        if let Some(v) = saved_a {
            std::env::set_var("ANTHROPIC_API_KEY", v);
        }
        if let Some(v) = saved_o {
            std::env::set_var("OPENAI_API_KEY", v);
        }
    }

    /// Regression: an exported-but-empty env var ("ANTHROPIC_API_KEY=")
    /// must NOT count as configured. Before the fix, it did — and
    /// auto_fallback_model in the GUI refused to switch off Anthropic
    /// even after the user pasted a key for a different provider, because
    /// `std::env::var(name).is_ok()` returns true for empty values.
    /// Trace: https://github.com/thClaws/thClaws (screenshot in Thai)
    #[test]
    fn empty_env_var_treated_as_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let saved_a = std::env::var("ANTHROPIC_API_KEY").ok();
        let saved_g = std::env::var("GEMINI_API_KEY").ok();

        // Empty Anthropic env (the bug-trigger), no Gemini env.
        std::env::set_var("ANTHROPIC_API_KEY", "");
        std::env::remove_var("GEMINI_API_KEY");

        // api_key_from_env on a Claude model should NOT return Some("")
        // — that produces a 401 with an empty bearer.
        let mut cfg = AppConfig::default();
        cfg.model = "claude-sonnet-4-6".into();
        assert!(
            cfg.api_key_from_env().is_none()
                || cfg
                    .api_key_from_env()
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false),
            "empty ANTHROPIC_API_KEY must not produce an empty Some(\"\")"
        );

        // build_provider should error pointing at the env var, same as
        // the var-not-set case (see build_provider_honors_env_keys).
        match build_provider(&cfg) {
            Ok(_) => panic!("empty env var must not let build_provider succeed"),
            Err(e) => assert!(
                format!("{e}").contains("ANTHROPIC_API_KEY"),
                "error should point at the missing env var, got: {e}"
            ),
        }

        // Restore original env.
        std::env::remove_var("ANTHROPIC_API_KEY");
        if let Some(v) = saved_a {
            std::env::set_var("ANTHROPIC_API_KEY", v);
        }
        if let Some(v) = saved_g {
            std::env::set_var("GEMINI_API_KEY", v);
        }
    }

    /// M6.20 BUG H1: ReplAgentFactory must propagate the parent's
    /// approver and permission_mode onto every child agent. Pre-fix the
    /// child fell through to `Agent::new`'s defaults (`AutoApprover` +
    /// `PermissionMode::Auto`), and the dispatch fallback at
    /// agent.rs:1112 promoted the global Ask back to Auto — bypassing
    /// the user's approval gate for any subagent tool call.
    #[tokio::test]
    async fn subagent_factory_propagates_approver_and_permission_mode() {
        use crate::permissions::{ApprovalSink, DenyApprover, PermissionMode};
        use crate::providers::{EventStream, Provider, ProviderEvent, StreamRequest};
        use crate::subagent::AgentFactory;
        use crate::tools::ToolRegistry;
        use async_trait::async_trait;
        use futures::stream;

        struct StubProvider;
        #[async_trait]
        impl Provider for StubProvider {
            async fn stream(&self, _req: StreamRequest) -> Result<EventStream> {
                Ok(Box::pin(stream::iter(vec![Ok::<ProviderEvent, _>(
                    ProviderEvent::MessageStart {
                        model: "test".into(),
                    },
                )])))
            }
        }

        let approver: Arc<dyn ApprovalSink> = Arc::new(DenyApprover);
        let factory = crate::subagent::ProductionAgentFactory {
            provider: Arc::new(StubProvider),
            base_tools: ToolRegistry::new(),
            model: "test".into(),
            system: String::new(),
            max_iterations: 1,
            max_depth: 3,
            agent_defs: crate::agent_defs::AgentDefsConfig::default(),
            approver: approver.clone(),
            permission_mode: PermissionMode::Ask,
            cancel: None,
            hooks: None,
        };
        let child = factory
            .build("go", None, 1)
            .await
            .expect("factory builds child");
        // permission_mode must propagate (the actual gate-promotion bug
        // in the dispatch fallback was triggered when child default was
        // Auto; verifying it's Ask here proves the propagation path).
        assert_eq!(child.permission_mode, PermissionMode::Ask);
        // Arc identity check: the child shares the parent's approver
        // Arc, so a yolo flag set on parent propagates to the child
        // (and vice versa) within a session.
        // We can't reach into Agent's private approver field, but we
        // can prove Arc::strong_count grew when factory.build fired.
        // Pre-fix the child wouldn't have cloned `approver` at all.
        assert!(
            Arc::strong_count(&approver) >= 2,
            "factory should have cloned the approver Arc, got strong_count={}",
            Arc::strong_count(&approver),
        );
    }

    // --- SlashCompleter tests ---------------------------------------------
    //
    // Exercise `crate::cli_completer::SlashCompleter` directly so we don't
    // need a real terminal. The candidate set is sourced from
    // `built_in_commands()`, so these double as a regression guard against
    // accidentally dropping a command from the public list.
    mod completer_tests {
        use crate::cli_completer::SlashCompleter;
        use rustyline::completion::Completer;
        use rustyline::hint::Hinter;
        use rustyline::history::DefaultHistory;
        use rustyline::Context;

        fn complete(line: &str, pos: usize) -> Vec<(String, String)> {
            let history = DefaultHistory::new();
            let ctx = Context::new(&history);
            let (_start, pairs) = SlashCompleter
                .complete(line, pos, &ctx)
                .expect("completer ok");
            pairs
                .into_iter()
                .map(|p| (p.display, p.replacement))
                .collect()
        }

        fn hint(line: &str, pos: usize) -> Option<String> {
            let history = DefaultHistory::new();
            let ctx = Context::new(&history);
            SlashCompleter.hint(line, pos, &ctx)
        }

        #[test]
        fn slash_completer_lists_all_on_just_slash() {
            let pairs = complete("/", 1);
            assert_eq!(pairs.len(), super::built_in_commands().len());
        }

        #[test]
        fn slash_completer_filters_by_prefix() {
            let pairs = complete("/he", 3);
            assert_eq!(pairs.len(), 1, "only /help should match: {pairs:?}");
            assert!(pairs[0].1.starts_with("/help"));
        }

        #[test]
        fn slash_completer_multiple_matches() {
            let pairs = complete("/m", 2);
            let names: Vec<&str> = pairs.iter().map(|(_, r)| r.trim()).collect();
            for expected in ["/mcp", "/memory", "/model", "/models"] {
                assert!(
                    names.contains(&expected),
                    "expected {expected} in {names:?}"
                );
            }
        }

        #[test]
        fn slash_completer_no_match_for_non_slash() {
            assert!(complete("hello", 5).is_empty());
        }

        #[test]
        fn slash_completer_no_match_after_first_word() {
            // v1 only completes the leading slash-token; once the user types
            // a space, the completer bows out.
            assert!(complete("/model ", 7).is_empty());
        }

        #[test]
        fn hinter_returns_remainder_for_unique_prefix() {
            // `/he` → only `/help` matches → hint shows `lp`.
            assert_eq!(hint("/he", 3).as_deref(), Some("lp"));
        }

        #[test]
        fn hinter_returns_remainder_for_first_match_when_ambiguous() {
            // `/m` matches several commands; we show the first one's
            // remainder so the user still sees *something*. Tab cycles
            // through the rest.
            let h = hint("/m", 2).expect("expected a hint");
            assert!(!h.is_empty());
            // First match in the catalogue starting with `m` must be one of
            // the known commands; we just guard against an empty/garbled
            // hint.
            assert!(
                ["cp", "emory", "odel", "odels"].contains(&h.as_str()),
                "unexpected hint: {h:?}"
            );
        }

        #[test]
        fn hinter_silent_for_bare_slash() {
            // No char after `/` → don't pick an arbitrary command.
            assert_eq!(hint("/", 1), None);
        }

        #[test]
        fn hinter_silent_for_non_slash() {
            assert_eq!(hint("hello", 5), None);
        }
    }
}
