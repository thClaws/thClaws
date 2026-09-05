//! `WorkflowRun` — model-callable wrapper around `/workflow run`.
//!
//! Authors a JavaScript orchestration script for the user-supplied
//! goal via [`crate::workflow::author`] (the same authoring path the
//! `/workflow run` slash command takes), then executes the script in
//! a Boa sandbox via [`crate::workflow::WorkflowSandbox`]. Returns
//! the script's final-expression string plus a one-line token rollup.
//!
//! Why a tool (not a slash command): users wanted the model to
//! reach for the workflow primitive on its own when a task looks
//! like deterministic fan-out — without needing to type `/workflow
//! run`. The slash command is preserved verbatim for the case where
//! the user wants the interactive author / review / re-author loop.
//!
//! ## Approval
//!
//! `requires_approval = true`. The tool executes JavaScript the LLM
//! authored against the workflow API — same blast radius as `Bash`
//! and warrants the same per-call gate. The approval prompt fires
//! BEFORE the authoring call, so a denied approval costs no provider
//! tokens.
//!
//! ## Nesting
//!
//! Rejected. If the model authors a workflow whose script invokes
//! `WorkflowRun` again via `thclaws.tool(...)`, the inner
//! `spawn_blocking` would stomp the outer's thread-locals (task_tool /
//! usage_sink) on unwind. `crate::workflow::is_inside_workflow()`
//! returns `true` once the outer sandbox sets the usage sink; we
//! check it at the top of `call` and bail.
//!
//! ## Subagent integration
//!
//! Scripts can call `thclaws.subagent(prompt)` to fan work out across
//! parallel side-quests. The runtime reads the Subagent tool from a
//! thread-local; we set that thread-local from the `task_tool`
//! captured at registration time. Without a `task_tool`, scripts that
//! call `thclaws.subagent(...)` fail with "Task tool not available" —
//! the same behaviour the `/workflow run` slash command exhibits when
//! the registry has no `Task` entry.
//!
//! ## Cancellation
//!
//! Inherits the caller's cancel posture. The agent loop that invoked
//! `WorkflowRun` propagates cancellation through `Cancel` events; the
//! `spawn_blocking` worker checks the workflow runtime's
//! `WORKFLOW_CANCELLED_MSG` on its own polling boundary. No extra
//! plumbing here.

use crate::error::{Error, Result};
use crate::providers::Provider;
use crate::tools::{req_str, Tool};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// Recover an `args` value a model serialized as a JSON *string* back into the
/// object/array it was meant to be. qwen and GLM (unlike deepseek) sometimes
/// pass the WorkflowRun `args` tool parameter as `"{\"query\":…}"` rather than a
/// nested object, so the script sees a string and `args.query` is undefined.
/// A genuine plain-string arg (not parseable as a JSON object/array) is left
/// untouched.
fn normalize_args(v: Value) -> Value {
    if let Value::String(s) = &v {
        if let Ok(parsed) = serde_json::from_str::<Value>(s.trim()) {
            if parsed.is_object() || parsed.is_array() {
                return parsed;
            }
        }
    }
    v
}

/// Hint appended to a successful result when a PRE-AUTHORED script that
/// references `args` was run with none. A model whose provider dropped the
/// nested `args` param (grok-4.5) otherwise sees a clean success and has to
/// guess why the output is empty/degenerate — this tells it what happened
/// and which parameter shapes fix it. Authored-from-prompt runs are exempt
/// (the author writes scripts around the absence of args).
fn argless_hint(scripted: bool, has_args: bool, script: &str) -> Option<&'static str> {
    (scripted && !has_args && script.contains("args")).then_some(
        "\n[hint: workflow ran with NO args, but the script references `args` — if it \
         expects input, re-invoke WorkflowRun with `args` (nested object) or, if your \
         tool-calling drops nested objects, `args_json` (the same payload as one JSON \
         string).]",
    )
}

