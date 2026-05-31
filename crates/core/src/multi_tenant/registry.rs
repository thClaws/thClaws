//! Per-user `SharedSessionHandle` multiplexing for multi-tenant
//! `--serve` mode.
//!
//! Each authenticated user (verified via [`crate::multi_tenant::auth`])
//! gets their own [`SharedSessionHandle`] — independent agent loop,
//! independent session JSONL, independent event broadcast. Multiple
//! users sharing one pod don't observe each other's state.
//!
//! Lifecycle:
//! 1. WS upgrade arrives with HMAC-signed user-id headers.
//! 2. `handle_socket` calls [`UserSessionRegistry::get_or_spawn`].
//! 3. If absent, registry spawns a fresh `SharedSessionHandle` via
//!    [`crate::shared_session::spawn_with_approver`] (shared approver
//!    across users; modal prompts route to the cloud routing layer
//!    for now, dev-plan/35 Tier 3 may revisit).
//! 4. Handle gets cached in an LRU; future requests from the same
//!    user reuse it.
//! 5. Background task evicts idle sessions past TTL — eviction
//!    drops the handle which dropss the worker thread which finalises
//!    its session JSONL.
//!
//! Per-user state paths (session JSONL subdirs, per-user GUI-shell
//! storage, per-user usage metering) are wired through here:
//! [`UserSessionRegistry::get_or_spawn`] builds a [`UserStatePaths`]
//! from `(config.project_root, user_id)`, converts it to a
//! [`SessionRoots`], and threads it into
//! [`crate::shared_session::spawn_with_roots`] so the per-user
//! worker writes its session JSONL under
//! `<project>/.thclaws/users/<user_id>/sessions/`, its gui-shell
//! storage under `<project>/.thclaws/users/<user_id>/storage/`,
//! and its usage aggregates under
//! `<project>/.thclaws/users/<user_id>/usage/` — fully isolated
//! per user, fully recoverable on pod restart.

use super::auth::UserId;
use super::user_state::{SessionRoots, UserStatePaths};
use crate::permissions::ApprovalSink;
use crate::shared_session::{spawn_with_roots, SharedSessionHandle};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One slot in the registry — the user's session handle + bookkeeping
/// for eviction decisions.
pub struct UserSession {
    pub user_id: UserId,
    pub handle: Arc<SharedSessionHandle>,
    /// Wall-clock of the most recent activity (any IPC message routed
    /// to this session). Drives idle-TTL eviction.
    last_activity: Mutex<Instant>,
}

impl UserSession {
    pub fn touch(&self) {
        if let Ok(mut lock) = self.last_activity.lock() {
            *lock = Instant::now();
        }
    }

    pub fn idle_for(&self) -> Duration {
        self.last_activity
            .lock()
            .ok()
            .map(|t| t.elapsed())
            .unwrap_or(Duration::ZERO)
    }
}

/// Registry of per-user session handles. Cheap to clone (internally
/// an `Arc<RwLock<...>>`), shared across all WS connections in the
/// serve process.
#[derive(Clone)]
pub struct UserSessionRegistry {
    inner: Arc<std::sync::RwLock<RegistryState>>,
    config: Arc<RegistryConfig>,
}

struct RegistryState {
    sessions: HashMap<UserId, Arc<UserSession>>,
}

/// Construction-time configuration. `Arc` so the background evictor
/// task can hold a reference without locking the registry mutex.
pub struct RegistryConfig {
    pub max_users: usize,
    pub idle_timeout: Duration,
    /// Shared across all users — modal-style approval prompts route
    /// through the cloud routing layer (dev-plan/34) rather than the
    /// pod. Tier 3 may give each user their own approver if/when
    /// the cloud wants user-specific approval flows.
    pub approver: Arc<dyn ApprovalSink>,
    /// Cwd at serve start — passed through to
    /// `UserStatePaths::new(&project_root, user_id)` so per-user
    /// session JSONLs, storage, and usage all land under
    /// `<project_root>/.thclaws/users/<user_id>/...`. Single-tenant
    /// `--serve` does not construct a registry, so this is always
    /// set; tests use a tempdir.
    pub project_root: PathBuf,
}

