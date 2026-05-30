//! Sub-agent tool — spawn nested agents with depth tracking and
//! named agent definitions.
//!
//! Supports multi-level recursion up to `max_depth` (default 3).
//! Child agents include their own `Task` tool at `depth + 1`, so
//! they can delegate further. At max depth, the tool refuses.
//!
//! Named agents: if `agent` field is provided in the input, loads
//! the definition from `~/.config/thclaws/agents.json` and uses
//! its instructions, model override, and tool subset.

use crate::agent::{collect_agent_turn_with_cancel, Agent};
use crate::agent_defs::{AgentDef, AgentDefsConfig};
use crate::cancel::CancelToken;
use crate::error::{Error, Result};
use crate::permissions::{ApprovalSink, PermissionMode};
use crate::providers::Provider;
use crate::tools::{req_str, Tool, ToolRegistry};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

pub const TOOL_NAME: &str = "Task";
pub const DEFAULT_MAX_DEPTH: usize = 3;

/// How to construct a child agent. Implementations produce a brand-new
/// `Agent` with the appropriate configuration.
#[async_trait]
pub trait AgentFactory: Send + Sync {
    /// Build a child agent. `agent_def` is `Some` if the Task input
    /// specified a named agent; `None` for the default.
    async fn build(
        &self,
        prompt: &str,
        agent_def: Option<&AgentDef>,
        child_depth: usize,
    ) -> Result<Agent>;
}

/// M6.33: production agent factory shared by CLI (`run_repl`) and GUI
/// (`build_state`). Pre-fix the CLI had its own `ReplAgentFactory` and
/// the GUI had no factory at all (Task tool unregistered — SUB1).
/// Consolidated here so both surfaces get identical subagent behavior.
///
/// Fields capture the parent's runtime state for propagation to child
/// agents:
/// - `provider` / `model` — wire layer for the child's LLM calls
/// - `base_tools` — tool registry the child inherits (filtered by
///   agent_def.tools allow-list + agent_def.disallowed_tools deny-list
///   inside `build`)
/// - `system` — parent's full system prompt (CLAUDE.md + memory + KMS +
///   plan + todos), copied to the child + agent_def addendum + the
///   embedded `subagent.md` "you are a sub-agent" wording
/// - `max_iterations` — fallback when agent_def doesn't specify
/// - `max_depth` — recursion ceiling; child gets a Task tool only when
///   child_depth < max_depth
/// - `agent_defs` — registry of named agents (for nested Task calls)
/// - `approver` + `permission_mode` — M6.20 BUG H1: parent's gate
///   propagates so subagents can't silently bypass Ask mode
/// - `cancel` — M6.33 SUB4: parent's cancel token propagates so
///   ctrl-C reaches a runaway subagent. CLI passes `None` (no cancel
///   plumbing yet); GUI passes the worker's CancelToken.
pub struct ProductionAgentFactory {
    pub provider: Arc<dyn Provider>,
    pub base_tools: ToolRegistry,
    pub model: String,
    pub system: String,
    pub max_iterations: usize,
    pub max_depth: usize,
    /// Per-request output token budget propagated from `AppConfig::max_tokens`.
    /// Subagents inherit the parent's value so a project's `settings.json`
    /// `maxTokens` override applies uniformly. Issue #72: pre-fix subagents
    /// hit the hardcoded `Agent::new` default of 8192 even when the parent
    /// was correctly configured.
    pub max_tokens: u32,
    pub agent_defs: AgentDefsConfig,
    pub approver: Arc<dyn ApprovalSink>,
    pub permission_mode: PermissionMode,
    pub cancel: Option<CancelToken>,
    /// M6.35 HOOK1: lifecycle hooks propagate parent → subagent so a
    /// pre/post_tool_use hook fires for tool calls inside a Task spawn,
    /// not just at the top-level agent. Audit hooks would otherwise miss
    /// every subagent action — silent gap.
    pub hooks: Option<Arc<crate::hooks::HooksConfig>>,
}

