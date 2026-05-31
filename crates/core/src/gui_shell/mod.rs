//! GUI Shell subsystem — domain-specific HTML frontends served as
//! sandboxed iframes inside the desktop GUI (Mode A) or as the bound
//! frontend in `--serve` mode (Mode B, Tier 2).
//!
//! Tier 1 scope: embedded built-in shells only, single hardcoded entry
//! in the GUI's new-tab menu, bridge surface limited to
//! `run` / `cancel` / `on("text"|"done"|"error")`. See
//! `dev-plan/33-gui-shell.md` for the full roadmap.

pub mod manifest;
pub mod registry;
pub mod serve;
pub mod storage;
pub mod tokens;

pub use manifest::ShellManifest;
pub use registry::{EmbeddedShell, ShellRef, ShellRegistry, ShellSource};
pub use tokens::ShellToken;

/// Bridge runtime served at `thclaws://localhost/gui-shell-bridge.js`.
/// Injected into every shell's `<head>` at HTML serve time so authors
/// don't have to ship the bridge themselves.
pub const BRIDGE_RUNTIME: &str = include_str!("../../assets/gui-shell-bridge.js");
