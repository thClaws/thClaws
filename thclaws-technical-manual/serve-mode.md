# Serve mode

`thclaws --serve --port <N>` runs an Axum HTTP server with a WebSocket IPC bridge backing the same React frontend the desktop GUI uses. Project folder is the deploy unit — `cd <project> && thclaws --serve` is the entire deployment story per agent. Single-user; binds to `127.0.0.1` by default; SSH tunnel handles auth.

This doc covers the `--serve` runtime — the transport-agnostic IPC dispatch (`crate::ipc`), the always-on translator (`crate::event_render`), the Axum server (`crate::server`), the frontend WS transport in `useIPC.ts`, the deployment workflow, the security model, and the migration story for the handler arms still living in `gui.rs`.

**Source modules:**
- `crates/core/src/server.rs` — Axum routes (`GET /` serves embedded `index.html`, `GET /healthz`, `GET /ws`), per-connection `handle_socket` (subscribes to `events_tx`, runs translators, forwards via `mpsc → sink writer task`)
- `crates/core/src/ipc.rs` — `IpcContext` + `handle_ipc(msg, ctx)` transport-agnostic dispatch table
- `crates/core/src/event_render.rs` — `render_chat_dispatches`, `render_terminal_ansi`, `strip_ansi`, `TerminalRenderState`, envelope helpers shared by wry + WS transports
- `crates/core/src/bin/app.rs` — `--serve --port --bind` CLI flags
- `crates/core/src/gui.rs` — wry desktop transport (still hosts ~60 handler arms not yet migrated to `ipc::handle_ipc`)
- `frontend/src/hooks/useIPC.ts` — environment detection (`window.ipc` → wry, else WebSocket); auto-reconnect with exponential backoff
- `Makefile` — `make serve PORT=… BIND=…`

**Cross-references:**
- [`app-architecture.md`](app-architecture.md) — three-surface (GUI / CLI / serve) one-engine model
- [`agentic-loop.md`](agentic-loop.md) — same `Agent::run_turn` runs in serve mode
- [`sessions.md`](sessions.md), [`hooks.md`](hooks.md), [`agent-team.md`](agent-team.md), [`plan-mode.md`](plan-mode.md), [`kms.md`](kms.md), [`memory.md`](memory.md) — every project subsystem moves with the project folder; serve mode inherits them all transparently

---

## 1. Architecture

```
                    ┌─────────────────── Browser tab ─────┐                    ┌──── Browser tab ────┐
                    │  React UI (same bundle as desktop)   │                    │   React UI (same)   │
                    │  useIPC.ts (WebSocket branch)        │                    │   useIPC.ts         │
                    └─────────────┬────────────────────────┘                    └─────────────┬───────┘
                                  │ WebSocket /ws                                              │
                                  ↓                                                            ↓
                    ┌────────────────────────────────────────────────────────────────────────────────┐
                    │  Axum  ──  GET /            → frontend/dist/index.html (include_str!)           │
                    │       ──  GET /healthz      → "ok"                                              │
                    │       ──  GET /ws (upgrade) → handle_socket → IpcContext + handle_ipc           │
                    └────────────────────────────────────────────┬───────────────────────────────────┘
                                                                 │  ShellInput / ViewEvent
                                                                 ↓
                    ┌────────────────────────────────────────────────────────────────────────────────┐
                    │  shared_session::SharedSessionHandle (one per process — single-user)            │
                    │   • input_tx: mpsc<ShellInput>                                                  │
                    │   • events_tx: broadcast<ViewEvent>  (each WS connection subscribes)            │
                    │   • WorkerState in worker thread (unchanged; same as desktop)                   │
                    └────────────────────────────────────────────────────────────────────────────────┘
```

Multiple browser tabs are supported via the broadcast channel — same multi-tab consistency the desktop GUI already has between Terminal + Chat tabs.

## 2. CLI

```
thclaws --serve --port <N> --bind <ADDR>
```

| Flag | Default | Notes |
|---|---|---|
| `--serve` | off | Required to enter serve mode (otherwise `thclaws` opens the wry GUI by default) |
| `--port` | 8443 | TCP port |
| `--bind` | 127.0.0.1 | Bind address. `0.0.0.0` exposes publicly — only safe behind Tailscale / Cloudflare Access / reverse-proxy with auth |

`--serve` short-circuits the CLI/GUI dispatch in `bin/app.rs` — process runs the Axum server and blocks until SIGTERM / panic / shutdown.

