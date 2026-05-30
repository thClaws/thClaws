//! `thclaws --workflow <file.js>` headless entry (dev-plan/32 Stage L).
//!
//! Skips the `/workflow run` author + review phase entirely — the
//! script is pre-vetted by the operator. Mirrors `telegram::headless`
//! for environment construction (provider, system prompt, KMS +
//! Memory tools) and adds the `SubAgentTool` registration so the
//! workflow's `thclaws.subagent` calls route through a real
//! `ProductionAgentFactory`.
//!
//! `resume_id: Some(...)` skips the fresh-id path and loads the
//! completed-worker cache from the named workflow's `state.jsonl`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::config::AppConfig;
use crate::context::ProjectContext;
use crate::memory::MemoryStore;
use crate::permissions::{ApprovalSink, AutoApprover, PermissionMode};
use crate::subagent::{ProductionAgentFactory, SubAgentTool};
use crate::tools::ToolRegistry;
use crate::Result;

pub async fn run(
    config: AppConfig,
    script_path: PathBuf,
    resume_id: Option<String>,
) -> Result<i32> {
    let script = std::fs::read_to_string(&script_path).map_err(|e| {
        crate::Error::Agent(format!(
            "can't read workflow script '{}': {e}",
            script_path.display()
        ))
    })?;

    let cwd = std::env::current_dir()?;
    let (workflow_id, cache) = if let Some(id_or_prefix) = resume_id.as_deref() {
        let id = crate::workflow::resolve_id_prefix(&cwd, id_or_prefix)
            .map_err(|e| crate::Error::Agent(format!("--resume: {e}")))?;
        let cache = crate::workflow::read_completed_workers(&cwd, &id)
            .map_err(|e| crate::Error::Agent(format!("--resume: {e}")))?;
        (id, cache)
    } else {
        let id = crate::workflow::generate_workflow_id();
        crate::workflow::write_workflow_script(&cwd, &id, &script)
            .map_err(|e| crate::Error::Agent(format!("can't persist script.js: {e}")))?;
        (id, Vec::new())
    };

    let ctx = ProjectContext::discover(&cwd)?;
    let memory_store = MemoryStore::default_path().map(MemoryStore::new);
    let system_fallback = if config.system_prompt.is_empty() {
        crate::prompts::defaults::SYSTEM
    } else {
        config.system_prompt.as_str()
    };
    let base_prompt = crate::prompts::load("system", system_fallback);
    let mut system = ctx.build_system_prompt(&base_prompt);
    if let Some(store) = &memory_store {
        if let Some(sec) = store.system_prompt_section() {
            system.push_str("\n\n# Memory\n");
            system.push_str(&sec);
        }
    }
    let kms_section = crate::kms::system_prompt_section(&config.kms_active);
    if !kms_section.is_empty() {
        system.push_str("\n\n");
        system.push_str(&kms_section);
    }

    let mut tools = ToolRegistry::with_builtins();
    tools.register(Arc::new(crate::tools::KmsReadTool));
    tools.register(Arc::new(crate::tools::KmsSearchTool));
    tools.register(Arc::new(crate::tools::KmsWriteTool));
    tools.register(Arc::new(crate::tools::KmsAppendTool));
    tools.register(Arc::new(crate::tools::KmsDeleteTool));
    tools.register(Arc::new(crate::tools::KmsCreateTool));
    tools.register(Arc::new(crate::tools::MemoryReadTool));
    tools.register(Arc::new(crate::tools::MemoryWriteTool));
    tools.register(Arc::new(crate::tools::MemoryAppendTool));

    let provider = crate::repl::build_provider(&config)?;
    let approver: Arc<dyn ApprovalSink> = Arc::new(AutoApprover);

    let agent_defs = crate::agent_defs::AgentDefsConfig::load();
    let factory = Arc::new(ProductionAgentFactory {
        provider: provider.clone(),
        base_tools: tools.clone(),
        model: config.model.clone(),
        system: system.clone(),
        max_iterations: config.max_iterations,
        max_depth: crate::subagent::DEFAULT_MAX_DEPTH,
        max_tokens: config.max_tokens,
        agent_defs: agent_defs.clone(),
        approver,
        permission_mode: PermissionMode::Auto,
        cancel: None,
        hooks: None,
    });
    tools.register(Arc::new(
        SubAgentTool::new(factory)
            .with_depth(0)
            .with_agent_defs(agent_defs),
    ));
    let task_tool = tools.get(crate::subagent::TOOL_NAME);

    let mut logger = crate::workflow::WorkflowLogger::new(workflow_id.clone(), &cwd)?;
    if cache.is_empty() {
        logger.start(&format!("(--workflow {})", script_path.display()), &script)?;
    } else {
        logger.set_next_worker_id(cache.len() as u32);
    }
    let logger_handle: crate::workflow::LoggerHandle = Arc::new(Mutex::new(logger));

    eprintln!("[workflow] id={workflow_id}");
    if !cache.is_empty() {
        eprintln!("[workflow] resuming with {} cached worker(s)", cache.len());
    }

    let script_for_thread = script.clone();
    let cache_for_thread = if cache.is_empty() { None } else { Some(cache) };
    let logger_for_thread = logger_handle.clone();

    let wf_started = std::time::Instant::now();
    let (result, usages, remaining): (
        std::result::Result<String, String>,
        Vec<crate::providers::Usage>,
        usize,
    ) = tokio::task::spawn_blocking(move || {
        crate::workflow::set_task_tool(task_tool);
        crate::workflow::set_logger(Some(logger_for_thread));
        crate::workflow::set_usage_sink(true);
        crate::workflow::set_replay_cache(cache_for_thread);
        let res = (|| -> std::result::Result<String, String> {
            let mut sandbox = crate::workflow::WorkflowSandbox::new().map_err(|e| e.to_string())?;
            sandbox.run(&script_for_thread).map_err(|e| e.to_string())
        })();
        let usages = crate::workflow::take_all_usages();
        let remaining = crate::workflow::replay_remaining();
        crate::workflow::set_task_tool(None);
        crate::workflow::set_logger(None);
        crate::workflow::set_usage_sink(false);
        crate::workflow::set_replay_cache(None);
        (res, usages, remaining)
    })
    .await
    .map_err(|e| crate::Error::Agent(format!("workflow worker thread: {e}")))?;

    if let Ok(mut l) = logger_handle.lock() {
        let _ = match &result {
            Ok(text) => l.done(text),
            Err(e) => l.error(e),
        };
    }

    let elapsed = crate::tool_display::format_duration(wf_started.elapsed());
    let total_in: u64 = usages.iter().map(|u| u.input_tokens as u64).sum();
    let total_out: u64 = usages.iter().map(|u| u.output_tokens as u64).sum();
    let diverged = if remaining > 0 {
        format!(" — {remaining} cache entries unused (script diverged)")
    } else {
        String::new()
    };
    eprintln!(
        "[workflow] done — {} fresh workers, {elapsed}, {total_in}in / {total_out}out{diverged}",
        usages.len()
    );

    match result {
        Ok(text) => {
            println!("{text}");
            Ok(0)
        }
        Err(e) => {
            eprintln!("[workflow] script failed: {e}");
            Ok(1)
        }
    }
}
