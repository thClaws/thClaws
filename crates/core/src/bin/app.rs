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
}

#[derive(Subcommand)]
enum ScheduleCmd {
    /// Add a new schedule. Errors if the id already exists.
    Add {
        /// Stable id for the schedule (used as the lookup key and log dir name).
        id: String,
        /// Standard 5-field POSIX cron expression (e.g. "30 8 * * MON-FRI").
        #[arg(long)]
        cron: String,
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
        None => {}
    }

    let use_cli = cli.cli || cli.print;

    // First-run bootstrap: drop a `.thclaws/settings.json` with model +
    // permissions defaults into the project so users get a working
    // config the first time they `cd` in. Skipped if a config already
    // exists or if a Claude Code `.claude/settings.json` is present.
    thclaws_core::config::ProjectConfig::ensure_default_exists();

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
fn run_schedule_subcommand(cmd: ScheduleCmd) -> i32 {
    match cmd {
        ScheduleCmd::Add {
            id,
            cron,
            prompt,
            cwd,
            model,
            max_iterations,
            timeout,
            disabled,
            watch,
        } => {
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
                cron,
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
                println!(
                    "{status} {exit} {watch}  {:24}  {:20}  {}  {}",
                    s.id,
                    s.cron,
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
