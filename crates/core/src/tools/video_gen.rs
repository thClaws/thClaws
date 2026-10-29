//! `TextToVideo` + `ImageToVideo` + `MediaJobStatus` — async video
//! generation (dev-plan/40, Tier 2).
//!
//! Video generation is long-running (~20s–3min), so these tools
//! split submit from poll:
//!
//!   - `TextToVideo` / `ImageToVideo` submit a job via a `VideoProvider`
//!     (`media::registry`), persist it to the `media::job` store, and
//!     return a `job_id` immediately — they do NOT block on the render.
//!   - `MediaJobStatus` polls a job by id; when the render completes it
//!     downloads the clip to `output/vid-<ts>-<sha8>.mp4` and flips the
//!     job to done. Poll survives a restart (the job log persists the
//!     provider operation ref).
//!
//! Opt-in: registered under the same `imageToolsEnabled` flag as the
//! image tools (media generation). Video is expensive (≈ $0.10–0.40 per
//! second), so the submit tools require approval; polling does not.

use crate::error::{Error, Result};
use crate::media::job::{self, MediaJob, STATUS_DONE, STATUS_FAILED, STATUS_RUNNING};
use crate::media::provider::{InputImage, JobState, ProviderJobRef, VideoProvider, VideoRequest};
use crate::media::{registry, save_video, sniff_video_ext};
use crate::tools::{req_str, Tool};
use async_trait::async_trait;
use serde_json::{json, Value};

