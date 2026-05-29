# Chapter 11 — Built-in tools

thClaws ships with around thirty built-in tools. The agent picks them
autonomously; you see each call as a `[tool: Name: …]` line, then a
✓ (success) or ✗ (error). This chapter is the reference.

## File tools

| Tool | Approval | Summary |
|---|---|---|
| `Ls` | auto | Non-recursive directory listing |
| `Read` | auto | Read a file (whole or line-range slice) |
| `Glob` | auto | Shell-glob pattern matching; respects `.gitignore` |
| `Grep` | auto | Regex search across files; respects `.gitignore` |
| `Write` | prompt | Create or overwrite a file |
| `Edit` | prompt | Exact string replacement (fails if non-unique) |

All of them are scoped to the sandbox ([Chapter 5](ch05-permissions.md)).
For large files the agent is trained to use `Glob` + `Grep` first to
narrow down, then `Read` with a line range, rather than slurping the
whole file — but there is no hard size cap enforced by the tool, so
`Read` on a multi-gigabyte file will try to load it. If you need a
binding upper bound, run in `ask` mode and deny the call.

## Shell

| Tool | Approval | Summary |
|---|---|---|
| `Bash` | prompt | Run a shell command via `/bin/sh -c` |

Defaults:

- 2-minute timeout (override with `timeout_ms` up to 10 min).
- Output over 50 KB truncated; full text saved to `/tmp/thclaws-tool-output/<id>.txt`.
- Destructive patterns (`rm -rf`, `sudo`, `curl | sh`, `dd`, `mkfs`,
  `> /dev/sda`) flagged with `⚠` before the approval prompt.
- Long-running servers: the agent is trained to either run them in
  the background (`... &`) or wrap them in `timeout 10` so the turn
  can't hang.
- Python `venv` auto-activated if `./.venv/bin/activate` exists (the
  tool sources the `activate` script before running).

## Web

| Tool | Approval | Summary |
|---|---|---|
| `WebFetch` | prompt | HTTP GET (100 KB body cap, per section). When `HAL_API_KEY` is set, runs **both** a HAL headless-browser scrape **and** a plain HTTP GET in parallel and returns a single combined response with each section labelled (see below). |
| `WebSearch` | prompt | Web search via Tavily / Brave / DuckDuckGo |
| `WebScrape` | prompt | Direct HAL scrape with advanced parameters (`wait_for` CSS selector, `scroll_to_bottom`, `remove_selectors`, `output_format`) — appears only when `HAL_API_KEY` is set |
| `YouTubeTranscript` | prompt | Fetch YouTube captions via HAL (multi-language fallback, optional timestamps) — appears only when `HAL_API_KEY` is set |

Search provider is picked via `TAVILY_API_KEY` or `BRAVE_SEARCH_API_KEY`
if set, else DuckDuckGo (no key, lower quality). Override with
`searchEngine: "tavily"` in settings.

### `WebFetch` combine behavior (HAL_API_KEY set)

Pre-fix `WebFetch` did a single plain HTTP GET. With `HAL_API_KEY` configured it now fires both paths in parallel and returns the model a labelled dual-section response:

```
[via HAL scrape — JS-rendered + extracted to Markdown]

# {page title}

(rendered Markdown content)

---

[via plain HTTP GET — raw response body]

(raw HTTP body — preserves JSON, headers-style content, anything HAL might mangle)
```

The agent picks the slice that answers its question:

- **HAL section** for SPA / JS-rendered / docs / blog content
- **Plain GET section** for JSON APIs / sitemaps / robots.txt / anything where raw bytes matter

If one path fails, the other still comes back with a `[note: …]` line explaining the drop. `prefer_raw: true` skips HAL entirely (faster, half the tokens) — use when you know the URL is a JSON endpoint. `max_bytes` (default 100 KB) caps each section independently. Without `HAL_API_KEY`, `WebFetch` is just a plain GET as before.

### Service-key tools (HAL)

`WebScrape` and `YouTubeTranscript` call HAL's public API
(`hal.thaigpt.com/api`) — both gated on a single `HAL_API_KEY`. Paste
it in **Settings → Providers → Service keys → HAL Public API**, or set
`HAL_API_KEY` in your shell. The tools auto-appear in the model's
tool list when the key is present and disappear when it isn't, so
they never waste tokens or invite failed calls. Live key changes flip
them in/out on the next turn — no restart.

