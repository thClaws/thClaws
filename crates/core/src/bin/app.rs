//! `thclaws` — unified binary: desktop GUI by default, CLI via --cli.
//!
//! Default: opens desktop GUI window.
//! `--cli`: interactive REPL in the terminal (same as thclaws-cli).
//! `--print`: non-interactive single-prompt mode (implies --cli).

//! To create a Rust GUI application that runs without spawning a command-line (terminal) window,
//! add this attribute to the very top of your main.rs file:
//!
//! #![windows_subsystem = "windows"]
//!
//! macOS and Linux?
//! Unlike Windows, macOS and Linux do not automatically spawn a terminal for every executable.
#![windows_subsystem = "windows"]

use clap::Parser;
use thclaws_core::config::AppConfig;
use thclaws_core::dotenv::load_dotenv;
use thclaws_core::repl::{run_print_mode, run_repl};
use thclaws_core::sandbox::Sandbox;
use thclaws_core::{endpoints, secrets};

#[derive(Parser)]
#[command(
    name = "thclaws",
    version = env!("CARGO_PKG_VERSION"),
    long_version = concat!(
        env!("CARGO_PKG_VERSION"), "\n",
        "revision: ", env!("THCLAWS_GIT_SHA"),
            " (", env!("THCLAWS_GIT_BRANCH"), ")\n",
        "built:    ", env!("THCLAWS_BUILD_TIME"),
            " (", env!("THCLAWS_BUILD_PROFILE"), ")"
    ),
    about = "thClaws AI agent workspace (GUI + CLI)"
)]
struct Cli {
    /// Run in CLI mode (interactive REPL) instead of GUI
    #[arg(long)]
    cli: bool,

    /// Non-interactive: run prompt and exit (implies --cli)
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

    /// Show verbose output (token counts, timing)
    #[arg(long)]
    verbose: bool,

    /// Resume a previous session by ID (or "last" for most recent)
    #[arg(long, alias = "continue")]
    resume: Option<String>,

    /// Output format: text (default), stream-json
    #[arg(long, default_value = "text")]
    output_format: String,

    /// Comma-separated list of allowed tool names
    #[arg(long)]
    allowed_tools: Option<String>,

    /// Comma-separated list of disallowed tool names
    #[arg(long)]
    disallowed_tools: Option<String>,

    /// Max agent loop iterations per turn (0 = unlimited, default 200)
    #[arg(long)]
    max_iterations: Option<usize>,

    /// Run as a team agent
    #[arg(long)]
    team_agent: Option<String>,

    /// Team directory
    #[arg(long)]
    team_dir: Option<String>,

    /// Prompt (positional args joined with spaces)
    prompt: Vec<String>,
}

/// Re-attach stdout/stderr to the parent terminal on Windows so
/// `--version`, `--help`, and any print-and-exit flag actually emit
/// their output to the shell that launched us. Without this, the
/// `windows_subsystem = "windows"` attribute (declared at the top of
/// this file to prevent a console flicker on File Explorer double-click)
/// detaches stdio entirely and clap's writes go nowhere. Issue #48.
///
/// `AttachConsole(ATTACH_PARENT_PROCESS)` is the same pattern used by
/// `winget`, `gh.exe`, modern Tauri apps, etc.: harmless when there's
/// no parent console (returns 0 / GetLastError = ERROR_INVALID_HANDLE
/// for double-click launches), and gives us a working stdio when one
/// exists. No-op on macOS / Linux.
#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: `AttachConsole` is a Win32 entry point with no Rust
    // invariants; calling it before any stdio I/O is the documented
    // contract. We deliberately ignore the return value — failure
    // means "no parent console available" (e.g. double-click launch),
    // which is the same state we'd be in if we hadn't called it at all.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
fn attach_parent_console() {}

#[tokio::main]
async fn main() {
    attach_parent_console();
    secrets::load_into_env();
    endpoints::load_into_env();
    load_dotenv();
    let _ = Sandbox::init();

    // Org policy file enforcement (Enterprise Edition foundation).
    // Runs before CLI parse so a fail-closed refusal happens identically
    // whether the user invoked GUI, CLI, or print mode. Open-core builds
    // with no policy file and no key are unaffected — `load_or_refuse`
    // returns Ok(false).
    if let Err(e) = thclaws_core::policy::load_or_refuse() {
        eprintln!("\x1b[31m{}\x1b[0m", e.refuse_message());
        std::process::exit(2);
    }

    let cli = Cli::parse();
    let use_cli = cli.cli || cli.print;

    if !use_cli {
        #[cfg(feature = "gui")]
        {
            thclaws_core::gui::run_gui();
            return;
        }
        #[cfg(not(feature = "gui"))]
        {
            eprintln!("\x1b[31mGUI not available — rebuild with: cargo build --features gui --bin thclaws\x1b[0m");
            eprintln!("\x1b[31mOr use --cli for terminal mode.\x1b[0m");
            std::process::exit(1);
        }
    }

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
    if let Some(ref agent_name) = cli.team_agent {
        let team_dir = cli.team_dir.as_deref().unwrap_or(".thclaws/team");
        std::env::set_var("THCLAWS_TEAM_AGENT", agent_name);
        std::env::set_var("THCLAWS_TEAM_DIR", team_dir);
    }

    if cli.print {
        let prompt = cli.prompt.join(" ");
        if prompt.is_empty() {
            eprintln!("\x1b[31m--print requires a prompt argument\x1b[0m");
            std::process::exit(1);
        }
        if let Err(e) = run_print_mode(config, &prompt).await {
            eprintln!("\n\x1b[31merror: {e}\x1b[0m");
            std::process::exit(1);
        }
    } else {
        if let Err(e) = run_repl(config).await {
            eprintln!("\n\x1b[31merror: {e}\x1b[0m");
            std::process::exit(1);
        }
    }
}
