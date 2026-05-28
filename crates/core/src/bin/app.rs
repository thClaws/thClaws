//! `thclaws` — unified binary: desktop GUI by default, CLI via --cli.
//!
//! Default: opens desktop GUI window.
//! `--cli`: interactive REPL in the terminal (same as thclaws-cli).
//! `--print`: non-interactive single-prompt mode (implies --cli).

use clap::{Parser, Subcommand};
use thclaws_core::config::AppConfig;
use thclaws_core::dotenv::load_dotenv;
use thclaws_core::repl::{run_print_mode, run_repl};
use thclaws_core::sandbox::Sandbox;
use thclaws_core::{endpoints, schedule, secrets};

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
    /// Subcommands. When omitted, the legacy flag-based CLI runs
    /// (GUI default / `--cli` REPL / `--print` / `--serve`).
    #[command(subcommand)]
    command: Option<Command>,

    /// Run in CLI mode (interactive REPL) instead of GUI
    #[arg(long)]
    cli: bool,

    /// Non-interactive: run prompt and exit (implies --cli)
    #[arg(short, long)]
    print: bool,

    /// Override model for this run only — applies to CLI, GUI, and --serve.
    /// One-shot, in-memory. Pair with --set-model to persist instead.
    #[arg(short, long)]
    model: Option<String>,

    /// Persist a model to `.thclaws/settings.json` as the project
    /// default, then use it for this run. Unlike --model (one-shot),
    /// subsequent invocations without --model will pick up this value.
    /// Refuses to overwrite an unreadable settings file to avoid
    /// clobbering sibling fields (maxTokens, allowedTools, etc.).
    #[arg(long, value_name = "MODEL")]
    set_model: Option<String>,

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

    /// M6.36: serve the React frontend over HTTP + WebSocket so the
    /// project is reachable from a browser. Single-user; binds to
    /// 127.0.0.1 by default — use an SSH tunnel for remote access.
    /// `--bind 0.0.0.0` exposes the server publicly (only with auth
    /// in front: e.g. Tailscale, Cloudflare Access, reverse proxy
    /// with basic auth). One project per process; cd into the project
    /// dir before running. Compose with `--gui` to also open the
    /// desktop window on the same engine; mutually exclusive with
    /// --cli / --print.
    #[arg(long)]
    serve: bool,

    /// Port for `--serve` mode. Default 8443.
    #[arg(long, default_value_t = 8443)]
    port: u16,

    /// Bind address for `--serve` mode. Default 127.0.0.1 (localhost).
    /// Set to `0.0.0.0` to bind all interfaces — only safe behind
    /// auth (Tailscale, reverse proxy, etc.).
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    /// Open the desktop GUI window. GUI is the implicit default when no
    /// other surface flag is set, so this flag's main use is composing
    /// with `--serve` (`--serve --gui`): the desktop window and any
    /// browser tab attach to the same Agent + Session — same
    /// conversation, two surfaces.
    #[arg(long)]
    gui: bool,

    /// Disable the in-process scheduler. Schedules stay in the store
    /// but won't auto-fire while this process runs — use external
    /// cron / launchd or `thclaws schedule run <id>` instead. Has no
    /// effect on `--print` and the `schedule` subcommand, neither of
    /// which spawn the scheduler in the first place.
    #[arg(long)]
    no_scheduler: bool,

    /// Run the Telegram bot headless (no GUI window). Reads the bot token
    /// from TELEGRAM_BOT_TOKEN or ~/.config/thclaws/telegram.json; set
    /// TELEGRAM_OWNER_ID=<your id> for instant DM access. The agent runs
    /// locally; Telegram is just the chat surface. dev-plan/29.
    #[arg(long)]
    telegram: bool,

    /// Run the Facebook Page Messenger bridge headless (no GUI window).
    /// Connects to the relay using the binding JWT in
    /// ~/.config/thclaws/messenger.json (pair via the GUI first). The
    /// agent runs locally; Messenger is just the chat surface.
    /// dev-plan/31.
    #[arg(long)]
    messenger: bool,

    /// Prompt (positional args joined with spaces)
    prompt: Vec<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Manage scheduled jobs.
    #[command(subcommand)]
    Schedule(ScheduleCmd),
    /// Run the scheduler daemon in the foreground. Normally invoked
    /// by launchd / systemd via `thclaws schedule install`. Run it
    /// manually to test schedules without installing the supervisor
    /// (Ctrl-C to stop).
    Daemon,
    /// Deploy the current project's `.thclaws/` (skills, MCP, plugins,
    /// KMS, AGENTS.md, settings.json) to a running `thclaws --serve`
    /// pod. Sessions / memory / team-runtime on the pod side are
    /// preserved across deploys. See dev-plan/28 for the contract.
    Deploy {
        /// Pod base URL (e.g. https://co-test.thcompany.ai). Required.
        #[arg(long)]
        pod: String,
        /// Bearer token for the pod's /v1/* API. Falls back to
        /// $THCLAWS_DEPLOY_TOKEN if unset.
        #[arg(long)]
        token: Option<String>,
        /// Include `.thclaws/memory/` in the upload (private agent
        /// notes — opt-in).
        #[arg(long)]
        include_memory: bool,
        /// Don't reject stdio MCP entries. They'll fail to start on
        /// the pod side; useful only for iterating on the cloud config.
        #[arg(long)]
        allow_stdio_mcp: bool,
        /// Print what would upload (file list + bytes) without sending.
        #[arg(long)]
        dry_run: bool,
        /// Skip the diff handshake and always upload the full bundle.
        /// Default is to query /v1/deploy/manifest first and only ship
        /// changed files (Phase 2 from dev-plan/28).
        #[arg(long)]
        full: bool,
        /// Skip the auto-restart after a successful deploy. By
        /// default the client POSTs /v1/restart so the pod
        /// re-initialises MCP servers, plugin runtimes, skill caches,
        /// and the system prompt. Pass --no-restart to keep the
        /// running --serve process up across the deploy (rare: hot
        /// config edits the snapshot doesn't read).
        #[arg(long = "no-restart")]
        no_restart: bool,
    },
    /// Manage the Telegram adapter (dev-plan/29). `status` prints the
    /// resolved config; `pair` prints setup instructions. Connecting a
    /// bot is done from the GUI Telegram Connect modal (or auto-loads on
    /// launch when `~/.config/thclaws/telegram.json` is present + enabled).
    Telegram {
        #[command(subcommand)]
        cmd: TelegramCmd,
    },
    /// Manage the Facebook Page Messenger adapter (dev-plan/31).
    /// `status` prints the resolved binding config; `pair` prints setup
    /// instructions. Connecting a Page is done from the GUI Messenger
    /// Connect modal.
    Messenger {
        #[command(subcommand)]
        cmd: MessengerCmd,
    },
}

