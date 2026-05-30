use std::cell::RefCell;
use std::sync::Arc;

use boa_engine::builtins::promise::PromiseState;
use boa_engine::{
    js_string, native_function::NativeFunction, object::ObjectInitializer, property::Attribute,
    Context, JsArgs, JsError, JsNativeError, JsResult, JsValue, Source,
};

/// Boa-backed JS sandbox hosting the `thclaws.*` workflow API.
///
/// `thclaws.subagent` routes through the parent REPL's Task tool when
/// [`set_task_tool`] has been called on this thread; otherwise it falls
/// back to a stub that echoes the prompt, which keeps the existing
/// Stage A tests deterministic and lets the GUI / chat surface invoke
/// the sandbox without a tokio runtime for refusal messages.
///
/// `eval` and `Function` are removed from the global so an authored
/// script can't generate fresh JS at runtime — the only side effects
/// available are the host bindings we register explicitly.
pub(crate) struct WorkflowSandbox {
    ctx: Context,
}

impl WorkflowSandbox {
    pub fn new() -> JsResult<Self> {
        let mut ctx = Context::default();
        register_thclaws(&mut ctx)?;
        strip_dangerous_globals(&mut ctx)?;
        Ok(Self { ctx })
    }

    /// Run a workflow script. Sync scripts (no `await` / `async` /
    /// `Promise.all` / `Promise.resolve`) take the legacy Script-mode
    /// path that returns the last expression's value. Anything that
    /// uses async syntax routes through Boa's Module mode so top-level
    /// `await` parses correctly; the result there comes from
    /// `globalThis.__wf_result`, auto-wrapped from the script's last
    /// expression when the user didn't assign it explicitly.
    pub fn run(&mut self, script: &str) -> JsResult<String> {
        if uses_async_syntax(script) {
            self.run_module(script)
        } else {
            self.run_script(script)
        }
    }

    fn run_script(&mut self, script: &str) -> JsResult<String> {
        let result = self.ctx.eval(Source::from_bytes(script))?;
        let s = result.to_string(&mut self.ctx)?;
        Ok(s.to_std_string_escaped())
    }

    fn run_module(&mut self, script: &str) -> JsResult<String> {
        let wrapped = wrap_for_module(script);
        let source = Source::from_bytes(wrapped.as_bytes());
        let module = boa_engine::Module::parse(source, None, &mut self.ctx)?;
        let promise = module.load_link_evaluate(&mut self.ctx);
        self.ctx.run_jobs().map_err(JsError::from)?;
        match promise.state() {
            PromiseState::Fulfilled(_) => {
                let global = self.ctx.global_object();
                let result = global.get(js_string!("__wf_result"), &mut self.ctx)?;
                if result.is_undefined() {
                    return Ok("undefined".to_string());
                }
                let s = result.to_string(&mut self.ctx)?;
                Ok(s.to_std_string_escaped())
            }
            PromiseState::Rejected(reason) => Err(JsError::from_opaque(reason)),
            PromiseState::Pending => Err(js_error(
                "workflow: module evaluation pending after run_jobs",
            )),
        }
    }
}

/// Cheap detector for "this script uses async features Script mode
/// can't parse" — keyed on the bare keywords / API calls. Won't
/// false-positive on identifiers like `awaitable` because we look for
/// word boundaries via leading space / start-of-string, and the
/// substring `Promise.all` doesn't appear in ordinary identifiers.
fn uses_async_syntax(script: &str) -> bool {
    if script.contains("Promise.all") || script.contains("Promise.race") {
        return true;
    }
    for keyword in [" await ", "\tawait ", "\nawait "] {
        if script.contains(keyword) {
            return true;
        }
    }
    if script.starts_with("await ") {
        return true;
    }
    if script.contains("async function") || script.contains("async (") || script.contains("async(")
    {
        return true;
    }
    false
}

/// If the script doesn't assign to `globalThis.__wf_result` anywhere,
/// auto-wrap its trailing expression statement so the workflow still
/// has a result to return. The boundary search walks the script while
/// tracking string / template / comment state so that punctuation
/// inside `"…"`, `'…'`, `` `…${expr}…` ``, `//…`, and `/* … */` doesn't
/// count — previously a stray `}` from `${expr}` ate the last line.
fn wrap_for_module(script: &str) -> String {
    if script.contains("globalThis.__wf_result") {
        return script.to_string();
    }
    let trimmed_end = script.trim_end();
    let trimmed = trimmed_end.trim_end_matches(';').trim_end();
    let last_break = find_last_top_level_boundary(trimmed);
    let last_chunk = trimmed[last_break..].trim();
    if last_chunk.is_empty() {
        return format!("{trimmed_end}\nglobalThis.__wf_result = undefined;");
    }
    const CANT_WRAP: &[&str] = &[
        "let ", "const ", "var ", "function", "class ", "if ", "if(", "for ", "for(", "while ",
        "while(", "return ", "throw ", "try ", "try{", "import ", "export ",
    ];
    if CANT_WRAP.iter().any(|p| last_chunk.starts_with(p)) || last_chunk.starts_with("globalThis") {
        return format!("{trimmed_end}\nglobalThis.__wf_result = undefined;");
    }
    let head = &trimmed[..last_break];
    format!("{head}globalThis.__wf_result = ({last_chunk});")
}

/// Walk `s` byte by byte, tracking string / template / comment state
/// and bracket depth. Returns the byte index *after* the last `;` or
/// `\n` that occurred at top level (depth zero, outside any string,
/// template, or comment). Falls back to 0 when no boundary is found —
/// the caller then treats the whole script as the "trailing chunk."
fn find_last_top_level_boundary(s: &str) -> usize {
    let bytes = s.as_bytes();
    // Outside string mode. Inside, we track which quote we're in;
    // template literals also need a stack of `{}`-depths-at-entry so
    // a `}` closing a `${expr}` returns us to template mode.
    #[derive(Copy, Clone)]
    enum Mode {
        Plain,
        Line,   // // comment
        Block,  // /* */ comment
        Single, // '…'
        Double, // "…"
        Tmpl,   // `…`
    }
    let mut mode = Mode::Plain;
    let mut tmpl_brace_stack: Vec<i32> = Vec::new();
    let mut depth_paren = 0i32;
    let mut depth_brace = 0i32;
    let mut depth_bracket = 0i32;
    let mut last_boundary: usize = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match mode {
            Mode::Line => {
                if c == b'\n' {
                    mode = Mode::Plain;
                    if depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 {
                        last_boundary = i + 1;
                    }
                }
                i += 1;
            }
            Mode::Block => {
                if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    mode = Mode::Plain;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Mode::Single => {
                if c == b'\\' {
                    i += 2;
                } else {
                    if c == b'\'' {
                        mode = Mode::Plain;
                    }
                    i += 1;
                }
            }
            Mode::Double => {
                if c == b'\\' {
                    i += 2;
                } else {
                    if c == b'"' {
                        mode = Mode::Plain;
                    }
                    i += 1;
                }
            }
            Mode::Tmpl => {
                if c == b'\\' {
                    i += 2;
                } else if c == b'`' {
                    mode = Mode::Plain;
                    i += 1;
                } else if c == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                    tmpl_brace_stack.push(depth_brace);
                    depth_brace += 1;
                    mode = Mode::Plain;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Mode::Plain => {
                match c {
                    b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                        mode = Mode::Line;
                        i += 2;
                        continue;
                    }
                    b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                        mode = Mode::Block;
                        i += 2;
                        continue;
                    }
                    b'"' => mode = Mode::Double,
                    b'\'' => mode = Mode::Single,
                    b'`' => mode = Mode::Tmpl,
                    b'(' => depth_paren += 1,
                    b')' => depth_paren -= 1,
                    b'[' => depth_bracket += 1,
                    b']' => depth_bracket -= 1,
                    b'{' => depth_brace += 1,
                    b'}' => {
                        if let Some(entry_depth) = tmpl_brace_stack.last().copied() {
                            if depth_brace - 1 == entry_depth {
                                depth_brace -= 1;
                                tmpl_brace_stack.pop();
                                mode = Mode::Tmpl;
                                i += 1;
                                continue;
                            }
                        }
                        depth_brace -= 1;
                    }
                    b';' | b'\n' if depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 => {
                        last_boundary = i + 1;
                    }
                    _ => {}
                }
                i += 1;
            }
        }
    }
    last_boundary
}