`make serve PORT=8443 BIND=127.0.0.1` is the convenience target.

## 3. Trust model + auth

**Phase 1 single-user:**
- Default bind `127.0.0.1` only — anyone on the host with network access to the loopback can reach the engine, which means root + the running user. That's the trust boundary.
- Remote access via `ssh -L <port>:localhost:<port> user@server` — SSH handles auth + encryption.
- Anyone who reaches the bound socket has full agent access: BashTool runs as the server user, file tools touch the server filesystem, MCP servers spawn subprocesses, etc. Treat the tunnel as the auth boundary.
- `THCLAWS_DISABLE_KEYCHAIN=1` automatic — keychains absent on most Linux server boxes; users put API keys in `.thclaws/.env` (sandbox carve-out: agent can't write it; user creates it).

**Phase 2 (deferred):**
- Optional HTTP basic / OAuth / Tailscale-aware auth — design TBD. Today: defer to the network layer (SSH, Cloudflare Tunnel + Access, reverse proxy with auth).
- **Multi-tenant — shipped Tier 1 in dev-plan/35.** Use `thclaws --serve --gui-shell <agent> --multi-tenant --multi-tenant-secret <s>` to host N users on one pod with HMAC-signed `X-Thclaws-User` headers from the routing layer, per-user `SharedSessionHandle`s, per-user JSONLs / gui-shell storage / usage under `<project>/.thclaws/users/<id>/`, LRU + idle eviction, file-asset HMAC + path-prefix gate, and `MeteringSink` ingest. See [`multi-tenant-serve.md`](multi-tenant-serve.md) for the HMAC contract, on-disk layout, registry semantics, and curl smoke. The "per-user containers" path remains valid for stronger isolation (Tier 1 doesn't yet do per-user cgroups or bash subprocess pools — that's Tier 3).

## 4. The protocol

Same JSON message protocol as the desktop GUI's wry IPC. Frontend code (`useIPC.ts`) sends:

```json
{"type": "shell_input", "text": "/help"}
```

via `ws.send(JSON.stringify(msg))`. Server responds with one or more frames in the same shape:

```json
{"type": "chat_user_message", "text": "/help"}
{"type": "chat_slash_output", "text": "Shell escape:\n  !<command> ..."}
{"type": "chat_done"}
```

Both chat-shaped (`chat_*`) and terminal-shaped (`terminal_data`, `terminal_history_replaced`) envelopes flow on the same WS — `ChatView.tsx` consumes one shape, `TerminalView.tsx` the other; the broadcast goes to both.

### Special envelope: `ws_status`

Synthetic events the WebSocket transport in `useIPC.ts` dispatches locally (NOT sent over the wire) so any banner component can render reconnect UI:

```json
{"type": "ws_status", "status": "connecting" | "connected" | "disconnected"}
```

Fired on every state transition.

## 5. Reconnect (Phase 1A)

WebSocket drops trigger exponential backoff (250ms → 5s cap). On every reconnect:
1. WS opens
2. `useIPC.ts` auto-sends `{type: "frontend_ready"}`
3. Server's `frontend_ready` arm calls `on_send_initial_state` (today: stub — snapshot frame is the deferred SERVE9 work)
4. Live event stream resumes via `events_tx` subscription

**Snapshot frame is incomplete today.** Future SERVE9 work:
- Add `ShellInput::SnapshotRequest { reply: oneshot::Sender<SnapshotPayload> }`
- Worker handles by reading `state.session.id` + `agent.history_snapshot()` + `plan_state::get()` + `current_mode()` + reading `.thclaws/todos.md`, replies on the oneshot
- Server's WS-flavored `on_send_initial_state` spawns a tokio task: send `SnapshotRequest`, await reply, build snapshot JSON, dispatch as first frame
- Frontend replaces its in-memory state with the snapshot before resuming live deltas

Until that lands, every reconnect = blank slate locally; conversation history reloads via the `chat_history_replaced` envelope when the next user message triggers a worker turn.

## 6. Deploy workflow (the user's mental model)

```bash
# At home — desktop GUI as today, no change
cd ~/projects/foo && thclaws

# Going outside:
# 1. Sync project to cloud (any of these works)
rsync -av --exclude=target/ --exclude=node_modules/ \
        ~/projects/foo/ server:/srv/agents/foo/
# OR git push + git pull
# OR syncthing (auto)

# 2. Drop API keys (one-time, gitignored)
ssh server 'echo "ANTHROPIC_API_KEY=sk-..." >> /srv/agents/foo/.thclaws/.env'

# 3. Run on cloud
ssh server 'cd /srv/agents/foo && thclaws --serve --port 8443'

# 4. Access from anywhere (laptop / phone browser)
ssh -L 8443:localhost:8443 server &
open http://localhost:8443
```

Everything that lives in `.thclaws/` (sessions, plans, KMS, todos, agent defs, plugins, skills, hooks, team config) carries with the project. Conversations resume exactly where you left them on the desktop.

### Multiple projects per server

```ini
# /etc/systemd/system/thclaws@.service
[Service]
Type=simple
WorkingDirectory=/srv/agents/%i
EnvironmentFile=/srv/agents/%i/.thclaws/.env
ExecStart=/usr/local/bin/thclaws --serve --port-file %t/thclaws-%i.port
Restart=on-failure
```

`systemctl enable thclaws@personal-blog`, `systemctl enable thclaws@client-foo`, etc. Each unit is independent — own process, own `.thclaws/`, own MCP servers, own conversation history. Reverse proxy maps subdomains:

```
personal-blog.you.tld   → :8001
client-foo.you.tld      → :8002
```

## 7. What lives where

| Concern | File | Symbol |
|---|---|---|
| Axum server entry | `server.rs` | `pub async fn run(config: ServeConfig)` |
| ServeConfig defaults | `server.rs` | `ServeConfig::default` (port 8443, bind 127.0.0.1) |
| WS upgrade + per-connection handler | `server.rs` | `ws_handler`, `handle_socket` |
| Outbound event subscription + translator | `server.rs::handle_socket` | `event_forwarder` task |
| Inbound JSON → ipc::handle_ipc | `server.rs::handle_socket` | inbound reader loop |
| IpcContext schema | `ipc.rs` | `IpcContext`, `DispatchFn`, `QuitFn`, `SendInitialStateFn`, `ZoomFn`, `PendingAsks` |
| Dispatch table | `ipc.rs` | `pub fn handle_ipc(msg, ctx)` |
| ViewEvent → chat envelopes | `event_render.rs` | `render_chat_dispatches` |
| ViewEvent → terminal ANSI | `event_render.rs` | `render_terminal_ansi` + `TerminalRenderState` |
| Envelope wrappers | `event_render.rs` | `terminal_data_envelope`, `terminal_history_replaced_envelope` |
| ANSI strip helper | `event_render.rs` | `strip_ansi` |
| CLI flag wiring | `bin/app.rs` | `--serve --port --bind` short-circuit at top of `main()` |
| Frontend WS transport | `frontend/src/hooks/useIPC.ts` | environment detection + connectWs + reconnect backoff + ws_status emit |
| Wry desktop transport (still hosts ~60 unmigrated arms) | `gui.rs` | `with_ipc_handler` closure |
| Integration test | `server.rs::tests::ws_round_trip_processes_slash_command` | spins up server, connects via tokio-tungstenite, asserts canonical sequence |
| Makefile target | `/Makefile` | `serve` |

## 8. Migration state

The `gui.rs::with_ipc_handler` closure originally hosted ~70 message-type arms. M6.36 SERVE9 migrates them incrementally to `ipc::handle_ipc` so both transports share one dispatch table.

**Migrated as of M6.36 ship:**
- `app_close`, `shell_input` / `chat_prompt` / `pty_write` (text), `frontend_ready`, `approval_response`, `shell_cancel`, `new_session`

**Still in `gui.rs` (wry-only — invisible to serve mode):**
- File browser: `file_list`, `file_read`, `file_write`, `pick_directory`, `get_cwd`, `set_cwd`
- Settings panel: `api_key_set/clear/status`, `endpoint_set/clear/status`, `model_set`, `instructions_get/save`, `theme_get/set`, `dotenv`, `keychain`, `secrets_backend_get/set`, `gui_scale_get`, `gui_set_zoom`
- Plan sidebar: `plan_approve`, `plan_cancel`, `plan_retry_step`, `plan_skip_step`, `plan_stalled_continue`
- KMS sidebar: `kms_list`, `kms_new`, `kms_toggle`
- Team tab: `team_enabled_get/set`, `team_list`, `team_send_message`
- MCP-Apps: `mcp_call_tool`, plus the static-asset MIME-type arms (`png`, `pdf`, etc. — these are wry custom-protocol responses, not IPC)
- Misc: `slash_commands_list`, `request_all_models`, `clipboard_read/write`, `open_external`, `confirm`, `ask_user_response`, `sso_login/logout/status`, `config_poll`

Any frontend feature whose handler hasn't migrated still works in the desktop GUI but won't function in the webapp. Migration is incremental — each commit moves a few related arms with `cargo test` as the regression backstop. The integration test in `server.rs::tests` catches breaks in the inbound dispatch + outbound translation pipeline.

## 9. Testing surface

`server::tests`:
- `default_serve_config_binds_localhost` — pins the security-relevant default
- `ws_round_trip_processes_slash_command` — full integration test: bind OS port → spawn `server::run` → connect via `tokio-tungstenite` → send `frontend_ready` + `shell_input: "/help"` → assert `chat_user_message` + `chat_slash_output` + `chat_done` arrive

`ipc::tests`:
- `ipc_context_is_constructible_with_noop_transport` — pins the IpcContext type signature (Send + Sync invariants)
- `handle_ipc_ignores_unknown_type` — defensive default branch is forgiving

`event_render::ansi_strip_tests` + `event_render::chat_render_tests` — round-trip of the renderers (CSI / OSC strip, chat envelope shapes).

## 10. Known gaps

- **Snapshot frame for reconnect** — Phase 1A target; needs `ShellInput::SnapshotRequest` plumbing (described in §5)
- **Handler-arm migration long tail** — ~60 arms remaining (see §8)
- ~~HTTP `/upload`~~ shipped in v0.9.6 — see §10b below.
- **Multi-tab snapshot consistency** — when a 2nd tab opens, both tabs see live events but the 2nd tab gets no snapshot of state-so-far (paired with the snapshot-frame work)
- **Auth beyond SSH tunnel** — Phase 2 design; today defer to network layer (Tailscale / Cloudflare / reverse proxy)
- **Project-scoped memory** — `~/.config/thclaws/memory/` stays user-scoped; not an issue for the deploy story (those are personal facts, not project facts) but worth noting

The webapp is **useful today** for slash-command-driven workflows + chat. Rich features (file tab, team tab, settings, plan sidebar buttons, KMS sidebar) unlock as the handler-arm migration completes.

---

## 10b. File upload (`POST /upload`)

Multipart-form HTTP endpoint added alongside the WS in v0.9.6. Each part lands at `<workspace>/uploads/<name>`; `crate::uploads` owns the path-safety, collision-suffix, and size-cap logic.

```rust
// server.rs:191
.route("/upload", post(serve_upload))

// uploads.rs:28
pub const UPLOAD_MAX_BYTES: u64 = 25 * 1024 * 1024;  // 25 MB per file
pub const UPLOAD_MAX_FILES: usize = 5;               // per request
pub const UPLOADS_DIRNAME: &str = "uploads";

pub fn unique_path(dir: &Path, filename: &str) -> PathBuf;
```

**Request shape:** standard `multipart/form-data` body. Each part's `Content-Disposition` filename is sanitised (path-traversal segments stripped) before passing through `unique_path`, which appends `_n` before the extension on collision (`notes.md` → `notes_1.md` → `notes_2.md`).

**Response:** JSON describing the saved files:

```json
{
  "uploads": [
    {"original": "screenshot.png", "saved": "screenshot.png", "size": 184231},
    {"original": "notes.md",       "saved": "notes_1.md",     "size": 412}
  ]
}
```

**Synthetic chat message:** after the response, the server calls `render_upload_message("serve", &saved)` and pushes the rendered text through the same `ShellInput::ChatText` pipeline the WS uses. The agent sees an indistinguishable "user said X" event — including the file paths it can now `Read` — without the WS having to carry the file bytes.

**Wire-format keys:**

- **Field name:** any. The handler iterates `multipart.next_field()` and uses the part's filename, not the field name.
- **Workspace target:** `IpcContext.workspace` (set at server boot from `--serve`'s cwd).
- **No auth beyond SSH-tunnel posture:** same trust model as the WS — Phase 1 expects localhost-only or reverse-proxy fronting.

**Test (`server::tests::upload_post_saves_to_workspace_uploads_dir`):** boots a server against a `tempdir` workspace, POSTs two parts with the same filename, asserts both land + the second carries the `_1` suffix.
