//! Spawn, watch, restart and reap one process per bot.
//!
//! Each child is an ordinary `thclaws --serve` with cwd set to the bot's
//! folder, so the engine needs no change to run as a bot: process cwd IS the
//! bot's identity, config, sandbox root and state dir.

use super::{bot_dir, BotDef};
use crate::error::{Error, Result};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;

const POLL: Duration = Duration::from_millis(50);
/// Consecutive-failure backoff. A child that dies instantly and forever
/// should not spin a core.
const BACKOFF: [Duration; 5] = [
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];
/// How many bots a host will run at once. Not a resource limit so much as a
/// guard against a bots.json someone pasted: each child is a full engine, and
/// a user who genuinely wants more should be met by an idle policy that tears
/// children down, not by a silent 30-process fan-out.
pub const MAX_LIVE_BOTS: usize = 8;
const STDERR_TAIL_LINES: usize = 20;
const STDERR_LINE_MAX: usize = 300;
const TOKEN_LEN: usize = 32;
/// 32 symbols, so a byte maps onto it without modulo bias.
const TOKEN_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";

/// When to give up on a child. Held by the supervisor rather than read from
/// constants so the policy itself is testable at speed — the production
/// values are [`RestartPolicy::default`].
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    pub backoff: &'static [Duration],
    /// More starts than this inside `window` and the bot is parked instead of
    /// restarted forever: a bot broken by its own config (a bad
    /// `settings.json`, a missing binary in `mcp.json`) never fixes itself by
    /// being run again, and the failure should be visible, not absorbed.
    pub max_starts: usize,
    pub window: Duration,
    /// A cold `--serve` start loads config, sandbox, MCP servers and skills,
    /// so this is generous. The supervisor is not blocked while it waits.
    pub ready_timeout: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            backoff: &BACKOFF,
            max_starts: 5,
            window: Duration::from_secs(60),
            ready_timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BotState {
    Starting,
    Ready {
        addr: SocketAddr,
    },
    Restarting {
        after_ms: u64,
        reason: String,
    },
    /// Parked after too many starts in too short a window. Needs an explicit
    /// restart — the supervisor will not try again on its own.
    CrashLooped {
        reason: String,
    },
    Stopped,
}

pub struct Bot {
    pub slug: String,
    pub dir: PathBuf,
    /// The workspace whose files every agent shares (dev-plan/61).
    workspace_root: PathBuf,
    /// Where the child publishes the address it actually bound. Lives under
    /// the HOST's `state/`, not in the bot's tree: it is the supervisor's
    /// runtime bookkeeping and is stripped from any publish.
    addr_file: PathBuf,
    /// Re-minted on every spawn, so a token that leaks dies with the process
    /// that used it.
    token: StdMutex<String>,
    /// One stop signal per bot, so a single bot can be removed without
    /// taking the others down. The supervisor's own shutdown fans out to
    /// these rather than keeping a second channel that both paths must
    /// remember to watch.
    stop_tx: watch::Sender<bool>,
    _stop_rx: watch::Receiver<bool>,
    state_tx: watch::Sender<BotState>,
    /// Held for the bot's lifetime. `watch::Sender::send` fails — and throws
    /// the value away — once every receiver has been dropped, so without this
    /// a bot nobody happened to be watching kept reporting whatever it was
    /// doing when the last watcher left. The host reports state long after a
    /// browser has come and gone, so it must never depend on one.
    _state_rx: watch::Receiver<BotState>,
    stderr_tail: StdMutex<VecDeque<String>>,
}

impl Bot {
    pub fn state(&self) -> BotState {
        self.state_tx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<BotState> {
        self.state_tx.subscribe()
    }

    pub fn token(&self) -> String {
        self.token.lock().expect("bot token").clone()
    }

    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail
            .lock()
            .expect("bot stderr tail")
            .iter()
            .cloned()
            .collect()
    }

    fn set_state(&self, next: BotState) {
        let _ = self.state_tx.send(next);
    }

    /// Stop this bot and leave it stopped.
    pub fn stop(&self) {
        let _ = self.stop_tx.send(true);
    }

    /// Block until the bot has actually stopped, or give up.
    pub async fn wait_stopped(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut rx = self.subscribe();
        loop {
            if matches!(
                *rx.borrow_and_update(),
                BotState::Stopped | BotState::CrashLooped { .. }
            ) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, rx.changed()).await.is_err() {
                return false;
            }
        }
    }

    /// Block until the bot is serving, and hand back what it takes to reach
    /// it. Errors rather than hangs when the bot is parked or stopped, so a
    /// browser connecting to a crash-looped bot gets told, not left waiting.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<(SocketAddr, String)> {
        let deadline = Instant::now() + timeout;
        let mut rx = self.subscribe();
        loop {
            let decision = match &*rx.borrow_and_update() {
                BotState::Ready { addr } => Some(Ok(*addr)),
                BotState::CrashLooped { reason } => Some(Err(format!(
                    "agent '{}' is not running: {reason}",
                    self.slug
                ))),
                BotState::Stopped => Some(Err(format!("agent '{}' has been stopped", self.slug))),
                _ => None,
            };
            match decision {
                Some(Ok(addr)) => return Ok((addr, self.token())),
                Some(Err(e)) => return Err(Error::Tool(e)),
                None => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, rx.changed()).await.is_err() {
                return Err(Error::Tool(format!(
                    "agent '{}' was not ready within {}s (state: {:?})",
                    self.slug,
                    timeout.as_secs(),
                    self.state()
                )));
            }
        }
    }
}

