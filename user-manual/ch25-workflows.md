# Chapter 25 — Workflows

Workflows are thClaws's **fourth orchestration tier**: the model writes a
JavaScript script that fans work out across many subagents, and a
sandboxed JS engine runs that script deterministically on your
machine. Unlike subagents (Chapter 15), `/agent` side-channels, or
Agent Teams (Chapter 17), the orchestrator here is **code**, not the
model — which means rerunning the same workflow gives the same shape
of work every time and a long-running job leaves a checkpoint on disk.

Workflows are **Tier 1** in v0.23 — fan-out works, schema validation
and resume land in Tier 2 (see "What's missing in Tier 1" below).

## When to use workflows

Use workflows for **bulk, deterministic, mostly-independent work**:

- "rewrite all 800 test files to use the new fixture"
- "for every `.md` page under `kms/bug/`, translate it to Thai"
- "audit each crate's `Cargo.toml` and flag deprecated deps"

Use the `Task` tool (Chapter 15) for **one-shot model-driven
side-quests** the agent decides to spawn during a normal turn — that's
what subagents are still for.

Use `/agent` (Chapter 15) when **you** know exactly what a specialist
should do and want it running concurrently with your main session.

Use Agent Teams (Chapter 17) when teammates need to **collaborate**
— exchange messages, debate hypotheses, coordinate on a shared task
list. Workflows are for stateless fan-out; teams are for stateful
collaboration.

## Quick start

```text
/workflow run summarize each .rs file under src/ in one line
```

What happens, in order:

