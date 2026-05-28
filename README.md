<div align="center">

# thClaws 🦞

**The open-source agent harness, in your terminal and on your desktop.**

A native-Rust AI agent workspace that codes, automates, remembers, and coordinates — running on your own machine. One binary. Sovereign by design. Built by a small team hacking in public.

[![GitHub stars](https://img.shields.io/github/stars/thClaws/thClaws?style=social)](https://github.com/thClaws/thClaws/stargazers)
[![Contributors](https://img.shields.io/github/contributors/thClaws/thClaws.svg)](https://github.com/thClaws/thClaws/graphs/contributors)
[![Discussions](https://img.shields.io/github/discussions/thClaws/thClaws)](https://github.com/thClaws/thClaws/discussions)
[![Release](https://img.shields.io/github/v/release/thClaws/thClaws)](https://github.com/thClaws/thClaws/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/thClaws/thClaws/release.yml?branch=main)](https://github.com/thClaws/thClaws/actions)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Platform](https://img.shields.io/badge/platform-macOS%20·%20Windows%20·%20Linux-lightgrey.svg)](#installation)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)

[Website](https://thclaws.ai) · [Download](https://thclaws.ai/downloads.html) · [Manual](https://thclaws.ai/manual) · [Discussions](https://github.com/thClaws/thClaws/discussions) · [Contribute](#contribute)

</div>

---

## See it work

Three tabs, one binary — captured from a live thClaws session looking at its own source.

<div align="center">

<a href="docs/img/screens-carousel.webp"><img src="docs/img/screens-carousel.webp" alt="thClaws Desktop GUI — cycling through Files, Terminal, and Chat tabs" width="900" /></a>

**Files** (codemirror + tiptap)  ·  **Terminal** (REPL · slash commands · ANSI tool output)  ·  **Chat** (markdown render · tool indicators)

</div>

---

## Hacking in public

thClaws started in April 2026. As of this writing the project has shipped **20+ releases**, drawn **27 contributors**, and lands roughly a release a week. It's developed by a small team at **ThaiGPT Co., Ltd.** — and a meaningful chunk of the codebase comes from outside contributors who heard about it and stayed.

We're aiming for **v1.0 = "the multi-platform agent"**: the same agent loop on your desktop, in your terminal, and bridged into Telegram, Discord, Slack, WhatsApp, Facebook Messenger, and LINE. Telegram, LINE, and Messenger are already shipping. Discord, Slack, and WhatsApp are next — and they're great places to plug in. ([Contribute →](#contribute))

> Built in Thailand. Meant for the world.

---

## Four surfaces, one engine

The same `Agent` loop, `Session`, and `ToolRegistry` back every UX:

- **Desktop GUI** (`thclaws`) — native window with Terminal, Chat, Files, and optional Team tabs.
- **CLI REPL** (`thclaws --cli`) — interactive terminal prompt for SSH, headless servers, or zero-GUI workflows.
- **Non-interactive mode** (`thclaws -p "prompt"`) — single turn, exits. Pipe-friendly for scripts and CI. `-v` for token usage on stderr.
- **Webapp** (`thclaws --serve --port 7878`) — same engine over WebSocket/HTTP. SSH-tunnel for "Claude Code anywhere" without opening a port.

---

## Features

Everything's in one binary. Pick the surface that fits the task, swap the provider, drop in a skill, glue in an MCP server, then walk away while a scheduled job or background agent finishes the work.

- **Multi-provider** — Anthropic (native + Claude Agent SDK via Claude Code auth), OpenAI (Chat Completions + Responses/Codex), Google Gemini & Gemma, Alibaba DashScope (Qwen), DeepSeek, Z.ai (GLM Coding Plan), NVIDIA NIM, NSTDA Thai LLM (OpenThaiGPT, Typhoon, Pathumma, THaLLE), OpenRouter, Agentic Press, Azure AI Foundry, Ollama (local + Anthropic-compatible + Cloud), LMStudio, plus a generic **OpenAI-compatible** slot (`oai/*`) for LiteLLM / Portkey / Helicone / vLLM / internal proxies. Switch mid-session with `/model` or `/provider`.
- **Open standards, not a walled garden** — [Model Context Protocol](https://modelcontextprotocol.io/) for tools, [`AGENTS.md`](https://agents.md) for project instructions (adopted by Google, OpenAI, Factory, Sourcegraph, Cursor), `SKILL.md` with YAML frontmatter for packaged workflows. Configuration portable between thClaws, other compliant agents, and whatever comes next.
- **Skills, plugins, MCP servers, hooks** — extend the agent without touching Rust. Skills are folders with a `SKILL.md`. Plugins bundle skills + commands + agent definitions + MCP servers under one manifest. MCP brings in third-party tools (GitHub, filesystems, browsers, Slack…) over stdio or HTTP-Streamable with OAuth 2.1+PKCE. Hooks run shell scripts on lifecycle events (`pre_tool_use`, `permission_denied`, `session_start`, …).
- **Three tiers of agent orchestration** — model-driven subagents (`Task` tool, blocking, up to 3 levels deep); user-driven concurrent side-channels (`/agent <name> <prompt>`, parallel to main, own cancel token); multi-process **Agent Teams** with shared mailbox, task queue, tmux panes, and optional git worktrees.
- **Knowledge bases (KMS) + `/dream`** — per-project and per-user wikis under `.thclaws/kms/<name>/pages/`, indexed by a one-line `index.md`. Grep + read (no embeddings), following Andrej Karpathy's LLM-wiki pattern. `/dream` mines your recent sessions in the background and writes a dated audit-trail page to review with `git diff`.
- **Plan mode** — `EnterPlanMode` proposes an ordered list of steps you Approve / Cancel / Skip / Retry. Same UX in GUI sidebar and `/plan` slash command.
- **Schedule recurring jobs** — `/schedule add` runs an agent on cron, fixed intervals, or filesystem changes (`watchWorkspace`). In-process scheduler for ephemeral, native daemon (`launchd` / `systemd-user`) for survives-reboot.
- **Long-running loops & overnight builds** — `/loop` for fixed-interval iteration, `/goal` for audit-driven completion. `/goal --auto` is a Ralph-style overnight builder that keeps going until the goal is satisfied or you wake up.
- **Document workflow** — native PDF, DOCX, PPTX, XLSX read + edit + create tools, plus image rendering. Ingest a 50-page PDF, summarize into KMS, produce a follow-up deck — one conversation.
- **Memory & project instructions** — `AGENTS.md` (or `CLAUDE.md`) walked up from `cwd` and injected into the system prompt. Persistent memory store classified as `user` / `feedback` / `project` / `reference`, stored as markdown you can read, edit, or commit.
- **Settings as one file** — `.thclaws/settings.json` (project) or `~/.config/thclaws/settings.json` (user). API keys go in the OS keychain by default (macOS Keychain / Windows Credential Manager / Linux Secret Service) with `.env` fallback for CI.
- **Session resume** — `thclaws --resume last` or `--resume <id>`. Sessions live as JSONL under `.thclaws/sessions/` — git-friendly, grep-friendly, never opaque.
- **Safety first** — filesystem sandbox scoped to working directory. Destructive shell commands flagged. You approve every mutating tool call unless you've opted into auto-approve. Permission requests label which agent is asking when multiple are running.
- **Offline-capable** — Ollama (native + Anthropic-compatible) lets you run entirely against a local model. No cloud round-trip, no API key.
- **Deploy what you build** — ship landing pages, web apps, APIs, and AI agents through [Agentic Press Hosting](https://agentic-press.com) (partnered with SIS Cloud Service and Artech.Cloud) — or any host you prefer. Deploy flow ships as a plugin (`/plugin install …-deploy`), so hosts are swappable. The client never locks you in.
- **Shell escape** — prefix any REPL line with `!` to run a shell command directly. No tokens, no approval prompt, no agent round-trip (`! git status`, `! ls`).

---

## Contribute

**We'd love your help.** thClaws is built in the open by a small team and ~25 contributors so far. Reviews are typically fast, the codebase is approachable, and there's plenty of room to make a real dent.

### Quick start for contributors

```sh
git clone https://github.com/thClaws/thClaws.git
cd thClaws

# One-shot: build frontend, then cargo build --features gui
./scripts/build.sh           # macOS / Linux
./scripts/build.ps1          # Windows PowerShell

# Verification suite (cargo fmt --check, clippy, tsc, cargo test)
./scripts/build.sh --check

# Run
cargo run --features gui              # GUI
cargo run -- --cli                    # CLI REPL
cargo run -- -p "explain crates/core" # one-shot
```

**Prerequisites:** Rust 1.85+, Node.js 20+, pnpm 9+. The helper enforces frontend-before-cargo order (the GUI build embeds `frontend/dist/index.html` at compile time via `include_str!`). See [CONTRIBUTING.md](CONTRIBUTING.md) for the full PR workflow.

### Where we need help right now

If you want to land something impactful, these are the places we'd most love a hand:

- 🚀 **Discord adapter** — bridge the agent into a Discord guild (same shape as our shipping Telegram + Messenger adapters in `crates/core/src/messenger/` and friends).
- 🚀 **Slack adapter** — same idea, Slack-side.
- 🚀 **WhatsApp adapter** — round out the v1.0 multi-platform thesis.
- 🪟 **Windows ARM polish** — installer, GUI smoke-test on Surface / Snapdragon X.
- 🧠 **KMS embeddings (opt-in)** — current KMS is grep + read; an optional embeddings layer alongside (not replacing) would be welcome.
- 🌏 **i18n & translations** — manual chapters and UI strings are EN/TH today; we'd love help with more locales.
- 🧩 **Skills, plugins, MCP servers** — no Rust needed. Build one, ship it on GitHub, list it on the marketplace. (See [Skills documentation](https://thclaws.ai/manual).)
- 📝 **Docs, examples, walkthroughs** — typo fixes welcome, case studies even more welcome.

Browse [**good first issues**](https://github.com/thClaws/thClaws/issues?q=is%3Aopen+is%3Aissue+label%3A%22good+first+issue%22) or [**help wanted**](https://github.com/thClaws/thClaws/issues?q=is%3Aopen+is%3Aissue+label%3A%22help+wanted%22) to start. If you want to land something bigger, drop into [Discussions](https://github.com/thClaws/thClaws/discussions) and pitch it first — that's the lowest-friction path.

### Codebase tour

Everything ships from a single crate, `crates/core/`. The interesting modules:

| Path | What it does |
|---|---|
| `crates/core/src/agent.rs` + `agent_runtime.rs` | The agent loop — turns, tool calls, streaming, cancellation |
| `crates/core/src/commands.rs` | Slash-command registry (`/help`, `/model`, `/skill`, `/schedule`, …) |
| `crates/core/src/kms.rs` | Knowledge bases (per-project + per-user) with `KmsRead`/`Search`/`Write`/`Append` tools |
| `crates/core/src/memory.rs` | Persistent memory store (user / feedback / project / reference) |
| `crates/core/src/mcp.rs` | MCP transport (stdio + HTTP-Streamable + OAuth 2.1 PKCE) |
| `crates/core/src/hooks.rs` | Lifecycle hooks (`pre_tool_use`, `post_tool_use`, …) |
| `crates/core/src/marketplace.rs` | Skill / plugin marketplace client |
| `crates/core/src/permissions.rs` | Filesystem sandbox + tool approval flow |
| `crates/core/src/compaction.rs` | Context-window management |
| `crates/core/src/messenger/`, `line/` | Multi-platform adapters (the v1.0 thesis) |
| `crates/core/src/gui.rs` + `ipc.rs` | Tauri ↔ React bridge |
| `frontend/src/` | React + Vite GUI (bundled into a single HTML file via `include_str!`) |

Deeper engineering reference: [`thclaws-technical-manual/`](thclaws-technical-manual/) — agent loop, provider abstraction, KMS internals, side-channel + `/dream` plumbing, schedule daemon, hooks lifecycle, plan-mode driver, and the rest. **Read this before sending non-trivial PRs.**

### Contributors

A huge thank you to the people who've shaped thClaws so far:

<a href="https://github.com/thClaws/thClaws/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=thClaws/thClaws" alt="thClaws contributors" />
</a>

---

## Installation

### Build from source

The fastest path if you want to contribute or just see how it's built. See [Contribute → Quick start](#quick-start-for-contributors).

### Pre-built binaries

Download for your platform from the [Releases page](https://github.com/thClaws/thClaws/releases) or [thclaws.ai/downloads](https://thclaws.ai/downloads.html).

Supported: macOS (Apple Silicon & Intel), Windows (x86_64 & ARM64), Linux (x86_64 & ARM64).

<details>
<summary><b>Linux runtime dependencies</b> — needed for the GUI binary on headless servers</summary>

The Linux GUI binary links against Wayland and webkit2gtk at runtime. Desktop distros (Ubuntu Desktop, Fedora Workstation) ship these by default. **Headless servers** (cloud VMs, EC2, Docker images without a display) typically don't — `thclaws` will fail at startup with `error while loading shared libraries: libwayland-client.so.0`.

Two options:

**(a) Use CLI mode** — no GUI deps required:

```sh
thclaws --cli
thclaws -p "what does src/main.rs do?"
```

**(b) Install the GUI deps**:

```sh
# Debian / Ubuntu
sudo apt install libwayland-client0 libwebkit2gtk-4.1-0 libsoup-3.0-0

# Fedora / RHEL
sudo dnf install wayland libsoup3 webkit2gtk4.1
```

</details>

---

## Quick start

```sh
# First run: pick a secrets backend (OS keychain or .env) when prompted
thclaws

# Configure a provider (inside the REPL)
❯ /provider anthropic
❯ /model claude-sonnet-4-6

# Or try OpenRouter for 300+ models via one key
❯ /provider openrouter
❯ /model openrouter/anthropic/claude-sonnet-4-6

# Drop an AGENTS.md or CLAUDE.md in your repo — it's read automatically

# Useful slash commands
❯ /help         # list everything
❯ /models       # list available models for the current provider
❯ /kms          # list attached knowledge bases
❯ /skill install https://github.com/anthropics/skills.git
❯ /mcp add github https://mcp.github.com
❯ ! git status  # shell escape

# Concurrent and long-running work
❯ /agent translator แปลไฟล์ src/foo.md เป็นภาษาไทย   # spawn a side-channel agent
❯ /agents                                            # list active background agents
❯ /dream                                             # consolidate KMS in the background
❯ /schedule add --cron "0 9 * * MON-FRI" "review the day's PRs"

# Headless mode
thclaws -p "summarize CHANGELOG.md"          # one-shot to stdout
thclaws -p "summarize CHANGELOG.md" -v       # + token usage on stderr
thclaws --resume last                        # pick up the latest session

# Web access
thclaws --serve --port 7878   # then ssh -L 7878:localhost:7878 user@remote
```

---

## Configuration

Settings are read in this precedence (higher wins):

1. CLI flags
2. `.thclaws/settings.json` (project)
3. `~/.config/thclaws/settings.json` (user)
4. `~/.claude/settings.json` (fallback)
5. Compiled-in defaults

Open-standard files honored directly:

- `CLAUDE.md` / `AGENTS.md` — system prompt additions, walked up from `cwd`
- `.thclaws/skills/` / `.claude/skills/` — skill catalog
- `.thclaws/agents/` / `.claude/agents/` — subagent definitions
- `.mcp.json` / `.thclaws/mcp.json` — MCP server configuration
- `.thclaws-plugin/plugin.json` / `.claude-plugin/plugin.json` — plugin manifest

API keys are **never stored in config files** — only the OS keychain (default) or `.env`.

---

## Community

- 💬 **[GitHub Discussions](https://github.com/thClaws/thClaws/discussions)** — questions, ideas, show-and-tell. The best place to start.
- 🐛 **[Issues](https://github.com/thClaws/thClaws/issues)** — bug reports + concrete feature requests.
- ✉️ **Email** — for security disclosures or commercial inquiries: [jimmy@thaigpt.com](mailto:jimmy@thaigpt.com) (see also [SECURITY.md](SECURITY.md)).

---

## Documentation

- **Official site** — [thclaws.ai](https://thclaws.ai)
- **Full user manual** — [thclaws.ai/manual](https://thclaws.ai/manual) or [`user-manual/`](user-manual/) (EN) / [`user-manual-th/`](user-manual-th/) (ภาษาไทย) — 24 chapters + 7 walkthrough case studies (static site deploy, Node.js reservation site, news-aggregation agent, …).
- **Technical manual** — [`thclaws-technical-manual/`](thclaws-technical-manual/) — engineering reference.
- [Contributing](CONTRIBUTING.md) — dev setup, PR flow, commit style
- [Changelog](CHANGELOG.md) — version history
- [Code of Conduct](CODE_OF_CONDUCT.md) — Contributor Covenant 2.1
- [Security](SECURITY.md) — vulnerability disclosure
- [Enterprise](ENTERPRISE.md) — EE features, private marketplaces, policy overrides

For books, training, and commercial deployment, see [agentic-press.com](https://agentic-press.com).

---

## License

Dual-licensed under either:

- [MIT License](LICENSE-MIT)
- [Apache License 2.0](LICENSE-APACHE)

at your option. Contributions are accepted under the same dual license — you keep your copyright; we just need permission to ship it.

---

## About

thClaws is developed by **ThaiGPT Co., Ltd.** and published under a dual MIT/Apache-2.0 license. The client is free and open source forever. Enterprise Edition, hosting, and support are commercial offerings — see [agentic-press.com](https://agentic-press.com) or contact [jimmy@thaigpt.com](mailto:jimmy@thaigpt.com).

> Built in Thailand. Meant for the world.
