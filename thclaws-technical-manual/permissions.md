# Permissions

Three layered gates for tool execution: a per-call **approval gate** (Auto/Ask/Plan modes routed through pluggable `ApprovalSink` implementations), a **filesystem sandbox** that restricts file tools to the project root, and an **EE policy layer** (Ed25519-signed org policy with host allowlist, script restrictions, gateway routing). On top: hard-coded **bash forbidden command lists** for lead/teammate processes, an **MCP stdio per-command allowlist** that prompts the user the first time it sees a new spawn, and a **subagent approval propagation** path so child agents inherit the parent's gate instead of silently auto-approving.

The agent loop consults the approval gate at tool dispatch time. The sandbox is consulted by file tools (Read/Write/Edit/Ls/Glob/Grep/Bash cwd) before every operation. The policy layer is consulted at install time (skills, MCP) and at config-load time (HTTP MCP filtering). All three layers fail closed independently — bypassing the approval gate doesn't bypass the sandbox, and a permissive sandbox doesn't override an org policy that disallows the URL.

This doc covers: the three permission modes, every `ApprovalSink` impl + the `GuiApprover`/`ReplApprover` IPC bridges, the dispatch gate at `agent.rs::run_turn`, plan-mode block + approval-window gate, MCP-Apps widget approval routing, the sandbox path-validation algorithm, the org policy file format + signature verification + host allowlist matching, MCP stdio per-command allowlist, bash lead/teammate forbidden command lists, the M6.20 subagent factory propagation fix, and session-swap state hygiene.

**Source modules:**
- `crates/core/src/permissions.rs` — `PermissionMode`, `ApprovalSink` trait, `AutoApprover`/`DenyApprover`/`ScriptedApprover`/`ReplApprover`/`GuiApprover`, global mode slot + pre-plan stash + broadcaster
- `crates/core/src/policy/mod.rs` — `Policy` schema, `ActivePolicy`, `find_file`, `load_or_refuse`, `external_scripts_disallowed`, `external_mcp_disallowed`, fingerprint matching, ISO-8601 expiry
- `crates/core/src/policy/verify.rs` — `KeySource::resolve`, `verify_policy`, canonical JSON encoder
- `crates/core/src/policy/allowlist.rs` — `check_url`, `normalize_url_for_match`, `matches_pattern`, host glob + path-segment matching
- `crates/core/src/policy/error.rs` — `PolicyError` variants (all fail-closed)
- `crates/core/src/sandbox.rs` — `Sandbox::init`, `check`, `check_write`, lexical normalize + canonicalize
- `crates/core/src/agent.rs` — dispatch gate at `run_turn` (plan-mode block, approval-window gate, approval gate)
- `crates/core/src/shared_session.rs` — `WorkerState.approver`, mode-changed broadcaster, MCP-Apps widget dispatch, session-swap reset (M2/M3)
- `crates/core/src/repl.rs` — `ReplAgentFactory` (M6.20 H1 propagation), CLI `/permissions` / `/plan` slashes, session-swap reset
- `crates/core/src/gui.rs` — approval IPC handlers (`approval_response`, `plan_approve`, `plan_cancel`)
- `crates/core/src/tools/plan.rs` — `EnterPlanModeTool` / `ExitPlanModeTool` mode flips
- `crates/core/src/tools/plan_state.rs` — plan-completion auto-restore
- `crates/core/src/tools/bash.rs` — `lead_forbidden_command`, `teammate_forbidden_command`, `is_destructive_command`
- `crates/core/src/mcp.rs` — `check_stdio_command_allowed`, `McpAllowlist` persistent storage
- `crates/core/src/skills.rs` — `enforce_scripts_policy` install + load gates
- `crates/core/src/config.rs` — HTTP MCP filtering against policy host allowlist

**Cross-references:**
- [`agentic-loop.md`](agentic-loop.md) — the dispatch site at `agent.rs::run_turn` is where every approval check fires
- [`sessions.md`](sessions.md) — session swap triggers the approval-state reset (M2 + M3)
- [`plugins.md`](plugins.md), [`skills.md`](skills.md) — install paths consult policy.check_url + enforce_scripts_policy

---

## 1. Overview

```
TOOL DISPATCH SITE (agent.rs::run_turn, per tool_use in turn)
                    │
                    ├── Layer 1: parse-error short-circuit  (synthetic ToolResult)
                    │
                    ├── Layer 2: tool lookup                (unknown tool → ToolResult)
                    │
                    ├── Layer 3: permission_mode resolve    (current_mode() + fallback)
                    │
                    ├── Layer 4: TodoWrite plan-mode block  (M6.20 BUG M1 — fires first)
                    │
                    ├── Layer 5: generic plan-mode block    (mutating tools blocked in Plan)
                    │
                    ├── Layer 6: approval-window gate       (no UpdatePlanStep/ExitPlanMode
                    │                                         while plan is awaiting approval)
                    │
                    ├── Layer 7: approval gate              (Ask + requires_approval → approver)
                    │
                    └── Layer 8: tool.call_multimodal       (now actually runs)
                            │
                            └── Layer 9: tool-internal      (Sandbox::check / check_write,
                                                             bash lead/teammate forbidden lists,
                                                             policy::check_url at install)
```

```
WORKER SPAWN
   ├── Sandbox::init                        (canonicalize CWD or $THCLAWS_PROJECT_ROOT)
   ├── policy::load_or_refuse               (find file → verify sig → check expiry/binding
   │                                          → ACTIVE: OnceLock<ActivePolicy>)
   ├── ApprovalSink construction            (GuiApprover with IPC channels, or ReplApprover)
   ├── set_current_mode(agent.permission_mode)
   └── set_mode_broadcaster(events_tx)      (every set_current_mode_and_broadcast →
                                             ViewEvent::PermissionModeChanged)
```

### Why three independent layers?

Each layer answers a different question:
- **Approval mode** — "should this tool execution happen at all right now?"
- **Sandbox** — "is this path inside the project the user opened?"
- **Policy** — "did the org admin authorize this MCP / skill source?"

Bypassing one doesn't bypass the others. A user in `permission_mode = "auto"` (no prompts) still can't `Write("/etc/passwd")` because the sandbox catches it. A user with a permissive sandbox can't install a skill from `evil.com` if the org policy says `allowed_hosts: ["github.com/acme/*"]`. An org with a strict policy can't override the user's "ask before bash" choice — the approval gate stays consulted.

