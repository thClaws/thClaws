//! Image and video on an AIServer Suite (the DGX box, or a Suite set in
//! Settings), through its API v1. Every call starts a job (`async`) read at
//! `GET /v1/jobs/{id}`, whose result is `GET /v1/jobs/{id}/artifacts/{n}`:
//! a slow model would otherwise hold one connection for its whole render.
//!
//! - text → image: `POST /v1/images/generations`.
//! - image → image: `POST /v1/images/compose` (Qwen-Image 2.1, up to 10
//!   reference images), or `/v1/images/refine` when the model is Z-Image,
//!   which polishes one picture and keeps its layout. `/v1/images/edits` is
//!   a painted-region edit, not general image to image.
//! - video: `POST /v1/videos/generations`.
//!
//! Registered only where a Suite is reachable ([`crate::aiserver::suite_reachable`])
//! and first in the order. On the box itself it also claims the empty model,
//! so media made there stays on the box unless a cloud model is named.

use crate::error::{Error, Result};
use crate::media::provider::{
    ImageModelInfo, ImageProvider, ImageRequest, ImageResult, JobState, ProviderJobRef,
    SpeechProvider, SpeechRequest, SpeechResult, VideoProvider, VideoRequest,
};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::{Client, RequestBuilder};
use serde_json::{json, Value};
use std::time::Duration;

pub const ID: &str = "aiserver";

const IMAGE_MODELS: &[ImageModelInfo] = &[
    ImageModelInfo {
        id: "z",
        aliases: &["aiserver", "local", "z-image", "z-image-turbo"],
        label: "Z-Image Turbo (ในเครื่อง)",
    },
    ImageModelInfo {
        id: "qwen21-turbo",
        aliases: &["qwen-image-2.1-turbo"],
        label: "Qwen-Image 2.1 Turbo (ในเครื่อง)",
    },
    ImageModelInfo {
        id: "qwen21",
        aliases: &["qwen-image-2.1"],
        label: "Qwen-Image 2.1 (ในเครื่อง)",
    },
];

const VIDEO_MODELS: &[ImageModelInfo] = &[ImageModelInfo {
    id: "video-default",
    aliases: &["aiserver", "local", "aiserver-video", "local-video"],
    label: "LTX (ในเครื่อง)",
}];

fn resolve_local(models: &[ImageModelInfo], raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return crate::aiserver::on_box()
            .then(|| models.first().map(|m| m.id.to_string()))
            .flatten();
    }
    models
        .iter()
        .find(|m| raw == m.id || m.aliases.contains(&raw))
        .map(|m| m.id.to_string())
}

fn endpoint() -> Result<(String, Option<String>)> {
    crate::aiserver::suite_api()
        .ok_or_else(|| Error::Tool("no AIServer Suite is reachable from this thClaws".into()))
}

fn client(secs: u64) -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(secs))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| Error::Tool(format!("http client: {e}")))
}

fn authed(rb: RequestBuilder, key: &Option<String>) -> RequestBuilder {
    let rb = crate::multi_tenant::attach_member(rb);
    match key {
        Some(k) => rb.bearer_auth(k),
        None => rb,
    }
}

/// The Suite's own words for a refusal (`error.code: error.message`).
async fn refusal(what: &str, resp: reqwest::Response) -> Error {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let said = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| {
            let e = &v["error"];
            let code = e["code"].as_str().or(e["type"].as_str()).unwrap_or("");
            let msg = e["message"].as_str().or(v["detail"].as_str()).unwrap_or("");
            (!code.is_empty() || !msg.is_empty()).then(|| {
                [code, msg]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(": ")
            })
        })
        .unwrap_or_else(|| body.chars().take(300).collect());
    Error::Tool(format!("AIServer {what} http {status}: {said}"))
}

