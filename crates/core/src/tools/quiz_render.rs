//! QuizRender — render an interactive study-quiz widget inline in chat.
//!
//! Fully self-contained: the agent generates the questions (LLM turn), calls
//! this tool with `{title, questions}`, and the tool returns a `UiResource`
//! whose `html` is a complete, offline quiz player (no MCP server, no Docker,
//! no CDN). The questions are inlined into the HTML at build time, so a
//! rendered widget never reads back any shared file — making it safe across
//! concurrent sessions/windows. No files are written to disk.
//!
//! Supported question types (mirrors the `/quiz` prompt schema):
//!   mcq        { stem, choices[], answer:index, explanation }
//!   truefalse  { stem, answer:bool, explanation }
//!   short      { stem, answer, accept[], keywords[], explanation }  (graded locally)
//!   match      { stem, pairs:[[left,right],...], explanation }

use super::{Tool, UiResource};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Mutex;

/// Self-contained player, embedded at compile time. Contains the token
/// `__QUIZ_DATA__` (exactly once) which `call()` replaces with a JS string
/// literal of the quiz JSON.
const PLAYER_TEMPLATE: &str = include_str!("../../resources/quiz_player.html");
const QUIZ_DATA_TOKEN: &str = "__QUIZ_DATA__";

pub struct QuizRenderTool {
    /// HTML built during the most recent `call()`, read back by
    /// `fetch_ui_resource()`. The agent loop runs `call()` then
    /// `fetch_ui_resource()` back-to-back on the same instance with no
    /// interleaving await on this tool (same pattern as `games::play`),
    /// so a single-slot stash is sufficient and collision-free per turn.
    last_html: Mutex<Option<String>>,
}

impl Default for QuizRenderTool {
    fn default() -> Self {
        Self::new()
    }
}

impl QuizRenderTool {
    pub fn new() -> Self {
        Self {
            last_html: Mutex::new(None),
        }
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Validate + normalize the questions array (mirror of the JS `normalize()`
/// in the previous StudyQuiz engine). Drops malformed entries; never panics.
fn normalize_questions(raw: Option<&Value>) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(arr) = raw.and_then(|v| v.as_array()) else {
        return out;
    };
    for q in arr {
        let Some(typ) = q.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        let typ = typ.to_lowercase();
        let stem = match q.get("stem").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => continue,
        };
        let explanation = q
            .get("explanation")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        match typ.as_str() {
            "mcq" => {
                let choices: Vec<String> = q
                    .get("choices")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().map(value_to_string).collect())
                    .unwrap_or_default();
                if choices.len() < 2 {
                    continue;
                }
                let mut answer = q.get("answer").and_then(|v| v.as_i64()).unwrap_or(0);
                if answer < 0 || answer as usize >= choices.len() {
                    answer = 0;
                }
                out.push(json!({"type":"mcq","stem":stem,"choices":choices,"answer":answer,"explanation":explanation}));
            }
            "truefalse" | "tf" => {
                let answer = match q.get("answer") {
                    Some(Value::Bool(b)) => *b,
                    Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
                    _ => false,
                };
                out.push(json!({"type":"truefalse","stem":stem,"answer":answer,"explanation":explanation}));
            }
            "short" => {
                let answer = q
                    .get("answer")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut accept = str_list(q.get("accept"));
                let keywords = str_list(q.get("keywords"));
                if !answer.is_empty() {
                    accept.push(answer.clone());
                }
                out.push(json!({"type":"short","stem":stem,"answer":answer,"accept":accept,"keywords":keywords,"explanation":explanation}));
            }
            "match" => {
                let pairs: Vec<Vec<String>> = q
                    .get("pairs")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| {
                                let pa = p.as_array()?;
                                if pa.len() < 2 {
                                    return None;
                                }
                                Some(vec![value_to_string(&pa[0]), value_to_string(&pa[1])])
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if pairs.len() < 2 {
                    continue;
                }
                out.push(
                    json!({"type":"match","stem":stem,"pairs":pairs,"explanation":explanation}),
                );
            }
            _ => continue,
        }
    }
    out
}

/// Build the self-contained player HTML with the quiz inlined as a safely
/// escaped JS string literal (defuses `</script>` and U+2028/U+2029 breakouts).
/// `source` is provenance shown on the review/result screen; `kms` (when
/// non-empty) is the knowledge base the quiz was drawn from — its presence is
/// what makes the player offer a "save score" button.
fn build_player_html(title: &str, source: &str, kms: &str, questions: &[Value]) -> Result<String> {
    let quiz = json!({ "title": title, "source": source, "kms": kms, "questions": questions });
    let inner = serde_json::to_string(&quiz)?; // the quiz JSON text
    let literal = serde_json::to_string(&inner)? // a valid JS string literal
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    Ok(PLAYER_TEMPLATE.replace(QUIZ_DATA_TOKEN, &literal))
}

#[async_trait]
impl Tool for QuizRenderTool {
    fn name(&self) -> &'static str {
        "QuizRender"
    }

