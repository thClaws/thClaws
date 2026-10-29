//! Gemini Omni Flash video provider — Google's replacement for Veo 3.1
//! (the Veo preview ids shut down 2026-10-22).
//!
//! Omni goes through the Interactions API, not `:predictLongRunning`:
//!   `POST /v1beta/interactions` with the model in the body. It answers
//!   synchronously (≈20 s for 360p, ≈45 s for 720p) with the finished clip
//!   and a `usage` block, which the gateway bills as tokens (video output
//!   $17.50/Mtok at 5,792 tokens per second of 720p ≈ $0.10/s).
//!
//! The trait is submit + poll, so `submit` makes the one generating call,
//! parks the bytes in a temp file and returns its path as the job ref;
//! `poll` hands them over. One POST per clip means one charge per clip —
//! a poll can never re-bill.
//!
//! Clips come back with audio and are always ~10 s: the API has no
//! duration knob, so `duration_seconds` is ignored. Delivery is `uri`
//! (inline base64 is refused past 4 MB), fetched through the gateway in
//! cloud mode like Veo's files.

use crate::error::{Error, Result};
use crate::media::provider::{
    ImageModelInfo, JobState, ProviderJobRef, VideoProvider, VideoRequest,
};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com";
const FILE_REF: &str = "omni-file:";

const MODELS: &[ImageModelInfo] = &[ImageModelInfo {
    id: "gemini-omni-1.1-flash",
    // "" = the default video model. The Veo aliases follow it: those ids
    // are gone, and a saved prompt asking for `veo`/`fast` should still
    // get a clip from Google's successor rather than an error.
    aliases: &[
        "",
        "omni",
        "omni-flash",
        "veo",
        "veo-fast",
        "fast",
        "quality",
        "lite",
    ],
    label: "Gemini Omni Flash",
}];

pub struct OmniVideoProvider;

impl OmniVideoProvider {
    /// Omni takes 16:9 (default) or 9:16 only.
    fn aspect(req: &VideoRequest) -> &'static str {
        match req.aspect_ratio.as_str() {
            "1:1" | "3:4" | "9:16" => "9:16",
            _ => "16:9",
        }
    }

    /// 360p / 720p (default) / 1080p / 4k — 1080p and 4k are upscaled.
    fn resolution(req: &VideoRequest) -> &'static str {
        match req.resolution.to_ascii_lowercase().as_str() {
            "360p" => "360p",
            "1080p" => "1080p",
            "4k" | "2160p" => "4k",
            _ => "720p",
        }
    }

    fn body(req: &VideoRequest) -> Value {
        let input = match &req.init_image {
            Some(img) => json!([
                { "type": "image", "data": B64.encode(&img.bytes), "mime_type": img.mime },
                { "type": "text", "text": req.prompt },
            ]),
            None => json!(req.prompt),
        };
        let mut body = json!({
            "model": req.model,
            "input": input,
            "response_format": {
                "type": "video",
                "aspect_ratio": Self::aspect(req),
                "resolution": Self::resolution(req),
                "delivery": "uri",
            },
        });
        if req.init_image.is_some() {
            body["generation_config"] = json!({ "video_config": { "task": "image_to_video" } });
        }
        body
    }

    /// The clip in a completed interaction: `steps[].content[]` of type
    /// `video`, as a URI or (if Google ignores `delivery`) inline base64.
    fn find_video(v: &Value) -> Option<Result<VideoOut>> {
        for step in v.get("steps")?.as_array()? {
            for c in step
                .get("content")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
            {
                if c.get("type").and_then(|t| t.as_str()) != Some("video") {
                    continue;
                }
                if let Some(uri) = c.get("uri").and_then(|u| u.as_str()) {
                    return Some(Ok(VideoOut::Uri(uri.to_string())));
                }
                if let Some(data) = c.get("data").and_then(|d| d.as_str()) {
                    return Some(
                        B64.decode(data)
                            .map(VideoOut::Bytes)
                            .map_err(|e| Error::Tool(format!("omni video base64: {e}"))),
                    );
                }
            }
        }
        None
    }

    /// Rewrite a Google file URI onto the gateway in cloud mode (the pod
    /// has no egress to Google), as Veo did.
    fn download_url(base_url: &str, uri: &str) -> String {
        match uri.split_once("generativelanguage.googleapis.com") {
            Some((_, suffix)) if base_url.ends_with("/google") => format!("{base_url}{suffix}"),
            _ => uri.to_string(),
        }
    }
}

enum VideoOut {
    Uri(String),
    Bytes(Vec<u8>),
}

