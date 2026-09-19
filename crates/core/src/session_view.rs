//! Ordered presentation snapshots, independent of the worker's execution context.
use crate::event_render::{
    render_chat_dispatches, render_gui_shell_dispatch, render_terminal_ansi,
    terminal_data_envelope, terminal_history_replaced_envelope, TerminalRenderState,
};
use crate::session::Session;
use crate::shared_session::{DisplayMessage, ViewEvent};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use tokio::sync::broadcast;

const MAX_TRANSCRIPT_EVENTS: usize = 4096;
const MAX_TRANSCRIPT_BYTES: usize = 2 * 1024 * 1024;
const MAX_TRANSCRIPTS: usize = 16;

#[derive(Default)]
struct Transcript {
    events: VecDeque<Value>,
    bytes: usize,
    trimmed: bool,
    last_viewed: u64,
    sequence: u64,
    terminal: TerminalRenderState,
}

#[derive(Default)]
pub(crate) struct SessionViews {
    active: String,
    transcripts: HashMap<String, Transcript>,
    clock: u64,
}

impl Transcript {
    fn extend(&mut self, events: Vec<Value>) {
        for event in events {
            let bytes = event.to_string().len();
            if bytes > MAX_TRANSCRIPT_BYTES {
                self.trimmed = true;
                continue;
            }
            self.bytes += bytes;
            self.events.push_back(event);
            while self.events.len() > MAX_TRANSCRIPT_EVENTS || self.bytes > MAX_TRANSCRIPT_BYTES {
                if let Some(old) = self.events.pop_front() {
                    self.bytes -= old.to_string().len();
                    self.trimmed = true;
                }
            }
        }
    }

    fn snapshot(&self) -> Vec<Value> {
        let mut events = Vec::new();
        if self.trimmed {
            events.push(json!({"type":"chat_slash_output", "text":"Earlier output trimmed from the in-memory preview. Full saved history remains in the session file."}));
            events.push(
                serde_json::from_str(&terminal_data_envelope(
                    "\r\n[Earlier output trimmed from the in-memory preview.]\r\n",
                ))
                .expect("terminal envelope"),
            );
        }
        events.extend(self.events.iter().cloned());
        events
    }
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
        let frames = render(&mut transcript.terminal, &event);
        transcript.extend(frames);
    }
    transcript
}

impl SessionViews {
    fn evict(&mut self) {
        while self.transcripts.len() > MAX_TRANSCRIPTS {
            let oldest = self
                .transcripts
                .iter()
                .filter(|(id, _)| **id != self.active)
                .min_by_key(|(_, t)| t.last_viewed)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                self.transcripts.remove(&id);
            } else {
                break;
            }
        }
    }

    pub(crate) fn apply(&mut self, event: ViewEvent) -> Vec<String> {
        self.clock += 1;
        match event {
            ViewEvent::SessionActivated(session) => {
                self.active = session.id.clone();
                self.transcripts
                    .entry(session.id.clone())
                    .or_insert_with(|| from_session(&session));
                self.transcripts.get_mut(&self.active).unwrap().last_viewed = self.clock;
                self.evict();
                vec![json!({"type":"session_execution", "session_id": self.active}).to_string()]
            }
            ViewEvent::SessionViewRequest {
                session,
                request_id,
            } => {
                if let Some(t) = self.transcripts.get_mut(&session.id) {
                    t.last_viewed = self.clock;
                }
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
                    "events": transcript.snapshot(),
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
                    transcript.bytes = transcript.events.iter().map(|e| e.to_string().len()).sum();
                    transcript.trimmed = false;
                    transcript.terminal = TerminalRenderState::default();
                }
                let events = render(&mut transcript.terminal, &event);
                if !events.is_empty() {
                    transcript.sequence += 1;
                    transcript.extend(events.clone());
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
                            *execution_session_id
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) = session.id.clone();
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
    fn session_view_bounds_event_count_bytes_and_marks_trimming() {
        let mut t = Transcript::default();
        for _ in 0..MAX_TRANSCRIPT_EVENTS + 20 {
            t.extend(vec![json!({"type":"chat_text_delta","text":"chunk"})]);
        }
        assert_eq!(t.events.len(), MAX_TRANSCRIPT_EVENTS);
        assert!(t.trimmed);
        assert!(t.snapshot()[0]["text"]
            .as_str()
            .unwrap()
            .contains("trimmed"));
        for _ in 0..100 {
            t.extend(vec![
                json!({"type":"chat_tool_result","output":"x".repeat(50_000)}),
            ]);
        }
        assert!(t.bytes <= MAX_TRANSCRIPT_BYTES);
        assert!(t.events.len() < MAX_TRANSCRIPT_EVENTS);
        t.extend(vec![
            json!({"type":"chat_text_delta","text":"x".repeat(MAX_TRANSCRIPT_BYTES + 1)}),
        ]);
        assert!(t.bytes <= MAX_TRANSCRIPT_BYTES);
    }

    #[test]
    fn session_view_evicts_old_sessions_but_retains_active_and_recently_viewed() {
        let mut views = SessionViews::default();
        for i in 0..MAX_TRANSCRIPTS {
            views.apply(ViewEvent::SessionActivated(Box::new(session(&format!(
                "s{i}"
            )))));
        }
        snapshot(&mut views, "s0");
        views.apply(ViewEvent::SessionActivated(Box::new(session("new"))));
        assert_eq!(views.transcripts.len(), MAX_TRANSCRIPTS);
        assert!(views.transcripts.contains_key("s0"));
        assert!(views.transcripts.contains_key("new"));
        assert!(!views.transcripts.contains_key("s1"));
        assert_eq!(snapshot(&mut views, "s1")["session_id"], "s1");
    }

    #[test]
    fn session_view_forwards_quit_as_an_ordered_control_frame() {
        let mut views = SessionViews::default();
        views.apply(ViewEvent::SessionActivated(Box::new(session("A"))));
        assert_eq!(
            views.apply(ViewEvent::QuitRequested),
            vec![json!({"type":"session_quit"}).to_string()]
        );
        assert!(!snapshot(&mut views, "A")
            .to_string()
            .contains("session_quit"));
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
