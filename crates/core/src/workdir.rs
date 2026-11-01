//! dev-plan/42: per-session working-directory chokepoint.
//!
//! In multiuser `--serve` (one pod, a `workspace-<id>/` per user) many
//! sessions share one process, so the agent's working directory must
//! NOT come from the process-global `std::env::current_dir()` — that's a
//! single mutable global racing across tenants (a `set_current_dir` on
//! one user's turn would relocate another user's in-flight path
//! resolution; see dev-plan/42 §Security). Each worker establishes a
//! task-local working root around its agent run, and every path-resolving
//! tool reads it through [`current_workdir`].
//!
//! A `tokio::task_local!` (not a `thread_local!`) is required because
//! each worker runs its own *multi-threaded* runtime, so a tool future
//! can resume on a different runtime thread after an `.await`; a
//! task-local follows the task across those hops, a thread-local would
//! not.
//!
//! Single-tenant `--serve`, desktop, and CLI never enter the scope, so
//! [`current_workdir`] falls back to `std::env::current_dir()` —
//! behaviour unchanged. In multiuser the worker always establishes the
//! scope, so process cwd is never consulted (fail-closed).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

tokio::task_local! {
    static WORKDIR: PathBuf;
}

/// Process-wide "this is a `--serve --multiuser` pod" flag. Set once at
/// serve start. Unlike [`workdir_is_scoped`] (true only *inside* a
/// worker's per-turn task-local scope), this is readable everywhere —
/// including IPC handlers that run outside any session scope — so global
/// mutators (`set_current_dir`, `Sandbox::init`) can refuse to fire in a
/// shared multi-tenant process where they'd clobber every tenant.
static MULTIUSER: AtomicBool = AtomicBool::new(false);

/// Mark the process as a multiuser serve pod. Called once from
/// `server::run` when multi-tenant mode is configured.
pub fn set_multiuser(on: bool) {
    #[cfg(test)]
    TEST_MULTIUSER.with(|c| c.set(on));
    #[cfg(not(test))]
    MULTIUSER.store(on, Ordering::Relaxed);
}