---

## 2. `PermissionMode`

Three values, lowercase serde:

```rust
pub enum PermissionMode {
    Auto,    // Never prompt
    Ask,     // Prompt on tools whose requires_approval returns true
    Plan,    // Block mutating tools entirely; only Read/Grep/Glob/Ls + plan tools work
}
```

`Default::default() == Ask`. Set at worker spawn from project `.thclaws/settings.json` (`permissions: "auto"` or `"ask"`); flipped at runtime by `EnterPlanMode` / `ExitPlanMode` / `/plan` slash / sidebar Approve / Cancel / `/permissions` slash.

The mode lives in a process-global `Mutex<PermissionMode>` (`current_mode_slot` at permissions.rs:53) — read at every dispatch gate so flips take effect on the very next tool call (not on the next user message). Reads are dynamic (cheap Mutex lock), writes are explicit through `set_current_mode` / `set_current_mode_and_broadcast`.

### `pre_plan_mode` stash

`Mutex<Option<PermissionMode>>`. Holds whichever mode was active before `EnterPlanMode` / sidebar-`/plan` flipped us into Plan. `ExitPlanMode` / sidebar Cancel / plan-completion auto-restore pops it so `Ask → Plan → Ask` instead of `Ask → Plan → Auto`. `None` while no plan-mode session is active. Single-slot — re-entering Plan from Plan is a no-op for the stash (current code guards via `if !matches!(prior, Plan)`).

### Mode broadcaster

The GUI worker registers a closure (shared_session.rs:828) that runs on every `set_current_mode_and_broadcast` call:
```rust
crate::permissions::set_mode_broadcaster(move |mode| {
    let _ = mode_tx.send(ViewEvent::PermissionModeChanged(mode));
});
```

So the sidebar's permission pill reflects the change live without polling. `set_current_mode` (the non-broadcasting variant) is used at worker init to seed the global without firing a redundant ViewEvent before the broadcaster is wired.

### Dispatch fallback (the M6.20 BUG H1 root)

`agent.rs:1110-1121`:
```rust
let permission_mode = {
    let m = crate::permissions::current_mode();
    if matches!(m, PermissionMode::Ask) && permission_mode_default == PermissionMode::Auto {
        permission_mode_default
    } else { m }
};
```

The fallback exists for "worker startup before init" — when the global is still its `Default::default() = Ask` but the agent was constructed with `permission_mode = Auto` for one-shot CLI runs. Without the fallback, those one-shots would prompt on the bare-default Ask.

Side effect (BUG H1, fixed): if a child agent (subagent built via `Task` factory) defaults to `Auto` and the parent worker set the global to `Ask`, the child triggers the fallback → `permission_mode = Auto` → no approval. M6.20's fix propagates the parent's `permission_mode` onto the child explicitly (see §16).

---

## 3. `ApprovalSink` trait

```rust
#[async_trait]
pub trait ApprovalSink: Send + Sync {
    async fn approve(&self, req: &ApprovalRequest) -> ApprovalDecision;

    /// Reset any "allow for session" state held by this sink.
    /// Default no-op for sinks without session-scoped state.
    fn reset_session_flag(&self) {}
}
```

`ApprovalRequest`:
```rust
pub struct ApprovalRequest {
    pub tool_name: String,
    pub input: Value,
    pub summary: Option<String>,    // Optional preview line for the UI
}
```

`ApprovalDecision`:
- `Allow` — approve this one call
- `AllowForSession` — approve + flip a session-scoped flag so subsequent calls auto-approve
- `Deny` — refuse; agent emits a ToolResult with `is_error: true` and `denied by user: <name>`

### Built-in implementations

| Sink | Use | Behavior |
|---|---|---|
| `AutoApprover` | `PermissionMode::Auto` default sink + tests | Always `Allow` |
| `DenyApprover` | Tests | Always `Deny` |
| `ScriptedApprover` | Integration tests | Plays back a `VecDeque<ApprovalDecision>`; defaults to `Deny` when exhausted; `AllowForSession` flips `session_allowed: AtomicBool` for subsequent calls |
| `ReplApprover` | CLI REPL | Prints prompt to stdout, reads stdin via `tokio::task::spawn_blocking`. Accepts `y`/`yes`/`n`/`no`/`yolo`. `yolo` flips `session_allowed`. M6.20 BUG M2: `reset_session_flag()` clears the flag on session swap. |
| `GuiApprover` | GUI mode | Bridges async `approve()` to the webview via `mpsc<GuiApprovalRequest>` + `oneshot<ApprovalDecision>` per request. |

### `GuiApprover` design

```rust
pub struct GuiApprover {
    tx: mpsc::UnboundedSender<GuiApprovalRequest>,           // outbound to GUI event loop
    pending: Mutex<HashMap<u64, oneshot::Sender<ApprovalDecision>>>,  // id → responder
    unresolved: Mutex<HashMap<u64, GuiApprovalRequest>>,     // id → request (for redispatch)
    next_id: AtomicU64,
    session_allowed: AtomicBool,
}
```

