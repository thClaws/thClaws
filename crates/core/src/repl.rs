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
    openai::OpenAIProvider, Provider, ProviderKind,
};
use crate::session::{Session, SessionStore};
use crate::subagent::{AgentFactory, SubAgentTool};
use crate::tools::ToolRegistry;
use async_trait::async_trait;
use futures::StreamExt;
use std::io::Write;
use std::sync::Arc;

const COLOR_RESET: &str = "\x1b[0m";
const COLOR_DIM: &str = "\x1b[90m";
const COLOR_GREEN: &str = "\x1b[32m";
const COLOR_CYAN: &str = "\x1b[36m";
const COLOR_YELLOW: &str = "\x1b[33m";
const COLOR_BOLD: &str = "\x1b[1m";

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
    SkillInstall {
        git_url: String,
        name: Option<String>,
        project: bool,
    },
    SkillShow(String),
    Permissions(String),
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
    Unknown(String),
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
                    "usage: /plugin install [--user] <git-url-or-.zip>".into(),
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
        "show" | "info" => match rest.split_whitespace().next() {
            Some(name) => SlashCommand::PluginShow { name: name.to_string() },
            None => SlashCommand::Unknown("usage: /plugin show <name>".into()),
        },
        other => SlashCommand::Unknown(format!(
            "unknown plugin subcommand: '{other}' (try: /plugin, /plugin install …, /plugin remove …, /plugin enable …, /plugin disable …, /plugin show …)"
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
        other => SlashCommand::Unknown(format!(
            "unknown mcp subcommand: '{other}' (try: /mcp, /mcp add …, /mcp remove …)"
        )),
    }
}

/// Default model to select when switching provider by name only.
/// Thin wrapper around `ProviderKind::from_name` + `default_model` for
/// backward-compat tests and REPL call sites that already use `&str`.
pub fn default_model_for_provider(provider: &str) -> Option<&'static str> {
    ProviderKind::from_name(provider).map(|k| k.default_model())
}

/// Parse a line as a slash command. Returns `None` when the line isn't a
/// slash command (so the caller can treat it as a user prompt).
pub fn parse_slash(input: &str) -> Option<SlashCommand> {
    let input = input.trim();
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
        "models" => match args.trim() {
            "refresh" => SlashCommand::ModelsRefresh,
            "" => SlashCommand::Models,
            other => SlashCommand::Unknown(format!(
                "unknown /models subcommand: '{other}' (try /models or /models refresh)"
            )),
        },
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
        "skills" => SlashCommand::Skills,
        "skill" => {
            // Supported (project scope is the default; --user opts out):
            //   /skill install <url>
            //   /skill install --user <url>
            //   /skill install <url> <name>
            //   /skill install --user <url> <name>
            // `<url>` is either a git repo or a `.zip` archive URL.
            let rest = args.trim();
            if let Some(after_show) = rest.strip_prefix("show").map(str::trim_start) {
                if after_show.is_empty() {
                    SlashCommand::Unknown("usage: /skill show <name>".into())
                } else {
                    SlashCommand::SkillShow(after_show.to_string())
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
                    [url] => SlashCommand::SkillInstall {
                        git_url: url.to_string(),
                        name: None,
                        project,
                    },
                    [url, name] => SlashCommand::SkillInstall {
                        git_url: url.to_string(),
                        name: Some(name.to_string()),
                        project,
                    },
                    _ => SlashCommand::Unknown(
                        "usage: /skill install [--user] <git-url-or-.zip> [name]".into(),
                    ),
                }
            } else {
                SlashCommand::Unknown(format!(
                    "unknown skill subcommand: '{rest}' (try: /skill install …)"
                ))
            }
        }
        "permissions" | "perms" => SlashCommand::Permissions(args.to_string()),
        "team" => SlashCommand::Team,
        "usage" => SlashCommand::Usage,
        "memory" => {
            let (sub, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
            match sub {
                "" | "list" => SlashCommand::MemoryList,
                "read" | "show" | "cat" => SlashCommand::MemoryRead(rest.trim().to_string()),
                other => SlashCommand::Unknown(format!("memory {other}")),
            }
        }
        "kms" => parse_kms_subcommand(args),
        _ => SlashCommand::Unknown(cmd.to_string()),
    })
}

