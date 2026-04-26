# thClaws Frontend

This directory contains the React frontend for the thClaws desktop GUI. It is
not a standalone web app in production: the Vite build is emitted as a
single-file bundle, then embedded into the Rust GUI binary and served inside a
`wry` webview.

## How It Fits Together

- React + TypeScript renders the desktop interface: Chat, Terminal, Files,
  Settings, and Team views.
- Vite builds `frontend/dist/index.html` with JavaScript and CSS inlined via
  `vite-plugin-singlefile`.
- The Rust GUI embeds that file at compile time and serves it through a custom
  `thclaws://` protocol.
- Both the Chat and Terminal tabs talk to the same Rust-side agent session, so
  they share conversation history, model state, tools, and saved sessions.

## Important Files

- `src/App.tsx` — main app shell, startup flow, tab layout, settings modals,
  and working-directory selection.
- `src/components/` — UI surfaces for chat, terminal, file browser/editor,
  settings, approvals, sidebar, and team panes.
- `src/hooks/useIPC.ts` — JSON message bridge between React and the Rust
  backend.
- `src/index.css` — global styles, Tailwind import, theme variables, and shared
  UI styling.
- `vite.config.ts` — Vite config, including React, Tailwind, and single-file
  bundling.

## IPC Contract

The frontend and Rust backend communicate with small JSON messages.

React sends messages to Rust through:

```ts
window.ipc.postMessage(JSON.stringify({ type: "chat_prompt", text }));
```

Rust sends messages back by evaluating:

```ts
window.__thclaws_dispatch(JSON.stringify({ type: "chat_text_delta", text }));
```

React components subscribe through `src/hooks/useIPC.ts`, which parses the
incoming JSON and fans it out to local handlers.

## Development Commands

Install dependencies:

```sh
pnpm install
```

Run the Vite dev server:

```sh
pnpm dev
```

Build the production bundle:

```sh
pnpm build
```

Lint the frontend:

```sh
pnpm lint
```

Preview the built bundle:

```sh
pnpm preview
```

## Building the Desktop GUI

The Rust GUI expects `frontend/dist/index.html` to exist because it is embedded
with `include_str!` during compilation. Build the frontend before compiling the
GUI binary:

```sh
cd frontend
pnpm install
pnpm build
cd ..
cargo build --features gui --bin thclaws
```

For local GUI runs, use the same ordering:

```sh
cd frontend && pnpm build && cd ..
cargo run --features gui --bin thclaws
```