impl UserSessionRegistry {
    pub fn new(config: RegistryConfig) -> Self {
        Self {
            inner: Arc::new(std::sync::RwLock::new(RegistryState {
                sessions: HashMap::new(),
            })),
            config: Arc::new(config),
        }
    }

    /// Fetch the existing session for this user, or spawn a fresh
    /// one. Bumps last_activity unconditionally so the LRU evictor
    /// keeps active users alive.
    pub fn get_or_spawn(&self, user_id: &UserId) -> Arc<UserSession> {
        // Fast path: read lock, hit.
        if let Ok(guard) = self.inner.read() {
            if let Some(session) = guard.sessions.get(user_id) {
                session.touch();
                return session.clone();
            }
        }

        // Slow path: write lock + double-check (another thread may
        // have spawned the same user between our read drop and
        // write acquire).
        let mut guard = self
            .inner
            .write()
            .expect("UserSessionRegistry write lock poisoned");
        if let Some(session) = guard.sessions.get(user_id) {
            session.touch();
            return session.clone();
        }

        // Cap enforcement: evict LRU if at capacity before inserting.
        if guard.sessions.len() >= self.config.max_users {
            evict_lru(&mut guard);
        }

        // dev-plan/35 Tier 1: per-user roots derived from
        // (project_root, user_id) — the SharedSessionHandle below
        // writes its session JSONL, gui-shell storage, and usage
        // tracker under <project>/.thclaws/users/<user_id>/...
        // instead of the cwd-relative single-tenant defaults.
        let user_state = UserStatePaths::new(&self.config.project_root, user_id);
        let roots = SessionRoots::for_user_state(&user_state);
        let handle = Arc::new(spawn_with_roots(self.config.approver.clone(), Some(roots)));
        let session = Arc::new(UserSession {
            user_id: user_id.clone(),
            handle,
            last_activity: Mutex::new(Instant::now()),
        });
        guard.sessions.insert(user_id.clone(), session.clone());
        session
    }

    /// Drop a user's session handle (called from admin endpoints —
    /// dev-plan/34 "evict on dispute / ban" path). Idempotent.
    /// Returns true if a session was actually removed. The dropped
    /// handle's worker thread observes channel close and exits,
    /// finalising its session JSONL via Drop on SharedSessionHandle.
    pub fn evict(&self, user_id: &UserId) -> bool {
        if let Ok(mut guard) = self.inner.write() {
            guard.sessions.remove(user_id).is_some()
        } else {
            false
        }
    }

    /// Count of currently-resident sessions. For metrics + tests.
    pub fn active_user_count(&self) -> usize {
        self.inner.read().map(|g| g.sessions.len()).unwrap_or(0)
    }

    /// Snapshot user-ids — for ops endpoints (`/admin/users`).
    pub fn active_user_ids(&self) -> Vec<UserId> {
        self.inner
            .read()
            .map(|g| g.sessions.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Spawn a background evictor that wakes every `interval` to
    /// drop sessions idle past the configured TTL. Returns the
    /// JoinHandle so the caller (server::run) can shut it down on
    /// process exit. Sweep cost is O(active_users), cheap.
    pub fn spawn_evictor(&self, interval: Duration) -> tokio::task::JoinHandle<()> {
        let registry = self.clone();
        let idle_timeout = self.config.idle_timeout;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // Skip the immediate first tick so we don't churn at boot.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                registry.sweep_idle(idle_timeout);
            }
        })
    }

    fn sweep_idle(&self, idle_timeout: Duration) {
        let to_evict: Vec<UserId> = match self.inner.read() {
            Ok(guard) => guard
                .sessions
                .iter()
                .filter(|(_, s)| s.idle_for() > idle_timeout)
                .map(|(id, _)| id.clone())
                .collect(),
            Err(_) => return,
        };
        if to_evict.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.inner.write() {
            for id in to_evict {
                guard.sessions.remove(&id);
            }
        }
    }
}