pub struct BotSupervisor {
    workspace: PathBuf,
    /// The program spawned per child — `current_exe()` in production, a stub
    /// in tests. Resolved once at construction rather than per spawn so a
    /// binary replaced under a running host (`make install`) cannot silently
    /// change what a restart launches.
    program: PathBuf,
    bots: StdMutex<HashMap<String, Arc<Bot>>>,
    /// Slugs in the order they were started, which is `bots.json` order. The
    /// rail reads this: sorting by slug would put a newly installed `alpha`
    /// above `main` and move the workspace's own agent under the user.
    order: StdMutex<Vec<String>>,
    /// The bot a browser reaches when it has not asked for one. Set from the
    /// FIRST entry in `bots.json`, not from `list()` — that sorts by slug, so
    /// installing `alpha` would silently steal the default from `main`.
    default_slug: StdMutex<Option<String>>,
    policy: RestartPolicy,
}

impl BotSupervisor {
    pub fn workspace(&self) -> &std::path::Path {
        &self.workspace
    }

    pub fn new(workspace: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let program = std::env::current_exe()
            .map_err(|e| Error::Tool(format!("cannot resolve this executable: {e}")))?;
        Ok(Self::with_program(workspace, program))
    }

    pub fn with_program(workspace: impl Into<PathBuf>, program: impl Into<PathBuf>) -> Arc<Self> {
        Self::with_policy(workspace, program, RestartPolicy::default())
    }

    pub fn with_policy(
        workspace: impl Into<PathBuf>,
        program: impl Into<PathBuf>,
        policy: RestartPolicy,
    ) -> Arc<Self> {
        Arc::new(Self {
            workspace: workspace.into(),
            program: program.into(),
            bots: StdMutex::new(HashMap::new()),
            order: StdMutex::new(Vec::new()),
            default_slug: StdMutex::new(None),
            policy,
        })
    }

    pub fn get(&self, slug: &str) -> Option<Arc<Bot>> {
        self.bots.lock().expect("bots map").get(slug).cloned()
    }

    /// The bot a browser talks to when it has not named one.
    pub fn focused(&self) -> Option<Arc<Bot>> {
        let want = self.default_slug.lock().expect("default slug").clone();
        want.and_then(|s| self.get(&s))
            .or_else(|| self.list().into_iter().next())
    }

    pub fn set_default(&self, slug: &str) {
        *self.default_slug.lock().expect("default slug") = Some(slug.to_string());
    }

    /// Bots in `bots.json` order — what the rail shows, and what makes the
    /// first entry the default.
    pub fn list(&self) -> Vec<Arc<Bot>> {
        let map = self.bots.lock().expect("bots map");
        let order = self.order.lock().expect("bots order");
        let mut v: Vec<_> = order.iter().filter_map(|s| map.get(s).cloned()).collect();
        // Anything started outside the configured order still shows, last.
        let mut extra: Vec<_> = map
            .values()
            .filter(|b| !order.contains(&b.slug))
            .cloned()
            .collect();
        extra.sort_by(|a, b| a.slug.cmp(&b.slug));
        v.extend(extra);
        v
    }

    /// Register a bot and start supervising it. Returns as soon as the
    /// supervision task is running — use [`Bot::wait_ready`] to wait for the
    /// child to serve.
    pub fn start(&self, def: &BotDef) -> Result<Arc<Bot>> {
        super::validate_slug(&def.slug)?;
        if let Some(existing) = self.get(&def.slug) {
            return Ok(existing);
        }
        let dir = bot_dir(&self.workspace, &def.slug);
        if !dir.is_dir() {
            return Err(Error::Config(format!(
                "agent '{}' has no folder at {}",
                def.slug,
                dir.display()
            )));
        }
        let addr_file = self
            .workspace
            .join(".thclaws/state/bots")
            .join(format!("{}.addr", def.slug));
        let (state_tx, _state_rx) = watch::channel(BotState::Starting);
        let (stop_tx, _stop_rx) = watch::channel(false);
        let bot = Arc::new(Bot {
            slug: def.slug.clone(),
            dir,
            workspace_root: self.workspace.clone(),
            addr_file,
            token: StdMutex::new(String::new()),
            stop_tx,
            _stop_rx,
            state_tx,
            _state_rx,
            stderr_tail: StdMutex::new(VecDeque::new()),
        });
        self.bots
            .lock()
            .expect("bots map")
            .insert(def.slug.clone(), bot.clone());
        {
            let mut order = self.order.lock().expect("bots order");
            if !order.contains(&def.slug) {
                order.push(def.slug.clone());
            }
        }
        let stop = bot.stop_tx.subscribe();
        tokio::spawn(supervise(
            bot.clone(),
            self.program.clone(),
            self.policy.clone(),
            stop,
        ));
        Ok(bot)
    }

