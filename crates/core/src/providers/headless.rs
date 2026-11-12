//! Knobs `thclaws -p` sets for harness use (training rollouts, evals):
//! sampling parameters and a log of every request body sent and every
//! reply received.
//!
//! These are process globals rather than `StreamRequest` fields because
//! dozens of call sites build a request and only print mode sets them.
//! The OpenAI-compatible chat provider (`openai/`, `oai/`, `vllm/`, …) reads
//! both; other providers ignore them.

use super::{EventStream, ProviderEvent, Usage};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sampling {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub seed: Option<u64>,
}

static SAMPLING: Mutex<Sampling> = Mutex::new(Sampling {
    temperature: None,
    top_p: None,
    seed: None,
});

pub fn set_sampling(s: Sampling) {
    *SAMPLING.lock().unwrap_or_else(|e| e.into_inner()) = s;
}

pub fn sampling() -> Sampling {
    *SAMPLING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Adds `temperature` / `top_p` to an OpenAI-style body when set; absent
/// fields leave the server's defaults in charge.
pub fn apply_sampling(body: &mut Value) {
    let s = sampling();
    if let Some(t) = s.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(p) = s.top_p {
        body["top_p"] = json!(p);
    }
    if let Some(n) = s.seed {
        body["seed"] = json!(n);
    }
}

struct RequestLog {
    file: std::fs::File,
    start: Instant,
}

static REQUEST_LOG: Mutex<Option<RequestLog>> = Mutex::new(None);

/// Opens (appends to) the JSONL file every request body is written to.
/// Refuses a symlink; the file is created 0600 because bodies carry the
/// whole conversation.
pub fn open_request_log(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    #[cfg(windows)]
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::other("request log path is a symlink"));
    }
    let file = o.open(path)?;
    *REQUEST_LOG.lock().unwrap_or_else(|e| e.into_inner()) = Some(RequestLog {
        file,
        start: Instant::now(),
    });
    Ok(())
}

fn write_line(mut rec: Value) {
    let mut guard = REQUEST_LOG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(log) = guard.as_mut() else { return };
    rec["ts"] = json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    rec["t_ms"] = json!(log.start.elapsed().as_millis() as u64);
    let mut line = rec.to_string();
    line.push('\n');
    let _ = log.file.write_all(line.as_bytes());
}

fn log_open() -> bool {
    REQUEST_LOG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

/// One line per request: wall time, ms since the log opened, the URL and the
/// exact JSON body. Headers (and so the API key) are never written.
pub fn log_request(url: &str, body: &Value) {
    write_line(json!({"type": "request", "url": url, "body": body}));
}

#[derive(Default)]
struct Reply {
    text: String,
    reasoning: String,
    tool_calls: Vec<(String, String, String)>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    error: Option<String>,
}

impl Reply {
    fn take(&mut self, ev: &crate::error::Result<ProviderEvent>) {
        match ev {
            Ok(ProviderEvent::TextDelta(t)) => self.text.push_str(t),
            Ok(ProviderEvent::ThinkingDelta(t)) => self.reasoning.push_str(t),
            Ok(ProviderEvent::ToolUseStart { id, name, .. }) => {
                self.tool_calls
                    .push((id.clone(), name.clone(), String::new()))
            }
            Ok(ProviderEvent::ToolUseDelta { partial_json }) => {
                if let Some(call) = self.tool_calls.last_mut() {
                    call.2.push_str(partial_json);
                }
            }
            Ok(ProviderEvent::MessageStop { stop_reason, usage }) => {
                if stop_reason.is_some() {
                    self.finish_reason = stop_reason.clone();
                }
                if usage.is_some() {
                    self.usage = usage.clone();
                }
            }
            Err(e) => self.error = Some(e.to_string()),
            _ => {}
        }
    }

    fn record(&self) -> Value {
        let tool_calls: Vec<Value> = self
            .tool_calls
            .iter()
            .map(|(id, name, args)| {
                json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}})
            })
            .collect();
        json!({
            "type": "response",
            "message": {
                "role": "assistant",
                "content": (!self.text.is_empty()).then_some(&self.text),
                "reasoning_content": (!self.reasoning.is_empty()).then_some(&self.reasoning),
                "tool_calls": (!tool_calls.is_empty()).then_some(tool_calls),
            },
            "finish_reason": self.finish_reason,
            "usage": self.usage.as_ref().map(|u| json!({
                "prompt_tokens": u.input_tokens,
                "completion_tokens": u.output_tokens,
                "cache_read_input_tokens": u.cache_read_input_tokens,
            })),
            "error": self.error,
        })
    }
}