// Unit tests run in parallel threads of one process; a process-wide flag
// flipped by one test would make every concurrent file-tool test fail closed.
#[cfg(test)]
thread_local! {
    static TEST_MULTIUSER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// True in a `--serve --multiuser` process. Global cwd/sandbox mutators
/// guard on this to stay no-ops (per-session roots come from the
/// task-local scope instead).
pub fn is_multiuser() -> bool {
    #[cfg(test)]
    if TEST_MULTIUSER.with(|c| c.get()) {
        return true;
    }
    MULTIUSER.load(Ordering::Relaxed)
}

/// A multiuser process running code outside any member's scope: there is no
/// right root to resolve against, so path and confinement checks refuse
/// rather than fall back to the pod-wide `/workspace` (every member's files).
pub fn member_scope_missing() -> bool {
    is_multiuser() && !workdir_is_scoped()
}

pub const UNSCOPED_REFUSAL: &str =
    "refused: no member workspace is active for this call in a shared workspace";

/// The active session's working directory: the task-local root when a
/// worker has scoped one (multiuser), else the process cwd (single-
/// tenant / desktop / CLI). This is the single site path-resolving tools
/// consult instead of `std::env::current_dir()`.
pub fn current_workdir() -> PathBuf {
    WORKDIR
        .try_with(|p| p.clone())
        .unwrap_or_else(|_| workspace_root())
}

/// dev-plan/61: the folder a user's files live in. An agent under a workspace
/// host runs with its own folder as the process cwd, so its settings,
/// sessions and memory stay its own, but its files are the workspace's: the
/// host passes `THCLAWS_WORKSPACE_ROOT`, and every agent reads and writes
/// there. Without the variable this is the process cwd, as before.
pub fn workspace_root() -> PathBuf {
    match std::env::var("THCLAWS_WORKSPACE_ROOT") {
        Ok(s) if !s.trim().is_empty() => PathBuf::from(s),
        _ => std::env::current_dir().unwrap_or_default(),
    }
}

/// The folder `/cloud push|pull|revision` moves. A multiuser pod hands each
/// session its own root; elsewhere it is the workspace, never an agent's own
/// folder under a host — syncing that left every user file behind and wrote
/// the agent's `.thclaws` over the cloud workspace's root.
pub fn cloud_sync_root(session_cwd: &std::path::Path) -> PathBuf {
    if is_multiuser() {
        session_cwd.to_path_buf()
    } else {
        workspace_root()
    }
}

/// finding 11: the agent's OWN folder under a workspace host.
///
/// A bot runs with its folder as the process cwd while
/// `THCLAWS_WORKSPACE_ROOT` points at the user's files, so the two differ
/// exactly when there is a host. Without one they are the same directory and
/// this is `None`, which keeps every rule below a no-op off a host.
pub fn agent_dir() -> Option<PathBuf> {
    let root = match std::env::var("THCLAWS_WORKSPACE_ROOT") {
        Ok(s) if !s.trim().is_empty() => PathBuf::from(s),
        _ => return None,
    };
    let cwd = std::env::current_dir().ok()?;
    (cwd != root).then_some(cwd)
}

/// Subpaths of `.thclaws/` that belong to the WORKSPACE rather than to the
/// agent: the project KMS and the engine's log, which every agent shares and
/// which `kms.rs` and `util.rs` already resolve at the workspace root.
const SHARED_THCLAWS: &[&str] = &["state/kms", "state/logs"];

/// Where a relative path the agent typed into a tool resolves from.
///
/// `.thclaws/…` is the agent's own — holding its settings, sessions, skills
/// and whatever it ships — and `AGENT_ENTRIES` in the migration says as much.
/// Everything else is the user's and resolves at the workspace root, as
/// before. Two subpaths are carved out because they are shared, not agent
/// state (see [`SHARED_THCLAWS`]).
///
/// Bash cannot follow this rule: its cwd is one directory for the whole
/// command, so a bare `.thclaws/…` inside a shell string cannot mean the
/// agent's folder for one process and the host's for another. Agents reach
/// their own folder from a shell through `THCLAWS_AGENT_DIR` instead.
pub fn tool_base(path: &str) -> PathBuf {
    match agent_dir() {
        Some(agent) if is_agent_owned(path) => agent,
        _ => current_workdir(),
    }
}

/// True for a relative `.thclaws/…` that belongs to the agent.
fn is_agent_owned(path: &str) -> bool {
    let p = path.strip_prefix("./").unwrap_or(path);
    if std::path::Path::new(p).is_absolute() {
        return false;
    }
    let Some(rest) = p
        .strip_prefix(".thclaws/")
        .or_else(|| (p == ".thclaws").then_some(""))
    else {
        return false;
    };
    !SHARED_THCLAWS
        .iter()
        .any(|s| rest == *s || rest.strip_prefix(s).is_some_and(|t| t.starts_with('/')))
}

/// True when a per-session working root is active (i.e. we're inside a
/// multiuser worker scope). Lets callers fail-closed instead of touching
/// process cwd when isolation is expected.
pub fn workdir_is_scoped() -> bool {
    WORKDIR.try_with(|_| ()).is_ok()
}

/// Run `fut` with `root` as the task-local working directory. The
/// multiuser worker wraps each agent turn in this so every awaited tool
/// call resolves against the user's `workspace-<id>/`.
pub async fn scope_workdir<F, T>(root: PathBuf, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    WORKDIR.scope(root, fut).await
}

/// Synchronous twin of [`scope_workdir`]: a multiuser WS frame's IPC
/// handler runs under the calling member's root.
pub fn sync_scope_workdir<F, T>(root: PathBuf, f: F) -> T
where
    F: FnOnce() -> T,
{
    WORKDIR.sync_scope(root, f)
}

/// The scoped root, if any — captured before a handler spawns a task so
/// the task can re-enter it with [`rescope`].
pub fn scoped_root() -> Option<PathBuf> {
    WORKDIR.try_with(|p| p.clone()).ok()
}

pub async fn rescope<F, T>(root: Option<PathBuf>, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    match root {
        Some(r) => WORKDIR.scope(r, fut).await,
        None => fut.await,
    }
}

/// The member's workdir and billing identity, captured before a thread or
/// spawned task (task-locals cross neither) and re-entered on the far side.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    root: Option<PathBuf>,
    member: Option<String>,
}