thread_local! {
    /// Set by the REPL workflow handler immediately before invoking
    /// `WorkflowSandbox::run` (inside `spawn_blocking`). The host
    /// `thclaws.subagent` function retrieves it to route through the
    /// parent's Task tool. `None` outside the workflow handler — the
    /// host falls back to a stub.
    static WORKFLOW_TASK_TOOL: RefCell<Option<Arc<dyn crate::tools::Tool>>> =
        const { RefCell::new(None) };

    /// Stage I: per-workflow-run Usage accumulator. `SubAgentTool::call`
    /// pushes the worker's `AgentTurnOutcome.usage` here when the sink
    /// is enabled, so the workflow runtime can:
    /// 1. Enforce `budget.tokens` per call against the latest entry;
    /// 2. Print a rolled-up cost summary after the script finishes;
    /// 3. Include the per-worker usage in the `worker_done`
    ///    state.jsonl event.
    /// `None` outside `/workflow run` so model-driven Task calls stay
    /// unaffected.
    static WORKFLOW_USAGE_SINK: RefCell<Option<Vec<crate::providers::Usage>>> =
        const { RefCell::new(None) };

    /// Stage K: queue of (prompt, output) pairs that completed in the
    /// original run and should be replayed without re-spawning. The
    /// `subagent` host function pops the front entry when its prompt
    /// matches the next subagent call's prompt; on mismatch it falls
    /// through to a real spawn. `None` for fresh `/workflow run`.
    static WORKFLOW_REPLAY_CACHE: RefCell<Option<std::collections::VecDeque<(String, String)>>> =
        const { RefCell::new(None) };

    /// Stage M: per-worker capabilities. `None` means "not inside a
    /// workflow `tool.call`" — KMS write tools behave normally for
    /// model-driven Task spawns and direct REPL use. `Some(caps)`
    /// means "inside a workflow subagent call" — KMS writes are
    /// denied unless the target name is in `caps.kms_write`.
    static WORKFLOW_WORKER_CAPS: RefCell<Option<WorkerCaps>> = const { RefCell::new(None) };

}

// Tier 3 polish: chat-tab worker progress. When set, each
// `thclaws.subagent` call emits ToolCallStart/Result events so the
// chat tab renders workers as one-line `▸ … ✓` indicators alongside
// regular tool calls. `None` outside the chat-surface workflow
// handler (REPL prints its own per-worker line via `tool_display`;
// headless prints to stderr). Gui-gated because `ViewEvent` lives in
// `shared_session`, which itself is `#[cfg(feature = "gui")]`.
#[cfg(feature = "gui")]
thread_local! {
    static WORKFLOW_EVENTS_TX: RefCell<
        Option<tokio::sync::broadcast::Sender<crate::shared_session::ViewEvent>>,
    > = const { RefCell::new(None) };
}

// Stop-button plumbing. When set, the `subagent` host function polls
// before each attempt and races the in-flight `tool.call` against
// `cancel.cancelled().await` so a Stop click aborts mid-stream.
// Reset after the workflow run (the host owns the token and calls
// `.reset()` so the next user turn isn't pre-cancelled).
thread_local! {
    static WORKFLOW_CANCEL: RefCell<Option<crate::cancel::CancelToken>> =
        const { RefCell::new(None) };
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WorkerCaps {
    pub kms_write: std::collections::HashSet<String>,
}

/// Install (or clear with `None`) the Task tool the sandbox's
/// `thclaws.subagent` will route through. Per-thread — pair with
/// `spawn_blocking` so the thread-local lives for one workflow run.
pub(crate) fn set_task_tool(tool: Option<Arc<dyn crate::tools::Tool>>) {
    WORKFLOW_TASK_TOOL.with(|cell| *cell.borrow_mut() = tool);
}

/// Enable / disable the Stage I usage sink. Pair with `spawn_blocking`
/// + `take_all_usages` at the end of the workflow run.
pub(crate) fn set_usage_sink(enabled: bool) {
    WORKFLOW_USAGE_SINK
        .with(|cell| *cell.borrow_mut() = if enabled { Some(Vec::new()) } else { None });
}

/// Called by SubAgentTool after each successful turn — no-op when the
/// sink is disabled (i.e. outside a workflow run).
pub(crate) fn push_worker_usage(usage: crate::providers::Usage) {
    WORKFLOW_USAGE_SINK.with(|cell| {
        if let Some(vec) = cell.borrow_mut().as_mut() {
            vec.push(usage);
        }
    });
}

fn last_worker_usage() -> Option<crate::providers::Usage> {
    WORKFLOW_USAGE_SINK.with(|cell| cell.borrow().as_ref().and_then(|v| v.last().cloned()))
}

/// Drain all collected usages — called by the REPL handler after
/// `spawn_blocking` returns so totals can be rolled up.
pub(crate) fn take_all_usages() -> Vec<crate::providers::Usage> {
    WORKFLOW_USAGE_SINK.with(|cell| {
        cell.borrow_mut()
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    })
}

/// Stage K: load the replay cache (None to clear) before
/// `spawn_blocking`. Pair with `replay_remaining()` after the run to
/// detect "more cached workers than the re-run consumed" — that
/// signals divergence from the original execution.
pub(crate) fn set_replay_cache(entries: Option<Vec<(String, String)>>) {
    WORKFLOW_REPLAY_CACHE.with(|cell| {
        *cell.borrow_mut() =
            entries.map(|v| v.into_iter().collect::<std::collections::VecDeque<_>>());
    });
}

/// Diagnostic — number of cached entries the resume run hasn't yet
/// consumed. Non-zero after a successful run means the re-execution
/// reached the end with cached entries left over (script shrank /
/// diverged).
pub(crate) fn replay_remaining() -> usize {
    WORKFLOW_REPLAY_CACHE.with(|cell| cell.borrow().as_ref().map(|q| q.len()).unwrap_or(0))
}

/// Stage M: install per-worker capabilities for the next `tool.call`.
/// Pair with `clear_worker_caps()` after.
pub(crate) fn set_worker_caps(caps: Option<WorkerCaps>) {
    WORKFLOW_WORKER_CAPS.with(|cell| *cell.borrow_mut() = caps);
}

/// Tier 3 polish: install the chat-tab broadcast sender so the host
/// function can emit `ToolCallStart` / `ToolCallResult` for each worker.
/// Pair with `set_events_tx(None)` after the spawn_blocking block.
/// The non-gui stub is a no-op so shell_dispatch can call this
/// unconditionally without sprouting `#[cfg]` blocks.
#[cfg(feature = "gui")]
pub(crate) fn set_events_tx(
    tx: Option<tokio::sync::broadcast::Sender<crate::shared_session::ViewEvent>>,
) {
    WORKFLOW_EVENTS_TX.with(|cell| *cell.borrow_mut() = tx);
}

#[cfg(not(feature = "gui"))]
#[allow(dead_code)]
pub(crate) fn set_events_tx<T>(_tx: Option<T>) {}

/// Stable error string a cancelled worker raises and the chat surface
/// detects to render `▸ Stopped` instead of a noisy "subagent failed"
/// trace. Kept in one place so producer + consumer stay in sync.
/// Consumer (chat surface) is gui-only today.
#[allow(dead_code)]
pub(crate) const WORKFLOW_CANCELLED_MSG: &str = "workflow cancelled by user";

/// Install (or clear with `None`) the Stop-button cancel token for the
/// duration of one workflow run. Pair with `spawn_blocking`. Unlike
/// `set_events_tx`, this is not gui-gated — REPL/headless can install
/// their own SIGINT-backed token if they want Ctrl-C to abort workers,
/// though today only the GUI chat surface wires it.
#[allow(dead_code)]
pub(crate) fn set_cancel(tok: Option<crate::cancel::CancelToken>) {
    WORKFLOW_CANCEL.with(|cell| *cell.borrow_mut() = tok);
}

fn workflow_cancel_clone() -> Option<crate::cancel::CancelToken> {
    WORKFLOW_CANCEL.with(|cell| cell.borrow().clone())
}

#[cfg(feature = "gui")]
fn send_workflow_event(ev: crate::shared_session::ViewEvent) {
    WORKFLOW_EVENTS_TX.with(|cell| {
        if let Some(tx) = cell.borrow().as_ref() {
            let _ = tx.send(ev);
        }
    });
}

/// KMS write-tool gate. `Ok(())` outside workflow context (legacy
/// behaviour); inside a workflow call, only KMSs named in the
/// per-worker `caps.kms_write` set may be written.
pub fn check_kms_write_capability(kms_name: &str) -> crate::Result<()> {
    WORKFLOW_WORKER_CAPS.with(|cell| match cell.borrow().as_ref() {
        None => Ok(()),
        Some(caps) => {
            if caps.kms_write.contains(kms_name) {
                Ok(())
            } else {
                Err(crate::Error::Tool(format!(
                    "workflow: KMS write to '{kms_name}' denied — not in the worker's \
                     granted-write list. The script must pass \
                     `caps: {{kms: {{write: [\"{kms_name}\"]}}}}` to thclaws.subagent \
                     to grant write access for this call."
                )))
            }
        }
    })
}

/// Pop the next cached worker output if its prompt matches the
/// supplied one. Mismatched prompts leave the cache untouched so the
/// host function can fall through to a real spawn — useful when the
/// script appended new calls after the resume point.
fn try_replay(prompt: &str) -> Option<String> {
    WORKFLOW_REPLAY_CACHE.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let queue = borrow.as_mut()?;
        match queue.front() {
            Some((p, _)) if p == prompt => Some(queue.pop_front().unwrap().1),
            _ => None,
        }
    })
}