    /// Ask every child to stop. `kill_on_drop` is the backstop for a host
    /// that dies without getting here; a supervised child also exits on
    /// stdin EOF, which covers a host killed outright.
    pub fn shutdown(&self) {
        for bot in self.list() {
            bot.stop();
        }
    }

    /// dev-plan/59 Step 5: install a catalogue agent as a bot and start it.
    /// Registration happens inside the installer and only on a successful
    /// extraction, so this never starts supervising an empty folder.
    pub async fn add_bot(
        self: &Arc<Self>,
        slug: &str,
        version: Option<&str>,
        force: bool,
    ) -> Result<super::install::Installed> {
        // The cap that `run_supervisor_on` applies at startup applies here
        // too, and BEFORE the download: a bot installed past it would be
        // listed but never started, which the rail would show as nothing.
        if self.get(slug).is_none() && self.list().len() >= MAX_LIVE_BOTS {
            return Err(Error::Config(format!(
                "this workspace is already running {MAX_LIVE_BOTS} agents — remove one before \
                 adding '{slug}'"
            )));
        }
        let installed = super::install::install(&self.workspace, slug, version, force).await?;
        // An update to a bot already running needs the old process gone before
        // the new definition is loaded — the files under it have just changed.
        if let Some(existing) = self.get(slug) {
            existing.stop();
            existing.wait_stopped(Duration::from_secs(10)).await;
            self.bots.lock().expect("bots map").remove(slug);
        }
        self.start(&BotDef {
            slug: slug.to_string(),
            name: None,
        })?;
        Ok(installed)
    }

    /// Add a bot with no agent — an empty folder, started like a new one.
    /// Same cap as an install, checked before anything is written.
    pub async fn add_blank_bot(self: &Arc<Self>, slug: &str) -> Result<super::install::Installed> {
        if self.get(slug).is_some() {
            return Err(Error::Config(format!(
                "an agent named '{slug}' already exists in this workspace — pick another name"
            )));
        }
        if self.list().len() >= MAX_LIVE_BOTS {
            return Err(Error::Config(format!(
                "this workspace is already running {MAX_LIVE_BOTS} agents — remove one before \
                 adding '{slug}'"
            )));
        }
        let created = super::install::create_blank(&self.workspace, slug)?;
        self.start(&BotDef {
            slug: slug.to_string(),
            name: None,
        })?;
        Ok(created)
    }

    /// Stop a bot and start it again — the way out of `crash_looped`, and
    /// what "restart" in the host panel means. Its place in the rail is kept.
    pub async fn restart_bot(&self, slug: &str) -> Result<Arc<Bot>> {
        let Some(bot) = self.get(slug) else {
            return Err(Error::Config(format!(
                "no agent '{slug}' is running in this workspace"
            )));
        };
        bot.stop();
        bot.wait_stopped(Duration::from_secs(10)).await;
        self.bots.lock().expect("bots map").remove(slug);
        self.start(&BotDef {
            slug: slug.to_string(),
            name: None,
        })
    }

    /// Stop a bot and delist it. The folder survives unless `purge` — it holds
    /// that bot's sessions, KMS and browser logins.
    pub async fn remove_bot(&self, slug: &str, purge: bool) -> Result<()> {
        // Ask whether it CAN be removed before stopping it. The first cut
        // stopped the child and then let `deregister` refuse, which left a
        // running host holding a bot that would never come back.
        if !super::install::can_deregister(&self.workspace, slug)? {
            return Err(Error::Config(format!(
                "no agent '{slug}' in this workspace"
            )));
        }
        if let Some(bot) = self.get(slug) {
            bot.stop();
            bot.wait_stopped(Duration::from_secs(10)).await;
        }
        super::install::deregister(&self.workspace, slug, purge)?;
        self.bots.lock().expect("bots map").remove(slug);
        self.order.lock().expect("bots order").retain(|s| s != slug);
        Ok(())
    }
}

