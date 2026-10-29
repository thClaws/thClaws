---
name: gui-shell
short_description: Scaffold a custom GUI Shell (HTML frontend) on top of thClaws
description: Build a custom HTML frontend ("GUI Shell") that runs as a sandboxed tab inside the thClaws desktop GUI and talks to the agent via the window.thclaws.* bridge. Invoke when the user asks to "build a UI for …", "make a custom view / dashboard / form / gallery", or "scaffold a frontend on top of thClaws" — they mean a GUI Shell. This skill enables the GuiShellCreate / GuiShellWriteFile / GuiShellList / GuiShellRemove tools.
tool-gate: gui-shell
---

# GUI Shells (authoring)

A **GUI Shell** is a custom HTML frontend the user opens as a tab inside the thClaws desktop GUI. It runs in a sandboxed iframe and talks to the agent through a `window.thclaws.*` bridge. When the user asks to "build a UI for X", "make a custom view / dashboard / form / gallery", or "scaffold a frontend on top of thClaws", build one of these.

## Use the tools — do NOT hand-write files

Invoking this skill enabled four tools. Use them instead of `Write`-ing into the shell folder yourself — they validate the manifest, jail writes to the shell folder, and resolve scope/paths for you:

- **`GuiShellCreate`** — scaffold a new shell. Pass `id` (kebab-case), `name`, `description`, `entry_html` (the full index.html), optional `permissions[]`, and optional `files{}` (relpath → content for style.css / main.js / icon.svg / AGENTS.md). Validates and writes the folder. Pick `scope`: `"project"` (./.thclaws/gui-shell/, this repo) or `"user"` (~/.config/thclaws/gui-shell/, all projects). Project wins on id clash.
- **`GuiShellWriteFile`** — iterate on one file (`id`, `relpath`, `content`). Refuses manifest.json — change the manifest by re-running `GuiShellCreate`.
- **`GuiShellList`** — show installed shells.
- **`GuiShellRemove`** — delete a shell (asks for approval).

## Manifest fields (passed as `GuiShellCreate` params, not raw JSON)

`id`, `name`, `version` (default 0.1.0), `description`, `entry` (default index.html), `icon`, and `permissions`. Permissions are declared up front and enforced at call time:

- `agent.run` — run the agent loop
- `tools.invoke:<tool-name>` — direct deterministic tool invocation
- `session.read` / `session.list` — read sidecar session data
- `fs.shell-scoped` — read/write inside the shell folder
- `network.outbound:<host>` — `fetch()` to that host (CSP injected)

Anything not declared throws. Request the minimum the shell needs.

## Bridge — `window.thclaws.*`

The bridge is the ONLY API the shell has. The iframe sandbox blocks direct workspace fs access, cross-shell storage leaks, and arbitrary network egress.

```js
// identity
thclaws.shell.id; thclaws.shell.sessionId; thclaws.transport;  // 'tauri' | 'ws'

// agent loop (same engine as Chat/Terminal)
const { runId } = await thclaws.run("user message");
thclaws.cancel(runId);
thclaws.on("text" | "tool_call" | "tool_result" | "done" | "error", cb);
// cb receives the payload itself, never an envelope, and none carries a runId:
//   "text"        → string (one streamed delta — append it)
//   "done"        → null   (the turn finished)
//   "error"       → { error }
//   "tool_call"   → { name, label, input }
//   "tool_result" → { name, output }
// Only one turn runs at a time, so track "my run is in flight" with a flag,
// not by matching ids. Never dereference the "done" payload.

// direct tool invocation — skips the model, deterministic
const out = await thclaws.tools.invoke("ToolName", args);

// per-shell, per-session persistence (file-backed JSON)
await thclaws.storage.set(key, value);
const v = await thclaws.storage.get(key);
```

You don't ship the bridge — it's injected into every shell's `<head>` at serve time.

## Shared chrome — `<thc-header>` (also injected)

