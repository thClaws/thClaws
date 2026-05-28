# thClaws User Manual

A native-Rust AI agent workspace with CLI and desktop GUI. This manual
covers everything from installation through building and deploying
real projects — coding, automation, knowledge bases, and multi-agent
teams.

## Part I — Using thClaws

| # | Chapter |
|---|---|
| 1 | [What is thClaws?](ch01-what-is-thclaws.md) |
| 2 | [Installation](ch02-installation.md) |
| 3 | [Desktop GUI tour](ch04-desktop-gui-tour.md) |
| 4 | [Working directory & running modes](ch03-working-directory-and-modes.md) |
| 5 | [Permissions](ch05-permissions.md) |
| 6 | [Providers, models & API keys](ch06-providers-models-api-keys.md) |
| 7 | [Sessions](ch07-sessions.md) |
| 8 | [Memory & project instructions (`CLAUDE.md` / `AGENTS.md`)](ch08-memory-and-agents-md.md) |
| 9 | [Knowledge bases (KMS)](ch09-knowledge-bases-kms.md) |
| 10 | [Slash commands](ch10-slash-commands.md) |
| 11 | [Built-in tools](ch11-built-in-tools.md) |
| 12 | [Skills](ch12-skills.md) |
| 13 | [Hooks](ch13-hooks.md) |
| 14 | [MCP servers](ch14-mcp.md) |
| 15 | [Subagents](ch15-subagents.md) |
| 16 | [Plugins](ch16-plugins.md) |
| 17 | [Agent teams](ch17-agent-teams.md) |
| 19 | [Scheduling](ch19-scheduling.md) |
| 20 | [Background research (`/research`)](ch20-research.md) |
| 21 | [LINE chat & web browser bridge](ch21-line-and-browser-chat.md) |
| 22 | [Paperclip adapter](ch22-paperclip-adapter.md) |
| 23 | [Telegram bot](ch23-telegram.md) |
| 24 | [Facebook Page Messenger bot](ch24-messenger.md) |

> **Part II — Case studies (chapters 25–28)** — applied walkthroughs
> for building real projects with thClaws (static sites, Node.js apps,
> AI agents, deploying to Agentic Press) are in active development and
> will be added to this manual as each is reviewed and ready.

## Conventions used in this manual

- `❯` is the REPL prompt; what follows on that line is what **you** type.
- `$` is a shell prompt outside thClaws.
- `[tool: Bash: …]` / `[tokens: Xin/Yout · Ts]` lines show what thClaws prints back.
- Code fences without a language are terminal output; fences with a language (`rust`, `json`, `bash`) are files you write or commands you run.
- **Bold** inside a command label indicates a required input (e.g. **name**).
- Every chapter is self-contained — skip around freely.
