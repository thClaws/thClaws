# Chapter 24 — Facebook Page Messenger bot

Drive thClaws from your Facebook Page inbox. Connect a Page once, and
every Messenger DM to the Page runs as a turn on your desktop — the
full tool registry (Bash, Edit, KMS, MCP, skills) executes locally,
and replies stream back as Messenger messages. Tool calls that need
approval show up as Messenger quick-reply chips you tap from your
phone. (dev-plan/31, Tier 1.)

## Why Messenger (and how it differs from Telegram)

The [Telegram bot](ch23-telegram.md) talks to `api.telegram.org`
directly because Telegram exposes long-polling (`getUpdates`) that
works behind NAT. Messenger is **webhook-only** — Meta will only
deliver messages by pushing an HTTPS webhook to a public endpoint —
so the Messenger bridge needs a **relay server**, the same way the
LINE bridge does ([Chapter 21](ch21-line-and-browser-chat.md)).
thClaws Tier 1 reuses the official LINE relay (`line.thclaws.ai`) with
an added `/messenger/webhook` route, so you don't have to run a server
yourself.

The desktop never goes away: your code, secrets, and tools stay local.
The relay only carries chat text, never your prompts upstream to
Anthropic / OpenAI / etc.

## How it works (one paragraph)

When you connect, thClaws opens a WebSocket to the relay using a
**binding JWT** that's scoped to your Page. Meta posts every inbound
Messenger event to the relay; the relay verifies the
`X-Hub-Signature-256` header, looks up the Page's binding, and pushes
the message to the desktop as a `user_message` frame. Your agent runs
the turn locally; the final assistant text is stripped of ANSI / tool
narration, chunked to Messenger's 2,000-character limit, and POSTed
back to the relay's `/messenger/reply/{request_id}` endpoint, which
calls the Graph **Send API** (`messaging_type: RESPONSE`) with the
Page Access Token (which lives on the relay, never on your desktop).
Mutating tools pause the turn and post a quick-reply row
(**Allow / Always / Deny**); your tap resolves the gate and the turn
continues.

## Setup

Messenger setup has two operator-side prerequisites that LINE / Telegram
don't: a Meta app, and a webhook subscription. The relay-side env vars
are configured once per relay deployment (the official relay already has
them in production).

### 1. Create a Meta app + Page token

