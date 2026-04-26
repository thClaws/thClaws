use super::{req_str, Tool};
use crate::error::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use tokio::sync::{mpsc, oneshot};

pub struct AskUserTool;

pub struct AskUserRequest {
    pub id: u64,
    pub question: String,
    pub response: oneshot::Sender<String>,
}

static NEXT_ASK_ID: AtomicU64 = AtomicU64::new(1);
static GUI_ASK_SENDER: OnceLock<Mutex<Option<mpsc::UnboundedSender<AskUserRequest>>>> =
    OnceLock::new();

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
                return Ok(normalize_answer(answer_rx.await.unwrap_or_default()));
            }
        }

        let answer = tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Write};
            println!("\n\x1b[36m[agent asks]: {question}\x1b[0m");
            print!("\x1b[36m> \x1b[0m");
            std::io::stdout().flush().ok();
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).ok();
            line.trim().to_string()
        })
        .await
        .unwrap_or_default();

        Ok(normalize_answer(answer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    #[tokio::test]
    async fn gui_ask_sender_round_trips_answer() {
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
}
