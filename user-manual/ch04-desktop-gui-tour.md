# Chapter 4 — Desktop GUI tour

Launching `thclaws` with no arguments opens the native desktop app: a wry webview wrapping a React frontend that talks to the same Rust agent core as the CLI. This chapter is a guided tour of the main window — read it once so you recognize every UI region when you need it.

If you only ever use the terminal REPL, you can skim this chapter and move on. Everything the GUI does is also available via slash commands.

> **First-launch setup** — the first time you open thClaws you'll see two modals in sequence (pick a working directory, then pick where API keys are stored). Both are covered in [chapter 3](ch03-working-directory-and-modes.md#first-launch-setup). This chapter assumes you're past them.

## The main window — layout

![thClaws main window — Terminal tab active, sidebar showing Provider / Sessions / Knowledge / MCP sections](../user-manual-img/ch-01/main-window-layout.png)

- **Tab bar** (top) — Terminal, Chat, Files, optional Team. Window title shows the current project.
- **Sidebar** (left column) — four collapsible sections covering active provider + model, saved sessions, attached knowledge bases, and configured MCP servers.
- **Active tab content** (right) — whatever tab you're on: a live terminal, a streaming chat, a file browser, or the team view.
- **Status bar** (bottom) — current working directory on the left, gear icon for Settings on the right.

### Sidebar (left column)

The sidebar is always visible and holds four sections:

| Section | Shows | Actions |
|---|---|---|
| **Provider** | Active provider + model, ready/not-ready dot, ▾ chevron | Click to open the inline model picker (v0.7.2+) |
| **Sessions** | Last 10 saved sessions (title or ID) | `+` to start a new session · hover row → pencil to rename · click to load |
| **Knowledge** | Every discoverable KMS with attach checkbox | `+` to create a new KMS — see [chapter 9](ch09-knowledge-bases-kms.md) |
| **MCP Servers** | Active MCP servers + their tool count | Read-only here — configure via `/mcp add` |

The **Provider** section has a visual health indicator:

- 🟢 green dot + normal text: provider ready to use
- 🔴 red dot + ~~strikethrough~~ + "no API key — set one in Settings": provider has no credentials

