# Multi-tenant `--serve` (dev-plan/35 Tier 1)

`thclaws --serve --gui-shell <agent> --multi-tenant` hosts N end users on a single pod, each with their own SharedSessionHandle, session JSONL, gui-shell storage, sandbox boundary, and per-turn metering. Built for the consumer scale dev-plan/34 (thClaws.cloud) launches against — Tier 1 lands "few-thousand users per pod, sticky-routed" with local-disk state; Tier 2 swaps to object storage for stateless pods.

This doc covers what shipped in Tier 1, the HMAC handshake the routing layer must implement, the on-disk layout, the failure modes Tier 1 does NOT cover, and the test surface that pins the isolation guarantees.

**Source modules:**
- `crates/core/src/multi_tenant/auth.rs` — HMAC-SHA256 user-header verification + signing; `UserId` newtype with traversal-safe charset; 5-minute skew window.
- `crates/core/src/multi_tenant/registry.rs` — `UserSessionRegistry`: per-user `SharedSessionHandle` cache, double-check spawn under `RwLock`, LRU + idle-TTL eviction, background sweep task.
- `crates/core/src/multi_tenant/user_state.rs` — `UserStatePaths` (per-user disk layout) + `SessionRoots` (override forwarded to `spawn_with_roots`) + `is_in_user_writable` (sandbox check).
- `crates/core/src/multi_tenant/metering.rs` — `MeteringSink` trait + HTTP/stdout/noop impls, env-driven via `THCLAWS_METERING_ENDPOINT`.
- `crates/core/src/shared_session.rs` — `spawn_with_roots(approver, Option<SessionRoots>)` plumbs per-user paths into the worker; `WorkerState.session_roots` keeps the override sticky across `/new`, `/reload`, cwd swaps.
- `crates/core/src/server.rs` — `MultiTenantState`, `resolve_session_handle`, `verify_file_asset_for_user`, multi-tenant route arms.
- `crates/core/src/sandbox.rs::check_write_for_user` — wraps `check_in` with the per-user write boundary.
- `crates/core/src/gui_shell/storage.rs::{get_in, set_in, storage_path_in, load_all_in}` — override-rooted gui-shell KV writes.
- `crates/core/src/bin/app.rs` — `--multi-tenant`, `--multi-tenant-secret`, `--multi-tenant-max-users`, `--multi-tenant-idle-timeout`.

**Cross-references:**
- [`serve-mode.md`](serve-mode.md) — single-tenant `--serve`; this doc is the multi-user superset.
- [`sandbox.md`](sandbox.md) — `check_in` / `check_write_for_user` invariants the per-user boundary builds on.
- [`sessions.md`](sessions.md) — `SessionStore` + JSONL append model; multi-tenant routes each user's JSONL to their own subdir.
- dev-plan/35 (workspace-private) — full Tier 1/2/3 roadmap including this doc's "Tier 1 done means" acceptance.

---

## 1. Architecture

```
            ┌── User A browser ──┐   ┌── User B browser ──┐   ┌── User C browser ──┐
            │  React UI / shell  │   │                    │   │                    │
            └────────┬───────────┘   └────────┬───────────┘   └────────┬───────────┘
                     │ WS /t/<token>/ws       │                        │
                     │ + X-Thclaws-User/-Ts/-Proof (HMAC-SHA256)        │
                     └────────────┬───────────┴────────────────────────┘
                                  ↓
            ┌────────────────────────────────────────────────────────────────┐
            │  thClaws pod  (one process, --multi-tenant on)                  │
            │  ┌─ Axum  ──  resolve_session_handle(headers)                    │
            │  │            verify_user_header(secret) → UserId                │
            │  │            UserSessionRegistry.get_or_spawn(user_id)          │
            │  │              ↓                                                │
            │  │  ┌──── per-user SharedSessionHandle ──────┐  ┌─────────────┐  │
            │  │  │  Agent loop                            │  │  Same for B │  │
            │  │  │  Session JSONL → users/A/sessions/     │  │  Same for C │  │
            │  │  │  gui-shell storage → users/A/storage/  │  │  …          │  │
            │  │  │  UsageTracker → users/A/usage/         │  └─────────────┘  │
            │  │  │  Sandbox writes → users/A/* subtrees   │                   │
            │  │  └────────────────────────────────────────┘                   │
            │  └─ Background evictor: drop sessions idle > idle_timeout         │
            └────────────────────────────────────────────────────────────────┘
```