impl Drop for BotSupervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One task per bot: spawn, wait for readiness, wait for exit, decide.
async fn supervise(
    bot: Arc<Bot>,
    program: PathBuf,
    policy: RestartPolicy,
    mut shutdown: watch::Receiver<bool>,
) {
    // `shutdown` is this bot's own stop signal; the supervisor fans its
    // shutdown out to every bot's.
    let mut starts: VecDeque<Instant> = VecDeque::new();
    let mut consecutive_failures = 0usize;

    loop {
        let now = Instant::now();
        while starts
            .front()
            .is_some_and(|t| now.duration_since(*t) > policy.window)
        {
            starts.pop_front();
        }
        if starts.len() >= policy.max_starts {
            let tail = bot.stderr_tail();
            let reason = format!(
                "started {} times in {}s and kept failing{}",
                policy.max_starts,
                policy.window.as_secs(),
                if tail.is_empty() {
                    String::new()
                } else {
                    format!("; last output: {}", tail.join(" / "))
                }
            );
            eprintln!("\x1b[31m[bots] {} parked: {reason}\x1b[0m", bot.slug);
            bot.set_state(BotState::CrashLooped { reason });
            return;
        }
        starts.push_back(now);

        bot.set_state(BotState::Starting);
        let Spawned {
            mut child,
            _stdin,
            stderr_done,
        } = match spawn_child(&bot, &program) {
            Ok(c) => c,
            Err(e) => {
                if !backoff(
                    &bot,
                    &policy,
                    &mut consecutive_failures,
                    &e.to_string(),
                    &mut shutdown,
                )
                .await
                {
                    return;
                }
                continue;
            }
        };

        // Readiness races the child's own death: a bot that dies during
        // startup must be noticed now, not after the ready timeout.
        let ready = tokio::select! {
            r = await_ready(&bot, policy.ready_timeout) => r,
            status = child.wait() => Err(match status {
                Ok(s) => format!("exited during startup ({s})"),
                Err(e) => format!("could not be waited on: {e}"),
            }),
            _ = shutdown.changed() => {
                reap(&bot, &mut child).await;
                return;
            }
        };
        match ready {
            Ok(addr) => {
                consecutive_failures = 0;
                eprintln!("\x1b[36m[bots] {} ready on {addr}\x1b[0m", bot.slug);
                bot.set_state(BotState::Ready { addr });
            }
            Err(reason) => {
                let _ = child.kill().await;
                drain(stderr_done).await;
                if !backoff(
                    &bot,
                    &policy,
                    &mut consecutive_failures,
                    &reason,
                    &mut shutdown,
                )
                .await
                {
                    return;
                }
                continue;
            }
        }

        tokio::select! {
            status = child.wait() => {
                let reason = match status {
                    Ok(s) => format!("exited ({s})"),
                    Err(e) => format!("could not be waited on: {e}"),
                };
                drain(stderr_done).await;
                if !backoff(&bot, &policy, &mut consecutive_failures, &reason, &mut shutdown).await {
                    return;
                }
            }
            _ = shutdown.changed() => {
                reap(&bot, &mut child).await;
                return;
            }
        }
    }
}

/// Sleep the backoff for this failure count. `false` means the host is
/// shutting down and the loop should stop instead of restarting.
async fn backoff(
    bot: &Bot,
    policy: &RestartPolicy,
    failures: &mut usize,
    reason: &str,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    let delay = policy.backoff[(*failures).min(policy.backoff.len() - 1)];
    *failures += 1;
    eprintln!(
        "\x1b[33m[bots] {} {reason} — restarting in {}ms\x1b[0m",
        bot.slug,
        delay.as_millis()
    );
    bot.set_state(BotState::Restarting {
        after_ms: delay.as_millis() as u64,
        reason: reason.to_string(),
    });
    tokio::select! {
        _ = tokio::time::sleep(delay) => true,
        _ = shutdown.changed() => {
            bot.set_state(BotState::Stopped);
            false
        }
    }
}

/// Let a dead child's stderr pump finish before its tail is quoted as the
/// reason it died — the same short join `McpClient::spawn` does, and for the
/// same reason: the explanation reaches the pipe after the exit status does.
async fn drain(stderr_done: Option<tokio::task::JoinHandle<()>>) {
    if let Some(h) = stderr_done {
        let _ = tokio::time::timeout(Duration::from_millis(250), h).await;
    }
}

async fn reap(bot: &Bot, child: &mut Child) {
    let _ = child.kill().await;
    bot.set_state(BotState::Stopped);
}

struct Spawned {
    child: Child,
    /// Held for as long as the child runs. `Child::wait` drops its own stdin
    /// handle before waiting, and a supervised child exits on stdin EOF — so
    /// leaving the handle in the `Child` made every bot exit the instant the
    /// supervisor waited on it. Found by running it, not by reading it.
    _stdin: Option<tokio::process::ChildStdin>,
    /// So a caller that finds the child dead can wait for its last lines
    /// before quoting them.
    stderr_done: Option<tokio::task::JoinHandle<()>>,
}