/// Parse `/kms [list|new|use|off|show ...]`.
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
            // Syntax: /kms ingest <kms-name> <file> [as <alias>] [--force]
            //
            // KMS name is always explicit — we don't want to "helpfully"
            // pick an active KMS when the user has several attached and
            // mean the one they think of.
            let mut parts: Vec<&str> = rest.split_whitespace().collect();
            let mut force = false;
            if let Some(i) = parts.iter().position(|p| *p == "--force" || *p == "-f") {
                force = true;
                parts.remove(i);
            }
            // Pull optional `as <alias>` out before parsing positionals so
            // the alias slot isn't sensitive to position.
            let mut alias: Option<String> = None;
            if let Some(i) = parts.iter().position(|p| *p == "as") {
                if i + 1 < parts.len() {
                    alias = Some(parts[i + 1].to_string());
                    parts.drain(i..=i + 1);
                } else {
                    return SlashCommand::Unknown(
                        "usage: /kms ingest <kms> <file> [as <alias>] [--force]".into(),
                    );
                }
            }
            match parts.as_slice() {
                [name, file] => SlashCommand::KmsIngest {
                    name: (*name).to_string(),
                    file: (*file).to_string(),
                    alias,
                    force,
                },
                _ => SlashCommand::Unknown(
                    "usage: /kms ingest <kms> <file> [as <alias>] [--force]".into(),
                ),
            }
        }
        other => SlashCommand::Unknown(format!(
            "unknown kms subcommand: '{other}' (try: /kms, /kms new …, /kms use …, /kms off …, /kms show …, /kms ingest …)"
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

        // Context / memory / knowledge
        BuiltInCommand { name: "context",  description: "Show context-window usage breakdown",        category: "Context", usage: "" },
        BuiltInCommand { name: "memory",   description: "List memory entries",                        category: "Context", usage: "" },
        BuiltInCommand { name: "kms",      description: "List knowledge bases",                       category: "Context", usage: "" },

        // Skills, plugins, MCP
        BuiltInCommand { name: "skills",   description: "List installed skills",                      category: "Extensions", usage: "" },
        BuiltInCommand { name: "plugins",  description: "List installed plugins",                     category: "Extensions", usage: "" },
        BuiltInCommand { name: "mcp",      description: "List active MCP servers and their tools",    category: "Extensions", usage: "" },

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
    "Slash commands:\n  \
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
        ProviderKind::Ollama
        | ProviderKind::OllamaAnthropic
        | ProviderKind::LMStudio
        | ProviderKind::AgentSdk => {
            unreachable!("handled above")
        }
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
        ProviderKind::Ollama,
        ProviderKind::OllamaAnthropic,
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

/// Agent factory used by the REPL's `Task` sub-agent tool.
///
/// Supports multi-level recursion: child agents get their own `Task` tool
/// at `depth + 1`, so they can delegate further up to `max_depth`.
/// Named agent definitions override model, instructions, and tool subset.
struct ReplAgentFactory {
    provider: Arc<dyn Provider>,
    base_tools: ToolRegistry,
    model: String,
    system: String,
    max_iterations: usize,
    max_depth: usize,
    agent_defs: crate::agent_defs::AgentDefsConfig,
}

#[async_trait]
impl AgentFactory for ReplAgentFactory {
    async fn build(
        &self,
        _prompt: &str,
        agent_def: Option<&crate::agent_defs::AgentDef>,
        child_depth: usize,
    ) -> Result<Agent> {
        let model = agent_def
            .and_then(|d| d.model.as_deref())
            .unwrap_or(&self.model);
        let mut system = agent_def
            .map(|d| {
                if d.instructions.is_empty() {
                    self.system.clone()
                } else {
                    format!(
                        "{}\n\n# Agent instructions\n{}",
                        self.system, d.instructions
                    )
                }
            })
            .unwrap_or_else(|| self.system.clone());
        // Every sub-agent (launched via the Task tool, i.e. child_depth > 0)
        // gets a generic addendum explaining sub-agent semantics. Override in
        // .thclaws/prompt/subagent.md.
        if child_depth > 0 {
            system.push_str(&crate::prompts::load(
                "subagent",
                crate::prompts::defaults::SUBAGENT,
            ));
        }
        let max_iter = agent_def
            .map(|d| d.max_iterations)
            .unwrap_or(self.max_iterations);

        // Build tool registry — filter by agent def's tools list if specified.
        let mut tools = if let Some(def) = agent_def {
            if def.tools.is_empty() {
                self.base_tools.clone()
            } else {
                let mut filtered = ToolRegistry::new();
                for name in &def.tools {
                    if let Some(tool) = self.base_tools.get(name) {
                        filtered.register(tool);
                    }
                }
                filtered
            }
        } else {
            self.base_tools.clone()
        };

        // Add a Task tool at the next depth (multi-level recursion).
        if child_depth < self.max_depth {
            let child_factory = Arc::new(ReplAgentFactory {
                provider: self.provider.clone(),
                base_tools: self.base_tools.clone(),
                model: self.model.clone(),
                system: self.system.clone(),
                max_iterations: self.max_iterations,
                max_depth: self.max_depth,
                agent_defs: self.agent_defs.clone(),
            });
            tools.register(Arc::new(
                SubAgentTool::new(child_factory)
                    .with_depth(child_depth)
                    .with_max_depth(self.max_depth)
                    .with_agent_defs(self.agent_defs.clone()),
            ));
        }

        Ok(Agent::new(self.provider.clone(), tools, model, &system).with_max_iterations(max_iter))
    }
}

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
    }
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
    }
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
        skill_store_handle = Some(skill_tool.store_handle());
        tool_registry.register(Arc::new(skill_tool));
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

    // Register the Task tool with multi-level recursion support.
    // Child agents get their own Task tool at depth+1 (up to max_depth).
    {
        let plugin_agent_dirs = crate::plugins::plugin_agent_dirs();
        let agent_defs = crate::agent_defs::AgentDefsConfig::load_with_extra(&plugin_agent_dirs);
        let base_tools = tool_registry.clone();
        let factory = Arc::new(ReplAgentFactory {
            provider: provider.clone(),
            base_tools,
            model: config.model.clone(),
            system: system.clone(),
            max_iterations: config.max_iterations,
            max_depth: crate::subagent::DEFAULT_MAX_DEPTH,
            agent_defs: agent_defs.clone(),
        });
        tool_registry.register(Arc::new(
            SubAgentTool::new(factory)
                .with_depth(0)
                .with_agent_defs(agent_defs),
        ));
    }
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
        // Always keep team-essential tools for teammates.
        if team_agent_name.is_some() {
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

    // Team agents always run in auto mode (no approval prompts).
    let perm_mode = if team_agent_name.is_some() || config.permissions == "auto" {
        PermissionMode::Auto
    } else {
        PermissionMode::Ask
    };
    let approver = ReplApprover::new();
    let mut agent = Agent::new(
        provider,
        tool_registry.clone(),
        config.model.clone(),
        system.clone(),
    )
    .with_max_iterations(config.max_iterations)
    .with_permission_mode(perm_mode)
    .with_approver(approver.clone());

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
    if team_agent_name.is_none() {
        const BANNER: &str = include_str!("../../../banner.txt");
        println!("\n{COLOR_CYAN}{BANNER}{COLOR_RESET}");
        println!();
    }
    println!(
        "{COLOR_BOLD}thClaws {}{COLOR_RESET} {COLOR_DIM}({}{}) — model: {} · permissions: {} · session: {}{COLOR_RESET}",
        v.version, v.git_sha, dirty_tag, config.model, perm_label, session.id
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
    if team_enabled {
        let mailbox = crate::team::Mailbox::new(crate::team::Mailbox::default_dir());
        tokio::spawn(async move {
            loop {
                let unread = mailbox.read_unread("lead").unwrap_or_default();
                if !unread.is_empty() {
                    let ids: Vec<String> = unread.iter().map(|m| m.id.clone()).collect();
                    let _ = mailbox.mark_as_read("lead", &ids);
                    let _ = inbox_tx.send(unread);
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(
                    crate::team::POLL_INTERVAL_MS,
                ))
                .await;
            }
        });
    }

    // Shared readline editor for spawn_blocking calls.
    let rl_mutex = std::sync::Arc::new(std::sync::Mutex::new(
        rustyline::DefaultEditor::new().map_err(|e| Error::Agent(format!("readline init: {e}")))?,
    ));

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

    // ── Normal interactive REPL ──────────────────────────────────────
    // Uses select! to race user input against team inbox messages so the
    // lead can respond to teammates without the user needing to press Enter.
    loop {
        // Spawn readline on a blocking thread so it doesn't block tokio.
        let rl_clone = rl_mutex.clone();
        let readline_task = tokio::task::spawn_blocking(move || {
            let mut rl = rl_clone.lock().unwrap();
            match rl.readline(&format!("{COLOR_CYAN}❯ {COLOR_RESET}")) {
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

        // Race readline against team inbox messages.
        let mut line: String;
        tokio::pin!(readline_task);
        loop {
            tokio::select! {
                result = &mut readline_task => {
                    match result {
                        Ok(Some(l)) => { line = l; break; }
                        _ => {
                            let _ = std::process::Command::new("pkill")
                                .args(["-f", "team-agent"])
                                .status();
                            println!("{COLOR_DIM}bye{COLOR_RESET}");
                            return Ok(());
                        }
                    }
                }
                Some(msgs) = inbox_rx.recv() => {
                    process_team_messages!(msgs);
                    // Reprint prompt hint since our output pushed it up.
                    print!("{COLOR_CYAN}❯ {COLOR_RESET}");
                    let _ = std::io::stdout().flush();
                }
            }
        }

        if line.is_empty() {
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
                        println!("{COLOR_DIM}model: {} (provider: {}){COLOR_RESET}", config.model, provider_name);
                        continue;
                    }
                    // Resolve short aliases ("sonnet" → "claude-sonnet-4-6",
                    // "flash" → "gemini-2.0-flash", etc.) to the canonical
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
                    .with_approver(approver.clone());
                    agent.clear_history();
                    session = Session::new(&config.model, session.cwd.clone());
                    save_project_model(&config.model);
                    println!(
                        "{COLOR_DIM}model → {} (saved to .thclaws/settings.json; new session {}){COLOR_RESET}",
                        config.model, session.id
                    );
                }
                SlashCommand::Config { key, value } => {
                    println!(
                        "{COLOR_DIM}(session-only) {key} = {value}{COLOR_RESET}"
                    );
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
                    .with_approver(approver.clone());
                    agent.clear_history();
                    session = Session::new(&config.model, session.cwd.clone());
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
                        Err(e) => println!(
                            "{COLOR_YELLOW}catalogue refresh failed: {e}{COLOR_RESET}"
                        ),
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
                                        Some(dn) => println!(
                                            "{COLOR_DIM}  {} — {}{COLOR_RESET}",
                                            m.id, dn
                                        ),
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
                            Ok(path) => println!(
                                "{COLOR_DIM}saved → {}{COLOR_RESET}",
                                path.display()
                            ),
                            Err(e) => println!("{COLOR_YELLOW}save failed: {e}{COLOR_RESET}"),
                        },
                        None => println!(
                            "{COLOR_YELLOW}no session store (set $HOME){COLOR_RESET}"
                        ),
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
                        None => println!(
                            "{COLOR_YELLOW}no session store (set $HOME){COLOR_RESET}"
                        ),
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
                                    Some(t) => println!(
                                        "{COLOR_DIM}session renamed → {t}{COLOR_RESET}"
                                    ),
                                    None => println!(
                                        "{COLOR_DIM}session title cleared{COLOR_RESET}"
                                    ),
                                }
                            }
                            Err(e) => println!("{COLOR_YELLOW}rename failed: {e}{COLOR_RESET}"),
                        }
                    }
                    None => println!(
                        "{COLOR_YELLOW}no session store (set $HOME){COLOR_RESET}"
                    ),
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
                    None => println!(
                        "{COLOR_YELLOW}no session store (set $HOME){COLOR_RESET}"
                    ),
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
                                println!(
                                    "{COLOR_DIM}  {}{} — {}{COLOR_RESET}",
                                    e.name, ty, desc
                                );
                            }
                        }
                        Err(e) => println!("{COLOR_YELLOW}memory list failed: {e}{COLOR_RESET}"),
                    },
                    None => println!(
                        "{COLOR_YELLOW}no memory store (set $HOME){COLOR_RESET}"
                    ),
                },
                SlashCommand::MemoryRead(name) => {
                    if name.is_empty() {
                        println!("{COLOR_YELLOW}usage: /memory read NAME{COLOR_RESET}");
                        continue;
                    }
                    match &memory_store {
                        Some(store) => match store.get(&name) {
                            Some(entry) => {
                                println!(
                                    "{COLOR_DIM}── {} ─────{COLOR_RESET}",
                                    entry.name
                                );
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
                        None => println!(
                            "{COLOR_YELLOW}no memory store (set $HOME){COLOR_RESET}"
                        ),
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
                            let mut line =
                                format!("  MEMORY.md              {}", crate::util::format_bytes(mem_index_bytes));
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
                    println!("{COLOR_DIM}built:    {} ({}){COLOR_RESET}", v.build_time, v.build_profile);
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
                        println!("{COLOR_DIM}thinking budget: {current} tokens (0 = off){COLOR_RESET}");
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
                                println!("{COLOR_YELLOW}usage: /thinking BUDGET (integer){COLOR_RESET}");
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
                                println!(
                                    "{COLOR_DIM}    source: {}{COLOR_RESET}",
                                    p.source
                                );
                            }
                        }
                    }
                }
                SlashCommand::PluginInstall { url, user } => {
                    match crate::plugins::install(&url, user).await {
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
                                        parts.push(format!(
                                            "{} command dir(s)",
                                            m.commands.len()
                                        ));
                                    }
                                    if !m.agents.is_empty() {
                                        parts.push(format!(
                                            "{} agent dir(s)",
                                            m.agents.len()
                                        ));
                                    }
                                    if !m.mcp_servers.is_empty() {
                                        parts.push(format!(
                                            "{} MCP server(s)",
                                            m.mcp_servers.len()
                                        ));
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
                            println!(
                                "{COLOR_YELLOW}restart thClaws to activate the plugin's skills / commands / MCP servers{COLOR_RESET}"
                            );
                        }
                        Err(e) => {
                            println!("{COLOR_YELLOW}plugin install failed: {e}{COLOR_RESET}");
                        }
                    }
                }
                SlashCommand::PluginEnable { name, user } => {
                    match crate::plugins::set_enabled(&name, user, true) {
                        Ok(true) => println!(
                            "{COLOR_DIM}plugin '{name}' enabled (restart to pick up its contributions){COLOR_RESET}"
                        ),
                        Ok(false) => println!(
                            "{COLOR_YELLOW}no plugin named '{name}' in that scope{COLOR_RESET}"
                        ),
                        Err(e) => println!("{COLOR_YELLOW}enable failed: {e}{COLOR_RESET}"),
                    }
                }
                SlashCommand::PluginDisable { name, user } => {
                    match crate::plugins::set_enabled(&name, user, false) {
                        Ok(true) => println!(
                            "{COLOR_DIM}plugin '{name}' disabled (restart to drop its contributions){COLOR_RESET}"
                        ),
                        Ok(false) => println!(
                            "{COLOR_YELLOW}no plugin named '{name}' in that scope{COLOR_RESET}"
                        ),
                        Err(e) => println!("{COLOR_YELLOW}disable failed: {e}{COLOR_RESET}"),
                    }
                }
                SlashCommand::PluginShow { name } => {
                    match crate::plugins::find_installed(&name) {
                        Some(p) => {
                            let status = if p.enabled { "enabled" } else { "disabled" };
                            println!(
                                "{COLOR_DIM}  {} v{} ({}){COLOR_RESET}",
                                p.name,
                                if p.version.is_empty() { "-" } else { &p.version },
                                status
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
                SlashCommand::PluginRemove { name, user } => {
                    match crate::plugins::remove(&name, user) {
                        Ok(true) => {
                            println!(
                                "{COLOR_DIM}plugin '{name}' removed (restart to drop its contributions){COLOR_RESET}"
                            );
                        }
                        Ok(false) => {
                            println!(
                                "{COLOR_YELLOW}no plugin named '{name}' in that scope{COLOR_RESET}"
                            );
                        }
                        Err(e) => {
                            println!(
                                "{COLOR_YELLOW}plugin remove failed: {e}{COLOR_RESET}"
                            );
                        }
                    }
                }
                SlashCommand::McpAdd { name, url, user } => {
                    let scope = if user { "user" } else { "project" };
                    let cfg = crate::mcp::McpServerConfig {
                        name: name.clone(),
                        transport: "http".into(),
                        command: String::new(),
                        args: Vec::new(),
                        env: Default::default(),
                        url: url.clone(),
                        headers: Default::default(),
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
                                .with_approver(approver.clone());
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
                            println!(
                                "{COLOR_YELLOW}/fork: can't build provider: {e}{COLOR_RESET}"
                            );
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
                    println!(
                        "{COLOR_DIM}/fork: forked {old_id} → {} ({} → {} messages){COLOR_RESET}",
                        session.id,
                        history.len(),
                        summary_history.len()
                    );
                }
                SlashCommand::Doctor => {
                    println!("{COLOR_DIM}── thClaws diagnostics ──{COLOR_RESET}");
                    let v = crate::version::info();
                    println!("{COLOR_DIM}version:    {}{COLOR_RESET}", v.version);
                    println!(
                        "{COLOR_DIM}revision:   {}{} ({}){COLOR_RESET}",
                        v.git_sha,
                        if v.git_dirty { "+dirty" } else { "" },
                        v.git_branch
                    );
                    println!("{COLOR_DIM}built:      {} ({}){COLOR_RESET}", v.build_time, v.build_profile);
                    println!("{COLOR_DIM}model:      {}{COLOR_RESET}", config.model);
                    println!(
                        "{COLOR_DIM}provider:   {}{COLOR_RESET}",
                        config.detect_provider().unwrap_or("unknown")
                    );
                    println!(
                        "{COLOR_DIM}api key:    {}{COLOR_RESET}",
                        if config.api_key_from_env().is_some() { "set ✓" } else { "MISSING ✗" }
                    );
                    println!(
                        "{COLOR_DIM}config:     {}{COLOR_RESET}",
                        {
                            let paths = AppConfig::user_config_paths();
                            paths.iter()
                                .find(|p| p.exists())
                                .map(|p| format!("{} ✓", p.display()))
                                .unwrap_or_else(|| {
                                    paths.first()
                                        .map(|p| format!("{} (not found)", p.display()))
                                        .unwrap_or_else(|| "none".into())
                                })
                        }
                    );
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
                            .map(|p| if p.exists() { format!("{} ✓", p.display()) } else { format!("{} (empty)", p.display()) })
                            .unwrap_or_else(|| "none".into())
                    );
                    println!(
                        "{COLOR_DIM}tmux:       {}{COLOR_RESET}",
                        if crate::team::has_tmux() { "available ✓" } else { "not found" }
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
                        println!(
                            "{COLOR_DIM}permissions: {} (auto = never prompt, ask = prompt on mutating tools){COLOR_RESET}",
                            if agent.permission_mode == PermissionMode::Auto { "auto" } else { "ask" }
                        );
                    } else {
                        match mode.as_str() {
                            "auto" | "yolo" => {
                                agent.permission_mode = PermissionMode::Auto;
                                println!("{COLOR_DIM}permissions → auto (no prompts){COLOR_RESET}");
                            }
                            "ask" | "default" => {
                                agent.permission_mode = PermissionMode::Ask;
                                println!("{COLOR_DIM}permissions → ask{COLOR_RESET}");
                            }
                            _ => {
                                println!("{COLOR_YELLOW}usage: /permissions auto|ask{COLOR_RESET}");
                            }
                        }
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
                            if dir.starts_with(&project_prefix) { "project" }
                            else if dir.starts_with(&user_prefix) { "user" }
                            else if dir.starts_with(&claude_prefix) { "claude" }
                            else { "?" }
                        };

                        let mut rows: Vec<(&str, &str, bool)> = store
                            .skills
                            .values()
                            .map(|s| (
                                s.name.as_str(),
                                level_of(&s.dir),
                                s.dir.join("scripts").exists(),
                            ))
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
                        if dir.starts_with(&project_prefix) { "project" }
                        else if dir.starts_with(&user_prefix) { "user" }
                        else { "system" }
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
                            println!(
                                "{COLOR_DIM}level: {}{COLOR_RESET}",
                                skill_level(&skill.dir)
                            );
                            println!(
                                "{COLOR_DIM}path:  {}{COLOR_RESET}",
                                skill.dir.display()
                            );
                        }
                        None => {
                            println!(
                                "{COLOR_YELLOW}unknown skill: '{name}' — run /skills to list{COLOR_RESET}"
                            );
                        }
                    }
                }
                SlashCommand::SkillInstall { git_url, name, project } => {
                    match crate::skills::install_from_url(&git_url, name.as_deref(), project).await {
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
                SlashCommand::Team => {
                    let session = "thclaws-team";
                    if crate::team::has_tmux() {
                        let exists = std::process::Command::new("tmux")
                            .args(["has-session", "-t", session])
                            .output()
                            .map(|o| o.status.success())
                            .unwrap_or(false);
                        if exists {
                            println!("{COLOR_DIM}attaching to tmux session '{session}'...{COLOR_RESET}");
                            println!("{COLOR_DIM}(press Ctrl+B then D to detach back here){COLOR_RESET}");
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
                                    println!("{COLOR_DIM}Team agents (no tmux session):{COLOR_RESET}");
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
                    let tracker = crate::usage::UsageTracker::new(
                        crate::usage::UsageTracker::default_path(),
                    );
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
                    } else if let Err(e) =
                        ProjectConfig::set_active_kms(config.kms_active.clone())
                    {
                        println!("{COLOR_YELLOW}save failed: {e}{COLOR_RESET}");
                    } else {
                        println!(
                            "{COLOR_DIM}KMS '{name}' detached (restart chat or start a new turn to apply){COLOR_RESET}"
                        );
                    }
                }
                SlashCommand::KmsShow(name) => {
                    match crate::kms::resolve(&name) {
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
                        None => println!(
                            "{COLOR_YELLOW}no KMS named '{name}'{COLOR_RESET}"
                        ),
                    }
                }
                SlashCommand::KmsIngest { name, file, alias, force } => {
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
                            println!(
                                "{COLOR_DIM}{verb} → {} — {}{COLOR_RESET}",
                                r.target.display(),
                                r.summary,
                            );
                        }
                        Err(e) => {
                            println!("{COLOR_YELLOW}ingest failed: {e}{COLOR_RESET}");
                        }
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
            let status = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(shell_cmd)
                .status();
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
        lead_log!("\n{COLOR_CYAN}❯ {line}{COLOR_RESET}\n{COLOR_GREEN}");
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
                Ok(AgentEvent::ToolCallResult { output, .. }) => {
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

    // Kill any teammate processes spawned by this session.
    let _ = std::process::Command::new("pkill")
        .args(["-f", "team-agent"])
        .status();
    println!("{COLOR_DIM}bye{COLOR_RESET}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parse_slash_models() {
        assert_eq!(parse_slash("/models"), Some(SlashCommand::Models));
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
            Some("gemini-2.0-flash")
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
}
