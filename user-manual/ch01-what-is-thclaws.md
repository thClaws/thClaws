# Chapter 1 — What is thClaws?

![logo](../user-manual-img/logo/thClaws-logo-line-art-banner.png)

thClaws is a **native-Rust AI Agent Platform** that runs locally on
your machine — for building AI Agents that help with a wide range of
work: writing code, automating workflows, reviewing and organizing
documents, managing knowledge bases, or assembling teams of agents
that work together. All in one binary. You tell it what you want in
natural language; it reads
your files, runs commands, uses tools, and talks back to you while it
works.

Nine surfaces ship as one binary, sharing a single `Agent` loop,
`Session`, and tool registry — the first eight are for a single person
(including chatting through LINE, Telegram, or Facebook Messenger on
your phone), the ninth lets other software hire thClaws to do work.
Beyond the binary, **[thClaws.cloud](#thclawscloud--browse-run-and-host-agents)**
adds a catalog you browse and a hosted runtime you rent — see the
dedicated bullet below and [Chapter 27](ch27-thclaws-cloud.md):

- **Desktop GUI** (`thclaws` with no flags) — a native window with a
  Terminal tab running the same interactive REPL as `--cli` mode, a
  streaming Chat tab, a Files browser, and an optional Team tab.
- **CLI REPL** (`thclaws --cli`) — an interactive terminal prompt for
  SSH sessions, headless servers, or when you want zero GUI overhead.
- **Non-interactive mode** (`thclaws -p "prompt"`, long form `--print`)
  — runs a single turn and exits. Handy for scripts, CI pipelines, and
  shell one-liners. Add `-v` / `--verbose` to see per-turn token usage
  on stderr without polluting stdout.
- **Webapp** (`thclaws --serve --port 7878` + open a browser) — same
  engine over WebSocket/HTTP, served from your laptop. Reach it
  remotely via SSH tunnel for "thClaws anywhere" without opening a
  port.
- **LINE Chat** (GUI → Line Connect modal; there is no `--line` flag,
  unlike Telegram and Messenger) — chat
  with your agent through your own LINE Official Account. Goes
  through a relay tunnel at `line.thclaws.ai` that bridges the LINE
  platform and the thClaws running on your machine — the agent stays
  local but you can reach it from anywhere via your phone (see
  [Chapter 21](ch21-line-and-browser-chat.md)).
- **Telegram bot** (`thclaws --telegram` or GUI Telegram Connect
  modal) — create a bot with `@BotFather`, paste its token, and every
  DM to the bot runs as a turn on your desktop. Tool calls that need
  approval arrive as inline-keyboard buttons you tap from your phone
  (see [Chapter 23](ch23-telegram.md)).
- **Facebook Page Messenger** (`thclaws --messenger` or GUI Messenger
  Connect modal) — connect a Facebook Page once, and every Messenger
  DM to the Page runs as a turn on your desktop, with approvals shown
  as quick-reply chips (see [Chapter 24](ch24-messenger.md)).
- **thClaws Remote** (GUI → pair with your thClaws.cloud account) —
  the desktop dials *out* and holds a tunnel open, so you can talk to
  the agent on your own machine from the cloud dashboard with no public
  IP and no inbound port. Unlike the chat bridges it binds to your
  cloud account rather than a messaging platform.
- **AI Agent (API server)** (`thclaws --serve` + HTTP API) — lets
  *other software* (orchestrators, external clients, schedulers) call
  thClaws as an agent over the same HTTP API — details in later
  chapters.

## What makes it different

- **thClaws.cloud — browse, run, and host agents.** An AI agent in
  thClaws is just a folder ([Chapter 8](ch08-memory-and-agents-md.md)),
  and thClaws.cloud turns that folder model into *git for AI agents*.
  **Browse** a curated catalog at
  [thclaws.cloud/templates](https://thclaws.cloud/templates), **install** any
  agent into a local folder with one command (`/cloud get <slug>`),
  **publish** your own (`/cloud publish`) — you own the folder and bring
  your own provider keys. Bind your desktop to the catalog by pasting a
  CLI token once in Settings → thClaws.cloud; from then on every catalog
  op is a slash command inside an open session. For teams there's a
  **hosted runtime** (managed runners, no setup — currently in closed
  beta) and **shared agents**: a company-owned agent several people use,
  gateway-billed to the owner with a read-only company knowledge base.
  See [Chapter 27](ch27-thclaws-cloud.md). <a id="thclawscloud--browse-run-and-host-agents"></a>
- **Self-improving AI Agent (auto-learn).** Turn on `autoLearn: true`
  in settings and thClaws automatically files every substantive
  session as a new page in a dedicated `self_learn` KMS (separate
  from your hand-curated active vaults), then runs throttled
  `/kms reconcile` to dedupe and resolve contradictions across pages.
  Built from existing primitives — `/kms ingest`, `kms-reconcile`,
  the session_end hook — no new agent prompts; just wiring. One flag
  to enable, `rm -rf .thclaws/state/kms/self_learn/` to reset. See
  [Chapter 9 §Self-improving AI Agent](ch09-knowledge-bases-kms.md#self-improving-ai-agent-auto-learn).
- **Four tiers of agent orchestration.**
  - **`Task` tool** — model-driven subagents that block the parent's
    turn. Each gets its own tool registry, recurses up to 3 levels
    deep. Right when the parent's reasoning should decide whether and
    when to delegate.
  - **`/agent <name> <prompt>`** — user-driven concurrent
    side-channels. Spawned on a fresh tokio task, runs in parallel with
    main, never enters main's history, has its own cancel token. Right
    when *you* know exactly what you want a specialist to do
    (`/agent translator แปลไฟล์ x` while you keep coding).
  - **Agent Teams** — multiple thClaws processes coordinating through
    a shared mailbox and task queue, each teammate in its own tmux
    pane and optional git worktree. One agent writes your backend
    while a teammate builds the frontend in parallel; lead calls
    `TeamMerge` when both are done.
  - **Workflows (`/workflow`)** — the orchestrator is *code*, not the
    model: the LLM writes a JavaScript script that fans work out across
    many subagents, and a sandboxed JS engine runs it deterministically
    on your machine. Rerunning gives the same shape of work every time,
    and a long job leaves a resumable checkpoint on disk. Right for
    **bulk, deterministic, mostly-independent** work ("rewrite all 800
    test files to the new fixture"; "translate every page under
    `kms/bug/` to Thai") ([Chapter 25](ch25-workflows.md)).
- **Hire-able as a working agent — your self-hosted sandbox.** The
  inverse direction of orchestration: thClaws itself runs as a
  *worker* for another orchestrator (a scheduler, a CI job, your own
  control plane). It can share the caller's machine as a local process,
  or stand alone as a pod on a VPS, cloud, or your own k3s — the agent
  loop is upstream while tool execution stays inside *your* perimeter.
  The orchestrator drives it through the same HTTP API users and IDEs
  use: `POST /agent/run` and `GET /v1/agent/info`.
- **Three tiers of long-term memory.**
  - **`AGENTS.md` / `CLAUDE.md`** — drop one in your repo; thClaws
    walks up from cwd and injects every match into the system prompt,
    the same way git resolves `.gitignore`
    ([Chapter 8](ch08-memory-and-agents-md.md)).
  - **Memory store** — longer-lived facts the agent has learned about
    you, your preferences, and each project, classified as `user` /
    `feedback` / `project` / `reference` and indexed as markdown files.
    Inside a project it lives at `.thclaws/memory/`, so it travels with
    the repo; with no project it falls back to
    `~/.local/share/thclaws/memory/`.
  - **KMS (knowledge bases)** — per-project and per-user wikis the
    agent searches and reads on demand. Drop markdown pages under
    `.thclaws/state/kms/<name>/pages/`, tick the box in the sidebar, and
    the agent gets a table of contents every turn plus a full
    mutation surface (`KmsRead` / `KmsSearch` / `KmsWrite` /
    `KmsAppend` / `KmsDelete`). Search two ways: line-grep by regex,
    or **BM25-ranked** search (`query:`) that ranks pages by relevance
    (title ×4, topic ×2, body) — still no embeddings, just grep + an
    on-disk index, following Andrej Karpathy's LLM-wiki pattern.
    Maintenance is automated by side-channel agents: `/dream` mines
    the 10 most recent sessions, dedupes pages, surfaces insights, and
    writes a dated audit-trail page (review with `git diff`);
    `/kms reconcile` resolves contradictions; `/kms challenge` stress-
    tests an idea against the vault. **Browse + visualize** in the GUI:
    a per-KMS browser sidebar, an Obsidian-style force-directed
    **graph view** of `[[wikilinks]]`, and `/kms html` to export a
    self-contained interactive site you can share or commit.
    **Interoperable** via **OKF** (Google's Open Knowledge Format):
    `/kms export-okf` ships a vendor-neutral bundle and
    `/kms import-okf` turns any OKF bundle into a KMS — a clean
    round-trip for handing knowledge between teams and agents
    ([Chapter 9](ch09-knowledge-bases-kms.md)).

  All three are plain markdown you read, edit, and commit. All three
  survive restart.
- **Skills.** Reusable expert workflows packaged as a directory with
  `SKILL.md` (YAML frontmatter + Markdown instructions the model
  follows) and optional scripts. The agent picks the right skill
  automatically when a user request matches the `whenToUse` trigger,
  or you can invoke one explicitly as `/<skill-name>`. Install with
  `/skill install` from a git URL or `.zip` archive. Discovery looks
  in `.thclaws/skills/`, `~/.config/thclaws/skills/`, plus
  `.claude/skills/` as a fallback location.
- **MCP servers.** The Model Context Protocol lets you plug in tools
  built by third parties — GitHub, filesystems, databases, browsers,
  Slack, and more. Both stdio (spawned subprocess) and HTTP Streamable
  transports are supported, with OAuth 2.1 + PKCE for protected
  servers. Add one with `/mcp add` or ship a `.mcp.json` in your
  project; discovered tools are namespaced by server name and the
  agent can call them like any built-in.
- **Plugin system.** Skills + commands + agent definitions + MCP
  servers bundled under a single manifest (`.thclaws-plugin/plugin.json`
  or `.claude-plugin/plugin.json`), installable from a git URL or a
  `.zip` archive. One install, one uninstall, one version to pin —
  ideal for sharing a team's extensions.
- **Multi-provider.** Anthropic (native + Claude Agent SDK via Claude
  Code auth), OpenAI (Chat Completions + Responses/Codex), Google
  Gemini & Gemma, Alibaba DashScope (Qwen), DeepSeek, Z.ai (GLM Coding
  Plan), NVIDIA NIM, NSTDA Thai LLM (OpenThaiGPT, Typhoon, Pathumma,
  THaLLE), OpenRouter, TokenRouter, Moonshot, xAI, Groq, MiniMax, Qwen Cloud, AtlasCloud, Unifically, Meta, 9router, OpenCode Go, Azure AI Foundry, Ollama (local,
  local Anthropic-compatible, and Ollama Cloud), LMStudio, self-hosted
  vLLM / llama.cpp / LiteLLM, plus a generic **OpenAI-compatible** slot
  (`oai/*`) for Portkey / Helicone / internal proxies — auto-detected by
  model name prefix. Switch models mid-session with `/model` (validated against
  the provider's catalogue) or swap the whole provider with `/provider`.
- **API-ready for standard tooling.** `--serve` exposes
  `/v1/chat/completions` (OpenAI-compatible for Cursor, Aider, n8n,
  openai-python), `/v1/messages` (Anthropic-compatible for the
  `anthropic` SDKs), and `/agent/run` + `/v1/agent/info`
  (thClaws-native for orchestrators). One agent instance can serve
  humans and other software at the same time.
- **Async webhook delivery.** Long-running runs (deploys, builds,
  multi-step research) send the prompt + `x_callback` and close the
  connection; thClaws POSTs the terminal result back when done.
  Survives network blips and orchestrator pod restarts mid-flight.
- **Plan mode.** For multi-step work, the agent can `EnterPlanMode`,
  propose an ordered list of steps, and let *you* review and approve
  before execution. Each step runs sequentially with its own retry
  budget; failures stop the chain so you can decide. Same UX in GUI
  (sidebar with Approve / Cancel / Skip / Retry per step) and REPL
  (`/plan` slash command).
- **Schedule recurring jobs.** `/schedule add` runs an agent on cron
  (`0 9 * * MON-FRI`), at fixed intervals, or whenever a watched
  directory changes (`watchWorkspace`). Three composable layers:
  manual `/schedule run`, in-process scheduler (lives as long as your
  REPL), and a native daemon (`launchd` on macOS / `systemd-user` on
  Linux) that survives reboots. Per-job working directory, optional
  model override, full output capture.
- **Long-running loops & overnight builds.** `/loop` for
  fixed-interval iteration. `/goal` for audit-driven completion (the
  agent works toward a goal until an audit prompt confirms "done" or
  hits the budget). Compose them: `/goal --auto` is a Ralph-style
  overnight builder that keeps going until the goal is satisfied or
  you wake up.
- **Document workflow.** Native PDF, DOCX, PPTX, XLSX read + edit +
  create tools, plus image rendering. The agent can ingest a 50-page
  PDF, summarize it into KMS, and produce a follow-up PowerPoint deck
  — all in one conversation, no separate file-conversion step.
- **Hooks.** Run shell scripts on agent lifecycle events:
  `pre_tool_use`, `post_tool_use`, `permission_denied`, `session_start`,
  `pre_compact`, etc. Audit every Bash invocation, gate `Edit`/`Write`
  through your linter, fire a Slack notification when long sessions
  end. Eight events × per-event environment variables × timeout-with-
  SIGKILL guarantees.
- **Any knowledge worker, not just engineers.** The Chat tab is a
  streaming conversation panel anyone can drive — researchers,
  analysts, PMs, ops, legal, marketing, finance. Ask in natural
  language; the agent reads your files, edits documents, searches
  your knowledge base, drafts outputs. Engineers prefer the Terminal
  tab's REPL. Both share the same sessions and config, so a mixed
  team can switch between interfaces freely without losing context.
- **File viewer & editor in the Files tab.** A working-directory file
  tree with a syntax-highlighted preview pane (CodeMirror 6, ~40
  languages) and server-rendered GFM markdown in a sandboxed iframe.
  Click the pencil icon to edit `.md` in a WYSIWYG editor (TipTap) or
  code in a highlighted editor (CodeMirror) — Cmd/Ctrl+S to save,
  native OS confirm dialog before discarding edits. Auto-refresh
  polling pauses while you're editing so concurrent `Write`/`Edit`
  tool calls from the agent can't clobber your in-progress buffer.
- **Runs on every major platform.** A single native Rust binary runs
  on macOS (Apple Silicon + Intel), Windows, and Linux. Drop the same
  binary into a Docker container to deploy on a VPS, cloud, or
  Kubernetes — one codebase covers everything from a personal laptop
  to a pod on a cluster.
- **Offline-capable.** Ollama (native and Anthropic-compat) lets you run
  entirely against a local model.
- **Open standards, not a walled garden.** thClaws is built on the
  conventions the agent-tooling industry is converging on, not on
  bespoke formats you have to learn only for us. The
  [Model Context Protocol](https://modelcontextprotocol.io/) for
  tool servers. [`AGENTS.md`](https://agents.md) for project
  instructions — the vendor-neutral standard stewarded by the Agentic
  AI Foundation and adopted by Google, OpenAI, Factory, Sourcegraph,
  and Cursor. `SKILL.md` with YAML frontmatter for packaged workflows.
  `.mcp.json` for MCP server configuration. Your configuration is
  portable — between thClaws, other agents that speak the same
  standards, and whatever comes next.
- **Safety first.** A filesystem sandbox scopes file tools to the
  working directory. Destructive shell commands are flagged before
  execution. You approve every mutating tool call unless you've opted
  into auto-approve. Permission requests label which agent is asking
  when multiple are running concurrently (main vs. side-channel vs.
  subagent), so you don't approve a translator's `Bash` thinking it's
  main's.
- **Transparent cost tracking.** Built-in model catalogue carries
  per-token-type pricing (input / output / cached read / cache write /
  reasoning) sourced from
  [LiteLLM](https://github.com/BerriAI/litellm). Every turn's `usage`
  block reports all five fields so orchestrators / UIs can compute
  cost locally without asking the provider.
- **Host thClaws anywhere.** Run it locally on your own machine, or
  deploy it to a host of your choice so a cloud-hosted thClaws runs
  under your account — driven by an orchestrator, or standing alone to
  take work directly. The deploy flow ships as a plugin
  (`/plugin install …-deploy`) so hosts are swappable — the client
  never locks you in.
- **Session resume.** `thclaws --resume last` picks up where you left
  off; `thclaws --resume <id>` jumps to a specific session. Sessions
  live as JSONL under `.thclaws/state/sessions/` — git-friendly,
  grep-friendly, never opaque.
- **Settings.** Every runtime knob — permission mode, thinking budget,
  allowed/disallowed tools, provider endpoints, KMS attachments,
  max output tokens — is one JSON file: `.thclaws/settings.json`
  (project, commit it with the repo) or
  `~/.config/thclaws/settings.json` (user-global).
  `~/.claude/settings.json` is read as a fallback location. API
  keys go in the OS keychain by default (Windows Credential Manager
  / macOS Keychain / Linux Secret Service) with `.env` fallback for
  CI and headless servers. The gear icon in the desktop GUI is a
  visual editor for keys, global/folder `AGENTS.md`, and the secrets
  backend choice.
- **Shell escape.** Prefix any REPL line with `!` to run the rest as a
  shell command directly in your terminal — no tokens, no approval
  prompt, no agent round-trip (e.g. `! git status`).

## What you need

- A supported OS: macOS (arm64 or x86_64), Linux (arm64 or x86_64), or
  Windows (arm64 or x86_64).
- At least one LLM API key — Anthropic, OpenAI, Gemini, OpenRouter,
  Moonshot, xAI, Groq, DashScope, DeepSeek, Z.ai, NVIDIA NIM, NSTDA Thai LLM,
  or Azure AI Foundry. (Or a local Ollama / LMStudio install if you'd
  rather stay offline.)

[Chapter 2](ch02-installation.md) walks through installation and first
launch. [Chapter 6](ch06-providers-models-api-keys.md) covers where
and how to paste keys.

## How this manual is organised

28 chapters of reference material — how to install thClaws and then
every user-facing feature explained once with the commands and
configuration you need:

**Setup**
- [Chapter 2](ch02-installation.md) — Installation
- [Chapter 3](ch03-working-directory-and-modes.md) — Working directory + run modes
- [Chapter 4](ch04-desktop-gui-tour.md) — Desktop GUI tour
- [Chapter 5](ch05-permissions.md) — Permissions
- [Chapter 6](ch06-providers-models-api-keys.md) — Providers, models, API keys

**Core features**
- [Chapter 7](ch07-sessions.md) — Sessions and resume
- [Chapter 8](ch08-memory-and-agents-md.md) — Memory and `AGENTS.md`
- [Chapter 9](ch09-knowledge-bases-kms.md) — Knowledge bases (KMS), including self-improving auto-learn
- [Chapter 10](ch10-slash-commands.md) — Slash commands
- [Chapter 11](ch11-built-in-tools.md) — Built-in tools
- [Chapter 12](ch12-skills.md) — Skills
- [Chapter 13](ch13-hooks.md) — Hooks
- [Chapter 14](ch14-mcp.md) — MCP

**Composing agents**
- [Chapter 15](ch15-subagents.md) — Subagents
- [Chapter 16](ch16-plugins.md) — Plugins
- [Chapter 17](ch17-agent-teams.md) — Agent teams
- [Chapter 18](ch18-plan-mode.md) — Plan mode
- [Chapter 19](ch19-scheduling.md) — Scheduling
- [Chapter 20](ch20-research.md) — `/research` (background research)
- [Chapter 25](ch25-workflows.md) — Workflows (the fourth orchestration tier)

**Reaching thClaws from elsewhere**
- [Chapter 21](ch21-line-and-browser-chat.md) — LINE chat + browser bridge
- [Chapter 23](ch23-telegram.md) — Telegram bot
- [Chapter 24](ch24-messenger.md) — Facebook Page Messenger bot
- [Chapter 27](ch27-thclaws-cloud.md) — thClaws.cloud (catalog + hosted runtime)

**Advanced surfaces & automation**
- [Chapter 26](ch26-gui-shells.md) — GUI Shells (domain-specific frontends)
- [Chapter 28](ch28-browser-automation.md) — Browser automation

If you're new, read Chapter 2 next. If you're migrating from Claude
Code, skip to Chapters 6, 7, 11, and 13. If you already know the
basics and want what's new, the recent additions live in Chapter 9
(auto-learn, `/dream`), Chapter 15 (`/agent` side-channels),
Chapter 21 (LINE), Chapter 23 (Telegram), Chapter 24 (Messenger),
and — the headline for this release — Chapter 27 (thClaws.cloud).