fn opt(input: &Value, key: &str) -> String {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn resolve_duration(input: &Value) -> u32 {
    let d = input.get("duration").and_then(|v| v.as_u64()).unwrap_or(8) as u32;
    // 4–8 s is what every duration-taking provider accepts (LTX snaps to
    // its grid, DashScope clamps); clamp rather than reject so a stray
    // value still produces a clip. Omni has no duration knob (~10 s).
    d.clamp(4, 8)
}

fn resolve_aspect(input: &Value) -> String {
    let a = opt(input, "aspect_ratio");
    if a.is_empty() {
        "16:9".into()
    } else {
        a
    }
}

/// A `-t2v` model given an image would render from the prompt alone —
/// DashScope accepts the frame and ignores it (seen 2026-10-22 with the
/// SIS default). Swap to the `-i2v` sibling, or refuse.
fn image_model(provider: &dyn VideoProvider, model: String) -> Result<String> {
    if !model.contains("-t2v") {
        return Ok(model);
    }
    let i2v = model.replace("-t2v", "-i2v");
    if provider.models().iter().any(|m| m.id == i2v) {
        return Ok(i2v);
    }
    Err(Error::Tool(format!(
        "{model} is text→video and has no image→video sibling — use happyhorse-1.0-i2v, \
         wan2.7-i2v or wan2.6-i2v"
    )))
}

fn resolve_resolution(input: &Value) -> String {
    match opt(input, "resolution").as_str() {
        "1080P" | "1080p" => "1080P".into(),
        // Omni and LTX only; DashScope clamps to 1080P.
        "4K" | "4k" | "2160P" | "2160p" => "4K".into(),
        // Omni only; the others treat it as 720P.
        "360P" | "360p" => "360P".into(),
        _ => "720P".into(),
    }
}

const MODEL_DESC: &str = "Video model. Provider inferred from the model. Gemini Omni Flash: \
`omni` (default; gemini-omni-1.1-flash — Google's successor to Veo, which shut down \
2026-10-22; the old Veo names map to it) — always ~10s with audio, `resolution` \
360P/720P/1080P/4K, ignores `duration`. LTX: `ltx` \
(ltx-2-3-fast) or `ltx-pro` for 2.3, `ltx-2-5` (ltx-2-5-fast) or `ltx-2-5-pro` \
for the newer line — native speech incl. Thai, honors `resolution` \
(720P/1080P/4K) and `fps`. DashScope HappyHorse: `happyhorse-1.0-t2v` (text→video) / \
`happyhorse-1.0-i2v` (image→video) — honor `resolution` (720P/1080P). DashScope Wan: \
`wan2.7-t2v` (alias `wan`) / `wan2.7-i2v` (`wan-i2v`), `wan2.6-t2v` / `wan2.6-i2v`, \
`wan2.5-t2v-preview` (5 or 10s), `wan2.2-t2v-plus` / `wan2.1-t2v-turbo` (5s). \
Default: omni (where the gateway serves only DashScope: happyhorse-1.0-t2v).";

fn model_desc() -> String {
    match registry::reachable_video_models() {
        Some(ids) => {
            format!("{MODEL_DESC} This workspace can only reach: {ids} — pick one of these.")
        }
        None => MODEL_DESC.to_string(),
    }
}

/// Submit a video job and persist it; returns the user-facing text.
async fn submit_job(kind: &str, input: &Value, init_image: Option<InputImage>) -> Result<String> {
    let prompt = req_str(input, "prompt")?.to_string();
    let (provider, model) = registry::resolve_video(&opt(input, "provider"), &opt(input, "model"))?;
    let model = if init_image.is_some() {
        image_model(provider.as_ref(), model)?
    } else {
        model
    };
    let asked = opt(input, "aspect_ratio");
    let (init_image, aspect) = match init_image {
        Some(img) if provider.fixed_aspects() => {
            let (b, m, a) = crate::media::pad_to_video_aspect(
                img.bytes,
                img.mime,
                (!asked.is_empty()).then_some(asked.as_str()),
            );
            let (bytes, mime) = crate::media::fit_input_image(b, m);
            (Some(InputImage { bytes, mime }), a.to_string())
        }
        other => (other, resolve_aspect(input)),
    };
    let duration = provider
        .fixed_duration()
        .unwrap_or_else(|| resolve_duration(input));
    let fps = input
        .get("fps")
        .and_then(Value::as_u64)
        .map(|f| f as u32)
        .filter(|f| matches!(f, 24 | 25 | 48 | 50));
    let generate_audio = input
        .get("generate_audio")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let req = VideoRequest {
        model: model.clone(),
        prompt,
        init_image,
        aspect_ratio: aspect,
        duration_seconds: duration,
        fps,
        generate_audio,
        resolution: resolve_resolution(input),
    };
    let job_ref = provider.submit(&req).await?;
    let id = MediaJob::new_id(&job_ref.op);
    let rec = MediaJob {
        id: id.clone(),
        kind: kind.to_string(),
        provider: provider.id().to_string(),
        model,
        op: job_ref.op,
        status: STATUS_RUNNING.to_string(),
        asset_path: None,
        error: None,
        duration_seconds: duration,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    job::create(&rec)?;
    Ok(format!(
        "Submitted {kind} job to {} ({}). job_id={id}, {duration}s.\n\
         Render is asynchronous — call MediaJobStatus with job_id=\"{id}\"; \
         it waits up to ~50s per call, so call it again until status is `done`.",
        provider.id(),
        rec.model,
    ))
}

// ─── TextToVideo ─────────────────────────────────────────────────

pub struct TextToVideoTool;

#[async_trait]
impl Tool for TextToVideoTool {
    fn name(&self) -> &'static str {
        "TextToVideo"
    }
    fn description(&self) -> &'static str {
        "Generate a short video clip from a text prompt. Providers: Gemini \
         Omni Flash (`omni`, the default; ~10s with audio, needs \
         `GEMINI_API_KEY`/`GOOGLE_API_KEY`), LTX \
         (`ltx`/`ltx-pro` for 2.3, `ltx-2-5`/`ltx-2-5-pro` for 2.5; needs \
         `LTX_API_KEY`; native audio incl. Thai speech, \
         `resolution` 720P/1080P/4K) or DashScope `happyhorse-1.0-t2v` (needs \
         `DASHSCOPE_API_KEY`; honors `resolution` 720P/1080P). Requires \
         `imageToolsEnabled: true` in \
         `.thclaws/settings.json`. Aspect ratios: 16:9 (default) or 9:16. \
         Duration: 4–8s (default 8). Video is expensive. Returns a job_id \
         immediately; poll MediaJobStatus until `done`."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "Description of the video: subject, action, camera motion, style." },
                "model": { "type": "string", "description": model_desc() },
                "provider": { "type": "string", "description": "Optional explicit provider (`omni` | `ltx` | `dashscope`).", "enum": ["omni", "ltx", "dashscope"] },
                "aspect_ratio": { "type": "string", "description": "16:9 (default) or 9:16.", "enum": ["16:9", "9:16", "1:1", "3:4", "4:3"] },
                "duration": { "type": "integer", "description": "Clip length in seconds, 4–8 (default 8).", "minimum": 4, "maximum": 8 },
                "resolution": { "type": "string", "description": "Output resolution for providers that support it (Omni, LTX, DashScope happyhorse). Default 720P; Omni also takes 360P, Omni and LTX 4K.", "enum": ["360P", "720P", "1080P", "4K"] },
                "fps": { "type": "integer", "description": "Frame rate — LTX only (24/25/48/50; default 25). 48 or 50 is the lever for clean speech: it resolves the P/B/M lip closures that 24/25 smears. Omni and DashScope ignore it.", "enum": [24, 25, 48, 50] },
                "generate_audio": { "type": "boolean", "description": "LTX only: generate the soundtrack (dialogue, ambience) with the video. Default true; set false for a silent clip. This is LTX's ONLY audio switch — the voice, language and accent come from the prompt, so quote the dialogue and name the language/accent (e.g. speaks in Thai with a natural Bangkok accent: \"…\")." }
            },
            "required": ["prompt"]
        })
    }
    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }
    async fn call(&self, input: Value) -> Result<String> {
        submit_job("text2video", &input, None).await
    }
}