Reach for `WebScrape` directly only when you need advanced HAL parameters (`wait_for` CSS selector, `scroll_to_bottom` for lazy content, `remove_selectors` to strip nav/ads, or switching `output_format` to `html_markdown` / `json`). For ordinary page reads, prefer `WebFetch` so the model also gets the raw plain-GET payload alongside HAL's rendered output.

This requires-env pattern is general: any tool can declare `requires_env` and the registry filters it out when the listed env var(s) aren't set. The two HAL tools are the first concrete users.

## Documents — PDF & Office

Native Rust tools for producing and reading PDF, Word, Excel, and
PowerPoint files. **Clean-room ports of Anthropic's source-available
skills** so thClaws can redistribute them under MIT/Apache. Embedded
Noto Sans + Noto Sans Thai fonts ship in the binary (~650 KB total)
so Thai content renders correctly without a system-font dependency.

| Tool | Approval | Summary |
|---|---|---|
| `PdfCreate` | prompt | Markdown → PDF (printpdf + embedded Thai font, A4/Letter/Legal) |
| `PdfRead` | auto | Extract text via `pdftotext` (poppler-utils — `brew install poppler` / `apt install poppler-utils`) |
| `DocxCreate` | prompt | Markdown → Word (.docx) via `docx-rs` — headings, lists, code blocks |
| `DocxRead` | auto | Extract text from a Word doc (pure Rust XML walk) |
| `DocxEdit` | prompt | `find_replace` / `append_paragraph` in place |
| `XlsxCreate` | prompt | CSV or JSON 2D-array → Excel (.xlsx) via `rust_xlsxwriter` |
| `XlsxRead` | auto | Read XLSX/XLSM/XLSB/XLS/ODS via `calamine`; CSV or typed JSON output |
| `XlsxEdit` | prompt | `set_cell` / `set_cells` / `add_sheet` / `delete_sheet` — format-preserving via `umya-spreadsheet` |
| `PptxCreate` | prompt | Markdown outline → PowerPoint (.pptx); `# Heading` = new slide |
| `PptxRead` | auto | Extract text per slide (numeric ordering — slide10 doesn't sort before slide2) |
| `PptxEdit` | prompt | `find_replace` across all slides — designed for `{{placeholder}}` template fill |

**Thai rendering across formats:**

- `PdfCreate` embeds the Noto Sans Thai TTF directly in the PDF —
  Thai renders identically on every viewer regardless of installed
  fonts.
- `DocxCreate` / `PptxCreate` set `<w:rFonts w:cs="Noto Sans Thai"/>`
  / `<a:cs typeface="Noto Sans Thai"/>` per run, so Word and
  PowerPoint pick the Thai font from the user's system. Modern Win/
  Mac/Linux ship Noto Sans Thai by default; Office falls back to
  Tahoma / Cordia New if absent.
- `XlsxCreate` uses Calibri (Excel's default) — Excel's text engine
  handles Thai script via the OS Thai font stack with no per-cell
  configuration.

**Edit-tool semantics:**

- `DocxEdit` / `PptxEdit` `find_replace` matches **per text-run**.
  Word and PowerPoint split text across runs when style changes mid-
  paragraph (e.g. one bold word in a sentence), so a substring spanning
  a styled boundary won't match. For docs you authored with the
  matching `*Create` tool this is a non-issue (each block is a single
  run); for human-authored docs with rich styling, flatten styling
  first.
- `XlsxEdit` is **format-preserving** — `umya-spreadsheet` is built for
  round-trip; styles, formulas, charts, and conditional formatting in
  unrelated regions survive the load+modify+save cycle. Cells use A1-
  style addresses (`B7`, `AA12`).

## User interaction

| Tool | Approval | Summary |
|---|---|---|
| `AskUserQuestion` | auto | Pause the turn and ask you a typed question |
| `EnterPlanMode` | auto | Switch to planning mode (no mutations until ExitPlanMode) |
| `ExitPlanMode` | auto | Resume normal execution |

## Task tracking

| Tool | Approval | Summary |
|---|---|---|
| `TaskCreate` | auto | Add a task / todo |
| `TaskUpdate` | auto | Change status (pending / in_progress / completed / deleted) |
| `TaskGet` | auto | Look up a task by id |
| `TaskList` | auto | Show current tasks |
| `TodoWrite` | auto | Replace the whole todo list in one call (Claude Code–style) |

`TaskCreate`/`Update`/`Get`/`List` are the granular, per-item interface;
`TodoWrite` rewrites the whole list at once and is what the agent
reaches for during long planning turns. See them mid-turn with
`/tasks`.

## Spawning agents

| Tool | Approval | Summary |
|---|---|---|
| `Task` | prompt | Spawn a sub-agent for an isolated sub-problem |

Sub-agents get their own tool registry and can recurse up to depth 3.
Details in [Chapter 15](ch15-subagents.md).

## Knowledge base (KMS)

| Tool | Approval | Summary |
|---|---|---|
| `KmsRead` | auto | Read a single page from an attached knowledge base (prepends a `[note: …]` staleness banner when `verified:` is missing or > 90 days old) |
| `KmsSearch` | auto | Grep across all pages in one knowledge base |
| `KmsWrite` | prompt | Create or replace a page; auto-injects `# {title}\nDescription: {topic}\n---` header; warns when `sources:` frontmatter is missing |
| `KmsAppend` | prompt | Append content to an existing page |
| `KmsDelete` | prompt | Remove a page (last resort; prefer KmsWrite to merge or supersede) |
| `KmsCreate` | auto | Ensure a KMS exists (idempotent). Used by `/dream` to bootstrap the `dreams` audit KMS. |

These are **always registered** regardless of whether a KMS is currently active. Pre-fix the registration was gated on `kms_active` being non-empty, which silently broke `/dream` and other side-channel agents that need to bootstrap an audit KMS from a zero state. The model sees each active KMS's `index.md` in the system prompt and calls these tools to pull in specific pages on demand.

```
[tool: KmsSearch(kms: "notes", pattern: "bearer")]
```

Returns `page:line:text` lines. Full concept + workflow + page-shape convention (`title:` / `topic:` / `sources:` / `verified:`) in [Chapter 9](ch09-knowledge-bases-kms.md).

## MCP tools

Every MCP server's tools are discovered at startup and registered with
names qualified by server: `weather__get_forecast`,
`github__list_issues`, etc. All prompt for approval. Details in
[Chapter 14](ch14-mcp.md).

## Reading the tool stream

A normal turn looks like:

```
❯ check if there's a README and show me its first section

[tool: Glob: README*] ✓
[tool: Read: README.md] ✓ 0.2s
The README's first section is "Install" — it walks through…
[tokens: 2100in/145out · 1.8s]
```

- `[tool: Name: detail]` — tool being called with an abbreviated
  argument preview (first path, command, URL, search query, etc.).
  Secret-looking values such as tokens, API keys, passwords, and bearer
  auth headers are redacted before display.
- Trailing `✓ <duration>` — tool succeeded and shows how long it ran.
- Trailing `✗ <error>` — tool failed; the model gets the error back
  and may retry with a different approach.
- Long-running tools emit a low-noise heartbeat after about 10 seconds,
  then roughly every 30 seconds while still active:

```
[tool: Bash (cargo test -p thclaws-core)] still running 40s
```

## Tool output truncation

Shell commands and file reads that produce more than 50 KB of output
have the body truncated in the model's view. A small preview is kept
for the model; the full content is saved to
`/tmp/thclaws-tool-output/<tool-id>.txt` so you can inspect it. The
model is told about the truncation and the preview is usually enough
to proceed.

## Limiting which tools run

Three mechanisms:

1. **`allowedTools` / `disallowedTools`** in settings — removes tools
   from the registry so the model never sees them. Useful for
   "read-only review" workflows.
2. **Agent defs** ([Chapter 15](ch15-subagents.md)) — per-agent tool scopes override the
   global registry.
3. **Permissions** ([Chapter 5](ch05-permissions.md)) — tools stay in the registry but prompt
   you before running; `n` denies the call.

## Hooks on tool events

Shell commands can fire on `pre_tool_use` / `post_tool_use` /
`post_tool_use_failure` / `permission_denied` — see [Chapter 13](ch13-hooks.md).
