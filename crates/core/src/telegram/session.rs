//! Bridge between the Telegram polling client and the agent loop.
//!
//! [`TelegramSession`] is what the worker spawns once a bot token is
//! configured. It owns a [`TelegramClient`] and routes each polled
//! [`Update`] through:
//!
//! 1. **callback_query** → resolve a pending tool approval
//!    ([`TelegramApprover`]), `answerCallbackQuery`, edit the prompt.
//! 2. **message** → authorize (DM allowlist / pairing; group allowlist),
//!    then either run the pairing flow or forward the text to the
//!    [`TelegramMessageHandler`] and ship the reply back (HTML-escaped +
//!    chunked).
//!
//! Tier 1 model (decision #7): all *authorized* chats forward into the
//! single shared worker session — there is no per-chat agent isolation
//! yet (that's Tier 2 forum-topic routing). [`ChatRegistry`] tracks
//! per-`chat_id` liveness for the status pill and 24h idle GC, and the
//! pairing flow gates *who* may reach the agent, so in practice a Tier 1
//! deployment is single-owner. Concurrency note: `active_chat` on the
//! approver is last-writer-wins, which is correct for the single-owner
//! case and revisited when per-chat sessions land.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::approver::{ApprovalReply, TelegramApprover};
use super::client::{TelegramClient, TelegramClientError, TelegramUpdateSink};
use super::config::{DmPolicy, TelegramConfig};
use super::pairing::PairingManager;
use super::protocol::{
    AnswerCallbackQuery, CallbackQuery, Chat, ChatKind, EditMessageText, Message, Update,
};
use super::stream::{PreviewSink, TelegramPreview};
use super::{channel, topic};

/// Idle window after which a chat's registry entry is GC'd (decision #7).
pub const IDLE_GC: Duration = Duration::from_secs(24 * 60 * 60);

/// Cap on a downloaded attachment. It becomes base64 inside the prompt,
/// so an oversized photo costs tokens and can trip the provider's
/// request limit long before it helps the user.
pub const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;

/// `file_id` → `(media_type, base64)`, ready for the multipart turn.
async fn fetch_photo(
    client: &TelegramClient,
    file_id: &str,
) -> Result<(String, String), TelegramClientError> {
    let file = client.get_file(file_id).await?;
    let path = file
        .file_path
        .ok_or_else(|| TelegramClientError::Http("getFile returned no file_path".to_string()))?;
    let bytes = client.download_file(&path, MAX_ATTACHMENT_BYTES).await?;
    let media_type = media_type_for(&path);
    Ok((media_type, base64_encode(&bytes)))
}

