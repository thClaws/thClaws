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

pub struct WorkflowRunTool {
    provider: Arc<dyn Provider>,
    model: String,
    /// The Subagent (`Task`) tool, captured at registration so the
    /// sandbox's `thclaws.subagent(...)` host binding has something
    /// to dispatch through. `None` is legal — scripts that don't
    /// call `subagent` work fine; ones that do see the runtime's
    /// own "Task tool not available" error.
    task_tool: Option<Arc<dyn Tool>>,
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
        }
    }
}

#[async_trait]
impl Tool for WorkflowRunTool {
    fn name(&self) -> &'static str {
        "WorkflowRun"
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
         queries use the Subagent (`Task`) tool instead."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Natural-language goal for the workflow author. \
                                    Examples: 'summarise each .rs file under src/ in one line', \
                                    'run pytest, parse failures, and open issues for the new ones'."
                }
            },
            "required": ["prompt"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let prompt = req_str(&input, "prompt")?;

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

        // Author the script via the same path `/workflow run` takes.
        let script = crate::workflow::author(&*self.provider, &self.model, prompt, None)
            .await
            .map_err(|e| Error::Tool(format!("workflow author failed: {e}")))?;

        // Run in spawn_blocking so the Boa runtime doesn't block the
        // tokio executor. Thread-locals (task_tool + usage_sink) are
        // set inside the blocking worker so they live exactly as long
        // as the run, then unwind on Drop / explicit clear.
        let task_tool = self.task_tool.clone();
        let script_for_thread = script;
        let outcome: std::result::Result<
            (
                std::result::Result<String, String>,
                Vec<crate::providers::Usage>,
            ),
            tokio::task::JoinError,
        > = tokio::task::spawn_blocking(move || {
            crate::workflow::set_task_tool(task_tool);
            crate::workflow::set_usage_sink(true);
            let res = (|| -> std::result::Result<String, String> {
                let mut sandbox =
                    crate::workflow::WorkflowSandbox::new().map_err(|e| e.to_string())?;
                sandbox.run(&script_for_thread).map_err(|e| e.to_string())
            })();
            let usages = crate::workflow::take_all_usages();
            crate::workflow::set_task_tool(None);
            crate::workflow::set_usage_sink(false);
            (res, usages)
        })
        .await;

        let (result, all_usages) = match outcome {
            Ok((res, u)) => (res, u),
            Err(e) => {
                return Err(Error::Tool(format!("workflow worker thread panicked: {e}")));
            }
        };

        let body = match result {
            Ok(text) => text,
            Err(e) => return Err(Error::Tool(format!("workflow script failed: {e}"))),
        };

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
        Ok(format!("{body}{summary}"))
    }
}

#[cfg(test)]
mod tests {
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
