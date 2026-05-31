# Chapter 26 — GUI Shells

GUI Shells let you swap the default Chat / Terminal view for a
**domain-specific HTML frontend** — an image-generation grid, a
trading dashboard, a campaign builder, anything. The shell renders
inside a sandboxed iframe and talks to the agent through a small
`window.thclaws.*` bridge. Built-in shells ship with thClaws; custom
shells are folders you drop on disk; the same shell can also be
served over the cloud at a tokenised URL so you can use it from a
phone browser or share it with a teammate.

> **Status:** Tier 1 lands in v0.24 (Session Explorer + tab loader);
> Tier 2 adds the picker, custom shells, and `--serve --gui-shell`;
> Tier 3 adds the SDK, permissions, and marketplace. See
> [dev-plan/33](../dev-plan/33-gui-shell.md) for the full roadmap.
> Sections below tag which tier introduces each capability.

## When to use a GUI Shell

Use a GUI Shell when the workload has a **better visualisation than
a chat transcript** and the user wants to interact through that
visualisation, not by typing prompts:

- Generating images — a grid of past generations beats scrolling chat
  for "show me what I've made".
- Reviewing a long agent session — a tool-call tree beats a linear
  scroll for "where did it call `bq_query`?".
- Building an ad campaign — form fields for targeting beat
  describing the filters in prose.

Stay with **Chat** (Chapter 4) when the workflow is conversational
and primarily text. Stay with **Terminal** (Chapter 4) when you want
the raw ANSI stream. GUI Shells are additive — they don't replace
either.

## Two delivery modes

Every shell can run in two places. The shell author writes the same
code; the user picks the surface.

| Mode | Where it runs | URL surface | Auth | Bridge transport |
|---|---|---|---|---|
| **A** Desktop tab | thClaws GUI app | `thclaws://` custom protocol | desktop session | `window.ipc.postMessage` |
| **B** Serve / cloud | `--serve` listener | `https://host/t/<token>/` | per-shell token in path | WebSocket |

Mode A is the default. Mode B (Tier 2) is for using a shell from
elsewhere — phone, teammate, headless server.

---

## Mode A — open a shell in the desktop GUI

### Tier 1 — built-in only

1. Launch thClaws (`thclaws` or `cargo run --features gui --bin
   thclaws`).
2. Click **"+ New Tab" → "Open Session Explorer"**. (Tier 1 ships one
   built-in shell wired directly into the new-tab menu; the picker
   for choosing between shells lands in Tier 2.)
3. The tab opens with the Session Explorer UI rendered inside.
   Click a session in the left rail, click a tool-call node in the
   tree to expand it, click "Summarise" to have the agent describe
   the call in one line.
4. Close the tab → the shell's session is persisted at
   `./.thclaws/sessions/<id>.jsonl` (same place as Chat/Terminal
   sessions, with an extra `shell: { id, version }` metadata field
   — still `cat`-able).
5. Reopen later → choose the same session from the Sessions browser;
   it relaunches the shell with the prior state.

### Tier 2 — picker + custom shells

After Tier 2:

1. **"+ New Tab" → "GUI Shell"** opens a **picker grid** showing
   every installed shell — built-ins, user-level (`~/.config/
   thclaws/gui-shell/`), and project-level (`./.thclaws/gui-shell/`).
2. Each card shows icon, name, version, source (`builtin` / `user` /
   `project`), declared permissions, and any past sessions for that
   shell with one-click resume.
3. Click a card → the picked shell replaces the picker in the Shell
   tab. Mode A is single-shell-at-a-time as of v0.24 — multi-instance
   shell tabs are still pending in a later release. (Multi-tenant
   `--serve` for hosting one shell to many users HAS shipped — see
   "Multi-tenant" below.) Use the "shells" breadcrumb to return to
   the picker and pick a different shell.
4. **"Refresh shells"** button rescans the discovery folders without
   restarting thClaws.

### Setting a default shell

If you always want a specific shell to open when you click "New GUI
Shell", set it in `settings.json`:

```jsonc
// ./.thclaws/settings.json  (project — wins)
// or ~/.config/thclaws/settings.json  (user — falls through)
{ "guiShell": "session-explorer" }
```

