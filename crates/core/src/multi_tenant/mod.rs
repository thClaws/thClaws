//! dev-plan/35 multi-tenant `--serve` mode.
//!
//! Lets one `thclaws --serve --gui-shell <agent>` process host N
//! users with per-user session isolation, storage, file output,
//! permissions, and metering. End users arrive via a trusted
//! routing layer (typically dev-plan/34 thClaws.cloud) that
//! attaches HMAC-signed `X-Thclaws-User` headers; the pod verifies
//! the signature and routes the request to the corresponding
//! per-user [`SharedSessionHandle`].
//!
//! Single-tenant mode (today's `--serve` behaviour with no
//! `--multi-tenant` flag) is preserved unchanged — this module's
//! types are only constructed when the operator opts in.

pub mod auth;
pub mod member;
pub mod metering;
pub mod user_state;

// registry depends on `crate::shared_session` (gui-gated — the worker
// thread + agent loop only exist when the gui feature is on). The
// other modules (auth, user_state, metering) are always-on because
// `sandbox::check_write_for_user` calls `user_state` from the
// always-on `sandbox` module, and `bin/thclaws-cli` would fail to
// link if any of them were gated. registry is only used from
// `server.rs` (also gui-gated), so gating here is internally
// consistent.
#[cfg(feature = "gui")]
pub mod registry;

pub use auth::{
    verify_identity, verify_user_header, AuthError, IdentityVerifier, UserId,
    MAX_TIMESTAMP_SKEW_SECS,
};
pub use member::{attach_member, current_member_id, scope_member, MEMBER_HEADER};
pub use metering::{
    from_env as metering_from_env, HttpMeteringSink, MessageEvent, MeteringSink, NoopMeteringSink,
    ProviderCall, StdoutMeteringSink,
};
#[cfg(feature = "gui")]
pub use registry::{RegistryConfig, UserSession, UserSessionRegistry};
pub use user_state::{is_in_user_writable, SessionRoots, UserStatePaths};

/// C2: resolve a client-supplied path inside the calling member's own
/// `workspace-<id>/`. Canonicalising, `..`- and symlink-safe (the same
/// [`crate::sandbox::Sandbox::check_in`] the file tools use); another
/// member's folder, the shared pod root and absolute escapes are refused.
pub fn resolve_member_path(
    member_root: &std::path::Path,
    raw: &str,
) -> crate::error::Result<std::path::PathBuf> {
    crate::sandbox::Sandbox::check_in(member_root, raw)
}

#[cfg(test)]
mod member_path_tests {
    use super::resolve_member_path;
    use crate::sandbox::Sandbox;

    fn two_members() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let alice = base.join("workspace-alice");
        let bob = base.join("workspace-bob");
        std::fs::create_dir_all(&alice).unwrap();
        std::fs::create_dir_all(&bob).unwrap();
        std::fs::write(alice.join("notes.md"), "a").unwrap();
        std::fs::write(bob.join("secret.md"), "b").unwrap();
        std::fs::write(base.join(".env"), "k").unwrap();
        (tmp, alice, bob)
    }

    #[test]
    fn multiuser_member_path_refuses_other_member_and_dotdot() {
        let (_tmp, alice, bob) = two_members();
        assert_eq!(
            resolve_member_path(&alice, "notes.md").unwrap(),
            alice.join("notes.md")
        );
        for bad in [
            "../workspace-bob/secret.md",
            "../.env",
            "sub/../../workspace-bob/secret.md",
            bob.join("secret.md").to_str().unwrap(),
        ] {
            assert!(
                resolve_member_path(&alice, bad).is_err(),
                "{bad} must be refused"
            );
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&bob, alice.join("peek")).unwrap();
            assert!(resolve_member_path(&alice, "peek/secret.md").is_err());
        }
    }

    /// The IPC path: a WS frame runs under `sync_scope_workdir(member_root)`,
    /// so `Sandbox::check` (file_read/file_write/file_list) is bounded the same way.
    #[test]
    fn multiuser_ipc_scope_bounds_sandbox_check() {
        let (_tmp, alice, bob) = two_members();
        crate::workdir::sync_scope_workdir(alice.clone(), || {
            assert_eq!(Sandbox::check("notes.md").unwrap(), alice.join("notes.md"));
            assert!(Sandbox::check("../workspace-bob/secret.md").is_err());
            assert!(Sandbox::check(bob.join("secret.md").to_str().unwrap()).is_err());
            assert!(Sandbox::check("../.env").is_err());
        });
    }
}