/// Telegram serves photos as `.jpg` but re-encodes some uploads; go by
/// the path it hands back rather than assuming.
fn media_type_for(file_path: &str) -> String {
    let ext = file_path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/jpeg",
    }
    .to_string()
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// What to do when an authorized Telegram user sends text. The worker
/// provides the concrete impl (forwards into the shared session).
#[async_trait]
pub trait TelegramMessageHandler: Send + Sync + 'static {
    /// Run one inbound text as an agent turn; return the final assistant
    /// text. `None` skips the Telegram reply. `agent_id` (Tier 2) is the
    /// forum-topic-routed AgentDef name to run as, or `None` for the
    /// default agent — implementers that can't route per-agent (e.g. the
    /// single-session GUI worker) ignore it. `preview` (Tier 3.1), when
    /// `Some`, receives the full assistant text on each delta so the
    /// implementer can stream a live in-place edit; implementers that
    /// can't stream ignore it and the sink sends the returned final text.
    async fn handle_message(
        &self,
        text: String,
        agent_id: Option<String>,
        preview: Option<Arc<dyn PreviewSink>>,
    ) -> Option<String>;

    /// Same, but the turn carries images (Tier 3 media — public issue
    /// #187, reported by HelloMAF). Each attachment is
    /// `(media_type, base64_data)`, matching `ShellInput::LineWithImages`.
    ///
    /// The default drops the attachments and runs the caption alone,
    /// which is what an implementer without a multimodal path can
    /// honestly do — but it says so in the reply rather than going
    /// silent, because a photo vanishing with no feedback at all is the
    /// bug being fixed.
    async fn handle_message_with_images(
        &self,
        text: String,
        images: Vec<(String, String)>,
        agent_id: Option<String>,
        preview: Option<Arc<dyn PreviewSink>>,
    ) -> Option<String> {
        let _ = &images;
        let reply = self.handle_message(text, agent_id, preview).await;
        Some(match reply {
            Some(r) => format!("{r}\n\n_(the image didn't reach the model — this session can't take attachments yet)_"),
            None => "I can't read images in this session yet — send the question as text.".to_string(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ChatState {
    pub kind: ChatKind,
    pub last_active: Instant,
    pub message_count: u64,
}

/// `chat_id → ChatState` map with idle GC. Drives the GUI status pill's
/// "active chats / messages" counts.
pub struct ChatRegistry {
    chats: Mutex<HashMap<i64, ChatState>>,
    idle: Duration,
}

impl Default for ChatRegistry {
    fn default() -> Self {
        Self::new(IDLE_GC)
    }
}

impl ChatRegistry {
    pub fn new(idle: Duration) -> Self {
        Self {
            chats: Mutex::new(HashMap::new()),
            idle,
        }
    }

    /// Record activity for a chat, bumping its message count and
    /// last-active stamp. GCs idle entries as a side effect so the map
    /// can't grow unbounded across many short-lived group chats.
    pub fn touch(&self, chat_id: i64, kind: ChatKind) {
        let Ok(mut g) = self.chats.lock() else { return };
        let now = Instant::now();
        g.retain(|_, s| now.duration_since(s.last_active) < self.idle);
        let entry = g.entry(chat_id).or_insert(ChatState {
            kind,
            last_active: now,
            message_count: 0,
        });
        entry.kind = kind;
        entry.last_active = now;
        entry.message_count += 1;
    }

    /// Count of chats active within the idle window.
    pub fn active_count(&self) -> usize {
        let Ok(mut g) = self.chats.lock() else {
            return 0;
        };
        let now = Instant::now();
        g.retain(|_, s| now.duration_since(s.last_active) < self.idle);
        g.len()
    }

    /// Total messages seen across all live chats.
    pub fn total_messages(&self) -> u64 {
        self.chats
            .lock()
            .map(|g| g.values().map(|s| s.message_count).sum())
            .unwrap_or(0)
    }
}

pub struct TelegramSession {
    client: Arc<TelegramClient>,
    handler: Arc<dyn TelegramMessageHandler>,
    approver: Option<Arc<TelegramApprover>>,
    pairing: Arc<PairingManager>,
    /// Shared with the worker: pairing approval mutates `allow_from`
    /// here (and persists), and the sink reads it for authorization.
    config: Arc<Mutex<TelegramConfig>>,
    registry: Arc<ChatRegistry>,
    output_ceiling: u32,
}

impl TelegramSession {
    pub fn new(
        client: Arc<TelegramClient>,
        handler: Arc<dyn TelegramMessageHandler>,
        config: Arc<Mutex<TelegramConfig>>,
        pairing: Arc<PairingManager>,
    ) -> Self {
        let output_ceiling = config
            .lock()
            .map(|c| c.output_ceiling)
            .unwrap_or(super::config::DEFAULT_OUTPUT_CEILING);
        Self {
            client,
            handler,
            approver: None,
            pairing,
            config,
            registry: Arc::new(ChatRegistry::default()),
            output_ceiling,
        }
    }

    pub fn with_approver(mut self, approver: Arc<TelegramApprover>) -> Self {
        self.approver = Some(approver);
        self
    }

    pub fn registry(&self) -> Arc<ChatRegistry> {
        self.registry.clone()
    }

    /// Drive the polling loop forever (until cancelled / fatal error).
    pub async fn run(self: Arc<Self>) -> Result<(), TelegramClientError> {
        let sink = SessionSink {
            client: self.client.clone(),
            handler: self.handler.clone(),
            approver: self.approver.clone(),
            pairing: self.pairing.clone(),
            config: self.config.clone(),
            registry: self.registry.clone(),
            output_ceiling: self.output_ceiling,
        };
        self.client.run(sink).await
    }
}

struct SessionSink {
    client: Arc<TelegramClient>,
    handler: Arc<dyn TelegramMessageHandler>,
    approver: Option<Arc<TelegramApprover>>,
    pairing: Arc<PairingManager>,
    config: Arc<Mutex<TelegramConfig>>,
    registry: Arc<ChatRegistry>,
    output_ceiling: u32,
}

impl SessionSink {
    /// True when `user_id` may DM without pairing.
    fn dm_authorized(&self, user_id: i64) -> bool {
        self.config
            .lock()
            .map(|c| c.allows_dm(user_id))
            .unwrap_or(false)
    }

    fn dm_policy(&self) -> DmPolicy {
        self.config.lock().map(|c| c.dm_policy).unwrap_or_default()
    }

    fn may_approve(&self, user_id: Option<i64>) -> bool {
        self.config
            .lock()
            .map(|c| c.may_approve(user_id))
            .unwrap_or(false)
    }

    fn group_authorized(&self, chat_id: i64) -> bool {
        self.config
            .lock()
            .map(|c| c.allows_group(chat_id))
            .unwrap_or(false)
    }

    /// Resolve the forum-topic-routed agent for `(chat_id, topic)` from
    /// the shared config. `None` ⇒ the handler's default agent.
    fn route_for(&self, chat_id: i64, topic: Option<i64>) -> Option<String> {
        self.config
            .lock()
            .ok()
            .and_then(|c| topic::resolve_agent(&c, chat_id, topic))
    }

    fn stream_preview_enabled(&self) -> bool {
        self.config
            .lock()
            .map(|c| c.stream_preview)
            .unwrap_or(false)
    }

    /// Forward an authorized text to the agent and ship the reply back
    /// into the same forum topic. Spawned so the polling loop never
    /// blocks on a turn. `agent_id` (Tier 2) routes the turn to a
    /// per-topic agent; when `stream_preview` is on (Tier 3.1) the reply
    /// is streamed as an in-place edit, otherwise sent once at the end.
    fn spawn_turn(
        &self,
        chat_id: i64,
        topic: Option<i64>,
        agent_id: Option<String>,
        text: String,
        photo_file_id: Option<String>,
    ) {
        if let Some(approver) = &self.approver {
            approver.set_active_chat(chat_id);
        }
        let handler = self.handler.clone();
        let client = self.client.clone();
        let ceiling = self.output_ceiling;
        let streaming = self.stream_preview_enabled();
        tokio::spawn(async move {
            let preview = streaming.then(|| {
                Arc::new(TelegramPreview::new(
                    client.clone(),
                    chat_id,
                    topic,
                    ceiling,
                ))
            });
            let preview_sink: Option<Arc<dyn PreviewSink>> =
                preview.clone().map(|p| p as Arc<dyn PreviewSink>);

            // Fetch attachments before the turn starts. A failure here
            // must not swallow the message the way the old early-return
            // did — fall through to a text-only turn and say why.
            let mut images: Vec<(String, String)> = Vec::new();
            let mut fetch_note: Option<String> = None;
            if let Some(file_id) = photo_file_id {
                match fetch_photo(&client, &file_id).await {
                    Ok(img) => images.push(img),
                    Err(e) => {
                        eprintln!("[telegram] photo {file_id} not fetched: {e}");
                        fetch_note = Some(format!("(couldn't download the image: {e})"));
                    }
                }
            }

            let reply = if images.is_empty() {
                handler.handle_message(text, agent_id, preview_sink).await
            } else {
                handler
                    .handle_message_with_images(text, images, agent_id, preview_sink)
                    .await
            };
            let Some(reply) = reply.map(|r| match &fetch_note {
                Some(note) => format!("{r}\n\n{note}"),
                None => r,
            }) else {
                return;
            };

            match preview {
                // Streaming: swap the live preview for the final reply.
                Some(p) => p.finish(&reply).await,
                // Non-streaming: one send into the originating topic
                // (General-topic quirk applied by `send_to_topic`).
                None => {
                    if let Err(e) =
                        channel::send_to_topic(&client, chat_id, topic, &reply, ceiling).await
                    {
                        eprintln!("[telegram] reply send failed (chat {chat_id}): {e}");
                    }
                }
            }
        });
    }

    /// First-contact pairing: mint/get a code, DM it to the user, and
    /// leave the request pending for the owner to approve in the GUI.
    fn spawn_pairing_prompt(&self, chat_id: i64, user_id: i64, display: String) {
        let pair = self.pairing.mint(user_id, chat_id, display);
        let client = self.client.clone();
        let code = pair.code.clone();
        tokio::spawn(async move {
            let body = format!(
                "👋 You're not paired with this thClaws yet.\n\nYour pairing code is <b>{code}</b>. \
                 Ask the thClaws owner to approve it in the desktop app. \
                 This code expires in 1 hour.",
            );
            if let Err(e) = client
                .send_message(&super::protocol::SendMessage::text(chat_id, body))
                .await
            {
                eprintln!("[telegram] pairing prompt send failed (chat {chat_id}): {e}");
            }
        });
    }

    /// Resolve a tapped inline-keyboard button: stop the spinner, edit
    /// the prompt to the chosen verdict, and unblock the waiting turn.
    fn handle_callback(&self, cbq: CallbackQuery) {
        let Some(approver) = self.approver.clone() else {
            return;
        };
        if !self.may_approve(Some(cbq.from.id)) {
            let client = self.client.clone();
            tokio::spawn(async move {
                let _ = client
                    .answer_callback_query(&AnswerCallbackQuery::with_toast(
                        cbq.id,
                        "Only an approved user can answer this.",
                    ))
                    .await;
            });
            return;
        }
        let data = cbq.data.clone().unwrap_or_default();
        // Resolve synchronously — this is what UNBLOCKS the agent turn
        // awaiting approval. Network confirmation (answer + edit) is
        // spawned so on_update returns promptly.
        let resolved = approver.record_decision_from_callback(&data);
        let client = self.client.clone();
        tokio::spawn(async move {
            let (toast, verdict_line) = match resolved {
                Some((ApprovalReply::Allow, _)) => ("Approved", "✅ Approved — running now."),
                Some((ApprovalReply::AllowAlways, _)) => (
                    "Always allowed",
                    "♾️ Approved for the rest of this session.",
                ),
                Some((ApprovalReply::Deny, _)) => ("Denied", "🚫 Denied — tool will not run."),
                _ => ("Expired", "This approval is no longer pending."),
            };
            let _ = client
                .answer_callback_query(&AnswerCallbackQuery::with_toast(cbq.id, toast))
                .await;
            // Edit the prompt message in place so the buttons disappear
            // and the verdict is shown. The CallbackQuery carries the
            // message to edit — no stored state needed.
            if let Some(msg) = cbq.message {
                let edit = EditMessageText::new(msg.chat.id, msg.message_id, verdict_line);
                if let Err(e) = client.edit_message_text(&edit).await {
                    eprintln!("[telegram] edit after approval failed: {e}");
                }
            }
        });
    }

    fn handle_message(&self, msg: Message) {
        let chat: Chat = msg.chat.clone();
        self.registry.touch(chat.id, chat.kind);

        // Tier 2: forum-topic lifecycle service messages (created/edited/
        // closed/reopened) carry no text and must NOT reach the agent —
        // just acknowledge them so the polling loop never trips.
        if let Some(ev) = msg.forum_topic_event() {
            eprintln!("[telegram] forum topic event {ev:?} in chat {}", chat.id);
            return;
        }

        // A photo arrives with its words in `caption`, not `text` — so
        // this used to look like a textless message and get dropped
        // without a reply or a log line (public issue #187). Voice,
        // documents and video are still out of scope and still fall
        // through the `else` below.
        let photo_file_id = msg.largest_photo().map(|p| p.file_id.clone());
        let Some(text) = msg.text_or_caption().map(str::to_string).or_else(|| {
            photo_file_id
                .as_ref()
                .map(|_| "What is in this image?".to_string())
        }) else {
            return;
        };

        // Effective forum topic + per-topic agent routing (Tier 2).
        let topic =
            topic::effective_topic_id(msg.message_thread_id, msg.is_topic_message.unwrap_or(false));

        match chat.kind {
            ChatKind::Private => {
                let user_id = msg.from.as_ref().map(|u| u.id).unwrap_or(chat.id);
                if self.dm_authorized(user_id) {
                    self.route_authorized_text(
                        chat.id,
                        None,
                        None,
                        Some(user_id),
                        text,
                        photo_file_id,
                    );
                    return;
                }
                match self.dm_policy() {
                    DmPolicy::Pairing => {
                        let display = msg
                            .from
                            .as_ref()
                            .map(|u| u.display())
                            .unwrap_or_else(|| format!("id {user_id}"));
                        self.spawn_pairing_prompt(chat.id, user_id, display);
                    }
                    DmPolicy::Allowlist => {
                        // Silent ignore — no pairing prompt under
                        // allowlist policy (decision: unknown senders
                        // get no signal the bot exists).
                        eprintln!("[telegram] ignoring DM from unallowlisted user {user_id}");
                    }
                }
            }
            ChatKind::Group | ChatKind::Supergroup => {
                // Serve if the group is allowlisted OR it's the linked
                // discussion group of a configured channel (Tier 2 —
                // comments on channel posts land here).
                let is_channel_surface = self
                    .config
                    .lock()
                    .map(|c| c.is_channel_surface(chat.id))
                    .unwrap_or(false);
                if self.group_authorized(chat.id) || is_channel_surface {
                    let agent_id = self.route_for(chat.id, topic);
                    let sender = msg.from.as_ref().map(|u| u.id);
                    self.route_authorized_text(
                        chat.id,
                        topic,
                        agent_id,
                        sender,
                        text,
                        photo_file_id,
                    );
                } else {
                    eprintln!(
                        "[telegram] ignoring message from unallowlisted group {}",
                        chat.id
                    );
                }
            }
            ChatKind::Channel => {
                // Posts authored in the channel itself arrive as
                // `channel_post` (handled in `on_update`); a `message`
                // with channel kind is unusual — ignore.
            }
        }
    }

    /// Authorized text: either resolve a pending approval (free-text
    /// fallback) or run a turn. Mirrors the LINE sink's short-circuit so
    /// a typed "approve"/"deny" answers the gate instead of starting a
    /// new turn. `topic` / `agent_id` carry Tier 2 forum-topic routing.
    fn route_authorized_text(
        &self,
        chat_id: i64,
        topic: Option<i64>,
        agent_id: Option<String>,
        sender: Option<i64>,
        text: String,
        photo_file_id: Option<String>,
    ) {
        if let Some(approver) = &self.approver {
            if approver.has_pending() && self.may_approve(sender) {
                if let Some(reply) = approver.record_decision_from_text(&text) {
                    let msg = match reply {
                        ApprovalReply::Allow => "✅ Approved — running now.",
                        ApprovalReply::AllowAlways => "♾️ Approved for the session.",
                        ApprovalReply::Deny => "🚫 Denied.",
                        ApprovalReply::Unrecognised => {
                            "Reply with the buttons above, or type approve / deny."
                        }
                    };
                    let client = self.client.clone();
                    let msg = msg.to_string();
                    let ceiling = self.output_ceiling;
                    tokio::spawn(async move {
                        let _ =
                            channel::send_to_topic(&client, chat_id, topic, &msg, ceiling).await;
                    });
                    return;
                }
            }
        }
        self.spawn_turn(chat_id, topic, agent_id, text, photo_file_id);
    }
}

#[async_trait]
impl TelegramUpdateSink for SessionSink {
    async fn on_update(&self, update: Update) {
        if let Some(cbq) = update.callback_query {
            self.handle_callback(cbq);
            return;
        }
        if let Some(msg) = update.incoming_message() {
            self.handle_message(msg.clone());
            return;
        }
        // Tier 2: a `channel_post` is the bot's (or an admin's) broadcast
        // in a channel. The agent drives turns from the linked discussion
        // group, not from channel posts themselves — acknowledge so the
        // poll loop advances, but don't run a turn.
        if let Some(post) = &update.channel_post {
            self.registry.touch(post.chat.id, post.chat.kind);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Telegram hands back a path, not a content type — and it doesn't
    /// always re-encode to JPEG. Guessing wrong means the provider gets
    /// a media_type that doesn't match the bytes.
    #[test]
    fn media_type_follows_the_returned_path() {
        assert_eq!(media_type_for("photos/file_7.jpg"), "image/jpeg");
        assert_eq!(media_type_for("photos/file_7.JPG"), "image/jpeg");
        assert_eq!(media_type_for("photos/file_7.png"), "image/png");
        assert_eq!(media_type_for("photos/file_7.webp"), "image/webp");
        assert_eq!(media_type_for("photos/file_7.gif"), "image/gif");
        // No extension at all → the format Telegram serves by default.
        assert_eq!(media_type_for("photos/file_7"), "image/jpeg");
    }

    #[test]
    fn base64_round_trips() {
        use base64::Engine as _;
        let raw = b"\x89PNG\r\n\x1a\n";
        let encoded = base64_encode(raw);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .unwrap();
        assert_eq!(decoded, raw);
    }

    #[test]
    fn registry_counts_and_gcs() {
        let reg = ChatRegistry::new(Duration::from_secs(3600));
        reg.touch(111, ChatKind::Private);
        reg.touch(111, ChatKind::Private);
        reg.touch(-100, ChatKind::Group);
        assert_eq!(reg.active_count(), 2);
        assert_eq!(reg.total_messages(), 3);
    }

    #[test]
    fn registry_gc_drops_idle_entries() {
        // Zero idle window ⇒ every prior entry is immediately stale, so
        // a later touch sees only itself.
        let reg = ChatRegistry::new(Duration::ZERO);
        reg.touch(111, ChatKind::Private);
        // With ZERO idle, the entry is considered expired on the next
        // observation.
        assert_eq!(reg.active_count(), 0);
    }

    // ── Tier 2: discussion-group ingest + per-topic routing ──

    use crate::telegram::config::{TelegramChannelConfig, TelegramConfig, TopicRoute};
    use std::collections::HashMap;

    /// Records each `(text, agent_id)` the sink routes; returns `None` so
    /// `spawn_turn` never attempts a (network) reply.
    struct RecordingHandler {
        tx: tokio::sync::mpsc::UnboundedSender<(String, Option<String>)>,
    }

    #[async_trait]
    impl TelegramMessageHandler for RecordingHandler {
        async fn handle_message(
            &self,
            text: String,
            agent_id: Option<String>,
            _preview: Option<Arc<dyn PreviewSink>>,
        ) -> Option<String> {
            let _ = self.tx.send((text, agent_id));
            None
        }
    }

    fn channel_cfg() -> TelegramConfig {
        // Channel -100123 ↔ discussion group -100987; topic 42 → coder,
        // General (1) → research; channel default → research.
        let mut topic_routing = HashMap::new();
        topic_routing.insert(
            "42".to_string(),
            TopicRoute {
                agent_id: Some("coder".into()),
            },
        );
        let mut channels = HashMap::new();
        channels.insert(
            "-100123".to_string(),
            TelegramChannelConfig {
                linked_discussion_group: Some("-100987".into()),
                agent_id: Some("research".into()),
                topic_routing,
            },
        );
        TelegramConfig {
            channels,
            ..Default::default()
        }
    }

    fn sink_with(
        cfg: TelegramConfig,
    ) -> (
        SessionSink,
        tokio::sync::mpsc::UnboundedReceiver<(String, Option<String>)>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = SessionSink {
            client: Arc::new(TelegramClient::new("1:aaaaaaaaaaaaaaaaaaaaaa")),
            handler: Arc::new(RecordingHandler { tx }),
            approver: None,
            pairing: Arc::new(PairingManager::new()),
            config: Arc::new(Mutex::new(cfg)),
            registry: Arc::new(ChatRegistry::default()),
            output_ceiling: 4000,
        };
        (sink, rx)
    }

    fn supergroup_msg(chat_id: i64, thread: Option<i64>, is_topic: bool, text: &str) -> Message {
        let thread_json = match thread {
            Some(t) => format!(r#","message_thread_id":{t}"#),
            None => String::new(),
        };
        let json = format!(
            r#"{{"message_id":1,"chat":{{"id":{chat_id},"type":"supergroup"}},"text":"{text}","is_topic_message":{is_topic}{thread_json}}}"#
        );
        serde_json::from_str(&json).unwrap()
    }

    #[tokio::test]
    async fn discussion_group_reply_routes_to_topic_agent() {
        // Acceptance #2 + #3: a reply in the linked discussion group
        // reaches the agent, routed by forum topic.
        let (sink, mut rx) = sink_with(channel_cfg());

        // Topic 42 → coder.
        sink.handle_message(supergroup_msg(-100987, Some(42), true, "build it"));
        let (text, agent) = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("handler called")
            .unwrap();
        assert_eq!(text, "build it");
        assert_eq!(agent.as_deref(), Some("coder"));

        // General topic (no thread id, flagged) → research.
        sink.handle_message(supergroup_msg(-100987, None, true, "status?"));
        let (_t, agent) = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("handler called")
            .unwrap();
        assert_eq!(agent.as_deref(), Some("research"));
    }

    #[tokio::test]
    async fn forum_topic_service_message_is_ignored() {
        // Acceptance #5: a forum_topic_created service message must not
        // reach the agent (and must not crash).
        let (sink, mut rx) = sink_with(channel_cfg());
        let json = r#"{"message_id":2,"chat":{"id":-100987,"type":"supergroup"},"message_thread_id":42,"forum_topic_created":{"name":"Coder","icon_color":7322096}}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        sink.handle_message(msg);
        // Nothing should be routed within a short window.
        let got = tokio::time::timeout(std::time::Duration::from_millis(150), rx.recv()).await;
        assert!(got.is_err(), "service message should not reach the agent");
    }

    #[tokio::test]
    async fn unconfigured_group_is_ignored() {
        // A supergroup that's neither allowlisted nor a channel surface
        // gets dropped.
        let (sink, mut rx) = sink_with(channel_cfg());
        sink.handle_message(supergroup_msg(-555555, Some(7), true, "hello"));
        let got = tokio::time::timeout(std::time::Duration::from_millis(150), rx.recv()).await;
        assert!(got.is_err(), "unconfigured group should be ignored");
    }
}
