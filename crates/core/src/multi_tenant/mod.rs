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

pub use auth::{verify_user_header, AuthError, UserId, MAX_TIMESTAMP_SKEW_SECS};
pub use metering::{
    from_env as metering_from_env, HttpMeteringSink, MessageEvent, MeteringSink, NoopMeteringSink,
    ProviderCall, StdoutMeteringSink,
};
#[cfg(feature = "gui")]
pub use registry::{RegistryConfig, UserSession, UserSessionRegistry};
pub use user_state::{is_in_user_writable, SessionRoots, UserStatePaths};