#[derive(Subcommand)]
enum TelegramCmd {
    /// Print the resolved Telegram config: whether a token is present
    /// (redacted), DM/group policy, allowlist size, output ceiling.
    Status,
    /// Print step-by-step instructions for creating a bot with
    /// @BotFather and connecting it.
    Pair,
}

#[derive(Subcommand)]
enum MessengerCmd {
    /// Print the resolved Messenger binding config: relay URL, whether
    /// a binding token is present (redacted), cached Page name/id.
    Status,
    /// Print step-by-step instructions for connecting a Facebook Page.
    Pair,
}

#[derive(Subcommand)]
enum ScheduleCmd {
    /// Add a new schedule. Errors if the id already exists.
    Add {
        /// Stable id for the schedule (used as the lookup key and log dir name).
        id: String,
        /// Standard 5-field POSIX cron expression for a recurring job
        /// (e.g. "30 8 * * MON-FRI"). Mutually exclusive with --at/--in.
        #[arg(long)]
        cron: Option<String>,
        /// One-shot: fire once at this absolute RFC 3339 time
        /// (e.g. "2026-05-24T15:30:00Z"), then auto-disable. Mutually
        /// exclusive with --cron and --in.
        #[arg(long, conflicts_with_all = ["cron", "in_delay"])]
        at: Option<String>,
        /// One-shot: fire once after this relative delay (e.g. 15m, 2h,
        /// 90s, 1d), then auto-disable. Mutually exclusive with --cron
        /// and --at.
        #[arg(long = "in", conflicts_with_all = ["cron", "at"])]
        in_delay: Option<String>,
        /// Prompt text to feed `thclaws --print` when this schedule fires.
        #[arg(long)]
        prompt: String,
        /// Working directory for the spawned job. Defaults to the current
        /// working directory at add time.
        #[arg(long)]
        cwd: Option<String>,
        /// Override model alias for this job (defaults to whatever the
        /// cwd's `.thclaws/settings.json` picks).
        #[arg(long)]
        model: Option<String>,
        /// Per-job iteration cap.
        #[arg(long)]
        max_iterations: Option<usize>,
        /// Per-job timeout in seconds. Default 600 (10 min). Pass 0 for no timeout.
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Add as disabled. Edit `~/.config/thclaws/schedules.json` (set
        /// `"enabled": true`) to turn it on later.
        #[arg(long)]
        disabled: bool,
        /// Also fire when any file in the schedule's working directory
        /// changes (debounced ~2s). Daemon-only — the in-process
        /// scheduler ignores this flag.
        #[arg(long)]
        watch: bool,
    },
    /// List all schedules.
    List,
    /// Print one schedule's full record as JSON.
    Show { id: String },
    /// Remove a schedule (does not delete its log directory).
    Rm { id: String },
    /// Fire a schedule once, synchronously. Captures stdout+stderr to
    /// `~/.local/share/thclaws/logs/<id>/<ts>.log` and returns the
    /// child's exit code as this process's exit code.
    Run { id: String },
    /// Install the scheduler daemon as a user-level supervised
    /// service (launchd plist on macOS, systemd-user unit on Linux).
    /// On macOS this also bootstraps the agent so the daemon starts
    /// immediately and on every login.
    Install,
    /// Stop and remove the daemon's supervisor entry. Schedules in
    /// the store are preserved.
    Uninstall,
    /// Print scheduler daemon status (running / stale / not running)
    /// and a brief recent-fires summary across all schedules.
    Status,
}

/// Hide the console allocated for the Windows console-subsystem binary when
/// the user is launching the GUI. CLI mode keeps the console attached so
/// `thclaws --cli` can read keys normally from PowerShell/CMD.
#[cfg(windows)]
fn detach_console_for_gui() {
    use windows_sys::Win32::System::Console::FreeConsole;

    // SAFETY: `FreeConsole` detaches this process from its console and has no
    // Rust-side invariants. Failure only means there was no console to detach.
    unsafe {
        FreeConsole();
    }
}

#[cfg(not(windows))]
fn detach_console_for_gui() {}

/// Windows-only: when about to launch the GUI from a console (cmd.exe /
/// PowerShell), respawn ourselves as a detached child and exit the parent
/// so the shell prompt returns immediately. Issue #109.
///
/// Background: `thclaws.exe` is built as a **console-subsystem** binary
/// (PR #60 / issue #48) so that `--cli`'s rustyline gets working stdio.
/// The side effect is that cmd.exe / PowerShell `WaitForSingleObject` on
/// every console-subsystem child until exit — `notepad.exe` returns the
/// prompt instantly only because it's a windows-subsystem binary, and
/// `FreeConsole()` in the child doesn't change cmd's wait. Result: typing
/// `thclaws.exe` from a shell blocks the prompt until the GUI window closes.
///
/// Workaround: at the GUI dispatch entry, respawn `current_exe()` with
/// `THCLAWS_GUI_DETACHED=1` and `DETACHED_PROCESS`, then `exit(0)`. The
/// child sees the env var, skips the respawn, runs the GUI in-process,
/// and survives parent / terminal closure because `DETACHED_PROCESS`
/// breaks the parent process group. The parent exits in microseconds,
/// so cmd's wait returns and the next prompt appears.
///
/// Called before the in-process scheduler and `/v1` loopback bind so
/// neither runs in the doomed parent (avoiding a port-bind race on
/// 18443). No-op on macOS / Linux — terminals there don't block on
/// GUI children.
#[cfg(all(windows, feature = "gui"))]
fn respawn_detached_for_gui_if_needed(cli: &Cli) {
    // Skip in the detached child itself.
    if std::env::var_os("THCLAWS_GUI_DETACHED").is_some() {
        return;
    }
    // Only respawn when the dispatch is actually GUI: not --cli/--print/
    // --telegram/--messenger, and either plain GUI (no --serve) or the
    // --serve --gui combo.
    let use_cli = cli.cli || cli.print || cli.telegram || cli.messenger;
    let is_gui_dispatch = !use_cli && (!cli.serve || cli.gui);
    if !is_gui_dispatch {
        return;
    }

    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS (0x00000008) — child has no console, no process-
    // group ties to the parent shell.
    const DETACHED_PROCESS: u32 = 0x00000008;

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let spawn = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("THCLAWS_GUI_DETACHED", "1")
        .creation_flags(DETACHED_PROCESS)
        .spawn();
    if spawn.is_ok() {
        std::process::exit(0);
    }
    // Spawn failed (antivirus quarantine, ENOMEM, etc.): fall through
    // and run the GUI in-process. User loses the prompt-return but
    // keeps a working app.
}

#[cfg(not(all(windows, feature = "gui")))]
fn respawn_detached_for_gui_if_needed(_cli: &Cli) {}

#[tokio::main]
async fn main() {
    secrets::load_into_env();
    endpoints::load_into_env();
    load_dotenv();
    let _ = Sandbox::init();

    // M6.45 / #79-followup: warn if there are additional thclaws
    // copies elsewhere on PATH. On Windows pairs with the MSI's
    // Part="first" PATH addition (which makes the new install win
    // PATH-search regardless of older entries) — this surfaces the
    // duplicates so the user can clean them up. On macOS/Linux,
    // catches version mismatch (e.g. /usr/local/bin/thclaws +
    // /opt/homebrew/bin/thclaws after a brew migration). Not gated
    // on any mode (CLI / GUI / --serve / --print).
    warn_about_stale_binaries();

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

    // Subcommand short-circuit. `thclaws schedule …` and
    // `thclaws daemon` don't need the bootstrap, don't open a
    // session, and shouldn't fall through to GUI/CLI/serve
    // dispatch — handle them here and exit.
    match cli.command {
        Some(Command::Schedule(sub)) => {
            let code = run_schedule_subcommand(sub);
            std::process::exit(code);
        }
        Some(Command::Daemon) => {
            // The daemon spawns its own scheduler — ensure the
            // app.rs auto-spawn block below does NOT also spawn one
            // (would mean two schedulers running against the same
            // store). The `cli.command.is_some()` check below
            // handles that.
            match schedule::run_daemon().await {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!("\x1b[31m[daemon] {e}\x1b[0m");
                    std::process::exit(1);
                }
            }
        }
        Some(Command::Deploy {
            pod,
            token,
            include_memory,
            allow_stdio_mcp,
            dry_run,
            full,
            no_restart,
        }) => {
            let code = thclaws_core::deploy_client::run(thclaws_core::deploy_client::DeployArgs {
                pod,
                token,
                include_memory,
                allow_stdio_mcp,
                dry_run,
                full,
                restart: !no_restart,
            })
            .await;
            std::process::exit(code);
        }
        Some(Command::Telegram { cmd }) => {
            let code = run_telegram_subcommand(cmd);
            std::process::exit(code);
        }
        Some(Command::Messenger { cmd }) => {
            let code = run_messenger_subcommand(cmd);
            std::process::exit(code);
        }
        None => {}
    }

