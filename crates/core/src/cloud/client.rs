//! HTTP client for the thClaws.cloud catalog backend.

use serde::{Deserialize, Serialize};

/// Live byte counters for a download, shared with the caller so it can
/// render a readout while the transfer runs. `total` is 0 until the
/// response arrives, and stays 0 when the far end sends no
/// `Content-Length` (chunked) — render bytes-so-far in that case.
#[derive(Debug, Default)]
pub struct DownloadProgress {
    pub received: std::sync::atomic::AtomicU64,
    pub total: std::sync::atomic::AtomicU64,
}

/// Stream a response body to a temp file so a large `.tar.gz` never rides in
/// memory. Returns the temp file (auto-deleted on drop). Pass `progress` to
/// have the counters updated as bytes land; `None` streams without a readout.
async fn stream_to_temp_with(
    res: reqwest::Response,
    progress: Option<std::sync::Arc<DownloadProgress>>,
) -> Result<tempfile::NamedTempFile, String> {
    use futures::StreamExt;
    use std::io::Write;
    use std::sync::atomic::Ordering;
    if let (Some(p), Some(total)) = (progress.as_ref(), res.content_length()) {
        p.total.store(total, Ordering::Relaxed);
    }
    let mut tmp = tempfile::NamedTempFile::new().map_err(|e| format!("temp: {}", e))?;
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("body: {}", e))?;
        if let Some(p) = progress.as_ref() {
            p.received.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
        tmp.as_file_mut()
            .write_all(&chunk)
            .map_err(|e| format!("write temp: {}", e))?;
    }
    tmp.as_file_mut()
        .flush()
        .map_err(|e| format!("flush temp: {}", e))?;
    Ok(tmp)
}

pub struct Client {
    base_url: String,
    http: reqwest::Client,
    /// No total-request timeout — for multi-hundred-MB / multi-GB workspace
    /// push/pull/export, where the 120 s cap on `http` would abort the transfer
    /// mid-stream (the client closes the upload and the edge returns a spurious
    /// 499). Keeps a connect timeout + TCP keepalive so a dead peer still fails.
    http_bulk: reqwest::Client,
    token: Option<String>,
}