Alongside the bridge, every shell gets a shared theme (design tokens keyed on the host's `data-theme`) and a `<thc-header>` navbar component — so you don't hand-roll a header or re-declare colors. Use it as your top bar:

```html
<thc-header label="My Studio">
  <svg slot="icon" ...>…</svg>                       <!-- optional brand icon -->
  <button slot="actions" id="search" title="Search">…</button>  <!-- your buttons -->
</thc-header>
```

```js
document.querySelector("thc-header").setStatus("working…");   // right-side status text
```

The right side is, left → right: `[status] [bridge-status] [your slot="actions" buttons] [theme toggle] [full-screen toggle]`. The **theme** and **full-screen** toggles are standard and always pinned far-right — the component wires them to `thclaws.ui` for you (theme toggle appears only in full-screen / standalone, where the app's own switch is out of reach). Add your own controls via `slot="actions"` (e.g. a search button) and wire their clicks in your `main.js`; buttons with no class get `.thc-iconbtn` to match. Don't re-implement an exit-fullscreen or theme button yourself — use these.

### `<thc-model>` — active-model widget (permission-gated)

A drop-in widget showing / switching the active model. Its behaviour is decided entirely by the **manifest permissions** you declare — no flag, no widget:

- `model.read` → read-only badge of the current model.
- `model.write` → the same picker as the main-app sidebar: a trigger that opens a searchable, provider-grouped dropdown (tier dividers, context sizes, current highlighted) to switch **provider + model** across the whole catalogue (the provider follows the chosen model id).
- neither → renders nothing.

**Standard placement is the sidebar, not the navbar.** Use `<thc-sidebar>` for your left column — it pins the model picker at the top automatically (it just disappears when the shell has no `model.*` permission) and makes everything you put inside the scrollable body below:

```html
<div class="app">
  <thc-sidebar>
    <div class="rail-header">…</div>   <!-- your sidebar content -->
    <div class="rail-body">…</div>
  </thc-sidebar>
  <main>…</main>
</div>
```

Add `no-model` to `<thc-sidebar>` to opt out of the auto picker, or drop a bare `<thc-model>` anywhere if you want it elsewhere. Declare the permission(s) in `GuiShellCreate` `permissions: ["model.read"]` (or `["model.read","model.write"]`). The matching bridge API is `thclaws.model.get()` / `.list()` / `.set(id)` / `.onChange(cb)` if you'd rather build your own UI. Switching the model is app-wide and persists, so every shell's `onChange` fires.

## Deterministic data APIs (no LLM) — `thclaws.kms.*` / `thclaws.research.*`

Read workspace data directly instead of prompting the agent for it — no token cost, nothing in the shared transcript, and the values are real (not model-inferred). Each is permission-gated:

- **`kms.read`** → `thclaws.kms.list()` → `{kmss:[{name, scope, active, pages}]}`; `thclaws.kms.browse(name)` → `{kms, pages, sources}`.
- **`research.read`** → `thclaws.research.list()` → `{jobs:[{id, query, status, phase, iterations_done, source_count, score, kms_target, result_page, error}]}` from the live research-job registry; `thclaws.research.get(id)` → `{job|null}`. `status` ∈ `pending|running|done|cancelled|failed`.

Rule of thumb: if a panel shows workspace *state* (a list, a status, counts), read it with these — never a sentinel prompt through `thclaws.run()`. That path costs a full model turn each poll and can hallucinate. (research-console's rail uses these.)

## Install + iterate

1. `GuiShellCreate` writes the folder.
2. GUI → "+ New Tab" → "GUI Shell" → click **Refresh shells** → the new tile appears.
3. Click the tile to open. No restart; the bridge is injected at iframe load.
4. After edits with `GuiShellWriteFile`, close + reopen the shell tab (no hot-reload in v1).

Set a project default in `.thclaws/settings.json`: `{ "guiShell": "<id>" }` — "+ New Tab" then opens this shell directly instead of the picker.

## Reference shells

Source-bundled at `<thclaws-source>/crates/core/assets/gui-shells/`:
- `chatbot/` — minimal `thclaws.run()` + `thclaws.storage` demo (~120 LOC)
- `session-explorer/` — tree-of-sessions browser with on-demand summaries

Full reference: user-manual chapter 26 (`ch26-gui-shells`).