    let use_cli = cli.cli || cli.print || cli.telegram || cli.messenger;

    // Issue #109: on Windows, respawn detached so cmd.exe / PowerShell
    // return the prompt instead of waiting on the GUI window. Runs
    // before the scheduler + /v1 loopback so they don't bind ports in
    // the doomed parent. See `respawn_detached_for_gui_if_needed`.
    respawn_detached_for_gui_if_needed(&cli);

    // First-run bootstrap: drop a `.thclaws/settings.json` with model +
    // permissions defaults into the project so users get a working
    // config the first time they `cd` in. Skipped if a config already
    // exists or if a Claude Code `.claude/settings.json` is present.
    thclaws_core::config::ProjectConfig::ensure_default_exists();

    // Wire up `--set-model` / `--model` before any AppConfig::load runs.
    // `--set-model` persists to `.thclaws/settings.json` (refusing to
    // overwrite an unreadable file so we don't clobber sibling settings)
    // and also takes effect this run; `--model` is in-memory only. Both
    // route through `set_cli_model_override`, which `AppConfig::load`
    // applies last — so every surface (CLI, GUI, --serve) sees the same
    // model without each path re-implementing the override step.
    if let Some(ref m) = cli.set_model {
        let resolved = thclaws_core::providers::ProviderKind::resolve_alias(m);
        match thclaws_core::config::persist_model_to_project_settings(&resolved) {
            Ok(path) => eprintln!(
                "\x1b[32m--set-model: persisted model={resolved} to {}\x1b[0m",
                path.display()
            ),
            Err(e) => {
                eprintln!("\x1b[31m--set-model: {e}\x1b[0m");
                std::process::exit(1);
            }
        }
        thclaws_core::config::set_cli_model_override(resolved);
    } else if let Some(ref m) = cli.model {
        let resolved = thclaws_core::providers::ProviderKind::resolve_alias(m);
        thclaws_core::config::set_cli_model_override(resolved);
    }