#[async_trait]
impl AgentFactory for ProductionAgentFactory {
    async fn build(
        &self,
        _prompt: &str,
        agent_def: Option<&AgentDef>,
        child_depth: usize,
    ) -> Result<Agent> {
        let model = agent_def
            .and_then(|d| d.model.as_deref())
            .unwrap_or(&self.model);

        // System prompt: parent's full prompt + (optional) agent
        // instructions + (when nested) the subagent-mode addendum.
        let mut system = agent_def
            .map(|d| {
                if d.instructions.is_empty() {
                    self.system.clone()
                } else {
                    format!(
                        "{}\n\n# Agent instructions\n{}",
                        self.system, d.instructions
                    )
                }
            })
            .unwrap_or_else(|| self.system.clone());
        if child_depth > 0 {
            system.push_str(&crate::prompts::load(
                "subagent",
                crate::prompts::defaults::SUBAGENT,
            ));
        }
        let max_iter = agent_def
            .map(|d| d.max_iterations)
            .unwrap_or(self.max_iterations);

        // Tool registry: agent_def.tools allow-list (when non-empty)
        // intersects base_tools, then agent_def.disallowed_tools
        // deny-list removes anything in it. M6.33 SUB2: pre-fix
        // disallowed_tools was parsed but never applied — agent
        // definitions claiming `disallowed_tools: Bash` got Bash anyway.
        let mut tools = if let Some(def) = agent_def {
            if def.tools.is_empty() {
                self.base_tools.clone()
            } else {
                let mut filtered = ToolRegistry::new();
                for name in &def.tools {
                    if let Some(tool) = self.base_tools.get(name) {
                        filtered.register(tool);
                    }
                }
                filtered
            }
        } else {
            self.base_tools.clone()
        };
        if let Some(def) = agent_def {
            for name in &def.disallowed_tools {
                tools.remove(name);
            }
        }

        // Add a Task tool at the next depth (multi-level recursion).
        // child_depth < max_depth → register; otherwise the leaf
        // subagent has no Task tool and the chain stops.
        if child_depth < self.max_depth {
            let child_factory = Arc::new(ProductionAgentFactory {
                provider: self.provider.clone(),
                base_tools: self.base_tools.clone(),
                model: self.model.clone(),
                system: self.system.clone(),
                max_iterations: self.max_iterations,
                max_depth: self.max_depth,
                max_tokens: self.max_tokens,
                agent_defs: self.agent_defs.clone(),
                approver: self.approver.clone(),
                permission_mode: self.permission_mode,
                cancel: self.cancel.clone(),
                hooks: self.hooks.clone(),
            });
            let mut child_tool = SubAgentTool::new(child_factory)
                .with_depth(child_depth)
                .with_max_depth(self.max_depth)
                .with_agent_defs(self.agent_defs.clone());
            if let Some(c) = self.cancel.clone() {
                child_tool = child_tool.with_cancel(c);
            }
            tools.register(Arc::new(child_tool));
        }

        // M6.33 SUB4: thread parent's cancel token into the child agent
        // so retry-backoff sleeps + collect_agent_turn observe ctrl-C.
        let mut agent = Agent::new(self.provider.clone(), tools, model, &system)
            .with_max_iterations(max_iter)
            .with_max_tokens(self.max_tokens)
            .with_approver(self.approver.clone())
            .with_permission_mode(self.permission_mode);
        if let Some(c) = self.cancel.clone() {
            agent = agent.with_cancel(c);
        }
        // M6.35 HOOK1: subagent inherits parent's hooks so audit hooks
        // see Task-spawned tool calls too.
        if let Some(h) = self.hooks.clone() {
            agent = agent.with_hooks(h);
        }
        Ok(agent)
    }
}

pub struct SubAgentTool {
    factory: Arc<dyn AgentFactory>,
    depth: usize,
    max_depth: usize,
    /// Agent definitions loaded at startup.
    agent_defs: crate::agent_defs::AgentDefsConfig,
    /// M6.33 SUB4: parent's cancel token. Observed by
    /// `collect_agent_turn_with_cancel` so ctrl-C reaches a runaway
    /// subagent. None when no parent cancel is wired (CLI today).
    cancel: Option<CancelToken>,
}

impl SubAgentTool {
    pub fn new(factory: Arc<dyn AgentFactory>) -> Self {
        Self {
            factory,
            depth: 0,
            max_depth: DEFAULT_MAX_DEPTH,
            agent_defs: crate::agent_defs::AgentDefsConfig::load_with_extra(
                &crate::plugins::plugin_agent_dirs(),
            ),
            cancel: None,
        }
    }

    pub fn with_depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    pub fn with_agent_defs(mut self, defs: crate::agent_defs::AgentDefsConfig) -> Self {
        self.agent_defs = defs;
        self
    }

    /// M6.33 SUB4: wire a cancel token. The token is observed inside
    /// `collect_agent_turn_with_cancel` so a parent ctrl-C / `/cancel`
    /// short-circuits the subagent's stream instead of waiting for it
    /// to run to completion.
    pub fn with_cancel(mut self, token: CancelToken) -> Self {
        self.cancel = Some(token);
        self
    }
}