`approve()` flow:
1. Check `session_allowed` — if true, return `Allow` without involving the user.
2. Mint a fresh request id (`fetch_add`).
3. Create `oneshot<ApprovalDecision>`, store responder in `pending[id]`.
4. Store the request in `unresolved[id]` so the GUI forwarder can re-dispatch on a timer (handles the case where the webview hadn't mounted React yet when the dispatch fired).
5. Send the request over the mpsc; if the channel is closed, clean up both maps and return `Deny`.
6. Await `resp_rx`. On `AllowForSession`, flip the flag and return `Allow`. On any error, return `Deny`.

`resolve(id, decision)` — called from the GUI IPC handler when the user clicks a button:
1. Remove from `unresolved`.
2. Remove from `pending` and send the decision down the responder.

`unresolved_requests()` — snapshot Vec returned to the GUI forwarder for periodic redispatch. Avoids needing a "frontend ready" handshake; the user clicking Allow eventually arrives and pairs with the still-waiting responder.

**Known limitations:**
- BUG L1 (deferred): cancellation doesn't unblock `resp_rx.await`. If the user clicks Cancel via `shell_cancel` then closes the modal without responding, the agent hangs.
- BUG L2 (deferred): if the consumer drops the agent stream mid-approval, the entry stays in `pending`/`unresolved` until process exit. Tiny memory leak + orphan-modal redispatch.

---

## 4. Per-tool `requires_approval` matrix

Default for any tool that doesn't override is `false` (read-only).

| Tool | requires_approval | Notes |
|---|---|---|
| Read / Ls / Glob / Grep | `false` | Read-only filesystem. |
| AskUser | `false` | Prompts the user — not a side effect. |
| EnterPlanMode / ExitPlanMode / SubmitPlan / UpdatePlanStep | `false` | Plan-control tools must sail through the dispatch gate. |
| Bash | `true` (always) | Plus `is_destructive_command` for stderr ⚠ marker (no extra block). |
| Write / Edit | `true` | Filesystem mutations. |
| DocxCreate / DocxEdit / XlsxCreate / XlsxEdit / PptxCreate / PptxEdit / PdfCreate / DocConvert | `true` | Office doc mutations (DocConvert also uploads to the document service). |
| WebFetch / WebSearch | `true` | Outbound HTTP — leaks the query/URL. |
| TodoWrite | `true` | Triggers M6.20 BUG M1 fix path: TodoWrite-specific plan-mode message fires before generic. |
| MCP tools (any) | `true` | MCP tool semantics aren't introspectable; conservative default until per-tool annotation lands. |
| Task (SubAgentTool) | `true` | Spawning a sub-agent is itself a sensitive action. |
| Team: SpawnTeammate / TeamCreate / TeamMerge | `true` | Subprocess spawn / team config / git merge. |
| Team: SendMessage / CheckInbox / TeamStatus / TeamTaskCreate / TeamTaskList / TeamTaskClaim / TeamTaskComplete | `false` | Internal coordination — lead is autonomous in team mode. |
| Skill / SkillList / SkillSearch | `false` | Read-only catalog inspection. The actual skill body invokes Bash/Edit etc. which gate normally. |

Read-only Office tools (DocxRead/XlsxRead/PptxRead/PdfRead) inherit the trait default (`false`).

---

## 5. Agent dispatch gate

`agent.rs::run_turn` per `tool_use` block (lines ~1060-1314). The order of layers is significant — the M6.20 BUG M1 fix reordered TodoWrite ahead of the generic block. Current order:

```rust
for tu in &turn_tool_uses {
    let ContentBlock::ToolUse { id, name, input } = tu else { continue };

    // Layer 1: parse-error short-circuit (M6.17 L4)
    if let Some((_, err)) = turn_parse_errors.iter().find(|(eid, _)| eid == id) {
        emit synthetic ToolResult(err);  continue;
    }

    // Layer 2: tool lookup
    let tool = match tools.get(name) {
        Some(t) => t,
        None => emit "unknown tool"; continue,
    };

    // Layer 3: permission_mode resolve (with M6.20 BUG H1-aware fallback)
    let permission_mode = {
        let m = current_mode();
        if matches!(m, Ask) && permission_mode_default == Auto { permission_mode_default }
        else { m }
    };

    // Layer 4: TodoWrite plan-mode block (M6.20 BUG M1 — fires first)
    if matches!(permission_mode, Plan) && name == "TodoWrite" {
        emit "use SubmitPlan instead";  continue;
    }

    // Layer 5: generic plan-mode block
    if matches!(permission_mode, Plan) && tool.requires_approval(input) {
        emit "use Read/Grep/Glob/Ls; SubmitPlan when ready";  continue;
    }

    // Layer 6: approval-window gate
    if matches!(permission_mode, Plan)
        && (name == "UpdatePlanStep" || name == "ExitPlanMode")
        && plan_state::get().is_some()
    {
        emit "wait for sidebar Approve/Cancel button";  continue;
    }

    // Layer 7: approval gate
    let needs_approval = matches!(permission_mode, Ask) && tool.requires_approval(input);
    if needs_approval {
        let decision = approver.approve(&req).await;
        if matches!(decision, Deny) {
            emit ToolCallDenied; continue;
        }
    }

    // Layer 8: tool.call_multimodal
    let tool_result = tool.call_multimodal(input.clone()).await;
    ...
}
```

### Approval-window gate (Layer 6)

While a plan is submitted-but-not-approved (`plan_state::get().is_some()` AND mode == Plan), the model must NOT progress steps (UpdatePlanStep) or unilaterally exit plan mode (ExitPlanMode). Both bypass the user's review window — the model could call ExitPlanMode interpreting a casual "Start" as approval, flip mode to Auto on its own, and start writing files before the user has reviewed the plan.

The sole legal path out of "plan submitted, awaiting approval" is the user clicking the sidebar Approve / Cancel button (which fire `plan_approve` / `plan_cancel` IPCs from the GUI). Re-submitting via SubmitPlan stays allowed — that's the model's "I changed my mind" channel and the new plan also waits for approval.

### What read-only tools get in plan mode

Tools with `requires_approval == false` (Read/Grep/Glob/Ls, plan tools, AskUser) sail through Layers 4-7. They're the only tools the model can call in Plan mode. The structured "Blocked" tool_results from Layers 4-5 read the model into the right next-turn behavior.

---

## 6. MCP-Apps widget dispatch

When a trusted MCP-Apps widget calls `app.callServerTool` (postMessage from the iframe), the request lands in `shared_session.rs::McpAppCallTool` handler — NOT through the agent loop. So the agent's dispatch gate doesn't fire. The handler runs its own approval check (M6.15 BUG 2 fix):

```rust
let mode = current_mode();
let needs_approval = matches!(mode, Ask | Plan) && t.requires_approval(&arguments);
if needs_approval {
    let req = ApprovalRequest { ... };
    if matches!(state.approver.approve(&req).await, Deny) {
        return error;
    }
}
t.call_multimodal(arguments).await
```

Differences from the agent loop:
- **No plan-mode structural block** — Plan triggers approval (same as Ask), not a hard refusal. Defensible UX argument: the user clicked something inside the widget, so they implicitly intended action. **BUG M4 (deferred):** inconsistent with the agent loop which structurally blocks the same call. Needs UX call.
- **No approval-window gate** — widget calls aren't UpdatePlanStep / ExitPlanMode candidates anyway.

The trust gate on widget HTML rendering is separate (see [`mcp.md`](mcp.md) — only servers marked `trusted: true` in mcp.json get to render iframe HTML; this gate runs at `fetch_ui_resource` time, not at tool dispatch).

---

## 7. Sandbox

`Sandbox` = a process-global `RwLock<Option<PathBuf>>` holding the canonicalized project root. Initialized at startup via `Sandbox::init()`:

```rust
let root_path = match std::env::var("THCLAWS_PROJECT_ROOT") {
    Ok(s) if !s.is_empty() => PathBuf::from(s),
    _ => std::env::current_dir()?,
};
let root = root_path.canonicalize()?;
*SANDBOX_ROOT.write().unwrap() = Some(root);
```

`$THCLAWS_PROJECT_ROOT` is exported by `SpawnTeammate` so teammate processes spawned in `.worktrees/<name>/` still treat the parent project root as their writable region (matching Claude Code's `getOriginalCwd()` model). Without this override, a worktree teammate's sandbox would shrink to its worktree and shared artifacts at the project root would be denied.

### `Sandbox::check(path)`

For Read/Ls/Glob/Grep/DocxRead/XlsxRead/PptxRead/PdfRead. Returns the canonical absolute path inside the sandbox, or an error.

```rust
pub fn check(path: &str) -> Result<PathBuf> {
    let Some(root) = Self::root() else {
        // No sandbox initialized — allow everything (tests / standalone)
        return ...;
    };
    let cwd = std::env::current_dir()?;
    Self::validate_against(&root, &cwd, path)
}
```

`validate_against` algorithm:
1. Resolve relative paths against `cwd` (NOT root) — teammate writes from `.worktrees/backend/src/foo` must land in the worktree, not workspace root.
2. **Lexical normalize** `..` and `.` BEFORE canonicalization. Required because the parent-walk below checks each *existing* ancestor for containment, but `cwd/../outside.txt` has `cwd` as its parent (which is inside the sandbox) yet points outside.
3. If the path exists, `canonicalize()` it (follows symlinks, catches escape via `/tmp` symlink) and check `starts_with(root)`.
4. If the path doesn't exist (Write to a deep new tree like `src/api/handlers/auth.ts` where `src/api/handlers/` isn't there), walk up to the longest existing ancestor and canonicalize THAT. Since the non-existing tail can't itself contain symlinks (it doesn't exist), and we already lexically resolved `..`, joining is safe.