// ─── ImageToVideo ────────────────────────────────────────────────

pub struct ImageToVideoTool;

#[async_trait]
impl Tool for ImageToVideoTool {
    fn name(&self) -> &'static str {
        "ImageToVideo"
    }
    fn description(&self) -> &'static str {
        "Animate an existing image into a short video clip (the image \
         conditions the first frame). Providers: Gemini Omni Flash (`omni`), \
         LTX (`ltx`/`ltx-pro` for 2.3, `ltx-2-5`/`ltx-2-5-pro` for 2.5 — the \
         reliable way to hold one character's face across shots) or DashScope `happyhorse-1.0-i2v` (honors `resolution`). Pass \
         `input_path` (a path under the workspace) + a `prompt` describing the \
         motion. Same gating + keys as TextToVideo. Returns a job_id; poll \
         MediaJobStatus until `done`."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "input_path": { "type": "string", "description": "Path to the source image inside the workspace (the first frame). PNG/JPEG/WebP." },
                "prompt": { "type": "string", "description": "Describe the motion/animation to apply to the image." },
                "model": { "type": "string", "description": model_desc() },
                "provider": { "type": "string", "enum": ["omni", "ltx", "dashscope"] },
                "aspect_ratio": { "type": "string", "enum": ["16:9", "9:16", "1:1", "3:4", "4:3"] },
                "duration": { "type": "integer", "minimum": 4, "maximum": 8 },
                "resolution": { "type": "string", "enum": ["720P", "1080P", "4K"] },
                "fps": { "type": "integer", "description": "Frame rate — LTX only (24/25/48/50; default 25). 48 or 50 is the lever for clean speech: it resolves the P/B/M lip closures that 24/25 smears. Omni and DashScope ignore it.", "enum": [24, 25, 48, 50] },
                "generate_audio": { "type": "boolean", "description": "LTX only: generate the soundtrack (dialogue, ambience) with the video. Default true; set false for a silent clip. This is LTX's ONLY audio switch — the voice, language and accent come from the prompt, so quote the dialogue and name the language/accent (e.g. speaks in Thai with a natural Bangkok accent: \"…\")." }
            },
            "required": ["input_path", "prompt"]
        })
    }
    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }
    async fn call(&self, input: Value) -> Result<String> {
        let input_path_raw = req_str(&input, "input_path")?;
        let abs = crate::sandbox::Sandbox::check(input_path_raw)
            .map_err(|e| Error::Tool(format!("input_path: {e}")))?;
        let bytes =
            std::fs::read(&abs).map_err(|e| Error::Tool(format!("read {}: {e}", abs.display())))?;
        let mime = match abs
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            _ => "image/png",
        }
        .to_string();
        let (bytes, mime) = crate::media::fit_input_image(bytes, mime);
        submit_job("image2video", &input, Some(InputImage { bytes, mime })).await
    }
}

