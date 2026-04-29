# Chapter 10 — Slash commands

Slash commands are the control plane. Type `/` followed by a name to
run a command instead of sending the line to the model. Type `/help`
any time to see the full list.

> **CLI and GUI are peers.** Every command in this chapter works
> identically from the CLI REPL, the GUI's Terminal tab, and the GUI's
> Chat tab — the `/<word>` input goes through the same dispatcher in
> all three surfaces. A few commands that mutate tool state
> (`/mcp add`, `/skill install`, `/plugin install`, `/kms use`) even
> activate their effects in the current session without a restart;
> the table notes which ones.

## Resolution order

When you type `/<word>`, thClaws resolves it in this order:

1. **Built-in command** — the table below.
2. **Installed skill** — rewrites the line into a `Skill(name: "word")`
   invocation ([Chapter 12](ch12-skills.md)).
3. **Legacy prompt command** — `.md` template from a `commands/`
   directory, with `$ARGUMENTS` substituted (this chapter).
4. **Unknown** — yellow error.

First match wins. Skills never shadow built-ins because built-ins are
tried first.

## Built-in command reference

### Session & model

| Command | What it does |
|---|---|
| `/help` | Show all built-in commands |
| `/model [NAME]` | Show current model, or switch to NAME (validated; typos revert) |
| `/models` | List available models from the current provider |
| `/provider [NAME]` | Show current provider, or switch |
| `/providers` | List every provider + its default model |
| `/save` | Force-save the current session to disk |
| `/load ID\|NAME` | Load a session by id, id-prefix, or title |
| `/sessions` | List saved sessions (newest first) |
| `/rename [NAME]` | Rename the current session (no arg clears the title) |
| `/resume ID\|NAME` | (CLI flag `--resume`) restart with a session loaded |
| `/clear` | Wipe in-memory history (doesn't touch saved files) |
| `/history` | Print a message-count summary |
| `/compact` | Summarise history to free tokens |
| `/cwd` | Show the working directory (sandbox root) |

### Memory & context

| Command | What it does |
|---|---|
| `/memory` | List memory entries |
| `/memory read NAME` | Print a memory entry |
| `/context` | Show the combined system prompt (project + agents + skills catalog) |

### Tools, skills, plugins, MCP

| Command | What it does |
|---|---|
| `/skills` | List loaded skills |
| `/skill show NAME` | Full description + path for a skill |
| `/skill marketplace [--refresh]` | Browse the catalog at thclaws.ai/api/marketplace.json |
| `/skill search QUERY` | Substring-search the marketplace catalog |
| `/skill info NAME` | Marketplace detail for one skill (license, source, install URL) |
| `/skill install [--user] <name-or-url> [name]` | Install a skill — bare slug → marketplace lookup, otherwise git or `.zip` URL |
| `/mcp marketplace [--refresh]` | Browse hosted + installable MCP servers in the catalog |
| `/mcp search QUERY` | Substring-search the MCP marketplace |
| `/mcp info NAME` | MCP marketplace detail (transport, command/url, license) |
| `/mcp install [--user] NAME` | Install a marketplace MCP — clones source if needed, writes mcp.json entry |
| `/plugin marketplace [--refresh]` | Browse the plugin catalog |
| `/plugin search QUERY` | Substring-search the plugin marketplace |
| `/plugin info NAME` | Marketplace detail for one plugin (use `/plugin show NAME` for installed) |
| `/<skill-name> [args]` | Invoke an installed skill directly |
| `/<command-name> [args]` | Invoke a legacy prompt command (template) |
| `/plugins` | List installed plugins (enabled + disabled) |
| `/plugin install [--user] <url>` | Install a plugin bundle |
| `/plugin remove [--user] <name>` | Uninstall a plugin |
| `/plugin enable [--user] <name>` | Enable a disabled plugin |
| `/plugin disable [--user] <name>` | Disable without uninstalling |
| `/plugin show <name>` | Manifest details |
| `/mcp` | List active MCP servers and their tools |
| `/mcp add [--user] <name> <url>` | Register a remote (HTTP) MCP server |
| `/mcp remove [--user] <name>` | Remove an MCP server from config |

### Knowledge bases (KMS)

| Command | What it does |
|---|---|
| `/kms` (or `/kms list`) | List every discoverable KMS; `*` marks ones attached to this project |
| `/kms new [--project] NAME` | Create a new KMS (default scope is user) |
| `/kms use NAME` | Attach a KMS to this project's chats |
| `/kms off NAME` | Detach a KMS |
| `/kms show NAME` | Print the KMS's `index.md` |

See [Chapter 9](ch09-knowledge-bases-kms.md) for the full KMS concept + workflow.

### Agent behaviour

| Command | What it does |
|---|---|
| `/permissions MODE` | Switch between `auto` and `ask` mid-session |
| `/thinking BUDGET` | Extended-thinking token budget (0 = off, only for Anthropic) |
| `/tasks` | List tasks / todos the agent has created |
| `/config key=val` | Override a config value for this session only |
| `/team` | Attach to the team tmux session (or show team status) |
| `/doctor` | Run diagnostic checks |
| `/usage` | Token usage by provider and model |
| `/version` | Show the thClaws version and commit SHA |
| `/quit` | Exit |

### Shell escape

| Command | What it does |
|---|---|
| `! <command>` | Run `<command>` in the terminal directly, bypassing the agent |

Useful for quick sanity checks (`! ls`, `! git status`) without spending
model tokens.

## Skill and command shortcuts

Any installed skill is callable as `/<skill-name>`:

```
❯ /skills
  docx — Create, read, edit Word documents
  pdf  — Read, split, merge, OCR PDFs
  …

❯ /pdf extract text from report.pdf
(/pdf → Skill(name: "pdf"))
Using the pdf skill to extract text from report.pdf…
```

Legacy prompt commands live as markdown files:

```markdown
# .thclaws/commands/review.md
---
description: Code review a branch
---
Review the diff from `main` to HEAD. Flag security issues, bad naming,
and missing tests. Focus on $ARGUMENTS.
```

```
❯ /review authentication
(/review → prompt from .thclaws/commands/review.md)
Reviewing the diff, focused on authentication…
```

`$ARGUMENTS` expands to whatever came after the command name. If the
template has no placeholder and the user typed args, they're appended
on a blank line.

## Writing your own slash commands

For quick one-liners, drop an `.md` file into `.thclaws/commands/`.
For anything with scripts or scaffolding, make it a **skill** ([Chapter 12](ch12-skills.md)).
For a whole bundle (skills + commands + MCP), ship it as a
**plugin** ([Chapter 16](ch16-plugins.md)).
