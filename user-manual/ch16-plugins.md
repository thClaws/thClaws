# Chapter 16 — Plugins

A plugin is a **bundle** of skills + legacy commands + agent
definitions + MCP servers, managed as one thing — install once, get
everything.

Plugins are the long-form answer to: "I want to hand a teammate (or
another machine) a folder with our team's agent extensions, and have
them just work."

## Manifest

Every plugin has a manifest at its root, looked up in this order:

1. `.thclaws-plugin/plugin.json` (thClaws-native, preferred)
2. `.claude-plugin/plugin.json` (Claude Code compat fallback)

Shape:

```json
{
  "name": "agentic-press-deploy",
  "version": "1.0.0",
  "description": "Deploy apps to Agentic Press Hosting",
  "author": "Agentic Press",
  "skills": ["skills"],
  "commands": ["commands"],
  "agents": ["agents"],
  "mcpServers": {
    "deploy-hub": {
      "transport": "http",
      "url": "https://api.agentic.cloud/mcp"
    }
  }
}
```

All paths are relative to the manifest root. Each contribution:

| Field | Points to | Each entry is |
|---|---|---|
| `skills` | Directories of `<name>/SKILL.md` subdirs | A skill-catalog directory ([ch 12](ch12-skills.md)) |
| `commands` | Directories of `*.md` prompt templates | A commands directory ([ch 10](ch10-slash-commands.md#resolution-order)) |
| `agents` | Directories of `*.md` agent defs | An agent-catalog directory ([ch 15](ch15-subagents.md)) |
| `mcpServers` | JSON map of server configs | Same shape as `mcp.json` ([ch 14](ch14-mcp.md)) |

## Directory layout of an installed plugin

Installed plugins live under:

| Scope | Install root | Registry file |
|---|---|---|
| Project | `.thclaws/plugins/<name>/` | `.thclaws/plugins.json` |
| User | `~/.config/thclaws/plugins/<name>/` | `~/.config/thclaws/plugins.json` |

The registry is a JSON array tracking name, source URL, install path,
version, and enabled flag.

## Marketplace

`/plugin marketplace` browses the curated catalog at
[`thClaws/marketplace`](https://github.com/thClaws/marketplace), same
shape as the skill marketplace. Three discovery commands plus a
name-based install:

```
❯ /plugin marketplace
plugin marketplace (baseline 2026-04-29, 1 plugin(s))
── workflow ──
  productivity             — Task management, workplace memory, visual dashboard
install with: /plugin install <name>   |   detail: /plugin info <name>
```

```
❯ /plugin info productivity
license:      Apache-2.0 (open)
homepage:     https://github.com/thClaws/marketplace/tree/main/plugins/productivity
install with: /plugin install productivity (resolves to https://github.com/thClaws/marketplace.git#main:plugins/productivity)
```

```
❯ /plugin install productivity
plugin 'productivity' installed (project, 1 skill dir(s)) → .thclaws/plugins/productivity
skills callable in this session — no restart needed
```

Use `/plugin show <name>` for an INSTALLED plugin's detail (path,
contributions, scope) — the marketplace's `/plugin info` is for
catalog entries before install.

## Install (custom URL)

For plugins not in the marketplace:

```
❯ /plugin install https://github.com/agentic-press/deploy-plugin.git
plugin 'agentic-press-deploy' installed (project) → .thclaws/plugins/agentic-press-deploy
Skills refreshed and callable this session.
1 plugin-contributed MCP server(s) still need a restart to spawn — or use /mcp add to register them now.
```

The `<git-url>#<branch>:<subpath>` extension also works for plugins
(same as skills) — useful when the upstream repo bundles many plugins
under a `plugins/` directory:

```
❯ /plugin install https://github.com/anthropics/knowledge-work-plugins.git#main:productivity
```

Plugin-contributed **skills** activate immediately in the current
session — the SkillTool's live store is refreshed and the new skill
shows in the system prompt's `# Available skills` section. Plugin-
contributed **MCP servers** still require either `/mcp add` (to
live-register) or a restart, because the auto-spawn path would need
to diff against already-running servers. Commands and agent
definitions live in directories that are re-scanned on next use, so
they're live too.

From a `.zip`:

```
❯ /plugin install --user https://agentic-press.com/plugins/deploy-v1.zip
```

Scope selection:

- default (no flag) → project install (`.thclaws/plugins/`)
- `--user` → user global (`~/.config/thclaws/plugins/`)

## List / show / enable / disable / remove

```
❯ /plugins
  agentic-press-deploy v1.0.0 (enabled) → .thclaws/plugins/agentic-press-deploy
    source: https://github.com/agentic-press/deploy-plugin.git
  big-noisy-plugin v0.2.3 (disabled) → .thclaws/plugins/big-noisy-plugin

❯ /plugin show agentic-press-deploy
  agentic-press-deploy v1.0.0 (enabled)
  path: .thclaws/plugins/agentic-press-deploy
  source: https://github.com/agentic-press/deploy-plugin.git
  description: Deploy apps to Agentic Press Hosting
  author: Agentic Press
  skill dirs: skills
  command dirs: commands
  agent dirs: agents
  mcp servers: deploy-hub

❯ /plugin disable big-noisy-plugin
plugin 'big-noisy-plugin' disabled (restart to drop its contributions)

❯ /plugin enable big-noisy-plugin
plugin 'big-noisy-plugin' enabled (restart to pick up its contributions)

❯ /plugin remove big-noisy-plugin
plugin 'big-noisy-plugin' removed (restart to drop active tools)
```

Disable ≠ remove: files stay on disk, only the `enabled` flag flips.

## What a plugin contributes

On next start, thClaws's discovery walks both the standard dirs **and**
each enabled plugin's declared contribution dirs:

- **Skills** from the plugin appear alongside project-local ones.
- **Commands** the same.
- **Agents** (the `agents` array) merge additively into the agent-def
  catalogue — a plugin agent can't shadow a user's or project's
  existing agent with the same name. Useful for shipping a team of
  specialist agents (e.g. `reviewer`, `tester`, `architect`) as one
  install.
- **MCP servers** in the manifest are merged into `config.mcp_servers`
  — project-level `mcp.json` entries always win on name clash.

On collision the rule is consistent everywhere: **your** stuff beats
**plugin** stuff. Skills, commands, agents, and MCP servers you define
project- or user-locally always override a plugin's contribution with
the same name, so installing a plugin can't silently change a
convention you've already committed.

## Writing a plugin

Minimal example — a skill, a command, and an agent def:

```
my-plugin/
├── .thclaws-plugin/
│   └── plugin.json
├── skills/
│   └── hello/
│       └── SKILL.md
├── commands/
│   └── greet.md
└── agents/
    └── reviewer.md
```

```json
{
  "name": "my-plugin",
  "version": "0.1.0",
  "description": "Say hello in style + a reviewer agent",
  "skills": ["skills"],
  "commands": ["commands"],
  "agents": ["agents"]
}
```

Example `agents/reviewer.md`:

```markdown
---
name: reviewer
description: Read-only code review focused on naming + security
model: claude-haiku-4-5
tools: Read, Glob, Grep
permissionMode: auto
---

You are a code reviewer. Read the files you're pointed at. Flag
naming inconsistencies, missing tests, and security-sensitive
patterns. Don't propose fixes unless asked.
```

Zip it up:

```bash
cd my-plugin
zip -r ../my-plugin.zip .thclaws-plugin skills commands agents
```

Host the zip anywhere reachable over HTTPS (your CDN, S3, GitHub
Releases) and share the URL. Or push to a git repo — `/plugin install
<git-url>` works identically.

## Compared to skills + `/mcp add`

You can get equivalent functionality by installing each piece
separately. Plugins are about **atomicity**: one install, one uninstall,
one version to pin. Ship one for your team; everyone gets the same
set.

## Deferred features

Not yet supported but on the roadmap:

- **Hook merging** — manifest `hooks` block applied to the runtime
  hooks config ([ch 13](ch13-hooks.md)).
- **Marketplace** — `/plugin search`, `/plugin browse`.

For now, do these by hand via separate files.