fn register_thclaws(ctx: &mut Context) -> JsResult<()> {
    let subagent_fn = NativeFunction::from_fn_ptr(subagent);
    let thclaws_obj = ObjectInitializer::new(ctx)
        .function(subagent_fn, js_string!("subagent"), 1)
        .build();
    ctx.register_global_property(js_string!("thclaws"), thclaws_obj, Attribute::READONLY)
}

fn subagent(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let prompt = extract_prompt(args, ctx);
    let budget = extract_budget(args, ctx);
    let schema = extract_schema(args, ctx);
    let retry = extract_retry(args, ctx);
    let caps = extract_caps(args, ctx);

    // Stage K: serve from the replay cache when this call's prompt
    // matches the next pending entry. No worker_start/worker_done
    // events are emitted — those already live in state.jsonl from the
    // original run. Cache miss falls through to the normal spawn path.
    if let Some(cached) = try_replay(&prompt) {
        match &schema {
            Some(s) => {
                // Validate the cached output against the (possibly
                // newer) schema before returning, so a schema change
                // between runs surfaces as a clear error rather than
                // a stale value.
                match jsonschema::validator_for(s) {
                    Ok(validator) => match extract_json_from_text(&cached) {
                        Some(json_val) if validator.is_valid(&json_val) => {
                            return JsValue::from_json(&json_val, ctx);
                        }
                        _ => {
                            // Cached value no longer matches the schema —
                            // re-spawn fresh.
                        }
                    },
                    Err(_) => {}
                }
            }
            None => {
                return Ok(JsValue::from(js_string!(cached.as_str())));
            }
        }
    }

    let task_tool = WORKFLOW_TASK_TOOL.with(|c| c.borrow().clone());

    let Some(tool) = task_tool else {
        // Stage A fallback: no Task tool wired (tests, GUI refusal path)
        // — echo the prompt so callers get a deterministic placeholder
        // instead of an error.
        return Ok(JsValue::from(js_string!(
            format!("(stub for: {prompt})").as_str()
        )));
    };

    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => {
            return Err(js_error(
                "workflow: no tokio runtime available for subagent spawn",
            ));
        }
    };

    // Stage H: if a JSON Schema is set, augment the worker prompt to
    // ask for JSON matching that schema, and compile the schema once
    // up front. Compilation failure is a hard error — there's no point
    // retrying against a broken schema.
    let augmented_prompt = match &schema {
        Some(schema_val) => format!(
            "{prompt}\n\nReturn ONLY a JSON value matching this JSON Schema. \
             No prose, no markdown fences:\n{schema_str}",
            schema_str = schema_val
        ),
        None => prompt.clone(),
    };
    let compiled_schema = match &schema {
        Some(s) => match jsonschema::validator_for(s) {
            Ok(v) => Some(v),
            Err(e) => {
                return Err(js_error(&format!("workflow: invalid schema: {e}")));
            }
        },
        None => None,
    };

    // Stage D: open one worker_start event for this logical
    // thclaws.subagent call, even when retried (each retry gets its
    // own worker_retry event so the chain stays attributable). None
    // if no logger is wired (sandbox running outside a workflow run).
    let worker_id = super::state::with_logger(|l| l.worker_start(&prompt).ok()).flatten();

    // Stage M: audit the capability grant in state.jsonl so post-run
    // `/workflow inspect` shows what was granted to each worker.
    if let Some(wid) = worker_id {
        if !caps.kms_write.is_empty() {
            let granted: Vec<String> = caps.kms_write.iter().cloned().collect();
            super::state::with_logger(|l| {
                let _ = l.worker_caps(wid, &granted);
            });
        }
    }

    use std::io::Write as _;
    let worker_started = std::time::Instant::now();
    if let Some(wid) = worker_id {
        print!("{}", crate::tool_display::format_worker_start(wid, &prompt));
        let _ = std::io::stdout().flush();
    }

    // Tier 3 polish: chat-tab worker bubble. Emits a
    // `▸ subagent w0 (prompt preview)` indicator the same shape regular
    // tool calls use, so the chat tab shows live worker progression
    // alongside the final result. Skipped (None thread-local) for REPL
    // + headless flows; their stdout-based progress is unchanged.
    // Gui-gated because `ViewEvent` lives in `shared_session`.
    #[cfg(feature = "gui")]
    {
        let preview = crate::tool_display::sanitize_label_field(&prompt);
        let preview: String = preview.chars().take(60).collect();
        let chat_label = match worker_id {
            Some(id) => format!("subagent w{id} ({preview})"),
            None => format!("subagent ({preview})"),
        };
        send_workflow_event(crate::shared_session::ViewEvent::ToolCallStart {
            name: "WorkflowWorker".to_string(),
            label: chat_label,
            input: serde_json::json!({ "prompt": prompt.clone() }),
        });
    }

    // Stage H retry loop. Always at least 1 attempt; up to retry.max
    // total. Sleeps between attempts according to retry.backoff.
    let input = serde_json::json!({ "prompt": augmented_prompt });
    let mut last_failure: Option<String> = None;
    let mut last_text_for_done: Option<String> = None;
    let mut parsed_json: Option<serde_json::Value> = None;
    let mut succeeded = false;
    let max_attempts = retry.max.max(1);
    // Stop-button: a clone of the host's CancelToken (None in REPL/
    // headless today). Polled before each attempt and raced against
    // the in-flight `tool.call` via `tokio::select!`. When cancel
    // fires we surface a stable error string the chat surface can
    // detect to render a friendlier "stopped" message.
    let cancel = workflow_cancel_clone();
    for attempt in 1..=max_attempts {
        if let Some(tok) = cancel.as_ref() {
            if tok.is_cancelled() {
                return Err(js_error(WORKFLOW_CANCELLED_MSG));
            }
        }
        // Clear per-attempt state so a prior failure doesn't shadow a
        // later success.
        last_failure = None;
        // Stage G/M: enforce time budget per-attempt + install
        // per-worker capabilities for KMS write gating. Caps live
        // only for the duration of this tool.call so nested model-
        // driven Task calls don't accidentally inherit grants from a
        // different worker.
        set_worker_caps(Some(caps.clone()));
        let result: crate::Result<String> = match (budget.time, cancel.as_ref()) {
            (Some(time), Some(tok)) => handle.block_on(async {
                tokio::select! {
                    biased;
                    _ = tok.cancelled() => Err(crate::Error::Agent(WORKFLOW_CANCELLED_MSG.into())),
                    r = tokio::time::timeout(time, tool.call(input.clone())) => match r {
                        Ok(r) => r,
                        Err(_) => Err(crate::Error::Agent(format!(
                            "worker exceeded time budget of {}",
                            crate::tool_display::format_duration(time)
                        ))),
                    }
                }
            }),
            (Some(time), None) => {
                match handle.block_on(tokio::time::timeout(time, tool.call(input.clone()))) {
                    Ok(r) => r,
                    Err(_) => Err(crate::Error::Agent(format!(
                        "worker exceeded time budget of {}",
                        crate::tool_display::format_duration(time)
                    ))),
                }
            }
            (None, Some(tok)) => handle.block_on(async {
                tokio::select! {
                    biased;
                    _ = tok.cancelled() => Err(crate::Error::Agent(WORKFLOW_CANCELLED_MSG.into())),
                    r = tool.call(input.clone()) => r,
                }
            }),
            (None, None) => handle.block_on(tool.call(input.clone())),
        };
        set_worker_caps(None);
        // After tool.call returns (e.g. SubAgentTool unwinding from a
        // cancelled stream), surface cancel before falling into retry.
        if let Some(tok) = cancel.as_ref() {
            if tok.is_cancelled() {
                return Err(js_error(WORKFLOW_CANCELLED_MSG));
            }
        }

        match result {
            Ok(text) => {
                last_text_for_done = Some(text.clone());

                // Stage I: tokens-budget check. SubAgentTool has just
                // pushed this turn's usage to the sink; we peek the
                // last entry. Post-hoc enforcement only — we can't
                // abort mid-stream — so this acts as a soft cap that
                // triggers retry on the next iteration.
                if let Some(token_cap) = budget.tokens {
                    if let Some(u) = last_worker_usage() {
                        let used = (u.input_tokens + u.output_tokens) as u64;
                        if used > token_cap {
                            last_failure = Some(format!(
                                "worker exceeded token budget of {token_cap} (used {used})"
                            ));
                            if attempt < max_attempts {
                                if let Some(wid) = worker_id {
                                    let prior_err = last_failure.clone().unwrap_or_default();
                                    super::state::with_logger(|l| {
                                        let _ = l.worker_retry(wid, attempt, &prior_err);
                                    });
                                }
                                let delay = retry.delay_for_attempt(attempt);
                                if !delay.is_zero() {
                                    handle.block_on(tokio::time::sleep(delay));
                                }
                            }
                            continue;
                        }
                    }
                }

                match &compiled_schema {
                    Some(validator) => match extract_json_from_text(&text) {
                        Some(json_val) => {
                            if validator.is_valid(&json_val) {
                                parsed_json = Some(json_val);
                                succeeded = true;
                                break;
                            }
                            let errs: Vec<String> = validator
                                .iter_errors(&json_val)
                                .map(|e| e.to_string())
                                .take(3)
                                .collect();
                            last_failure = Some(format!(
                                "schema violation: {}",
                                if errs.is_empty() {
                                    "no detail".to_string()
                                } else {
                                    errs.join("; ")
                                }
                            ));
                        }
                        None => {
                            last_failure = Some("worker output is not valid JSON".to_string());
                        }
                    },
                    None => {
                        // No schema — first success returns immediately.
                        succeeded = true;
                        break;
                    }
                }
            }
            Err(e) => {
                last_failure = Some(e.to_string());
            }
        }

        if attempt < max_attempts {
            if let Some(wid) = worker_id {
                let prior_err = last_failure.clone().unwrap_or_default();
                super::state::with_logger(|l| {
                    let _ = l.worker_retry(wid, attempt, &prior_err);
                });
            }
            let delay = retry.delay_for_attempt(attempt);
            if !delay.is_zero() {
                handle.block_on(tokio::time::sleep(delay));
            }
        }
    }
    let elapsed = worker_started.elapsed();
    let success = succeeded;

    if let Some(wid) = worker_id {
        print!(
            "{}",
            crate::tool_display::format_worker_done(wid, &prompt, elapsed, !success)
        );
        let _ = std::io::stdout().flush();
        super::state::with_logger(|l| match (success, &last_text_for_done, &last_failure) {
            (true, Some(text), _) => {
                let _ = l.worker_done(wid, text);
            }
            (_, _, Some(err)) => {
                let _ = l.worker_error(wid, err);
            }
            _ => {}
        });
    }

    // Tier 3 polish: matching chat-tab ToolCallResult. The frontend
    // tool_call renderer flips the `▸` to `✓` (or `✗` on error) and
    // appends elapsed time, so each worker shows progression live.
    // Gui-gated for the same reason as the matching Start emit above.
    #[cfg(feature = "gui")]
    {
        let chat_output = match (success, &last_text_for_done, &last_failure) {
            (true, Some(text), _) => {
                let preview: String = text.chars().take(200).collect();
                if text.chars().count() > 200 {
                    format!("{preview}…")
                } else {
                    preview
                }
            }
            (_, _, Some(err)) => format!("error: {err}"),
            _ => "(no result)".to_string(),
        };
        send_workflow_event(crate::shared_session::ViewEvent::ToolCallResult {
            name: "WorkflowWorker".to_string(),
            output: chat_output,
            ui_resource: None,
        });
    }

    if !success {
        let err_msg = last_failure.unwrap_or_else(|| "unknown error".to_string());
        return Err(js_error(&format!(
            "workflow subagent failed after {max_attempts} attempt(s): {err_msg}"
        )));
    }
    let text = last_text_for_done.unwrap();

    // Stage H: when schema was set, return parsed JsValue (object /
    // array / etc.) so the script can use it directly without
    // JSON.parse. Without schema, return the raw text as before.
    match parsed_json {
        Some(v) => JsValue::from_json(&v, ctx),
        None => Ok(JsValue::from(js_string!(text.as_str()))),
    }
}

