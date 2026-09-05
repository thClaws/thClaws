//! HTTP sink: buffers records and POSTs them as a JSONL body in batches.
//! Flushes when the batch fills or every `flush_secs`, whichever comes
//! first; a failed POST drops that batch (fail-open) and counts it.

use super::sink::AuditSink;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub struct HttpSink {
    url: String,
    auth_header_template: Option<String>,
    batch: usize,
    buf: Arc<Mutex<Vec<String>>>,
    client: reqwest::Client,
    dropped: Arc<AtomicU64>,
}

impl HttpSink {
    pub fn new(
        url: String,
        auth_header_template: Option<String>,
        batch: usize,
        flush_secs: u64,
    ) -> Self {
        let sink = Self {
            url,
            auth_header_template,
            batch: batch.max(1),
            buf: Arc::new(Mutex::new(Vec::new())),
            client: reqwest::Client::new(),
            dropped: Arc::new(AtomicU64::new(0)),
        };
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            let flusher = sink.flusher();
            h.spawn(async move {
                let mut tick =
                    tokio::time::interval(std::time::Duration::from_secs(flush_secs.max(1)));
                loop {
                    tick.tick().await;
                    flusher.post_pending().await;
                }
            });
        }
        sink
    }

    /// Records this sink itself lost to failed POSTs. Added to the
    /// registry's count so `session_end.dropped` sees both.
    pub fn dropped_in_flight(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn flusher(&self) -> Flusher {
        Flusher {
            url: self.url.clone(),
            auth: self
                .auth_header_template
                .as_deref()
                .map(crate::providers::gateway::render_template),
            buf: self.buf.clone(),
            client: self.client.clone(),
            dropped: self.dropped.clone(),
        }
    }
}

#[derive(Clone)]
struct Flusher {
    url: String,
    auth: Option<String>,
    buf: Arc<Mutex<Vec<String>>>,
    client: reqwest::Client,
    dropped: Arc<AtomicU64>,
}

impl Flusher {
    async fn post_pending(&self) {
        let lines: Vec<String> = match self.buf.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(_) => return,
        };
        if lines.is_empty() {
            return;
        }
        let n = lines.len() as u64;
        let body = lines.join("\n") + "\n";
        let mut rb = self
            .client
            .post(&self.url)
            .header("content-type", "application/x-ndjson")
            .timeout(std::time::Duration::from_secs(10))
            .body(body);
        if let Some(a) = self.auth.as_deref().filter(|a| !a.is_empty()) {
            rb = rb.header("authorization", a);
        }
        match rb.send().await {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => {
                self.dropped.fetch_add(n, Ordering::Relaxed);
                eprintln!("[audit] http sink {}: {}", self.url, r.status());
            }
            Err(e) => {
                self.dropped.fetch_add(n, Ordering::Relaxed);
                eprintln!("[audit] http sink {}: {e}", self.url);
            }
        }
    }
}

impl AuditSink for HttpSink {
    fn name(&self) -> &'static str {
        "http"
    }

    fn write(&self, line: &str) -> Result<(), String> {
        let full = {
            let mut g = self
                .buf
                .lock()
                .map_err(|_| "audit http buffer poisoned".to_string())?;
            g.push(line.to_string());
            g.len() >= self.batch
        };
        if full {
            if let Ok(h) = tokio::runtime::Handle::try_current() {
                let f = self.flusher();
                h.spawn(async move { f.post_pending().await });
            }
        }
        Ok(())
    }

    fn dropped_extra(&self) -> u64 {
        self.dropped_in_flight()
    }

    fn flush(&self) {
        let f = self.flusher();
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(async move { f.post_pending().await });
            }
            Err(_) => {
                // Process exit outside a runtime: best-effort blocking send.
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    rt.block_on(f.post_pending());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn batches_post_as_ndjson_with_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audit"))
            .and(header("authorization", "Bearer t0k"))
            .and(header("content-type", "application/x-ndjson"))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
            .await;
        std::env::set_var("THCLAWS_TEST_AUDIT_TOKEN", "t0k");
        let sink = HttpSink::new(
            format!("{}/audit", server.uri()),
            Some("Bearer {{env:THCLAWS_TEST_AUDIT_TOKEN}}".into()),
            2,
            3600,
        );
        sink.write(r#"{"v":1}"#).unwrap();
        sink.write(r#"{"v":1}"#).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(String::from_utf8_lossy(&reqs[0].body).lines().count(), 2);
        assert_eq!(sink.dropped_in_flight(), 0);
    }

    #[tokio::test]
    async fn failed_post_counts_drops_and_never_errors_write() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let sink = HttpSink::new(server.uri(), None, 1, 3600);
        assert!(sink.write("{}").is_ok());
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(sink.dropped_in_flight(), 1);
    }
}