When a key is saved via Settings, the dot flips to green and the active model may auto-switch to the first provider with credentials — see [chapter 6](ch06-providers-models-api-keys.md#auto-switch-on-key-save).

**Inline model picker** (v0.7.2+): clicking the Provider row opens a search-as-you-type dropdown listing every model the catalogue knows about, grouped by provider, plus any local Ollama models discovered live via `/api/tags`. Click a row to switch — the change persists to `.thclaws/settings.json` and the active provider rebuilds in place (same path as `/model`). Esc or click-outside dismisses without changing.

### Tab bar

Four main tabs, plus the settings gear on the right.

#### 1. Terminal tab

An embedded xterm.js terminal running `thclaws --cli` (the same REPL you get from the CLI). Keystrokes go through a PTY bridge to the child process; output streams back via base64-encoded frames.

![Terminal tab — agent just finished scaffolding a static site; `[tool: Write …]` lines show each file being created and the token/time totals appear at the bottom](../user-manual-img/ch-04/thClaws-gui-terminal.png)

Key behaviors worth knowing:

- **Copy / paste** — Cmd+C / Cmd+V (macOS) or Ctrl+Shift+C / Ctrl+Shift+V (Linux/Windows). These go through a native `arboard` IPC bridge because wry blocks `navigator.clipboard`.
- **Ctrl+C** is context-sensitive: if the current typed line is non-empty, it clears the line (like `Ctrl+U` in bash); if the line is empty, it passes through as SIGINT.
- **Resize** — the terminal size follows the window, propagated via `portable-pty` resize.
- **Ctrl+L** clears the screen.

#### 2. Chat tab

A streaming chat panel that shares history with the Terminal tab (same agent, same session). Messages render as Markdown; tool calls show collapsible `[tool: Name]` blocks; token usage appears after each assistant response.

Use the Chat tab when you prefer a conversational UI; use the Terminal tab when you want to see raw output and run slash commands.

#### 3. Files tab

A filesystem browser rooted at the working directory. Click a file in the tree to open it in the right-hand pane; click the pencil icon next to the path to switch to edit mode.

**Preview mode** (default):

- `.md` files — rendered to HTML server-side (GFM tables, task lists, strikethrough, autolinks, footnotes), displayed in a sandboxed iframe. Raw HTML inside markdown is stripped before rendering.
- `.html` files — rendered in the same sandboxed iframe.
- Code files (`.js`, `.ts`, `.tsx`, `.py`, `.rs`, `.go`, `.java`, `.cpp`, `.php`, `.json`, `.yaml`, `.sql`, `.xml`, `.css`, and more) — syntax-highlighted via CodeMirror 6 in read-only mode, with line numbers, bracket matching, and a search panel.
- Images and PDFs — inline preview.
- Plain text / config files (`.txt`, `.log`, `.env`, `.conf`, `.ini`, `.toml`, `.sh`, `Dockerfile`, …) — plain `<pre>` block.

![Files-tab preview mode — `script.js` rendered through CodeMirror with line numbers and syntax highlighting; the Edit button on the top-right switches to edit mode](../user-manual-img/ch-04/thClaws-gui-file-viewer.png)

`.html` files render live in the sandboxed iframe, so you see the page as a browser would — styles, images, and interactive JS intact:

![Files-tab HTML preview — `index.html` rendered inside the sandboxed iframe, complete with stylesheet, image, and clickable button](../user-manual-img/ch-04/thClaws-gui-file-html-viewer.png)

**Edit mode** (pencil icon):

- Markdown opens in a **TipTap WYSIWYG** editor — the same round-tripping editor used for `AGENTS.md` in the Settings menu.
- Code files open in **CodeMirror 6** with per-language syntax highlighting, bracket matching, undo history, and a search panel. The language is picked from the file extension.
- A filled dot (●) next to the filename marks unsaved changes. The Save button is disabled until the buffer is dirty.
- **Cmd/Ctrl+S** saves. A green "saved" or red "save failed: …" toast confirms.
- **Discard** (shown while dirty) / **Preview** (shown while clean) exits edit mode. Clicking Discard pops a native OS confirm dialog ("Discard / Keep editing") before dropping edits.
- Clicking a different file in the sidebar while dirty also pops the same native confirm — save or discard before navigating away.
- Auto-refresh polling pauses while you're editing, so a concurrent `Write`/`Edit` tool call from the agent can't clobber your in-progress buffer.

![Files-tab edit mode — `index.html` opened in CodeMirror; the ● after the filename marks unsaved changes, and the Save / Discard controls appear in the top-right](../user-manual-img/ch-04/thClaws-gui-file-editor.png)

Files are written through the same working-directory sandbox the agent uses, so edits stay inside the project tree. User-initiated saves do **not** go through the agent approval prompt — the Save button is your approval.

**Refresh button** — re-fetches the file from disk and remounts the preview iframe. Use it after the agent updates a file behind the scenes (e.g. the productivity plugin's `dashboard.html` regenerating its inlined task snapshot). Forces the iframe to re-render rather than relying on browser cache. Prompts before discarding any unsaved editor changes.

**Dashboard host bridge** — self-contained HTML dashboards opened in this tab can read and write sibling files via `postMessage` to the React shell, no File System Access API picker required. The productivity plugin's `dashboard.html` uses this: on Refresh it re-reads `TASKS.md` live from the bridge (no more snapshot-staleness), and Save writes back to disk through thClaws's `file_write` IPC. Any HTML page that posts `{type: "thclaws-dashboard-load" | "thclaws-dashboard-save", filename, content?}` to its parent works the same way.

#### 4. Team tab

Always present. When no team exists it shows an empty-state pointer ("No team agents running — ask the agent to create a team"). Once the agent calls `TeamCreate`, each teammate gets its own pane in this tab — click a pane to focus, scroll to browse history, type into it to send input. The agent only has access to the team-spawning tools (`TeamCreate`, `SpawnTeammate`, `SendMessage`, …) when `teamEnabled: true` is set in `.thclaws/settings.json`; the tab itself surfaces regardless. See [chapter 17](ch17-agent-teams.md) for the team concept.

### Settings menu (gear icon)

Click the gear ⚙ in the top-right to open a popup menu:

| Item | Opens |
|---|---|
| **Global instructions** | Tiptap markdown editor on `~/.config/thclaws/AGENTS.md` |
| **Folder instructions** | Tiptap editor on `./AGENTS.md` (in the working directory) |
| **Provider API keys** | Settings modal for keys — see [chapter 6](ch06-providers-models-api-keys.md) |
| **Appearance** | Light / Dark / System theme toggle — see below |
| **GUI scale** | Zoom factor for HiDPI / 4K displays — 75 / 90 / 100 / 110 / 125 / 150 / 175 / 200% (v0.7.3+) |

The Tiptap editor round-trips markdown through `tiptap-markdown`: you edit in a rich-text UI (headings, bold, lists, code fences), save to disk as markdown, and the agent reads the file on its next turn. No lossy conversion for standard Markdown.

The path shown at the top of the editor is the resolved filename so you always know exactly what you're editing.

### Appearance (Light / Dark / System)

The bottom of the gear menu has three theme options — Light, Dark, System — each with a check next to the active one. Clicking a theme applies immediately and persists to `~/.config/thclaws/theme.json` (per-user; never committed with your project). The menu deliberately stays open when you click a theme so you can try all three without reopening the gear.

**Light** and **Dark** are explicit overrides — they are honoured even if your OS is set to the opposite scheme. **System** follows `prefers-color-scheme` and flips live when the OS appearance changes (macOS Appearance, Linux DE theme, Windows personalization) without an app restart.

### GUI scale (v0.7.3+)

Below the theme rows, **GUI scale** is a dropdown of zoom presets that tunes WebView text size for HiDPI / 4K displays without changing OS-level display scaling. Pick a preset (75–200%) and the entire app scales live — Chat, Terminal, Files, Settings, sidebar — same primitive used by VS Code and Slack. The value persists per-project to `.thclaws/settings.json` as `guiScale: <number>` and is reapplied on every launch.

Use case: a 4K laptop screen at 100% Windows scaling renders thClaws text too small relative to other dev tools. Bump to 125% or 150% to match without affecting any other app.

The theme covers every surface:

- App chrome (tabs, sidebar, status bar, menus) — via CSS custom properties
- Terminal tab — xterm.js palette swaps live, scrollback preserved
- CodeMirror editor/preview — dark uses `oneDark`, light uses the default highlighter
- Files-tab Markdown preview — comrak re-renders with the matching palette baked into the iframe

## Keyboard shortcuts

These work anywhere in the app (including the Terminal tab):

| Shortcut | Action |
|---|---|
| Cmd/Ctrl+C | Copy selection |
| Cmd/Ctrl+X | Cut selection |
| Cmd/Ctrl+V | Paste from clipboard |
| Cmd/Ctrl+A | Select all (in text inputs) |
| Cmd/Ctrl+Z | Undo (in text inputs) |
| Cmd+Q (macOS) | Quit |

Inside the Terminal tab specifically:

| Shortcut | Action |
|---|---|
| Ctrl+C | Clear line if non-empty, else SIGINT |
| Ctrl+L | Clear screen |
| Ctrl+U | Kill line (standard bash) |

## Sidebar polling + cross-process updates

The sidebar polls the Rust backend every 5 seconds for config changes, so if you type `/model gpt-4o` in the Terminal tab, the Chat tab's active-model display updates within 5 seconds without a restart.

When you save an API key via Settings, both the GUI and any child PTY-REPL can read the keychain entry on the next request — no need to restart either process.

## Session sharing

Terminal tab and Chat tab **share the same session**. History scrolls together; `/save` in one persists both. If you load a saved session from the sidebar, both tabs reflect it.

## Where things are stored

| What | Where |
|---|---|
| Window size | `.thclaws/settings.json` → `windowWidth` / `windowHeight` |
| Recent working directories | `~/.config/thclaws/recent_dirs.json` |
| Secrets backend choice | `~/.config/thclaws/secrets.json` |
| API keys (keychain mode) | OS keychain, service `thclaws`, account `api-keys` (JSON blob) |
| API keys (.env mode) | `~/.config/thclaws/.env` |
| Sessions | `.thclaws/sessions/` (project-scoped) — see [chapter 7](ch07-sessions.md) |
| KMS (user) | `~/.config/thclaws/kms/` — see [chapter 9](ch09-knowledge-bases-kms.md) |
| KMS (project) | `.thclaws/kms/` inside the working directory |
| MCP servers (user) | `~/.config/thclaws/mcp.json` |
| MCP servers (project) | `.mcp.json` or `.thclaws/mcp.json` |

## Changing the working directory mid-session

Settings menu → "Change working directory" opens a folder picker.
After you pick a new folder, the GUI:

1. `cd`s the process to the new folder
2. Re-initialises the filesystem sandbox to match the new root (see [chapter 5](ch05-permissions.md#sandbox-filesystem))
3. **Reloads `ProjectConfig` from the new project's `.thclaws/settings.json`** — if the new `model` differs from the running one, the provider/agent get swapped and a **fresh session is minted** (the old provider's message history can't always reflow into a different provider's schema; safer to start clean)
4. Rebuilds the system prompt (cwd is embedded in it)
5. Broadcasts a line in Terminal/Chat: `[cwd] /new/path → model: X (was: Y)` so you can see the swap actually happened

The contract is **"project settings win"** — the new project's `.thclaws/settings.json` overrides every other layer (user config, env vars, the previous session's effective config) the moment you change directory. If you don't want a model swap on cwd change, make sure the new project's `settings.json` has the same `model` as before.

## When to use CLI instead of GUI

Use the CLI (`thclaws --cli`) when you want:

- SSH sessions / headless servers (no webview)
- Faster cold start (no webview initialization)
- Scripting / piping with `thclaws -p "prompt"` non-interactive mode

Everything the GUI exposes is available via slash commands in the CLI — the two are peer UIs on the same engine, not parent/child.

[Chapter 3](ch03-working-directory-and-modes.md) goes deeper on the working directory, modes, and command-line flags.