/// The job id a `202` start answers with.
async fn started(what: &str, resp: reqwest::Response) -> Result<String> {
    if !resp.status().is_success() {
        return Err(refusal(what, resp).await);
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| Error::Tool(format!("AIServer {what} reply not json: {e}")))?;
    v["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::Tool(format!("AIServer {what} answered without a job id")))
}

/// One look at a job. `Done` carries its first artifact of `kind`
/// (`image` / `video`), or its first artifact of any kind.
async fn job_state(id: &str, kind: &str) -> Result<JobState> {
    let (base, key) = endpoint()?;
    let url = format!("{base}/v1/jobs/{id}");
    let resp = authed(client(60)?.get(&url), &key)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("AIServer job http: {e}")))?;
    if resp.status().as_u16() == 404 {
        return Ok(JobState::Failed {
            msg: "the AIServer no longer has this job".into(),
        });
    }
    if !resp.status().is_success() {
        return Err(refusal("job", resp).await);
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| Error::Tool(format!("AIServer job reply not json: {e}")))?;
    match v["state"].as_str().unwrap_or("") {
        "succeeded" => {
            let arts = v["artifacts"].as_array().cloned().unwrap_or_default();
            let art = arts
                .iter()
                .find(|a| a["type"].as_str().is_some_and(|t| t.starts_with(kind)))
                .or(arts.first())
                .ok_or_else(|| Error::Tool("AIServer job finished with no file".into()))?;
            let n = art["n"].as_u64().unwrap_or(0);
            let resp = authed(client(900)?.get(format!("{url}/artifacts/{n}")), &key)
                .send()
                .await
                .map_err(|e| Error::Tool(format!("AIServer download http: {e}")))?;
            if !resp.status().is_success() {
                return Err(refusal("download", resp).await);
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| Error::Tool(format!("AIServer download: {e}")))?;
            Ok(JobState::Done {
                bytes: bytes.to_vec(),
            })
        }
        state @ ("failed" | "cancelled") => {
            let e = &v["error"];
            let msg = e["message"]
                .as_str()
                .or(e["code"].as_str())
                .or(e.as_str())
                .unwrap_or("the AIServer gave no reason");
            Ok(JobState::Failed {
                msg: format!("{state}: {msg}"),
            })
        }
        _ => Ok(JobState::Running {
            pct: v["progress"]
                .as_f64()
                .map(|p| (p * 100.0).clamp(0.0, 100.0) as u8),
        }),
    }
}

/// How long an image job may take: a model's first load on the box alone
/// can run to minutes.
const IMAGE_WAIT: Duration = Duration::from_secs(900);

pub struct AiServerImageProvider;

impl AiServerImageProvider {
    /// The engine's aspect and size tiers as `WxH`, each side a multiple of 16.
    fn size(req: &ImageRequest) -> &'static str {
        let tier = match req.size.as_str() {
            "512" => 0,
            "2K" => 2,
            _ => 1,
        };
        match req.aspect_ratio.as_str() {
            "1:1" => ["512x512", "1024x1024", "1536x1536"][tier],
            "3:4" => ["480x640", "864x1152", "1152x1536"][tier],
            "4:3" => ["640x480", "1152x864", "1536x1152"][tier],
            "9:16" => ["432x768", "720x1280", "1088x1920"][tier],
            _ => ["768x432", "1280x720", "1920x1088"][tier],
        }
    }
}

