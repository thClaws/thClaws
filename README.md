# thClaws 🦞

> **Open-source Agent Harness Platform** — a native AI agent workspace that codes, automates, remembers, and coordinates. Runs on your own machine. Sovereign by design.

thClaws is a **native-Rust AI agent workspace** that runs locally on your machine. Not just coding — it edits code, automates workflows, searches your knowledge bases, and coordinates teams of agents, all in one binary. You tell it what you want in natural language; it reads your files, runs commands, uses tools, and talks back to you while it works.

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Platform: macOS · Windows · Linux](https://img.shields.io/badge/platform-macOS%20·%20Windows%20·%20Linux-lightgrey.svg)](#installation)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)

---

## Three interfaces, one binary

- **Desktop GUI** (`thclaws`) — a native window with Terminal, Chat, Files, and optional Team tabs.
- **CLI REPL** (`thclaws --cli`) — an interactive terminal prompt for SSH, headless servers, or when you want zero GUI overhead.
- **Non-interactive mode** (`thclaws -p "prompt"`) — runs a single turn and exits. Handy for scripts, CI pipelines, and shell one-liners.

---

## What makes it different

- **Multi-provider.** Anthropic, OpenAI, Gemini, Alibaba DashScope, OpenRouter, Ollama (local and Anthropic-compatible), and Agentic Press — auto-detected by model name prefix. Switch mid-session with `/model` or swap the whole provider with `/provider`.

- **Any knowledge worker, not just engineers.** Chat tab for researchers, PMs, ops, legal, marketing, finance — natural-language prompts, file access, knowledge-base lookup, drafting. Terminal tab for engineers who want the raw REPL. Same engine, same sessions, same config — different preferred surface.

- **Open standards, not a walled garden.** Built on the conventions the agent-tooling industry is converging on, not bespoke formats you have to learn only for us. [Model Context Protocol](https://modelcontextprotocol.io/) for tool servers. [`AGENTS.md`](https://agents.md) for project instructions — the vendor-neutral standard adopted by Google, OpenAI, Factory, Sourcegraph, and Cursor. `SKILL.md` with YAML frontmatter for packaged workflows. Your configuration is portable between thClaws, other agents that speak the same standards, and whatever comes next.

- **Skills.** Reusable expert workflows packaged as a directory with `SKILL.md` plus optional scripts. The agent picks the right skill automatically when a request matches the `whenToUse` trigger, or you can invoke one explicitly as `/<skill-name>`. Install from a git URL or `.zip` archive with `/skill install`.

- **MCP servers.** Plug in tools built by third parties — GitHub, filesystems, databases, browsers, Slack, and more. Both stdio and HTTP Streamable transports, with OAuth 2.1 + PKCE for protected servers. Add one with `/mcp add` or ship a `.mcp.json` in your project.

- **Plugin system.** Skills + commands + agent definitions + MCP servers bundled under a single manifest, installable from git or `.zip`. One install, one uninstall, one version to pin — ideal for sharing a team's extensions.

- **Memory & project instructions.** Drop an `AGENTS.md` (or `CLAUDE.md`) in your repo — thClaws walks up from `cwd` and injects every match into the system prompt. A persistent memory store holds longer-lived facts the agent has learned about you, classified as `user` / `feedback` / `project` / `reference` and stored as markdown you can read, edit, or commit.

- **Knowledge bases (KMS).** Per-project and per-user wikis the agent can search and read on demand. Drop markdown pages under `.thclaws/kms/<name>/pages/`, give each a one-line entry in `index.md`, and the agent gets a table of contents every turn plus `KmsRead` / `KmsSearch` tools. No embeddings — grep + read, following Andrej Karpathy's LLM-wiki pattern.

- **Agent orchestration.** Delegate subtasks to isolated sub-agents via the `Task` tool — each gets its own tool registry and can recurse up to 3 levels deep. Scale further with **Agent Teams**: multiple thClaws processes coordinating through a shared mailbox and task queue, each in its own tmux pane and optional git worktree. One agent writes your backend while a teammate builds the frontend in parallel, lead merges the branches when both are done.

- **Settings as one file.** Every knob — permission mode, thinking budget, allowed/disallowed tools, provider endpoints, KMS attachments — lives in `.thclaws/settings.json` (project) or `~/.config/thclaws/settings.json` (user). API keys go in the OS keychain by default (macOS Keychain / Windows Credential Manager / Linux Secret Service) with `.env` fallback for CI.

- **Safety first.** A filesystem sandbox scopes file tools to the working directory. Destructive shell commands are flagged before execution. You approve every mutating tool call unless you've opted into auto-approve.

- **Offline-capable.** Ollama (native and Anthropic-compatible) lets you run entirely against a local model — no cloud round-trip, no API key.

- **Deploy what you build.** Ship the landing pages, web apps, APIs, and AI agents you create through [Agentic Press Hosting](https://agentic-press.com) (partnered with SIS Cloud Service and Artech.Cloud) — or any other host you prefer. Schedule agents on cron, respond to webhooks, stream from public URLs. The deploy flow ships as a plugin (`/plugin install …-deploy`), so hosts are swappable; the client never locks you in.

- **Shell escape.** Prefix any REPL line with `!` to run a shell command directly — no tokens, no approval prompt, no agent round-trip (`! git status`, `! ls`, etc.).

---

## Installation

### Pre-built binaries

Download the latest release for your platform from the [Releases page](https://github.com/thClaws/thClaws/releases) or from [thclaws.ai/downloads](https://thclaws.ai/downloads).

Supported: macOS (Apple Silicon & Intel), Windows (x86_64 & ARM64), Linux (x86_64 & ARM64).

### Build from source

**Prerequisites:** Rust 1.78+, Node.js 20+, pnpm 9+.

```sh
git clone https://github.com/thClaws/thClaws.git
cd thClaws

# Build frontend (React + Vite, bundled as a single HTML file)
cd frontend && pnpm install && pnpm build && cd ..

# Build Rust (CLI + GUI)
cargo build --release --features gui --bin thclaws

# Run
./target/release/thclaws          # GUI
./target/release/thclaws --cli    # CLI
./target/release/thclaws -p "what does src/main.rs do?"  # one-shot
```

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
```

---

## Configuration

thClaws reads settings in this precedence order (higher wins):

1. CLI flags
2. `.thclaws/settings.json` (project)
3. `~/.config/thclaws/settings.json` (user)
4. `~/.claude/settings.json` (fallback location)
5. Compiled-in defaults

Open-standard files are honored directly:

- `CLAUDE.md` / `AGENTS.md` — system prompt additions, walked up from `cwd`
- `.thclaws/skills/` / `.claude/skills/` — skill catalog
- `.thclaws/agents/` / `.claude/agents/` — subagent definitions
- `.mcp.json` / `.thclaws/mcp.json` — MCP server configuration
- `.thclaws-plugin/plugin.json` / `.claude-plugin/plugin.json` — plugin manifest

API keys are **never stored in config files** — only in the OS keychain (default) or `.env`.

---

## Documentation

- **Official site** — [thclaws.ai](https://thclaws.ai)
- **Full user manual** — [thclaws.ai/manual](https://thclaws.ai/manual) *(soon)* or the [`user-manual/`](https://github.com/thClaws/user-manual) companion repo. 24 chapters covering every feature plus 7 walkthrough case studies (static site deploy, Node.js reservation site, news-aggregation agent, etc.).
- [Contributing](CONTRIBUTING.md) — dev setup, PR flow, commit style
- [Changelog](CHANGELOG.md) — version history
- [Code of Conduct](CODE_OF_CONDUCT.md) — Contributor Covenant 2.1
- [Security](SECURITY.md) — vulnerability disclosure

For books, training, and commercial deployment, see [agentic-press.com](https://agentic-press.com).

---

## License

Dual-licensed under either:

- [MIT License](LICENSE-MIT)
- [Apache License 2.0](LICENSE-APACHE)

at your option. Contributions are accepted under the same dual license.

---

## About

thClaws is developed by **ThaiGPT Co., Ltd.** and published under a dual MIT/Apache-2.0 license. The client is free and open source forever. Enterprise Edition, hosting, and support are commercial offerings — see [agentic-press.com](https://agentic-press.com) or contact [jimmy@thaigpt.com](mailto:jimmy@thaigpt.com).

Built in Thailand. Meant for the world.
