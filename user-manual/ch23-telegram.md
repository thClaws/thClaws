# Chapter 23 — Telegram bot

Drive thClaws from Telegram. Create a bot with `@BotFather`, paste its
token into thClaws, and every message you DM the bot runs as a turn on
your desktop — the full tool registry (Bash, Edit, KMS, MCP, skills)
executes locally, and replies stream back as Telegram messages. Tool
calls that need approval show up as inline-keyboard buttons you tap
from your phone. (dev-plan/29, Tier 1.)

## Why Telegram (and how it differs from LINE)

The [LINE bridge](ch21-line-and-browser-chat.md) needs a relay server
(`line.thclaws.ai`) because LINE only delivers messages by pushing an
HTTPS webhook to an endpoint someone has to host. Telegram's Bot API
exposes **long-polling** (`getUpdates`), which works fine behind NAT —
so thClaws talks to `api.telegram.org` **directly**. No relay, no
server to run, no third party in the message path. Outside Thailand,
Telegram is also simply the more common default.

The desktop never goes away: your code, secrets, and tools stay local.
Telegram is only the chat surface.

## How it works (one paragraph)

When you connect, thClaws opens a long-poll loop to `api.telegram.org`,
pulling new messages as they arrive. Each authorized message is fed to
the agent on your desktop; the final assistant text is HTML-escaped,
chunked to fit Telegram's 4096-character limit, and sent back with
`sendMessage`. Mutating tools pause the turn and post an inline keyboard
(**Allow / Always / Deny**); your tap resolves the gate and the message
is edited in place to show the verdict.

## Setup

### 1. Create a bot

In Telegram, open a chat with **`@BotFather`** and send `/newbot`.
Follow the prompts (pick a name and a username ending in `bot`).
BotFather replies with a **token** like:

```
<your-bot-id>:<token-from-botfather>
```

(format: digits, then `:`, then ~35 chars from `[A-Za-z0-9_-]`)

Keep it secret — the token is the bot's full API key.

### 2a. Connect from the GUI

1. Open **Settings → Telegram Connect…**.
2. Paste the bot token and click **Connect**. thClaws validates it
   against Telegram (`getMe`) and starts polling; the sidebar shows a
   **Telegram** pill with the bot's `@username`.
3. **Approve yourself on thClaws.** Connecting starts the bridge, but
   nobody is allowlisted yet (the default DM policy is `pairing`), so
   the bot won't answer anyone — *including you* — until approved. DM
   the bot from your phone; a **pairing request** appears in the
   Telegram Connect modal with an **Approve** button. Click it, and
   you're cleared to chat. See **Pairing** below for the full flow.

### 2b. …or run headless

No GUI needed — set the token in the environment and start the bot
loop:

```bash
export TELEGRAM_BOT_TOKEN="123456789:AA…"
export TELEGRAM_OWNER_ID="<your numeric Telegram user id>"   # optional but recommended
thclaws --telegram
```

`--telegram` runs its own agent loop (the GUI worker is desktop-only),
prints `connected as @yourbot`, and serves messages until Ctrl-C. It
honours the same project `.thclaws/settings.json` as the REPL.

