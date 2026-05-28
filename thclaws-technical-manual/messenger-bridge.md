# Messenger Bridge (dev-plan/31)

Facebook Page Messenger ↔ thClaws desktop bridge: a user DMs a paired Facebook Page, the agent runs on the operator's local machine, and the LINE relay (extended with a `/messenger/webhook` route + Graph Send API client) routes messages between the Meta-graph and the desktop's WebSocket.

| Layer | Lives at | Role |
|---|---|---|
| Client adapter | `crates/core/src/messenger/` | WS client + reply-sender + `MessengerApprover` + binding-JWT config + filter |
| GUI worker integration | `crates/core/src/shared_session.rs` `ShellInput::Messenger*` arms | drives `Agent::run_turn` per inbound message; permission swap to `MessengerGated` |
| GUI bootstrap | `crates/core/src/messenger/bootstrap.rs` (`#[cfg(feature = "gui")]`) | forwards messages into the worker via `ShellInput::MessengerMessage`; manages `MessengerSessionHandle` |
| Headless loop | `crates/core/src/messenger/headless.rs` | standalone agent loop for `thclaws --messenger` (no GUI) |
| IPC | `crates/core/src/ipc.rs` `messenger_*` arms | status / pair / disconnect |
| Frontend modal | `frontend/src/components/MessengerConnectModal.tsx` | DM-prompt pairing → exchange code → store binding JWT |
| Sidebar pill | `frontend/src/components/Sidebar.tsx` | Page name + bridge state |
| Relay routes | `crates/line-server/src/routes/messenger.rs` | `webhook` (Meta ingest) + `reply` (desktop outbound) + `push` (admin) |
| Relay Meta-graph | `crates/line-server/src/meta/` | `X-Hub-Signature-256` verify + Send API client + event deserialisation |

## Why a relay (vs Telegram)

Messenger is **webhook-only** — Meta will only deliver events by POSTing to a publicly reachable HTTPS endpoint, so a desktop client behind NAT cannot receive without a server in front. This puts Messenger in the same architectural slot as LINE: the relay (`crates/line-server/`) holds the Meta-graph credentials and demuxes events to per-Page WebSocket subscribers. Tier 1 deliberately reuses the deployed LINE relay (`line.thclaws.ai`) so there is no separate `messenger-server` crate. The rename to a neutral gateway host is dev-plan/31 open-question #1, deferred to Tier 3.

## Module layout

```
crates/core/src/messenger/
├── mod.rs        public exports; gui feature gate on bootstrap
├── protocol.rs   WS wire types (WsEnvelope::{UserMessage,Postback,Notice}) + ReplyBody + QuickReply
├── config.rs     MessengerConfig + ~/.config/thclaws/messenger.json; binding JWT + relay URL + page id/name cache
├── client.rs     WebSocket client (connect+reconnect with backoff); POST /messenger/reply/{id} sender
├── filter.rs     chunks_for_messenger() — clean_for_stream() reuse + 1,900-char window under the 2,000-hard cap
├── session.rs    MessengerSession + MessengerMessageHandler trait; quick-reply / postback approver fallthrough
├── approver.rs   MessengerApprover: quick-reply approval, callback payload state machine (`tool:<verb>:<id>`)
├── bootstrap.rs  GUI worker wiring (#[cfg(feature = "gui")])
└── headless.rs   standalone agent loop for --messenger
```

Relay-side (workspace-only crate, NOT in the public mirror):

```
crates/line-server/src/
├── routes/messenger.rs   /messenger/{webhook,reply,push} handlers + pairing onboarding via Send API
├── meta/
│   ├── mod.rs            module glue + shared types
│   ├── signature.rs      X-Hub-Signature-256 verify (constant-time HMAC-SHA256 over raw body)
│   ├── send.rs           Graph Send API client (POST /v19.0/me/messages with messaging_type)
│   └── events.rs         deserialise Meta webhook payload (object/entry/messaging/{sender.id,message.text,postback.payload})
├── broker.rs             PSID-keyed inbound routing (adds messenger_user_message / messenger_postback variants)
├── config.rs             MESSENGER_* env vars + Page subject convention
├── state.rs              add Messenger bindings keyed by `fb:<pageId>`
├── store.rs              persistence for Page bindings + pairing codes
└── main.rs               wires routes/messenger.rs into the Axum router
```

