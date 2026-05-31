# Built-in tools

Every model invocation can call one of the tools registered in the `ToolRegistry`. This manual covers the **non-document** built-in tools: filesystem (Read, Write, Edit, Ls, Glob, Grep), shell (Bash), web (WebFetch, WebSearch), planning (TodoWrite, EnterPlanMode/SubmitPlan/UpdatePlanStep/ExitPlanMode), user interaction (AskUserQuestion), knowledge (KmsRead, KmsSearch), and the in-memory task tracker (TaskCreate/Update/Get/List).

Document tools (DocxCreate/Edit/Read, XlsxCreate/Edit/Read, PptxCreate/Edit/Read, PdfCreate/Read) are covered separately in [`document-tools.md`](document-tools.md) — they share patterns specific to office-format generation that warrant their own treatment.

**Source:** `crates/core/src/tools/`
**Cross-references:**
- [`agentic-loop.md`](agentic-loop.md) — `Tool::call_multimodal` is invoked from the agent's per-turn dispatch
- [`permissions.md`](permissions.md) — `requires_approval()` gate, `Sandbox::check`/`check_write` enforcement
- [`mcp.md`](mcp.md) — MCP-contributed tools register into the same `ToolRegistry`

---

## 1. The `Tool` trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;
    async fn call(&self, input: Value) -> Result<String>;

    /// Multimodal variant. Default wraps `call()`'s string as Text.
    /// Override for tools that produce non-text (Read on image, etc.)
    async fn call_multimodal(&self, input: Value) -> Result<ToolResultContent> {
        self.call(input).await.map(ToolResultContent::Text)
    }

    /// Whether this tool requires user approval when permission_mode == Ask.
    fn requires_approval(&self, _input: &Value) -> bool { false }

    /// MCP-Apps widget to embed inline. Only McpTool overrides today.
    async fn fetch_ui_resource(&self) -> Option<UiResource> { None }

    /// Env vars this tool needs at runtime. When any listed var is
    /// unset/empty, the tool is hidden from `tool_defs()` and `call()`
    /// rejects invocation. Default `&[]` = always available.
    fn requires_env(&self) -> &'static [&'static str] { &[] }
}
```

Six methods:
- `name` — the dispatch key (matches model's `tool_use.name`). Must be unique within the registry. CamelCase convention.
- `description` — sent to the model verbatim as part of the tool catalog. Should be concise + actionable.
- `input_schema` — JSON Schema describing the input object. Sent to the model so it can construct valid `tool_use.input`.
- `call(input) -> Result<String>` — the work.
- `call_multimodal(input) -> Result<ToolResultContent>` — for tools that return images/blocks; default delegates to `call`.
- `requires_approval` — gates the user prompt in Ask mode (see [`permissions.md`](permissions.md) §4 for the full matrix).
- `fetch_ui_resource` — only `McpTool` overrides; produces an iframe widget for chat surface ([`mcp.md`](mcp.md)).
- `requires_env` — names the env var(s) the tool needs. The registry filters out tools whose env isn't satisfied (see §2).

---

## 2. `ToolRegistry`

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self;
    pub fn with_builtins() -> Self;     // 26 builtins registered
    pub fn register(&mut self, tool: Arc<dyn Tool>);
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>>;
    pub fn remove(&mut self, name: &str);
    pub fn names(&self) -> Vec<&str>;
    pub fn tool_defs(&self) -> Vec<ToolDef>;        // sorted by name
    pub async fn call(&self, name: &str, input: Value) -> Result<String>;
}
```

`with_builtins()` registers the 26 "built-in" tools (file + search + shell + web + ask + planning + 12 document tools). Task tools (TaskCreate/Update/Get/List) require shared state and are registered separately via `register_task_tools(&mut registry) -> SharedTaskStore`. Team tools register via `register_team_tools` (see team docs). `Skill` / `SkillList` / `SkillSearch` register per-surface (need `SkillStore` access). `WorkflowRun` registers per-surface (needs the live `Provider` + `model` + Subagent `Tool` reference at construction — see §9b). MCP tools register at MCP-server-spawn time.