impl Scope {
    pub fn capture() -> Self {
        Self {
            root: scoped_root(),
            member: crate::multi_tenant::current_member_id(),
        }
    }

    pub fn new(root: Option<PathBuf>, member: Option<String>) -> Self {
        Self { root, member }
    }

    pub async fn run<F, T>(self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        match self.member {
            Some(m) => rescope(self.root, crate::multi_tenant::scope_member(m, fut)).await,
            None => rescope(self.root, fut).await,
        }
    }

    pub fn run_sync<F, T>(self, f: F) -> T
    where
        F: FnOnce() -> T,
    {
        let inner = move || match self.member {
            Some(m) => crate::multi_tenant::member::sync_scope_member(m, f),
            None => f(),
        };
        match self.root {
            Some(r) => WORKDIR.sync_scope(r, inner),
            None => inner(),
        }
    }
}

/// `tokio::spawn` that keeps the caller's [`Scope`].
pub fn spawn_scoped<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(Scope::capture().run(fut))
}

/// `tokio::task::spawn_blocking` that keeps the caller's [`Scope`]: the
/// blocking pool's threads carry no task-locals of their own.
pub fn spawn_blocking_scoped<F, T>(f: F) -> tokio::task::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let scope = Scope::capture();
    tokio::task::spawn_blocking(move || scope.run_sync(f))
}

#[cfg(test)]
mod tests {

    /// dev-plan/61: an agent under a workspace host shares the workspace's
    /// files. The host passes `THCLAWS_WORKSPACE_ROOT`; the agent's own folder
    /// stays its process cwd.
    #[test]
    fn an_agent_under_a_host_resolves_files_against_the_workspace() {
        let _g = crate::kms::test_env_lock();
        let prev = std::env::var("THCLAWS_WORKSPACE_ROOT").ok();
        let ws = tempfile::tempdir().unwrap();

        std::env::remove_var("THCLAWS_WORKSPACE_ROOT");
        assert_eq!(workspace_root(), std::env::current_dir().unwrap());

        std::env::set_var("THCLAWS_WORKSPACE_ROOT", ws.path());
        assert_eq!(workspace_root(), ws.path());
        assert_eq!(
            current_workdir(),
            ws.path(),
            "unscoped tools use the shared root"
        );

        std::env::set_var("THCLAWS_WORKSPACE_ROOT", "   ");
        assert_eq!(
            workspace_root(),
            std::env::current_dir().unwrap(),
            "blank is unset"
        );

        match prev {
            Some(v) => std::env::set_var("THCLAWS_WORKSPACE_ROOT", v),
            None => std::env::remove_var("THCLAWS_WORKSPACE_ROOT"),
        }
    }

    use super::*;

    #[test]
    fn cloud_sync_moves_the_workspace_not_the_agent_folder() {
        let _g = crate::kms::test_env_lock();
        let prev = std::env::var("THCLAWS_WORKSPACE_ROOT").ok();
        let ws = tempfile::tempdir().unwrap();
        let agent = ws.path().join(".thclaws/bots/main");

        std::env::set_var("THCLAWS_WORKSPACE_ROOT", ws.path());
        assert_eq!(
            cloud_sync_root(&agent),
            ws.path(),
            "under a host: the workspace"
        );

        std::env::remove_var("THCLAWS_WORKSPACE_ROOT");
        assert_eq!(
            cloud_sync_root(&agent),
            std::env::current_dir().unwrap(),
            "no host: the process cwd"
        );

        match prev {
            Some(v) => std::env::set_var("THCLAWS_WORKSPACE_ROOT", v),
            None => std::env::remove_var("THCLAWS_WORKSPACE_ROOT"),
        }
    }