## Wire protocol

### Meta → relay: `POST /messenger/webhook`

Meta delivers events per [the Messenger Platform spec](https://developers.facebook.com/docs/messenger-platform/reference/webhook-events/messages):

```json
{
  "object": "page",
  "entry": [{
    "id": "<PAGE_ID>",
    "time": 1735689600000,
    "messaging": [{
      "sender":    { "id": "<PSID>" },
      "recipient": { "id": "<PAGE_ID>" },
      "timestamp": 1735689600000,
      "message":   { "mid": "m_xyz", "text": "hi" }
    }]
  }]
}
```

Handler flow (`routes/messenger.rs`):

1. **Verify handshake** — `GET /messenger/webhook?hub.mode=subscribe&hub.challenge=…&hub.verify_token=…` returns the challenge plain when `verify_token` matches `MESSENGER_VERIFY_TOKEN`. Mismatch → 403.
2. **Verify signature** — `meta::signature::verify_x_hub_signature_256(app_secret, raw_body, header)` does an HMAC-SHA256 over the **raw bytes** (Axum's `Bytes` extractor preserves them — no JSON normalisation), constant-time compares against `sha256=<hex>`. Mismatch → 401. The signature buys integrity, not confidentiality.
3. **Dedup + dispatch** — for each `messaging` entry, deserialise into `meta::events::IncomingEvent`. A `message.text` becomes `WsEnvelope::UserMessage { text, psid: sender.id, request_id: <fresh>, mid }`. A `postback` becomes `WsEnvelope::Postback { payload }`. Push the envelope to the broker keyed by `fb:<recipient.id>` (the Page id).
4. **Respond 200** within a few seconds — Meta retries on 5xx / timeouts. The actual agent turn is async; the relay only acks ingest.

### Relay → desktop: WebSocket `/ws?token=<binding_jwt>`

`WsEnvelope` (`protocol.rs`) uses a `kind`-tag namespaced `messenger_*` so the same `/ws` stream can also carry LINE variants:

```json
{ "kind": "messenger_user_message", "text": "hi", "psid": "9876…", "request_id": "abc", "mid": "m_xyz" }
{ "kind": "messenger_postback",     "payload": "tool:allow:abc" }
{ "kind": "messenger_notice",       "text": "paired" }
```

`#[serde(default)]` on every field plus the `kind`-tag enum means unknown variants (future `messenger_attachment`, `messenger_referral`, …) deserialise side-by-side without breaking existing arms. Notices are logged locally and not forwarded to the agent.

### Desktop → relay: `POST /messenger/reply/{request_id}`

Authenticated by `Authorization: Bearer <binding_jwt>`. Body (`ReplyBody`):

```json
{
  "text": "agent response",
  "quick_reply": [
    { "title": "Allow",  "payload": "tool:allow:abc" },
    { "title": "Always", "payload": "tool:always:abc" },
    { "title": "Deny",   "payload": "tool:deny:abc"  }
  ]
}
```

The relay (`routes/messenger.rs`) maps `quick_reply[]` → Send API `quick_replies[]` (each `content_type: "text"`), then calls `meta::send::send_message(page_token, psid, text, quick_replies)` which POSTs `/v19.0/me/messages` with `messaging_type: "RESPONSE"`. The `request_id` is looked up to recover the original PSID — the desktop never sees the PSID directly except as a payload field, so reply addressing lives on the relay.

### Relay → Meta: Graph Send API

`meta::send::send_message`:

```http
POST https://graph.facebook.com/v19.0/me/messages?access_token=<PAGE_TOKEN>
Content-Type: application/json

{ "messaging_type": "RESPONSE",
  "recipient": { "id": "<PSID>" },
  "message":   { "text": "…", "quick_replies": [ … ] } }
```

`messaging_type: RESPONSE` is correct only inside the 24-hour standard messaging window after the user's last inbound message. Sends outside that window need `MESSAGE_TAG` + tag value (not exposed in Tier 1; future Page Inbox handoff or transactional notifications would extend this).

## Config & token resolution

`MessengerConfig` (`config.rs`) is the desktop-side state:

```rust
pub struct MessengerConfig {
    pub binding_token: String,                    // HS256 JWT from /pair
    pub server_url: Option<String>,               // None → env → DEFAULT_SERVER_URL
    pub page_name: Option<String>,                // GUI pill label cache
    pub page_id:   Option<String>,                // sanity check for inbound events
}
```

Persisted at `~/.config/thclaws/messenger.json` (atomic write via `.tmp` + rename). `resolved_server_url()`:

1. `self.server_url` if `Some(s)` and non-empty.
2. `$THCLAWS_MESSENGER_SERVER` (12-factor override for dev/mock).
3. `DEFAULT_SERVER_URL = "https://line.thclaws.ai"`.

**What's NOT here:** the Page Access Token, the App Secret, the Verify Token. Those live in the relay's k8s Secret (`MESSENGER_*` env). The binding JWT is the desktop's only credential; revoking the binding on the relay side cuts off this desktop without touching Meta.

## Session routing

`MessengerSession` (`session.rs`) is what `bootstrap.rs` (GUI worker) or `headless.rs` spawns once a valid binding token is loaded. It owns:

- A `MessengerClient` (`client.rs`) reading `WsEnvelope` frames + posting replies.
- A `dyn MessengerMessageHandler` (pluggable turn-runner — `WorkerForwardHandler` in GUI / `HeadlessAgentHandler` in `--messenger`).
- An `Option<MessengerApprover>` set on enter of `MessengerGated` mode.

On `WsEnvelope::UserMessage`:

1. If an approver is `active` for this PSID, free-text fallback (`approve` / `deny`) short-circuits.
2. Else `handler.handle_message(text)` drives the agent. Its `Option<String>` return is the final assistant text — `None` skips the reply.
3. Reply text → `chunks_for_messenger()` (`filter.rs`) → each chunk POSTed to `/messenger/reply/{request_id}` in order. The relay calls Send API once per chunk; Messenger preserves order.

On `WsEnvelope::Postback`:

1. If the payload matches `tool:<verb>:<request_id>` and the request_id is pending in the approver, resolve the oneshot (unblocks the waiting turn).
2. Else `handler.handle_postback(payload)` (default no-op).

**Tier 1 has no per-PSID isolation.** All `UserMessage` envelopes for a paired Page funnel into the same shared session; the relay-side Page binding gates *who* can DM the Page (via Meta App Review / tester roles), and the operator gates *what runs* via `MessengerGated`. Per-PSID routing is a Tier 2 follow-up alongside Page Inbox handoff.

## Pairing

1. **User DMs the Page.** Meta delivers the message to the relay's `/messenger/webhook`.
2. **Relay has no binding for this Page yet** — `state::pair_intent_for(page_id)` creates a 6-digit numeric code (`getrandom`) with a 1-hour TTL.
3. **Relay DMs the code via Send API** to the same PSID (within the 24h messaging window opened by the user's message). The body is templated in `routes/messenger.rs`:
   ```
   thClaws: pairing code 123456 (1h)
   Paste it into the GUI Messenger Connect modal to bind this Page.
   ```
4. **User pastes the code into `MessengerConnectModal`**, which calls `messenger_pair` IPC. The handler POSTs to the relay's pair endpoint with the code; relay swaps it for an HS256 binding JWT scoped to `fb:<pageId>`, plus the cached `page_name` and `page_id`.
5. **GUI persists the binding** to `~/.config/thclaws/messenger.json` and spawns `MessengerSession`. The sidebar pill goes green.

Pairing codes are **in-memory** on the relay (relay restart drops pending codes; user re-DMs for a fresh one). Approved bindings live in Postgres alongside LINE bindings.

## `MessengerApprover` (tool gates)

`approver.rs`. When the runtime swaps to `PermissionMode::MessengerGated` (on `MessengerSessionHandle::connect`), `Approver::request` routes through `MessengerApprover::request`:

1. Format the prompt (HTML-safe-ish; Messenger doesn't render Markdown, so plain text + emoji glyphs).
2. POST a `ReplyBody { text, quick_reply: [Allow, Always, Deny] }` to the relay; payloads encode `tool:<verb>:<request_id>` so the postback round-trips the request id back.
3. Register a `tokio::sync::oneshot` keyed by request_id.
4. Wait on the oneshot with a 60s timeout — auto-deny on expiry.
5. The next `WsEnvelope::Postback { payload }` matching the request_id fires the oneshot. Free-text `approve` / `deny` (case-insensitive) inside the active PSID is the fallback.

`Allow` → `ApprovalReply::Allow`, `Always` → `AllowForSession`, `Deny` → `Deny`. Same shape as LINE / Telegram approvers; the three fold into `PermissionMode::BotGated` (planned Tier 2 consolidation).

**Tier-1 known gap:** approval prompts target the Page's **most-recent inbound PSID**, not the PSID of whoever triggered the tool call. With a single-tester Page that's invisible; multi-tester Pages will see approvals occasionally go to the wrong inbox. The fix is per-PSID approver state, blocked on per-PSID session routing.

## Permission posture

`PermissionMode::MessengerGated` (`permissions.rs`):

```rust
pub enum PermissionMode {
    Auto,
    Ask,
    LineGated,
    TelegramGated,
    MessengerGated,   // dev-plan/31
    Plan,
}
```

Folds into the `is_ask_like()` set alongside `Ask | LineGated | TelegramGated`. Connect/disconnect swaps the runtime mode the same way LINE/Telegram do — the user's pre-connect mode is saved and restored on disconnect. The `messengergated` lowercase string is what `shell_dispatch.rs` prints for `/permissions`.

## GUI integration

`bootstrap.rs` (gui-feature-gated) defines:

- `MessengerSessionHandle` — JoinHandle + cancel token + status broadcaster. `start()` validates the binding JWT shape, opens the WS, swaps `PermissionMode`, and returns the handle. `stop()` cancels + flips the mode back.
- `MessengerStatus` — `{ state: Disconnected | Connecting | Ready, page_name?, page_id?, last_error? }` broadcast over `ViewEvent::MessengerStatus`. The sidebar polls + the modal subscribes.
- `WorkerForwardHandler` — `MessengerMessageHandler` impl that forwards into the worker via `ShellInput::MessengerMessage { text, request_id, psid }`. The worker handles the turn through the shared session machinery, then emits the final assistant text back through `ViewEvent::MessengerReply` which the bootstrap layer translates into a `/messenger/reply/{id}` POST.

IPC arms (`ipc.rs`):

| Arm | Effect |
|---|---|
| `messenger_status` | Snapshot current `MessengerStatus` + return resolved `server_url`, `page_name`, `page_id`. |
| `messenger_pair` | Exchange a 6-digit code with the relay for a binding JWT; persist to `messenger.json` and (re)start `MessengerSession`. Emits `messenger_pair_result`. |
| `messenger_disconnect` | Cancel the running session, write `binding_token: ""` (preserving cached page metadata), restore the saved permission mode. Emits `messenger_disconnect_ack`. |

Boot-time auto-reconnect: if `messenger.json` exists with a non-empty `binding_token` at GUI startup, `bootstrap.rs` calls `start()` automatically so the pill comes up green without a manual click.

## Headless loop

`headless.rs::run(config)` (called from `bin/app.rs` when `--messenger` is passed):

1. Load `MessengerConfig`. If missing / empty token → print a one-liner pointing at `thclaws messenger pair` and exit 1.
2. Construct an `Agent` the same way `repl::run_print_mode` does (provider, KMS, memory, tools — full local stack, no GUI worker).
3. Wrap it in a `HeadlessAgentHandler` whose `turn_lock: tokio::sync::Mutex<()>` serialises turns so two simultaneous inbound messages can't race the shared agent history.
4. Spawn `MessengerSession::run(client, handler)` and block on it. Ctrl-C cancels gracefully.

The `--telegram` and `--messenger` headless paths share the same construction shape (single shared session, lock-serialised turns, in-process approver) — the difference is only the wire layer.

## Output filter

`filter.rs::chunks_for_messenger`:

1. Reuse `crate::line::filter::clean_for_stream` to strip ANSI + tool-narration lines (`[tool: …]`, `⏺ thinking`, etc.). The two surfaces share this so a narration leak in either gets a one-place fix.
2. Trim whitespace; empty → `vec![]` (caller skips the send).
3. Greedy split at `\n` boundaries within `CHUNK_AT = 1_900` characters (leaves headroom under the hard 2,000 cap for a future `(1/N)` prefix). Falls back to a char-boundary cut for unbroken runs.
4. UTF-8 safe — `.chars()` iteration only.

The cap is Messenger's text-message hard limit; longer texts get rejected with an "exceeds maximum length" Graph error. Telegram's 4,096 and LINE's 5,000 are higher, so be careful not to copy `output_ceiling` from those surfaces.

## CLI

`bin/app.rs` adds:

- `--messenger` flag → `messenger::headless::run`.
- `thclaws messenger status` → load + print resolved config (binding token shown by prefix only).
- `thclaws messenger pair` → print the operator-side runbook (Meta app + webhook URL + relay env + pairing handshake). Read-only.

`--cli` is implied when any of `--cli | --print | --telegram | --messenger` is set, so `--messenger` skips the GUI bootstrap entirely on platforms where the `gui` feature is compiled in.

## Testing

- **24 core messenger tests** in `crates/core/src/messenger/` — config round-trip, filter chunking + UTF-8, approver state machine, protocol deserialisation, session dispatch table.
- **53 relay tests** in `crates/line-server/` (added under `meta/` + `routes/messenger.rs`) — `X-Hub-Signature-256` verify (constant-time + reject on missing prefix), webhook handshake, Send API request shape, broker variant routing.
- Manual end-to-end runbook in [`docs/fb-test-guidline.md`](../docs/fb-test-guidline.md): Meta-app + Page-token setup, webhook subscription, pairing through GUI + headless, the Tier-1 acceptance scenarios, and isolated `curl` webhook tests including an `openssl dgst -sha256 -hmac` snippet for forging a valid `X-Hub-Signature-256` against a local relay.

Build / fmt:

```bash
cd thclaws
cargo fmt --check
cargo build --features gui          # gui-gated bootstrap.rs compiles
cargo test -p thclaws-core messenger
cargo test -p thclaws-line-server -- meta routes::messenger
```

## Tier 2 / 3 roadmap

- **Per-PSID session routing + approval addressing** — one paired Page can serve multiple end-users with isolated state; fixes the most-recent-PSID approval gap.
- **Page Inbox handoff** (`pass_thread_control`) — let a human Page admin take over a conversation seamlessly, and let the bot reclaim it later.
- **Headless pairing redemption** — a `thclaws messenger pair --code 123456` subcommand so headless hosts don't need the GUI for the one-time pair.
- **Attachment ingest** (image / file / voice) + sticker vision, mirroring the LINE Tier-3 roadmap.
- **`MESSAGE_TAG` outbound** for transactional notifications outside the 24h window (subject to Meta policy / App Review).
- **Neutral gateway host** — rename `line.thclaws.ai` → `gw.thclaws.ai` (or similar) so the relay's role isn't visually LINE-only.
- **PermissionMode consolidation** — fold `LineGated | TelegramGated | MessengerGated` into one `BotGated` posture with surface metadata.

## Cross-references

- [`line-bridge.md`](line-bridge.md) — the relay machinery this bridge reuses, including `/pair` + WS multiplex + browser-chat addendum.
- [`telegram-bridge.md`](telegram-bridge.md) — the relay-free alternative; identical approver / pairing UX shape, completely different connectivity model.
- User-facing chapter: [`user-manual/ch24-messenger.md`](../user-manual/ch24-messenger.md).
- Test runbook: [`docs/fb-test-guidline.md`](../docs/fb-test-guidline.md).
- Design doc: [`dev-plan/31-facebook-messenger-adapter.md`](../dev-plan/31-facebook-messenger-adapter.md).
