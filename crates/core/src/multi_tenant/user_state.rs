//! Per-user state path resolver for multi-tenant `--serve`.
//!
//! Every persistent thing a user writes — session JSONL, GUI Shell
//! storage, permission grants, usage metering, agent-produced
//! output files — lives under a single per-user prefix at
//! `<project>/.thclaws/users/<user_id>/...` and
//! `<project>/output/users/<user_id>/...`. Single prefix means:
//!
//! - `tar czf user-snapshot.tgz .thclaws/users/<id>/ output/users/<id>/`
//!   captures everything for that user (dispute escalation, GDPR
//!   export, debugging).
//! - The Mode B file-asset URL `/t/<token>/file-asset/users/<id>/...`
//!   matches the on-disk layout 1:1; sticky-routed user can only
//!   request their own subtree.
//! - The sandbox boundary for writes is one directory:
//!   `<project>/users/<user_id>/` ∪ `<project>/.thclaws/users/<user_id>/`.
//!
//! Single-tenant mode (no `--multi-tenant`) doesn't construct any of
//! these paths — the existing `./.thclaws/sessions/` + `./output/`
//! layout is unchanged.

use super::auth::UserId;
use std::path::{Path, PathBuf};

/// All paths a user's state spans, relative to a project root.
/// Cheap to construct; just string concatenation.
#[derive(Debug, Clone)]
pub struct UserStatePaths {
    /// Project root (cwd at serve start when `--multi-tenant` is on).
    pub project_root: PathBuf,
    /// `<project>/.thclaws/users/<user_id>/`. Hidden namespace for
    /// session JSONLs, storage, grants, usage — anything internal
    /// the agent / runtime writes that the user shouldn't poke at.
    pub thclaws_user_root: PathBuf,
    /// `<project>/output/users/<user_id>/`. The visible side — files
    /// the agent generates intended for the user (images, reports,
    /// downloads). Served via `/t/<token>/file-asset/users/<id>/...`.
    pub output_root: PathBuf,
}

impl UserStatePaths {
    /// Resolve all paths for `(project_root, user_id)`. Does not
    /// create directories — first write to each branch calls
    /// `fs::create_dir_all` lazily.
    pub fn new(project_root: &Path, user_id: &UserId) -> Self {
        let user_segment = user_id.as_str();
        Self {
            project_root: project_root.to_path_buf(),
            thclaws_user_root: project_root
                .join(".thclaws")
                .join("users")
                .join(user_segment),
            output_root: project_root.join("output").join("users").join(user_segment),
        }
    }

    /// `<thclaws_user_root>/sessions/` — directory the per-user
    /// `SessionStore` writes its JSONLs into.
    pub fn sessions_dir(&self) -> PathBuf {
        self.thclaws_user_root.join("sessions")
    }

    /// `<thclaws_user_root>/storage/` — GUI Shell `thclaws.storage`
    /// backing files, one JSON per (shell_id, session_id).
    pub fn storage_dir(&self) -> PathBuf {
        self.thclaws_user_root.join("storage")
    }

    /// Per-user permission grants (Tier 3 will write here; declared
    /// now for path stability).
    pub fn grants_path(&self) -> PathBuf {
        self.thclaws_user_root.join("grants.json")
    }

    /// `<thclaws_user_root>/usage/<provider>/<model>.json` —
    /// streamed metering aggregates. Currently informational; the
    /// authoritative metering pipeline (Task 29) emits to the
    /// MeteringSink and the cloud rolls up. This local copy is a
    /// per-pod debug aid.
    pub fn usage_dir(&self) -> PathBuf {
        self.thclaws_user_root.join("usage")
    }

    /// Writable subtree for Bash / Write / Edit when this user is
    /// the authenticated principal. Used by
    /// `Sandbox::check_write_for_user` as the per-user root.
    /// Includes both the `.thclaws/users/<id>/` and `output/users/<id>/`
    /// halves — the resolver returns the parent that contains both,
    /// the per-user share of the project tree, not the project root.
    pub fn writable_root(&self) -> PathBuf {
        // Both .thclaws/users/<id>/ and output/users/<id>/ live under
        // project_root; the user can't escape project_root anyway
        // (existing Sandbox), but per-user write must NOT touch
        // shared assets like AGENTS.md or another user's subtree.
        // The cleanest model: return project_root as "writable",
        // and the check_write_for_user wrapper verifies the resolved
        // path lives under one of the two per-user subtrees. See
        // `Sandbox::check_write_for_user`.
        self.project_root.clone()
    }
}