pub struct WorkflowRunTool {
    provider: Arc<dyn Provider>,
    model: String,
    /// The Subagent (`Task`) tool, captured at registration so the
    /// sandbox's `thclaws.subagent(...)` host binding has something
    /// to dispatch through. `None` is legal — scripts that don't
    /// call `subagent` work fine; ones that do see the runtime's
    /// own "Task tool not available" error.
    task_tool: Option<Arc<dyn Tool>>,
    /// Chat broadcast sender so the sandbox's `thclaws.log(...)` narrator
    /// lines and per-worker `thclaws.subagent` progress render live in the
    /// chat while the script runs. Without it a multi-minute workflow (e.g.
    /// research) goes silent between the tool call and its result. Only the
    /// GUI / --serve shared session has one; CLI/headless registrations leave
    /// it `None` (progress still prints to stdout via `thclaws.log`).
    #[cfg(feature = "gui")]
    events_tx: Option<tokio::sync::broadcast::Sender<crate::shared_session::ViewEvent>>,
}

impl WorkflowRunTool {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: String,
        task_tool: Option<Arc<dyn Tool>>,
    ) -> Self {
        Self {
            provider,
            model,
            task_tool,
            #[cfg(feature = "gui")]
            events_tx: None,
        }
    }

    /// Wire the chat broadcast sender so workflow progress streams to the
    /// user mid-run. Call at the GUI / --serve registration site.
    #[cfg(feature = "gui")]
    pub fn with_events_tx(
        mut self,
        tx: tokio::sync::broadcast::Sender<crate::shared_session::ViewEvent>,
    ) -> Self {
        self.events_tx = Some(tx);
        self
    }
}

