//! thclaws-core: native Rust AI agent workspace library.
//!
//! Module layout follows the phased port plan in `dev-log/007-native-port-plan.md`.
//! Phase 5 lands the foundations: errors, types, config, token estimation.
//! Higher layers (providers, tools, context, agent, repl) land in later phases.

pub mod agent;
pub mod agent_defs;
/// Workspace-scoped agent runtime builder. Used by the HTTP
/// `/agent/run` endpoint to construct a per-request `Agent` parameterized
/// by an explicit workspace directory (see
/// `dev-plan/25-thclaws-as-agent.md`).
pub mod agent_runtime;
/// OpenAI-compatible HTTP API surface mounted on `--serve` (see
/// `dev-plan/19-thclaws-openai-compat.md`).
pub mod api_v1;
/// Self-improving AI Agent — auto-learn pipeline that files each
/// ended session as a page in a dedicated KMS and periodically
/// reconciles it. See `dev-plan/27-self-improving-agent.md`.
pub mod auto_learn;
pub mod branding;
pub mod cancel;
mod cli_completer;
/// ChatGPT/Codex OAuth token model (ported from themion).
pub mod codex_auth;
/// ChatGPT/Codex auth file persistence under `~/.config/thclaws/auth/`.
pub mod codex_auth_store;
pub mod commands;
pub mod compaction;
pub mod config;
pub mod context;
#[cfg(feature = "cost_bridge")]
pub mod cost_bridge;
pub mod deploy_client;
pub mod dotenv;
pub mod endpoints;
pub mod error;
// event_render, ipc, server, file_preview all transitively depend on
// crate::shared_session (which is gui-gated below) and/or `comrak`
// (also gui-gated in Cargo.toml). M6.36 SERVE9 introduced them as
// always-on by mistake; gate them behind the same `gui` feature so
// the CLI-only thclaws-cli binary still builds.
#[cfg(feature = "gui")]
pub mod event_render;
pub mod external_url;
#[cfg(feature = "gui")]
pub mod file_preview;
pub mod goal_state;
#[cfg(feature = "gui")]
pub mod gui;
pub mod hooks;
pub mod instructions;
#[cfg(feature = "gui")]
pub mod ipc;
pub mod kms;
pub mod line;
/// ChatGPT OAuth device-code flow for the `chatgpt-codex` provider.
pub mod login_codex;
pub mod marketplace;
pub mod mcp;
pub mod memory;
pub mod messenger;
pub mod model_catalogue;
pub mod oauth;
pub mod permissions;
pub mod plugins;
pub mod policy;
pub mod prompts;
pub mod providers;
pub mod recent_dirs;
pub mod remote_agent;
pub mod repl;
pub mod research;
pub mod sandbox;
pub mod schedule;
pub mod schedule_presets;
pub mod sdk_mcp;
pub mod secrets;
#[cfg(feature = "gui")]
pub mod server;
pub mod session;
#[cfg(feature = "gui")]
pub mod shared_session;
pub mod shell_bang;
#[cfg(feature = "gui")]
pub mod shell_dispatch;
#[cfg(feature = "gui")]
pub mod side_channel;
pub mod skills;
pub mod skills_state;
pub mod sso;
pub mod subagent;
pub mod team;
pub mod telegram;
pub mod theme;
pub mod tokens;
pub mod tool_display;
pub mod tools;
pub mod types;
pub mod uploads;
pub mod usage;
pub mod util;
pub mod version;

pub use error::{Error, Result};
