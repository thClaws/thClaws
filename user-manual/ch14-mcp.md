# Chapter 14 — MCP servers

MCP — **Model Context Protocol** — is Anthropic's open standard for
giving LLMs access to external tools. Any MCP server runs as a
subprocess (stdio) or listens over HTTP; thClaws discovers its tools
and registers them in the tool registry, namespaced by server name.

## Two transports

### Stdio (subprocess)

A local binary that speaks JSON-RPC on stdin/stdout. This is the most
common form — every `@modelcontextprotocol/server-*` NPM package works
this way.

```json
{
  "mcpServers": {
    "weather": {
      "command": "npx",
      "args": ["-y", "@h1deya/mcp-server-weather"]
    },
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "ghp_…" }
    }
  }
}
```

### HTTP Streamable (remote)

A server you reach over HTTPS. OAuth 2.1 with PKCE is handled
automatically for protected servers.

```json
{
  "mcpServers": {
    "agentic-cloud": {
      "transport": "http",
      "url": "https://api.agentic.cloud/mcp",
      "headers": { "Authorization": "Bearer …" }
    }
  }
}
```

For a Bearer-token server, set `headers` and you're done. For an
OAuth-protected server, leave `headers` out — on first use, thClaws
opens your browser for the OAuth flow and caches tokens at
`~/.config/thclaws/oauth_tokens.json`.

## Configuration files

thClaws reads MCP servers from (merged in order, project wins on name
clash):

1. Plugin manifests — `mcpServers` block
2. `~/.config/thclaws/mcp.json`
3. `~/.claude/mcp.json` (Claude Code compat)
4. `.thclaws/mcp.json` (project)
5. `.claude/mcp.json` (Claude Code compat)

## Adding a server at runtime

Instead of editing JSON by hand, use `/mcp add`:

```
❯ /mcp add weather https://mcp.weather-example.com/v1
[mcp-http] weather: probing with ping...
mcp 'weather' added (project, 2 tool(s)) → .thclaws/mcp.json
  - weather__get-forecast
  - weather__get-alerts
```

The server is persisted to `mcp.json`, connected, and its tools are
registered into the current session — no restart, works in the CLI
REPL and either GUI tab. `--user` writes to
`~/.config/thclaws/mcp.json` instead.

Remove:

```
❯ /mcp remove weather
mcp 'weather' removed from .thclaws/mcp.json (restart to drop active tools)
```

## Marketplace

`/mcp marketplace` browses curated MCP servers vetted by the thClaws
team. Same shape as the skill marketplace — three discovery commands
plus a name-based install:

```
❯ /mcp marketplace
MCP marketplace (baseline 2026-04-29, 1 server(s))
── data ──
  weather-mcp              — Global weather (current + forecast) via Open-Meteo
install with: /mcp install <name>   |   detail: /mcp info <name>
```

```
❯ /mcp info weather-mcp
name:         weather-mcp
description:  Global weather MCP server — current conditions and...
transport:    stdio
command:      python -m thclaws_weather
source:       https://github.com/thClaws/marketplace.git#main:mcp/weather-mcp
note:         Run `pip install -e <clone-path>` to install dependencies...
install with: /mcp install weather-mcp
```

```
❯ /mcp install --user weather-mcp
  cloned https://github.com/thClaws/marketplace.git → ~/.config/thclaws/mcp/weather-mcp
  registered 'weather-mcp' in ~/.config/thclaws/mcp.json (stdio transport)
  note: Run `pip install -e <clone-path>` …
```

For stdio entries with a `post_install_message`, follow that
instruction (typically `pip install` / `npm install` to fetch runtime
dependencies) before the server can start. After the install, restart
or reconnect to load the new tools.

For hosted entries (transport `sse`), no source clone happens — the
mcp.json entry just points at the hosted URL, and the agent connects
on next session start.

## Listing what's available

```
❯ /mcp
  weather (2 tool(s))
    - weather__get-forecast
    - weather__get-alerts
  github (20 tool(s))
    - github__list_issues
    - github__create_issue
    …
```

## Tool naming

All MCP tool names are prefixed with the server name + `__`:
`weather__get_forecast`, not `get_forecast`. This prevents collisions
(two servers can both have a `list` tool) and lets you deny a single
server's tools without touching others:

```json
{
  "permissions": {
    "deny": ["github__create_issue"]
  }
}
```

The double-underscore is compatible with every provider's tool-name
regex.

## Approvals

All MCP tools are **prompt-to-approve** — no way to mark them auto
except globally via `/permissions auto` or per-tool via `allow`.

## When things go wrong

### Stdio servers that fail to start

```
[mcp] weather … spawn failed: command not found: npx
```

Usually an npm / npx missing — install Node, or use a different
server. thClaws will keep running without that server's tools.

### HTTP servers returning 200 + error body

Some OpenAI-compat gateways wrap upstream errors in a single SSE data
frame with HTTP 200. thClaws's parser detects and surfaces these as
hard errors so they're not silent. If you see
`upstream error: …` in the tool output, the remote is misbehaving —
file a bug with the server's operator.

### OAuth flow issues

Tokens cached at `~/.config/thclaws/oauth_tokens.json` expire; thClaws
auto-refreshes using the refresh token. If refresh fails (server
rotated its keys), delete the entry for that server and reconnect to
trigger a fresh browser flow.

## Writing your own MCP server

Outside this manual's scope, but two starters:

- **TypeScript**: `@modelcontextprotocol/sdk` on npm.
- **Python**: `modelcontextprotocol` on PyPI.

Spec is at modelcontextprotocol.io. Once built, register via `/mcp
add` (HTTP) or by editing `mcp.json` (stdio).
