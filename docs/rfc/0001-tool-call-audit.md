# RFC 0001 — Client-side tool-call audit (`policies.audit`)

**Status:** Implemented (ships in v0.120.0; `crates/core/src/audit/`) · **Tracking:** [#203](https://github.com/thClaws/thClaws/issues/203) · **Phase:** 5 (see [ENTERPRISE.md](../../ENTERPRISE.md#status-by-phase))

This is the version of the Phase 5 proposal from #203 that maintainers
will merge. It fixes a few assumptions that did not match the code and
pins the record schema so the client-side and hosted implementations
emit the same thing. Contributors implementing any part of it should
code against this document, not the original proposal.

## Scope

Gateway audit (Phase 3) remains the source of truth for *who called
which model, when*. Phase 5 records what the tool loop did between
provider calls — which tool ran, who approved it, how it was confined,
whether it failed — as a thin index into the session JSONL that
already stores every `tool_use` / `tool_result` verbatim. Nothing in
the audit record duplicates payload content.

## What the current code actually provides

- **One dispatch site.** Every tool, including MCP tools (`McpTool`
  implements `Tool`) and workflow tools, is dispatched from one place in
  `crates/core/src/agent.rs` (two copies of the arm, ~1730 and ~2039).
  That site already has the `ApprovalSink` decision, the `pre_tool_use`
  gate result, the input, the output and the timing in scope. Phase 5
  adds a single `audit::record_tool_call()` helper there — it does not
  wrap `ApprovalSink` implementations or hook `mcp.rs` separately.
- **No gateway `run_id`.** `GatewayPolicy` only carries
  `auth_header_template`; nothing correlates provider calls today.
  Correlation is added by this RFC as outbound headers (below).
- **Session JSONL is the evidence store.** Files under
  `~/.local/share/thclaws/sessions/` are append-only and hold every tool
  call and result. Audit records reference them by `tool_use_id` and
  carry SHA-256 digests of the stored input/output so a record can be
  verified against the session file.
- **Hooks are not an audit substrate.** `pre_tool_use` / `post_tool_use`
  live in user-editable `settings.json`; anything built on them is not
  tamper-resistant. Phase 5 is policy-driven, in `crates/core`.

## Policy block

```json
"audit": {
  "enabled": true,
  "sinks": [
    { "type": "file", "path": "~/.local/share/thclaws/audit/%Y-%m-%d.jsonl" },
    { "type": "http", "url": "https://siem.acme.example/thclaws",
      "auth_header_template": "Bearer {{env:THCLAWS_AUDIT_TOKEN}}",
      "batch": 50, "flush_secs": 5 }
  ],
  "include_summary": true,
  "correlate_gateway": true
}
```

- `sinks` is a list; each entry is one `AuditSink` implementation.
  v1 ships `file` and `http`. `syslog` is not planned (no Windows
  equivalent; `http` covers SIEM ingestion).
- `file.path` accepts strftime tokens — rotation by naming, no rotator
  process. Default when omitted: one file per day under the data dir.
- `http` batches records and renders its auth header with the same
  template engine as the gateway (`{{env:NAME}}`, `{{sso_token}}`).
- `include_summary` (default `true`) controls the bounded `summary`
  field. There is **no** v1 option for full tool input or output.
- `correlate_gateway` (default `true`): when `policies.gateway.enabled`,
  add `X-ThClaws-Session: <session_id>` and `X-ThClaws-Turn: <turn>` to
  every provider request so gateway logs join on the same keys.
- Serde shape mirrors the other blocks: `Option<AuditPolicy>` on
  `Policies` with `#[serde(default, skip_serializing_if = "Option::is_none")]`,
  so already-signed policies keep verifying and `thclaws-policy-tool
  inspect` shows it unchanged. `enabled: true` with an empty `sinks` list
  refuses to start, like `gateway.enabled` with an empty `url`.
- **Fail-open.** A sink error never blocks or delays a tool call. Each
  sink keeps a dropped-record counter, reported in the `session_end`
  record and by `/policy status`.

## Record schema v1

One JSON object per line, UTF-8, keys in the order below. The field
table is normative; the JSON Schema is derived from it and will be
checked in as `crates/core/src/audit/record.v1.schema.json` with a test
that every emitted record validates.

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | ✓ | always `1` |
| `ts` | string | ✓ | RFC 3339 UTC, ms precision |
| `event` | enum | ✓ | `tool_call` · `tool_denied` · `session_start` · `session_end` |
| `session_id` | string | ✓ | matches the session JSONL filename |
| `turn` | int | ✓ | 1-based assistant turn within the session |
| `tool_use_id` | string | tool events | provider-issued id; joins to the session JSONL `tool_use` block |
| `tool` | string | tool events | registry name (`Bash`, `Write`, `mcp__<server>__<tool>` …) |
| `tool_kind` | enum | tool events | `builtin` · `mcp` · `plugin` · `workflow` |
| `mcp_server` | string | mcp only | server name from `mcp.json` |
| `actor` | object | ✓ | `{kind: "sso" \| "multiuser" \| "os", id: string}` — `sso` = email/sub from the active OIDC session; `multiuser` = `x-thclaws-user`; `os` = login name |
| `host` | object | ✓ | `{engine: "0.120.0", policy_fp: "<sha256 prefix>", machine: "<stable hash>"}` |
| `decision` | enum | tool events | `allow` · `allow_for_session` · `deny` |
| `decided_by` | enum | tool events | `auto` · `repl` · `gui` · `bot:line` · `bot:telegram` · `bot:messenger` · `hook` · `policy` (programmatic sinks) |
| `deny_reason` | string | deny only | hook stderr / policy message, ≤256 bytes |
| `confine` | object | Bash only | `{mode: "workspace" \| "strict" \| "off", enforced: bool}` — `enforced:false` = platform confiner unavailable, ran unconfined |
| `targets` | string[] | file tools | workspace-relative paths touched (Write/Edit/Read/Glob targets); no content |
| `summary` | string | optional | tool-defined, ≤256 bytes, redaction-safe (Bash: first line of command; MCP: tool name only). Omitted when `include_summary:false` |
| `input_sha256` | string | tool events | digest of the canonical JSON input as stored in the session JSONL |
| `output_sha256` | string | tool_call | digest of the tool result text |
| `is_error` | bool | tool_call | tool result `is_error` |
| `duration_ms` | int | tool_call | wall-clock of the tool body only |
| `dropped` | object | session_end | `{<sink>: <count>}` records lost to fail-open since session_start |

Rules:

- **No payloads.** Auditors verify a record against the session JSONL
  via `tool_use_id` + `input_sha256`.
- `targets` and `summary` are produced by the tool through a new
  `Tool::audit_summary(&input) -> Option<AuditSummary>` method (default
  `None`), never by a generic regex over the input. That keeps redaction
  off the critical path: nothing sensitive is written in v1, so there is
  nothing to redact.
- `tool_denied` is emitted for every refused call (approval sink, hook,
  or policy) — a denial is itself auditable.
- Records for one session are strictly ordered by `(turn, ts)`.

Example:

```json
{"v":1,"ts":"2026-09-05T03:12:44.101Z","event":"tool_call","session_id":"2026-09-05T03-10-02_a1b2","turn":7,"tool_use_id":"toolu_01XYZ","tool":"Bash","tool_kind":"builtin","actor":{"kind":"sso","id":"alice@acme.example"},"host":{"engine":"0.120.0","policy_fp":"9f3c1a2b","machine":"m-4d1e"},"decision":"allow","decided_by":"repl","confine":{"mode":"workspace","enforced":true},"summary":"git status","input_sha256":"e0d3e391760d0a9b6c24bf66cecfc5a66557784782cbc704052385bf6e9bb287","output_sha256":"88769d72e94c3c66a0fef812c005f5a13d72d8ad647386b6590dbdecb42a5bfd","is_error":false,"duration_ms":42}
```

JSON Schema (draft 2020-12):

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://thclaws.ai/schemas/audit.v1.json",
  "type": "object",
  "required": ["v", "ts", "event", "session_id", "turn", "actor", "host"],
  "additionalProperties": false,
  "properties": {
    "v": {"const": 1},
    "ts": {"type": "string", "format": "date-time"},
    "event": {"enum": ["tool_call", "tool_denied", "session_start", "session_end"]},
    "session_id": {"type": "string"},
    "turn": {"type": "integer", "minimum": 0},
    "tool_use_id": {"type": "string"},
    "tool": {"type": "string"},
    "tool_kind": {"enum": ["builtin", "mcp", "plugin", "workflow"]},
    "mcp_server": {"type": "string"},
    "actor": {"type": "object", "required": ["kind", "id"], "additionalProperties": false,
              "properties": {"kind": {"enum": ["sso", "multiuser", "os"]}, "id": {"type": "string"}}},
    "host": {"type": "object", "required": ["engine"], "additionalProperties": false,
             "properties": {"engine": {"type": "string"}, "policy_fp": {"type": "string"}, "machine": {"type": "string"}}},
    "decision": {"enum": ["allow", "allow_for_session", "deny"]},
    "decided_by": {"enum": ["auto", "repl", "gui", "bot:line", "bot:telegram", "bot:messenger", "hook", "policy"]},
    "deny_reason": {"type": "string", "maxLength": 256},
    "confine": {"type": "object", "required": ["mode", "enforced"], "additionalProperties": false,
                "properties": {"mode": {"enum": ["workspace", "strict", "off"]}, "enforced": {"type": "boolean"}}},
    "targets": {"type": "array", "items": {"type": "string"}, "maxItems": 64},
    "summary": {"type": "string", "maxLength": 256},
    "input_sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
    "output_sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
    "is_error": {"type": "boolean"},
    "duration_ms": {"type": "integer", "minimum": 0},
    "dropped": {"type": "object", "additionalProperties": {"type": "integer", "minimum": 0}}
  },
  "allOf": [
    {"if": {"properties": {"event": {"enum": ["tool_call", "tool_denied"]}}},
     "then": {"required": ["tool_use_id", "tool", "tool_kind", "decision", "decided_by", "input_sha256"]}},
    {"if": {"properties": {"event": {"const": "tool_call"}}},
     "then": {"required": ["output_sha256", "is_error", "duration_ms"]}},
    {"if": {"properties": {"event": {"const": "tool_denied"}}},
     "then": {"properties": {"decision": {"const": "deny"}}}}
  ]
}
```

## Work split

v1 is implemented by the maintainers end to end and ships in v0.120.0:
`crates/core/src/audit/` (`record.rs`, `record.v1.schema.json`,
`sink.rs`, `file.rs`, `http.rs`), `policy::AuditPolicy`, the emit sites
in `agent.rs`, gateway correlation headers on the org-gateway provider,
`Tool::audit_summary` for Bash / Write / Edit / Read / MCP, and
`/policy status`.

Open for contribution:

| Piece | Notes |
|---|---|
| `Tool::audit_summary` impls for MCP tools and richer `targets` for file tools | v1 ships Bash / Write / Edit / Read only |
| Windows verification of the `file` sink (path expansion, strftime, rotation) | |
| Okta / Entra end-to-end check of `actor.kind = "sso"` | v1 is verified against Google only |
| A worked SIEM example (`http` sink → a real collector) for `ENTERPRISE.md` | docs |

Design credit for this phase goes to the author of #203.

## Acceptance

- Block absent or `enabled: false`: one `Option` check on the hot path,
  no allocations per tool call, behaviour identical to today.
- Enabled: exactly one `tool_call` or `tool_denied` record per dispatched
  or refused tool call — builtin, MCP and workflow — in both GUI and
  headless modes.
- Every emitted record validates against `record.v1.schema.json`.
- Sink failure: the tool call completes normally, the dropped counter
  increments, `/policy status` shows it.
- Tamper test: editing `settings.json` hooks does not disable auditing.

## Non-goals (v1)

- Full tool input/output in the record (`include_tool_input` /
  `include_tool_output`) — the session JSONL already has it.
- Regex redaction.
- A `syslog` sink.
- Auditing provider calls — that is the gateway's job (Phase 3).
