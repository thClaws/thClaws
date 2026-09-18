//! Ordered presentation snapshots, independent of the worker's execution context.
use crate::event_render::{
    render_chat_dispatches, render_gui_shell_dispatch, render_terminal_ansi,
    terminal_data_envelope, terminal_history_replaced_envelope, TerminalRenderState,
};
use crate::session::Session;
use crate::shared_session::{DisplayMessage, ViewEvent};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::broadcast;

#[derive(Default)]
struct Transcript {
    events: Vec<Value>,
    sequence: u64,
    terminal: TerminalRenderState,
}

#[derive(Default)]
pub(crate) struct SessionViews {
    active: String,
    transcripts: HashMap<String, Transcript>,
}

fn render(terminal: &mut TerminalRenderState, event: &ViewEvent) -> Vec<Value> {
    let mut frames = render_chat_dispatches(event);
    if let Some(frame) = render_terminal_ansi(terminal, event) {
        frames.push(if matches!(event, ViewEvent::HistoryReplaced(_)) {
            terminal_history_replaced_envelope(&frame)
        } else {
            terminal_data_envelope(&frame)
        });
    }
    frames
        .into_iter()
        .filter_map(|s| serde_json::from_str(&s).ok())
        .collect()
}

fn from_session(session: &Session) -> Transcript {
    let mut transcript = Transcript::default();
    for event in [
        ViewEvent::HistoryReplaced(DisplayMessage::from_session(session)),
        ViewEvent::PlanUpdate(session.plan.clone()),
        ViewEvent::GoalUpdate(session.goal.clone()),
    ] {
        transcript
            .events
            .extend(render(&mut transcript.terminal, &event));
    }
    transcript
}

impl SessionViews {
    pub(crate) fn apply(&mut self, event: ViewEvent) -> Vec<String> {
        match event {
            ViewEvent::SessionActivated(session) => {
                self.active = session.id.clone();
                self.transcripts
                    .entry(session.id.clone())
                    .or_insert_with(|| from_session(&session));
                vec![json!({"type":"session_execution", "session_id": self.active}).to_string()]
            }
            ViewEvent::SessionViewRequest {
                session,
                request_id,
            } => {
                let fallback;
                let transcript = match self.transcripts.get(&session.id) {
                    Some(transcript) => transcript,
                    None => {
                        fallback = from_session(&session);
                        &fallback
                    }
                };
                vec![json!({
                    "type":"session_view", "session_id": session.id,
                    "request_id": request_id, "sequence": transcript.sequence,
                    "team_agent":session.owner_agent, "team_live":false,
                    "events": transcript.events,
                })
                .to_string()]
            }
            event => {
                let mut global = Vec::new();
                if let Some(shell) = render_gui_shell_dispatch(&event) {
                    global.push(shell);
                }
                if matches!(
                    event,
                    ViewEvent::SessionActionRejected { .. }
                        | ViewEvent::BusyChanged(_)
                        | ViewEvent::SessionListRefresh(_)
                        | ViewEvent::ProviderUpdate(_)
                        | ViewEvent::SettingsChanged(_)
                        | ViewEvent::KmsUpdate(_)
                        | ViewEvent::McpUpdate(_)
                        | ViewEvent::LineStatus(_)
                        | ViewEvent::TelegramStatus(_)
                        | ViewEvent::MessengerStatus(_)
                        | ViewEvent::ResearchUpdate(_)
                        | ViewEvent::QuitRequested
                        | ViewEvent::ReloadRequested
                ) || self.active.is_empty()
                {
                    global.extend(render_chat_dispatches(&event));
                    return global;
                }
                let transcript = self.transcripts.entry(self.active.clone()).or_default();
                if matches!(event, ViewEvent::HistoryReplaced(_)) {
                    transcript.events.retain(|e| {
                        matches!(
                            e["type"].as_str(),
                            Some("chat_plan_update" | "chat_goal_update" | "chat_permission_mode")
                        )
                    });
                    transcript.terminal = TerminalRenderState::default();
                }
                let events = render(&mut transcript.terminal, &event);
                if !events.is_empty() {
                    transcript.sequence += 1;
                    transcript.events.extend(events.clone());
                    global.push(json!({"type":"session_event", "session_id": self.active, "sequence": transcript.sequence, "events":events}).to_string());
                }
                global
            }
        }
    }
}