    fn description(&self) -> &'static str {
        "Render an interactive study-quiz widget inline in the chat from \
         { title, questions[] }. Fully self-contained — writes no files, uses \
         no external server. Supports mcq, truefalse, short (free-text), and \
         match questions with local grading, per-question feedback, a score \
         screen, and an answer review. Call this after you have generated the \
         questions."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Short quiz title shown on the result screen." },
                "source": { "type": "string", "description": "Optional provenance: the URL, file path, or topic the quiz was built from." },
                "kms": { "type": "string", "description": "Optional knowledge-base name. Set this only for closed-book quizzes drawn from a KMS — it makes the player show a 'save score' button that records the attempt into this KMS's `_scores` page. Omit for URL/file/topic quizzes." },
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "description": "The quiz questions.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["mcq", "truefalse", "short", "match"] },
                            "stem": { "type": "string", "description": "The question text." },
                            "choices": { "type": "array", "items": { "type": "string" }, "description": "mcq: 2–4 answer options." },
                            "answer": { "description": "mcq: 0-based index into choices; truefalse: boolean; short: the canonical answer string." },
                            "accept": { "type": "array", "items": { "type": "string" }, "description": "short: additional acceptable answers (normalized match)." },
                            "keywords": { "type": "array", "items": { "type": "string" }, "description": "short: terms that must all appear to count as correct." },
                            "pairs": { "type": "array", "items": { "type": "array", "items": { "type": "string" } }, "description": "match: correct [left,right] pairings; the widget shuffles the right column." },
                            "explanation": { "type": "string", "description": "Shown after answering and in review." }
                        },
                        "required": ["type", "stem"]
                    }
                }
            },
            "required": ["title", "questions"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        false
    }

    async fn call(&self, input: Value) -> Result<String> {
        let title = input
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Quiz".to_string());
        let source = input
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let kms = input
            .get("kms")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let questions = normalize_questions(input.get("questions"));
        if questions.is_empty() {
            return Err(Error::Tool(
                "QuizRender: no valid questions — each needs a `type` (mcq/truefalse/short/match) \
                 and a non-empty `stem` (mcq needs ≥2 choices; match needs ≥2 pairs)."
                    .into(),
            ));
        }

        let html = build_player_html(&title, &source, &kms, &questions)?;
        *self.last_html.lock().unwrap() = Some(html);

        Ok(format!(
            "Quiz ready: {} question(s) — \"{}\". The playable widget is shown inline in the chat.",
            questions.len(),
            title
        ))
    }

    async fn fetch_ui_resource(&self) -> Option<UiResource> {
        let html = self.last_html.lock().unwrap().clone()?;
        Some(UiResource {
            uri: "ui://quiz/player".to_string(),
            html,
            mime: Some("text/html;profile=mcp-app".to_string()),
            allow_same_origin: false,
            auto_size: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_drops_malformed_and_coerces() {
        let raw = json!([
            { "type": "mcq", "stem": "q1", "choices": ["a","b","c"], "answer": 9 }, // answer clamps to 0
            { "type": "MCQ", "stem": "q1b", "choices": ["only-one"] },              // <2 choices -> dropped
            { "type": "tf", "stem": "q2", "answer": "true" },                       // tf -> truefalse, string coerced
            { "type": "short", "stem": "q3", "answer": "Paris", "accept": ["paris"] },
            { "type": "match", "stem": "q4", "pairs": [["a","1"],["b","2"]] },
            { "type": "match", "stem": "q4b", "pairs": [["a","1"]] },               // <2 pairs -> dropped
            { "type": "bogus", "stem": "q5" },                                      // unknown -> dropped
            { "stem": "no type" },                                                  // no type -> dropped
            { "type": "mcq" },                                                      // no stem -> dropped
        ]);
        let out = normalize_questions(Some(&raw));
        let types: Vec<&str> = out.iter().map(|q| q["type"].as_str().unwrap()).collect();
        assert_eq!(types, vec!["mcq", "truefalse", "short", "match"]);
        assert_eq!(out[0]["answer"].as_i64().unwrap(), 0); // clamped
        assert_eq!(out[1]["answer"].as_bool().unwrap(), true); // "true" -> true
                                                               // short pushes canonical answer into accept
        let accept: Vec<&str> = out[2]["accept"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(accept.contains(&"Paris"));
        assert!(accept.contains(&"paris"));
    }

    #[test]
    fn build_html_inlines_and_escapes() {
        let questions = normalize_questions(Some(&json!([
            { "type": "mcq", "stem": "Has a </script> breakout?", "choices": ["yes","no"], "answer": 1, "explanation": "e" }
        ])));
        let html = build_player_html("T", "", "", &questions).unwrap();
        // token fully substituted
        assert!(!html.contains(QUIZ_DATA_TOKEN));
        // the stem text survives (the player will JSON.parse it back)
        assert!(html.contains("\\u003c/script>"));
        // the only literal </script> is the player's own closing tag — the
        // injected one must be escaped, so exactly one remains.
        assert_eq!(html.matches("</script>").count(), 1);
    }

    #[test]
    fn build_html_inlines_kms_and_source() {
        let questions = normalize_questions(Some(&json!([
            { "type": "mcq", "stem": "q?", "choices": ["a","b"], "answer": 0 }
        ])));
        let html = build_player_html("T", "KMS: bio101", "bio101", &questions).unwrap();
        // both provenance fields ride into the inlined quiz JSON so the
        // save-score callback can echo them back to the agent.
        assert!(html.contains("bio101"));
        assert!(html.contains("KMS: bio101"));
    }

    #[test]
    fn build_html_without_kms_keeps_empty_field() {
        // absent kms/source -> empty strings, still a valid template (the
        // player treats empty kms as falsy and shows no save button).
        let questions = normalize_questions(Some(&json!([
            { "type": "truefalse", "stem": "q?", "answer": true }
        ])));
        let html = build_player_html("T", "", "", &questions).unwrap();
        assert!(!html.contains(QUIZ_DATA_TOKEN));
        assert!(html.contains("\\\"kms\\\":\\\"\\\""));
    }

    #[test]
    fn kms_and_source_breakouts_are_escaped() {
        let questions = normalize_questions(Some(&json!([
            { "type": "mcq", "stem": "q?", "choices": ["a","b"], "answer": 0 }
        ])));
        let html = build_player_html("T", "</script>", "</script>", &questions).unwrap();
        // the new string fields go through the same escaping as the stem, so
        // the player's closing tag stays the only literal one.
        assert_eq!(html.matches("</script>").count(), 1);
    }

    #[tokio::test]
    async fn call_then_fetch_returns_widget() {
        let tool = QuizRenderTool::new();
        // before any call, no widget
        assert!(tool.fetch_ui_resource().await.is_none());
        let out = tool
            .call(json!({
                "title": "Solar",
                "questions": [{ "type": "mcq", "stem": "closest planet?", "choices": ["Mercury","Venus"], "answer": 0 }]
            }))
            .await
            .unwrap();
        assert!(out.contains("1 question"));
        let ui = tool
            .fetch_ui_resource()
            .await
            .expect("widget present after call");
        assert!(ui.html.contains("closest planet?"));
        assert!(!ui.allow_same_origin);
        assert!(ui.auto_size);
        assert_eq!(ui.uri, "ui://quiz/player");
    }

    #[tokio::test]
    async fn call_with_no_valid_questions_errors() {
        let tool = QuizRenderTool::new();
        let err = tool
            .call(json!({ "title": "x", "questions": [{ "type": "bogus", "stem": "" }] }))
            .await;
        assert!(err.is_err());
    }

    #[test]
    fn does_not_require_approval() {
        assert!(!QuizRenderTool::new().requires_approval(&json!({})));
    }
}