### `Sandbox::check_write(path)`

For Write/Edit/DocxCreate/DocxEdit/XlsxCreate/XlsxEdit/PptxCreate/PptxEdit/PdfCreate. Calls `check()` then enforces `enforce_write_policy`:

```rust
fn enforce_write_policy(root: &Path, resolved: PathBuf) -> Result<PathBuf> {
    let protected = root.join(".thclaws");
    if resolved == protected || resolved.starts_with(&protected) {
        return Err("access denied: ... is inside .thclaws/ — that directory is reserved");
    }
    Ok(resolved)
}
```

`.thclaws/` holds team state (settings, agents, inboxes, tasks) and must not be rewritten by file tools. Teammate worktrees live at `.worktrees/<name>/` (sibling of `.thclaws/`) and are writable like any other project subdirectory.

### Bash sandbox interaction

Bash validates its `cwd` parameter (or `Sandbox::root()` default), but the actual `sh -c` command can do anything once approved. The sandbox protects file tools, not bash itself. Bash gates via the always-`requires_approval=true` + lead-/teammate-forbidden command lists instead.

---

## 8. EE Policy layer

Optional, organization-controlled. Fully no-op when no policy file exists (today's open-core behavior). Present a verified policy and the `policies.*` blocks selectively turn enforcement on. Present an unverified one and the binary refuses to start — silent fallback would defeat the point.

### Resolution flow

1. `KeySource::resolve()` — find a verification key:
   - **Embedded** (compile-time `env!("THCLAWS_EMBEDDED_POLICY_PUBKEY")`) — highest trust, cannot be overridden
   - **Env** (`THCLAWS_POLICY_PUBLIC_KEY`) — runtime override for testing / power-user self-locking
   - **File** (`/etc/thclaws/policy.pub` then `~/.config/thclaws/policy.pub`) — system-wide beats user-scoped
   - **None** — no key configured anywhere
2. `Policy::find_file()` — `THCLAWS_POLICY_FILE` env → `/etc/thclaws/policy.json` → `~/.config/thclaws/policy.json`. First existing file wins.
3. If a file exists: parse → verify signature → check binding → check expiry → return `ActivePolicy`.
4. If anything fails → `PolicyError`. Startup wrapper prints `refuse_message()` and exits non-zero.
5. If no file exists: return `Ok(None)`. Today's open-core behavior.

### `Policy` schema

```rust
pub struct Policy {
    pub version: u32,                          // SUPPORTED_VERSION = 1
    pub issuer: String,
    pub issued_at: String,
    pub expires_at: Option<String>,            // ISO-8601
    pub binding: Option<Binding>,              // org_id + binary fingerprint
    pub policies: Policies,                    // per-feature blocks
    pub signature: Option<String>,             // base64 Ed25519 over canonical JSON
}

pub struct Policies {
    pub branding: Option<BrandingPolicy>,
    pub plugins: Option<PluginsPolicy>,
    pub gateway: Option<GatewayPolicy>,
    pub sso: Option<SsoPolicy>,
}

pub struct PluginsPolicy {
    pub enabled: bool,
    pub allowed_hosts: Vec<String>,            // glob patterns
    pub allow_external_scripts: bool,          // default true
    pub allow_external_mcp: bool,              // default true
}
```

Each per-feature block has `enabled: bool`. Disabled or omitted blocks fall back to open-core default behavior. `policies.plugins.enabled: true` with empty `allowed_hosts` is "deny-all" (intentional — useful for paranoid air-gapped deployments).

### Signature verification

Ed25519 over the canonical JSON form of the document with the `signature` field removed. Canonical = recursively-sorted object keys, no insignificant whitespace, escape `\n`/`\r`/`\t`/`\b`/`\f`/`<0x20`. Implemented without external `canonical-json` dependency (verify.rs:185-256).

```rust
key.verify(&canonical_signed_payload(doc), &Signature::from_slice(&base64_decoded_sig)?)
```

Tampered docs → `SignatureMismatch`. Missing sig → `MissingSignature`. Malformed sig → `MalformedSignature`. No key configured but signature present → `NoVerificationKey`. All fail-closed.

### Expiry + binding

```rust
if let Some(exp) = &policy.expires_at {
    if is_expired(exp) { return Err(PolicyError::Expired { ... }); }
}
if let Some(binding) = &policy.binding {
    if let Some(expected_fp) = &binding.binary_fingerprint {
        let actual = binary_fingerprint();
        if !fingerprint_matches(expected_fp, &actual) {
            return Err(PolicyError::BindingMismatch { ... });
        }
    }
}
```

`is_expired` parses `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SS[Z|±HH:MM]` (offsets stripped, treated as UTC — fine for expiry semantics, off by hours never by days). Unparseable timestamps → not expired (fail-open for typos rather than lock-out).

`binary_fingerprint()` reads `current_exe()` and SHA-256 hashes it; cached in a OnceLock. `fingerprint_matches` accepts prefix matches (`sha256:abcd` matches `sha256:abcd1234`) so admins don't have to re-issue policies for every benign rebuild. Empty prefix correctly rejected (defensive guard against `starts_with("")` matching everything).

### `validate_policies` cross-check

Catches misconfigurations that would silently fail open at runtime:
- `gateway.enabled: true` with `gateway.url` empty → `InvalidConfig`
- `sso.enabled: true` with `sso.issuer_url` or `sso.client_id` empty → `InvalidConfig`

Refuses to start with a clear field-naming message rather than silently bypassing the subpolicy.

### `ACTIVE: OnceLock<Option<ActivePolicy>>`

Set once at startup. Cannot be reloaded mid-process — by design, prevents runtime tampering. Policy updates require restart. Feature modules read via `policy::active()`.

### Convenience predicates

- `external_scripts_disallowed()` — `policy active AND plugins.enabled AND !allow_external_scripts`. Read by skill installer + load path.
- `external_mcp_disallowed()` — `policy active AND plugins.enabled AND !allow_external_mcp`. Read by `AppConfig::load` to filter HTTP MCP servers.

---

## 9. Host allowlist (`policy::check_url`)

Used by skill install (`install_from_url`), plugin install (`install_plugin`), and MCP HTTP server loading (`AppConfig::load`).

```rust
pub fn check_url(url: &str) -> AllowDecision {
    match crate::policy::active() {
        Some(a) => check_url_with(url, &a.policy),
        None => AllowDecision::NoPolicy,
    }
}
```

`AllowDecision`:
- `NoPolicy` — no policy active or `plugins.enabled: false`. Caller treats as allowed (today's open-core).
- `Allowed` — matched at least one pattern.
- `Denied { reason: String }` — caller refuses with the reason.

### Pattern matching

URL normalization (`normalize_url_for_match`):
1. Strip scheme (`https://`, `git@`)
2. Strip user prefix (`git@github.com:foo/bar` → `github.com:foo/bar`)
3. Strip query/fragment (`?token=abc&ref=main` → dropped)
4. Strip trailing `.git`
5. Drop port from host segment
6. Lowercase host
7. Result: `host[/path/...]`

Pattern matching (`matches_pattern`):
- `*.host` — host-glob; matches `foo.host` and `host` itself
- `host` — host-only; matches any path
- `host/seg1/*` — `*` matches one path segment
- `host/seg-*` — mid-segment wildcard via `glob_segment` (split on `*`, find chunks in order)
- Case-insensitive

Examples:
| Pattern | Matches | Doesn't match |
|---|---|---|
| `github.com` | `github.com/anyone/anything.git` | `gitlab.com/anyone/anything` |
| `github.com/acmecorp/*` | `github.com/acmecorp/internal-skills` | `github.com/randomuser/skills` |
| `*.acme.example` | `internal.acme.example/...`, `acme.example/...` | `attacker.example/...` |
| `github.com/acme/skill-*` | `github.com/acme/skill-deploy` | `github.com/acme/plugin-other` |

---

## 10. MCP stdio per-command allowlist

Independent of the org policy. Per-user, persistent at `~/.config/thclaws/mcp_allowlist.json`.

```rust
async fn check_stdio_command_allowed(
    config: &McpServerConfig,
    approver: Option<Arc<dyn ApprovalSink>>,
) -> Result<()> {
    if std::env::var("THCLAWS_MCP_ALLOW_ALL").ok().as_deref() == Some("1") {
        return Ok(());   // CI bypass
    }
    let mut allowlist = McpAllowlist::load();
    if allowlist.contains(&config.command) { return Ok(()); }

    if let Some(approver) = approver {
        // GUI mode: route through GuiApprover modal (same UI as tool approval)
        let req = ApprovalRequest { ... };
        return match approver.approve(&req).await {
            Allow | AllowForSession => {
                allowlist.insert(&config.command);
                allowlist.save();   // atomic write via tmp + rename (M6.15 BUG 5)
                Ok(())
            }
            Deny => Err(...),
        };
    }

    // CLI fallback: legacy stderr/stdin prompt
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(...);   // No TTY → fail closed
    }
    eprintln!(...);  read y/yes from stdin;  insert + save on yes
}
```

Why per-command (not per-server): a malicious `.thclaws/mcp.json` could point `command` at `/usr/bin/curl` or `/bin/sh`. The allowlist keys by the literal command string so users who change `PATH` or substitute the binary re-trigger approval.

HTTP MCP transport doesn't go through this gate (returns early at `spawn_with_approver`). HTTP MCPs are remote URLs not local commands, gated by `policy::check_url` instead.

---

## 11. Bash hard-blocks

Bash runs via `/bin/sh -c` — once approved by the user, the command can do anything the user can. Two hard-coded denylists run AT TOOL DISPATCH (after approval, before exec) to catch specific footguns.

### `lead_forbidden_command`

Active only when `crate::team::is_team_lead() == true`. Blocks:
- `git reset --hard`
- `git clean -f` / `-d`
- `git push --force` / `-f`
- `git rebase`
- `git worktree remove` / `prune`
- `git checkout --` / `git checkout .`
- `git restore --worktree` / `git restore .`
- `git merge --abort`
- `rm -rf` / `-fr` / `rm -r`

Reason: the lead is a coordinator. Destructive workspace ops have repeatedly cascade-killed teammate worktrees and processes when the LLM lead reached for `git reset --hard` or `rm -rf` to "clean up" unexpected state. The prompt rule alone is honor-system; this is the seatbelt.

`set_is_team_lead(team_enabled && !is_teammate)` is called at worker startup — teammates (processes spawned with `THCLAWS_TEAM_AGENT=1`) are NOT leads.

### `teammate_forbidden_command`

Active only when `is_teammate_process() == true` (`THCLAWS_TEAM_AGENT` env set). Catches the cross-branch reset pattern that wiped teammate worktrees in past runs:

`git reset --hard <ref>` is allowed when `<ref>` is:
- `HEAD` / `HEAD~N` / `HEAD^` / `HEAD@{N}`
- `tags/...` / `refs/tags/...`
- A hex SHA ≥ 7 chars (`is_ascii_hexdigit`)

Blocked when `<ref>` is anything else (bare branch name, `origin/main`, `team/backend`, etc.).

### `is_destructive_command`

Just prints a yellow `⚠ destructive command detected` to stderr — does NOT block. Triggers on a comprehensive list (rm -rf, sudo, kill -9, mkfs, dd if=, drop database, kubectl delete, terraform destroy, aws s3 rm, curl ... | sh, etc., 80+ patterns). Useful as audit-trail signal.

---

## 12. Script-bearing skills policy

`enforce_scripts_policy`:

```rust
fn enforce_scripts_policy(skill_dir: &std::path::Path) -> Result<()> {
    if !crate::policy::external_scripts_disallowed() {
        return Ok(());
    }
    let scripts = skill_dir.join("scripts");
    if !scripts.exists() { return Ok(()); }
    if has_entries(&scripts) {
        return Err("skill ships scripts/; org policy disallows external scripts");
    }
    Ok(())
}
```

Called at every install rename point in skills.rs (lines 540, 583, 823, 846, 879) AND at load-time in `SkillStore::load_dir` (M6.20 BUG M5 fix). Pre-fix: a skill installed BEFORE the policy was active continued to load post-policy. Now policy rotation takes effect on next launch — script-bearing skills installed pre-policy are skipped at discovery with a yellow `[skills] skipping <path>: …` warning.

---

## 13. HTTP MCP filtering at config load

`config.rs::load_mcp_servers` reads `mcp.json`, parses into `McpServerConfig`, then filters HTTP entries through `policy::check_url`:

```rust
let filtered = if crate::policy::external_mcp_disallowed() {
    parsed.into_iter().filter(|s| {
        if s.transport != "http" { return true; }
        match crate::policy::check_url(&s.url) {
            Allowed | NoPolicy => true,
            Denied { reason } => {
                eprintln!("[mcp] '{}' skipped: {}", s.name, reason);
                false
            }
        }
    }).collect()
} else {
    parsed
};
```

Stdio MCPs pass through unconditionally — gating arbitrary stdio commands is the per-command allowlist (§10), not the policy host-allowlist.

---

## 14. Subagent approval propagation (M6.20 BUG H1)

The CLI's `Task` tool dispatches to `SubAgentTool::call` which calls `factory.build(...)` to construct a child agent. Pre-fix `ReplAgentFactory::build` did:

```rust
Ok(Agent::new(self.provider.clone(), tools, model, &system).with_max_iterations(max_iter))
```

No `.with_approver(...)`, no `.with_permission_mode(...)`. So the child got `Agent::new`'s defaults: `approver = Arc::new(AutoApprover)` and `permission_mode = PermissionMode::Auto`. Combined with the dispatch fallback (§2):
- CLI never calls `set_current_mode(...)` (only GUI worker does)
- `current_mode()` returns the bare default `Ask`
- Child's `permission_mode_default = Auto`
- Fallback fires: `permission_mode = Auto`
- `needs_approval = matches!(Auto, Ask) && ...` = `false` — no approval gating

Reproducer: `thclaws --cli`, model emits `Task(prompt: "use bash to write a file")`. Parent prompted for the Task spawn (Task itself has `requires_approval=true`). User approved. Subagent then called Bash without prompting.

**Fix (M6.20):** added `approver: Arc<dyn ApprovalSink>` + `permission_mode: PermissionMode` to `ReplAgentFactory`. Refactored `run_repl` to build them BEFORE the factory block so both the factory and the top-level agent share the same `Arc<ReplApprover>`. Recursive child factories propagate too. Child build:
```rust
Agent::new(self.provider.clone(), tools, model, &system)
    .with_max_iterations(max_iter)
    .with_approver(self.approver.clone())
    .with_permission_mode(self.permission_mode)
```

**Scope:** CLI only. GUI mode never registers `SubAgentTool` (`with_builtins` doesn't include it; team tools register `SpawnTeammate` which is subprocess-spawn, not in-process `Task`).

---

## 15. Session-swap state hygiene (M6.20 BUG M2 + M3)

The `permission_mode` global slot, the `pre_plan_mode` stash, and the `session_allowed` flag on the approver all persist for the worker's lifetime. Pre-M6.20, none of them reset on session swap. So:

- BUG M2: yolo flag set in session A continued to auto-approve in session B
- BUG M3: Plan mode entered in session A leaked into session B (where there's no plan to submit)

**Fix:** at every session-swap site, three resets:

```rust
state.approver.reset_session_flag();                       // M2
let _ = crate::permissions::take_pre_plan_mode();          // M3 (clear stash)
crate::permissions::set_current_mode_and_broadcast(state.agent.permission_mode);  // M3
```

Wired into:
- GUI: `NewSession`, `LoadSession`, `SessionDeletedExternal` (shared_session.rs)
- CLI: `/load`, `/fork`, `/model`, `/provider` (repl.rs)
- `/clear` does NOT reset — it only clears history, doesn't change session.

The `reset_session_flag()` method is on the `ApprovalSink` trait with a default no-op — `AutoApprover`/`DenyApprover`/`ScriptedApprover` use the default (no session-scoped state to clear); `GuiApprover` and `ReplApprover` override to clear their `session_allowed: AtomicBool`.

---

## 16. Code organization

```
crates/core/src/
├── permissions.rs                                    ── ~560 LOC
│   ├── PermissionMode { Auto, Ask, Plan }            (lowercase serde)
│   ├── current_mode_slot / current_mode /
│   │   set_current_mode / set_current_mode_and_broadcast
│   ├── pre_plan_mode_slot / stash_pre_plan_mode /
│   │   take_pre_plan_mode
│   ├── set_mode_broadcaster                          (GUI worker registers events_tx)
│   ├── ApprovalRequest / ApprovalDecision / ApprovalSink
│   ├── AutoApprover / DenyApprover / ScriptedApprover
│   ├── ReplApprover                                  (stdin via spawn_blocking)
│   ├── GuiApprover                                   (mpsc + oneshot bridge to webview)
│   └── tests                                         (~12 unit tests including M2 regression)
│
├── policy/
│   ├── mod.rs                                        (~660 LOC)
│   │   ├── Policy / Binding / Policies / *Policy structs
│   │   ├── ActivePolicy + ACTIVE: OnceLock
│   │   ├── load_or_refuse                            (startup entry; fail-closed on every error)
│   │   ├── find_file                                 (env → /etc → ~/.config search order)
│   │   ├── validate_policies                         (cross-check enabled vs required fields)
│   │   ├── is_expired / parse_iso8601 / days_from_civil
│   │   ├── binary_fingerprint / fingerprint_matches  (prefix match with empty rejection)
│   │   ├── external_scripts_disallowed / external_mcp_disallowed   (convenience predicates)
│   │   └── tests                                     (~14 unit tests)
│   ├── verify.rs                                     (~490 LOC)
│   │   ├── KeySource { Embedded, Env, File, None }   (resolve order + label)
│   │   ├── verify_policy                             (Ed25519 verify against canonical bytes)
│   │   ├── canonical_signed_payload + write_canonical (recursive sorted-key JSON encoder)
│   │   ├── parse_pubkey / read_pubkey_bytes /
│   │   │   decode_text_pubkey                        (raw 32B / base64 / PEM)
│   │   └── tests                                     (~12 unit tests including round-trip,
│   │                                                  tampered, cross-keypair, no-key fail-closed)
│   ├── allowlist.rs                                  (~370 LOC)
│   │   ├── AllowDecision { NoPolicy, Allowed, Denied { reason } }
│   │   ├── check_url / check_url_with                (NoPolicy when no policy or plugins disabled)
│   │   ├── normalize_url_for_match                   (strip scheme/user/query/.git/port)
│   │   ├── matches_pattern                           (host-glob via *.; segment glob)
│   │   ├── glob_segment                              (mid-segment * via split + chunk-find)
│   │   └── tests                                     (~10 unit tests)
│   └── error.rs                                      (~100 LOC)
│       └── PolicyError + refuse_message              (every variant fail-closed)
│
├── sandbox.rs                                        ── ~415 LOC
│   ├── SANDBOX_ROOT: RwLock<Option<PathBuf>>
│   ├── Sandbox::init                                 (canonicalize cwd or $THCLAWS_PROJECT_ROOT)
│   ├── Sandbox::root / check / check_write
│   ├── validate_against                              (lexical normalize → canonicalize → walk-up)
│   ├── enforce_write_policy                          (.thclaws/ deny)
│   ├── lexical_normalize                             (resolve .. and . without FS access)
│   └── tests                                         (~14 unit tests including dotdot, symlink,
│                                                      worktree relative, deep-new-path)
│
├── agent.rs::run_turn (line ~1060-1314)              ── the dispatch site
│   ├── permission_mode resolve (with fallback)       (M6.20 BUG H1-aware)
│   ├── TodoWrite plan-mode block                     (M6.20 BUG M1 — fires first)
│   ├── generic plan-mode block
│   ├── approval-window gate                          (UpdatePlanStep/ExitPlanMode while awaiting)
│   ├── approval gate                                 (Ask + requires_approval → approver.approve)
│   └── tool.call_multimodal
│
├── shared_session.rs
│   ├── WorkerState.approver: Arc<dyn ApprovalSink>
│   ├── set_mode_broadcaster                          (mode change → ViewEvent)
│   ├── McpAppCallTool handler                        (widget tool dispatch with own approval gate)
│   ├── NewSession / LoadSession / SessionDeletedExternal
│   │   handlers                                      (M6.20 BUG M2 + M3 reset)
│   └── ChangeCwd handler                             (re-init Sandbox)
│
├── repl.rs
│   ├── ReplAgentFactory                              (M6.20 BUG H1: approver + perm_mode fields)
│   ├── /permissions slash                            (auto/ask/yolo)
│   ├── /plan slash                                   (on/off/status, stash + restore)
│   └── /load /fork /model /provider                  (M6.20 BUG M2 + M3 reset)
│
├── gui.rs
│   ├── approval_response IPC                         (resolve(id, decision))
│   ├── plan_approve IPC                              (set Auto, kick "Begin executing")
│   ├── plan_cancel IPC                               (restore pre_plan, clear plan)
│   └── PermissionModeChanged ViewEvent forwarder     (sidebar status pill)
│
├── tools/plan.rs
│   ├── EnterPlanModeTool                             (stash_pre_plan_mode + set Plan)
│   └── ExitPlanModeTool                              (take_pre_plan_mode + restore)
│
├── tools/plan_state.rs::update_step / force_step_done
│   └── plan-completion auto-restore                  (final step Done → take_pre_plan)
│
├── tools/bash.rs
│   ├── lead_forbidden_command                        (rm -rf, git reset --hard, etc.)
│   ├── teammate_forbidden_command                    (cross-branch reset)
│   └── is_destructive_command                        (yellow ⚠ marker; doesn't block)
│
├── mcp.rs
│   ├── McpAllowlist + mcp_allowlist_path             (~/.config/thclaws/mcp_allowlist.json)
│   ├── check_stdio_command_allowed                   (THCLAWS_MCP_ALLOW_ALL bypass + first-time prompt)
│   └── McpClient::spawn_with_approver                (HTTP transport short-circuits the gate)
│
├── skills.rs
│   ├── enforce_scripts_policy                        (install paths + load_dir as of M6.20)
│   └── install_from_url                              (policy::check_url gate)
│
└── config.rs::AppConfig::load
    └── HTTP MCP filtering                            (policy::check_url per server)
```

---

## 17. Testing

`permissions::tests` — ~14 unit tests:

**Sink behavior:**
- `auto_approver_always_allows`
- `deny_approver_always_denies`
- `scripted_approver_plays_back_queue_and_defaults_to_deny`
- `allow_for_session_sticks_after_first_call`

**GuiApprover round-trip:**
- `gui_approver_round_trip` — request forwarded, resolve unblocks
- `gui_approver_allow_for_session_sticks`
- `gui_approver_denies_when_receiver_dropped`

**Mode + serde:**
- `permission_mode_default_is_ask`
- `permission_mode_serde_lowercase`

**M6.20 regression (BUG M2):**
- `gui_approver_reset_session_flag_clears_yolo`
- `repl_approver_reset_session_flag_clears_yolo`

`policy::tests` — ~14 unit tests covering parse, expiry, fingerprint, and `validate_policies`:
- `iso_parses_date_only` / `iso_parses_full_timestamp` / `iso_handles_offset_by_treating_as_utc` / `iso_garbage_is_treated_as_unparseable`
- `unparseable_expiry_is_not_treated_as_expired` / `past_expiry_is_expired` / `future_expiry_is_not_expired`
- `fingerprint_exact_match` / `fingerprint_prefix_match` / `fingerprint_mismatch` / `fingerprint_empty_expected_does_not_match`
- `validate_rejects_gateway_enabled_with_empty_url` / `validate_accepts_gateway_disabled_with_empty_url`
- `validate_rejects_sso_enabled_with_empty_issuer`
- `policy_round_trips_through_json`

`policy::verify::tests` — ~12 tests covering the signature surface:
- `canonical_payload_sorts_keys_recursively` / `canonical_payload_strips_signature_field`
- `round_trip_signature_verifies` / `tampered_payload_fails_verification` / `cross_keypair_signature_does_not_verify`
- `missing_signature_field_errors_explicitly` / `malformed_signature_field_errors_explicitly`
- `no_key_source_with_present_signature_fails_closed`
- `canonical_form_normalizes_key_order_for_signing`
- `read_pubkey_bytes_accepts_raw_32_bytes` / `accepts_base64_text` / `accepts_pem_wrapped_text` / `rejects_garbage`
- `key_source_file_label_includes_path` / `key_source_file_acts_as_valid_verification_source`

`policy::allowlist::tests` — ~10 tests covering URL normalization and pattern matching.

`sandbox::tests` — ~14 tests including dotdot escape, symlink escape, deep-new-path, worktree relative resolution, `.thclaws/` write protection.

`agent::tests` — covers the dispatch gate end-to-end (deny in Ask mode, skip read-only in Ask mode, Auto bypasses). M1 regression test would race the global mode slot with other tests; verified by source inspection.

`repl::tests` — `subagent_factory_propagates_approver_and_permission_mode` (M6.20 BUG H1 regression).

`bash::tests` — destructive + lead-forbidden + teammate-forbidden command list assertions.

735 lib tests total as of M6.20 (was 732).

---

## 18. Migration / known limitations

### M6.20 fixes (`dev-log/138`)

| # | Severity | What | Where |
|---|---|---|---|
| H1 | HIGH | Subagent factory dropped approver + perm_mode → silent auto-approve in CLI Ask mode | `repl.rs::ReplAgentFactory` |
| M1 | MED | TodoWrite plan-mode message was dead code (generic block fired first) | `agent.rs` reorder |
| M2 | MED | AllowForSession yolo flag persisted across session swaps | `permissions.rs` + worker handlers |
| M3 | MED | permission_mode + pre_plan stash persisted across session swaps | `shared_session.rs` + `repl.rs` |
| M5 | MED | Script-bearing skills installed pre-policy continued to load post-policy | `skills.rs::SkillStore::load_dir` |

### Earlier fixes

- M6.15 BUG 2 — Widget tool calls route through approval gate
- M6.15 BUG 5 — MCP allowlist atomic write via tmp + rename
- M6.15 BUG 8 — MCP HTTP debug log gated behind `THCLAWS_MCP_DEBUG`

### Deferred (still)

- **BUG L1** — Cancellation doesn't unblock pending approval. `GuiApprover::approve` doesn't `tokio::select!` on cancel. Practical: rare hang scenario.
- **BUG L2** — `pending`/`unresolved` HashMap entries leak when consumer drops mid-approval. Cosmetic.
- **BUG L3** — `HookEvent::PermissionDenied` defined but never fired. Entire hooks subsystem (`PreToolUse`/`PostToolUse`/`SessionStart`/etc.) is unwired. Wiring just `PermissionDenied` would be inconsistent — needs broader work.
- **BUG L4** — `pre_plan_mode` is single-slot. Defensive note only; current call sites guard against re-entry.
- **BUG L5** — Two-step lock window in `GuiApprover::resolve`. Cosmetic, not exploitable.
- **BUG M4** — MCP-Apps widget tool dispatch in plan mode prompts via approval gate instead of structurally blocking. Defensible UX argument, but inconsistent with agent loop. Needs UX call.

### Sprint chronology

| Sprint | Dev-log | What shipped (permissions-relevant) |
|---|---|---|
| Phase 11 (plan mode M2) | `~115` | `PermissionMode::Plan`, dispatch gate, `EnterPlanMode`/`ExitPlanMode`, plan-mode structural block |
| Phase 11 (plan mode M3) | `~117` | sidebar `plan_approve` flips to Auto, `plan_cancel` restores pre_plan |
| Compaction + plan persistence | `~125` | plan_state restore on session load |
| EE Phase 1 | `~130` | `policy/` module: signature, expiry, binding, allowlist, `external_scripts_disallowed`, `external_mcp_disallowed` |
| M6.9 (Bug C2) | `~129` | `plan_approve` defensive guard requires unfinished plan |
| M6.13 (C3) | `~131` | `force_step_done` resets per-step attempts |
| M6.14 | `132` | `Sandbox::init` honors `$THCLAWS_PROJECT_ROOT` for worktree teammates |
| M6.15 | `133` | MCP-Apps widget approval routing (BUG 2), allowlist atomic write (BUG 5), debug log gate (BUG 8) |
| M6.20 | `138` | THIS sprint — H1 + M1 + M2 + M3 + M5 |