> **Finding your user id:** message `@userinfobot` (or any "what's my
> id" bot) on Telegram. `TELEGRAM_OWNER_ID` adds you to the allowlist
> at startup so you can DM the bot immediately — headless mode has no
> GUI to approve pairing codes (see below).

## Pairing — who's allowed to talk to the bot

Anyone who knows the bot's `@username` can message it, so by default
thClaws does **not** answer strangers. The default DM policy is
`pairing`:

1. A new user DMs the bot.
2. The bot replies: *"You're not paired yet. Your pairing code is
   `123456`. Ask the thClaws owner to approve it."*
3. In **Settings → Telegram Connect…**, the owner sees the request
   (name + code) with **Approve** / **Reject** buttons.
4. On **Approve**, the user's id is added to `allowFrom`, saved to
   disk, and the bot DMs them *"You're approved!"*.

Codes expire after **1 hour**. In headless mode there's no GUI to
approve from — use `TELEGRAM_OWNER_ID` (instant allowlist) or
pre-populate `allowFrom` in the config file.

Set `dmPolicy: "allowlist"` instead if you want unknown senders ignored
silently with no pairing prompt at all.

## Approving tool calls from your phone

While Telegram is connected the runtime permission mode is
`telegramgated` (see [Chapter 5](ch05-permissions.md)) — semantically
the same as `ask`, but **every** approval prompt routes to your
Telegram chat regardless of which surface (Terminal, Chat, REPL,
Telegram) typed the original request. The bot posts:

```
🔐 thClaws wants to run: Bash

Input: {"command":"ls -la ~/Downloads"}

Tap a button (auto-denies in 60s).
[ ✅ Allow ] [ ♾️ Always ] [ 🚫 Deny ]
```

- **Allow** — runs this one call.
- **Always** — runs this and every later call this session (maps to
  "allow for session").
- **Deny** — the agent gets the denial and continues the turn.

After you tap, the buttons disappear and the message is rewritten to
show the verdict. No tap within **60 seconds** auto-denies. You can
also just type `approve` / `deny` as a fallback.

**To stop approvals routing to Telegram:** disconnect from the GUI
(restores your pre-connect `auto` / `ask` mode), or set
`/permissions auto`.

## Groups

Add the bot to a group and, by default (`groupPolicy: "allowlist"`),
it ignores the group until you opt that chat in. Add the group's chat
id (a negative integer) under `groups` in the config, or set
`groupPolicy: "open"` to serve every group the bot is added to. In
Tier 1 a group shares one session (no per-user split); broadcast
**channels** and forum-topic routing are a later tier (see below).

> Telegram bots in groups only receive messages by default if they're
> mentioned or sent as commands ("privacy mode"). Toggle this in
> BotFather (`/setprivacy`) if you want the bot to see all group text.

## Configuration

Runtime state lives in `~/.config/thclaws/telegram.json` (written by
the GUI modal). A project can also ship a block under `telegram` in
`.thclaws/settings.json`. Fields:

```json
{
  "enabled": true,
  "botToken": "123456789:AA…",
  "dmPolicy": "pairing",
  "allowFrom": ["111111111"],
  "groupPolicy": "allowlist",
  "groups": { "-1001234567890": { "label": "Team room" } },
  "outputCeiling": 4000
}
```

| Field | Meaning |
|---|---|
| `enabled` | Auto-reconnect on launch when `true` and a token resolves |
| `botToken` | BotFather token. **Optional** — `TELEGRAM_BOT_TOKEN` env wins over it |
| `dmPolicy` | `pairing` (default) or `allowlist` |
| `allowFrom` | Telegram user ids (strings) allowed to DM |
| `groupPolicy` | `allowlist` (default) or `open` |
| `groups` | Allowlisted group chat ids → `{ label? }` |
| `outputCeiling` | Per-message char cap before chunking (default 4000) |

**Token precedence:** `TELEGRAM_BOT_TOKEN` env → `botToken` in the file
→ nothing. Env-wins means you never have to commit a token to disk for
CI / container runs. A pre-upload check refuses tokens bundled in a
deployed config.

## CLI

```
thclaws --telegram          Run the bot headless until Ctrl-C
thclaws telegram status     Print resolved config (token redacted)
thclaws telegram pair       Print @BotFather setup instructions
```

`telegram status` is handy for confirming the token is detected:

```
$ thclaws telegram status
Telegram adapter status
  enabled:        true
  bot token:      123456789:<redacted> (present)
  dm policy:      Pairing
  group policy:   Allowlist
  allow_from:     1 user(s)
  groups:         0 allowlisted
  output ceiling: 4000 chars
```

## Output formatting

- Replies are sent in **HTML parse mode** — only `<`, `>`, `&` are
  escaped (a much smaller foot-gun than MarkdownV2). Fenced code blocks
  become `<pre>` blocks; other markdown shows literally.
- Long replies are split into multiple messages below `outputCeiling`
  (default 4000) chars, on line boundaries where possible. UTF-8 is
  preserved — Thai, emoji, and CJK never get cut mid-character.
- ANSI escape sequences and the GUI's tool-call narration (the `⏺`/`🔧`
  lines) are stripped before sending.

## Privacy and trust boundary

- **No relay.** thClaws talks straight to `api.telegram.org`. Nobody
  but Telegram and your desktop is in the message path.
- **The token is the key.** Anyone with it can drive your bot's API.
  Prefer `TELEGRAM_BOT_TOKEN` (env) over writing it to
  `telegram.json`; keychain storage lands in a later tier.
- **Upstream LLM calls never go through Telegram.** Your prompts go
  desktop → Anthropic / OpenAI / etc. directly. Telegram only carries
  the chat text.
- **Pairing codes are in-memory, 1-hour TTL.** A process restart drops
  pending codes (re-DM for a fresh one). Approved users persist in
  `allowFrom`.

## Not in Tier 1 (coming later)

This chapter documents Tier 1 — DM + basic group + plain text +
pairing + inline-keyboard approvals. Planned for later tiers:

- **Broadcast channels + linked discussion groups + forum-topic
  routing** (Tier 2) — "a background research agent posts status to a
  channel I glance at".
- **Streaming preview edits, media (photo/document) up/download, voice
  transcription, sticker vision, webhook mode, multi-account, proxy
  support** (Tier 3).

Until then, inbound photos/voice/stickers are ignored (text only).

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| "token rejected by Telegram (401)" on connect | Wrong/expired token | Re-copy from `@BotFather`; check for trailing spaces |
| Bot never replies to your DM | You're not allowlisted yet | Approve the pairing code in the GUI, or set `TELEGRAM_OWNER_ID` (headless) |
| Bot ignores group messages | Group not allowlisted, or BotFather privacy mode on | Add the chat id to `groups` (or `groupPolicy: "open"`); `/setprivacy` in BotFather |
| Approval buttons time out | No tap within 60s | Tap again on a fresh request, or type `approve` |
| Headless: "no allowlisted users yet" warning | `allowFrom` empty and no owner id | `export TELEGRAM_OWNER_ID=<your id>` and restart |
| Old messages replay on startup | — | They don't: backlog from before launch is drained and discarded by design |
| Two bots fight ("Conflict") | Another `getUpdates`/webhook is running for the same token | Run only one thClaws (or one webhook) per token |

## What's NOT in this chapter

- Internal architecture (wire types, long-poll loop, approver state
  machine, pairing manager) — see the technical manual's
  [`telegram-bridge.md`](../../thclaws-technical-manual/telegram-bridge.md).
- The LINE bridge and browser chat — [Chapter 21](ch21-line-and-browser-chat.md).