fn spawn_child(bot: &Arc<Bot>, program: &Path) -> Result<Spawned> {
    // A stale address from the previous run would be read as this run's, so
    // it goes before the child can write a new one.
    let _ = std::fs::remove_file(&bot.addr_file);
    if let Some(parent) = bot.addr_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // #207/#208: an agent keeps the user's HOME. Its own folder is its cwd,
    // which already keeps its settings, sessions and memory apart. HOME is
    // where the user-level things live that must NOT split per agent: the
    // global AGENTS.md, secrets.json and .env, and on macOS the login keychain,
    // which the OS finds through $HOME. v0.126.0–v0.128.0 pointed HOME at
    // `<agent>/.home`, and every one of those went missing inside an agent.
    warn_about_stranded_home(bot);
    let token = mint_token();
    *bot.token.lock().expect("bot token") = token.clone();

    let mut cmd = Command::new(program);
    cmd.arg("--serve")
        .arg("--bind")
        .arg("127.0.0.1")
        // Port 0: the host asking the OS for a port, dropping the listener
        // and handing the number to the child leaves a window where anything
        // on the machine can take it. The child binds, then publishes what it
        // got.
        .arg("--port")
        .arg("0")
        .current_dir(&bot.dir)
        .env("THCLAWS_SERVE_TOKEN", &token)
        .env("THCLAWS_SERVE_ADDR_FILE", &bot.addr_file)
        .env("THCLAWS_SUPERVISED", "1")
        // dev-plan/61: an agent's files are the workspace's. Its own folder
        // stays its cwd, for its settings, sessions and memory.
        .env("THCLAWS_WORKSPACE_ROOT", &bot.workspace_root)
        // Held open by this process: the child exits when it reads EOF, so a
        // host that dies without reaping does not leave orphans behind.
        .stdin(std::process::Stdio::piped())
        // Drained below. Piping without draining would wedge the child once
        // the pipe filled.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Tool(format!("spawn {}: {e}", program.display())))?;
    if let Some(out) = child.stdout.take() {
        pump(bot.clone(), out, false);
    }
    let stderr_done = child.stderr.take().map(|err| pump(bot.clone(), err, true));
    Ok(Spawned {
        _stdin: child.stdin.take(),
        stderr_done,
        child,
    })
}

/// Relay a child stream to the host's own stderr, prefixed, and keep the last
/// lines of stderr. When a child dies before it ever serves, that tail is the
/// only account of why — the same reason `McpClient::spawn` captures it.
fn pump<R>(bot: Arc<Bot>, reader: R, is_stderr: bool) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            eprintln!("\x1b[2m[bot:{}]\x1b[0m {line}", bot.slug);
            if is_stderr {
                let mut tail = bot.stderr_tail.lock().expect("bot stderr tail");
                if tail.len() == STDERR_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(clip(line));
            }
        }
    })
}

fn clip(line: &str) -> String {
    if line.chars().count() <= STDERR_LINE_MAX {
        return line.to_string();
    }
    let head: String = line.chars().take(STDERR_LINE_MAX).collect();
    format!("{head}…")
}

/// Copy the user's secrets-BACKEND choice into a bot's fresh `$HOME`.
///
/// `~/.config/thclaws/secrets.json` holds `{"backend": …}` and no keys — it
/// records which store the user picked. A bot with its own HOME starts
/// without it and asks again, so every newly installed bot opened with a
/// "Where should thClaws store API keys?" modal the user had already
/// answered for this machine. Copied, never merged, and only when the bot
/// has none: a bot that has been told something else keeps its answer.
/// An agent run by v0.126.0–v0.128.0 had its own HOME, and anything saved from
/// its Settings (an `.env`, a secrets backend choice) landed there. That file is
/// no longer read, so say so once, where the host logs.
fn warn_about_stranded_home(bot: &Bot) {
    let cfg = bot.dir.join(".home/.config/thclaws");
    let stranded: Vec<&str> = [".env", "secrets.json", "AGENTS.md"]
        .into_iter()
        .filter(|f| cfg.join(f).is_file())
        .collect();
    if stranded.is_empty() {
        return;
    }
    static WARNED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let warned = WARNED.get_or_init(Default::default);
    if !warned.lock().expect("warned set").insert(bot.slug.clone()) {
        return;
    }
    eprintln!(
        "\x1b[33m[bots] agent '{}' has {} from an earlier version in {} — it is no longer read. \
         Settings now use ~/.config/thclaws/ for every agent; move anything you still need there.\x1b[0m",
        bot.slug,
        stranded.join(", "),
        cfg.display()
    );
}

pub fn mint_token() -> String {
    let mut bytes = [0u8; TOKEN_LEN];
    if let Err(e) = getrandom::getrandom(&mut bytes) {
        // A predictable per-child token on loopback is bad but not worth
        // refusing to start a bot over; say so loudly instead.
        eprintln!(
            "\x1b[31m[bots] WARNING: getrandom failed ({e}); child token is predictable\x1b[0m"
        );
    }
    bytes
        .iter()
        .map(|b| TOKEN_ALPHABET[*b as usize % TOKEN_ALPHABET.len()] as char)
        .collect()
}

fn health_client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            // A machine-wide HTTP_PROXY must not intercept a loopback probe.
            .no_proxy()
            .build()
            .unwrap_or_default()
    })
}