    /// finding 11: an agent's own `.thclaws/…` resolves in its folder, so it
    /// can reach the scripts and state it ships. The user's files and the two
    /// shared subpaths keep resolving at the workspace root.
    #[test]
    fn an_agents_own_thclaws_resolves_in_its_folder() {
        let _g = crate::kms::test_env_lock();
        let prev = std::env::var("THCLAWS_WORKSPACE_ROOT").ok();
        let ws = tempfile::tempdir().unwrap();
        let agent = std::env::current_dir().unwrap();

        std::env::set_var("THCLAWS_WORKSPACE_ROOT", ws.path());
        assert_eq!(
            agent_dir(),
            Some(agent.clone()),
            "cwd differs from the root"
        );

        for p in [
            ".thclaws",
            ".thclaws/book.json",
            "./.thclaws/scripts/book.py",
            ".thclaws/state/sessions/a.jsonl",
            ".thclaws/state/kmsx/not-the-carve-out",
        ] {
            assert_eq!(tool_base(p), agent, "{p} is the agent's");
        }
        for p in [
            "chapters/ch01.md",
            ".thclaws/state/kms",
            ".thclaws/state/kms/vault/page.md",
            ".thclaws/state/logs/engine.log",
            "/etc/hosts",
        ] {
            assert_eq!(tool_base(p), ws.path(), "{p} is not the agent's");
        }

        // Off a host cwd IS the root, so nothing moves.
        std::env::remove_var("THCLAWS_WORKSPACE_ROOT");
        assert_eq!(agent_dir(), None);
        assert_eq!(tool_base(".thclaws/book.json"), agent);

        match prev {
            Some(v) => std::env::set_var("THCLAWS_WORKSPACE_ROOT", v),
            None => std::env::remove_var("THCLAWS_WORKSPACE_ROOT"),
        }
    }

    #[tokio::test]
    async fn unscoped_falls_back_to_process_cwd() {
        // Other tests move the process cwd under this lock; without it the
        // two reads below can straddle one of those moves.
        let _g = crate::kms::test_env_lock();
        let expected = std::env::current_dir().unwrap();
        assert_eq!(current_workdir(), expected);
        assert!(!workdir_is_scoped());
    }

    /// M-a/M-e: a member's scope survives the thread and blocking-pool hops
    /// tool callers make; without it the far side would fail closed.
    #[tokio::test]
    async fn scope_crosses_threads_and_blocking_tasks() {
        let root = PathBuf::from("/tmp/workspace-alice");
        let (blocking, spawned, thread) = scope_workdir(root.clone(), async {
            let blocking = spawn_blocking_scoped(scoped_root).await.unwrap();
            let spawned = spawn_scoped(async { scoped_root() }).await.unwrap();
            let scope = Scope::capture();
            let thread = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                rt.block_on(scope.run(async { scoped_root() }))
            })
            .join()
            .unwrap();
            (blocking, spawned, thread)
        })
        .await;
        assert_eq!(blocking.as_ref(), Some(&root));
        assert_eq!(spawned.as_ref(), Some(&root));
        assert_eq!(thread.as_ref(), Some(&root));
        assert_eq!(
            tokio::task::spawn_blocking(scoped_root).await.unwrap(),
            None
        );
    }

    #[test]
    fn member_scope_missing_only_when_multiuser_and_unscoped() {
        assert!(!member_scope_missing());
        set_multiuser(true);
        let unscoped = member_scope_missing();
        let scoped = sync_scope_workdir(PathBuf::from("/tmp/w"), member_scope_missing);
        set_multiuser(false);
        assert!(unscoped);
        assert!(!scoped);
    }

    #[tokio::test]
    async fn scope_overrides_and_is_isolated() {
        let a = PathBuf::from("/tmp/workspace-alice");
        let b = PathBuf::from("/tmp/workspace-bob");

        let got_a = scope_workdir(a.clone(), async {
            assert!(workdir_is_scoped());
            // Survives an await point (the multi-thread-runtime hazard).
            tokio::task::yield_now().await;
            current_workdir()
        })
        .await;
        let got_b = scope_workdir(b.clone(), async { current_workdir() }).await;

        assert_eq!(got_a, a);
        assert_eq!(got_b, b);
        assert_ne!(got_a, got_b, "concurrent sessions resolve independently");
        // Back outside any scope.
        assert!(!workdir_is_scoped());
    }
}