pub(crate) fn spawn(
    mut input: broadcast::Receiver<ViewEvent>,
    execution_session_id: std::sync::Arc<std::sync::Mutex<String>>,
) -> broadcast::Sender<String> {
    let (output, _) = broadcast::channel(2048);
    let tx = output.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("session view runtime");
        rt.block_on(async move {
            let mut views = SessionViews::default();
            loop {
                match input.recv().await {
                    Ok(event) => {
                        if let ViewEvent::SessionActivated(session) = &event {
                            *execution_session_id.lock().unwrap() = session.id.clone();
                        }
                        for frame in views.apply(event) {
                            let _ = tx.send(frame);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Never silently keep an incomplete replay. Clients request a fresh view.
                        views.transcripts.clear();
                        let _ = tx.send(json!({"type":"session_view_invalidated"}).to_string());
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    });
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> Session {
        let mut s = Session::new_detached("mock", "/tmp");
        s.id = id.to_string();
        s
    }
    fn snapshot(views: &mut SessionViews, id: &str) -> Value {
        serde_json::from_str(
            &views.apply(ViewEvent::SessionViewRequest {
                session: Box::new(session(id)),
                request_id: "request".into(),
            })[0],
        )
        .unwrap()
    }

    #[test]
    fn session_view_preserves_output_owner_and_replays_all_event_kinds() {
        let mut views = SessionViews::default();
        views.apply(ViewEvent::SessionActivated(Box::new(session("A"))));
        views.apply(ViewEvent::UserPrompt("work in A".into()));
        views.apply(ViewEvent::AssistantThinkingDelta("thinking in A".into()));
        let before = snapshot(&mut views, "A");
        for _ in 0..5 {
            assert!(snapshot(&mut views, "B")["events"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["text"] != "work in A"));
            assert_eq!(snapshot(&mut views, "A"), before);
        }
        views.apply(ViewEvent::ToolCallStart {
            name: "Mock".into(),
            label: "Mock".into(),
            input: json!({"owner":"A"}),
        });
        views.apply(ViewEvent::ToolCallResult {
            name: "Mock".into(),
            output: "A result".into(),
            ui_resource: None,
        });
        views.apply(ViewEvent::AssistantTextDelta("A complete".into()));
        views.apply(ViewEvent::TurnUsage("A usage".into()));
        views.apply(ViewEvent::TurnDone);
        let a = snapshot(&mut views, "A");
        let types: Vec<_> = a["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["type"].as_str())
            .collect();
        for ty in [
            "chat_user_message",
            "chat_thinking_delta",
            "chat_tool_call",
            "chat_tool_result",
            "chat_text_delta",
            "chat_turn_usage",
            "chat_done",
        ] {
            assert!(types.contains(&ty), "missing {ty}");
        }
        assert_eq!(views.active, "A");
        let b = snapshot(&mut views, "B");
        assert!(!b.to_string().contains("A complete"));
        assert!(!b.to_string().contains("A usage"));
    }

    #[test]
    fn session_view_keeps_plan_goal_snapshots_across_history_replacement() {
        let mut a = session("A");
        a.plan = Some(crate::tools::plan_state::Plan {
            id: "A plan".into(),
            steps: vec![],
        });
        let mut views = SessionViews::default();
        views.apply(ViewEvent::SessionActivated(Box::new(a)));
        views.apply(ViewEvent::HistoryReplaced(vec![]));
        assert!(snapshot(&mut views, "A").to_string().contains("A plan"));
        assert!(!snapshot(&mut views, "B").to_string().contains("A plan"));
    }

    #[test]
    fn busy_event_captures_event_time_state_not_later_global_state() {
        let event = ViewEvent::BusyChanged(Some(crate::agent_activity::BusyMeta {
            session_id: "A".into(),
            started_at: std::time::UNIX_EPOCH,
            last_progress: Some("working".into()),
        }));
        let frame: Value = serde_json::from_str(&render_chat_dispatches(&event)[0]).unwrap();
        assert_eq!(frame["sessionId"], "A");
        assert_eq!(frame["busy"], true);
    }
}

/// A teammate's GUI journal lives beside its existing mailbox/output log. Each
/// line is a complete rendered event; readers resume at a byte offset.
pub(crate) struct TeamSessionJournal {
    file: std::fs::File,
    terminal: TerminalRenderState,
}

impl TeamSessionJournal {
    pub(crate) fn new(
        mailbox: &crate::team::Mailbox,
        agent: &str,
        session: &Session,
    ) -> crate::error::Result<Self> {
        let path = mailbox.session_events_path(agent, &session.id)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        let mut journal = Self {
            file,
            terminal: TerminalRenderState::default(),
        };
        journal.append(&ViewEvent::HistoryReplaced(DisplayMessage::from_session(
            session,
        )))?;
        journal.append(&ViewEvent::PlanUpdate(session.plan.clone()))?;
        journal.append(&ViewEvent::GoalUpdate(session.goal.clone()))?;
        mailbox.bind_session(agent, &session.id)?;
        Ok(journal)
    }

    pub(crate) fn append(&mut self, event: &ViewEvent) -> crate::error::Result<()> {
        use fs2::FileExt;
        use std::io::Write;
        let frames = render(&mut self.terminal, event);
        if frames.is_empty() {
            return Ok(());
        }
        let line = serde_json::to_string(&frames)?;
        self.file.lock_exclusive()?;
        let result = writeln!(self.file, "{line}").and_then(|_| self.file.flush());
        let _ = self.file.unlock();
        result.map_err(Into::into)
    }

    pub(crate) fn agent_event(
        &mut self,
        event: &crate::agent::AgentEvent,
    ) -> crate::error::Result<()> {
        use crate::agent::AgentEvent;
        let event = match event {
            AgentEvent::Text(text) => ViewEvent::AssistantTextDelta(text.clone()),
            AgentEvent::Thinking(text) => ViewEvent::AssistantThinkingDelta(text.clone()),
            AgentEvent::ToolCallStart { name, input, .. } => ViewEvent::ToolCallStart {
                name: name.clone(),
                label: crate::tool_display::tool_label(name, input),
                input: input.clone(),
            },
            AgentEvent::ToolCallResult {
                name,
                output,
                ui_resource,
                ..
            } => ViewEvent::ToolCallResult {
                name: name.clone(),
                output: output.clone().unwrap_or_else(|e| e),
                ui_resource: ui_resource.clone(),
            },
            AgentEvent::ToolCallDenied { name, .. } => ViewEvent::ToolCallResult {
                name: name.clone(),
                output: "Denied".into(),
                ui_resource: None,
            },
            AgentEvent::UserMessageInjected { text } => ViewEvent::UserPrompt(text.clone()),
            _ => return Ok(()),
        };
        self.append(&event)
    }
}

#[cfg(test)]
mod team_tests {
    use super::*;
    use crate::providers::{EventStream, Provider, ProviderEvent, StreamRequest};
    use futures::StreamExt;
    use std::sync::{Arc, Mutex};

    struct ControlledProvider(Mutex<Option<EventStream>>);
    #[async_trait::async_trait]
    impl Provider for ControlledProvider {
        async fn stream(&self, _: StreamRequest) -> crate::error::Result<EventStream> {
            Ok(self.0.lock().unwrap().take().expect("one mock turn"))
        }
    }

    #[tokio::test]
    async fn simultaneous_agents_keep_independent_journals_and_persisted_history() {
        let temp = tempfile::tempdir().unwrap();
        let mailbox = crate::team::Mailbox::new(temp.path().join("team"));
        let store = crate::session::SessionStore::new(temp.path().join("sessions"));
        let mut tasks = Vec::new();
        let mut controls = Vec::new();
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
        for name in ["researcher", "coder"] {
            mailbox.init_agent(name).unwrap();
            let mut session = Session::new_detached("mock", temp.path().to_str().unwrap());
            session.owner_agent = Some(name.into());
            store.save(&mut session).unwrap();
            let id = session.id.clone();
            let mut journal = TeamSessionJournal::new(&mailbox, name, &session).unwrap();
            journal
                .append(&ViewEvent::UserPrompt(format!("task for {name}")))
                .unwrap();
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let stream = futures::stream::unfold(rx, |mut rx| async {
                rx.recv().await.map(|event| (Ok(event), rx))
            })
            .boxed();
            let agent = crate::agent::Agent::new(
                Arc::new(ControlledProvider(Mutex::new(Some(stream)))),
                crate::tools::ToolRegistry::new(),
                "mock",
                "mock",
            );
            let seen = seen_tx.clone();
            let root = temp.path().join("sessions");
            tasks.push(tokio::spawn(async move {
                let mut turn = Box::pin(agent.run_turn(format!("task for {name}")));
                while let Some(event) = turn.next().await {
                    let event = event.unwrap();
                    journal.agent_event(&event).unwrap();
                    if matches!(event, crate::agent::AgentEvent::Text(_)) {
                        seen.send(name).unwrap();
                    }
                }
                session.sync(agent.history_snapshot());
                crate::session::SessionStore::new(root)
                    .save(&mut session)
                    .unwrap();
                journal.append(&ViewEvent::TurnDone).unwrap();
            }));
            controls.push((name, id, tx));
        }
        // Both real Agent loops are suspended in their providers concurrently.
        for (name, _, tx) in &controls {
            tx.send(ProviderEvent::TextDelta(format!("{name} partial")))
                .unwrap();
        }
        for _ in 0..2 {
            tokio::time::timeout(std::time::Duration::from_secs(5), seen_rx.recv())
                .await
                .unwrap()
                .unwrap();
        }
        let mut cursors = Vec::new();
        for (name, id, _) in &controls {
            let (offset, events) = mailbox.read_session_events(name, id, 0).unwrap();
            assert!(events
                .iter()
                .any(|e| e["text"] == format!("{name} partial")));
            let other = if *name == "coder" {
                "researcher"
            } else {
                "coder"
            };
            assert!(!serde_json::to_string(&events)
                .unwrap()
                .contains(&format!("{other} partial")));
            assert!(mailbox
                .read_session_events(name, id, offset)
                .unwrap()
                .1
                .is_empty());
            cursors.push(offset);
        }
        // Team communication uses the existing inbox while both sessions run.
        mailbox
            .write_to_mailbox(
                "coder",
                crate::team::TeamMessage::new("researcher", "use my findings"),
            )
            .unwrap();
        assert_eq!(mailbox.read_unread("coder").unwrap().len(), 1);
        assert!(mailbox.read_unread("researcher").unwrap().is_empty());
        let stop = serde_json::to_string(&crate::team::ProtocolMessage::AbortTurn {
            from: "user".into(),
        })
        .unwrap();
        mailbox
            .write_to_mailbox("researcher", crate::team::TeamMessage::new("user", &stop))
            .unwrap();
        assert!(mailbox
            .read_unread("coder")
            .unwrap()
            .iter()
            .all(|m| crate::team::parse_protocol_message(m.content()).is_none()));
        for (_, _, tx) in &controls {
            tx.send(ProviderEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            })
            .unwrap();
        }
        // Closing each mock stream ends its turn without timers or network calls.
        let identities: Vec<_> = controls
            .into_iter()
            .map(|(name, id, tx)| {
                drop(tx);
                (name, id)
            })
            .collect();
        for task in tasks {
            tokio::time::timeout(std::time::Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
        }
        for ((name, id), offset) in identities.iter().zip(cursors) {
            let loaded = store.load(id).unwrap();
            assert_eq!(loaded.owner_agent.as_deref(), Some(*name));
            assert!(serde_json::to_string(&loaded.messages)
                .unwrap()
                .contains(&format!("{name} partial")));
            let (end, tail) = mailbox.read_session_events(name, id, offset).unwrap();
            assert!(tail.iter().any(|e| e["type"] == "chat_done"));
            assert!(mailbox
                .read_session_events(name, id, end)
                .unwrap()
                .1
                .is_empty());
            assert!(mailbox.read_session_events(name, id, 0).unwrap().1.len() > tail.len());
            mailbox.bind_session(name, "replacement").unwrap();
            assert!(mailbox.read_session_events(name, id, end).is_err());
        }
    }

    #[tokio::test]
    async fn stopping_one_mock_agent_leaves_peer_running() {
        let temp = tempfile::tempdir().unwrap();
        let mailbox = crate::team::Mailbox::new(temp.path().to_path_buf());
        let mut jobs = Vec::new();
        let mut controls = Vec::new();
        let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
        for name in ["researcher", "coder"] {
            mailbox.init_agent(name).unwrap();
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let ready = started.clone();
            let stream = futures::stream::unfold(rx, move |mut rx| {
                let ready = ready.clone();
                async move {
                    ready.send(()).unwrap();
                    rx.recv().await.map(|event| (Ok(event), rx))
                }
            })
            .boxed();
            let agent = crate::agent::Agent::new(
                Arc::new(ControlledProvider(Mutex::new(Some(stream)))),
                crate::tools::ToolRegistry::new(),
                "mock",
                "mock",
            );
            let cancel = crate::cancel::CancelToken::new();
            let token = cancel.clone();
            jobs.push(tokio::spawn(async move {
                crate::agent::collect_agent_turn_with_cancel(
                    agent.run_turn("work".into()),
                    Some(token),
                )
                .await
            }));
            controls.push((tx, cancel));
        }
        for _ in 0..2 {
            tokio::time::timeout(std::time::Duration::from_secs(5), starts.recv())
                .await
                .unwrap()
                .unwrap();
        }
        let stop = serde_json::to_string(&crate::team::ProtocolMessage::AbortTurn {
            from: "user".into(),
        })
        .unwrap();
        mailbox
            .write_to_mailbox("researcher", crate::team::TeamMessage::new("user", &stop))
            .unwrap();
        assert!(!mailbox.take_abort_request("coder"));
        assert!(mailbox.take_abort_request("researcher"));
        assert!(!mailbox.take_abort_request("researcher"));
        controls[0].1.cancel();
        let stopped = jobs.remove(0);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), stopped)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(!jobs[0].is_finished());
        assert!(!controls[1].1.is_cancelled());
        controls[1]
            .0
            .send(ProviderEvent::TextDelta("coder finished".into()))
            .unwrap();
        controls[1]
            .0
            .send(ProviderEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            })
            .unwrap();
        drop(controls);
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), jobs.remove(0))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.text, "coder finished");
    }

    #[test]
    fn journal_cursor_waits_for_complete_rows_and_rejects_invalid_paths() {
        use std::io::Write;
        let temp = tempfile::tempdir().unwrap();
        let mailbox = crate::team::Mailbox::new(temp.path().to_path_buf());
        mailbox.init_agent("coder").unwrap();
        mailbox.bind_session("coder", "session").unwrap();
        let path = mailbox.session_events_path("coder", "session").unwrap();
        std::fs::write(&path, "[{\"type\":\"chat_done\"}]").unwrap();
        assert_eq!(
            mailbox.read_session_events("coder", "session", 0).unwrap(),
            (0, vec![])
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        let (offset, events) = mailbox.read_session_events("coder", "session", 0).unwrap();
        assert_eq!(events.len(), 1);
        assert!(mailbox
            .read_session_events("coder", "session", offset + 1)
            .is_err());
        assert!(mailbox.session_events_path("../coder", "session").is_err());
        assert!(mailbox
            .session_events_path("coder", "../../secret")
            .is_err());
    }
}