async fn await_ready(bot: &Bot, timeout: Duration) -> std::result::Result<SocketAddr, String> {
    let deadline = Instant::now() + timeout;
    let addr = loop {
        if let Ok(raw) = std::fs::read_to_string(&bot.addr_file) {
            if let Ok(addr) = raw.trim().parse::<SocketAddr>() {
                break addr;
            }
        }
        if Instant::now() >= deadline {
            return Err("never published the address it bound".into());
        }
        tokio::time::sleep(POLL).await;
    };
    // `/healthz` is the one route the per-child bearer leaves open, which is
    // what makes it usable as the supervisor's probe.
    let url = format!("http://{addr}/healthz");
    loop {
        if let Ok(r) = health_client().get(&url).send().await {
            if r.status().is_success() {
                return Ok(addr);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("bound {addr} but never answered /healthz"));
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Resolves when the supervisor's end of stdin closes — a host that died
/// without reaping. The serving loop uses it as its shutdown signal, so the
/// bot leaves gracefully and its own children are reaped with it; the first
/// cut called `process::exit` here, which skips every destructor and orphaned
/// the bot's MCP servers.
pub async fn stdin_closed() {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    // A blocking read on a dedicated thread rather than async stdin: this
    // parks for the life of the process doing nothing, and it keeps the
    // watchdog independent of whether the runtime is healthy.
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 64];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        // Not `eprintln!`: this is the host's pipe, and by now the host may
        // already be gone — a failed print here is what aborted the agent (#210).
        crate::util::log_line("\x1b[33m[serve] supervisor closed stdin — shutting down\x1b[0m");
        let _ = tx.send(());
    });
    let _ = rx.await;
}

#[cfg(test)]
impl BotSupervisor {
    pub(crate) fn add_ready_for_test(&self, slug: &str, dir: PathBuf, addr: SocketAddr) {
        let mut bot = Bot::fake_ready(slug, addr, "test-token");
        Arc::get_mut(&mut bot).unwrap().dir = dir;
        self.bots.lock().unwrap().insert(slug.to_string(), bot);
        self.order.lock().unwrap().push(slug.to_string());
    }
}

#[cfg(test)]
impl Bot {
    /// A bot that is already serving at `addr`, for tests that exercise the
    /// proxy without spawning a process.
    pub(crate) fn fake_ready(slug: &str, addr: SocketAddr, token: &str) -> Arc<Self> {
        let (state_tx, _state_rx) = watch::channel(BotState::Ready { addr });
        Arc::new(Bot {
            slug: slug.to_string(),
            dir: PathBuf::from("."),
            workspace_root: PathBuf::from("."),
            addr_file: PathBuf::from("."),
            token: StdMutex::new(token.to_string()),
            stop_tx: watch::channel(false).0,
            _stop_rx: watch::channel(false).1,
            state_tx,
            _state_rx,
            stderr_tail: StdMutex::new(VecDeque::new()),
        })
    }