    // In-process scheduler (Step 2): spawn a background tokio task
    // that polls `~/.config/thclaws/schedules.json` every 30s and
    // fires due jobs as `thclaws --print` subprocesses. Skipped for
    // `--print` (short-lived, would add subprocess noise to a 5s
    // run) and when the user passes `--no-scheduler`. The task
    // ends when the process exits.
    if !cli.print && !cli.no_scheduler {
        match std::env::current_exe() {
            Ok(binary) => {
                schedule::spawn_scheduler_task(binary);
            }
            Err(e) => {
                eprintln!("\x1b[33m[schedule] could not resolve current_exe: {e} — scheduler disabled\x1b[0m");
            }
        }
    }

    // Always-on loopback `/v1/*` listener for out-of-process MCP-Apps
    // servers that need to reach the user's authenticated LLM provider
    // (e.g. thclaws-gamedev-mcp's HTTP-transport server forwarding game
    // AI moves). Binds 127.0.0.1:18443 by default (override with
    // $THCLAWS_LOOPBACK_PORT) with `THCLAWS_API_TOKEN=disable-auth` so
    // the out-of-process server doesn't need to discover a per-launch
    // token. Skipped under `--print` (short-lived runs don't host MCP
    // widgets) and `--serve` (that path already mounts /v1 on the
    // user's chosen bind; a parallel loopback would double-bind on
    // operators who pick 18443 for serve, and serve users know their
    // own URL already). Bind failures are logged + ignored — MCP-Apps
    // widgets that don't need the bridge keep working without it.
    if !cli.print && !cli.serve {
        if let Err(e) = thclaws_core::api_v1::spawn_loopback().await {
            eprintln!(
                "\x1b[33m[api_v1] loopback listener failed to bind: {e} — out-of-process MCP-Apps tools relying on the /v1 bridge (e.g. GamedevAiMove) won't be reachable; set THCLAWS_LOOPBACK_PORT to pick a free port\x1b[0m"
            );
        }
    }