impl Client {
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        let ua = concat!("thclaws-cli/", env!("CARGO_PKG_VERSION"));
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .user_agent(ua)
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("reqwest client"),
            http_bulk: reqwest::Client::builder()
                .user_agent(ua)
                .connect_timeout(std::time::Duration::from_secs(30))
                .tcp_keepalive(std::time::Duration::from_secs(60))
                // Force HTTP/1.1 for bulk sync transfers. A single large
                // streaming POST gains nothing from h2 multiplexing but
                // inherits its failure modes (flow-control stalls,
                // RST_STREAM on proxy churn) — a mid-upload cut surfaces
                // as an opaque "error sending request". h1 keeps the
                // byte stream dumb and robust through Traefik.
                .http1_only()
                .build()
                .expect("reqwest bulk client"),
            token,
        }
    }

    fn auth_header(&self) -> Result<String, String> {
        let t = self
            .token
            .as_deref()
            .ok_or("not logged in — paste your CLI token in Settings → thClaws.cloud (mint one at /dashboard)")?;
        Ok(format!("Bearer {}", t))
    }

    pub async fn me(&self) -> Result<Me, String> {
        let auth = self.auth_header()?;
        let res = self
            .http
            .get(format!("{}/api/auth/me", self.base_url))
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode: {}", e))
    }

    pub async fn list_agents(&self, mine: bool) -> Result<Vec<AgentSummary>, String> {
        let mut url = format!("{}/api/agents", self.base_url);
        if mine {
            url.push_str("?mine=true");
        }
        let mut req = self.http.get(&url);
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {}", t));
        }
        let res = req.send().await.map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode: {}", e))
    }

    pub async fn publish(&self, tarball: Vec<u8>) -> Result<PublishResult, String> {
        let auth = self.auth_header()?;
        let part = reqwest::multipart::Part::bytes(tarball)
            .file_name("agent.tar.gz")
            .mime_str("application/gzip")
            .map_err(|e| format!("mime: {}", e))?;
        let form = reqwest::multipart::Form::new().part("file", part);

        let res = self
            .http
            .post(format!("{}/api/agents/publish", self.base_url))
            .header("Authorization", auth)
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode: {}", e))
    }

    /// Publish a self-contained HTML file to the apps domain.
    ///
    /// Unlike every other call here, this one works **without a token**.
    /// That is the point: "show someone this page" should not require an
    /// account. The server decides the lifetime from whether the header
    /// arrived — an hour anonymously, three days signed in — so the
    /// client just sends what it has and reports what comes back.
    pub async fn publish_app(&self, html: Vec<u8>) -> Result<PublishedApp, String> {
        let mut req = self
            .http
            .post(format!("{}/api/apps", self.base_url))
            .header("Content-Type", "text/html; charset=utf-8")
            .body(html);
        // Anonymous is a supported mode, not a failure — attach the
        // token when there is one and carry on when there isn't.
        if let Some(t) = self.token.as_deref() {
            req = req.header("Authorization", format!("Bearer {}", t));
        }
        let res = req.send().await.map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            // The API answers a refusal (too big, rate-limited, quota)
            // with a plain sentence meant for the person who typed
            // /publish — pass it through rather than wrapping it in
            // status-code noise.
            let detail = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("detail").and_then(|d| d.as_str().map(String::from)))
                .unwrap_or(body);
            return Err(if status.as_u16() == 400 {
                detail
            } else {
                format!("status {}: {}", status, detail)
            });
        }
        res.json().await.map_err(|e| format!("decode: {}", e))
    }

    pub async fn download(
        &self,
        slug: &str,
        version: Option<&str>,
    ) -> Result<DownloadResult, String> {
        let auth = self.auth_header()?;
        let mut url = format!("{}/api/agents/{}/download", self.base_url, slug);
        if let Some(v) = version {
            url.push_str(&format!("?version={}", urlencoding::encode(v)));
        }
        let res = self
            .http
            .get(&url)
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        let version = res
            .headers()
            .get("x-agent-version")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let sha256 = res
            .headers()
            .get("x-agent-sha256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let uuid = res
            .headers()
            .get("x-agent-uuid")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let official = res
            .headers()
            .get("x-agent-official")
            .is_some_and(|v| v.as_bytes() == b"1");
        let bytes = res
            .bytes()
            .await
            .map_err(|e| format!("body: {}", e))?
            .to_vec();
        Ok(DownloadResult {
            version,
            sha256,
            uuid,
            official,
            bytes,
        })
    }

    // ---- workspace sync (dev-plan/51) ----

    /// Exchange the stored CLI token for a short-lived session JWT, used as
    /// Bearer to the hosted-workspace runner ingress.
    pub async fn cli_exchange(&self) -> Result<String, String> {
        let auth = self.auth_header()?;
        let res = self
            .http
            .post(format!("{}/api/auth/cli-exchange", self.base_url))
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "cli-exchange status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        let r: CliExchangeResp = res.json().await.map_err(|e| format!("decode: {}", e))?;
        Ok(r.token)
    }

    /// List the caller's hosted workspaces.
    pub async fn list_workspaces(&self) -> Result<Vec<WorkspaceSummary>, String> {
        let auth = self.auth_header()?;
        let res = self
            .http
            .get(format!("{}/api/hosted/workspaces", self.base_url))
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "list-workspaces status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json()
            .await
            .map_err(|e| format!("decode workspaces: {}", e))
    }

    /// `GET <ws_url>/workspace/sync/stat` (runner ingress, JWT auth).
    pub async fn ws_sync_stat(&self, ws_url: &str, jwt: &str) -> Result<SyncStatResp, String> {
        let res = self
            .http
            .get(format!(
                "{}/workspace/sync/stat",
                ws_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {}", jwt))
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "sync/stat status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode stat: {}", e))
    }

    /// Download the cloud workspace as a `.tar.gz`, streamed to a temp file so a
    /// large payload never rides in memory. Caller untars from the temp path.
    pub async fn ws_sync_pull(
        &self,
        ws_url: &str,
        jwt: &str,
        include_runtime: bool,
        progress: Option<std::sync::Arc<DownloadProgress>>,
    ) -> Result<tempfile::NamedTempFile, String> {
        let mut url = format!("{}/workspace/sync/pull", ws_url.trim_end_matches('/'));
        if include_runtime {
            url.push_str("?include_runtime=true");
        }
        let res = self
            .http_bulk
            .get(&url)
            .header("Authorization", format!("Bearer {}", jwt))
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if res.status() == reqwest::StatusCode::CONFLICT {
            return Err("cloud workspace is busy (active turn) — try again when idle".into());
        }
        if !res.status().is_success() {
            return Err(format!(
                "sync/pull status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        stream_to_temp_with(res, progress).await
    }

    /// Upload a `.tar.gz` (from `tarball_path`) to the cloud workspace,
    /// streamed off disk so a large tarball never rides in memory.
    /// `workspace_id` records the binding on the runner. When `progress` is set,
    /// the counter is bumped as each chunk is handed to the socket, so the
    /// caller can render a live byte/percentage readout.
    pub async fn ws_sync_push(
        &self,
        ws_url: &str,
        jwt: &str,
        tarball_path: &std::path::Path,
        delete: bool,
        workspace_id: &str,
        progress: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    ) -> Result<SyncPushResp, String> {
        let url = format!(
            "{}/workspace/sync/push?delete={}&workspace_id={}",
            ws_url.trim_end_matches('/'),
            delete,
            urlencoding::encode(workspace_id)
        );
        let file = tokio::fs::File::open(tarball_path)
            .await
            .map_err(|e| format!("open tarball: {}", e))?;
        let reader = tokio_util::io::ReaderStream::new(file);
        let body = match progress {
            Some(counter) => {
                use futures::StreamExt;
                use std::sync::atomic::Ordering;
                let stream = reader.inspect(move |chunk| {
                    if let Ok(c) = chunk {
                        counter.fetch_add(c.len() as u64, Ordering::Relaxed);
                    }
                });
                reqwest::Body::wrap_stream(stream)
            }
            None => reqwest::Body::wrap_stream(reader),
        };
        let res = self
            .http_bulk
            .post(&url)
            .header("Authorization", format!("Bearer {}", jwt))
            .header("Content-Type", "application/gzip")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if res.status() == reqwest::StatusCode::CONFLICT {
            return Err("cloud workspace is busy (active turn) — try again when idle".into());
        }
        if !res.status().is_success() {
            return Err(format!(
                "sync/push status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode push: {}", e))
    }

    // ---- P2 incremental ----

    /// GET the runner's content manifest. `Ok(None)` when the runner predates
    /// P2 (404) — the caller falls back to the full-tarball path.
    pub async fn ws_sync_manifest(
        &self,
        ws_url: &str,
        jwt: &str,
    ) -> Result<Option<Vec<crate::cloud::wssync::FileEntry>>, String> {
        let res = self
            .http
            .get(format!(
                "{}/workspace/sync/manifest",
                ws_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {}", jwt))
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if res.status() == reqwest::StatusCode::CONFLICT {
            return Err("cloud workspace is busy (active turn) — try again when idle".into());
        }
        if !res.status().is_success() {
            return Err(format!(
                "sync/manifest status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        Ok(Some(
            res.json()
                .await
                .map_err(|e| format!("decode manifest: {}", e))?,
        ))
    }

    /// POST a path list; download a partial `.tar.gz` of just those files,
    /// streamed to a temp file. Caller untars from the temp path.
    pub async fn ws_sync_export(
        &self,
        ws_url: &str,
        jwt: &str,
        paths: &[String],
        progress: Option<std::sync::Arc<DownloadProgress>>,
    ) -> Result<tempfile::NamedTempFile, String> {
        let res = self
            .http_bulk
            .post(format!(
                "{}/workspace/sync/export",
                ws_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {}", jwt))
            .json(paths)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "sync/export status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        stream_to_temp_with(res, progress).await
    }

    /// POST a path list to move to `.sync-trash/` on the runner.
    pub async fn ws_sync_trash(
        &self,
        ws_url: &str,
        jwt: &str,
        paths: &[String],
    ) -> Result<SyncPushResp, String> {
        let res = self
            .http
            .post(format!(
                "{}/workspace/sync/trash",
                ws_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {}", jwt))
            .json(paths)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "sync/trash status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode trash: {}", e))
    }

    /// Record the agreed sync revision on the runner so both ends name the
    /// same number. One endpoint for both directions (push and pull alike
    /// end by agreeing on a revision), called once a sync has actually
    /// landed. `Ok(None)` when the runner predates revisions (404) — the
    /// client keeps its own count and the next sync re-converges.
    pub async fn ws_sync_set_revision(
        &self,
        ws_url: &str,
        jwt: &str,
        revision: u64,
        workspace_id: &str,
    ) -> Result<Option<u64>, String> {
        let res = self
            .http
            .post(format!(
                "{}/workspace/sync/revision",
                ws_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {}", jwt))
            .json(&serde_json::json!({
                "revision": revision,
                "workspace_id": workspace_id,
            }))
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !res.status().is_success() {
            return Err(format!(
                "sync/revision status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        let r: SyncRevisionResp = res
            .json()
            .await
            .map_err(|e| format!("decode revision: {}", e))?;
        Ok(Some(r.revision))
    }

    /// Wake (resume) a paused workspace pod (dev-plan/51 P3a auto-resume).
    pub async fn wake_workspace(&self, id: &str) -> Result<(), String> {
        let auth = self.auth_header()?;
        let res = self
            .http
            .post(format!(
                "{}/api/hosted/workspaces/{}/wake",
                self.base_url, id
            ))
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "wake status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CliExchangeResp {
    pub token: String,
    #[serde(default)]
    pub expires_in: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceSummary {
    pub id: String,
    pub slug: String,
    pub url: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyncStatResp {
    #[serde(default)]
    pub file_count: usize,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub empty: bool,
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// The runner's recorded sync revision. `None` from an engine that
    /// predates revisions — the client then counts from its own binding.
    #[serde(default)]
    pub revision: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyncRevisionResp {
    #[serde(default)]
    pub revision: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyncPushResp {
    #[serde(default)]
    pub written: usize,
    #[serde(default)]
    pub deleted: usize,
    #[serde(default)]
    pub trashed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Me {
    pub email: String,
    pub display_name: Option<String>,
    pub can_publish: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummary {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub categories: Vec<String>,
    pub tags: Vec<String>,
    pub current_version: Option<String>,
    pub purchase_usd: f64,
    pub author_handle: Option<String>,
}

/// A single-file HTML app published to `<id>.thclaws.app`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishedApp {
    pub id: String,
    pub url: String,
    pub title: Option<String>,
    pub size_bytes: u64,
    /// RFC 3339. The link stops working at this instant — 1 hour when
    /// published without a token, 3 days with one.
    pub expires_at: String,
    /// True when no CLI token was sent. Surfaced so the CLI can say why
    /// the link is short-lived instead of leaving the user to notice.
    pub anonymous: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishResult {
    pub slug: String,
    pub version: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub url: String,
    /// Server-assigned UUID for this agent. Stable across versions and
    /// across folder renames — the CLI writes this back to
    /// `./.thclaws/settings.json::agent.uuid` so re-publish from the
    /// same folder targets the same catalog entry.
    pub uuid: String,
}

#[derive(Debug)]
pub struct DownloadResult {
    pub version: String,
    pub sha256: String,
    /// Server-authoritative UUID from the `X-Agent-UUID` header.
    /// Preferred over peeking inside the tarball — the on-disk
    /// manifest.json may pre-date the Option-A identity split.
    pub uuid: Option<String>,
    /// `X-Agent-Official: 1`: owned by the platform publisher. An older
    /// server sends nothing, which reads as not official.
    pub official: bool,
    pub bytes: Vec<u8>,
}
