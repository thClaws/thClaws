#![cfg(feature = "gui")]
//! User-driven side-channel agents — `/agent <name> <prompt>`.
//!
//! Spawns a fresh `Agent` instance on its own tokio task, **independent
//! of the main agent**. Differences from the LLM-driven `Task` tool
//! (`crate::subagent`):
//!
//! | | side-channel (this) | subagent (`Task` tool) |
//! |---|---|---|
//! | Trigger | User types `/agent name prompt` | Model calls `Task` tool |
//! | Concurrency | Runs concurrently with main agent | Blocks main's turn |
//! | Main's history | Not affected — main doesn't see prompt or result | Tool result lands in main's history |
//! | Cancel | Independent CancelToken — main's Cmd-C does NOT kill it | Inherits parent's cancel (main Cmd-C kills child) |
//! | Surface | `chat_side_channel_*` events on the chat tab | Single `Task` tool indicator |
//!
//! Per-spawn lifecycle:
//!
//! 1. Resolve `agent_name` → `AgentDef` from the loaded registry. Errors
//!    early if the name isn't known.
//! 2. Generate a stable `SideChannelId` (`side-<8 hex>`).
//! 3. Build a child `Agent` via the supplied factory, with `origin =
//!    SideChannel { id, agent_name }` so every approval request the
//!    child fires is tagged for the GUI.
//! 4. Wire an independent `CancelToken` (NOT a child of main's) — main
//!    Cmd-C doesn't reach the side channel. The token is stashed in the
//!    `SideChannelRegistry` so `/agent cancel <id>` can fire it later.
//! 5. Spawn a tokio task that drives `agent.run_turn(prompt)`, fans
//!    every `AgentEvent` out as `ViewEvent::SideChannel*`, and emits
//!    `SideChannelDone` on natural stop or `SideChannelError` on
//!    cancel / failure. The task removes the channel from the registry
//!    on exit.
//!
//! The registry is a process-level Mutex<HashMap<id, Handle>> exposed
//! via `registry()` so the slash-command dispatch (`/agents`, `/agent
//! cancel`) can introspect / mutate without each surface threading the
//! handle through.

use crate::agent::AgentEvent;
use crate::cancel::CancelToken;
use crate::error::{Error, Result};
use crate::permissions::AgentOrigin;
use crate::shared_session::ViewEvent;
use crate::subagent::AgentFactory;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tokio::sync::broadcast;

/// Stable handle for a running side-channel agent. Used by
/// `/agent cancel <id>` and `/agents` list.
pub type SideChannelId = String;