Long form, when desktop and serve defaults should differ:

```jsonc
{
  "guiShell": {
    "tabDefault":   "session-explorer",   // for Mode A "New Shell"
    "serveDefault": "my-image-bot"      // for Mode B --serve fallback
  }
}
```

---

## Mode B — serve a shell over the cloud  *(Tier 2)*

Use this to access a shell from a phone, share it with a teammate,
or run it on a server.

### Launch

```sh
thclaws --serve --gui-shell my-image-bot --port 8080
```

stdout:

```
Serving My Image Bot (v0.1.0) at
  https://localhost:8080/t/abc...xyz/
Token persisted to ~/.config/thclaws/gui-shell-tokens.json
```

Open that URL in any browser. A landing flash confirms
`Connecting to: my-image-bot v0.1.0 on <host>`, then the shell
renders as the entire page — same UI as Mode A, same bridge, just
WebSocket under the hood instead of Tauri IPC.

### The token IS the credential

- The URL `https://host:8080/t/<token>/` is everything you need.
  Anyone with it gets in; anyone without it gets a silent 404 (the
  server doesn't even advertise that a shell is bound).
- Token is generated on first launch and **persisted** in
  `~/.config/thclaws/gui-shell-tokens.json` keyed by `(shellId,
  port)`. Restarting `--serve` keeps the same URL, so sharing it
  once is meaningful.
- Direct URLs like `/gui-shell/session-explorer/` or `/shells/`
  return 404. Only the shell launched with `--gui-shell` is
  reachable, and only through `/t/<token>/`.

### Pinning the token (for deployments)

For k8s manifests or systemd units that need a stable URL:

```sh
thclaws --serve \
        --gui-shell my-image-bot \
        --gui-shell-token "$MY_TOKEN" \
        --gui-shell-token-ttl 90d \
        --port 8080
```

### Rotating

If a URL leaks or you want to invalidate sharing:

```sh
thclaws shell rotate-token my-image-bot
# → prints the new URL, old URL stops working immediately
```

### No-auth mode (localhost / intranet only)

```sh
thclaws --serve --gui-shell my-image-bot --gui-shell-no-auth
```

Routes mount at `/` directly — no `/t/<token>/` prefix. By default
this refuses to bind on non-loopback addresses. To expose
unauthenticated on a public IP (you'd better know what you're
doing — typically behind your own auth proxy):

```sh
thclaws --serve --gui-shell my-image-bot \
        --gui-shell-no-auth --gui-shell-no-auth-allow-public \
        --bind 0.0.0.0 --port 8080
```

The same guardrail pattern as `--dangerously-skip-permissions`
(Chapter 5).

### Serve defaults from `settings.json`

If `--gui-shell` is omitted, the launcher reads
`guiShell.serveDefault` (or the shorthand `guiShell` if it's a
string) from `settings.json`. If neither is set, `--serve` keeps its
current behaviour — serves the regular React frontend.

### Multi-tenant — one shell, many users

Everything above ("Mode B") is **single-tenant**: every visitor to
the URL shares one agent + one session + one storage. That's the
right model when you're sharing a shell with a teammate or running
it for yourself from your phone.

When you want to *host* a shell for many users — each with their own
conversation, their own gui-shell storage, their own output files —
add `--multi-tenant` and a shared HMAC secret:

```sh
thclaws --serve --gui-shell my-image-bot \
        --multi-tenant \
        --multi-tenant-secret "$THCLAWS_CLOUD_HMAC_SECRET" \
        --port 8080
```

(The `--multi-tenant-secret` flag also accepts `THCLAWS_CLOUD_HMAC_SECRET`
from the environment, which is the common deployment pattern.)

This mode expects requests to arrive from a trusted routing layer
(typically thClaws.cloud) that attaches three signed headers per
request:

```
X-Thclaws-User:       <user_id>           # filesystem-safe, [a-zA-Z0-9_-], ≤64 chars
X-Thclaws-User-Ts:    <unix_seconds>
X-Thclaws-User-Proof: hex(HMAC-SHA256(secret, "<user_id>:<ts>"))
```

What you get:

- **Separate agent + session per user** — alice and bob hosted in
  the same pod see independent conversations.
- **Per-user storage** — `thclaws.storage.set("notes", …)` from
  alice's shell goes to `users/alice/storage/<shell>/…`; bob's goes
  to `users/bob/...`. No collisions on the same key.
- **Per-user output** — files the agent generates land at
  `output/users/<id>/...` and the file-asset URL won't serve another
  user's subtree even if the URL is guessed.
- **LRU + idle eviction** — `--multi-tenant-max-users 1000` (default)
  and `--multi-tenant-idle-timeout 30m` (default) bound resource use.
- **Restart-resumable** — alice's session JSONLs survive pod restart;
  she reconnects and her prior conversation reloads from disk.

The shell author writes **the same shell** as for single-tenant Mode
B — no code change required. The bridge automatically routes
storage / file-asset calls through the per-user prefix.

This is what powers thClaws.cloud (dev-plan/34). For the full
contract — HMAC signing recipe, on-disk layout, registry semantics,
curl smoke recipe, what Tier 1 does NOT include (object storage,
cross-pod state portability, cgroup-style resource limits) — see
[`thclaws-technical-manual/multi-tenant-serve.md`](../thclaws-technical-manual/multi-tenant-serve.md).

---

## Installing a custom shell  *(Tier 2)*

A shell is just a folder. Drop it in one of two places:

```
~/.config/thclaws/gui-shell/<id>/      # cross-project, every workspace sees it
./.thclaws/gui-shell/<id>/              # repo-scoped, project override by id
```

The folder must contain:

```
<id>/
  manifest.json         # see below
  index.html            # entry point — the bridge is injected at serve time
  ...                   # any CSS / JS / images / fonts
```

Minimum `manifest.json`:

```json
{
  "id": "hello-shell",
  "name": "Hello Shell",
  "version": "0.1.0",
  "description": "Smallest possible shell.",
  "entry": "index.html",
  "icon": "icon.svg",
  "minBridgeVersion": "1",
  "permissions": ["agent.run"]
}
```

Then in the GUI: open the picker, click **"Refresh shells"** —
your shell appears alongside the built-ins.

**Project shell overrides a user shell** with the same id. Useful
when a team wants to ship a customised version of a public shell
to everyone in the repo.

### Tier 3 — install from a git URL

```sh
thclaws shell install https://github.com/someone/cool-shell
thclaws shell install ./mything --scope project   # default scope: user
thclaws shell list
thclaws shell remove cool-shell
```

On first install, a permission prompt summarises what the shell
declares it needs:

> *"This shell wants to: run the agent, invoke
> `mcp__pinn_ai__text2image`, store data in `<shell-root>/state/`,
> read your sessions. Allow?"*

Grants are persisted at `~/.config/thclaws/gui-shell-grants.json`
(user-scoped — a teammate cloning the repo doesn't inherit your
trust decision). Revoke from the picker's context menu, or via
`thclaws shell remove`.

---

## Authoring your own shell  *(Tier 3)*

A shell is HTML + CSS + JS. No build step required.

### Starter template

```sh
git clone https://github.com/thclaws/gui-shell-template my-shell
cd my-shell
make dev          # under the hood: thclaws shell dev .
```

`make dev` mounts your folder as a temporary shell with file-watch
+ auto-reload. Edit `index.html` / `main.js` / `manifest.json`,
save, the iframe refreshes automatically. No thClaws rebuild needed.

### The bridge — `window.thclaws.*`

Your shell's JavaScript gets exactly one global. Everything is
async.

```js
// Identity
thclaws.shell.id          // "hello-shell"
thclaws.shell.sessionId   // session this tab is bound to
thclaws.transport         // "tauri" (Mode A) or "ws" (Mode B)

// Run the agent — same loop that powers Chat/Terminal
const { runId } = await thclaws.run("Summarise this in one line.");

// Cancel an in-flight turn (equivalent of Cmd+. in Chat)
thclaws.cancel(runId);

// Subscribe to streaming events
const unsubscribe = thclaws.on("text", (chunk) => render(chunk));
thclaws.on("tool_call",   (call)   => …);   // Tier 2
thclaws.on("tool_result", (result) => …);   // Tier 2
thclaws.on("done",        ()        => …);
thclaws.on("error",       (err)     => …);

// Direct tool invocation — bypass the agent loop for deterministic actions
// (Tier 2; manifest must declare `tools.invoke:<name>` in Tier 3).
// `<name>` is whatever tool you've registered — typically an MCP tool
// like `mcp__pinn_ai__text2image` (sanitised from the server name) or
// a built-in like `Ls`. Prefer thclaws.run() + an AGENTS.md playbook
// for most shells — it composes with whatever provider stack the
// user has configured. See the Image Generator example shell.
const result = await thclaws.tools.invoke("mcp__your_server__your_tool", { … });

// Per-shell, per-session storage
// (Tier 2; file-backed at <shell-root>/state/<sessionId>.json)
await thclaws.storage.set("last_query", query);
const last = await thclaws.storage.get("last_query");
```

The bridge is **the only API**. Shells cannot reach the workspace
filesystem, the network (unless `network.outbound:<host>` is
declared in Tier 3), or any other shell's storage. Two shells'
`storage` namespaces are isolated by id.

### Permissions (Tier 3)

Declare what your shell does in `manifest.json::permissions`:

| Permission | Allows |
|---|---|
| `agent.run` | `thclaws.run()` and event subscription |
| `tools.invoke:<name>` | direct `thclaws.tools.invoke("<name>", …)` per tool |
| `session.read` / `session.list` | read sidecar session data |
| `fs.shell-scoped` | read/write inside the shell's resolved root |
| `network.outbound:<host>` | `fetch()` to that host (CSP injected at serve time) |

Users see this list before installing. Anything not declared throws
at call time.

### Doctor

```sh
thclaws shell doctor my-shell
# checks: manifest valid, entry exists, permissions sensible,
# no Tauri-only APIs that would break in Mode B, no external links
# that would leak the serve token via Referer.
```

---

## Sessions and persistence

A shell session is a normal thClaws session. Same JSONL format,
same location (`./.thclaws/sessions/<id>.jsonl`), same `--resume`
machinery. The only addition is an optional `shell: { id, version }`
field on the session header — non-shell sessions write byte-
identical JSONL to before, so `cat` still works on everything.

```sh
# Look at a shell session like any other
cat ./.thclaws/sessions/sess-abc123.jsonl | head -3
# {"type":"header","id":"sess-abc123","shell":{"id":"image-generator","version":"0.1.0"},…}
# {"type":"user","content":"generate a picture of a sunset"}
# {"type":"assistant","content":[…]}
```

Closing a shell tab persists the session. Reopening from the
picker's "Past sessions" sub-list resumes it. A session stamped
with a `shell.id` only opens in that shell — there is no
generic-chat fallback view in v1 (it's a Tier 3+ open question).

---

## Cost awareness

A shell that calls `thclaws.run()` consumes the same tokens as a
Chat-tab turn. A shell that calls `thclaws.tools.invoke()`
directly skips the agent loop entirely — no model tokens for that
call, just the tool's own cost (e.g. image-generation provider
charges).

In Tier 3, manifests can declare a daily token budget and the
permission prompt surfaces it ("Allow up to 50k tokens/day?"). The
existing budget accounting tracks usage; over-budget shells get a
rejected promise from `thclaws.run()`.

---

## What's missing in Tier 1

Tier 1 ships Mode A with one built-in shell (Session Explorer) and
the `run` / `cancel` / `on("text"|"done"|"error")` bridge surface.
Documented gaps land in Tier 2 / 3 per
[dev-plan/33](../dev-plan/33-gui-shell.md):

- **No picker UI.** New-tab menu has one entry ("Open Session
  Explorer"); Tier 2 adds the grid.
- **No custom shells.** Only the embedded built-in is discoverable;
  Tier 2 adds `~/.config/thclaws/gui-shell/` + `./.thclaws/
  gui-shell/` discovery.
- **No `tools.invoke` / `storage` bridge methods.** Tier 1 ships
  `run` / `cancel` / `on` only. Tier 2 widens the surface.
- **No serve mode.** Mode B (`--serve --gui-shell`) lands in
  Tier 2.
- **No permission enforcement.** Manifests can declare permissions
  in Tier 1, but they aren't checked at call time. Tier 3 enforces.
- **No SDK / dev mode.** `thclaws shell dev` + starter template
  land in Tier 3.

---

## Security model — what each mode actually protects

- **Mode A iframe sandbox** — every shell runs inside an `<iframe
  sandbox="allow-scripts allow-same-origin">`. A buggy shell that
  calls `document.location = "…"` cannot navigate the parent GUI
  away. Per-shell origin separation (subdomain in the custom
  protocol) prevents two shells from reading each other's cookies
  / localStorage.
- **Mode B token-in-path** — 160-bit per-shell tokens. Silent 404
  on missing/wrong tokens (no auth challenge advertised). Per-IP
  rate limit on token-prefix attempts. Referer stripping
  (Permissions-Policy header + `<meta name="referrer">`) to prevent
  token leakage when the shell links externally.
- **Path traversal** — both modes call the same `Sandbox::check_in
  (&shell_root, &rel)` helper. URL-decoded `..` sequences collapse
  via lexical normalize → canonicalize → `starts_with` check.
- **Tool invocation** — Tier 3 permission gating means a shell
  cannot call a tool it didn't declare. Permission grants are per
  shell per user, stored in `~/.config/thclaws/gui-shell-grants
  .json`, revocable from the picker.

What is **not** protected:

- The shell author. You're trusting their code with your agent
  session. There is no marketplace verification in v1; Tier 3 adds
  the marketplace catalog kind but governance ultimately depends
  on who you install from.
- Network exposure of `--gui-shell-no-auth-allow-public`. The flag
  is named that way for a reason — read Chapter 5 first.

---

## Quick reference

| Goal | Command / location |
|---|---|
| Try Session Explorer now (Tier 1) | thClaws GUI → New Tab → Open Session Explorer |
| Open the shell picker (Tier 2) | thClaws GUI → New Tab → GUI Shell |
| Set default shell for "New Shell" | `"guiShell": "<id>"` in `settings.json` |
| Install someone's shell (manual) | drop folder in `~/.config/thclaws/gui-shell/<id>/` → Refresh |
| Install from git (Tier 3) | `thclaws shell install <git-url>` |
| Serve a shell over HTTP (Tier 2) | `thclaws --serve --gui-shell <id> --port 8080` |
| Pin the serve URL | add `--gui-shell-token <token>` |
| Rotate compromised URL | `thclaws shell rotate-token <id>` |
| List installed shells (Tier 3) | `thclaws shell list` |
| Develop a new shell (Tier 3) | clone template, `make dev` |
| Remove a shell (Tier 3) | `thclaws shell remove <id>` |
| Look at a shell session | `cat ./.thclaws/sessions/<id>.jsonl` |

---

## Troubleshooting

**"Shell tab is blank / spinner forever"** — open the WebView
devtools (`THCLAWS_DEVTOOLS=1 thclaws`) and check the iframe's
console. Common causes: shell's `index.html` has a strict CSP that
blocks the injected bridge script (Tier 3 adds a manifest
`cspMode: "managed"` field); shell's JS throws before calling
`thclaws.on()` so no events ever bind.

**"Mode B URL returns 404"** — confirm the URL includes the
`/t/<token>/` prefix and trailing slash. The token is printed to
the launcher's stdout; if you lost it, check
`~/.config/thclaws/gui-shell-tokens.json`. URLs without the token
404 by design (no auth challenge advertised).

**"Shell can't call a tool"** — Tier 3: manifest didn't declare
`tools.invoke:<name>`. Add it, restart thClaws (or `Refresh
shells`), re-approve the new permission.

**"Two shells share storage"** — they shouldn't. Confirm they have
distinct `manifest.json::id` values; `storage` is namespaced by
id. If the ids are different and storage still leaks, file a bug —
that's a sandbox failure.

**"Headless serve refuses to start with `--gui-shell-no-auth`"** —
intended. `--gui-shell-no-auth` only allows loopback binds; add
`--gui-shell-no-auth-allow-public` *and* re-confirm you have your
own auth in front of it.

**"`Sandbox::check_in` rejected my asset"** — the path resolved
outside the shell's folder. Usually a relative URL with too many
`../` in it, or a symlink pointing outside. Both modes apply the
same check — if it fails in the desktop tab, it'll fail in serve
mode for the same reason.