/// Override the three on-disk roots a [`SharedSessionHandle`]
/// writes into. Single-tenant `--serve` (and desktop GUI / CLI
/// REPL) passes `None` through `spawn_with_approver`, preserving
/// the cwd-relative defaults (`./.thclaws/sessions/`,
/// `~/.config/thclaws/gui-shell/<id>/state/`,
/// `./.thclaws/usage/`). Multi-tenant `--serve` builds one
/// `SessionRoots` per user from [`UserStatePaths`] and threads
/// it in via `spawn_with_roots` so two users hosted in the same
/// pod write to fully separate subtrees.
///
/// The three roots are independent on purpose: a future caller
/// might want per-user JSONLs but a shared storage backend
/// (Tier 2 Redis), or per-user usage but shared JSONLs for a
/// single-tenant team mode. Keep them broken out.
///
/// [`SharedSessionHandle`]: crate::shared_session::SharedSessionHandle
#[derive(Debug, Clone)]
pub struct SessionRoots {
    /// Replaces `SessionStore::default_path()` — the directory the
    /// per-user `SessionStore` writes session JSONLs into.
    pub sessions_dir: PathBuf,
    /// Replaces the default `~/.config/thclaws/gui-shell/<id>/state/`
    /// for `thclaws.storage` writes from a GUI Shell — files land
    /// at `<storage_dir>/<shell_id>/<session_id>.json` instead.
    pub storage_dir: PathBuf,
    /// Replaces `UsageTracker::default_path()` — the directory the
    /// per-user usage tracker writes `<provider>/<model>.json`
    /// aggregates into.
    pub usage_dir: PathBuf,
}

impl SessionRoots {
    /// Derive the three per-user roots from [`UserStatePaths`].
    /// `<user_state>.sessions_dir() / .storage_dir() / .usage_dir()`
    /// — the canonical layout the multi-tenant doc commits to.
    pub fn for_user_state(paths: &UserStatePaths) -> Self {
        Self {
            sessions_dir: paths.sessions_dir(),
            storage_dir: paths.storage_dir(),
            usage_dir: paths.usage_dir(),
        }
    }
}

/// Verify that a path resolved by `Sandbox::check_in(project_root, …)`
/// actually lies under THIS user's permitted write subtrees. Used by
/// `Sandbox::check_write_for_user`.
///
/// Permitted write zones for any user:
/// - `<project>/output/users/<user_id>/...`
/// - `<project>/.thclaws/users/<user_id>/...`
///
/// Everything else (shared `output/`, shared `.thclaws/`, the agent's
/// AGENTS.md / kms/, another user's subtree) is read-only for that
/// user.
pub fn is_in_user_writable(paths: &UserStatePaths, resolved: &Path) -> bool {
    resolved.starts_with(&paths.output_root) || resolved.starts_with(&paths.thclaws_user_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(project: &str, user: &str) -> UserStatePaths {
        UserStatePaths::new(Path::new(project), &UserId::new_for_test(user))
    }

    #[test]
    fn paths_use_user_id_segment() {
        let p = paths("/tmp/proj", "usr_alice");
        assert_eq!(
            p.thclaws_user_root,
            PathBuf::from("/tmp/proj/.thclaws/users/usr_alice")
        );
        assert_eq!(
            p.output_root,
            PathBuf::from("/tmp/proj/output/users/usr_alice")
        );
        assert_eq!(
            p.sessions_dir(),
            PathBuf::from("/tmp/proj/.thclaws/users/usr_alice/sessions")
        );
        assert_eq!(
            p.storage_dir(),
            PathBuf::from("/tmp/proj/.thclaws/users/usr_alice/storage")
        );
        assert_eq!(
            p.usage_dir(),
            PathBuf::from("/tmp/proj/.thclaws/users/usr_alice/usage")
        );
        assert_eq!(
            p.grants_path(),
            PathBuf::from("/tmp/proj/.thclaws/users/usr_alice/grants.json")
        );
    }

    #[test]
    fn is_in_user_writable_accepts_per_user_subtrees() {
        let p = paths("/tmp/proj", "alice");
        assert!(is_in_user_writable(
            &p,
            Path::new("/tmp/proj/output/users/alice/image.png")
        ));
        assert!(is_in_user_writable(
            &p,
            Path::new("/tmp/proj/.thclaws/users/alice/storage/abc.json")
        ));
    }

    #[test]
    fn is_in_user_writable_rejects_shared_or_other_user() {
        let p = paths("/tmp/proj", "alice");
        // Shared (no users/<id>/ segment):
        assert!(!is_in_user_writable(&p, Path::new("/tmp/proj/AGENTS.md")));
        assert!(!is_in_user_writable(
            &p,
            Path::new("/tmp/proj/.thclaws/settings.json")
        ));
        assert!(!is_in_user_writable(
            &p,
            Path::new("/tmp/proj/output/shared.png")
        ));
        // Another user's subtree:
        assert!(!is_in_user_writable(
            &p,
            Path::new("/tmp/proj/output/users/bob/image.png")
        ));
        assert!(!is_in_user_writable(
            &p,
            Path::new("/tmp/proj/.thclaws/users/bob/storage/abc.json")
        ));
    }
}