#[async_trait]
impl VideoProvider for OmniVideoProvider {
    fn id(&self) -> &'static str {
        "omni"
    }
    fn fixed_aspects(&self) -> bool {
        true
    }

    fn models(&self) -> &'static [ImageModelInfo] {
        MODELS
    }
    fn fixed_duration(&self) -> Option<u32> {
        Some(10)
    }
    fn resolve_model(&self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        for m in MODELS {
            if raw == m.id || m.aliases.contains(&raw) {
                return Some(m.id.to_string());
            }
        }
        if raw.starts_with("gemini-omni-") || raw.starts_with("veo-") {
            return Some(MODELS[0].id.to_string());
        }
        None
    }

    async fn submit(&self, req: &VideoRequest) -> Result<ProviderJobRef> {
        let ep = crate::media::provider::resolve_endpoint(
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            GEMINI_BASE,
            "google",
        )?;
        let base = ep.base_url.trim_end_matches('/');
        let client = Client::builder()
            .timeout(Duration::from_secs(900))
            .build()
            .map_err(|e| Error::Tool(format!("http client: {e}")))?;
        let resp =
            crate::multi_tenant::attach_member(client.post(format!("{base}/v1beta/interactions")))
                .header("x-goog-api-key", &ep.api_key)
                .header("content-type", "application/json")
                .json(&Self::body(req))
                .send()
                .await
                .map_err(|e| Error::Tool(format!("omni http: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Tool(format!("omni body: {e}")))?;
        if !status.is_success() {
            return Err(Error::Tool(format!(
                "omni http {status}: {}",
                &text[..text.len().min(400)]
            )));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| Error::Tool(format!("omni response not json: {e}")))?;
        let bytes = match Self::find_video(&v) {
            Some(Ok(VideoOut::Bytes(b))) => b,
            Some(Ok(VideoOut::Uri(uri))) => {
                let resp =
                    crate::multi_tenant::attach_member(client.get(Self::download_url(base, &uri)))
                        .header("x-goog-api-key", &ep.api_key)
                        .send()
                        .await
                        .map_err(|e| Error::Tool(format!("omni download http: {e}")))?;
                if !resp.status().is_success() {
                    return Err(Error::Tool(format!("omni download http {}", resp.status())));
                }
                resp.bytes()
                    .await
                    .map_err(|e| Error::Tool(format!("omni download body: {e}")))?
                    .to_vec()
            }
            Some(Err(e)) => return Err(e),
            None => {
                return Err(Error::Tool(format!(
                    "omni returned no video (status {}): {}",
                    v.get("status").and_then(|s| s.as_str()).unwrap_or("?"),
                    &text[..text.len().min(300)]
                )))
            }
        };
        let path = std::env::temp_dir().join(format!("thclaws-omni-{}.mp4", uuid::Uuid::new_v4()));
        std::fs::write(&path, &bytes).map_err(|e| Error::Tool(format!("omni temp file: {e}")))?;
        Ok(ProviderJobRef {
            op: format!("{FILE_REF}{}", path.display()),
        })
    }

    async fn poll(&self, job: &ProviderJobRef) -> Result<JobState> {
        let Some(path) = job.op.strip_prefix(FILE_REF) else {
            return Ok(JobState::Failed {
                msg: format!("not an omni job ref: {}", job.op),
            });
        };
        match std::fs::read(path) {
            Ok(bytes) => {
                let _ = std::fs::remove_file(path);
                Ok(JobState::Done { bytes })
            }
            Err(e) => Ok(JobState::Failed {
                msg: format!("omni clip no longer on disk ({e}) — generate it again"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::provider::InputImage;

    fn req(image: bool) -> VideoRequest {
        VideoRequest {
            model: "gemini-omni-1.1-flash".into(),
            prompt: "a bicycle".into(),
            init_image: image.then(|| InputImage {
                bytes: vec![1, 2, 3],
                mime: "image/png".into(),
            }),
            aspect_ratio: "1:1".into(),
            duration_seconds: 8,
            resolution: "1080P".into(),
            fps: None,
            generate_audio: true,
        }
    }

    #[test]
    fn request_body_matches_the_interactions_api() {
        let b = OmniVideoProvider::body(&req(false));
        assert_eq!(b["model"], "gemini-omni-1.1-flash");
        assert_eq!(b["input"], "a bicycle");
        assert_eq!(b["response_format"]["aspect_ratio"], "9:16");
        assert_eq!(b["response_format"]["resolution"], "1080p");
        assert_eq!(b["response_format"]["delivery"], "uri");
        assert!(b.get("generation_config").is_none());

        let i = OmniVideoProvider::body(&req(true));
        assert_eq!(i["input"][0]["type"], "image");
        assert_eq!(i["input"][0]["data"], "AQID");
        assert_eq!(i["input"][1]["text"], "a bicycle");
        assert_eq!(
            i["generation_config"]["video_config"]["task"],
            "image_to_video"
        );
    }

    #[test]
    fn finds_the_clip_in_a_completed_interaction() {
        let v: Value = serde_json::from_str(
            r#"{"status":"completed","steps":[{"type":"thought"},{"type":"model_output","content":[{"uri":"https://generativelanguage.googleapis.com/v1beta/files/znkn2asr6exq:download?alt=media","mime_type":"video/mp4","type":"video"}]}]}"#,
        )
        .unwrap();
        match OmniVideoProvider::find_video(&v) {
            Some(Ok(VideoOut::Uri(u))) => {
                assert_eq!(
                    OmniVideoProvider::download_url("http://gw:8080/google", &u),
                    "http://gw:8080/google/v1beta/files/znkn2asr6exq:download?alt=media"
                );
                assert_eq!(OmniVideoProvider::download_url(GEMINI_BASE, &u), u);
            }
            _ => panic!("no uri"),
        }
    }

    #[test]
    fn veo_names_follow_to_omni() {
        let p = OmniVideoProvider;
        for raw in [
            "",
            "veo",
            "fast",
            "quality",
            "lite",
            "veo-3.1-fast-generate-preview",
            "omni",
        ] {
            assert_eq!(
                p.resolve_model(raw).as_deref(),
                Some("gemini-omni-1.1-flash"),
                "{raw}"
            );
        }
        assert!(p.resolve_model("ltx").is_none());
    }

    #[tokio::test]
    async fn poll_hands_over_the_parked_clip_once() {
        let path =
            std::env::temp_dir().join(format!("thclaws-omni-test-{}.mp4", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"mp4").unwrap();
        let job = ProviderJobRef {
            op: format!("{FILE_REF}{}", path.display()),
        };
        match OmniVideoProvider.poll(&job).await.unwrap() {
            JobState::Done { bytes } => assert_eq!(bytes, b"mp4"),
            _ => panic!("not done"),
        }
        assert!(matches!(
            OmniVideoProvider.poll(&job).await.unwrap(),
            JobState::Failed { .. }
        ));
    }
}
