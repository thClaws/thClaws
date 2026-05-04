//! thclaws-core: native Rust AI agent workspace library.
//!
//! Module layout follows the phased port plan in `dev-log/007-native-port-plan.md`.
//! Phase 5 lands the foundations: errors, types, config, token estimation.
//! Higher layers (providers, tools, context, agent, repl) land in later phases.

pub mod agent;
pub mod agent_defs;
pub mod branding;
pub mod cancel;
mod cli_completer;
pub mod commands;
pub mod compaction;
pub mod config;
pub mod context;
pub mod dotenv;
pub mod endpoints;
pub mod error;
pub mod event_render;
pub mod external_url;
pub mod file_preview;
pub mod goal_state;
#[cfg(feature = "gui")]
pub mod gui;
pub mod hooks;
pub mod instructions;
pub mod ipc;
pub mod kms;
pub mod marketplace;
pub mod mcp;
pub mod memory;
pub mod model_catalogue;
pub mod oauth;
pub mod permissions;
pub mod plugins;
pub mod policy;
pub mod prompts;
pub mod providers;
pub mod recent_dirs;
pub mod repl;
pub mod sandbox;
pub mod secrets;
pub mod server;
pub mod session;
#[cfg(feature = "gui")]
pub mod shared_session;
pub mod shell_bang;
#[cfg(feature = "gui")]
pub mod shell_dispatch;
pub mod skills;
pub mod sso;
pub mod subagent;
pub mod team;
pub mod theme;
pub mod tokens;
pub mod tools;
pub mod types;
pub mod usage;
pub mod util;
pub mod version;

pub use error::{Error, Result};