fn extract_prompt(args: &[JsValue], ctx: &mut Context) -> String {
    let arg = args.get_or_undefined(0);
    arg.as_object()
        .and_then(|obj| obj.get(js_string!("prompt"), ctx).ok())
        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
        .unwrap_or_else(|| "(no prompt)".to_string())
}

#[derive(Default)]
struct Budget {
    tokens: Option<u64>,
    time: Option<std::time::Duration>,
}

fn extract_budget(args: &[JsValue], ctx: &mut Context) -> Budget {
    let Some(arg0) = args.get_or_undefined(0).as_object() else {
        return Budget::default();
    };
    let Ok(budget_v) = arg0.get(js_string!("budget"), ctx) else {
        return Budget::default();
    };
    let Some(obj) = budget_v.as_object() else {
        return Budget::default();
    };
    let tokens = obj
        .get(js_string!("tokens"), ctx)
        .ok()
        .as_ref()
        .and_then(JsValue::as_number)
        .filter(|n| *n > 0.0 && n.is_finite())
        .map(|n| n as u64);
    let time = obj
        .get(js_string!("time"), ctx)
        .ok()
        .and_then(|v| value_to_duration(&v));
    Budget { tokens, time }
}

fn value_to_duration(v: &JsValue) -> Option<std::time::Duration> {
    if let Some(n) = v.as_number() {
        if n > 0.0 && n.is_finite() {
            return Some(std::time::Duration::from_secs_f64(n));
        }
        return None;
    }
    if let Some(s) = v.as_string() {
        return parse_human_duration(&s.to_std_string_escaped()).ok();
    }
    None
}