    /// A bot the supervisor has given up on.
    pub(crate) fn fake_crash_looped(slug: &str, reason: &str) -> Arc<Self> {
        let (state_tx, _state_rx) = watch::channel(BotState::CrashLooped {
            reason: reason.to_string(),
        });
        Arc::new(Bot {
            slug: slug.to_string(),
            dir: PathBuf::from("."),
            workspace_root: PathBuf::from("."),
            addr_file: PathBuf::from("."),
            token: StdMutex::new(String::new()),
            stop_tx: watch::channel(false).0,
            _stop_rx: watch::channel(false).1,
            state_tx,
            _state_rx,
            stderr_tail: StdMutex::new(VecDeque::new()),
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    static FAST: [Duration; 1] = [Duration::from_millis(10)];

    fn fast_policy() -> RestartPolicy {
        RestartPolicy {
            backoff: &FAST,
            max_starts: 3,
            window: Duration::from_secs(60),
            // Long enough that a slow first exec does not beat the child's
            // own exit to the finish line and report the wrong reason.
            ready_timeout: Duration::from_secs(2),
        }
    }

    /// Lay out a workspace with one bot folder and a stub "engine".
    fn fixture(script: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(super::bot_dir(dir.path(), "main")).unwrap();
        let program = dir.path().join("stub-engine");
        std::fs::write(&program, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, program)
    }

    async fn settle(bot: &Bot, want: impl Fn(&BotState) -> bool) -> BotState {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut rx = bot.subscribe();
        loop {
            let now = bot.state();
            if want(&now) {
                return now;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "stuck in {now:?}");
            let _ = tokio::time::timeout(left, rx.changed()).await;
        }
    }

    /// A bot that cannot start is parked, not restarted forever, and the
    /// reason carries the child's own last words — which is the only account
    /// of why a bot that never served did not serve.
    #[tokio::test]
    async fn a_bot_that_dies_instantly_is_parked_with_its_stderr() {
        let (dir, program) = fixture("#!/bin/sh\necho 'stub: config is broken' >&2\nexit 3\n");
        let sup = BotSupervisor::with_policy(dir.path(), &program, fast_policy());
        let bot = sup
            .start(&BotDef {
                slug: "main".into(),
                name: None,
            })
            .unwrap();

        let state = settle(&bot, |s| matches!(s, BotState::CrashLooped { .. })).await;
        let BotState::CrashLooped { reason } = state else {
            unreachable!()
        };
        assert!(reason.contains("started 3 times"), "{reason}");
        assert!(reason.contains("stub: config is broken"), "{reason}");

        // And a browser that connects to it is told, rather than parked on a
        // socket that will never carry anything.
        let err = bot
            .wait_ready(Duration::from_secs(1))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not running"), "{err}");
    }

    /// A child that lives but never publishes an address is a hang, not a
    /// crash — the readiness deadline is what turns it into one.
    #[tokio::test]
    async fn a_child_that_never_publishes_its_address_times_out() {
        let (dir, program) = fixture("#!/bin/sh\nsleep 30\n");
        let sup = BotSupervisor::with_policy(
            dir.path(),
            &program,
            RestartPolicy {
                ready_timeout: Duration::from_millis(150),
                ..fast_policy()
            },
        );
        let bot = sup
            .start(&BotDef {
                slug: "main".into(),
                name: None,
            })
            .unwrap();

        let state = settle(&bot, |s| matches!(s, BotState::CrashLooped { .. })).await;
        let BotState::CrashLooped { reason } = state else {
            unreachable!()
        };
        assert!(reason.contains("started 3 times"), "{reason}");
    }

    /// Shutting the host down stops the restart loop instead of racing it.
    #[tokio::test]
    async fn shutdown_stops_the_restart_loop() {
        static SLOW: [Duration; 1] = [Duration::from_secs(30)];
        let (dir, program) = fixture("#!/bin/sh\nexit 1\n");
        let sup = BotSupervisor::with_policy(
            dir.path(),
            &program,
            RestartPolicy {
                backoff: &SLOW,
                ..fast_policy()
            },
        );
        let bot = sup
            .start(&BotDef {
                slug: "main".into(),
                name: None,
            })
            .unwrap();
        settle(&bot, |s| matches!(s, BotState::Restarting { .. })).await;
        sup.shutdown();
        settle(&bot, |s| matches!(s, BotState::Stopped)).await;
    }

    /// #207/#208: an agent keeps the user's HOME, so the global AGENTS.md,
    /// secrets.json/.env and the macOS keychain are the user's, not a copy in
    /// the agent's folder. Its files come from the workspace root.
    #[tokio::test]
    async fn an_agent_keeps_the_users_home_and_gets_the_workspace_root() {
        let out_dir = tempfile::tempdir().unwrap();
        let out = out_dir.path().join("env.txt");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n%s\\n' \"$HOME\" \"$THCLAWS_WORKSPACE_ROOT\" > '{}'\nsleep 30\n",
            out.display()
        );
        let (dir, program) = fixture(&script);
        let sup = BotSupervisor::with_policy(dir.path(), &program, fast_policy());
        let bot = sup
            .start(&BotDef {
                slug: "main".into(),
                name: None,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !out.is_file()
            || std::fs::read_to_string(&out)
                .unwrap_or_default()
                .lines()
                .count()
                < 2
        {
            assert!(
                Instant::now() < deadline,
                "the stub never wrote its environment"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let seen = std::fs::read_to_string(&out).unwrap();
        let mut lines = seen.lines();
        assert_eq!(
            lines.next(),
            Some(std::env::var("HOME").unwrap_or_default().as_str()),
            "HOME is the user's"
        );
        assert_eq!(
            lines.next(),
            Some(dir.path().to_str().unwrap()),
            "files are the workspace's"
        );
        assert!(!bot.dir.join(".home").exists(), "no per-agent home is made");
        sup.shutdown();
    }

    #[tokio::test]
    async fn a_bot_without_a_folder_is_refused_before_anything_is_spawned() {
        let (dir, program) = fixture("#!/bin/sh\nexit 0\n");
        let sup = BotSupervisor::with_program(dir.path(), &program);
        let err = match sup.start(&BotDef {
            slug: "missing".into(),
            name: None,
        }) {
            Ok(_) => panic!("a bot with no folder must not start"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("no folder"), "{err}");
        assert!(sup.list().is_empty());
    }

    /// The host reports a bot's state to whoever asks, long after any browser
    /// has come and gone — so state must survive having no watcher at all.
    /// With `watch::Sender::send` dropping the value when the last receiver
    /// went away, a live bot reported `starting` forever.
    #[tokio::test]
    async fn state_moves_even_when_no_one_is_subscribed() {
        let (dir, program) = fixture("#!/bin/sh\nexit 7\n");
        let sup = BotSupervisor::with_policy(dir.path(), &program, fast_policy());
        let bot = sup
            .start(&BotDef {
                slug: "main".into(),
                name: None,
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        // Deliberately no `subscribe()` — poll the state the way an HTTP
        // handler does.
        while matches!(bot.state(), BotState::Starting) {
            assert!(Instant::now() < deadline, "state never left starting");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// dev-plan/59 Step 5: removing one bot must not take its siblings with
    /// it. Each bot owns its stop signal; the supervisor's shutdown fans out
    /// to all of them.
    #[tokio::test]
    async fn one_bot_stops_without_touching_the_others() {
        static SLOW: [Duration; 1] = [Duration::from_secs(30)];
        let (dir, program) = fixture("#!/bin/sh\nsleep 30\n");
        std::fs::create_dir_all(super::bot_dir(dir.path(), "other")).unwrap();
        let sup = BotSupervisor::with_policy(
            dir.path(),
            &program,
            RestartPolicy {
                backoff: &SLOW,
                ..fast_policy()
            },
        );
        let a = sup
            .start(&BotDef {
                slug: "main".into(),
                name: None,
            })
            .unwrap();
        let b = sup
            .start(&BotDef {
                slug: "other".into(),
                name: None,
            })
            .unwrap();

        a.stop();
        assert!(a.wait_stopped(Duration::from_secs(10)).await);
        assert_eq!(a.state(), BotState::Stopped);
        assert_ne!(b.state(), BotState::Stopped, "its sibling kept running");

        // And shutting the host down still reaches every bot.
        sup.shutdown();
        assert!(b.wait_stopped(Duration::from_secs(10)).await);
    }

    /// `list()` sorts by slug so the UI reads consistently, but the default
    /// bot is the FIRST in `bots.json`. Installing `alpha` next to `main`
    /// must not silently hand every browser to `alpha`.
    #[tokio::test]
    async fn the_bot_order_follows_bots_json_not_the_alphabet() {
        let (dir, program) = fixture("#!/bin/sh\nsleep 30\n");
        std::fs::create_dir_all(super::bot_dir(dir.path(), "alpha")).unwrap();
        let sup = BotSupervisor::with_policy(dir.path(), &program, fast_policy());
        sup.set_default("main");
        for slug in ["main", "alpha"] {
            sup.start(&BotDef {
                slug: slug.into(),
                name: None,
            })
            .unwrap();
        }
        // The rail reads `list()`, so it must follow bots.json order too —
        // sorting put a freshly installed `alpha` above the workspace's own
        // `main`, and the browser landed on it.
        assert_eq!(sup.list()[0].slug, "main", "list follows start order");
        assert_eq!(sup.focused().unwrap().slug, "main");
        sup.shutdown();
    }

    /// Refusing a removal must leave the bot running. Stopping first and
    /// refusing afterwards left a live host holding a dead child.
    #[tokio::test]
    async fn a_refused_removal_does_not_stop_the_bot() {
        let (dir, program) = fixture("#!/bin/sh\nsleep 30\n");
        std::fs::write(
            dir.path().join(super::super::CONFIG_REL),
            r#"{"version":1,"bots":[{"slug":"main"}]}"#,
        )
        .unwrap();
        let sup = BotSupervisor::with_policy(dir.path(), &program, fast_policy());
        let bot = sup
            .start(&BotDef {
                slug: "main".into(),
                name: None,
            })
            .unwrap();

        let err = match sup.remove_bot("main", false).await {
            Ok(()) => panic!("the only bot must not be removable"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("only agent"), "{err}");
        assert_ne!(bot.state(), BotState::Stopped, "it was stopped anyway");
        sup.shutdown();
    }

    /// The way out of `crash_looped`, and what the panel's Restart does: a
    /// fresh supervise loop for the same slug, in the same rail position.
    #[tokio::test]
    async fn restart_gives_a_parked_bot_a_fresh_loop() {
        let (dir, program) = fixture("#!/bin/sh\nexit 1\n");
        std::fs::create_dir_all(super::bot_dir(dir.path(), "other")).unwrap();
        let sup = BotSupervisor::with_policy(dir.path(), &program, fast_policy());
        for slug in ["main", "other"] {
            sup.start(&BotDef {
                slug: slug.into(),
                name: None,
            })
            .unwrap();
        }
        let bot = sup.get("main").unwrap();
        settle(&bot, |s| matches!(s, BotState::CrashLooped { .. })).await;

        let again = sup.restart_bot("main").await.unwrap();
        assert!(
            !Arc::ptr_eq(&bot, &again),
            "a new supervise loop, not the parked one"
        );
        assert!(!matches!(again.state(), BotState::CrashLooped { .. }));
        assert_eq!(
            sup.list()
                .iter()
                .map(|b| b.slug.as_str())
                .collect::<Vec<_>>(),
            vec!["main", "other"],
            "its place in the rail is kept"
        );
        match sup.restart_bot("nope").await {
            Ok(_) => panic!("unknown bot must not restart"),
            Err(e) => assert!(e.to_string().contains("no agent 'nope'")),
        }
        sup.shutdown();
    }

    /// Bytes map onto a 32-symbol alphabet, so no symbol is favoured.
    #[test]
    fn token_alphabet_divides_a_byte_evenly() {
        assert_eq!(256 % TOKEN_ALPHABET.len(), 0);
        assert!(mint_token().bytes().all(|b| TOKEN_ALPHABET.contains(&b)));
    }

    #[test]
    fn tokens_are_long_and_not_repeated() {
        let a = mint_token();
        let b = mint_token();
        assert_eq!(a.len(), TOKEN_LEN);
        assert_ne!(a, b);
    }
}
