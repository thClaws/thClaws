//! GUI Shell subsystem — domain-specific HTML frontends served as
//! sandboxed iframes inside the desktop GUI (Mode A) or as the bound
//! frontend in `--serve` mode (Mode B, Tier 2).
//!
//! Tier 1 scope: embedded built-in shells only, single hardcoded entry
//! in the GUI's new-tab menu, bridge surface limited to
//! `run` / `cancel` / `on("text"|"done"|"error")`. See
//! `dev-plan/33-gui-shell.md` for the full roadmap.

pub mod inline_approval;
pub mod manifest;
pub mod registry;
pub mod router;
pub mod serve;
pub mod shell_cli;
pub mod shell_preview;
pub mod storage;
pub mod team;
pub mod tokens;

pub use manifest::ShellManifest;
pub use registry::{
    project_shell_dir, user_shell_dir, EmbeddedShell, ShellRef, ShellRegistry, ShellSource,
};
pub use router::resolve_default_shell;
pub use tokens::ShellToken;

/// Bridge runtime served at `thclaws://localhost/gui-shell-bridge.js`.
/// Injected into every shell's `<head>` at HTML serve time so authors
/// don't have to ship the bridge themselves.
pub const BRIDGE_RUNTIME: &str = include_str!("../../assets/gui-shell-bridge.js");

/// Shared design-token + chrome stylesheet, injected as an inline
/// `<style>` next to the bridge so shells don't re-declare the palette
/// or `<thc-*>` styling. SSOT for the thClaws GUI-Shell look.
pub const THEME_RUNTIME: &str = include_str!("../../assets/gui-shell-theme.css");

/// Shared chrome runtime (`<thc-header>` web component), injected as an
/// inline `<script>` after the bridge so every shell gets the same
/// navbar/bridge-status/full-screen behaviour for free.
pub const UI_RUNTIME: &str = include_str!("../../assets/gui-shell-ui.js");

/// Build the inline `<style>` + `<script>` block for the shared theme +
/// chrome runtime, escaped so neither can break out of its tag. Injected
/// right after the bridge in every serve path (Mode A/B/C).
pub fn shared_chrome_head() -> String {
    let theme_safe = THEME_RUNTIME.replace("</", "<\\/");
    let ui_safe = UI_RUNTIME.replace("</", "<\\/");
    format!("<style>{theme_safe}</style><script>{ui_safe}</script>")
}