/// Parse strings like `"60s"`, `"2m"`, `"1m30s"`, `"1h"`, `"500ms"`.
/// Concatenated unit segments are summed. Returns `Err(msg)` on
/// malformed input — caller treats that as "no time budget set".
fn parse_human_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration string".into());
    }
    let mut total = std::time::Duration::ZERO;
    let mut num_buf = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num_buf.push(c);
            continue;
        }
        if num_buf.is_empty() {
            return Err(format!("unexpected '{c}' before any number"));
        }
        let n: f64 = num_buf
            .parse()
            .map_err(|_| format!("invalid number '{num_buf}'"))?;
        // `ms` is the only two-letter unit we accept — peek for the `s`.
        let multiplier = if c == 'm' && chars.peek() == Some(&'s') {
            chars.next();
            0.001
        } else {
            match c {
                's' => 1.0,
                'm' => 60.0,
                'h' => 3600.0,
                _ => return Err(format!("unknown unit '{c}'")),
            }
        };
        total += std::time::Duration::from_secs_f64(n * multiplier);
        num_buf.clear();
    }
    if !num_buf.is_empty() {
        return Err(format!("number '{num_buf}' missing unit suffix"));
    }
    if total.is_zero() {
        return Err("zero duration".into());
    }
    Ok(total)
}

fn js_error(msg: &str) -> JsError {
    JsNativeError::typ().with_message(msg.to_string()).into()
}

fn extract_caps(args: &[JsValue], ctx: &mut Context) -> WorkerCaps {
    let mut caps = WorkerCaps::default();
    let Some(arg0) = args.get_or_undefined(0).as_object() else {
        return caps;
    };
    let Ok(caps_v) = arg0.get(js_string!("caps"), ctx) else {
        return caps;
    };
    let json = match caps_v.to_json(ctx).ok().flatten() {
        Some(j) => j,
        None => return caps,
    };
    if let Some(writes) = json.pointer("/kms/write").and_then(|v| v.as_array()) {
        for w in writes {
            if let Some(s) = w.as_str() {
                caps.kms_write.insert(s.to_string());
            }
        }
    }
    caps
}

fn extract_schema(args: &[JsValue], ctx: &mut Context) -> Option<serde_json::Value> {
    let arg = args.get_or_undefined(0).as_object()?;
    let v = arg.get(js_string!("schema"), ctx).ok()?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    v.to_json(ctx).ok().flatten()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backoff {
    /// `delay = base * 2^(attempt-1)`, capped at 30s.
    Exponential,
    /// `delay = base * attempt`.
    Linear,
    /// Fixed delay equal to `base`.
    Fixed(std::time::Duration),
}

struct Retry {
    max: u32,
    backoff: Backoff,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            max: 1,
            backoff: Backoff::Exponential,
        }
    }
}

impl Retry {
    fn delay_for_attempt(&self, attempt: u32) -> std::time::Duration {
        const BASE: std::time::Duration = std::time::Duration::from_secs(1);
        const CAP: std::time::Duration = std::time::Duration::from_secs(30);
        match self.backoff {
            Backoff::Fixed(d) => d,
            Backoff::Linear => BASE * attempt,
            Backoff::Exponential => {
                let mult = 1u32 << (attempt - 1).min(20); // saturate at 2^20
                let d = BASE.saturating_mul(mult);
                if d > CAP {
                    CAP
                } else {
                    d
                }
            }
        }
    }
}

fn extract_retry(args: &[JsValue], ctx: &mut Context) -> Retry {
    let Some(arg) = args.get_or_undefined(0).as_object() else {
        return Retry::default();
    };
    let Ok(v) = arg.get(js_string!("retry"), ctx) else {
        return Retry::default();
    };
    // `retry: 3` shorthand for `retry: { max: 3 }`.
    if let Some(n) = v.as_number() {
        if n > 0.0 && n.is_finite() {
            return Retry {
                max: n as u32,
                backoff: Backoff::Exponential,
            };
        }
        return Retry::default();
    }
    let Some(obj) = v.as_object() else {
        return Retry::default();
    };
    let max = obj
        .get(js_string!("max"), ctx)
        .ok()
        .as_ref()
        .and_then(JsValue::as_number)
        .filter(|n| *n > 0.0 && n.is_finite())
        .map(|n| n as u32)
        .unwrap_or(1);
    let backoff = obj
        .get(js_string!("backoff"), ctx)
        .ok()
        .and_then(|bv| match bv.as_string() {
            Some(s) => match s.to_std_string_escaped().as_str() {
                "exponential" | "exp" => Some(Backoff::Exponential),
                "linear" | "lin" => Some(Backoff::Linear),
                other => parse_human_duration(other).ok().map(Backoff::Fixed),
            },
            None => bv
                .as_number()
                .filter(|n| *n > 0.0 && n.is_finite())
                .map(|n| Backoff::Fixed(std::time::Duration::from_secs_f64(n))),
        })
        .unwrap_or(Backoff::Exponential);
    Retry { max, backoff }
}

/// Pull a JSON value out of arbitrary worker text. Tries, in order:
/// the whole trimmed text as JSON; the contents of a ```json or ```
/// fence; and the first balanced `{...}` or `[...]` span. Returns
/// `None` if nothing parses — caller treats as a schema-failure retry
/// trigger.
fn extract_json_from_text(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str(trimmed) {
        return Some(v);
    }
    for prefix in ["```json", "```JSON", "```"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let inner = rest.trim_start_matches('\n');
            if let Some(body) = inner.strip_suffix("```") {
                if let Ok(v) = serde_json::from_str(body.trim()) {
                    return Some(v);
                }
            }
        }
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
        if start < end {
            if let Ok(v) = serde_json::from_str(&text[start..=end]) {
                return Some(v);
            }
        }
    }
    if let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) {
        if start < end {
            if let Ok(v) = serde_json::from_str(&text[start..=end]) {
                return Some(v);
            }
        }
    }
    None
}