#[async_trait]
impl ImageProvider for AiServerImageProvider {
    fn id(&self) -> &'static str {
        ID
    }
    fn models(&self) -> &'static [ImageModelInfo] {
        IMAGE_MODELS
    }
    fn resolve_model(&self, raw: &str) -> Option<String> {
        resolve_local(IMAGE_MODELS, raw)
    }

    async fn generate(&self, req: &ImageRequest) -> Result<ImageResult> {
        let (base, key) = endpoint()?;
        let size = Self::size(req);
        let client = client(120)?;
        let rb = if req.input_images.is_empty() {
            client
                .post(format!("{base}/v1/images/generations"))
                .json(&json!({ "model": req.model, "prompt": req.prompt, "size": size, "n": 1, "async": true }))
        } else {
            let refine = req.model == "z";
            let mut form = reqwest::multipart::Form::new()
                .text("prompt", req.prompt.clone())
                .text("async", "true");
            if !refine {
                form = form.text("size", size);
            }
            let take = if refine { 1 } else { 10 };
            for (i, img) in req.input_images.iter().take(take).enumerate() {
                let ext = match img.mime.as_str() {
                    "image/jpeg" => "jpg",
                    "image/webp" => "webp",
                    _ => "png",
                };
                let part = reqwest::multipart::Part::bytes(img.bytes.clone())
                    .file_name(format!("input-{i}.{ext}"))
                    .mime_str(&img.mime)
                    .map_err(|e| Error::Tool(format!("multipart part: {e}")))?;
                form = form.part(if refine { "image" } else { "images[]" }, part);
            }
            let path = if refine { "refine" } else { "compose" };
            client
                .post(format!("{base}/v1/images/{path}"))
                .multipart(form)
        };
        let resp = authed(rb, &key)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("AIServer image http: {e}")))?;
        let id = started("image", resp).await?;
        let deadline = tokio::time::Instant::now() + IMAGE_WAIT;
        loop {
            match job_state(&id, "image").await? {
                JobState::Done { bytes } => return Ok(ImageResult { bytes }),
                JobState::Failed { msg } => {
                    return Err(Error::Tool(format!("AIServer image job {msg}")))
                }
                JobState::Running { .. } => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Tool(format!(
                    "AIServer image job {id} did not finish in {} minutes",
                    IMAGE_WAIT.as_secs() / 60
                )));
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
    }
}

pub struct AiServerVideoProvider;

impl AiServerVideoProvider {
    fn body(req: &VideoRequest) -> Value {
        let mut body = json!({
            "prompt": req.prompt,
            "seconds": req.duration_seconds.clamp(1, 60),
            "resolution": if req.resolution.eq_ignore_ascii_case("1080p") { "1080p" } else { "720p" },
            "aspect": if req.aspect_ratio == "9:16" { "9:16" } else { "16:9" },
            "audio": req.generate_audio,
        });
        if let Some(fps) = req.fps.filter(|f| (12..=50).contains(f)) {
            body["fps"] = fps.into();
        }
        // The Suite takes the picture as a data URL, not bare base64.
        if let Some(img) = &req.init_image {
            body["image"] = format!("data:{};base64,{}", img.mime, B64.encode(&img.bytes)).into();
        }
        body
    }
}

#[async_trait]
impl VideoProvider for AiServerVideoProvider {
    fn id(&self) -> &'static str {
        ID
    }
    fn models(&self) -> &'static [ImageModelInfo] {
        VIDEO_MODELS
    }
    fn resolve_model(&self, raw: &str) -> Option<String> {
        resolve_local(VIDEO_MODELS, raw)
    }
    fn fixed_aspects(&self) -> bool {
        true
    }

    async fn submit(&self, req: &VideoRequest) -> Result<ProviderJobRef> {
        let (base, key) = endpoint()?;
        let rb = client(300)?
            .post(format!("{base}/v1/videos/generations"))
            .json(&Self::body(req));
        let resp = authed(rb, &key)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("AIServer video submit http: {e}")))?;
        Ok(ProviderJobRef {
            op: started("video submit", resp).await?,
        })
    }

    async fn poll(&self, job: &ProviderJobRef) -> Result<JobState> {
        job_state(&job.op, "video").await
    }
}

const SPEECH_MODELS: &[ImageModelInfo] = &[ImageModelInfo {
    id: "tts-default",
    aliases: &["aiserver", "local", "aiserver-tts", "local-tts"],
    label: "เสียงในเครื่อง",
}];

/// Speech on the Suite: `POST /v1/audio/speech` (OpenAI shape) answers with
/// the audio itself. Voices are the box's own (`GET /v1/audio/voices`), the
/// ones voice mode offers.
pub struct AiServerSpeechProvider;

impl AiServerSpeechProvider {
    async fn speak(text: &str, voice: &str) -> Result<reqwest::Response> {
        let (base, key) = endpoint()?;
        // One TTS worker per box; a voice's first load alone can take minutes.
        let rb = client(600)?
            .post(format!("{base}/v1/audio/speech"))
            .json(&json!({ "model": "tts-default", "input": text, "voice": voice, "response_format": "wav" }));
        authed(rb, &key)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("AIServer speech http: {e}")))
    }
}