/// Per-channel state held in the registry. The cancel token is the
/// same instance handed to the spawned `Agent`; firing `cancel.cancel()`
/// from `/agent cancel` propagates into the agent's retry sleeps and
/// the `collect_agent_turn_with_cancel` select gate.
pub struct SideChannelHandle {
    pub agent_name: String,
    pub started_at: Instant,
    pub cancel: CancelToken,
    /// JoinHandle so callers (tests, future shutdown logic) can await
    /// the spawn task. Wrapped in Mutex<Option<>> because `JoinHandle`
    /// is `!Clone` and the registry is shared — we hand it out by
    /// `take()`-ing it once.
    pub join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Process-wide registry of active side channels. Singleton — same
/// instance shared across surfaces (CLI REPL, GUI Chat dispatch). The
/// slash-command handlers consult this for `/agents` listing and
/// `/agent cancel <id>`. Entries are removed by the spawn task itself
/// when the channel exits (whether by success, error, or cancel).
pub fn registry() -> &'static Arc<Mutex<HashMap<SideChannelId, SideChannelHandle>>> {
    static REG: OnceLock<Arc<Mutex<HashMap<SideChannelId, SideChannelHandle>>>> = OnceLock::new();
    REG.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

/// Spawn a new side channel. Returns the assigned id immediately;
/// the actual agent work happens on a tokio task that streams
/// `ViewEvent::SideChannel*` events through `events_tx`.
///
/// Errors:
/// - `agent_name` not found in `factory`'s known agent_defs (caller
///   should have validated, but defensive double-check)
/// - factory.build returns an error (provider issue, etc.)
///
/// The function does NOT block on the agent — it returns once the
/// task is spawned. Use `registry()` to track state, or subscribe
/// to `events_tx` for streaming updates.
pub async fn spawn_side_channel(
    agent_name: String,
    prompt: String,
    factory: Arc<dyn AgentFactory>,
    agent_defs: crate::agent_defs::AgentDefsConfig,
    events_tx: broadcast::Sender<ViewEvent>,
) -> Result<SideChannelId> {
    let mut agent_def = agent_defs
        .agents
        .iter()
        .find(|d| d.name == agent_name)
        .cloned()
        .ok_or_else(|| {
            Error::Tool(format!(
                "unknown agent '{agent_name}' — try /agents to list known agents"
            ))
        })?;

    // A user launching an agent from the sidebar is an explicit "go do
    // this for me" — let it read/write artifacts even when its def is
    // declared read-only. See [`grant_file_tools_for_sidechannel`].
    grant_file_tools_for_sidechannel(&mut agent_def);

    let id: SideChannelId = format!(
        "side-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("anon")
            .to_string()
    );

    // Independent cancel — main's Cmd-C does NOT kill this. User
    // cancels via `/agent cancel <id>`.
    let cancel = CancelToken::new();

    // Build the child agent. The factory currently sets origin =
    // Main (or whatever its parent's origin is); we override here so
    // the side channel's permission requests are tagged correctly.
    let mut agent = factory.build(&prompt, Some(&agent_def), 0).await?;
    agent = agent
        .with_origin(AgentOrigin::SideChannel {
            id: id.clone(),
            agent_name: agent_name.clone(),
        })
        .with_cancel(cancel.clone());

    let started_at = Instant::now();

    // Emit start event before spawning so the UI sees the indicator
    // immediately, not after the task scheduler decides to run.
    let _ = events_tx.send(ViewEvent::SideChannelStart {
        id: id.clone(),
        agent_name: agent_name.clone(),
    });

    let id_for_task = id.clone();
    let agent_name_for_task = agent_name.clone();
    // Kept separate from `agent_name_for_task` (moved into the Done
    // event) so we can still test it after the match to decide whether
    // to refresh the KMS sidebar.
    let agent_name_for_refresh = agent_name.clone();
    let is_artifact_agent = agent_name == ARTIFACT_AGENT;
    let events_tx_for_task = events_tx.clone();
    let cancel_for_task = cancel.clone();

    let inner = tokio::spawn(async move {
        // Stream events as the child runs, forwarding to chat surface.
        let stream = agent.run_turn(prompt);
        let mut stream = Box::pin(stream);
        let mut full_text = String::new();
        let mut errored: Option<String> = None;
        let mut artifact_dir_seen: Option<String> = None;

        loop {
            let next = tokio::select! {
                ev = futures::StreamExt::next(&mut stream) => ev,
                _ = cancel_for_task.cancelled() => {
                    errored = Some("cancelled".into());
                    break;
                }
            };
            let Some(ev) = next else { break };
            match ev {
                Ok(AgentEvent::Text(s)) => {
                    full_text.push_str(&s);
                    let _ = events_tx_for_task.send(ViewEvent::SideChannelTextDelta {
                        id: id_for_task.clone(),
                        text: s,
                    });
                }
                Ok(AgentEvent::ToolCallStart { name, input, .. }) => {
                    // Progress feedback: build a descriptive label from the
                    // tool input (e.g. `WebFetch (https://…)`, `Write
                    // (articles/…/article.md)`, `FetchImages (…/article.md)`)
                    // instead of the bare tool name, so `/extract` and other
                    // side-channel agents show WHAT each step is doing, not just
                    // that a tool ran.
                    let label = crate::tool_display::tool_label(&name, &input);
                    if is_artifact_agent {
                        if let Some(dir) = artifact_dir(&name, &input) {
                            artifact_dir_seen = Some(dir);
                        }
                    }
                    let _ = events_tx_for_task.send(ViewEvent::SideChannelToolCall {
                        id: id_for_task.clone(),
                        tool_name: name,
                        label,
                    });
                }
                Ok(AgentEvent::Done { .. }) => break,
                Ok(_) => {}
                Err(e) => {
                    errored = Some(format!("{e}"));
                    break;
                }
            }
        }

        let duration_ms = started_at.elapsed().as_millis() as u64;
        match errored {
            Some(error) => {
                let _ = events_tx_for_task.send(ViewEvent::SideChannelError {
                    id: id_for_task.clone(),
                    error,
                });
            }
            None => {
                let result_text = match artifact_dir_seen.as_deref() {
                    Some(dir) => ensure_dir_first_line(&full_text, dir),
                    None => full_text,
                };
                let _ = events_tx_for_task.send(ViewEvent::SideChannelDone {
                    id: id_for_task.clone(),
                    agent_name: agent_name_for_task,
                    duration_ms,
                    result_text,
                });
            }
        }

        // A finished `dream` run has written/updated KMS pages (digests
        // + topic pages), so push a `kms_update` to refresh the sidebar
        // without a manual /reload — same pattern research-job
        // completion uses. Fire on any outcome: a run that errored or
        // was cancelled may still have written pages before stopping.
        if agent_name_for_refresh == "dream" {
            let _ = events_tx_for_task.send(ViewEvent::KmsUpdate(
                crate::kms::build_update_payload().to_string(),
            ));
        }

        // Always remove from registry on exit, regardless of outcome.
        if let Ok(mut reg) = registry().lock() {
            reg.remove(&id_for_task);
        }
    });

    // Watchdog. The inner task's body emits Done/Error and cleans the
    // registry on its own — but only if it reaches that code. A panic
    // inside the agent loop (or the provider it polls) unwinds the
    // task and tokio converts that into `Err(JoinError::Panic)` on the
    // JoinHandle. Without this watchdog the sidebar would show the
    // agent as "running forever" and the registry entry would leak.
    // The watchdog awaits the inner task, detects abnormal exits, and
    // emits a `SideChannelError` + removes the registry entry so the
    // UI converges to the ✗ state and `/agents` listings stay clean.
    let id_for_watchdog = id.clone();
    let events_tx_for_watchdog = events_tx.clone();
    let join = tokio::spawn(async move {
        match inner.await {
            Ok(()) => {
                // Normal path — inner already emitted Done/Error and
                // removed itself from the registry. Nothing for the
                // watchdog to do.
            }
            Err(je) => {
                let err_msg = if je.is_panic() {
                    let payload = je.into_panic();
                    let panic_msg = payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic payload".to_string());
                    format!("agent panicked: {panic_msg}")
                } else if je.is_cancelled() {
                    // Tokio-level abort (e.g. runtime shutdown). The
                    // inner task didn't get to emit. Treat as error so
                    // the UI doesn't stick on "running".
                    "tokio task aborted".to_string()
                } else {
                    "tokio task ended unexpectedly".to_string()
                };
                let _ = events_tx_for_watchdog.send(ViewEvent::SideChannelError {
                    id: id_for_watchdog.clone(),
                    error: err_msg,
                });
                if let Ok(mut reg) = registry().lock() {
                    reg.remove(&id_for_watchdog);
                }
            }
        }
    });

    if let Ok(mut reg) = registry().lock() {
        reg.insert(
            id.clone(),
            SideChannelHandle {
                agent_name,
                started_at,
                cancel,
                join: Mutex::new(Some(join)),
            },
        );
    }

    Ok(id)
}

/// Cancel a running side channel by id. Returns `true` if the channel
/// was found and signalled, `false` if no such id is active. The
/// channel exits asynchronously after this returns — the cancel
/// token wakes the agent's `cancelled()` await and the spawn task
/// emits `SideChannelError { error: "cancelled" }` before removing
/// itself from the registry.
pub fn cancel_side_channel(id: &str) -> bool {
    if let Ok(reg) = registry().lock() {
        if let Some(handle) = reg.get(id) {
            handle.cancel.cancel();
            return true;
        }
    }
    false
}

/// Snapshot of all active side channels for `/agents` listing.
/// Returns (id, agent_name, elapsed_seconds) tuples — kept simple to
/// avoid exposing the JoinHandle / CancelToken to callers.
pub fn list_side_channels() -> Vec<(String, String, f64)> {
    let Ok(reg) = registry().lock() else {
        return Vec::new();
    };
    reg.iter()
        .map(|(id, h)| {
            (
                id.clone(),
                h.agent_name.clone(),
                h.started_at.elapsed().as_secs_f64(),
            )
        })
        .collect()
}

/// Widen a sidebar-launched agent's tool allow-list with the file tools
/// (Read/Write/Edit) so a user-invoked agent can save the artifact it was
/// asked for — even when its def is declared read-only (e.g. a planner
/// that normally only returns text). Running an agent from the sidebar is
/// an explicit user action, so this matches the expectation that "if I
/// tell it to write a file, it can."
///
/// Rules: a def with an EMPTY allow-list already inherits every tool, so
/// it's left alone. A tool listed in `disallowed_tools` stays denied (the
/// author's explicit deny wins). The base registry still gates what
/// actually exists, so adding a name the registry lacks is a harmless
/// no-op.
fn grant_file_tools_for_sidechannel(def: &mut crate::agent_defs::AgentDef) {
    if def.tools.is_empty() {
        return;
    }
    for t in ["Read", "Write", "Edit"] {
        let denied = def.disallowed_tools.iter().any(|d| d == t);
        let present = def.tools.iter().any(|x| x == t);
        if !denied && !present {
            def.tools.push(t.to_string());
        }
    }
}

/// Agent whose result the GUI is expected to open in the Files tab, so
/// its output folder has to survive the chat's first-line-only render.
const ARTIFACT_AGENT: &str = "content-extractor";

/// Folder an artifact-producing run landed in — the parent of a `.md`
/// path it wrote (or handed to `FetchImages`). `articles/<slug>/<slug>.md`
/// and the `FetchImages` rewrite of the same file both yield the slug
/// folder, so the last one seen is the article's home.
fn artifact_dir(name: &str, input: &serde_json::Value) -> Option<String> {
    let key = match name {
        "Write" | "Edit" => "file_path",
        "FetchImages" => "markdown_path",
        _ => return None,
    };
    let path = input
        .get(key)
        .or_else(|| input.get("path"))
        .and_then(|v| v.as_str())?;
    if !path.to_ascii_lowercase().ends_with(".md") {
        return None;
    }
    let parent = std::path::Path::new(path).parent()?.to_str()?;
    (!parent.is_empty()).then(|| parent.to_string())
}

/// Splice the output folder onto the first line of a side-channel result.
///
/// `ChatView` renders only the first non-blank line of `result_text`, so an
/// agent that opens with prose leaves the user no path to open. The prompt
/// asks for a leading status line; this makes it hold even when the model
/// ignores it. A first line that already names the folder is left alone.
fn ensure_dir_first_line(result: &str, dir: &str) -> String {
    let body = result.trim_start();
    if body.is_empty() {
        return format!("{dir}/");
    }
    let (first, rest) = match body.split_once('\n') {
        Some((f, r)) => (f, Some(r)),
        None => (body, None),
    };
    if first.contains(dir) {
        return result.to_string();
    }
    let head = format!("{dir}/ — {}", first.trim());
    match rest {
        Some(rest) => format!("{head}\n{rest}"),
        None => head,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::agent_defs::AgentDef;
    use crate::error::Result as CrateResult;
    use crate::permissions::AutoApprover;
    use crate::providers::{EventStream, Provider, ProviderEvent, StreamRequest};
    use async_trait::async_trait;
    use futures::stream;

    /// Tiny in-test provider that emits a fixed text + MessageStop.
    /// Mirrors the pattern used by `agent.rs::tests::ScriptedProvider`
    /// without re-exporting it.
    struct InlineProvider {
        text: String,
    }

    #[async_trait]
    impl Provider for InlineProvider {
        async fn stream(&self, _req: StreamRequest) -> CrateResult<EventStream> {
            let events = vec![
                Ok(ProviderEvent::MessageStart {
                    model: "stub".into(),
                }),
                Ok(ProviderEvent::TextDelta(self.text.clone())),
                Ok(ProviderEvent::ContentBlockStop),
                Ok(ProviderEvent::MessageStop {
                    stop_reason: Some("end_turn".into()),
                    usage: None,
                }),
            ];
            Ok(Box::pin(stream::iter(events)))
        }
    }

    /// Test factory that builds a tiny Agent backed by InlineProvider.
    struct StubFactory {
        text: String,
    }

    #[async_trait]
    impl AgentFactory for StubFactory {
        async fn build(
            &self,
            _prompt: &str,
            _agent_def: Option<&AgentDef>,
            _child_depth: usize,
        ) -> Result<Agent> {
            let provider = Arc::new(InlineProvider {
                text: self.text.clone(),
            });
            Ok(Agent::new(
                provider,
                crate::tools::ToolRegistry::new(),
                "stub",
                "system",
            )
            .with_approver(Arc::new(AutoApprover))
            .with_max_iterations(2))
        }
    }

    #[tokio::test]
    async fn spawn_emits_start_text_done_events() {
        // Before each test clear any leftover state from other tests
        // sharing the same process-level registry.
        if let Ok(mut r) = registry().lock() {
            r.clear();
        }

        let (events_tx, mut events_rx) = broadcast::channel(64);
        let factory = Arc::new(StubFactory {
            text: "hello world".into(),
        });
        let agent_defs = crate::agent_defs::AgentDefsConfig {
            agents: vec![AgentDef {
                name: "translator".into(),
                max_iterations: 1,
                ..AgentDef::default()
            }],
        };
        let id = spawn_side_channel(
            "translator".into(),
            "test".into(),
            factory,
            agent_defs,
            events_tx,
        )
        .await
        .expect("spawn ok");
        assert!(id.starts_with("side-"));

        // Drain events until Done. 2-second timeout safety.
        let mut saw_start = false;
        let mut saw_text = false;
        let mut saw_done = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            let res =
                tokio::time::timeout(std::time::Duration::from_millis(200), events_rx.recv()).await;
            match res {
                Ok(Ok(ViewEvent::SideChannelStart { id: i, .. })) if i == id => {
                    saw_start = true;
                }
                Ok(Ok(ViewEvent::SideChannelTextDelta { id: i, text }))
                    if i == id && text.contains("hello") =>
                {
                    saw_text = true;
                }
                Ok(Ok(ViewEvent::SideChannelDone {
                    id: i, result_text, ..
                })) if i == id => {
                    saw_done = true;
                    assert!(result_text.contains("hello"));
                    break;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => continue,
            }
        }
        assert!(saw_start, "missing SideChannelStart");
        assert!(saw_text, "missing SideChannelTextDelta");
        assert!(saw_done, "missing SideChannelDone");

        // Registry should be empty after the channel exits.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let reg = registry().lock().unwrap();
        assert!(
            !reg.contains_key(&id),
            "channel should be removed from registry on exit"
        );
    }

    /// Provider whose `stream()` panics on first invocation — used to
    /// drive the watchdog code path. The panic happens after
    /// spawn_side_channel returns (inside the inner tokio task), so
    /// the function call itself succeeds and the SideChannelError
    /// surfaces asynchronously via the broadcast channel.
    struct PanicProvider;

    #[async_trait]
    impl Provider for PanicProvider {
        async fn stream(&self, _req: StreamRequest) -> CrateResult<EventStream> {
            panic!("intentional test panic from provider.stream");
        }
    }

    struct PanicFactory;

    #[async_trait]
    impl AgentFactory for PanicFactory {
        async fn build(
            &self,
            _prompt: &str,
            _agent_def: Option<&AgentDef>,
            _child_depth: usize,
        ) -> Result<Agent> {
            Ok(Agent::new(
                Arc::new(PanicProvider),
                crate::tools::ToolRegistry::new(),
                "stub",
                "system",
            )
            .with_approver(Arc::new(AutoApprover))
            .with_max_iterations(2))
        }
    }

    #[tokio::test]
    async fn spawn_emits_error_on_panic() {
        if let Ok(mut r) = registry().lock() {
            r.clear();
        }

        // Suppress the default panic hook for the duration of this
        // test so the intentional panic doesn't print a noisy
        // backtrace to the test output. The runtime still catches it
        // and reports via JoinError::Panic; we just don't need the
        // stderr noise.
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        let (events_tx, mut events_rx) = broadcast::channel(64);
        let factory = Arc::new(PanicFactory);
        let agent_defs = crate::agent_defs::AgentDefsConfig {
            agents: vec![AgentDef {
                name: "translator".into(),
                max_iterations: 1,
                ..AgentDef::default()
            }],
        };
        let id = spawn_side_channel(
            "translator".into(),
            "boom".into(),
            factory,
            agent_defs,
            events_tx,
        )
        .await
        .expect("spawn ok — panic happens inside task, not at spawn time");

        // Watchdog should fire a SideChannelError within ~1s.
        let mut saw_error = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            let res =
                tokio::time::timeout(std::time::Duration::from_millis(200), events_rx.recv()).await;
            if let Ok(Ok(ViewEvent::SideChannelError { id: i, error })) = res {
                if i == id && error.contains("panicked") {
                    saw_error = true;
                    break;
                }
            }
        }

        // Restore the panic hook before any assertion can fail so the
        // test harness's own panic reporting works correctly.
        std::panic::set_hook(prev_hook);

        assert!(saw_error, "watchdog should emit SideChannelError on panic");

        // Registry should be empty after the watchdog cleans up.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let reg = registry().lock().unwrap();
        assert!(
            !reg.contains_key(&id),
            "watchdog should remove registry entry after a panicked task"
        );
    }

    #[tokio::test]
    async fn spawn_unknown_agent_errors() {
        if let Ok(mut r) = registry().lock() {
            r.clear();
        }
        let (events_tx, _rx) = broadcast::channel(8);
        let factory = Arc::new(StubFactory { text: "x".into() });
        let agent_defs = crate::agent_defs::AgentDefsConfig { agents: vec![] };
        let result = spawn_side_channel(
            "nonexistent".into(),
            "test".into(),
            factory,
            agent_defs,
            events_tx,
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown agent"), "got: {err}");
    }

    #[test]
    fn list_returns_active_channels() {
        // No async needed — list/cancel are sync.
        if let Ok(mut r) = registry().lock() {
            r.clear();
        }
        assert!(list_side_channels().is_empty());

        // Insert a fake handle directly to test the snapshot shape.
        let cancel = CancelToken::new();
        let _join: tokio::task::JoinHandle<()> = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .spawn(async {});
        // Construct a join we can stash without running an async ctx.
        // Easier: just stash None.
        if let Ok(mut reg) = registry().lock() {
            reg.insert(
                "side-test".into(),
                SideChannelHandle {
                    agent_name: "translator".into(),
                    started_at: Instant::now(),
                    cancel,
                    join: Mutex::new(None),
                },
            );
        }

        let snapshot = list_side_channels();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].0, "side-test");
        assert_eq!(snapshot[0].1, "translator");

        if let Ok(mut r) = registry().lock() {
            r.clear();
        }
    }