1. **Author phase.** The model writes a JavaScript script using the
   `thclaws.*` API (the API is detailed in the model's system prompt,
   so the script you get back already knows what's available).
2. **Review.** The script is printed with line numbers. You're
   prompted:
   ```text
   [a]pprove · [c]ancel · [r]e-author:
   ```
   - `a` — run the script as written.
   - `c` — drop the workflow.
   - `r` — give a one-line revision note ("use the read tool not bash
     cat") and the model rewrites the script with that feedback. Loop
     until you `a` or `c`.
3. **Execute.** A workflow id (`wf-…`) prints, then each subagent
   invocation shows a progress line:
   ```text
   ✓ w0  List every .rs file under src/, recursively. Return o…   2s
   ✓ w1  Read crates/core/src/agent.rs and write ONE sentence …   3s
   ✓ w2  Read crates/core/src/repl.rs and write ONE sentence d…   4s
   …
   workflow done — 47 workers, total 1m 12s
   crates/core/src/agent.rs — the streaming agent loop
   crates/core/src/repl.rs — REPL command parser + rustyline I/O
   …
   ```

If a worker errors, you see `✗ wN  …` for that line and the script
typically catches and continues (depending on what the model wrote).

## The `thclaws.*` API

Your script gets exactly one global — `thclaws` — with these
fields:

```js
thclaws.subagent({
  prompt: string,           // required — the worker's task
  budget?: {                // Stages G + I: both enforced
    time?: number | string, //   "60s" / "2m" / "1m30s" / 60 (seconds)
    tokens?: number,        //   input + output cap per worker call
  },
  schema?: object,          // Stage H: JSON Schema. Worker is asked for JSON
                            //   matching the schema; on success the call
                            //   returns the parsed value (not text).
  retry?: number | {        // Stage H: retries on hard errors + schema misses
    max: number,
    backoff?: string,       //   "exponential" / "linear" / "500ms" / etc.
  },
  caps?: {                  // Stage M: explicit grants — default DENY for KMS writes
    kms?: { write?: string[] },
  },
  // model? — Stage L
}) → string | parsed_value
```

**`caps.kms.write` controls what a worker can write.** Outside a
workflow run KMS write tools work as before. Inside `/workflow run`,
workers default to **deny** for all KMS writes; the script must
pass `caps: { kms: { write: ["scratch", "audit-log"] } }` to grant
write access to specific KMS names for that single call. Grants are
audited as `worker_caps` events in state.jsonl, and they are NOT
transitive — a worker's own model-driven Task spawns get fresh
(empty) caps unless re-granted explicitly.

Time budget triggers `tokio::time::timeout`; exceeding it throws.
Schema validation runs after each attempt — if the worker's text isn't
parsable JSON or doesn't match the schema, the call retries up to
`retry.max` with the chosen `backoff`. Every retry is logged as a
`worker_retry` event so `/workflow inspect <id>` shows the journey. Workers inherit the parent session's provider,
model, system prompt, tool registry, memory, KMS, and permission mode
— so a worker can `Bash`, `Read`, `Edit`, search KMS, use MCP servers,
etc. Subagent recursion (a worker calling Task itself) is bounded by
the same `DEFAULT_MAX_DEPTH = 3` ceiling sub-agents already honour.

**Async syntax works** — scripts that use `await` / `async` /
`Promise.all` route through Boa Module mode. `thclaws.subagent` is
still synchronous internally in Stage J MVP, so `Promise.all([...])`
resolves correctly but workers execute in source order (one at a
time). Genuine parallelism via tokio JobExecutor is Stage J.2.

### What you can write in the script

JS control flow: `for`, `while`, `if`/`else`, `try`/`catch`, `await`,
`async` functions, `Promise.all`, destructuring, template literals,
`Array` and `String` methods, regex, JSON parsing.

### What you can't write

- `eval`, `Function` (stripped from the sandbox)
- `fetch`, `require`, `process`, DOM, `console.log`

Anything I/O-flavoured must go through a subagent.

### A short example

```js
// Workflow: list .rs files, summarise each
const list = await thclaws.subagent({
  prompt: "List every .rs file under src/, recursively. Paths only."
});
const paths = list.split("\n").map(s => s.trim()).filter(Boolean);

const summaries = await Promise.all(
  paths.map(p => thclaws.subagent({
    prompt: `Read ${p} and write ONE sentence describing what it does.`
  }))
);

paths.map((p, i) => `${p} — ${summaries[i]}`).join("\n");
```

For sync scripts the **last expression** is the result. For async
scripts (which run in Module mode) the result is the last expression
the auto-wrapper finds, OR an explicit `globalThis.__wf_result = …`
assignment, OR `undefined` if neither.

## State on disk

Every run writes a JSONL log to:

```text
.thclaws/workflows/wf-<id>/state.jsonl
```

One event per line, flushed after each write so a Ctrl-C leaves the
file in a recoverable shape. Event shapes:

```jsonl
{"ts":"…","kind":"start","id":"wf-…","prompt":"…","script_sha":"…","script_chars":234}
{"ts":"…","kind":"worker_start","id":"wf-…","worker":"w0","prompt":"…"}
{"ts":"…","kind":"worker_done","id":"wf-…","worker":"w0","output":"…"}
{"ts":"…","kind":"worker_error","id":"wf-…","worker":"w1","error":"…"}
{"ts":"…","kind":"done","id":"wf-…","result":"…"}
```

You can `cat`, `grep`, or `jq` the file at any time — it's plain
JSONL, never opaque. The companion `script.js` (the approved JS) is
written next to it at start, so `/workflow resume <id>` can replay
against the same source.

Slash commands for managing runs (all REPL-only in Tier 2):

```text
/workflow list             one line per run on disk, newest first
/workflow inspect <id>     dump state.jsonl events in human form
/workflow resume <id>      re-run, replaying completed workers from
                           the cache; fresh spawns continue numbering
/workflow rm <id>          y/N confirm + remove the workflow dir
```

`resume` is keyed by **prompt match** at each `thclaws.subagent`
call. A cached entry is consumed only when its prompt equals the
current call's prompt; mismatches fall through to fresh spawn (the
script may have been edited or paths changed). Any cached entries
left over at script end are reported as "diverged" so you know.

If `.thclaws/` can't be written (read-only volume, permissions), the
workflow runs anyway and prints:
```text
/workflow run: state.jsonl unavailable — proceeding without checkpoint
```
You lose the audit trail but not the run.

## Headless mode

`thclaws -p "/workflow run <goal>"` is **refused**. The author phase
produces a script that needs your review before execution; `-p` mode
has no surface for that review and default-approving an arbitrary
script is dangerous.

Pre-authored scripts run headless via `thclaws --workflow <file.js>`
(Stage L). The author phase is skipped entirely — the file is
operator-vetted. Useful for CI, cron jobs (chapter 19), and
dev-plan/28 deploy hooks.

```sh
# Fresh run:
thclaws --workflow ./scripts/audit-crates.js

# Resume a previous workflow id (or unique prefix):
thclaws --workflow ./scripts/audit-crates.js --resume wf-18b3fa

# stdout = script's final value; stderr = id + done summary.
# Pipe stdout to jq, redirect to file, etc.:
thclaws --workflow ./scripts/audit-crates.js > result.txt
```

Exit code is 0 on success, 1 on script failure. Headless mode auto-
approves every subagent tool call (same as
`--dangerously-skip-permissions` — the operator vetted the script).

## What's missing in Tier 1

These are documented gaps, not bugs — they land in Tier 2 / 3 per
[dev-plan/32](../dev-plan/32-dynamic-workflows.md) (workspace-only):

- **`Promise.all` resolves but doesn't truly parallelise (Stage J MVP).**
  Boa now runs scripts that use `await` / `Promise.all` in Module mode,
  so the syntax parses and `await thclaws.subagent(...)` returns the
  worker's text. But the host function still blocks the JS thread
  per-call, so each subagent call inside `Promise.all` runs
  sequentially. Wall-clock = sum of latencies, not max. Stage J.2 will
  add a tokio-integrated JobExecutor so workers genuinely run in
  parallel.
- **No budget caps.** Per-worker `budget: { tokens, time }` is
  ignored. Tier 2 enforces both.
- **No verification phase.** `thclaws.verify({...})` doesn't exist
  yet — Tier 3.
- **No GUI worker grid.** From the chat tab `/workflow run` is
  explicitly refused with a one-line explanation. The interactive
  review UX doesn't fit a single chat bubble, and a real-time grid of
  worker progress is a Tier 3 frontend deliverable.

## Cost awareness

Each `thclaws.subagent` call is a separate model turn — typically a
few seconds and a few hundred to a few thousand tokens. A 200-worker
workflow can easily burn $5–$20 of API tokens depending on the model.
Two practical guardrails:

- **Cap the fan-out before writing the script.** If the goal is
  unbounded ("every file"), have a *discovery* subagent return the
  list first so you see the cardinality before approving the script.
- **Watch the close-out summary.** `workflow done — N workers, total
  Xs, In tokens / Out tokens (≈$Y.YY)` is printed after every
  `/workflow run`. Tier-billed or unknown models show `(cost unknown)`
  instead of a number.

## Quick reference

| | Subagent (`Task`) | `/agent` | Agent Teams | Workflow |
|---|---|---|---|---|
| Who orchestrates | The model | You (one-shot) | Team-lead model | Code |
| Number of workers | 1 (blocking) | 1 (concurrent) | 3–5 collaborators | Tens to hundreds |
| Inter-worker chat | No | No | Yes (mailbox) | No (stateless) |
| Determinism | Model-driven | Model-driven | Model-driven | Deterministic execution |
| Resumable | No | No | Limited | Logged (Tier 2 reads it back) |
| Best for | Side-quest during a turn | Specialist running in parallel | Debate / collaboration | Bulk fan-out |

## Troubleshooting

**"workflow: state.jsonl unavailable — proceeding without checkpoint"**
— `.thclaws/workflows/` can't be created or written. Check
permissions on `.thclaws/` in the project root.

**Script error: `ReferenceError: thclaws is not defined`** — you're
probably running a script outside `/workflow run`. The `thclaws.*`
global only exists inside the workflow sandbox.

**Workflow hangs after `⠋ wN  …` line** — that worker is taking a
while. Subagent calls have no timeout in Tier 1; Ctrl-C cancels the
whole run.

**Re-author loop keeps producing the same script** — the model may be
ignoring your revision note. Try cancelling and re-running with a
sharper goal phrasing rather than relying on `r`-loops.