#[async_trait]
impl SpeechProvider for AiServerSpeechProvider {
    fn id(&self) -> &'static str {
        ID
    }
    fn models(&self) -> &'static [ImageModelInfo] {
        SPEECH_MODELS
    }
    fn resolve_model(&self, raw: &str) -> Option<String> {
        resolve_local(SPEECH_MODELS, raw)
    }

    async fn synthesize(&self, req: &SpeechRequest) -> Result<SpeechResult> {
        let asked = req.voice.trim();
        let voice = if asked.is_empty() {
            crate::aiserver::VOICE_DEFAULT
        } else {
            asked
        };
        let mut resp = Self::speak(&req.text, voice).await?;
        // A cloud voice name (Kore, Charon…) means nothing to the box: say it
        // in the box's default voice instead of refusing.
        if !resp.status().is_success() && voice != crate::aiserver::VOICE_DEFAULT {
            let err = refusal("speech", resp).await;
            if !err.to_string().contains("voice_not_found") {
                return Err(err);
            }
            resp = Self::speak(&req.text, crate::aiserver::VOICE_DEFAULT).await?;
        }
        if !resp.status().is_success() {
            return Err(refusal("speech", resp).await);
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::Tool(format!("AIServer speech body: {e}")))?;
        Ok(SpeechResult {
            bytes: bytes.to_vec(),
            ext: "wav",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_req(aspect: &str, size: &str) -> ImageRequest {
        ImageRequest {
            model: "z".into(),
            prompt: "p".into(),
            input_images: vec![],
            aspect_ratio: aspect.into(),
            size: size.into(),
            text: vec![],
            font: None,
        }
    }

    #[test]
    fn every_size_is_a_multiple_of_16() {
        for aspect in ["1:1", "3:4", "4:3", "9:16", "16:9"] {
            for size in ["512", "1K", "2K"] {
                let s = AiServerImageProvider::size(&image_req(aspect, size));
                let (w, h) = s.split_once('x').unwrap();
                for side in [w, h] {
                    assert_eq!(side.parse::<u32>().unwrap() % 16, 0, "{aspect} {size}: {s}");
                }
            }
        }
    }

    #[test]
    fn names_resolve_and_unknown_ones_do_not() {
        assert_eq!(resolve_local(IMAGE_MODELS, "z").as_deref(), Some("z"));
        assert_eq!(resolve_local(IMAGE_MODELS, "local").as_deref(), Some("z"));
        assert_eq!(
            resolve_local(IMAGE_MODELS, "qwen21-turbo").as_deref(),
            Some("qwen21-turbo")
        );
        assert_eq!(resolve_local(IMAGE_MODELS, "flash"), None);
        assert_eq!(
            resolve_local(VIDEO_MODELS, "aiserver").as_deref(),
            Some("video-default")
        );
    }

    #[test]
    fn video_body_maps_the_portable_tiers() {
        let b = AiServerVideoProvider::body(&VideoRequest {
            model: "video-default".into(),
            prompt: "p".into(),
            init_image: None,
            aspect_ratio: "1:1".into(),
            duration_seconds: 500,
            resolution: "1080P".into(),
            fps: None,
            generate_audio: true,
        });
        assert_eq!(b["aspect"], "16:9");
        assert_eq!(b["seconds"], 60);
        assert_eq!(b["resolution"], "1080p");
        assert_eq!(b["audio"], true);
        assert!(b.get("image").is_none());
        assert!(b.get("fps").is_none());
    }

    #[test]
    fn video_body_sends_the_picture_as_a_data_url() {
        let b = AiServerVideoProvider::body(&VideoRequest {
            model: "video-default".into(),
            prompt: "p".into(),
            init_image: Some(crate::media::provider::InputImage {
                bytes: vec![1, 2, 3],
                mime: "image/jpeg".into(),
            }),
            aspect_ratio: "9:16".into(),
            duration_seconds: 5,
            resolution: "720P".into(),
            fps: Some(48),
            generate_audio: false,
        });
        assert_eq!(b["image"], "data:image/jpeg;base64,AQID");
        assert_eq!(b["aspect"], "9:16");
        assert_eq!(b["fps"], 48);
    }
}