// ─── MediaJobStatus ──────────────────────────────────────────────

/// How long one `MediaJobStatus` call waits for a render before
/// answering `running`, and how often it looks meanwhile.
const POLL_WAIT: std::time::Duration = std::time::Duration::from_secs(50);
const POLL_EVERY: std::time::Duration = std::time::Duration::from_secs(8);

pub struct MediaJobStatusTool;

#[async_trait]
impl Tool for MediaJobStatusTool {
    fn name(&self) -> &'static str {
        "MediaJobStatus"
    }
    fn description(&self) -> &'static str {
        "Check (and advance) an async media job submitted by TextToVideo / \
         ImageToVideo. Pass the `job_id`. While the render is in flight returns \
         `running` after waiting up to ~50s for it (call again); on completion downloads the clip to \
         `output/vid-<ts>-<sha8>.mp4` and returns `done` with the path; on \
         failure returns `failed` with the error. Polling is free (no new \
         generation cost) and resumes across restarts."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string", "description": "The job_id returned by TextToVideo / ImageToVideo." }
            },
            "required": ["job_id"]
        })
    }
    fn requires_approval(&self, _input: &Value) -> bool {
        false
    }
    async fn call(&self, input: Value) -> Result<String> {
        // Wait out a short render here instead of answering `running`
        // straight away: an agent polling every few seconds sends the same
        // call over and over, and some upstreams (Alibaba Model Studio)
        // refuse a conversation of repeated identical tool calls —
        // "Repetitive tool calls detected" ended a 1080p happyhorse turn.
        let id = req_str(&input, "job_id")?;
        let deadline = std::time::Instant::now() + POLL_WAIT;
        loop {
            let answer = advance(id).await?;
            if !answer.starts_with("running") || std::time::Instant::now() >= deadline {
                return Ok(answer);
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
    }
}

/// Report a media job, polling its provider once if it is still running
/// and saving the clip when it is done. `MediaJobStatus` and the cloud
/// heartbeat (which keeps fetching a render after its turn ended) both
/// come through here.
pub async fn advance(id: &str) -> Result<String> {
    let mut rec =
        job::get(id)?.ok_or_else(|| Error::Tool(format!("no media job with id {id:?}")))?;

    if rec.is_terminal() {
        return Ok(match rec.status.as_str() {
            STATUS_DONE => format!(
                "done — {}",
                rec.asset_path.as_deref().unwrap_or("(asset path missing)")
            ),
            _ => format!(
                "failed — {}",
                rec.error.as_deref().unwrap_or("(no error recorded)")
            ),
        });
    }

    // Still running — poll the provider once and advance.
    let (provider, _model) = registry::resolve_video(&rec.provider, &rec.model)?;
    let state = provider
        .poll(&ProviderJobRef { op: rec.op.clone() })
        .await?;
    match state {
        JobState::Running { pct } => Ok(match pct {
            Some(p) => format!("running ({p}%) — call MediaJobStatus again. job_id={id}"),
            None => format!("running — call MediaJobStatus again. job_id={id}"),
        }),
        JobState::Done { bytes } => {
            let path = save_video(&bytes, sniff_video_ext(&bytes))?;
            rec.status = STATUS_DONE.to_string();
            rec.asset_path = Some(path.display().to_string());
            job::update(&rec)?;
            Ok(format!(
                "done — {} ({} bytes, {}s {})",
                path.display(),
                bytes.len(),
                rec.duration_seconds,
                rec.model
            ))
        }
        JobState::Failed { msg } => {
            rec.status = STATUS_FAILED.to_string();
            rec.error = Some(msg.clone());
            job::update(&rec)?;
            Ok(format!("failed — {msg}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::media::registry;

    #[test]
    fn default_video_model_resolves_to_omni() {
        let (p, m) = registry::resolve_video("", "").expect("default resolves");
        assert_eq!(p.id(), "omni");
        assert_eq!(m, "gemini-omni-1.1-flash");
    }

    #[test]
    fn retired_veo_names_resolve_to_omni() {
        for raw in ["quality", "lite", "fast", "veo-3.1-fast-generate-preview"] {
            assert_eq!(
                registry::resolve_video("", raw).unwrap().1,
                "gemini-omni-1.1-flash",
                "{raw}"
            );
        }
        assert_eq!(registry::resolve_video("omni", "").unwrap().0.id(), "omni");
    }

    #[test]
    fn happyhorse_resolves_to_dashscope_video() {
        let (p, m) = registry::resolve_video("", "happyhorse-1.0-t2v").unwrap();
        assert_eq!(p.id(), "dashscope");
        assert_eq!(m, "happyhorse-1.0-t2v");
        // bare alias + explicit provider
        assert_eq!(
            registry::resolve_video("", "happyhorse").unwrap().0.id(),
            "dashscope"
        );
        assert_eq!(
            registry::resolve_video("dashscope", "").unwrap().1,
            "happyhorse-1.0-t2v"
        );
        // i2v sibling.
        let (pi, mi) = registry::resolve_video("", "happyhorse-1.0-i2v").unwrap();
        assert_eq!(pi.id(), "dashscope");
        assert_eq!(mi, "happyhorse-1.0-i2v");
        // Omni stays the default video provider.
        assert_eq!(registry::resolve_video("", "").unwrap().0.id(), "omni");
    }

    #[test]
    fn an_image_never_goes_to_a_text_to_video_model() {
        let (p, m) = registry::resolve_video("dashscope", "").unwrap();
        assert_eq!(m, "happyhorse-1.0-t2v");
        assert_eq!(
            super::image_model(p.as_ref(), m).unwrap(),
            "happyhorse-1.0-i2v"
        );
        assert_eq!(
            super::image_model(p.as_ref(), "wan2.7-t2v".into()).unwrap(),
            "wan2.7-i2v"
        );
        assert_eq!(
            super::image_model(p.as_ref(), "wan2.7-i2v".into()).unwrap(),
            "wan2.7-i2v"
        );
        assert!(super::image_model(p.as_ref(), "wan2.1-t2v-turbo".into()).is_err());
        let (o, om) = registry::resolve_video("", "").unwrap();
        assert_eq!(
            super::image_model(o.as_ref(), om).unwrap(),
            "gemini-omni-1.1-flash"
        );
    }

    #[test]
    fn unknown_video_model_errors() {
        assert!(registry::resolve_video("", "sora-2").is_err());
        assert!(registry::resolve_video("runway", "gen3").is_err());
    }

    #[test]
    fn duration_clamps_to_the_provider_range() {
        use serde_json::json;
        assert_eq!(super::resolve_duration(&json!({})), 8); // default
        assert_eq!(super::resolve_duration(&json!({"duration": 2})), 4); // below min → 4
        assert_eq!(super::resolve_duration(&json!({"duration": 6})), 6);
        assert_eq!(super::resolve_duration(&json!({"duration": 99})), 8); // above max → 8
    }
}