`tool_defs()` is what gets sent to the provider — sorted by name for deterministic output (helps with prompt caching: the byte-stable ordering means the tools array doesn't change across turns until a tool registers/removes).

**`requires_env` filter (M6.38.7).** Both `tool_defs()` and `call()` consult `Tool::requires_env()` against the live process env. A tool whose env-var list contains any unset or empty entry is hidden from the provider-facing tool list and rejected at invocation time:

```rust
fn tool_is_available(t: &dyn Tool) -> bool {
    t.requires_env()
        .iter()
        .all(|v| std::env::var(v).map(|val| !val.is_empty()).unwrap_or(false))
}
```

Re-evaluated every call; no registry-level cache. Live key changes (`api_key_set` / `api_key_clear` followed by the existing `rebuild_agent` after `ReloadConfig`) flip tools in/out on the next turn — no restart, no re-registration. The `call()` path also gates: even a stale provider response or hand-crafted tool_use can't reach a tool whose env isn't satisfied. The first concrete users are the two HAL tools (§5).

---

## 3. Filesystem tools

### Ls

| | |
|---|---|
| Name | `Ls` |
| Approval | no |
| Schema | `{path?: string, depth?: integer}` |
| Path validation | `Sandbox::check` |

Lists files and directories under a path. Default path is the project root. `depth` controls recursion (default 1 = direct children only). Returns one entry per line, dirs end with `/`.

### Read

| | |
|---|---|
| Name | `Read` |
| Approval | no |
| Schema | `{path: string, offset?: integer, limit?: integer}` |
| Path validation | `Sandbox::check` |
| Override | `call_multimodal` for image files |

Read a file's contents. Optional `offset` (1-indexed line) + `limit` (max lines) for slicing. Image files (`.png`/`.jpg`/`.jpeg`/`.webp`/`.gif`) use the `call_multimodal` override:
1. Cap raw bytes at `MAX_IMAGE_BYTES = 5 MB` (Anthropic's per-image limit; over this returns an error asking the user to resize).
2. Sniff actual MIME from magic bytes (`0x89 0x50 0x4E 0x47` for PNG, etc.) — extension is just for routing; the wire MIME is from the bytes (file named `.png` containing JPEG would otherwise 400 the provider).
3. Return as `ToolResultContent::Blocks([Image, Text])` so vision models see the pixels and non-vision models still get the text summary.

The plain `call()` errors on image extensions ("use call_multimodal or invoke via the agent loop") to avoid surfacing UTF-8 errors from `read_to_string`.

### Write

| | |
|---|---|
| Name | `Write` |
| Approval | yes |
| Schema | `{path: string, content: string}` |
| Path validation | `Sandbox::check_write` |

Create or overwrite a file. Parent directories created if missing. `Sandbox::check_write` rejects paths inside `.thclaws/` (team state, settings, sessions — must not be touched by file tools).

Lead-only block: when running as the team lead, refuses to write source files unless actively resolving a merge conflict (lead is a coordinator; delegates source changes to teammates via SendMessage). Exception narrowed by `team::lead_resolving_merge_conflict(path)` which checks for `<<<<<<<` markers in the existing file.

### Edit

| | |
|---|---|
| Name | `Edit` |
| Approval | yes |
| Schema | `{path: string, old_string: string, new_string: string, replace_all?: bool}` |
| Path validation | `Sandbox::check_write` |

Find-and-replace exactly one occurrence. Errors when:
- `old_string == new_string` ("identical")
- `old_string not found` (zero matches)
- multiple matches AND `replace_all != true` ("appears N times; use replace_all or add more context")

Same lead-only block as Write. Returns `Replaced N occurrence(s) in <path>`.

### Glob

| | |
|---|---|
| Name | `Glob` |
| Approval | no |
| Schema | `{pattern: string, path?: string}` |
| Path validation | `Sandbox::check` for `path` |

Match files under `path` (default cwd) against a glob pattern (e.g. `src/**/*.rs`). Uses `globset` for matching + `ignore::WalkBuilder` for traversal — RESPECTS `.gitignore` inside git repositories. Returns absolute paths, one per line, sorted.

### Grep

| | |
|---|---|
| Name | `Grep` |
| Approval | no |
| Schema | `{pattern: string, path?: string, glob?: string}` |
| Path validation | `Sandbox::check` for `path` |

Search file contents for a regex pattern. Optional `glob` filter restricts to matching filenames (matched against file name alone, NOT full path — avoids dir-name false matches). Uses Rust's `regex` crate. Returns `path:line:text` per match, sorted. Respects `.gitignore`.

---

## 4. Shell tool

### Bash

| | |
|---|---|
| Name | `Bash` |
| Approval | always (`requires_approval` returns true unconditionally) |
| Schema | `{command: string, cwd?: string, timeout?: integer, timeout_secs?: integer (legacy), description?: string}` |
| Path validation | `Sandbox::check` for `cwd` |
| Default timeout | 120000 ms (max 600000 ms) |

Run a shell command via `/bin/sh -c`. Captures stdout + stderr, interleaves in the returned string. On timeout, kills the child and reports the timeout (partial output discarded).

**Hard-coded denylists** (run AFTER approval, BEFORE exec):
- `lead_forbidden_command` — when running as team lead, blocks `git reset --hard`, `git clean -f/-d`, `git push --force`, `git rebase`, `git worktree remove/prune`, `git checkout -- / .`, `git restore --worktree / .`, `git merge --abort`, `rm -rf / -fr / -r`. Reason: lead is a coordinator; destructive ops belong to teammates inside their own worktrees.
- `teammate_forbidden_command` — when running as a teammate, blocks `git reset --hard <other-branch-or-remote>`. `HEAD`, `HEAD~N`, `HEAD^`, `HEAD@{N}`, hex SHAs ≥7 chars, `tags/...` are allowed (legitimate same-branch recovery).
- `is_destructive_command` — yellow `⚠` print but doesn't block (already approved). 80+ patterns for defense-in-depth: `rm -rf`, `sudo`, `kill -9`, `mkfs`, `dd if=`, `drop database`, `kubectl delete`, `terraform destroy`, `aws s3 rm`, `curl ... | sh`, etc.

**Auto-helpers:**
- `maybe_wrap_with_venv` — for `pip`/`python` commands when no `.venv` exists in cwd, prepends `python -m venv .venv && source .venv/bin/activate &&` so deps install into the project venv.
- `split_chained_server_command` — for `pip install X && uvicorn app` style chains, runs setup synchronously then runs the server with a 5s capture timeout (the server keeps running; we return after sampling startup output).
- `is_server_command` — token-aware detection for `npx vite`, `pnpm dev`, `python -m http.server`, etc. Server commands that don't end in `&` get the 5s capture treatment.
- `apply_noninteractive_env` — sets `CI=1`, `npm_config_yes=true`, etc. so package-manager prompts don't hang waiting for stdin.

See [`permissions.md`](permissions.md) §11 for the full forbidden-command lists.

---

## 5. Web tools

### WebFetch

| | |
|---|---|
| Name | `WebFetch` |
| Approval | yes |
| Schema | `{url: string, max_bytes?: integer, prefer_raw?: boolean}` |
| Default max_bytes | 102400 (100 KB), applied per section |

**Combined-fetch behavior when `HAL_API_KEY` is set.** Pre-fix `WebFetch` was a plain HTTP GET; an intermediate "HAL with fallback to GET on failure" iteration kept only HAL output on the happy path, which hid the raw payload for URLs where the model actually wanted it (JSON APIs, sitemaps, robots.txt — anything HAL might render through a browser tab that mangles the structure).

Current behavior: when `crate::tools::hal::hal_available()` returns `true` and `prefer_raw != true`, the tool fires both paths concurrently via `tokio::join!(hal_client_scrape, plain_http_get)` and returns a single combined response with each section explicitly labelled:

```
[via HAL scrape — JS-rendered + extracted to Markdown]

# {page title}

(rendered Markdown content from HAL's /scrape/v1/url endpoint)

---

[via plain HTTP GET — raw response body]

(raw HTTP body — preserves JSON, headers-style content, anything browser-rendering would corrupt)
```

`max_bytes` (default 100 KB) caps each section independently (`truncate_for_bytes` walks back to a UTF-8 char boundary, never splits mid-character). Result-merging logic:

| HAL | Plain GET | Output |
|---|---|---|
| Ok | Ok | both sections labelled, separated by `---` |
| Ok | Err | HAL section + `[note: plain HTTP GET also attempted but failed: …]` |
| Err | Ok | `[note: HAL scrape failed: …; returning plain GET only]` prefix + plain section |
| Err | Err | `Error::Tool("fetch {url} failed on both paths — HAL: …; plain GET: …")` |

`prefer_raw: true` skips HAL entirely (faster, half the tokens) — model uses this when it knows the URL is a JSON endpoint or similar where HAL's browser-rendering would be harmful. When `HAL_API_KEY` is absent the tool is a plain GET regardless of `prefer_raw`.

The HAL section reuses `hal::build_client()` (90 s timeout, same as the dedicated `WebScrape` tool) so the two clients stay in lockstep on TLS / timeout configuration. The plain section uses `WebFetchTool::client` with a 30 s timeout — page servers are on the hot path of plain GET so the shorter limit is right there.

### WebSearch

| | |
|---|---|
| Name | `WebSearch` |
| Approval | yes |
| Schema | `{query: string, max_results?: integer}` |
| Default max_results | 5 |

Multi-backend web search with auto-detection. Backend priority:

1. **Tavily** — `TAVILY_API_KEY`; clean JSON, includes a synthesized `answer` field
2. **Brave Search** — `BRAVE_SEARCH_API_KEY`; clean JSON
3. **DuckDuckGo HTML scrape** — no key required; fallback

Constructed via `WebSearchTool::new("auto" | "tavily" | "brave" | "duckduckgo")`. With `"auto"` (default), tries each in priority order. Explicit engine name forces that backend; `"duckduckgo"` skips the keyed backends entirely.

If the configured backend's key is missing, falls through to the next available backend — always returns SOMETHING rather than panicking.

### YouTubeTranscript & WebScrape (HAL Public API)

| | |
|---|---|
| Names | `YouTubeTranscript`, `WebScrape` |
| Approval | yes |
| Source file | [`tools/hal.rs`](../thclaws/crates/core/src/tools/hal.rs) |
| `requires_env` | `&["HAL_API_KEY"]` |

Both wrap [HAL's public API](https://hal.thaigpt.com/api) with one shared `X-API-Key` header. They're the first tools to declare `requires_env`, so they're hidden from `tool_defs()` until the user pastes a key in **Settings → Providers → Service keys → HAL Public API** (or sets `HAL_API_KEY` in the shell).

- `YouTubeTranscript { url? | video_id?, languages?, with_timestamps? }` → `POST /youtube/v1/transcript`. Either `url` (any common YouTube shape) or `video_id` (11-char) is required. Default languages: `["en", "th"]`. Returns the JSON shape from HAL — `{video_id, title, channel, language, transcript|segments}`.
- `WebScrape { url, wait_for?, scroll_to_bottom?, remove_selectors?, output_format? }` → `POST /scrape/v1/url`. Renders in headless browser, returns `{title, content, metadata, scraped_at}`. Use this instead of `WebFetch` when the page is JS-heavy / needs scrolling / has noise to strip.

90s hard timeout per request (`HAL_TIMEOUT`); the scrape endpoint can be slow on heavy pages with `scroll_to_bottom`. The shared `hal_post` helper surfaces HAL's `detail` field on non-2xx so error messages are actionable (e.g. `HAL 404: No transcript available for this video`).

---

## 6. User interaction

### AskUserQuestion

| | |
|---|---|
| Name | `AskUserQuestion` |
| Approval | no |
| Schema | `{question: string}` |

Surface a question to the user and wait for their typed response. Two channels:

- **GUI**: when `set_gui_ask_sender(Some(tx))` has been called (worker startup wires it), the tool sends an `AskUserRequest { id, question, response: oneshot::Sender<String> }` over the channel. The frontend renders a modal; user types an answer; GUI handler resolves the oneshot. Tool returns the answer (normalized).
- **CLI fallback**: when no GUI sender is configured, prints `[agent asks]: <question>` to stdout and reads a line from stdin via `tokio::task::spawn_blocking`.

Empty response → `(no response from user)` placeholder so the model knows the user dismissed the prompt.

`NEXT_ASK_ID: AtomicU64` for unique request ids. `GUI_ASK_SENDER: OnceLock<Mutex<Option<...>>>` for the singleton channel.

---

## 7. Planning tools

Four tools form the structured-plan dispatch surface; live in `tools/plan.rs` (request side) + `tools/plan_state.rs` (state machine). Used together with the `PermissionMode::Plan` mode (see [`permissions.md`](permissions.md) §2).

### EnterPlanMode

| | |
|---|---|
| Name | `EnterPlanMode` |
| Approval | no (so it can sail through the dispatch gate) |
| Schema | `{}` |

Stashes the current permission mode (via `permissions::stash_pre_plan_mode(prior)`) then sets `permissions::set_current_mode_and_broadcast(PermissionMode::Plan)`. The agent loop's dispatch gate then blocks all mutating tools (anything with `requires_approval=true`) with a structured "use Read/Grep/Glob/Ls; SubmitPlan when ready" tool_result. Idempotent — re-entering plan mode while already in it doesn't double-stash.

### SubmitPlan

| | |
|---|---|
| Name | `SubmitPlan` |
| Approval | no |
| Schema | `{steps: [{id: string, title: string, description?: string}]}` |

Publish a structured ordered plan to the right-side sidebar. Replaces any prior plan wholesale. Each step starts as `Todo`. Validation:
- Empty `steps` array → error
- Empty step `id` or `title` → error
- Duplicate step ids → error

Returns the plan id + first step's id with a "wait for approval, then UpdatePlanStep('<step1>', 'in_progress')" hint. The user reviews via the sidebar Approve / Cancel buttons (which fire `plan_approve` / `plan_cancel` IPCs).

### UpdatePlanStep

| | |
|---|---|
| Name | `UpdatePlanStep` |
| Approval | no |
| Schema | `{step_id: string, status: "todo"\|"in_progress"\|"done"\|"failed", note?: string, output?: string}` |

Apply a step transition with Layer-1 gating. Legal transitions:
- `todo → in_progress` (only when previous step is `done`)
- `todo → failed` ("blocked by upstream failure" — note REQUIRED)
- `in_progress → done`
- `in_progress → failed` (note recommended)
- `failed → in_progress` (retry)

`done` transitions can carry an optional `output` (capped at 1KB) — the cross-step data channel for IDs / hashes / paths / port numbers later steps need to consume.

Plan-completion auto-restore: when the final step transitions to `done`, `take_pre_plan_mode()` pops the stash and restores the prior permission mode automatically.

### ExitPlanMode

| | |
|---|---|
| Name | `ExitPlanMode` |
| Approval | no |
| Schema | `{}` |

Restores the pre-plan permission mode (defaults to `Ask` if no stash). Triggered by sidebar Cancel button or model-initiated exit.

**Approval-window gate** (separate from the plan tools themselves): while a plan is submitted-but-not-approved, `UpdatePlanStep` and `ExitPlanMode` are blocked at dispatch with a "wait for sidebar Approve/Cancel" message. The sole legal path forward is the user clicking a sidebar button.

---

## 8. Knowledge management

Six tools, all **always-registered** regardless of `config.kms_active` contents. Pre-fix the registration was gated on `!kms_active.is_empty()`, which silently broke side-channel agents (notably `/dream`) that need to bootstrap an audit KMS from a zero state — they'd inherit an empty filtered registry, exit ~30 s into the run with no work done, and the UI would show ✓ as if everything succeeded. The gate is removed; per-tool errors ("no KMS named 'X'") still provide a clear signal when a model targets a missing KMS. See [`kms.md`](kms.md) for the full subsystem (architecture, frontmatter, ingest, lint, slash commands, security model, Obsidian compatibility).

### KmsRead

| | |
|---|---|
| Name | `KmsRead` |
| Approval | no |
| Schema | `{kms: string, page: string}` |

Read a single page from an attached knowledge base. `kms` is the KMS name (project-scope wins on collision with user-scope, per `kms::resolve`). `page` is the page name with or without `.md` extension. Returns the file contents.

Prepends a `[note: …]` staleness banner when the page's frontmatter signals trouble:

- `verified:` older than 90 days → `[note: this page was last verified N days ago — sources may have drifted; re-verify before citing as current fact]`
- Frontmatter present but no `verified:` key → `[note: this page has no \`verified:\` frontmatter — provenance is best-effort, treat factual claims with caution]`
- No frontmatter at all (legacy / hand-written page) → no banner (don't shout at user-curated content)

The 90-day threshold lives in `staleness_warning::STALE_DAYS_THRESHOLD` (constant; not user-configurable yet). Day count uses a cheap `YYYY-MM-DD` parser — months treated as 30 d, years as 365 d. Off by a couple of days at boundaries, fine for a 90-day-granularity banner.

### KmsSearch

| | |
|---|---|
| Name | `KmsSearch` |
| Approval | no |
| Schema | `{kms: string, pattern: string}` |

Grep across all `.md` pages in one knowledge base. Returns `page:line:text` per match, sorted. Defensive against symlink-based exfiltration:
- Refuses to walk if `pages/` itself is a symlink (would otherwise let `pages -> /etc` exfil arbitrary files)
- Skips entries that are symlinks (prevents `ln -s ~/.ssh/id_rsa pages/leak.md`)

### KmsWrite

| | |
|---|---|
| Name | `KmsWrite` |
| Approval | **yes** |
| Schema | `{kms: string, page: string, content: string}` |

Create or replace a page in an attached knowledge base. `content` should begin with YAML frontmatter — `title:` + `topic:` + `sources:` are the three keys the tool description asks for. `created:` (new pages) and `updated:` (always today) are auto-stamped; `verified:` is preserved as-passed (only the research pipeline stamps it today). `kms::write_page` invokes `maybe_inject_canonical_header(body, stem, fm)` after parsing, prepending `# {title}\nDescription: {topic}\n---\n\n` between frontmatter and body when the body doesn't already lead with a `# heading`. Updates `index.md` bullet (using the user-supplied body, not the canonical-header version, so the index summary reflects the model's first real paragraph), appends `## [date] wrote | <stem>` to `log.md`. Path validated by `kms::writable_page_path` (no `..` / separators / control chars / reserved stems; canonicalized inside `pages/`; refuses symlinked `pages/`). Bypasses `Sandbox::check_write` to land inside the KMS root — same intentional carve-out pattern as `TodoWrite` (see [`kms.md`](kms.md) §7 for the security rationale).

`KmsWriteTool::call` runs a `check_provenance(content)` pre-flight check after frontmatter is parsed. If the page has frontmatter but no `sources:` key (or `sources:` with a blank value), the write still goes through (soft enforcement — keeps the tool usable for legacy / quick captures), but the tool response carries `warning: no \`sources:\` frontmatter — add a URL list (or [] for opinion/convention pages, or session-<id> / memory for in-conversation provenance) so the page is auditable later`. The companion `KmsRead` staleness banner is the second layer of the same enforcement. Explicit `sources: []` is the deliberate "opinion / convention, no external source" form and does NOT trigger the warning.

### KmsAppend

| | |
|---|---|
| Name | `KmsAppend` |
| Approval | **yes** |
| Schema | `{kms: string, page: string, content: string}` |

Append `content` to a page. If page exists with frontmatter: bumps `updated:` and re-serializes. If exists without: plain append. If doesn't exist: creates with bare body (no frontmatter). Always appends `## [date] appended | <stem>` to `log.md`. Same path-validation + sandbox-carve-out as `KmsWrite`.

### KmsCreate

| | |
|---|---|
| Name | `KmsCreate` |
| Approval | no (idempotent + name-validated; same risk profile as `SessionRename`) |
| Schema | `{name: string, scope: "project" \| "user"}` |

Ensure a KMS exists. Wraps `kms::create(name, scope)` directly: returns the existing `KmsRef` when the directory is already present, otherwise seeds the tree (`pages/`, `sources/`, `index.md`, `log.md`, `SCHEMA.md`, `manifest.json`). Name validation rejects path separators, `..`, leading `.`, control chars, absolute paths, and empty strings.

Primary motivation: `/dream`'s Pass 5 calls `KmsCreate({name: "dreams", scope: "project"})` to bootstrap the dedicated audit-log KMS before writing the run summary. The dispatch path also auto-creates `dreams` before spawning the dream side channel — both layers are defense-in-depth so a stale binary or filesystem race can't trap the dream agent in a retry loop on "no KMS named 'dreams'".

All KMS tools rely on `kms::resolve(name)` (project KMS list first, then user). They're now always-registered regardless of `kms_active` contents.

### MemoryRead / MemoryWrite / MemoryAppend (M6.26)

Three tools register **always** (not conditional on entry presence — the agent needs them to create the first entry). See [`memory.md`](memory.md) for the full subsystem (resolution, frontmatter, system-prompt injection, slash commands, sandbox carve-out).

| Tool | Approval | Schema | Purpose |
|---|---|---|---|
| `MemoryRead` | no | `{name: string}` | Fetch full body of a deferred entry (when system prompt marks it `body deferred`) |
| `MemoryWrite` | **yes** | `{name: string, content: string}` | Create or replace an entry. Frontmatter preserved; `created:` stamped on new, `updated:` always today. Auto-updates `MEMORY.md` |
| `MemoryAppend` | **yes** | `{name: string, content: string}` | Append a chunk; bumps `updated:`. Creates with bare body if missing |

`MemoryWrite` and `MemoryAppend` bypass `Sandbox::check_write` to land inside the resolved memory root — same intentional carve-out pattern as `TodoWrite` (`.thclaws/todos.md`) and `KmsWrite` (`.thclaws/kms/...`). Path safety enforced via `memory::writable_entry_path` (no `..` / separators / control chars / reserved `MEMORY` stem; canonicalized inside the memory root).

---

## 9. In-memory tasks

### TaskCreate / TaskUpdate / TaskGet / TaskList

Four tools sharing one `Arc<Mutex<TaskStore>>` registered via `register_task_tools(&mut registry) -> SharedTaskStore`. Tasks are in-memory only — they don't persist across restarts (use TodoWrite for persistent across-session todos).

```rust
pub struct Task {
    pub id: String,        // monotonic numeric, assigned by store
    pub subject: String,
    pub description: String,
    pub status: String,    // "pending" by default
}
```

| Tool | Approval | Schema | Behavior |
|---|---|---|---|
| `TaskCreate` | no | `{subject: string, description: string}` | Creates with auto-incremented id, status="pending". Returns formatted task. |
| `TaskUpdate` | no | `{id: string, status?: string, subject?: string, description?: string}` | Updates the named fields on the existing task. Returns updated task or "not found". |
| `TaskGet` | no | `{id: string}` | Returns formatted task or "not found". |
| `TaskList` | no | `{}` | Returns all tasks formatted, one per pair of lines. |

Format: `#{id} [{status}] {subject}\n  {description}`.

The `register_task_tools` returns the `SharedTaskStore` so the REPL can read the task list for `/tasks` slash command output.

### TodoWrite (separate from Tasks)

| | |
|---|---|
| Name | `TodoWrite` |
| Approval | yes |
| Schema | `{todos: [{id: string, content: string, status: "pending"\|"in_progress"\|"completed"}]}` |
| Persists | `<cwd>/.thclaws/todos.md` (markdown) |

Casual self-tracking scratchpad. Writes the entire todo list as a markdown checklist (`- [x]`, `- [-]`, `- [ ]` for completed/in_progress/pending). REPLACES the entire list (full state replacement, not append).

Distinct from the structured plan tools above:
- TodoWrite: invisible to the user (only visible if they open `.thclaws/todos.md`), no driver, no sequential gating, no audit
- SubmitPlan + UpdatePlanStep: sidebar-rendered with checkmarks, sequential gating, per-step verification, audit

The model is instructed (via the tool's description) to read existing `todos.md` at session start and resume / replace based on user intent — don't silently start fresh on top of stale work.

In Plan mode the dispatch gate blocks TodoWrite with a "use SubmitPlan instead" message (per [`permissions.md`](permissions.md) §5 layer 4).

**Validation chain (M6.30 audit fixes — `dev-log/146`):** every input is validated before any disk write:
- **Symlink defense** — refuses if `<cwd>/.thclaws/` is a symlink (`std::fs::write` follows symlinks; pre-fix an attacker-planted symlink could escape the project root — verified empirically).
- **Field sanitization** — `id` (max 64 chars) and `content` (max 500 chars) reject empty values and control chars (`\n`, `\r`, `\t`, `\0`, etc.). Newlines in particular would corrupt the markdown bullet structure and poison the `build_todos_reminder` parser.
- **Server-side `status` validation** — JSON Schema `enum` is sent to providers but compliance varies; pre-fix unknown values like `"InProgress"` (capitalization) or `"in-progress"` (hyphen) silently rendered as `[ ]` AND counted as zero of all categories. Post-fix returns a clear error so the model can correct on retry.
- **Unique-id check** — duplicate ids rejected with `'<id>' — every todo must have a unique id` (pre-fix: file kept both bullets, frontend logged React key collisions, next-read state was ambiguous).

Same intentional sandbox carve-out as KMS / Memory writes — `.thclaws/` is reserved-write but TodoWrite specifically targets it via the validated path.

---

## 9b. Orchestration: WorkflowRun

| | |
|---|---|
| Name | `WorkflowRun` |
| Approval | **yes** — every call prompts (same posture as `Bash`; runs LLM-authored JavaScript) |
| Schema | `{prompt: string}` — natural-language goal for the workflow author |
| Source | `crates/core/src/tools/workflow_run.rs` |
| Returns | Script's final-expression value as a string, plus a one-line token rollup `[workflow: N subagent turn(s), X in / Y out tokens]` |

Model-callable wrapper around the same `crate::workflow::script::author` + `crate::workflow::WorkflowSandbox::run` flow the user-typed `/workflow run` slash command takes. The model decides when fan-out is the right primitive instead of the user typing the slash command. Both paths share one engine — no duplicate authoring logic.

Internally:

1. **Nested-call guard** — `crate::workflow::is_inside_workflow()` checks the `WORKFLOW_USAGE_SINK` thread-local. If set (we're inside a running sandbox), bail with "WorkflowRun cannot be invoked from inside a running workflow…" before authoring so we don't burn tokens on an unrunnable script.
2. **Author phase** — `workflow::script::author(provider, model, prompt, None)` makes ONE provider stream call with `WORKFLOW_AUTHOR` as the system prompt and the user goal as the only message. Returns the JS script body (markdown fences stripped).
3. **Execute phase** — `tokio::task::spawn_blocking` opens a worker thread; inside:
   - `workflow::set_task_tool(Some(subagent_arc))` — installs the Subagent (`Task`) tool the sandbox's `thclaws.subagent(...)` host binding dispatches through.
   - `workflow::set_usage_sink(true)` — enables per-turn usage capture.
   - `WorkflowSandbox::new()` + `sandbox.run(&script)` — Boa runs the script.
   - `workflow::take_all_usages()` drains; `set_task_tool(None)` + `set_usage_sink(false)` unwind the thread-locals.
4. **Result** — script's final-expression string + token rollup.

**Captured state at registration:** `Arc<dyn Provider>`, `model: String`, `Option<Arc<dyn Tool>>` (the live Subagent tool). The provider+model snapshot means a `/model` swap mid-session leaves WorkflowRun pinned to the swap-time provider until the surface re-registers (REPL: on `/reload`, GUI: on rebuild_agent path). The Subagent reference may be `None` on surfaces that don't register Subagent (print mode, `agent_runtime` HTTP) — non-subagent scripts still work; scripts calling `thclaws.subagent(...)` fail with the runtime's own "Task tool not available" error.

**Surface availability matrix:**

| Surface | Registered? | Subagent threaded? |
|---|---|---|
| CLI REPL (`thclaws --cli`) | yes | yes |
| GUI / `--serve` (worker) | yes | yes |
| Print mode (`thclaws -p`) | yes | no |
| `agent_runtime` HTTP (`/v1`) | yes | no |

**Tests** in `crates/core/src/tools/workflow_run.rs::tests`:
- `workflow_run_executes_authored_script_and_returns_result` — stub provider returns `"'hi'"`, tool runs end-to-end through `spawn_blocking` + Boa, returns `"hi"` + token rollup. Pins the pipeline composes from the tool layer, not just from the slash-command handler.
- `nested_workflow_run_is_rejected_via_thread_local` — sets `WORKFLOW_USAGE_SINK` by hand, calls tool, expects "inside a running workflow" error.

**Cancellation** — the workflow runtime's polling boundary observes the standard cancellation token set by the calling worker (`shell_dispatch.rs:3733` for GUI / `repl.rs:9080` for CLI slash-command path). The tool inherits whichever surface invoked it; no extra plumbing here.

**Why a tool and not just the slash command** — pre-fix users wanted the model to reach for the workflow primitive on its own when a task looked like deterministic fan-out, without needing to remember `/workflow run`. The slash command stays as the interactive-review path for novel patterns; the tool path skips review for speed. Both go through the same author + sandbox flow so changes to the engine don't drift.

---

## 10. Code organization

```
crates/core/src/tools/
├── mod.rs                                              ── Tool trait + ToolRegistry + with_builtins
├── ask.rs (129 LOC)                                    ── AskUserQuestion + GUI/CLI bridge
├── bash.rs (1561 LOC)                                  ── Bash + lead/teammate forbidden lists +
│                                                          destructive detection + venv auto-wrap +
│                                                          server detection + non-interactive env
├── edit.rs (168 LOC)                                   ── Edit
├── glob.rs (167 LOC)                                   ── Glob (globset + ignore::WalkBuilder)
├── grep.rs (195 LOC)                                   ── Grep (regex crate + ignore + glob filter)
├── kms.rs (238 LOC)                                    ── KmsRead + KmsSearch
├── ls.rs (103 LOC)                                     ── Ls
├── plan.rs (299 LOC)                                   ── EnterPlanMode / ExitPlanMode /
│                                                          SubmitPlan / UpdatePlanStep
├── plan_state.rs (900 LOC)                             ── Plan state machine, transition gating,
│                                                          completion auto-restore (covered in
│                                                          permissions.md §7-8)
├── read.rs (411 LOC)                                   ── Read (text + image multimodal)
├── search.rs (238 LOC)                                 ── WebSearch (Tavily/Brave/DDG)
├── tasks.rs (299 LOC)                                  ── TaskCreate/Update/Get/List + SharedTaskStore
├── todo.rs (382 LOC)                                   ── TodoWrite (markdown checklist)
├── web.rs (91 LOC)                                     ── WebFetch
├── write.rs (123 LOC)                                  ── Write
└── (document tools — see document-tools.md)
```

---

## 11. Testing

Each tool ships with unit tests in its own `mod tests`. Total coverage:

| Tool | Tests | Notable |
|---|---|---|
| AskUserQuestion | 1 | `gui_ask_sender_round_trips_answer` |
| Bash | ~25 | destructive matching, lead/teammate forbidden, server detection, venv wrap, timeout |
| Edit | 5 | single/multi/replace_all/missing/identical |
| Glob | 6 | recursive, specific pattern, empty, sorted, gitignore |
| Grep | 6 | regex, glob filter, gitignore, bad regex |
| Kms | 6 | read/search round-trip, missing extension fallback, unknown KMS, symlink defense |
| Ls | 3 | basic listing, depth, missing path |
| Plan / plan_state | ~30 | full state-machine matrix, gating, completion restore |
| Read | ~10 | text, slicing, image multimodal, MIME sniff, oversize cap |
| WebSearch | ~6 | per-backend round-trip, auto fallback |
| Tasks | 4 | create / update / get / list |
| TodoWrite | 5 | parse, write, status counts, doc rendering |
| WebFetch | 2 | basic + truncation |
| Write | 4 | basic, parent mkdir, .thclaws block, lead block |

Tests are deterministic via `tempfile::tempdir` for filesystem state. Tests that touch globals (KMS env, `is_team_lead`) use guards to restore prior state on Drop.

---

## 12. Adding a new built-in tool

1. Create `tools/foo.rs`:
   ```rust
   use super::{req_str, Tool};
   use crate::error::Result;
   use async_trait::async_trait;
   use serde_json::{json, Value};

   pub struct FooTool;

   #[async_trait]
   impl Tool for FooTool {
       fn name(&self) -> &'static str { "Foo" }
       fn description(&self) -> &'static str { "Does foo." }
       fn input_schema(&self) -> Value {
           json!({"type":"object","properties":{"bar":{"type":"string"}},"required":["bar"]})
       }
       fn requires_approval(&self, _input: &Value) -> bool { /* true for mutating */ false }
       async fn call(&self, input: Value) -> Result<String> {
           let bar = req_str(&input, "bar")?;
           Ok(format!("did foo with {bar}"))
       }
   }
   ```
2. Add to `tools/mod.rs`: `pub mod foo;` + `pub use foo::FooTool;` + register in `with_builtins()`.
3. Add a test module in `tools/foo.rs` with at least:
   - happy path
   - missing-required-field error
   - any tool-specific edge cases
4. Update the test in `tools/mod.rs::tool_defs_are_sorted_and_complete` to include `"Foo"` in the expected names list (alphabetical position).
5. If the tool touches the filesystem, decide between `Sandbox::check` (read) and `Sandbox::check_write` (write) — see [`permissions.md`](permissions.md) §7.
6. If the tool requires approval, set `requires_approval(input) -> true`. The agent dispatch gate (and per-mode behavior — Plan blocks all mutating tools) handles the rest.

---

## 13. Notable behaviors / gotchas

- **`call_multimodal` default delegates to `call`** — overriding `call_multimodal` without overriding `call` is fine but unusual; only Read does this today.
- **`requires_approval(input)` takes the input** — so future tools can be selectively approved (e.g. `Bash` could approve only when `command` matches a pattern). Today no tool varies by input.
- **`Ls` / `Read` / `Glob` / `Grep` / `Kms*` / `Ask` / `TaskGet` / `TaskList` — read-only tools ([`permissions.md`](permissions.md) §4 matrix)** sail through the dispatch gate even in `Ask` mode.
- **`Edit` / `Write` / `Bash` / `WebFetch` / `WebSearch` / `TodoWrite` / `TaskCreate` / `TaskUpdate` — mutating tools** require approval in Ask mode and are BLOCKED in Plan mode (replaced by structured tool_result telling the model to use Read/Grep/Glob/Ls).
- **`AskUserQuestion`** is read-only-ish (asks for input, doesn't mutate state) — sails through the gate in Ask mode but is the user-facing way for the model to request clarification.
- **Plan tools have `requires_approval=false`** so they can run in Plan mode (they manage the plan-mode state itself).
- **Tool names are CamelCase.** Don't use snake_case; the model is trained on CamelCase tool names from Anthropic conventions.
- **`description` is BUDGETED.** It contributes to the system-prompt-equivalent "tools" budget in every request. Keep it concise; avoid restating things the schema already says.
- **`input_schema` should always have `"type": "object"`** at the top level. The agent's `tool_defs_are_sorted_and_complete` test enforces this.
- **`call` returning a very long string triggers truncate-to-disk** (see [`agentic-loop.md`](agentic-loop.md) — `TOOL_RESULT_CONTEXT_LIMIT = 50_000` bytes; over this gets spilled to a temp file with a preview kept in context). Tools don't need to self-limit.
- **`Sandbox::check_write` rejects `.thclaws/`** even if the path is otherwise inside the project root. This protects team state from being overwritten by the model.
- **Bash hard-blocks fire AFTER approval.** The user approving a `git reset --hard main` from the lead context still doesn't run — the dispatch gate denies before exec.

---

## 14. What's NOT a built-in tool

- **MCP tools** — registered dynamically when MCP servers connect. See [`mcp.md`](mcp.md).
- **Skill tools** — registered by the skill system (`SkillTool`, `SkillListTool`, `SkillSearchTool`). See [`skills.md`](skills.md).
- **Team tools** — registered by `register_team_tools` when `team_enabled=true`. SendMessage, CheckInbox, TeamStatus, TeamCreate, SpawnTeammate, TeamTaskCreate/List/Claim/Complete, TeamMerge.
- **Subagent (`Task`) tool** — registered by the CLI REPL only (not GUI), with multi-level recursion via `ReplAgentFactory`. See subagent docs.
- **Document tools** — DocxCreate, DocxEdit, DocxRead, XlsxCreate/Edit/Read, PptxCreate/Edit/Read, PdfCreate/Read. See [`document-tools.md`](document-tools.md).