    // M6.36 SERVE5: --serve mode short-circuits the CLI/GUI dispatch.
    // Single-purpose deployment shape — operator runs one process per
    // project on a server. Gated behind `gui` because crate::server
    // transitively depends on crate::shared_session (also gui-gated)
    // — they share the same WorkerState engine. The CLI-only
    // thclaws-cli binary doesn't ship --serve.
    //
    // `--serve --gui` is the combo path: same process owns the desktop
    // window and the HTTP/WS listener, both attached to one engine.
    if cli.serve {
        #[cfg(feature = "gui")]
        {
            let bind_ip: std::net::IpAddr = match cli.bind.parse() {
                Ok(ip) => ip,
                Err(e) => {
                    eprintln!("\x1b[31m--bind: invalid IP '{}': {e}\x1b[0m", cli.bind);
                    std::process::exit(1);
                }
            };
            let serve_config = thclaws_core::server::ServeConfig {
                bind: std::net::SocketAddr::new(bind_ip, cli.port),
                ..Default::default()
            };
            if cli.gui {
                if use_cli {
                    eprintln!("\x1b[31m--gui is incompatible with --cli/--print\x1b[0m");
                    std::process::exit(1);
                }
                detach_console_for_gui();
                thclaws_core::gui::run_gui_with_serve(serve_config);
                return;
            }
            if let Err(e) = thclaws_core::server::run(serve_config).await {
                eprintln!("\n\x1b[31mserve error: {e}\x1b[0m");
                std::process::exit(1);
            }
            return;
        }
        #[cfg(not(feature = "gui"))]
        {
            eprintln!(
                "\x1b[31m--serve not available — rebuild with: cargo build --features gui --bin thclaws\x1b[0m"
            );
            std::process::exit(1);
        }
    }