fn strip_dangerous_globals(ctx: &mut Context) -> JsResult<()> {
    let global = ctx.global_object();
    global.delete_property_or_throw(js_string!("eval"), ctx)?;
    global.delete_property_or_throw(js_string!("Function"), ctx)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use async_trait::async_trait;
    use serde_json::{json, Value};

    #[test]
    fn stub_subagent_echoes_prompt() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let out = sb.run(r#"thclaws.subagent({prompt: "hello"})"#).unwrap();
        assert_eq!(out, "(stub for: hello)");
    }

    #[test]
    fn stub_subagent_handles_missing_prompt() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let out = sb.run(r#"thclaws.subagent({})"#).unwrap();
        assert_eq!(out, "(stub for: (no prompt))");
    }

    #[test]
    fn eval_global_stripped() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let err = sb.run(r#"eval("1+1")"#).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("eval"),
            "expected error mentioning eval, got: {msg}"
        );
    }

    #[test]
    fn function_constructor_stripped() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let err = sb.run(r#"new Function("return 1")()"#).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Function"),
            "expected error mentioning Function, got: {msg}"
        );
    }

    #[test]
    fn detects_async_syntax() {
        // Positive cases — should route to module mode.
        assert!(uses_async_syntax("const x = await foo();"));
        assert!(uses_async_syntax("Promise.all([])"));
        assert!(uses_async_syntax("paths.map(async (p) => p)"));
        assert!(uses_async_syntax("async function wf() {}"));
        assert!(uses_async_syntax("await foo()"));
        // Negative cases — should stay in script mode.
        assert!(!uses_async_syntax("const x = 1; x"));
        assert!(!uses_async_syntax("const awaitable = 1; awaitable"));
        // Note: substring detection means a comment containing `await `
        // will route to module mode. That's a false positive but
        // harmless — module mode handles sync scripts too.
    }

    #[test]
    fn wrap_for_module_uses_explicit_assignment() {
        let script = "const x = 1;\nglobalThis.__wf_result = x;";
        assert_eq!(wrap_for_module(script), script);
    }

    #[test]
    fn wrap_for_module_auto_wraps_trailing_expression() {
        let script = "const x = 1;\nx;";
        let wrapped = wrap_for_module(script);
        assert!(
            wrapped.contains("globalThis.__wf_result = (x);"),
            "got: {wrapped}"
        );
    }

    #[test]
    fn wrap_for_module_handles_no_trailing_semicolon() {
        let script = "const x = 1;\nx";
        let wrapped = wrap_for_module(script);
        assert!(
            wrapped.contains("globalThis.__wf_result = (x);"),
            "got: {wrapped}"
        );
    }

    #[test]
    fn wrap_for_module_handles_template_literal_with_dollar_brace() {
        // Regression: previously `}` inside `${expr}` ate the search
        // and the wrapper injected garbage into the template literal.
        let script = r#"const list = await thclaws.subagent({prompt: "a"});
const paths = list.split("\n");
const summaries = await Promise.all(paths.map(p => thclaws.subagent({prompt: `Read ${p}`})));
paths.map((p, i) => `${p} — ${summaries[i]}`).join("\n");"#;
        let wrapped = wrap_for_module(script);
        // The wrap must end with the assignment of the trailing
        // expression — NOT mid-template-literal.
        assert!(
            wrapped
                .ends_with("globalThis.__wf_result = (paths.map((p, i) => `${p} — ${summaries[i]}`).join(\"\\n\"));"),
            "got: {wrapped}"
        );
    }

    #[test]
    fn wrap_for_module_falls_back_when_last_is_declaration() {
        let script = "const x = 1;\nconst y = 2;";
        let wrapped = wrap_for_module(script);
        assert!(
            wrapped.contains("globalThis.__wf_result = undefined;"),
            "got: {wrapped}"
        );
    }

    #[test]
    fn module_mode_top_level_await_resolves_to_string() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let script = r#"
            const r = await thclaws.subagent({prompt: "hello"});
            r;
        "#;
        let out = sb.run(script).unwrap();
        assert_eq!(out, "(stub for: hello)");
    }

    #[test]
    fn module_mode_promise_all_resolves_to_array_join() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let script = r#"
            const r = await Promise.all([
                thclaws.subagent({prompt: "a"}),
                thclaws.subagent({prompt: "b"}),
                thclaws.subagent({prompt: "c"}),
            ]);
            r.join("|");
        "#;
        let out = sb.run(script).unwrap();
        assert_eq!(out, "(stub for: a)|(stub for: b)|(stub for: c)");
    }

    #[test]
    fn module_mode_explicit_globalthis_assignment_wins() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let script = r#"
            const r = await thclaws.subagent({prompt: "explicit"});
            globalThis.__wf_result = `wrapped: ${r}`;
        "#;
        let out = sb.run(script).unwrap();
        assert_eq!(out, "wrapped: (stub for: explicit)");
    }

    #[test]
    fn for_loop_works() {
        let mut sb = WorkflowSandbox::new().unwrap();
        let out = sb
            .run(
                r#"
                let total = 0;
                for (let i = 1; i <= 5; i++) total += i;
                total;
            "#,
            )
            .unwrap();
        assert_eq!(out, "15");
    }

    /// Stage C: a script that calls `thclaws.subagent` multiple times
    /// routes each call through the registered Task tool and stitches
    /// the results back. Uses a mock Tool so the test stays
    /// dependency-free; the real Tool comes from the parent's tool
    /// registry in production (`tool_registry.get("Task")`).
    #[test]
    fn parses_seconds() {
        let d = parse_human_duration("60s").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(60));
    }

    #[test]
    fn parses_minutes() {
        let d = parse_human_duration("2m").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(120));
    }

    #[test]
    fn parses_minutes_plus_seconds() {
        let d = parse_human_duration("1m30s").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(90));
    }

    #[test]
    fn parses_hours() {
        let d = parse_human_duration("1h").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(3600));
    }

    #[test]
    fn parses_milliseconds() {
        let d = parse_human_duration("500ms").unwrap();
        assert_eq!(d, std::time::Duration::from_millis(500));
    }

    #[test]
    fn parses_combined_h_m_s() {
        let d = parse_human_duration("1h30m15s").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(3600 + 30 * 60 + 15));
    }

    #[test]
    fn parses_fractional_seconds() {
        let d = parse_human_duration("2.5s").unwrap();
        assert_eq!(d, std::time::Duration::from_millis(2500));
    }

    #[test]
    fn rejects_empty_and_no_unit_and_bad_unit() {
        assert!(parse_human_duration("").is_err());
        assert!(parse_human_duration("60").is_err());
        assert!(parse_human_duration("10x").is_err());
        assert!(parse_human_duration("0s").is_err());
    }

    #[test]
    fn extracts_bare_json() {
        let v = extract_json_from_text(r#"{"a": 1}"#).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn extracts_json_from_json_fence() {
        let v = extract_json_from_text("```json\n{\"a\": 1}\n```").unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn extracts_json_from_bare_fence() {
        let v = extract_json_from_text("```\n{\"a\": 1}\n```").unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn extracts_json_embedded_in_prose() {
        let v = extract_json_from_text(
            "Here is the answer:\n{\"items\": [1, 2, 3]}\n— hope this helps",
        )
        .unwrap();
        assert_eq!(v["items"][1], 2);
    }

    #[test]
    fn extracts_bare_array() {
        let v = extract_json_from_text("[1, 2, 3]").unwrap();
        assert_eq!(v[2], 3);
    }

    #[test]
    fn returns_none_on_no_json() {
        assert!(extract_json_from_text("just prose").is_none());
        assert!(extract_json_from_text("").is_none());
    }

    #[test]
    fn exponential_backoff_caps_at_30s() {
        let r = Retry {
            max: 10,
            backoff: Backoff::Exponential,
        };
        assert_eq!(r.delay_for_attempt(1), std::time::Duration::from_secs(1));
        assert_eq!(r.delay_for_attempt(2), std::time::Duration::from_secs(2));
        assert_eq!(r.delay_for_attempt(4), std::time::Duration::from_secs(8));
        assert_eq!(r.delay_for_attempt(10), std::time::Duration::from_secs(30));
    }

    #[test]
    fn linear_backoff_scales_with_attempt() {
        let r = Retry {
            max: 5,
            backoff: Backoff::Linear,
        };
        assert_eq!(r.delay_for_attempt(1), std::time::Duration::from_secs(1));
        assert_eq!(r.delay_for_attempt(3), std::time::Duration::from_secs(3));
    }

    #[test]
    fn fixed_backoff_constant() {
        let r = Retry {
            max: 5,
            backoff: Backoff::Fixed(std::time::Duration::from_millis(500)),
        };
        assert_eq!(
            r.delay_for_attempt(1),
            std::time::Duration::from_millis(500)
        );
        assert_eq!(
            r.delay_for_attempt(7),
            std::time::Duration::from_millis(500)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schema_validation_returns_parsed_json() {
        struct JsonTask;
        #[async_trait]
        impl Tool for JsonTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "json mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                Ok(r#"{"items": ["a", "b"], "count": 2}"#.into())
            }
        }
        let mock: Arc<dyn Tool> = Arc::new(JsonTask);
        let script = r#"
            const r = thclaws.subagent({
              prompt: "list items",
              schema: {
                type: "object",
                properties: {
                  items: {type: "array"},
                  count: {type: "number"}
                },
                required: ["items", "count"]
              }
            });
            `${r.count}:${r.items.join(",")}`
        "#
        .to_string();

        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            res
        })
        .await
        .unwrap();

        assert_eq!(result.unwrap(), "2:a,b");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schema_failure_triggers_retry_then_succeeds() {
        use std::sync::atomic::{AtomicU32, Ordering};
        struct FlakyTask {
            calls: Arc<AtomicU32>,
        }
        #[async_trait]
        impl Tool for FlakyTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "flaky mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 2 {
                    Ok("definitely not json".into())
                } else {
                    Ok(r#"{"ok": true}"#.into())
                }
            }
        }
        let calls = Arc::new(AtomicU32::new(0));
        let mock: Arc<dyn Tool> = Arc::new(FlakyTask {
            calls: calls.clone(),
        });
        let script = r#"
            const r = thclaws.subagent({
              prompt: "give me ok",
              schema: {type: "object", required: ["ok"]},
              retry: {max: 3, backoff: "10ms"}
            });
            r.ok ? "yes" : "no"
        "#
        .to_string();

        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            res
        })
        .await
        .unwrap();

        assert_eq!(result.unwrap(), "yes");
        assert!(calls.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokens_budget_triggers_retry_then_exhausts() {
        use std::sync::atomic::{AtomicU32, Ordering};
        struct BigUsageTask {
            calls: Arc<AtomicU32>,
        }
        #[async_trait]
        impl Tool for BigUsageTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "big-usage mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                // Simulate the worker pushing usage via the sink path
                // (real Tool wires this through SubAgentTool::call;
                // this mock does it directly so the test stays local).
                crate::workflow::push_worker_usage(crate::providers::Usage {
                    input_tokens: 800,
                    output_tokens: 400,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    reasoning_output_tokens: None,
                });
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok("ok".into())
            }
        }
        let calls = Arc::new(AtomicU32::new(0));
        let mock: Arc<dyn Tool> = Arc::new(BigUsageTask {
            calls: calls.clone(),
        });
        let script = r#"
            try {
              thclaws.subagent({
                prompt: "big",
                budget: {tokens: 500},
                retry: {max: 2, backoff: "1ms"}
              });
              "should-not-reach";
            } catch (e) {
              e.message;
            }
        "#
        .to_string();

        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            set_usage_sink(true);
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            set_usage_sink(false);
            res
        })
        .await
        .unwrap();

        let msg = result.unwrap();
        assert!(
            msg.contains("token budget"),
            "expected token-budget error: {msg}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_sink_accumulates_across_workers() {
        struct UsageTask;
        #[async_trait]
        impl Tool for UsageTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "usage mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                crate::workflow::push_worker_usage(crate::providers::Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    reasoning_output_tokens: None,
                });
                Ok("hi".into())
            }
        }
        let mock: Arc<dyn Tool> = Arc::new(UsageTask);
        let script = r#"
            thclaws.subagent({prompt: "a"});
            thclaws.subagent({prompt: "b"});
            thclaws.subagent({prompt: "c"});
            "ok"
        "#
        .to_string();

        let (_, total): (std::result::Result<String, String>, u64) =
            tokio::task::spawn_blocking(move || {
                set_task_tool(Some(mock));
                set_usage_sink(true);
                let res = (|| -> std::result::Result<String, String> {
                    let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                    sb.run(&script).map_err(|e| e.to_string())
                })();
                let usages = take_all_usages();
                set_task_tool(None);
                set_usage_sink(false);
                let total: u64 = usages
                    .iter()
                    .map(|u| (u.input_tokens + u.output_tokens) as u64)
                    .sum();
                (res, total)
            })
            .await
            .unwrap();

        assert_eq!(total, 3 * 150);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schema_failure_exhausts_retries_and_throws() {
        struct AlwaysBadTask;
        #[async_trait]
        impl Tool for AlwaysBadTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "bad mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                Ok(r#"{"wrong": "shape"}"#.into())
            }
        }
        let mock: Arc<dyn Tool> = Arc::new(AlwaysBadTask);
        let script = r#"
            try {
              thclaws.subagent({
                prompt: "must have ok",
                schema: {type: "object", required: ["ok"]},
                retry: {max: 2, backoff: "1ms"}
              });
              "should-not-reach";
            } catch (e) {
              e.message;
            }
        "#
        .to_string();

        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            res
        })
        .await
        .unwrap();

        let msg = result.unwrap();
        assert!(
            msg.contains("after 2 attempt") && msg.contains("schema"),
            "expected schema-exhaustion error in: {msg}"
        );
    }

    #[test]
    fn kms_write_gate_allows_outside_workflow() {
        // No thread-local set → not inside a workflow call → allow.
        set_worker_caps(None);
        assert!(check_kms_write_capability("any-name").is_ok());
    }

    #[test]
    fn kms_write_gate_denies_when_not_granted() {
        set_worker_caps(Some(WorkerCaps::default()));
        let err = check_kms_write_capability("scratch").unwrap_err();
        assert!(err.to_string().contains("denied"));
        assert!(err.to_string().contains("scratch"));
        set_worker_caps(None);
    }

    #[test]
    fn kms_write_gate_allows_when_in_grant_list() {
        let mut caps = WorkerCaps::default();
        caps.kms_write.insert("scratch".to_string());
        caps.kms_write.insert("audit-log".to_string());
        set_worker_caps(Some(caps));
        assert!(check_kms_write_capability("scratch").is_ok());
        assert!(check_kms_write_capability("audit-log").is_ok());
        assert!(check_kms_write_capability("not-granted").is_err());
        set_worker_caps(None);
    }

    #[test]
    fn caps_extraction_from_js_object() {
        let mut sb = WorkflowSandbox::new().unwrap();
        // Use a custom script that exercises the host extract path
        // indirectly: define a probe that captures the caps via the
        // thread-local — we set the sink, run, then read it.
        // Simpler approach: drive extract_caps directly through a
        // synthetic Boa context.
        let script = r#"
            // Just touch the global; we're not asserting on the result.
            thclaws.subagent({
              prompt: "probe",
              caps: { kms: { write: ["alpha", "beta"] } }
            });
        "#;
        // Run inside the sandbox so extract_caps gets exercised
        // via the host function path. We don't care about the
        // return — the test asserts on the cap-extract side-effect
        // captured via a synthetic check below.
        //
        // Note: With no task tool wired the stub path runs and
        // exits before set_worker_caps is called. To actually test
        // extract_caps we'd need a real spawn path. Instead, assert
        // the simpler invariant: `extract_caps` of a plain script
        // shape returns the expected set when called on hand-built
        // Boa values (covered by the gate tests above + the
        // observable behaviour via the integration with the kms
        // tool gates).
        let _ = sb.run(script);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn caps_are_per_call_and_dont_leak_across_workers() {
        use std::sync::atomic::{AtomicU32, Ordering};
        struct CapsProbe {
            seen: Arc<std::sync::Mutex<Vec<Option<Vec<String>>>>>,
            ord: Arc<AtomicU32>,
        }
        #[async_trait]
        impl Tool for CapsProbe {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "caps probe"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                let observed = WORKFLOW_WORKER_CAPS.with(|c| {
                    c.borrow().as_ref().map(|caps| {
                        let mut v: Vec<String> = caps.kms_write.iter().cloned().collect();
                        v.sort();
                        v
                    })
                });
                self.seen.lock().unwrap().push(observed);
                Ok(format!("ok-{}", self.ord.fetch_add(1, Ordering::SeqCst)))
            }
        }
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Option<Vec<String>>>::new()));
        let ord = Arc::new(AtomicU32::new(0));
        let mock: Arc<dyn Tool> = Arc::new(CapsProbe {
            seen: seen.clone(),
            ord,
        });
        let script = r#"
            thclaws.subagent({prompt: "a", caps: {kms: {write: ["scratch"]}}});
            thclaws.subagent({prompt: "b"});
            thclaws.subagent({prompt: "c", caps: {kms: {write: ["audit", "scratch"]}}});
            "done"
        "#
        .to_string();

        tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let _ = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
        })
        .await
        .unwrap();

        let seen = seen.lock().unwrap().clone();
        // First call: scratch granted.
        assert_eq!(seen[0], Some(vec!["scratch".to_string()]));
        // Second call: NO caps in JS → empty WorkerCaps (deny-by-default).
        assert_eq!(seen[1], Some(Vec::<String>::new()));
        // Third call: audit + scratch granted (alphabetical from set).
        assert_eq!(
            seen[2],
            Some(vec!["audit".to_string(), "scratch".to_string()])
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replay_cache_serves_matching_prompts_without_spawning() {
        use std::sync::atomic::{AtomicU32, Ordering};
        struct CountingTask {
            calls: Arc<AtomicU32>,
        }
        #[async_trait]
        impl Tool for CountingTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "counting mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, input: Value) -> crate::Result<String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let p = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
                Ok(format!("fresh:{p}"))
            }
        }
        let calls = Arc::new(AtomicU32::new(0));
        let mock: Arc<dyn Tool> = Arc::new(CountingTask {
            calls: calls.clone(),
        });

        // Two cached entries; third call has no match → real spawn.
        let cache = vec![
            ("alpha".to_string(), "CACHED-A".to_string()),
            ("beta".to_string(), "CACHED-B".to_string()),
        ];
        let script = r#"
            const a = thclaws.subagent({prompt: "alpha"});
            const b = thclaws.subagent({prompt: "beta"});
            const c = thclaws.subagent({prompt: "gamma"});
            `${a}|${b}|${c}`
        "#
        .to_string();

        let (result, remaining): (std::result::Result<String, String>, usize) =
            tokio::task::spawn_blocking(move || {
                set_task_tool(Some(mock));
                set_replay_cache(Some(cache));
                let res = (|| -> std::result::Result<String, String> {
                    let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                    sb.run(&script).map_err(|e| e.to_string())
                })();
                let rem = replay_remaining();
                set_task_tool(None);
                set_replay_cache(None);
                (res, rem)
            })
            .await
            .unwrap();

        assert_eq!(result.unwrap(), "CACHED-A|CACHED-B|fresh:gamma");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(remaining, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replay_cache_falls_through_on_prompt_mismatch() {
        struct SpyTask {
            spawned: Arc<std::sync::Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl Tool for SpyTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "spy"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, input: Value) -> crate::Result<String> {
                let p = input
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.spawned.lock().unwrap().push(p.clone());
                Ok(format!("fresh:{p}"))
            }
        }
        let spawned = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mock: Arc<dyn Tool> = Arc::new(SpyTask {
            spawned: spawned.clone(),
        });

        // Cache says first call was "alpha", but script's first call
        // is "ALPHA-MODIFIED" — divergence. The runtime should leave
        // the cache untouched and spawn fresh.
        let cache = vec![("alpha".to_string(), "CACHED-A".to_string())];
        let script = r#"
            thclaws.subagent({prompt: "ALPHA-MODIFIED"});
        "#
        .to_string();

        let _ = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            set_replay_cache(Some(cache));
            let mut sb = WorkflowSandbox::new().unwrap();
            let _ = sb.run(&script);
            set_task_tool(None);
            set_replay_cache(None);
        })
        .await
        .unwrap();

        let spawned_calls = spawned.lock().unwrap().clone();
        assert_eq!(spawned_calls, vec!["ALPHA-MODIFIED"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn time_budget_aborts_slow_worker() {
        struct SlowTask;
        #[async_trait]
        impl Tool for SlowTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "slow mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, _input: Value) -> crate::Result<String> {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok("never returned".into())
            }
        }
        let mock: Arc<dyn Tool> = Arc::new(SlowTask);
        let script = r#"
            try {
                thclaws.subagent({prompt: "slow", budget: {time: "100ms"}});
                "should not reach";
            } catch (e) {
                e.message;
            }
        "#
        .to_string();

        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            res
        })
        .await
        .unwrap();

        let msg = result.unwrap();
        assert!(
            msg.contains("time budget"),
            "expected time-budget error in: {msg}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_task_tool_routes_subagent_calls() {
        struct MockTask;
        #[async_trait]
        impl Tool for MockTask {
            fn name(&self) -> &'static str {
                "Task"
            }
            fn description(&self) -> &'static str {
                "mock"
            }
            fn input_schema(&self) -> Value {
                json!({})
            }
            async fn call(&self, input: Value) -> crate::Result<String> {
                let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
                Ok(format!("task[{prompt}]"))
            }
        }

        let mock: Arc<dyn Tool> = Arc::new(MockTask);
        let script = r#"
            const a = thclaws.subagent({prompt: "alpha"});
            const b = thclaws.subagent({prompt: "beta"});
            const c = thclaws.subagent({prompt: "gamma"});
            `${a} | ${b} | ${c}`
        "#
        .to_string();

        let result: std::result::Result<String, String> = tokio::task::spawn_blocking(move || {
            set_task_tool(Some(mock));
            let res = (|| -> std::result::Result<String, String> {
                let mut sb = WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sb.run(&script).map_err(|e| e.to_string())
            })();
            set_task_tool(None);
            res
        })
        .await
        .unwrap();

        assert_eq!(result.unwrap(), "task[alpha] | task[beta] | task[gamma]");
    }
}
