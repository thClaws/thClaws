//! thclaws-core: native Rust AI agent workspace library.
//!
//! Module layout follows the phased port plan in `dev-log/007-native-port-plan.md`.
//! Phase 5 lands the foundations: errors, types, config, token estimation.
//! Higher layers (providers, tools, context, agent, repl) land in later phases.

pub mod agent;
pub mod agent_defs;
pub mod branding;
mod cli_completer;
pub mod commands;
pub mod compaction;
pub mod config;
pub mod context;
pub mod dotenv;
pub mod endpoints;
pub mod error;
#[cfg(feature = "gui")]
pub mod gui;
pub mod hooks;
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
pub mod repl;
pub mod sandbox;
pub mod secrets;
pub mod session;
#[cfg(feature = "gui")]
pub mod shared_session;
#[cfg(feature = "gui")]
pub mod shell_dispatch;
pub mod skills;
pub mod sso;
pub mod subagent;
pub mod team;
pub mod tokens;
pub mod tools;
pub mod types;
pub mod usage;
pub mod util;
pub mod version;

pub use error::{Error, Result};
