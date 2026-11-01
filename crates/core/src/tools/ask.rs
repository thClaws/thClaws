use super::{req_str, Tool};
use crate::error::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// M6.23 BUG AT1: hard timeout on the user-response await. Pre-fix the
/// agent stalled indefinitely if the user closed the GUI modal without
/// responding (or closed the terminal during a CLI prompt). 30 minutes
/// is generous for "let me think about this" while preventing the
/// forgotten-modal-stalls-forever case. The user can still /cancel
/// sooner if the cancel token is wired upstream.
const ASK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct AskUserTool;

pub struct AskUserRequest {
    pub id: u64,
    pub question: String,
    pub response: oneshot::Sender<String>,
}

static NEXT_ASK_ID: AtomicU64 = AtomicU64::new(1);
static GUI_ASK_SENDER: OnceLock<Mutex<Option<mpsc::UnboundedSender<AskUserRequest>>>> =
    OnceLock::new();

/// Plan-07 follow-up: when the active turn was driven by LINE,
/// the GUI Chat tab is somewhere the user can't see (different
/// device, different room). Routing the AskUserQuestion modal
/// there would hang the LINE conversation. The worker sets this
/// flag at the top of each LINE-driven turn and clears it after,
/// so AskUserQuestion can short-circuit with a message that
/// teaches the model to ask in its reply text instead.
static LINE_DRIVEN_TURN: AtomicBool = AtomicBool::new(false);

pub fn set_line_driven_turn(active: bool) {
    LINE_DRIVEN_TURN.store(active, Ordering::Relaxed);
}

pub fn is_line_driven_turn() -> bool {
    LINE_DRIVEN_TURN.load(Ordering::Relaxed)
}

pub fn set_gui_ask_sender(sender: Option<mpsc::UnboundedSender<AskUserRequest>>) {
    let slot = GUI_ASK_SENDER.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = slot.lock() {
        *guard = sender;
    }
}

fn gui_ask_sender() -> Option<mpsc::UnboundedSender<AskUserRequest>> {
    GUI_ASK_SENDER
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|guard| guard.clone()))
}

fn normalize_answer(answer: String) -> String {
    let trimmed = answer.trim().to_string();
    if trimmed.is_empty() {
        "(no response from user)".to_string()
    } else {
        trimmed
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &'static str {
        "AskUserQuestion"
    }

    fn description(&self) -> &'static str {
        "Ask the user a question and wait for their typed response. Use when \
         you need clarification, a decision, or any input that can't be \
         resolved from context or tools alone."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the user"
                }
            },
            "required": ["question"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        false
    }

    async fn call(&self, input: Value) -> Result<String> {
        let question = req_str(&input, "question")?.to_string();
        // Plan-07 follow-up: LINE-driven turn. The user is on
        // their phone via LINE; routing the question through the
        // GUI Chat tab modal (`gui_ask_sender`) would silently
        // hang the LINE conversation because the prompt lands on
        // a screen they can't see. Short-circuit with a polite
        // message telling the model to fold the question into its
        // reply text — the user's next LINE message becomes the
        // answer naturally.
        if is_line_driven_turn() {
            return Ok(format!(
                "(user is on LINE — please rephrase \"{question}\" as part of your reply text; \
                 their next LINE message will be the answer)"
            ));
        }
        if let Some(sender) = gui_ask_sender() {
            let id = NEXT_ASK_ID.fetch_add(1, Ordering::Relaxed);
            let (response, answer_rx) = oneshot::channel();
            if sender
                .send(AskUserRequest {
                    id,
                    question: question.clone(),
                    response,
                })
                .is_ok()
            {
                // M6.23 BUG AT1: bound the await on the user's
                // response. If they close the modal without
                // responding, the await would otherwise block
                // forever — `oneshot::Receiver` only resolves on
                // either send or sender-drop, and the modal closing
                // doesn't necessarily drop the responder.
                return Ok(normalize_answer(
                    match tokio::time::timeout(ASK_TIMEOUT, answer_rx).await {
                        Ok(Ok(answer)) => answer,
                        Ok(Err(_)) => String::new(), // sender dropped
                        Err(_) => {
                            return Ok(format!(
                                "(no response — user did not reply within {} minutes)",
                                ASK_TIMEOUT.as_secs() / 60,
                            ))
                        }
                    },
                ));
            }
        }

        // M6.23 BUG AT1: same timeout for the CLI fallback. The
        // blocking task itself can't be cancelled (read_line is
        // synchronous and blocking), but bounding the await prevents
        // the agent from waiting forever on a terminal that's been
        // closed or detached. The orphan blocking thread will be
        // reaped when the process exits.
        let blocking = tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Write};
            println!("\n\x1b[36m[agent asks]: {question}\x1b[0m");
            print!("\x1b[36m> \x1b[0m");
            std::io::stdout().flush().ok();
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).ok();
            line.trim().to_string()
        });

        match tokio::time::timeout(ASK_TIMEOUT, blocking).await {
            Ok(Ok(answer)) => Ok(normalize_answer(answer)),
            Ok(Err(_)) => Ok(normalize_answer(String::new())),
            Err(_) => Ok(format!(
                "(no response — user did not reply within {} minutes)",
                ASK_TIMEOUT.as_secs() / 60,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    /// Both tests in this module mutate `GUI_ASK_SENDER` +
    /// `LINE_DRIVEN_TURN` globals; rust's default parallel test
    /// runner would race them. Serialize via a shared mutex.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn gui_ask_sender_round_trips_answer() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (sender, mut requests) = mpsc::unbounded_channel();
        set_gui_ask_sender(Some(sender));

        let pending = tokio::spawn(async {
            AskUserTool
                .call(json!({ "question": "Ready?" }))
                .await
                .expect("ask call")
        });

        let req = requests.recv().await.expect("ask request");
        assert_eq!(req.question, "Ready?");
        req.response.send("yes".to_string()).expect("send response");

        assert_eq!(pending.await.expect("join ask"), "yes");
        set_gui_ask_sender(None);
    }

    #[tokio::test]
    async fn line_driven_turn_short_circuits_without_gui_modal() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Simulate "GUI is bound but user is on LINE": gui sender
        // IS registered (paired thClaws install always has it),
        // but the LINE-driven flag must take precedence so the
        // turn doesn't hang on a modal the user can't see.
        let (sender, mut requests) = mpsc::unbounded_channel();
        set_gui_ask_sender(Some(sender));
        set_line_driven_turn(true);

        let result = AskUserTool
            .call(json!({ "question": "Which file?" }))
            .await
            .expect("ask call");

        // Model gets a hint to rephrase, not a hang or modal route.
        assert!(result.contains("LINE"), "got: {result}");
        assert!(result.contains("Which file?"), "got: {result}");
        // GUI modal was NOT invoked despite sender being set.
        assert!(requests.try_recv().is_err(), "GUI modal was invoked");

        set_line_driven_turn(false);
        set_gui_ask_sender(None);
    }
}