/// When the request log is open, passes the stream through unchanged and
/// writes the assembled reply (content, reasoning, tool calls with their raw
/// argument strings, finish_reason, usage) as a `response` line once it
/// ends. A stream dropped before its end (timeout) writes nothing.
pub fn log_responses(stream: EventStream) -> EventStream {
    use futures::StreamExt;
    if !log_open() {
        return stream;
    }
    let reply = std::sync::Arc::new(Mutex::new(Reply::default()));
    let seen = reply.clone();
    let tapped = stream.inspect(move |ev| seen.lock().unwrap_or_else(|e| e.into_inner()).take(ev));
    let done = futures::stream::once(async move {
        write_line(reply.lock().unwrap_or_else(|e| e.into_inner()).record());
    })
    .filter_map(|_| async { None });
    Box::pin(tapped.chain(done))
}

#[cfg(test)]
pub(crate) fn close_request_log() {
    *REQUEST_LOG.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_and_request_log() {
        let mut body = json!({"model": "m"});
        set_sampling(Sampling::default());
        apply_sampling(&mut body);
        assert!(body.get("temperature").is_none() && body.get("top_p").is_none());

        set_sampling(Sampling {
            temperature: Some(0.7),
            top_p: Some(0.9),
            seed: Some(42),
        });
        apply_sampling(&mut body);
        set_sampling(Sampling::default());
        assert_eq!(body["temperature"], json!(0.7));
        assert_eq!(body["top_p"], json!(0.9));
        assert_eq!(body["seed"], json!(42));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/requests.jsonl");
        open_request_log(&path).unwrap();
        log_request("http://x/v1/chat/completions", &body);
        log_request("http://x/v1/chat/completions", &json!({"n": 2}));
        let events: Vec<crate::error::Result<ProviderEvent>> = vec![
            Ok(ProviderEvent::ThinkingDelta("think ".into())),
            Ok(ProviderEvent::ThinkingDelta("more".into())),
            Ok(ProviderEvent::TextDelta("calling".into())),
            Ok(ProviderEvent::ToolUseStart {
                id: "c1".into(),
                name: "Bash".into(),
                thought_signature: None,
            }),
            Ok(ProviderEvent::ToolUseDelta {
                partial_json: "{\"command\":".into(),
            }),
            Ok(ProviderEvent::ToolUseDelta {
                partial_json: "\"ls\"}".into(),
            }),
            Ok(ProviderEvent::MessageStop {
                stop_reason: Some("tool_calls".into()),
                usage: None,
            }),
        ];
        let passed: Vec<_> = futures::executor::block_on(futures::StreamExt::collect::<Vec<_>>(
            log_responses(Box::pin(futures::stream::iter(events))),
        ));
        assert_eq!(passed.len(), 7);
        close_request_log();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["type"], json!("request"));
        let r = &lines[2];
        assert_eq!(r["type"], json!("response"));
        assert_eq!(r["finish_reason"], json!("tool_calls"));
        assert_eq!(r["message"]["reasoning_content"], json!("think more"));
        assert_eq!(r["message"]["content"], json!("calling"));
        assert_eq!(
            r["message"]["tool_calls"][0]["function"]["arguments"],
            json!("{\"command\":\"ls\"}")
        );
        assert_eq!(lines[0]["body"]["temperature"], json!(0.7));
        assert_eq!(lines[1]["body"]["n"], json!(2));
        assert!(lines[0]["t_ms"].is_u64() && lines[0]["ts"].is_string());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let link = dir.path().join("link.jsonl");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(open_request_log(&link).is_err());
        }
    }
}
