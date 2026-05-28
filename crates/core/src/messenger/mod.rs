//! Facebook Page Messenger bridge — thClaws-side client for the
//! Meta-graph relay (dev-plan/31).
//!
//! Architecture (Tier 1, relay-based — Messenger is webhook-only, so
//! it follows the LINE model, not Telegram's direct long-poll):
//! 1. User pairs their thClaws install with their Facebook Page via
//!    the GUI Messenger Connect modal. The relay (the existing
//!    workspace-only `line-server/` crate, extended with a
//!    `/messenger/webhook` route) hands back a binding JWT we persist
//!    at `~/.config/thclaws/messenger.json`.
//! 2. On startup (CLI `--messenger` flag or GUI modal) we open a
//!    WebSocket to the relay and listen for `WsEnvelope` frames.
//! 3. `UserMessage` envelopes drive an `Agent` turn; the final
//!    assistant text is chunked to Messenger's 2 000-char limit and
//!    shipped back via `POST /reply/<request_id>`, which the relay
//!    turns into a Graph API Send API call (`messaging_type: RESPONSE`).
//! 4. `Postback` envelopes are quick-reply taps for the tool-approval
//!    UX (`BotGated` tool gate).
//!
//! The Page Access Token + App Secret live on the relay (k3s Secret),
//! never on the desktop — see [`config::MessengerConfig`].
//!
//! Not yet wired (next increment): GUI `bootstrap` into
//! `shared_session`, the `--messenger` CLI flag + IPC events, and the
//! relay's broker routing + Send API reply. The client surface here
//! (config/protocol/filter/client/session/approver) is the
//! self-contained Tier 1 foundation.

pub mod approver;
// `bootstrap` wires the bridge into the GUI worker (`shared_session`),
// which is `#[cfg(feature = "gui")]`, so this module is too. The
// headless `--messenger` path needs no worker.
#[cfg(feature = "gui")]
pub mod bootstrap;
pub mod client;
pub mod config;
pub mod filter;
pub mod headless;
pub mod protocol;
pub mod session;

pub use approver::{ApprovalReply, MessengerApprover};
#[cfg(feature = "gui")]
pub use bootstrap::{MessengerSessionHandle, MessengerStatus};
pub use client::{MessengerClient, MessengerClientError};
pub use config::{MessengerConfig, MessengerConfigError};
pub use filter::chunks_for_messenger;
pub use protocol::{QuickReply, ReplyBody, WsEnvelope, WsIncoming};
pub use session::{MessengerMessageHandler, MessengerSession};