    if !use_cli {
        #[cfg(feature = "gui")]
        {
            detach_console_for_gui();
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

    // CLI overrides. `--model` / `--set-model` already routed through
    // `set_cli_model_override` above, so `AppConfig::load` has applied
    // them. The rest of these flags are CLI/REPL-only knobs that the
    // GUI and --serve don't honor today.
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

    if cli.telegram {
        // Headless Telegram bot — its own agent loop (the GUI worker is
        // gui-gated). Runs until Ctrl-C. dev-plan/29 Tier 1.
        if let Err(e) = thclaws_core::telegram::headless::run(config).await {
            eprintln!("\n\x1b[31m[telegram] error: {e}\x1b[0m");
            std::process::exit(1);
        }
    } else if cli.messenger {
        // Headless Facebook Page Messenger bridge — its own agent loop.
        // Runs until Ctrl-C. dev-plan/31 Tier 1.
        if let Err(e) = thclaws_core::messenger::headless::run(config).await {
            eprintln!("\n\x1b[31m[messenger] error: {e}\x1b[0m");
            std::process::exit(1);
        }
    } else if cli.print {
        let prompt = cli.prompt.join(" ");
        if prompt.is_empty() {
            eprintln!("\x1b[31m--print requires a prompt argument\x1b[0m");
            std::process::exit(1);
        }
        if let Err(e) = run_print_mode(config, &prompt, cli.verbose).await {
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

/// Dispatch table for `thclaws schedule …`. Returns the exit code the
/// process should report. `run` returns the child's exit code (or 124
/// on timeout, mirroring GNU `timeout(1)`); the management subcommands
/// return 0 on success and 1 on user error.
fn run_telegram_subcommand(cmd: TelegramCmd) -> i32 {
    use thclaws_core::telegram::{config::redact_token, TelegramConfig};
    match cmd {
        TelegramCmd::Status => {
            let cfg = match TelegramConfig::load() {
                Ok(Some(c)) => c,
                Ok(None) => TelegramConfig::default(),
                Err(e) => {
                    eprintln!("\x1b[31m[telegram] failed to read config: {e}\x1b[0m");
                    return 1;
                }
            };
            let token = cfg.resolved_token();
            println!("Telegram adapter status");
            println!("  enabled:        {}", cfg.enabled);
            match token {
                Some(t) => println!("  bot token:      {} (present)", redact_token(&t)),
                None => println!(
                    "  bot token:      <none> (set TELEGRAM_BOT_TOKEN or connect via the GUI)"
                ),
            }
            println!("  dm policy:      {:?}", cfg.dm_policy);
            println!("  group policy:   {:?}", cfg.group_policy);
            println!("  allow_from:     {} user(s)", cfg.allow_from.len());
            println!("  groups:         {} allowlisted", cfg.groups.len());
            println!("  output ceiling: {} chars", cfg.output_ceiling);
            0
        }
        TelegramCmd::Pair => {
            println!(
                "Connect a Telegram bot to thClaws\n\
                 \n\
                 1. In Telegram, message @BotFather and send /newbot. Follow the\n\
                    prompts to pick a name and username; it replies with a bot token\n\
                    that looks like 123456789:AA…\n\
                 2. Either:\n\
                    • paste the token into the GUI \u{2192} Settings \u{2192} Telegram Connect, or\n\
                    • export TELEGRAM_BOT_TOKEN=<token> and relaunch thClaws.\n\
                 3. DM your bot. The first message mints a 6-digit pairing code;\n\
                    approve it in the Telegram Connect modal. You're then chatting\n\
                    with thClaws over Telegram.\n\
                 \n\
                 Run `thclaws telegram status` to confirm the token is detected."
            );
            0
        }
    }
}

/// `thclaws messenger …` — print binding status or setup help.
fn run_messenger_subcommand(cmd: MessengerCmd) -> i32 {
    use thclaws_core::messenger::MessengerConfig;
    match cmd {
        MessengerCmd::Status => {
            let cfg = match MessengerConfig::load() {
                Ok(Some(c)) => c,
                Ok(None) => MessengerConfig::default(),
                Err(e) => {
                    eprintln!("\x1b[31m[messenger] failed to read config: {e}\x1b[0m");
                    return 1;
                }
            };
            println!("Messenger adapter status");
            println!("  relay:          {}", cfg.resolved_server_url());
            if cfg.binding_token.trim().is_empty() {
                println!("  binding token:  <none> (pair a Page via the GUI Messenger Connect)");
            } else {
                let shown = cfg.binding_token.chars().take(6).collect::<String>();
                println!("  binding token:  {shown}… (present)");
            }
            println!(
                "  page:           {}",
                cfg.page_name.as_deref().unwrap_or("<unknown>")
            );
            println!(
                "  page id:        {}",
                cfg.page_id.as_deref().unwrap_or("<unknown>")
            );
            0
        }
        MessengerCmd::Pair => {
            println!(
                "Connect a Facebook Page to thClaws (Messenger)\n\
                 \n\
                 1. In Meta for Developers, create an app (type: Business) and add the\n\
                    Messenger product. Generate a Page Access Token for your Page and\n\
                    note your App Secret.\n\
                 2. Configure the relay (operator step) with MESSENGER_PAGE_ACCESS_TOKEN,\n\
                    MESSENGER_APP_SECRET, MESSENGER_VERIFY_TOKEN, MESSENGER_PAGE_ID, and\n\
                    point the app's webhook at https://<relay>/messenger/webhook\n\
                    subscribed to the `messages` field.\n\
                 3. Message your Page. The relay DMs a pairing code; paste it into the\n\
                    GUI \u{2192} Messenger Connect modal to bind this machine.\n\
                 4. Run `thclaws --messenger` (or connect from the GUI) to start the\n\
                    bridge. You're then chatting with thClaws from the Page inbox.\n\
                 \n\
                 Note: messaging users beyond your app's admins/testers needs Meta App\n\
                 Review + Business Verification for the pages_messaging permission.\n\
                 \n\
                 Run `thclaws messenger status` to confirm the binding is detected."
            );
            0
        }
    }
}

fn run_schedule_subcommand(cmd: ScheduleCmd) -> i32 {
    match cmd {
        ScheduleCmd::Add {
            id,
            cron,
            at,
            in_delay,
            prompt,
            cwd,
            model,
            max_iterations,
            timeout,
            disabled,
            watch,
        } => {
            // Resolve the trigger: one-shot (--at absolute / --in
            // relative) vs recurring (--cron). clap already enforces
            // mutual exclusion; here we require exactly one and turn
            // --in into an absolute run_at.
            let run_at = match (at, in_delay) {
                (Some(ts), _) => match schedule::parse_run_at(&ts) {
                    Ok(dt) => Some(dt.to_rfc3339()),
                    Err(e) => {
                        eprintln!("\x1b[31merror: {e}\x1b[0m");
                        return 1;
                    }
                },
                (_, Some(dur)) => match schedule::parse_relative_duration(&dur) {
                    Ok(d) => Some((chrono::Utc::now() + d).to_rfc3339()),
                    Err(e) => {
                        eprintln!("\x1b[31merror: {e}\x1b[0m");
                        return 1;
                    }
                },
                (None, None) => None,
            };
            if run_at.is_none() && cron.is_none() {
                eprintln!(
                    "\x1b[31merror: a schedule needs a trigger — pass --cron \
                     for recurring, or --at/--in for a one-shot\x1b[0m"
                );
                return 1;
            }
            let cwd_path = match cwd {
                Some(p) => std::path::PathBuf::from(p),
                None => match std::env::current_dir() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("\x1b[31merror: cannot read current dir: {e}\x1b[0m");
                        return 1;
                    }
                },
            };
            if !cwd_path.exists() {
                eprintln!(
                    "\x1b[31merror: cwd does not exist: {}\x1b[0m",
                    cwd_path.display()
                );
                return 1;
            }
            let mut store = match schedule::ScheduleStore::load() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("\x1b[31merror: load schedule store: {e}\x1b[0m");
                    return 1;
                }
            };
            let entry = schedule::Schedule {
                id: id.clone(),
                cron: cron.unwrap_or_default(),
                run_at,
                cwd: cwd_path,
                prompt,
                model,
                max_iterations,
                timeout_secs: if timeout == 0 { None } else { Some(timeout) },
                enabled: !disabled,
                watch_workspace: watch,
                last_run: None,
                last_exit: None,
            };
            if let Err(e) = store.add(entry) {
                eprintln!("\x1b[31merror: {e}\x1b[0m");
                return 1;
            }
            if let Err(e) = store.save() {
                eprintln!("\x1b[31merror: save schedule store: {e}\x1b[0m");
                return 1;
            }
            println!("added schedule '{id}'");
            0
        }
        ScheduleCmd::List => {
            let store = match schedule::ScheduleStore::load() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("\x1b[31merror: load schedule store: {e}\x1b[0m");
                    return 1;
                }
            };
            if store.schedules.is_empty() {
                println!(
                    "no schedules — `thclaws schedule add <id> --cron \"...\" --prompt \"...\"`"
                );
                return 0;
            }
            // Compact list: id, cron, enabled flag, watchWorkspace
            // indicator, last-run timestamp (or "never"), and cwd.
            // One line per schedule.
            for s in &store.schedules {
                let status = if s.enabled { "on " } else { "off" };
                let watch = if s.watch_workspace {
                    "+watch"
                } else {
                    "      "
                };
                let last = s.last_run.as_deref().unwrap_or("never");
                let exit = match s.last_exit {
                    Some(0) => " ok ",
                    Some(_) => " err",
                    None => "    ",
                };
                // Trigger column: cron expression for recurring jobs,
                // or "once@<run_at> (pending|fired)" for one-shots.
                let trigger = match &s.run_at {
                    Some(run_at) => {
                        let state = if s.last_run.is_some() {
                            "fired"
                        } else {
                            "pending"
                        };
                        format!("once@{run_at} ({state})")
                    }
                    None => s.cron.clone(),
                };
                println!(
                    "{status} {exit} {watch}  {:24}  {:30}  {}  {}",
                    s.id,
                    trigger,
                    last,
                    s.cwd.display()
                );
            }
            0
        }
        ScheduleCmd::Show { id } => {
            let store = match schedule::ScheduleStore::load() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("\x1b[31merror: load schedule store: {e}\x1b[0m");
                    return 1;
                }
            };
            match store.get(&id) {
                Some(s) => match serde_json::to_string_pretty(s) {
                    Ok(json) => {
                        println!("{json}");
                        0
                    }
                    Err(e) => {
                        eprintln!("\x1b[31merror: serialize: {e}\x1b[0m");
                        1
                    }
                },
                None => {
                    eprintln!("\x1b[31merror: no schedule with id '{id}'\x1b[0m");
                    1
                }
            }
        }
        ScheduleCmd::Rm { id } => {
            let mut store = match schedule::ScheduleStore::load() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("\x1b[31merror: load schedule store: {e}\x1b[0m");
                    return 1;
                }
            };
            if !store.remove(&id) {
                eprintln!("\x1b[31merror: no schedule with id '{id}'\x1b[0m");
                return 1;
            }
            if let Err(e) = store.save() {
                eprintln!("\x1b[31merror: save schedule store: {e}\x1b[0m");
                return 1;
            }
            println!("removed schedule '{id}'");
            0
        }
        ScheduleCmd::Install => match schedule::install_daemon() {
            Ok(report) => {
                println!("wrote {}", report.supervisor_path.display());
                if report.next_steps.is_empty() {
                    println!("daemon bootstrapped — `thclaws schedule status` to verify");
                } else {
                    println!("\nnext steps:");
                    for step in &report.next_steps {
                        println!("  $ {step}");
                    }
                }
                0
            }
            Err(e) => {
                eprintln!("\x1b[31merror: {e}\x1b[0m");
                1
            }
        },
        ScheduleCmd::Uninstall => match schedule::uninstall_daemon() {
            Ok(path) => {
                if path.exists() {
                    println!(
                        "warning: supervisor file at {} still exists",
                        path.display()
                    );
                    1
                } else {
                    println!("daemon uninstalled");
                    0
                }
            }
            Err(e) => {
                eprintln!("\x1b[31merror: {e}\x1b[0m");
                1
            }
        },
        ScheduleCmd::Status => {
            let status = schedule::daemon_status();
            match status {
                schedule::DaemonStatus::Running(pid) => {
                    println!("daemon: \x1b[32mrunning\x1b[0m (pid {pid})");
                }
                schedule::DaemonStatus::Stale(pid) => {
                    println!(
                        "daemon: \x1b[33mstale PID file\x1b[0m (last pid {pid} not alive — \
                         supervisor will reclaim on next start)"
                    );
                }
                schedule::DaemonStatus::NotRunning => {
                    println!(
                        "daemon: \x1b[33mnot running\x1b[0m \
                         (`thclaws schedule install` to enable)"
                    );
                }
            }
            // Compact recent-fires summary so the user can see
            // whether jobs are firing without `tail`-ing each log.
            match schedule::ScheduleStore::load() {
                Ok(store) if !store.schedules.is_empty() => {
                    println!("\nrecent fires:");
                    for s in &store.schedules {
                        let last = s.last_run.as_deref().unwrap_or("never");
                        let exit = match s.last_exit {
                            Some(0) => "ok ",
                            Some(_) => "err",
                            None => "—  ",
                        };
                        println!("  {exit}  {:24}  {}", s.id, last);
                    }
                }
                _ => {}
            }
            0
        }
        ScheduleCmd::Run { id } => {
            // Use the *currently running* binary as the spawn target so
            // the scheduled job runs against the same thclaws build that
            // registered it. `current_exe` follows symlinks on macOS so
            // a homebrew-installed thclaws still resolves correctly.
            let binary = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("\x1b[31merror: cannot resolve current_exe: {e}\x1b[0m");
                    return 1;
                }
            };
            match schedule::run_once(&id, &binary) {
                Ok(outcome) => {
                    eprintln!(
                        "\x1b[36m[schedule] '{id}' ran in {}.{:03}s, log: {}\x1b[0m",
                        outcome.duration.as_secs(),
                        outcome.duration.subsec_millis(),
                        outcome.log_path.display(),
                    );
                    if outcome.timed_out {
                        eprintln!("\x1b[33m[schedule] '{id}' timed out\x1b[0m");
                        return 124;
                    }
                    outcome.exit_code.unwrap_or(1)
                }
                Err(e) => {
                    eprintln!("\x1b[31merror: {e}\x1b[0m");
                    1
                }
            }
        }
    }
}