#[async_trait]
impl Tool for SubAgentTool {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Launch a sub-agent with its own history to handle a bounded subtask. \
         The sub-agent runs independently, may call tools (and spawn further \
         sub-agents up to the recursion limit), and returns its final response \
         as text. Use `agent` to pick a named agent definition from agents.json."
    }

    fn input_schema(&self) -> Value {
        let mut agent_names = self.agent_defs.names();
        agent_names.sort();
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Short label for the sub-task (shown in logs)."
                },
                "prompt": {
                    "type": "string",
                    "description": "The full instruction for the sub-agent."
                },
                "agent": {
                    "type": "string",
                    "description": format!(
                        "Optional named agent from agents.json. Available: {}",
                        if agent_names.is_empty() { "none configured".to_string() }
                        else { agent_names.join(", ") }
                    )
                }
            },
            "required": ["prompt"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        if self.depth >= self.max_depth {
            return Err(Error::Agent(format!(
                "sub-agent recursion limit reached (depth {}/{})",
                self.depth, self.max_depth
            )));
        }

        let prompt = req_str(&input, "prompt")?.to_string();
        let agent_name = input.get("agent").and_then(Value::as_str);

        // Look up named agent definition if specified.
        let agent_def = agent_name.and_then(|name| self.agent_defs.get(name));
        if agent_name.is_some() && agent_def.is_none() {
            let available = self.agent_defs.names().join(", ");
            return Err(Error::Agent(format!(
                "unknown agent '{}'. Available: {}",
                agent_name.unwrap(),
                if available.is_empty() {
                    "none"
                } else {
                    &available
                }
            )));
        }

        let child_depth = self.depth + 1;
        let agent = self.factory.build(&prompt, agent_def, child_depth).await?;
        let stream = agent.run_turn(prompt);
        // M6.33 SUB4: collect_agent_turn_with_cancel observes the
        // parent's cancel token between stream events. Pre-fix the
        // subagent stream ran to completion regardless of ctrl-C.
        let outcome = collect_agent_turn_with_cancel(stream, self.cancel.clone()).await?;

        // dev-plan/32 Stage I: push this turn's Usage to the workflow
        // usage sink if one is active on this thread. No-op outside
        // `/workflow run` — model-driven Task calls and tests stay
        // unaffected.
        if let Some(u) = outcome.usage.as_ref() {
            crate::workflow::push_worker_usage(u.clone());
        }

        if outcome.text.is_empty() {
            Err(Error::Agent("sub-agent returned empty response".into()))
        } else {
            Ok(outcome.text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentEvent;
    use crate::error::Error;
    use crate::providers::{EventStream, Provider, ProviderEvent, StreamRequest};
    use crate::tools::ToolRegistry;
    use async_trait::async_trait;
    use futures::stream;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct ScriptedProvider {
        scripts: Arc<Mutex<VecDeque<Vec<ProviderEvent>>>>,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Vec<ProviderEvent>>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Arc::new(Mutex::new(VecDeque::from(scripts))),
            })
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn stream(&self, _req: StreamRequest) -> Result<EventStream> {
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::Provider("no more scripts".into()))?;
            let events: Vec<Result<ProviderEvent>> = script.into_iter().map(Ok).collect();
            Ok(Box::pin(stream::iter(events)))
        }
    }

    fn text_script(chunks: &[&str]) -> Vec<ProviderEvent> {
        let mut out = vec![ProviderEvent::MessageStart {
            model: "test".into(),
        }];
        for c in chunks {
            out.push(ProviderEvent::TextDelta((*c).to_string()));
        }
        out.push(ProviderEvent::ContentBlockStop);
        out.push(ProviderEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        });
        out
    }

    struct SimpleFactory {
        scripts: Arc<Mutex<Vec<Vec<Vec<ProviderEvent>>>>>,
    }

    impl SimpleFactory {
        fn new(scripts: Vec<Vec<Vec<ProviderEvent>>>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Arc::new(Mutex::new(scripts)),
            })
        }
    }

    #[async_trait]
    impl AgentFactory for SimpleFactory {
        async fn build(
            &self,
            _prompt: &str,
            _def: Option<&AgentDef>,
            _depth: usize,
        ) -> Result<Agent> {
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| Error::Agent("factory exhausted".into()))?;
            let provider = ScriptedProvider::new(script);
            Ok(Agent::new(provider, ToolRegistry::new(), "test", ""))
        }
    }

    #[tokio::test]
    async fn sub_agent_returns_text() {
        let factory = SimpleFactory::new(vec![vec![text_script(&["done"])]]);
        let tool = SubAgentTool::new(factory);
        let out = tool.call(json!({"prompt": "go"})).await.unwrap();
        assert_eq!(out, "done");
    }

    #[tokio::test]
    async fn depth_limit_enforced() {
        let factory = SimpleFactory::new(vec![]);
        let tool = SubAgentTool::new(factory).with_depth(3).with_max_depth(3);
        let err = tool.call(json!({"prompt": "go"})).await.unwrap_err();
        assert!(format!("{err}").contains("recursion limit"));
    }

    #[tokio::test]
    async fn unknown_agent_errors() {
        let factory = SimpleFactory::new(vec![]);
        let tool = SubAgentTool::new(factory);
        let err = tool
            .call(json!({"prompt": "go", "agent": "nonexistent"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("unknown agent"));
    }

    struct EchoTool {
        name: &'static str,
    }
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "echo"
        }
        fn input_schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn call(&self, _input: Value) -> Result<String> {
            Ok(String::new())
        }
    }

    struct StubProvider;
    #[async_trait]
    impl Provider for StubProvider {
        async fn stream(&self, _r: StreamRequest) -> Result<EventStream> {
            Ok(Box::pin(stream::iter(vec![Ok(
                ProviderEvent::MessageStart {
                    model: "test".into(),
                },
            )])))
        }
    }

    /// M6.33 SUB2: agent_def.disallowed_tools must be honored. Pre-fix
    /// the field was parsed but never applied — agent definitions
    /// claiming `disallowed_tools: Bash` got Bash anyway.
    #[tokio::test]
    async fn production_factory_applies_agent_def_disallowed_tools() {
        let mut base = ToolRegistry::new();
        base.register(Arc::new(EchoTool { name: "Bash" }));
        base.register(Arc::new(EchoTool { name: "Read" }));

        let factory = ProductionAgentFactory {
            provider: Arc::new(StubProvider),
            base_tools: base,
            model: "test".into(),
            system: String::new(),
            max_iterations: 1,
            max_depth: 3,
            max_tokens: 8192,
            agent_defs: AgentDefsConfig::default(),
            approver: Arc::new(crate::permissions::DenyApprover),
            permission_mode: PermissionMode::Auto,
            cancel: None,
            hooks: None,
        };
        let def = AgentDef {
            name: "restricted".into(),
            disallowed_tools: vec!["Bash".into()],
            ..Default::default()
        };
        let child = factory.build("go", Some(&def), 1).await.unwrap();
        let names = child.tools.names();
        assert!(
            !names.contains(&"Bash"),
            "Bash should be removed by disallowed_tools, got {names:?}"
        );
        assert!(names.contains(&"Read"), "Read should remain, got {names:?}");
    }

    /// M6.33 SUB4: parent's cancel token propagates into the built
    /// child agent so retry-backoff sleeps + the streaming collector
    /// observe ctrl-C. Pre-fix the subagent ran to completion.
    #[tokio::test]
    async fn production_factory_propagates_cancel_token() {
        let cancel = CancelToken::new();
        let factory = ProductionAgentFactory {
            provider: Arc::new(StubProvider),
            base_tools: ToolRegistry::new(),
            model: "test".into(),
            system: String::new(),
            max_iterations: 1,
            max_depth: 3,
            max_tokens: 8192,
            agent_defs: AgentDefsConfig::default(),
            approver: Arc::new(crate::permissions::DenyApprover),
            permission_mode: PermissionMode::Auto,
            cancel: Some(cancel.clone()),
            hooks: None,
        };
        let child = factory.build("go", None, 1).await.unwrap();
        cancel.cancel();
        assert!(
            child
                .cancel
                .as_ref()
                .map(|c| c.is_cancelled())
                .unwrap_or(false),
            "child agent should observe parent's cancel token"
        );
    }

    #[tokio::test]
    async fn named_agent_passed_to_factory() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let saw_def = Arc::new(AtomicBool::new(false));
        let saw_def_clone = saw_def.clone();

        struct DefCheckFactory(Arc<AtomicBool>);
        #[async_trait]
        impl AgentFactory for DefCheckFactory {
            async fn build(&self, _p: &str, def: Option<&AgentDef>, _d: usize) -> Result<Agent> {
                if let Some(d) = def {
                    assert_eq!(d.name, "researcher");
                    self.0.store(true, Ordering::Relaxed);
                }
                let provider = ScriptedProvider::new(vec![text_script(&["found it"])]);
                Ok(Agent::new(provider, ToolRegistry::new(), "test", ""))
            }
        }

        let defs = crate::agent_defs::AgentDefsConfig {
            agents: vec![AgentDef {
                name: "researcher".into(),
                instructions: "Research things".into(),
                max_iterations: 5,
                ..Default::default()
            }],
        };

        let factory = Arc::new(DefCheckFactory(saw_def_clone));
        let tool = SubAgentTool::new(factory).with_agent_defs(defs);
        let out = tool
            .call(json!({"prompt": "find X", "agent": "researcher"}))
            .await
            .unwrap();
        assert_eq!(out, "found it");
        assert!(saw_def.load(Ordering::Relaxed));
    }
}
