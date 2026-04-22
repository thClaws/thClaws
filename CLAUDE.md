# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build order matters

The frontend must be built **before** the Rust crate when using the `gui` feature. [crates/core/src/gui.rs](crates/core/src/gui.rs) embeds the compiled webview via `include_str!("../../../frontend/dist/index.html")`, so if `frontend/dist/index.html` is missing, `cargo build --features gui` fails with a confusing include error rather than a missing-file message.

```sh
# Frontend first (produces single-file dist/index.html via vite-plugin-singlefile)
cd frontend && pnpm install && pnpm build && cd ..

# Then the Rust crate
cd crates/core && cargo build --features gui
```

There is no workspace `Cargo.toml` at the repo root — `cargo` commands run from [crates/core/](crates/core/).

## Common commands

All `cargo` commands assume `cd crates/core` (or `--manifest-path crates/core/Cargo.toml`).

| Task | Command |
|---|---|
| Build (with GUI) | `cargo build --features gui` |
| Run GUI | `cargo run --features gui --bin thclaws` |
| Run CLI REPL | `cargo run --bin thclaws -- --cli` (GUI feature optional) |
| One-shot prompt | `cargo run --bin thclaws -- -p "prompt"` |
| Full test suite | `cargo test --features gui` |
| Single test | `cargo test --features gui <test_name>` |
| Tests without keychain | `THCLAWS_DISABLE_KEYCHAIN=1 cargo test --features gui` (required on headless CI / sandboxed envs) |
| Format check | `cargo fmt --all -- --check` |
| Lint | `cargo clippy --features gui` (warnings are non-fatal in CI) |
| Frontend typecheck | `cd frontend && pnpm tsc --noEmit` |
| Frontend lint | `cd frontend && pnpm lint` |
| Frontend dev server | `cd frontend && pnpm dev` (only useful for isolated UI work — no backend IPC) |

There are two binaries defined in [crates/core/Cargo.toml](crates/core/Cargo.toml):
- `thclaws` ([src/bin/app.rs](crates/core/src/bin/app.rs)) — unified entry point; GUI by default, `--cli` for REPL, `-p` for print mode.
- `thclaws-cli` ([src/bin/cli.rs](crates/core/src/bin/cli.rs)) — CLI-only build (no GUI dependencies).

## CI constraints to remember

