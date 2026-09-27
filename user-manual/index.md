# thClaws User Manual

A native-Rust AI agent workspace with CLI and desktop GUI. This manual
covers everything from installation through building and deploying
real projects — coding, automation, knowledge bases, and multi-agent
teams.

## Part I — Using thClaws

Chapter numbers are stable — they are the published URLs — so the
groups below are a reading order, not a renumbering. Jump to whichever
group matches what you are trying to do.

### Getting started

| # | Chapter |
|---|---|
| 1 | [What is thClaws?](ch01-what-is-thclaws.md) |
| 2 | [Installation](ch02-installation.md) |
| 3 | [Working directory & running modes](ch03-working-directory-and-modes.md) |
| 4 | [Desktop GUI tour](ch04-desktop-gui-tour.md) |

### Everyday use

| # | Chapter |
|---|---|
| 6 | [Providers, models & API keys](ch06-providers-models-api-keys.md) |
| 7 | [Sessions](ch07-sessions.md) |
| 10 | [Slash commands](ch10-slash-commands.md) |
| 11 | [Built-in tools](ch11-built-in-tools.md) |
| 28 | [Browser automation](ch28-browser-automation.md) |

### Keeping the agent in bounds

How you decide what the agent may do before it does it — and what
leaves your machine.

| # | Chapter |
|---|---|
| 5 | [Permissions](ch05-permissions.md) |
| 18 | [Plan mode](ch18-plan-mode.md) |
| 32 | [Thai PII masking](ch32-thai-pii-masking.md) |

### What the agent knows

| # | Chapter |
|---|---|
| 8 | [Memory & project instructions (`CLAUDE.md` / `AGENTS.md`)](ch08-memory-and-agents-md.md) |
| 9 | [Knowledge bases (KMS)](ch09-knowledge-bases-kms.md) |
| 20 | [Background research (`/research`)](ch20-research.md) |

### Extending the agent

| # | Chapter |
|---|---|
| 12 | [Skills](ch12-skills.md) |
| 13 | [Hooks](ch13-hooks.md) |
| 14 | [MCP servers](ch14-mcp.md) |
| 16 | [Plugins](ch16-plugins.md) |
| 26 | [GUI Shells](ch26-gui-shells.md) |

### Running several agents

| # | Chapter |
|---|---|
| 35 | [Agents in a workspace](ch35-agents-in-a-workspace.md) |
| 15 | [Subagents](ch15-subagents.md) |
| 17 | [Agent Teams](ch17-agent-teams.md) |
| 25 | [Workflows (`/workflow run`)](ch25-workflows.md) |
| 19 | [Scheduling](ch19-scheduling.md) |
| 31 | [Loops and goals (`/loop`, `/goal`)](ch31-loops-and-goals.md) |

### Reaching thClaws from elsewhere

| # | Chapter |
|---|---|
| 21 | [LINE chat & web browser bridge](ch21-line-and-browser-chat.md) |
| 23 | [Telegram bot](ch23-telegram.md) |
| 24 | [Facebook Page Messenger bot](ch24-messenger.md) |
| 33 | [thClaws Remote](ch33-thclaws-remote.md) |
| 30 | [Job Artifacts (files in/out for orchestrators)](ch30-job-artifacts.md) |

### thClaws.cloud and managed copies

| # | Chapter |
|---|---|
| 27 | [thClaws.cloud (Agent Templates + hosted + gateway)](ch27-thclaws-cloud.md) |
| 34 | [Managed builds and org policy](ch34-managed-builds.md) |

## Appendices

| # | Appendix |
|---|---|
| A | [Providers, models & prices (thClaws.cloud gateway)](appendix-a-providers-models-prices.md) |

## Conventions used in this manual

- `❯` is the REPL prompt; what follows on that line is what **you** type.
- `$` is a shell prompt outside thClaws.
- `[tool: Bash: …]` / `[tokens: Xin/Yout · Ts]` lines show what thClaws prints back.
- Code fences without a language are terminal output; fences with a language (`rust`, `json`, `bash`) are files you write or commands you run.
- **Bold** inside a command label indicates a required input (e.g. **name**).
- Every chapter is self-contained — skip around freely.