    #[test]
    fn cancel_returns_false_for_unknown() {
        if let Ok(mut r) = registry().lock() {
            r.clear();
        }
        assert!(!cancel_side_channel("does-not-exist"));
    }

    #[test]
    fn sidechannel_grants_file_tools_to_restricted_agent() {
        use crate::agent_defs::AgentDef;

        // Read-only planner (like the marketplace `outliner`): gains the
        // file tools, without duplicating the Read it already had.
        let mut def = AgentDef {
            name: "outliner".into(),
            tools: vec!["Read".into(), "Grep".into(), "Glob".into()],
            ..Default::default()
        };
        grant_file_tools_for_sidechannel(&mut def);
        assert!(def.tools.contains(&"Write".to_string()));
        assert!(def.tools.contains(&"Edit".to_string()));
        assert_eq!(def.tools.iter().filter(|t| *t == "Read").count(), 1);

        // Empty allow-list already inherits everything → untouched.
        let mut open = AgentDef {
            name: "open".into(),
            tools: vec![],
            ..Default::default()
        };
        grant_file_tools_for_sidechannel(&mut open);
        assert!(open.tools.is_empty());

        // An explicit deny wins — Write stays out.
        let mut denied = AgentDef {
            name: "reviewer".into(),
            tools: vec!["Read".into()],
            disallowed_tools: vec!["Write".into(), "Edit".into()],
            ..Default::default()
        };
        grant_file_tools_for_sidechannel(&mut denied);
        assert!(!denied.tools.contains(&"Write".to_string()));
        assert!(!denied.tools.contains(&"Edit".to_string()));
    }