/// M6.45 / #79-followup: scan PATH for additional thclaws copies
/// and warn the user. Cross-platform: Windows looks for `thclaws.exe`,
/// Mac/Linux for `thclaws`; PATH is split via `std::env::split_paths`
/// which handles `;` (Windows) vs `:` (Unix) correctly.
///
/// On Windows the MSI's `Part="first"` PATH addition guarantees the
/// new install wins PATH-search — this function is informational,
/// nudging the user to clean up stale copies (e.g. the manual
/// `C:\tools\thclaws.exe` from before the installer existed).
///
/// On macOS / Linux there's no installer-side PATH manipulation so
/// PATH order is whatever the user set — the warning catches version
/// mismatch when multiple manual / brew installs coexist (e.g.
/// `/usr/local/bin/thclaws` + `/opt/homebrew/bin/thclaws`).
fn warn_about_stale_binaries() {
    #[cfg(windows)]
    const BIN_NAME: &str = "thclaws.exe";
    #[cfg(not(windows))]
    const BIN_NAME: &str = "thclaws";
    #[cfg(windows)]
    const RM_HINT: &str = "del \"<path-above>\"";
    #[cfg(not(windows))]
    const RM_HINT: &str = "rm <path-above>";

    let Ok(current_exe) = std::env::current_exe() else {
        return;
    };
    let current_canon = std::fs::canonicalize(&current_exe).ok();
    let Some(path_var) = std::env::var_os("PATH") else {
        return;
    };

    let mut duplicates: Vec<std::path::PathBuf> = Vec::new();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(BIN_NAME);
        if !candidate.is_file() {
            continue;
        }
        let canon = match std::fs::canonicalize(&candidate) {
            Ok(p) => p,
            Err(_) => continue,
        };
        // Skip if same file as we're running (covers symlinks too —
        // a symlink in /usr/local/bin pointing at the .app bundle
        // binary canonicalizes to the same path as current_exe).
        if let Some(curr) = &current_canon {
            if &canon == curr {
                continue;
            }
        }
        if !duplicates.iter().any(|p| p == &canon) {
            duplicates.push(canon);
        }
    }
    if duplicates.is_empty() {
        return;
    }
    eprintln!(
        "\x1b[33m[thclaws] warning: {} additional {} install(s) found on PATH:\x1b[0m",
        duplicates.len(),
        BIN_NAME
    );
    eprintln!("  running:  {}", current_exe.display());
    for d in &duplicates {
        eprintln!("  also at:  {}", d.display());
    }
    eprintln!(
        "\x1b[33m[thclaws] only the first one on PATH is invoked when you type `thclaws`. The other copies still take ~17 MB each.\nTo clean up:  {}\x1b[0m",
        RM_HINT
    );
}