#[async_trait]
impl Tool for WorkflowRunTool {
    fn name(&self) -> &'static str {
        "WorkflowRun"
    }

    fn audit_kind(&self) -> crate::audit::AuditToolKind {
        crate::audit::AuditToolKind::Workflow
    }

    fn description(&self) -> &'static str {
        "Author and run a JavaScript orchestration workflow in a sandboxed Boa \
         runtime to handle deterministic fan-out, multistep pipelines, or \
         retry loops that exceed a single Subagent call. The model authors \
         the script (you don't write it — call this tool with a natural-\
         language goal and the workflow author produces JS); the script \
         executes against the `thclaws.*` runtime API (subagent fan-out, \
         logging, JSON-schema validation). Returns the script's final-\
         expression value as a string plus a one-line token rollup. \
         REQUIRES USER APPROVAL on each invocation. Nested `WorkflowRun` \
         calls (from inside a script) are rejected. Use when a task is \
         decomposable into N parallel side-quests; for one-off side \
         queries use the Subagent (`Task`) tool instead. To run a \
         PRE-AUTHORED workflow file shipped with an agent (e.g. \
         `.thclaws/state/workflows/draft-all-parallel.js`), pass `script_path` \
         instead of `prompt` — the file executes verbatim, no authoring. \
         Parameterize a pre-authored workflow via `args` (any JSON value, \
         exposed to the script as the global `args` — no TASK.md side-channel \
         needed: the script reads `args.query` directly)."
    }

    // Property ORDER is load-bearing: grok-4.5's constrained tool-call
    // decoding silently drops optional properties when the untyped `args`
    // sorts FIRST in the schema (live-bisected 2026-07-09: identical
    // schemas, authored order emitted args 5/5, alphabetized order 0/5 —
    // even when the user message said "pass EXACTLY this input"). serde_json
    // ships with `preserve_order` (Cargo.toml) so this map reaches the wire
    // as written: primary params first, args/args_json after. Keep property
    // descriptions to one line; long-form guidance lives in description().
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Natural-language goal; the workflow author writes the \
                                    JS. Ignored when script_path is given."
                },
                "script_path": {
                    "type": "string",
                    "description": "Workspace-relative path to a pre-authored workflow .js \
                                    file to execute verbatim."
                },
                "args": {
                    "description": "Structured input exposed to the script as the global \
                                    `args` (any JSON value), e.g. {\"query\":\"…\"}."
                },
                "args_json": {
                    "type": "string",
                    "description": "JSON-encoded alternative to `args` (one JSON string). \
                                    Ignored when `args` is present."
                }
            },
            "required": []
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let script_path = input
            .get("script_path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // Nested-call guard. The outer workflow's spawn_blocking has
        // set `WORKFLOW_USAGE_SINK`; running another sandbox inside
        // it would stomp those thread-locals on unwind. Cheap check;
        // bail before we burn tokens authoring.
        if crate::workflow::is_inside_workflow() {
            return Err(Error::Tool(
                "WorkflowRun cannot be invoked from inside a running workflow. \
                 The script you're currently executing should orchestrate the \
                 work directly via `thclaws.subagent(...)` / `thclaws.parallel(...)`."
                    .to_string(),
            ));
        }

        // Pre-authored file (agent-shipped workflows) executes
        // verbatim — mirrors `/workflow exec`. Sandbox: the path must
        // resolve inside the workspace.
        let script = if let Some(path) = script_path {
            let cwd = crate::workdir::current_workdir();
            let resolved = crate::sandbox::Sandbox::check_in(&cwd, path)
                .map_err(|e| Error::Tool(format!("script_path {path:?}: {e}")))?;
            std::fs::read_to_string(&resolved)
                .map_err(|e| Error::Tool(format!("read {path:?}: {e}")))?
        } else {
            // Author the script via the same path `/workflow run` takes.
            let prompt = req_str(&input, "prompt")?;
            crate::workflow::author(&*self.provider, &self.model, prompt, None)
                .await
                .map_err(|e| Error::Tool(format!("workflow author failed: {e}")))?
        };

        // Run in spawn_blocking so the Boa runtime doesn't block the
        // tokio executor. Thread-locals (task_tool + usage_sink) are
        // set inside the blocking worker so they live exactly as long
        // as the run, then unwind on Drop / explicit clear.
        let task_tool = self.task_tool.clone();
        let script_for_thread = script;
        // 47.3: structured input forwarded into the sandbox as `args`.
        // Robustness across models: some (qwen, GLM) serialize the `args` tool
        // parameter as a JSON *string* ("{\"query\":…}") instead of a nested
        // object, which would reach the script as a string where `args.query`
        // is undefined. If args is a string that parses to a JSON object/array,
        // use the parsed value; a genuine plain-string arg is left untouched.
        //
        // `args_json` is a flat-STRING escape hatch carrying the same payload,
        // parsed here into args; `args` wins when both are present. Kept as
        // belt-and-braces for providers that mangle nested object params —
        // the original grok-4.5 22/22 args drop turned out to be schema
        // property ORDER (see input_schema), not nested-object inability.
        let args_for_thread = input
            .get("args")
            .cloned()
            .or_else(|| {
                input
                    .get("args_json")
                    .and_then(Value::as_str)
                    .map(|s| Value::String(s.to_string()))
            })
            .map(normalize_args);
        let hint = argless_hint(
            script_path.is_some(),
            args_for_thread.is_some(),
            &script_for_thread,
        );
        // Install the chat broadcast sender on the blocking worker so the
        // script's thclaws.log / thclaws.subagent progress streams to the
        // chat mid-run (mirrors shell_dispatch's `/workflow run` path).
        #[cfg(feature = "gui")]
        let events_tx_for_thread = self.events_tx.clone();
        // Lifecycle breadcrumbs to stderr so a run that hangs or dies
        // silently is diagnosable from the terminal log: "starting" with no
        // matching "finished"/"FAILED" line pinpoints a hang; the per-worker
        // and thclaws.log lines in between (now streamed to chat) show which
        // stage stalled.
        eprintln!(
            "[workflow] run starting ({} bytes of script)",
            script_for_thread.len()
        );
        let run_started = std::time::Instant::now();
        let outcome: std::result::Result<
            (
                std::result::Result<String, String>,
                Vec<crate::providers::Usage>,
            ),
            tokio::task::JoinError,
        > = tokio::task::spawn_blocking(move || {
            crate::workflow::set_task_tool(task_tool);
            crate::workflow::set_usage_sink(true);
            #[cfg(feature = "gui")]
            crate::workflow::set_events_tx(events_tx_for_thread);
            let res = (|| -> std::result::Result<String, String> {
                let mut sandbox =
                    crate::workflow::WorkflowSandbox::new().map_err(|e| e.to_string())?;
                if let Some(av) = args_for_thread.as_ref() {
                    sandbox.set_args(av).map_err(|e| e.to_string())?;
                }
                sandbox.run(&script_for_thread).map_err(|e| e.to_string())
            })();
            let usages = crate::workflow::take_all_usages();
            crate::workflow::set_task_tool(None);
            crate::workflow::set_usage_sink(false);
            #[cfg(feature = "gui")]
            crate::workflow::set_events_tx(None);
            (res, usages)
        })
        .await;

        let elapsed = run_started.elapsed();
        let (result, all_usages) = match outcome {
            Ok((res, u)) => (res, u),
            Err(e) => {
                eprintln!("[workflow] run PANICKED after {elapsed:.1?}: {e}");
                return Err(Error::Tool(format!("workflow worker thread panicked: {e}")));
            }
        };

        let body = match result {
            Ok(text) => text,
            Err(e) => {
                eprintln!("[workflow] run FAILED after {elapsed:.1?}: {e}");
                return Err(Error::Tool(format!("workflow script failed: {e}")));
            }
        };
        eprintln!(
            "[workflow] run finished ok after {elapsed:.1?} ({} subagent turn(s))",
            all_usages.len()
        );

        // One-line token rollup so the model can see what the run
        // cost — mirrors the `/workflow run` REPL output shape.
        let total_in: u64 = all_usages.iter().map(|u| u.input_tokens as u64).sum();
        let total_out: u64 = all_usages.iter().map(|u| u.output_tokens as u64).sum();
        let summary = format!(
            "\n\n[workflow: {} subagent turn(s), {} in / {} out tokens]",
            all_usages.len(),
            total_in,
            total_out
        );
        Ok(format!("{body}{summary}{}", hint.unwrap_or_default()))
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_args;
    use serde_json::{json, Value};

    /// grok-4.5 escape hatch: `args_json` (flat string param) must resolve
    /// to the same value the nested `args` object would have produced, and
    /// `args` must win when both are present.
    #[test]
    fn args_json_string_param_resolves_like_args() {
        let resolve = |input: Value| -> Option<Value> {
            input
                .get("args")
                .cloned()
                .or_else(|| {
                    input
                        .get("args_json")
                        .and_then(Value::as_str)
                        .map(|s| Value::String(s.to_string()))
                })
                .map(normalize_args)
        };
        // args_json alone → parsed object.
        assert_eq!(
            resolve(json!({"args_json": r#"{"query":"OKF","min_iter":4}"#})),
            Some(json!({"query": "OKF", "min_iter": 4}))
        );
        // args wins over args_json.
        assert_eq!(
            resolve(json!({"args": {"query":"A"}, "args_json": r#"{"query":"B"}"#})),
            Some(json!({"query": "A"}))
        );
        // neither → None (workflow runs argless).
        assert_eq!(resolve(json!({"script_path": "x.js"})), None);
    }

    /// The argless hint fires only for a pre-authored script that references
    /// `args` and ran with none — authored-from-prompt runs and scripts that
    /// take no input stay hint-free.
    #[test]
    fn argless_hint_only_for_scripted_args_reading_runs() {
        let hint = super::argless_hint(true, false, "const q = args.query;");
        assert!(hint.is_some_and(|h| h.contains("args_json")));
        // args were passed → no hint.
        assert!(super::argless_hint(true, true, "const q = args.query;").is_none());
        // script never reads args → no hint.
        assert!(super::argless_hint(true, false, "'hi'").is_none());
        // authored from prompt (not script_path) → no hint.
        assert!(super::argless_hint(false, false, "const q = args.query;").is_none());
    }

    #[test]
    fn normalize_args_recovers_json_string_object() {
        // qwen/GLM: args passed as a JSON string → recover the object.
        assert_eq!(
            normalize_args(Value::String(r#"{"query":"OKF","kms":"okf"}"#.into())),
            json!({"query": "OKF", "kms": "okf"})
        );
        // stringified array likewise.
        assert_eq!(normalize_args(Value::String("[1,2]".into())), json!([1, 2]));
        // deepseek: already an object → unchanged.
        assert_eq!(normalize_args(json!({"query": "x"})), json!({"query": "x"}));
        // a genuine plain-string arg is not JSON-object/array → left as-is.
        assert_eq!(
            normalize_args(Value::String("hello".into())),
            Value::String("hello".into())
        );
        // a bare JSON scalar string ("42") stays a string, not a number.
        assert_eq!(
            normalize_args(Value::String("42".into())),
            Value::String("42".into())
        );
    }

    use super::*;
    use crate::providers::{EventStream, ProviderEvent, StreamRequest, Usage};
    use async_trait::async_trait;
    use futures::stream;

    /// Minimal provider that returns a fixed script for `author` calls.
    /// Used by the smoke test to avoid live network / API-key
    /// dependencies. Emits the script body as a single `TextDelta`
    /// followed by `MessageStop` — matches the real provider event
    /// stream the workflow author consumes.
    struct ScriptStubProvider {
        script: String,
    }

    #[async_trait]
    impl Provider for ScriptStubProvider {
        async fn stream(
            &self,
            _req: StreamRequest,
        ) -> std::result::Result<EventStream, crate::error::Error> {
            let script = self.script.clone();
            let events = vec![
                Ok(ProviderEvent::TextDelta(script)),
                Ok(ProviderEvent::MessageStop {
                    stop_reason: None,
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 20,
                        ..Default::default()
                    }),
                }),
            ];
            Ok(Box::pin(stream::iter(events)))
        }
    }

    /// Smoke: a trivial script (just returns `'hi'`) authored by the
    /// stub provider, executed in the real Boa sandbox, returns
    /// 'hi' plus the token rollup. Pins that the spawn_blocking +
    /// thread-local + WorkflowSandbox::new+run pipeline composes
    /// correctly when invoked from a tool's `call` rather than the
    /// slash-command handler.
    #[tokio::test]
    async fn workflow_run_executes_authored_script_and_returns_result() {
        let provider: Arc<dyn Provider> = Arc::new(ScriptStubProvider {
            script: "'hi'".to_string(),
        });
        let tool = WorkflowRunTool::new(provider, "test-model".to_string(), None);
        let out = tool
            .call(json!({"prompt": "say hi"}))
            .await
            .expect("workflow should succeed");
        assert!(out.starts_with("hi"), "got: {out}");
        assert!(out.contains("[workflow:"), "missing token rollup: {out}");
    }

    /// Pins that the tool rejects nesting. We can't easily set the
    /// thread-local from the async test thread (spawn_blocking sets
    /// it inside the blocking worker), so simulate by setting +
    /// reading the thread-local directly. The Err message must
    /// reference subagent / parallel — that's the user-facing guidance.
    #[test]
    fn nested_workflow_run_is_rejected_via_thread_local() {
        let provider: Arc<dyn Provider> = Arc::new(ScriptStubProvider {
            script: "'hi'".to_string(),
        });
        let tool = WorkflowRunTool::new(provider, "test-model".to_string(), None);
        crate::workflow::set_usage_sink(true);
        let result = futures::executor::block_on(tool.call(json!({"prompt": "nested"})));
        crate::workflow::set_usage_sink(false);
        let err = result.expect_err("nested call must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("inside a running workflow"),
            "expected nested-call error, got: {msg}"
        );
    }
}