In [Meta for Developers](https://developers.facebook.com/apps):

1. **Create App** → type **Business**. Add the **Messenger** product.
2. Under **Messenger → Settings**, generate a **Page Access Token** for
   the Page you want to drive. Long-lived. Keep it secret.
3. From **App → Settings → Basic**, copy the **App Secret**.
4. Pick a random **Verify Token** (e.g. `openssl rand -hex 16`) — this
   is just for the webhook handshake.
5. Get your numeric **Page ID** (Page → About, or
   `curl 'https://graph.facebook.com/me?access_token=<PAGE_TOKEN>'`).

> **Reality check:** in **Development mode** Meta only delivers messages
> from people who have a role on your app (admins / developers /
> testers). That's enough to test end-to-end — add yourself as a tester.
> Messaging the general public needs **App Review** + **Business
> Verification** for the `pages_messaging` permission (days–weeks). Don't
> block testing on it.

### 2. Point Meta's webhook at the relay (operator step)

App → Messenger → Settings → Webhooks → **Add Callback URL**:

| Field | Value |
|---|---|
| Callback URL | `https://<relay>/messenger/webhook` |
| Verify Token | The same random string from step 1.4 |
| Subscribe to | `messages` (at minimum) |

After Meta verifies the URL, subscribe the Page itself under **Webhooks
→ Add Subscriptions → Page → messages**.

The relay reads these env vars (already set on `line.thclaws.ai` in
production; only matters if you self-host):

```sh
MESSENGER_APP_SECRET=<app secret>
MESSENGER_VERIFY_TOKEN=<the string you picked>
MESSENGER_PAGE_ACCESS_TOKEN=<page token>
MESSENGER_PAGE_ID=<numeric page id>
```

### 3a. Connect from the GUI

1. Open **Settings → Messenger Connect…**.
2. DM your Page from a personal Facebook account that has a role on
   the app (admin / tester). The relay replies with a **6-digit
   pairing code** sent via the Send API.
3. Paste the code into the modal and click **Connect**. thClaws
   exchanges the code for a binding JWT, saves it locally, and starts
   the WebSocket. The sidebar shows a **Messenger** pill with the Page
   name.

### 3b. …or run headless

The Tier-1 headless path needs a binding JWT already on disk (pair via
the GUI first), then:

```bash
thclaws --messenger
```

`--messenger` runs its own agent loop (no GUI), prints
`connected to <relay>`, and serves Messenger turns until Ctrl-C. It
honours the same project `.thclaws/settings.json` as the REPL.

> Headless pairing redemption itself (entering the 6-digit code without
> the GUI modal) is a follow-up. For now, do the one-time pair through
> the GUI on any machine; the resulting `~/.config/thclaws/messenger.json`
> can be copied to a headless host.

## Configuration

Runtime state lives in `~/.config/thclaws/messenger.json` (written by
the GUI modal). It's small by design — the sensitive bits stay on the
relay:

```json
{
  "binding_token": "<HS256 JWT issued by the relay>",
  "server_url": null,
  "page_name": "My Test Page",
  "page_id": "1234567890"
}
```

| Field | Meaning |
|---|---|
| `binding_token` | JWT the relay issues at pair time. The desktop authenticates the WS and `/messenger/reply` calls with this — *not* the Page Access Token. |
| `server_url` | Relay base URL. `null` falls back to `$THCLAWS_MESSENGER_SERVER` then `https://line.thclaws.ai`. |
| `page_name` | Cached Page name for the GUI pill. |
| `page_id` | Cached numeric Page ID, used for sanity checks on inbound events. |

**What's NOT here:** the Page Access Token, the App Secret, the Verify
Token. Those live on the relay's k8s Secret and never touch your
desktop.

## CLI

```
thclaws --messenger          Run the bridge headless until Ctrl-C
thclaws messenger status     Print resolved binding (token shown by prefix only)
thclaws messenger pair       Print Meta-side setup instructions
```

`messenger status` confirms the binding is wired up:

```
$ thclaws messenger status
Messenger adapter status
  relay:          https://line.thclaws.ai
  binding token:  eyJhbG… (present)
  page:           My Test Page
  page id:        1234567890
```

`messenger pair` prints the operator-side runbook (Meta app, webhook
URL, env vars, pairing handshake) — useful when bootstrapping a new
Page or rebuilding the binding.

## Approving tool calls from your phone

While Messenger is connected the runtime permission mode is
`messengergated` (see [Chapter 5](ch05-permissions.md)) — same semantics
as `ask`, but **every** approval prompt routes to the Messenger thread
as quick-reply chips. The Page DMs:

```
🔐 thClaws wants to run: Bash

Input: {"command":"ls -la ~/Downloads"}

Tap a chip below (auto-denies in 60s).
[ ✅ Allow ]   [ ♾️ Always ]   [ 🚫 Deny ]
```

- **Allow** — runs this one call.
- **Always** — runs this and every later call this session (maps to
  "allow for session").
- **Deny** — the agent gets the denial and continues the turn.

No tap within **60 seconds** auto-denies. You can also type
`approve` / `deny` as a fallback. Quick-reply chips disappear once the
turn moves on; older prompts in the thread remain visible but become
inert.

**To stop approvals routing to Messenger:** disconnect from the GUI
(restores your pre-connect mode), or set `/permissions auto`.

## Output formatting

- Replies are **plain text** — Messenger doesn't render Markdown, so
  fences and bold show literally. ANSI escapes are stripped before
  sending.
- Long replies are split into multiple messages below the
  **2,000-char limit** (Messenger's hard cap is lower than Telegram's
  4,096 and LINE's 5,000). Chunks break at line boundaries where
  possible; UTF-8 is preserved — Thai, emoji, and CJK never get cut
  mid-character.
- Tool-call narration (`[tool: Bash …]`, ANSI status lines) is stripped
  before sending. Only the final assistant text reaches Messenger.

## Privacy and trust boundary

- **The relay sees chat text.** Messenger requires a webhook, so the
  Messenger ↔ desktop path goes Meta → relay → your machine over WSS.
  The official relay logs the minimum needed for routing and
  troubleshooting; nothing about your prompts to Anthropic / OpenAI
  is in that path.
- **The Page Access Token + App Secret live on the relay**, never on
  your desktop. The binding JWT in `messenger.json` is scoped to a
  single Page and can be revoked relay-side without touching Meta.
- **Upstream LLM calls never go through Messenger.** Your prompts go
  desktop → Anthropic / OpenAI / etc. directly. Messenger only carries
  Page chat text.
- **Pairing codes are 6 digits, in-memory, 1-hour TTL** on the relay.
  A relay restart drops pending codes (re-DM the Page for a fresh
  one). Approved bindings persist in the relay's Postgres.

## Not in Tier 1 (coming later)

This chapter documents Tier 1 — DM + plain text + 6-digit pairing +
quick-reply approvals + GUI/headless connect. Planned for later tiers:

- **Page Inbox handoff** (Meta's `pass_thread_control` flow) so a human
  agent can take over a conversation seamlessly.
- **Per-PSID session routing** (one paired Page → one shared session
  today; multiple end-users hitting the same Page share state).
- **Media (image/file/voice) up/download, sticker vision, streaming
  preview edits, headless pairing redemption, neutral gateway host**.

Until then, inbound photos/voice/stickers are ignored (text only),
and approval prompts target the Page's **most-recent inbound PSID**.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Webhook verify fails in Meta UI | Verify Token mismatch | Make sure `MESSENGER_VERIFY_TOKEN` on the relay matches what you typed in the Meta webhook form, exactly |
| Page is silent when you DM it | App still in Development mode + sender isn't a tester | Add the sender's FB account as a tester under App Roles, or submit for App Review |
| Page is silent + you are a tester | Webhook isn't subscribed to `messages`, or Page isn't subscribed to the app | Re-check **Webhooks → Add Subscriptions → Page → messages** is ticked |
| "binding token rejected" on connect | Stale / revoked JWT | Re-pair through the GUI; old binding rows can be revoked relay-side |
| Pairing code never arrives | `MESSENGER_PAGE_ACCESS_TOKEN` invalid on the relay | Relay logs show the Send API error; regenerate the token in Meta and update the relay |
| Replies arrive but are cut off | Per-message hard cap (2,000) | Expected — long replies arrive as multiple messages; Messenger preserves order |
| Approval chips don't appear | `dmPolicy` blocks the sender OR permission mode isn't `messengergated` | Check `thclaws messenger status` + `/permissions` in the REPL |
| Multiple replies to one message | Webhook re-delivery (Meta retries failed deliveries) | Idempotency is handled relay-side via the `mid` dedup key; if you see this, check relay logs |

## What's NOT in this chapter

- Relay internals (Meta-graph webhook verification, the broker
  routing layer, Send API client, `/messenger/{webhook,reply,push}`
  routes) — see the technical manual's
  [`messenger-bridge.md`](../../thclaws-technical-manual/messenger-bridge.md).
- LINE OA and browser chat — [Chapter 21](ch21-line-and-browser-chat.md).
- Telegram (long-polling, no relay) — [Chapter 23](ch23-telegram.md).
