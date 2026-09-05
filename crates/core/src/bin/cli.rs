//! `thclaws-cli` — lightweight CLI-only agent (no GUI dependencies).

use clap::Parser;
use thclaws_core::config::AppConfig;
use thclaws_core::dotenv::load_dotenv;
use thclaws_core::repl::run_repl;
use thclaws_core::sandbox::Sandbox;
use thclaws_core::{endpoints, secrets};

#[derive(Parser)]
#[command(
    name = "thclaws-cli",
    version = env!("CARGO_PKG_VERSION"),
    long_version = concat!(
        env!("CARGO_PKG_VERSION"), "\n",
        "revision: ", env!("THCLAWS_GIT_SHA"),
            " (", env!("THCLAWS_GIT_BRANCH"), ")\n",
        "built:    ", env!("THCLAWS_BUILD_TIME"),
            " (", env!("THCLAWS_BUILD_PROFILE"), ")"
    ),
    about = "thClaws AI agent workspace (CLI only)"
)]
struct Cli {
    /// Non-interactive: run prompt and exit
    #[arg(short, long)]
    print: bool,

    /// Override model (e.g. claude-sonnet-4-5, gpt-4o, ollama/llama3.2)
    #[arg(short, long)]
    model: Option<String>,

    /// Never ask for tool-call approval (alias: --dangerously-skip-permissions)
    #[arg(long, alias = "dangerously-skip-permissions")]
    accept_all: bool,

    /// Permission mode: auto, ask (default: from config)
    #[arg(long)]
    permission_mode: Option<String>,

    /// Override system prompt
    #[arg(long)]
    system_prompt: Option<String>,

    /// Show per-turn token usage + timing on stderr (only takes effect with -p / --print)
    #[arg(long, short = 'v')]
    verbose: bool,

    /// Resume a previous session by ID (or "last" for most recent)
    #[arg(long, alias = "continue")]
    resume: Option<String>,

    /// Print mode: don't persist the session
    #[arg(long)]
    no_session: bool,

    /// Output format: text (default), stream-json
    #[arg(long, default_value = "text")]
    output_format: String,

    /// Input format: text (default), stream-json
    #[arg(long, default_value = "text")]
    input_format: String,

    /// Comma-separated list of allowed tool names
    #[arg(long)]
    allowed_tools: Option<String>,

    /// Comma-separated list of disallowed tool names
    #[arg(long)]
    disallowed_tools: Option<String>,

    /// Max agent loop iterations per turn (0 = unlimited, default 200)
    #[arg(long)]
    max_iterations: Option<usize>,

    /// Run as a team agent (receives work via filesystem mailbox)
    #[arg(long)]
    team_agent: Option<String>,

    /// Team directory (default: .thclaws/team)
    #[arg(long)]
    team_dir: Option<String>,

    /// Prompt (positional args joined with spaces)
    prompt: Vec<String>,
}

#[tokio::main]
async fn main() {
    // dev-plan/49: handle the internal `__confine` re-exec (Linux Landlock).
    thclaws_core::confine::maybe_handle_confine_subcommand();
    secrets::load_into_env();
    endpoints::load_into_env();
    load_dotenv();
    // Same appliance detection as the `thclaws` binary — someone on the DGX
    // box running the CLI should reach the local gateway too.
    thclaws_core::aiserver::bootstrap().await;
    let _ = Sandbox::init();

    // Org policy file enforcement (Enterprise Edition foundation).
    // Same gate as `thclaws` — a fail-closed refusal exits non-zero
    // before any further startup work happens.
    if let Err(e) = thclaws_core::policy::load_or_refuse() {
        eprintln!("\x1b[31m{}\x1b[0m", e.refuse_message());
        std::process::exit(2);
    }
    thclaws_core::audit::init();

    let cli = Cli::parse();
    // Workspace layout migration (v1 flat → v2 `state/`) before any
    // config load resolves project paths. No-op once migrated.
    thclaws_core::config::ProjectConfig::migrate_workspace_if_needed();
    let mut config = match AppConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[31mconfig error: {e}\x1b[0m");
            std::process::exit(1);
        }
    };

    // CLI overrides.
    if let Some(m) = cli.model {
        config.model = thclaws_core::providers::ProviderKind::resolve_alias(&m);
    }
    if cli.accept_all {
        config.permissions = "auto".to_string();
    }
    if let Some(ref mode) = cli.permission_mode {
        config.permissions = mode.clone();
    }
    if let Some(ref sp) = cli.system_prompt {
        config.system_prompt = sp.clone();
    }
    if let Some(ref tools) = cli.allowed_tools {
        config.allowed_tools = Some(tools.split(',').map(|s| s.trim().to_string()).collect());
    }
    if let Some(ref tools) = cli.disallowed_tools {
        config.disallowed_tools = Some(tools.split(',').map(|s| s.trim().to_string()).collect());
    }
    if let Some(ref session_id) = cli.resume {
        config.resume_session = Some(session_id.clone());
    }
    if let Some(n) = cli.max_iterations {
        config.max_iterations = n;
    }

    // Team agent mode.
    if let Some(ref agent_name) = cli.team_agent {
        let team_dir = cli.team_dir.as_deref().unwrap_or(".thclaws/state/team");
        std::env::set_var("THCLAWS_TEAM_AGENT", agent_name);
        std::env::set_var("THCLAWS_TEAM_DIR", team_dir);
    }

    if cli.print {
        let prompt = cli.prompt.join(" ");
        if prompt.is_empty() {
            eprintln!("\x1b[31m--print requires a prompt argument\x1b[0m");
            std::process::exit(1);
        }
        if let Err(e) =
            thclaws_core::repl::run_print_mode_with(config, &prompt, cli.verbose, !cli.no_session)
                .await
        {
            eprintln!("\n\x1b[31merror: {e}\x1b[0m");
            std::process::exit(1);
        }
    } else {
        if let Err(e) = run_repl(config).await {
            eprintln!("\n\x1b[31merror: {e}\x1b[0m");
            std::process::exit(1);
        }
    }
    thclaws_core::audit::shutdown();
}