Per-user isolation is **in-memory** (separate worker thread, separate broadcast channel) **and on-disk** (separate JSONL dir, separate storage tree, separate usage subdir, sandbox writes confined to per-user subtree).

The pod itself is single-instance — Tier 1 binds users to whichever pod the cloud routes them to (sticky route's job). Cross-pod state portability is Tier 2.

---

## 2. CLI

```
thclaws --serve --gui-shell <agent>
        --multi-tenant
        [--multi-tenant-secret <secret>]
        [--multi-tenant-max-users <N>]
        [--multi-tenant-idle-timeout <duration>]
        [--port <N>] [--bind <addr>]
```

| Flag | Default | Notes |
|---|---|---|
| `--multi-tenant` | off | Enables HMAC user routing + per-user state. Single-tenant `--serve` (the default) is unchanged when this is absent. |
| `--multi-tenant-secret` | — | Shared secret for HMAC-SHA256 user-header verification. **Required** when `--multi-tenant` is set. Falls back to `THCLAWS_CLOUD_HMAC_SECRET` env if the flag is omitted. Process exits with a clear error if neither is set. |
| `--multi-tenant-max-users` | 1000 | LRU cap. Reaching the cap evicts the least-recently-active session before admitting a new one. |
| `--multi-tenant-idle-timeout` | 30m | Background evictor drops sessions idle past this. The 30s sweep interval is hard-coded for Tier 1 (re-tune if Tier 3 wants sub-minute sessions). |

**Boot log:** on successful start the pod prints

```
[serve] multi-tenant on — max_users=1000, idle_timeout=1800s
```

so deployments can grep for the line as a readiness check.

---

## 3. HMAC handshake (the routing-layer contract)

Every request to the multi-tenant pod must carry three headers signed with the shared secret. The pod's [`auth::verify_user_header`](../thclaws/crates/core/src/multi_tenant/auth.rs) checks them in this order:

| Header | Format | Purpose |
|---|---|---|
| `X-Thclaws-User` | `[a-zA-Z0-9_-]{1,64}` | Stable user id. Used as the on-disk subdir segment (`users/<id>/...`), so it must be filesystem-safe; the verifier rejects `/`, `\`, `..`, empty, and `> MAX_USER_ID_LEN`. |
| `X-Thclaws-User-Ts` | unix seconds (decimal) | Replay-window pin. Verifier rejects if `\|now - ts\| > 300s` (`MAX_TIMESTAMP_SKEW_SECS`). |
| `X-Thclaws-User-Proof` | hex(HMAC-SHA256(secret, "`<user>:<ts>`")) | Constant-time compared against the recomputed digest. |

### Signing recipe

```python
# routing layer (or test harness)
import hmac, hashlib, time
SECRET   = b"<shared with the pod>"
user_id  = "u_alice"
ts       = int(time.time())
message  = f"{user_id}:{ts}".encode()
proof    = hmac.new(SECRET, message, hashlib.sha256).hexdigest()
# attach: X-Thclaws-User: u_alice
#         X-Thclaws-User-Ts: 1781000000
#         X-Thclaws-User-Proof: 7af0b7...bed7
```

```bash
# debug from the shell — see "curl smoke" in §10 for the full set
proof=$(printf '%s' "u_alice:$(date +%s)" \
        | openssl dgst -sha256 -hmac "$SECRET" -hex \
        | awk '{print $2}')
```

### Verifier rejections (test surface)

| Condition | HTTP code | Test |
|---|---|---|
| Any of the three headers missing | `401` | `resolve_session_handle_multi_tenant_rejects_missing_headers` |
| Proof mismatch (constant-time compare fails) | `401` | `resolve_session_handle_multi_tenant_rejects_forged_proof`, `verify_file_asset_for_user_rejects_forged_hmac` |
| Timestamp skew > 300s (past OR future) | `401` | `rejects_stale_timestamp_past_skew_window`, `rejects_future_timestamp_past_skew_window` |
| User-id contains `/`, `\`, `..`, empty, > MAX_USER_ID_LEN | `401` | `rejects_traversal_in_user_id`, `rejects_user_id_too_long` |

Pod behaviour on rejection is deliberately quiet — `401` with no body — to avoid leaking which check failed to an attacker. The reason is logged at `eprintln!` for the operator.

---

## 4. On-disk layout

Per user `u`, under the serve project root:

```
<project>/
├── .thclaws/
│   └── users/
│       └── <u>/
│           ├── sessions/        ← SessionStore root (replaces `default_path()`)
│           │   └── sess-<id>.jsonl
│           ├── storage/         ← gui-shell `thclaws.storage` files
│           │   └── <shell_id>/<session_id>.json
│           ├── usage/           ← UsageTracker root
│           │   └── <provider>/<model>.json
│           └── grants.json      ← Tier 3 (path declared, write-path TBD)
└── output/
    └── users/
        └── <u>/                 ← agent-produced files (images, reports, downloads)
```

Computed by [`UserStatePaths::new(&project_root, &user_id)`](../thclaws/crates/core/src/multi_tenant/user_state.rs) once per user-spawn. The four subdirs are independent — Tier 2 will swap each backing store separately (sessions → S3, storage → Redis, usage → Postgres, output → S3+signed URLs) without changing the path templates a user-facing URL might reference.

### Why this layout

- **One prefix per user:** `tar czf user-snapshot.tgz .thclaws/users/<u>/ output/users/<u>/` captures everything for that user (dispute escalation, GDPR export, debugging).
- **URL matches disk:** the file-asset URL `/t/<token>/file-asset/users/<u>/...` is identical to the on-disk path, so sticky-routing keeps a user reading their own subtree.
- **Single sandbox check:** the per-user write boundary is exactly two directory prefixes (`output/users/<u>/` ∪ `.thclaws/users/<u>/`), so [`Sandbox::check_write_for_user`](../thclaws/crates/core/src/sandbox.rs) doesn't need cross-cutting allow-lists.

---

## 5. Session registry

[`UserSessionRegistry`](../thclaws/crates/core/src/multi_tenant/registry.rs) is the per-pod cache of `(user_id → Arc<UserSession>)`. Each `UserSession` owns one `SharedSessionHandle` and a `last_activity: Mutex<Instant>` for LRU bookkeeping.

### `get_or_spawn` — the spawn path

```rust
pub fn get_or_spawn(&self, user_id: &UserId) -> Arc<UserSession> {
    // 1. Fast path: read lock, hit → touch + clone Arc.
    // 2. Slow path: write lock + double-check (another thread may have
    //    spawned the same user between our read drop and write acquire).
    // 3. Cap enforcement: evict LRU if at capacity before inserting.
    // 4. Build UserStatePaths(project_root, user_id) → SessionRoots
    //    → spawn_with_roots(approver, Some(roots)).
    // 5. Insert and return.
}
```

The double-check is the actual concurrency-correctness surface. Without it, two threads racing on the same user-id could each pass step 2 with a `None` and each spawn a separate worker — wasted thread + state divergence. `concurrent_50_users_no_cross_leakage_or_double_spawn` pins this with 50 users × 4 racing threads = 200 spawns; every per-user group of 4 returns `Arc::ptr_eq`.

### LRU eviction

```rust
// state.sessions.iter().max_by_key(|(_, s)| s.idle_for())
// "LRU = longest idle = MAX idle_for (oldest last_activity)."
```

Easy bug to write as `min_by_key` — `idle_for` is "how long since touched", so "least recently used" wants the BIGGEST one. The test `capacity_triggers_lru_eviction` catches the inversion.

### Idle eviction (background sweep)

```rust
let evictor = registry.spawn_evictor(Duration::from_secs(30));
// ticker.tick().await every 30s; for each session where idle_for() > idle_timeout, remove.
```

Eviction drops the `Arc<UserSession>`, which drops the `SharedSessionHandle`, which closes the input channel, which exits the worker thread, which (via `Drop` on the in-flight Session) finalises the session JSONL. No explicit shutdown protocol needed — the borrow-chain does it.

### Acceptance tests

| Test | What it pins |
|---|---|
| `per_user_handles_carry_distinct_roots` | Each spawned handle's `session_roots` resolves to its own `users/<id>/` subtree. Without this, in-memory isolation is a fiction — both users would write to the same JSONL dir. |
| `restart_recovery_user_sees_prior_session_on_new_registry` | Drop registry+handle (= pod restart), build a fresh registry on the same `project_root`, alice reconnects, `SessionStore::list()` returns her prior JSONL, gui-shell storage round-trips the prior key. Closes dev-plan/35 Tier 1 "kill the pod, restart" acceptance. |
| `concurrent_50_users_no_cross_leakage_or_double_spawn` | 50 users × 4 racing threads = 200 spawns. Asserts (a) 50 distinct `UserSession`s, (b) 4 `Arc::ptr_eq` per user (no double-spawn), (c) 50 unique `sessions_dir`s. The closest practical check to the "50 concurrent users × 1 hour" acceptance — we can't soak for an hour in a unit test, but we CAN hammer the actual concurrency surface (RwLock + double-check) from many threads. |
| `capacity_triggers_lru_eviction`, `touch_updates_lru_so_active_user_survives` | LRU correctness — wrong direction (`min_by_key`) regresses immediately. |
| `evictor_sweeps_idle_sessions` | Background sweep actually drops idle sessions. |

---

## 6. File-asset HMAC + path-prefix gate

The file-asset route `/t/<token>/file-asset/<rel>` is the only URL that serves files from the per-user subtree to the browser. Under multi-tenant it MUST satisfy three checks before responding (in this order):

1. **Token gate** — same as single-tenant: URL token matches the shell's persisted token. Wrong token → 404 (route doesn't match).
2. **HMAC gate** — `verify_user_header(headers, secret)` resolves a `UserId`. Missing/forged/stale → 401.
3. **Path-prefix gate** — `rel` must start with `users/<user_id>/`. Cross-user subtree access (alice requesting `users/bob/...`) → 403.

```rust
// server.rs:739
let user_segment = format!("users/{}/", user_id.as_str());
if !rel.starts_with(&user_segment) {
    return Err(StatusCode::FORBIDDEN);
}
```

This is a **prefix** check, not a substring check — `users/alicebob/...` would still fail because `"users/alice/".starts_with("users/alicebob/")` is false (and vice-versa). Test: `verify_file_asset_for_user_rejects_shared_subtree`.

### Why both HMAC and path-prefix

The HMAC alone would let alice request `output/notes/shared.txt` (no `users/` segment) — anything outside any user's subtree. The path-prefix alone would let an unauthenticated request to `users/alice/...` succeed. Together: only the authenticated user can read their own subtree.

---

## 7. Metering sink

[`MeteringSink`](../thclaws/crates/core/src/multi_tenant/metering.rs) is the trait every per-turn cost/usage event passes through on its way to the cloud control plane:

```rust
pub trait MeteringSink: Send + Sync {
    fn record_message(&self, event: MessageEvent);
}
```

`MessageEvent` is camelCase-serialised so the routing layer can hand it straight to a JSON pipeline:

```json
{
  "userId": "u_alice",
  "agentId": "chatbot",
  "messageId": "msg-<uuid>",
  "tsUnixMs": 1781000000123,
  "providerCalls": [
    {"provider": "anthropic", "model": "claude-opus-4-7",
     "inputTokens": 1234, "outputTokens": 567, "cacheReadTokens": 0,
     "cacheWriteTokens": 0, "durationMs": 1820}
  ]
}
```

### Sinks

| Sink | Use case |
|---|---|
| `HttpMeteringSink` | POST each event to a configured URL. Set `THCLAWS_METERING_ENDPOINT=https://cp.thclaws.cloud/ingest/messages` at boot; `metering::from_env()` returns this when the env var is set. |
| `StdoutMeteringSink` | One JSON line per event on stdout. Dev / debugging. |
| `NoopMeteringSink` | Default when no env var is set — events are silently dropped. Keeps unit tests + local serve quiet. |

### What Tier 1 ships vs. what's still TODO

Shipped: trait, three impls, env-driven selection, per-user `usage_dir` aggregates written via `UsageTracker`.

**Not yet wired** in this commit: the actual call site emitting `MessageEvent` per turn from the multi-tenant agent loop. The `record()` aggregate side works; the discrete-event HTTP roundtrip is the remaining Tier 1 metering gap (see §11). Tracked for the next slice.

---

## 8. Sticky vs portable state

Tier 1 is **sticky-routed**: the cloud routes user X to pod_N, and X's state lives on pod_N's local disk. If a different request from X hits pod_M (because the router lost stickiness, the pod went away, or X reconnected mid-deploy), pod_M's registry sees no entry, spawns a fresh `SharedSessionHandle`, and that handle reads `<project>/.thclaws/users/X/sessions/` on pod_M — which is empty.

**Symptoms when stickiness breaks:**
- User's conversation history disappears on reconnect (new pod, no JSONL on disk).
- gui-shell storage values reset (same).
- A second concurrent connection from the same user-id to a different pod sees independent state — both pods think they're authoritative.

**Mitigations in Tier 1:**
- Cloud router MUST keep per-user stickiness via consistent hashing on `X-Thclaws-User` or session cookie.
- HPA wait-on-shutdown grace period: drain pod → users sticky-route to replacement → new pod's disk needs to have the user's subtree mounted (shared PV) OR the user accepts a fresh session.
- Tier 2 fixes by cold-start hydration from S3 — any pod can serve any user, state is portable.

Sticky routing is the cloud's problem; the pod assumes it's been delivered the right user.

---

## 9. What lives where

| Concern | File | Symbol |
|---|---|---|
| HMAC header verify | `multi_tenant/auth.rs` | `verify_user_header(user, ts, proof, secret, now_secs)` |
| HMAC header sign (tests / cloud router) | `multi_tenant/auth.rs` | `sign_user_header(user, ts, secret) -> String` |
| Per-user disk layout resolver | `multi_tenant/user_state.rs` | `UserStatePaths::new`, `.sessions_dir()`, `.storage_dir()`, `.usage_dir()`, `.writable_root()` |
| SessionRoots override | `multi_tenant/user_state.rs` | `SessionRoots { sessions_dir, storage_dir, usage_dir }`, `for_user_state(&paths)` |
| Per-user write-boundary check | `sandbox.rs` | `Sandbox::check_write_for_user(project_root, paths, path)` |
| Registry + LRU + idle eviction | `multi_tenant/registry.rs` | `UserSessionRegistry`, `RegistryConfig`, `UserSession`, `spawn_evictor` |
| SharedSessionHandle override threading | `shared_session.rs` | `spawn_with_roots(approver, Option<SessionRoots>)` |
| Worker sticky-state for the override | `shared_session.rs` | `WorkerState.session_roots` |
| gui-shell storage override | `gui_shell/storage.rs` | `get_in`, `set_in`, `storage_path_in`, `load_all_in` |
| MultiTenantState | `server.rs` | `MultiTenantState { registry, hmac_secret }` |
| HMAC + path-prefix gate | `server.rs` | `resolve_session_handle`, `verify_file_asset_for_user` |
| MeteringSink trait | `multi_tenant/metering.rs` | `MeteringSink`, `MessageEvent`, `ProviderCall`, `HttpMeteringSink`, `StdoutMeteringSink`, `NoopMeteringSink`, `from_env()` |
| CLI flags | `bin/app.rs` | `--multi-tenant{,-secret,-max-users,-idle-timeout}` |
| Boot wiring | `server.rs::run_with_engine` | `config.multi_tenant.as_ref().map(...)` |

---

## 10. Curl smoke (the operational quick-check)

This is the same five-row smoke that lives in dev-plan/35 Tier 1 close-out. Useful for verifying a fresh deploy or a routing-layer change without spinning up a real client.

```bash
cd <project>
thclaws --serve --gui-shell chatbot --port 5551 \
  --gui-shell-token testmttoken1234567890 \
  --multi-tenant --multi-tenant-secret testsecret &
PID=$!
sleep 2

SECRET=testsecret
BASE=http://127.0.0.1:5551/t/testmttoken1234567890
REL=output/users/alice/nonexistent.png
ASSET=$BASE/file-asset/$REL

NOW=$(date +%s)
STALE=$((NOW - 1000))
proof() {
  printf '%s' "alice:$1" \
    | openssl dgst -sha256 -hmac "$SECRET" -hex \
    | awk '{print $2}'
}

# 1. Missing headers → 401
curl -s -o /dev/null -w "no-headers : %{http_code}\n" "$ASSET"

# 2. Signed alice, file missing → 404 (auth passed, file genuinely absent)
curl -s -o /dev/null -w "signed-ok  : %{http_code}\n" \
  -H "X-Thclaws-User: alice" \
  -H "X-Thclaws-User-Ts: $NOW" \
  -H "X-Thclaws-User-Proof: $(proof $NOW)" \
  "$ASSET"

# 3. Forged proof → 401
curl -s -o /dev/null -w "forged     : %{http_code}\n" \
  -H "X-Thclaws-User: alice" \
  -H "X-Thclaws-User-Ts: $NOW" \
  -H "X-Thclaws-User-Proof: 00deadbeef00deadbeef00deadbeef00deadbeef00deadbeef00deadbeef0000" \
  "$ASSET"

# 4. Stale ts (skew > 300s) → 401
curl -s -o /dev/null -w "stale-ts   : %{http_code}\n" \
  -H "X-Thclaws-User: alice" \
  -H "X-Thclaws-User-Ts: $STALE" \
  -H "X-Thclaws-User-Proof: $(proof $STALE)" \
  "$ASSET"

# 5. Cross-user subtree → 403
curl -s -o /dev/null -w "cross-user : %{http_code}\n" \
  -H "X-Thclaws-User: alice" \
  -H "X-Thclaws-User-Ts: $NOW" \
  -H "X-Thclaws-User-Proof: $(proof $NOW)" \
  "$BASE/file-asset/output/users/bob/secret.png"

kill $PID
```

Expected output:

```
no-headers : 401
signed-ok  : 404
forged     : 401
stale-ts   : 401
cross-user : 403
```

---

## 11. Tier 1 explicitly does NOT include

Per dev-plan/35:

- **Object-storage state backend** — Tier 2. Today's state is local disk; pods are sticky-routed, not stateless.
- **Cross-pod state portability** — sticky-routing's job. Two pods serving the same user-id concurrently both think they're authoritative.
- **Resource limits (rlimit, cgroups)** — Tier 3. Bash still runs as the pod's UNIX user; no per-user CPU/RAM caps.
- **Bash subprocess pool** — Tier 3. A bash tool call on user A's behalf today shares process namespace with users B/C/D on the same pod.
- **Cross-agent state isolation** — out of scope (k8s namespace level).
- **Multi-tenant MCP** — out of scope. MCPs configured for the agent are shared across users; vet for statelessness before enabling user-callable MCPs that touch shared state.
- **End-to-end metering HTTP roundtrip** — the trait + impls + env-driven selection ship, the per-turn call site does not yet emit `MessageEvent` from the multi-tenant agent loop. Aggregates land in per-user `usage_dir`; the discrete-event HTTP path is the next slice.
- **50-user × 1-hour soak in CI** — practical proxy (`concurrent_50_users_no_cross_leakage_or_double_spawn`, 200 racing spawns) ships; the literal hour-long soak is a deployment-side validation, not a unit-test.

---

## 12. Known sharp edges

1. **HMAC secret rotation** is currently single-secret. Rotation drops every in-flight conversation. Future: accept N most-recent secrets in the verifier (Tier 1 follow-up, ~20 LoC).
2. **`User-Id` charset** is intentionally narrow (`[a-zA-Z0-9_-]`). Routing layers that use email-as-id MUST hash or encode first — feeding `@`/`.` through trips the traversal check by design.
3. **Sticky-route loss = session loss**, until Tier 2 lands. Document this in the routing layer's runbook.
4. **Sandbox boundary is the only Bash isolation in Tier 1.** A user who Bash-exfiltrates can't write outside their subtree, but `cat /etc/passwd` or `curl https://evil.example` still works. Tier 3 hardens with subprocess pools + cgroups.
5. **`gui-shell` storage path changed.** Single-tenant storage lands at `~/.config/thclaws/gui-shell/<id>/state/<sess>.json` (user-level, by design — survives uninstall). Multi-tenant lands at `<project>/.thclaws/users/<u>/storage/<shell>/<sess>.json` (project-level, per-user). A shell that hard-codes the single-tenant path assumption breaks; the canonical path is whatever `ctx.shared.session_roots.storage_dir` resolves to — use `gui_shell::storage::*_in` from native callers.