    /// `/extract`'s folder has to reach the chat's first-line-only render
    /// even when the model opens with prose instead of the status line.
    #[test]
    fn artifact_dir_first_line_is_spliced_in() {
        let prose = "I clipped the article for you.\nImages: 8 downloaded.";
        assert_eq!(
            ensure_dir_first_line(prose, "articles/thai-nlp"),
            "articles/thai-nlp/ — I clipped the article for you.\nImages: 8 downloaded."
        );

        // Already led with the folder → left exactly as the agent wrote it.
        let good = "articles/thai-nlp/ — thai-nlp.md · 8 images (1 failed)";
        assert_eq!(ensure_dir_first_line(good, "articles/thai-nlp"), good);

        // Empty result still names the folder.
        assert_eq!(
            ensure_dir_first_line("   \n\n", "articles/x"),
            "articles/x/"
        );

        // Single line, no trailing body.
        assert_eq!(
            ensure_dir_first_line("done", "articles/x"),
            "articles/x/ — done"
        );
    }

    #[test]
    fn artifact_dir_reads_the_md_parent_only() {
        use serde_json::json;

        assert_eq!(
            artifact_dir("Write", &json!({"file_path": "articles/loops/loops.md"})),
            Some("articles/loops".to_string())
        );
        assert_eq!(
            artifact_dir(
                "FetchImages",
                &json!({"markdown_path": "articles/loops/loops.md"})
            ),
            Some("articles/loops".to_string())
        );
        // Non-markdown artifacts (the saved raw source) don't name the folder.
        assert_eq!(
            artifact_dir("Write", &json!({"file_path": "articles/loops/source.html"})),
            None
        );
        // A bare filename has no parent to report.
        assert_eq!(
            artifact_dir("Write", &json!({"file_path": "notes.md"})),
            None
        );
        // Unrelated tools are ignored.
        assert_eq!(
            artifact_dir("WebFetch", &json!({"url": "https://x/y.md"})),
            None
        );
    }
}