/// Drop the single least-recently-active session. Called holding
/// the write lock when at capacity. No-op if the registry is empty
/// (shouldn't happen — caller checks `len() >= max_users` first).
fn evict_lru(state: &mut RegistryState) {
    // LRU = longest idle = MAX idle_for (oldest last_activity).
    // Easy bug to write as min — `idle_for` is "how long since
    // touched", so "least recently used" wants the biggest one.
    let oldest = state
        .sessions
        .iter()
        .max_by_key(|(_, s)| s.idle_for())
        .map(|(id, _)| id.clone());
    if let Some(id) = oldest {
        state.sessions.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::AutoApprover;

    fn config(max_users: usize, idle_timeout: Duration) -> RegistryConfig {
        // Existing unit tests don't exercise the per-user roots
        // (just registry mechanics) so std::env::temp_dir() is a
        // safe shared root — UserSessionRegistry only stores it,
        // the spawned worker thread is the only consumer and it
        // gets dropped at test end before writing anything that
        // would collide.
        config_with_root(max_users, idle_timeout, std::env::temp_dir())
    }

    fn config_with_root(
        max_users: usize,
        idle_timeout: Duration,
        project_root: PathBuf,
    ) -> RegistryConfig {
        RegistryConfig {
            max_users,
            idle_timeout,
            approver: Arc::new(AutoApprover),
            project_root,
        }
    }

    #[test]
    fn get_or_spawn_returns_same_handle_per_user() {
        let reg = UserSessionRegistry::new(config(10, Duration::from_secs(60)));
        let alice = UserId::new_for_test("alice");
        let a = reg.get_or_spawn(&alice);
        let b = reg.get_or_spawn(&alice);
        assert!(Arc::ptr_eq(&a, &b), "same user → same session arc");
        assert_eq!(reg.active_user_count(), 1);
    }

    #[test]
    fn different_users_get_different_handles() {
        let reg = UserSessionRegistry::new(config(10, Duration::from_secs(60)));
        let a = reg.get_or_spawn(&UserId::new_for_test("alice"));
        let b = reg.get_or_spawn(&UserId::new_for_test("bob"));
        assert!(!Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a.handle, &b.handle));
        assert_eq!(reg.active_user_count(), 2);
    }

    #[test]
    fn evict_removes_user() {
        let reg = UserSessionRegistry::new(config(10, Duration::from_secs(60)));
        let alice = UserId::new_for_test("alice");
        reg.get_or_spawn(&alice);
        assert_eq!(reg.active_user_count(), 1);
        assert!(reg.evict(&alice));
        assert_eq!(reg.active_user_count(), 0);
        // Idempotent: second evict returns false.
        assert!(!reg.evict(&alice));
    }

    #[test]
    fn capacity_triggers_lru_eviction() {
        let reg = UserSessionRegistry::new(config(2, Duration::from_secs(60)));
        let a = UserId::new_for_test("a");
        let b = UserId::new_for_test("b");
        let c = UserId::new_for_test("c");
        reg.get_or_spawn(&a);
        std::thread::sleep(Duration::from_millis(10));
        reg.get_or_spawn(&b);
        std::thread::sleep(Duration::from_millis(10));
        // c arrives → at cap (2) → a should be evicted (oldest).
        reg.get_or_spawn(&c);
        let active: Vec<_> = reg.active_user_ids().into_iter().collect();
        assert_eq!(active.len(), 2);
        assert!(active.contains(&b));
        assert!(active.contains(&c));
        assert!(!active.contains(&a), "LRU should have dropped a");
    }

    #[test]
    fn touch_updates_lru_so_active_user_survives() {
        let reg = UserSessionRegistry::new(config(2, Duration::from_secs(60)));
        let a = UserId::new_for_test("a");
        let b = UserId::new_for_test("b");
        let c = UserId::new_for_test("c");
        reg.get_or_spawn(&a);
        std::thread::sleep(Duration::from_millis(10));
        reg.get_or_spawn(&b);
        std::thread::sleep(Duration::from_millis(10));
        // Re-touching a refreshes its activity — now b is oldest.
        reg.get_or_spawn(&a);
        std::thread::sleep(Duration::from_millis(10));
        reg.get_or_spawn(&c);
        let active = reg.active_user_ids();
        assert!(active.contains(&a), "a should survive — recently touched");
        assert!(active.contains(&c));
        assert!(!active.contains(&b), "b is now the LRU");
    }

    /// dev-plan/35 Tier 1 acceptance: every user's spawned
    /// SharedSessionHandle carries a `session_roots` that points at
    /// THEIR per-user subtree under the registry's project_root.
    /// Without this, the registry's "per-user isolation" is purely
    /// in-memory — restart loses everyone, and two users can collide
    /// on storage keys. With it, the worker's own SessionStore /
    /// gui-shell storage / usage tracker write to disjoint paths.
    #[test]
    fn per_user_handles_carry_distinct_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = UserSessionRegistry::new(config_with_root(
            10,
            Duration::from_secs(60),
            tmp.path().to_path_buf(),
        ));
        let alice = UserId::new_for_test("alice");
        let bob = UserId::new_for_test("bob");
        let a = reg.get_or_spawn(&alice);
        let b = reg.get_or_spawn(&bob);

        let a_roots = a.handle.session_roots.as_ref().expect("alice roots set");
        let b_roots = b.handle.session_roots.as_ref().expect("bob roots set");

        // Each lands under its own user-id segment.
        let a_expected = tmp.path().join(".thclaws/users/alice/sessions");
        let b_expected = tmp.path().join(".thclaws/users/bob/sessions");
        assert_eq!(a_roots.sessions_dir, a_expected);
        assert_eq!(b_roots.sessions_dir, b_expected);

        // storage_dir + usage_dir are siblings under the same user
        // prefix — they must not collide either.
        assert!(a_roots
            .storage_dir
            .starts_with(tmp.path().join(".thclaws/users/alice")));
        assert!(b_roots
            .storage_dir
            .starts_with(tmp.path().join(".thclaws/users/bob")));
        assert!(a_roots
            .usage_dir
            .starts_with(tmp.path().join(".thclaws/users/alice")));
        assert!(b_roots
            .usage_dir
            .starts_with(tmp.path().join(".thclaws/users/bob")));
    }

    /// dev-plan/35 Tier 1 "done means": kill the pod (drop the
    /// registry), restart against the same project dir, the user's
    /// session JSONLs and gui-shell storage are still on disk and
    /// the new registry recovers them when the user reconnects.
    /// This test simulates the on-disk side end-to-end:
    /// 1. Build registry; spawn alice; write a session JSONL +
    ///    storage value via her roots.
    /// 2. Drop registry + handle (worker thread tears down).
    /// 3. Build a NEW registry on the same tempdir; spawn alice
    ///    again. The new roots must be byte-identical so the new
    ///    worker's SessionStore + storage handlers see the
    ///    pre-restart files.
    #[test]
    fn restart_recovery_user_sees_prior_session_on_new_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().to_path_buf();
        let alice = UserId::new_for_test("alice");

        // --- pod 1 ---
        let pre_roots = {
            let reg = UserSessionRegistry::new(config_with_root(
                10,
                Duration::from_secs(60),
                project_root.clone(),
            ));
            let a = reg.get_or_spawn(&alice);
            let roots = a.handle.session_roots.as_ref().unwrap().clone();

            // Write a session JSONL via the per-user SessionStore at
            // the same path the worker would use. Mirrors what
            // shared_session.rs:1683 does on its first turn.
            std::fs::create_dir_all(&roots.sessions_dir).unwrap();
            let store = crate::session::SessionStore::new(roots.sessions_dir.clone());
            let mut sess = crate::session::Session::new("test-model", "/tmp/proj");
            let sess_id = sess.id.clone();
            let path = store.path_for(&sess_id);
            sess.append_to(&path).unwrap();
            assert!(path.exists(), "pre-restart session jsonl must exist");

            // Write a gui-shell storage value via the per-user override.
            crate::gui_shell::storage::set_in(
                &roots.storage_dir,
                "chatbot",
                &sess_id,
                "msg-1",
                serde_json::json!("from-pod-1"),
            )
            .unwrap();

            roots
            // reg + handle drop here — worker thread exits, just like
            // a pod restart.
        };

        // --- pod 2 (fresh registry, same project_root) ---
        let reg2 = UserSessionRegistry::new(config_with_root(
            10,
            Duration::from_secs(60),
            project_root.clone(),
        ));
        let a2 = reg2.get_or_spawn(&alice);
        let post_roots = a2.handle.session_roots.as_ref().unwrap();

        // The recovered roots are byte-identical — the new worker
        // sees the same on-disk subtree the old one wrote to.
        assert_eq!(pre_roots.sessions_dir, post_roots.sessions_dir);
        assert_eq!(pre_roots.storage_dir, post_roots.storage_dir);

        // Files written before restart are visible to the new pod's
        // SessionStore + storage handler.
        let store_after = crate::session::SessionStore::new(post_roots.sessions_dir.clone());
        let sessions: Vec<_> = store_after.list().unwrap();
        assert_eq!(sessions.len(), 1, "alice's prior session must persist");
        let sess_id = sessions[0].id.clone();
        let recovered = crate::gui_shell::storage::get_in(
            &post_roots.storage_dir,
            "chatbot",
            &sess_id,
            "msg-1",
        )
        .unwrap();
        assert_eq!(recovered, serde_json::json!("from-pod-1"));
    }

    /// dev-plan/35 Tier 1 "done means": 50 concurrent users on the
    /// same Agent run without state corruption. We can't soak for an
    /// hour in a unit test, but we CAN hammer the registry from many
    /// threads in parallel and verify (a) no panic, (b) every user
    /// gets back the same Arc across calls (no double-spawn races),
    /// (c) each user's roots are distinct, (d) no cross-user leakage.
    /// This is the closest practical check to a real soak — the
    /// registry's RwLock + double-check spawn path is the actual
    /// concurrency surface we need to prove.
    #[test]
    fn concurrent_50_users_no_cross_leakage_or_double_spawn() {
        use std::sync::Arc as StdArc;
        let tmp = tempfile::tempdir().unwrap();
        let reg = StdArc::new(UserSessionRegistry::new(config_with_root(
            100,
            Duration::from_secs(60),
            tmp.path().to_path_buf(),
        )));

        let mut handles = Vec::new();
        // 50 users × 4 threads each — exercises (a) initial spawn
        // races between threads asking for the same user, and (b)
        // distinct-user spawn fan-out under lock contention.
        for u in 0..50 {
            for _t in 0..4 {
                let reg = reg.clone();
                let user_id = UserId::new_for_test(&format!("u{u:02}"));
                handles.push(std::thread::spawn(move || reg.get_or_spawn(&user_id)));
            }
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // (a) 200 calls, 50 users → exactly 50 distinct UserSessions.
        assert_eq!(reg.active_user_count(), 50);

        // (b) for each user, all 4 racing calls returned the SAME
        // Arc — double-check spawn worked, no leak.
        for u in 0..50 {
            let id = UserId::new_for_test(&format!("u{u:02}"));
            let same: Vec<_> = results.iter().filter(|s| s.user_id == id).collect();
            assert_eq!(same.len(), 4, "u{u:02} should have 4 entries in results");
            for s in &same[1..] {
                assert!(
                    Arc::ptr_eq(s, same[0]),
                    "u{u:02} returned distinct Arcs — double-spawn race"
                );
            }
        }

        // (c) every user's roots are distinct + cleanly disjoint.
        let mut session_dirs: Vec<PathBuf> = Vec::new();
        for u in 0..50 {
            let id = UserId::new_for_test(&format!("u{u:02}"));
            let s = reg.get_or_spawn(&id);
            let r = s.handle.session_roots.as_ref().unwrap();
            session_dirs.push(r.sessions_dir.clone());
        }
        let mut sorted = session_dirs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            50,
            "every user must have a unique sessions_dir"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evictor_sweeps_idle_sessions() {
        let reg = UserSessionRegistry::new(config(10, Duration::from_millis(50)));
        reg.get_or_spawn(&UserId::new_for_test("idle"));
        assert_eq!(reg.active_user_count(), 1);
        let evictor = reg.spawn_evictor(Duration::from_millis(20));
        // Wait long enough for idle to exceed 50ms + a sweep cycle.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(reg.active_user_count(), 0);
        evictor.abort();
    }
}