[.github/workflows/ci.yml](.github/workflows/ci.yml) runs tests on ubuntu + macOS only; **Windows is excluded** because several tests assume Unix path separators. Windows coverage comes from the release workflow building the binary. If you add tests that touch paths, use `Path::new` / `PathBuf` and avoid hardcoded `/` or `\`.

The CI `frontend` job uploads `frontend/dist/` as an artifact that the `clippy` and `test` jobs download — the Rust build depends on it existing.

## Architecture

### Three surfaces, one engine

The same [Agent](crates/core/src/agent.rs) loop + [Session](crates/core/src/session.rs) + [ToolRegistry](crates/core/src/tools/mod.rs) backs all three UX surfaces:

1. **Desktop GUI** ([gui.rs](crates/core/src/gui.rs), `gui` feature, wry + tao): embeds the React dist as a single HTML string via `include_str!`. Tabs (Terminal, Chat, Files, Team) are React views that talk to Rust via a `window.ipc` bridge.
2. **CLI REPL** ([repl.rs](crates/core/src/repl.rs)): interactive terminal via `rustyline`.
3. **Print mode**: `repl::run_print_mode` — single turn, no input loop.

Under the GUI, Terminal and Chat tabs **share one Agent and Session** via [shared_session.rs](crates/core/src/shared_session.rs). Input from either tab feeds the same history; output is broadcast as `ViewEvent`s that an event-translator thread fans out to both a chat-shaped (`chat_text_delta`, `chat_tool_call`, …) and terminal-shaped (`terminal_data` with base64 ANSI) frontend dispatch. When editing tab logic, remember that diverging state between tabs is a bug — they render the same conversation from different angles.

### Agent loop

[Agent::run_turn](crates/core/src/agent.rs) is a streaming state machine that yields `AgentEvent`s as it unfolds. Per iteration it:

1. Compacts history if over `budget_tokens` ([compaction.rs](crates/core/src/compaction.rs)).
2. Calls `provider.stream()` → [assemble.rs](crates/core/src/providers/assemble.rs) stitches chunked wire events into complete `ContentBlock`s.
3. Persists the assistant message.
4. Executes any `tool_use` blocks via the registry (with approval via [permissions.rs](crates/core/src/permissions.rs)), appends `tool_result` blocks, and loops.
5. Stops when no tool calls remain or `max_iterations` is hit.

`TOOL_RESULT_CONTEXT_LIMIT = 50_000` bytes — larger results are spilled to disk with a preview kept in context.

### Provider abstraction

[providers/mod.rs](crates/core/src/providers/mod.rs) defines `ProviderKind` as a closed enum. Every variant must have matching arms in every helper method — the compiler enforces exhaustiveness, so **adding a provider means updating every match**. Existing variants: Anthropic, AgentSdk (Anthropic Agent SDK subprocess), OpenAI, OpenAIResponses, OpenRouter, Gemini, Ollama, OllamaAnthropic, DashScope, AgenticPress.

Model prefix routing (e.g. `ollama/llama3.2`, `openrouter/anthropic/claude-…`) is resolved by the provider kind, not by configuration.

### Tools

[tools/mod.rs](crates/core/src/tools/mod.rs) defines the `Tool` trait: name, description, JSON input schema, async `call`, and `requires_approval` (default `false`). Mutating tools (bash, edit, write) override `requires_approval` to gate on `PermissionMode::Ask`. Read-only tools (read, ls, glob, grep) skip approval.

MCP-contributed tools ([mcp.rs](crates/core/src/mcp.rs)) register into the same registry with a `server.tool` name prefix.

### Settings layering

[config.rs](crates/core/src/config.rs) layers settings in this precedence order (higher wins): CLI flags → `.thclaws/settings.json` → `~/.config/thclaws/settings.json` → `~/.claude/settings.json` → compiled-in defaults. Keep this contract intact when adding new settings — project settings must be overridable by CLI, and user settings must fall through to the Claude Code fallback path so configs are portable.

API keys are **never** stored in `settings.json`; they live in the OS keychain ([secrets.rs](crates/core/src/secrets.rs)) with `.env` fallback via [dotenv.rs](crates/core/src/dotenv.rs).

### Default prompts

[default_prompts/](crates/core/src/default_prompts/) holds markdown files (`system.md`, `subagent.md`, `lead.md`, `compaction.md`, `compaction_system.md`, `agent_team.md`, `worktree.md`) compiled into the binary via `include_str!`. Edit these files directly rather than inlining strings — the embedding is driven from [prompts.rs](crates/core/src/prompts.rs).

### Build metadata

[build.rs](crates/core/build.rs) injects `THCLAWS_GIT_SHA`, `THCLAWS_GIT_BRANCH`, `THCLAWS_GIT_DIRTY`, `THCLAWS_BUILD_TIME`, `THCLAWS_BUILD_PROFILE` as `rustc-env` vars read by [version.rs](crates/core/src/version.rs). `git` missing at build time is tolerated (values become `"unknown"`).

## Frontend

React 19 + Vite 8 + Tailwind 4, bundled into a single HTML file via [vite-plugin-singlefile](https://github.com/richardtallent/vite-plugin-singlefile) (JS and CSS are inlined). Key pieces:

- [components/TerminalView.tsx](frontend/src/components/TerminalView.tsx): xterm.js terminal, subscribes to `terminal_data` broadcasts.
- [components/ChatView.tsx](frontend/src/components/ChatView.tsx): same conversation, rendered as chat bubbles; subscribes to `chat_*` broadcasts.
- [components/FilesView.tsx](frontend/src/components/FilesView.tsx): file browser with CodeMirror preview (syntax highlighting across ~40 languages) and TipTap markdown editing.
- [hooks/useIPC.ts](frontend/src/hooks/useIPC.ts): `send()` + `subscribe()` wrap `window.ipc.postMessage` and `window.__thclaws_dispatch` — the bridge wry exposes and our JSON protocol on top.

`THCLAWS_DEVTOOLS=1` opens the WebView devtools when running the GUI — use this to debug frontend issues rather than shipping console.log statements.

## Code style conventions (from [CONTRIBUTING.md](CONTRIBUTING.md))

- Default to **no comments**. Only comment when the *why* is non-obvious.
- Keep PRs small and scoped — don't refactor unrelated code in the same PR.
- Match surrounding style; the codebase has a consistent terse Rust style and the existing `//!` module docs are the primary architectural reference — read them before editing a module.
- Add tests alongside new features.

Commit style loosely follows Conventional Commits (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`) but is not strictly enforced — recent history shows short imperative subjects without prefixes is also accepted.
