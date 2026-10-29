//! LTX-2.3 video provider (`api.ltx.video`, `LTX_API_KEY` bearer).
//!
//! The async surface: `POST /v2/{text-to-video|image-to-video}` returns
//! `{id, created_at}` and `GET /v2/{endpoint}/{id}` returns
//! `{status: queued|processing|completed|failed, result: {video_url}}`.
//! (There is also a sync `/v1/...` that streams raw MP4 back — unusable
//! here, since the tool layer hands the caller a job id immediately.)
//!
//! LTX takes an explicit `WxH` resolution rather than an aspect tier, so
//! the engine's portable `aspect_ratio` + `resolution` pair is mapped onto
//! the sizes the API accepts (live-probed: 1280x720 / 1920x1080 / 3840x2160
//! and their portrait transposes; square is rejected).
//!
//! The hosted API takes exactly six fields — `prompt`, `model`, `duration`,
//! `resolution`, `fps`, `generate_audio` (plus `camera_motion` and
//! `image_uri`). Anything else is silently ignored, so there is no
//! `guidance_scale` / `negative_prompt` here; those exist only in the
//! open-source pipeline. `generate_audio` is the ONLY audio switch — voice,
//! language and accent come from the prompt (quote the dialogue, name the
//! language and accent), and lip/consonant detail from `fps`: 48/50 resolves
//! the P/B/M closures that 24/25 smears.
//!
//! `LTX_BASE_URL` repoints the native path at a self-hosted deployment
//! (the playbook's DGX Spark setup). Ignored in gateway mode, where the
//! base is the gateway's `/ltx` segment.

use super::super::provider::{
    resolve_endpoint, ImageModelInfo, JobState, ProviderJobRef, VideoProvider, VideoRequest,
};
use crate::error::{Error, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

const LTX_BASE: &str = "https://api.ltx.video";
/// LTX's general default. Only 24 / 25 / 48 / 50 are accepted; callers pick
/// 48 or 50 for dialogue.
const DEFAULT_FPS: u32 = 25;
/// Durations the API accepts (live-checked: 7s is rejected outright, and so
/// is every other odd length). A request is snapped to the nearest accepted
/// value rather than failing after the user already picked one.
const DURATIONS: &[u32] = &[4, 6, 8, 10, 12, 14, 16, 18, 20];

const MODELS: &[ImageModelInfo] = &[
    ImageModelInfo {
        id: "ltx-2-3-fast",
        aliases: &["ltx", "ltx-fast"],
        label: "LTX-2.3 Fast",
    },
    ImageModelInfo {
        id: "ltx-2-3-pro",
        aliases: &["ltx-pro"],
        label: "LTX-2.3 Pro",
    },
    // 2.5 is the current generation: multi-shot scenes that hold a voice
    // across cuts, and a stronger foundation for speech. Roughly 3× the
    // 2.3 rate, so it stays opt-in rather than becoming the default.
    ImageModelInfo {
        id: "ltx-2-5-fast",
        aliases: &["ltx-2-5", "ltx25"],
        label: "LTX-2.5 Fast",
    },
    ImageModelInfo {
        id: "ltx-2-5-pro",
        aliases: &["ltx-2-5-pro", "ltx25-pro"],
        label: "LTX-2.5 Pro",
    },
];

pub struct LtxVideoProvider;

impl LtxVideoProvider {
    /// The engine's `aspect_ratio` + `resolution` tier → the `WxH` string
    /// LTX wants. Portrait aspects transpose; anything else lands
    /// landscape. Square is not offered by either model, so `1:1` is
    /// treated as portrait rather than failing the request.
    fn resolution(req: &VideoRequest) -> String {
        let (long, short) = match req.resolution.to_ascii_uppercase().as_str() {
            "4K" | "2160P" => (3840, 2160),
            "1080P" => (1920, 1080),
            _ => (1280, 720),
        };
        match req.aspect_ratio.as_str() {
            "9:16" | "3:4" | "1:1" => format!("{short}x{long}"),
            _ => format!("{long}x{short}"),
        }
    }

    /// Snap to a duration the API takes. Asking for 7s otherwise fails the
    /// whole request; 5s silently isn't on the grid either.
    fn duration(seconds: u32) -> u32 {
        if DURATIONS.contains(&seconds) {
            return seconds;
        }
        DURATIONS
            .iter()
            .copied()
            .min_by_key(|d| d.abs_diff(seconds))
            .unwrap_or(6)
    }

    fn client(timeout_secs: u64) -> Result<Client> {
        Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| Error::Tool(format!("http client: {e}")))
    }

    /// Native base, honouring a self-host override; gateway mode keeps
    /// whatever the gateway resolved.
    fn endpoint() -> Result<crate::media::provider::ResolvedEndpoint> {
        let mut ep = resolve_endpoint(&["LTX_API_KEY"], LTX_BASE, "ltx")?;
        if !ep.via_gateway {
            if let Ok(base) = std::env::var("LTX_BASE_URL") {
                if !base.trim().is_empty() {
                    ep.base_url = base.trim().to_string();
                }
            }
        }
        Ok(ep)
    }

    /// The request body, exactly the fields the hosted API declares —
    /// anything else is silently dropped upstream, so sending more would
    /// only look like it worked.
    fn body(req: &VideoRequest) -> Value {
        let mut body = json!({
            "model": req.model,
            "prompt": req.prompt,
            "duration": Self::duration(req.duration_seconds),
            "resolution": Self::resolution(req),
            "fps": req.fps.unwrap_or(DEFAULT_FPS),
            "generate_audio": req.generate_audio,
        });
        if let Some(img) = &req.init_image {
            body["image_uri"] = json!(format!(
                "data:{};base64,{}",
                img.mime,
                B64.encode(&img.bytes)
            ));
        }
        body
    }

    /// `text-to-video` vs `image-to-video` — the path segment doubles as
    /// the poll path, so it's derived from the request, not stored.
    fn path(req: &VideoRequest) -> &'static str {
        if req.init_image.is_some() {
            "image-to-video"
        } else {
            "text-to-video"
        }
    }
}

#[async_trait]
impl VideoProvider for LtxVideoProvider {
    fn id(&self) -> &'static str {
        "ltx"
    }

    fn fixed_aspects(&self) -> bool {
        true
    }

    fn models(&self) -> &'static [ImageModelInfo] {
        MODELS
    }

    fn resolve_model(&self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        for m in MODELS {
            if raw == m.id || m.aliases.contains(&raw) {
                return Some(m.id.to_string());
            }
        }
        // Forward-compat: accept any future `ltx-*` id verbatim.
        if raw.starts_with("ltx-") {
            return Some(raw.to_string());
        }
        None
    }

    async fn submit(&self, req: &VideoRequest) -> Result<ProviderJobRef> {
        let ep = Self::endpoint()?;
        let path = Self::path(req);
        let body = Self::body(req);
        let url = format!("{}/v2/{}", ep.base_url.trim_end_matches('/'), path);
        let resp = crate::multi_tenant::attach_member(Self::client(120)?.post(&url))
            .bearer_auth(&ep.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("ltx video submit http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let b = resp.text().await.unwrap_or_default();
            return Err(Error::Tool(format!(
                "ltx video submit http {status}: {}",
                b.chars().take(400).collect::<String>()
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("ltx video submit not json: {e}")))?;
        let id = v
            .get("id")
            .and_then(|t| t.as_str())
            .ok_or_else(|| Error::Tool("ltx video submit missing id".into()))?;
        // The poll URL needs the same endpoint the job was created on, and
        // the id alone doesn't say which — carry both.
        Ok(ProviderJobRef {
            op: format!("{path}/{id}"),
        })
    }

    async fn poll(&self, job: &ProviderJobRef) -> Result<JobState> {
        let ep = Self::endpoint()?;
        // Older refs (and hand-written ones) may carry a bare id; those
        // are text-to-video by construction.
        let op = if job.op.contains('/') {
            job.op.clone()
        } else {
            format!("text-to-video/{}", job.op)
        };
        let url = format!("{}/v2/{}", ep.base_url.trim_end_matches('/'), op);
        let resp = crate::multi_tenant::attach_member(Self::client(30)?.get(&url))
            .bearer_auth(&ep.api_key)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("ltx video poll http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let b = resp.text().await.unwrap_or_default();
            return Err(Error::Tool(format!(
                "ltx video poll http {status}: {}",
                b.chars().take(400).collect::<String>()
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("ltx video poll not json: {e}")))?;
        match v
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
        {
            "queued" | "processing" | "pending" | "running" => Ok(JobState::Running { pct: None }),
            "completed" | "succeeded" => {
                let video_url = v
                    .pointer("/result/video_url")
                    .and_then(|u| u.as_str())
                    .ok_or_else(|| Error::Tool("ltx video done but no result.video_url".into()))?;
                let bytes = Self::client(300)?
                    .get(video_url)
                    .send()
                    .await
                    .map_err(|e| Error::Tool(format!("video download: {e}")))?
                    .bytes()
                    .await
                    .map(|b| b.to_vec())
                    .map_err(|e| Error::Tool(format!("video body: {e}")))?;
                Ok(JobState::Done { bytes })
            }
            other => {
                let msg = v
                    .pointer("/error/message")
                    .or_else(|| v.get("error"))
                    .and_then(|m| m.as_str())
                    .unwrap_or(other);
                Ok(JobState::Failed {
                    msg: format!("ltx video {other}: {msg}"),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::provider::InputImage;

    fn req(aspect: &str, resolution: &str) -> VideoRequest {
        VideoRequest {
            model: "ltx-2-3-fast".into(),
            prompt: "p".into(),
            init_image: None,
            aspect_ratio: aspect.into(),
            duration_seconds: 6,
            resolution: resolution.into(),
            fps: None,
            generate_audio: true,
        }
    }

    #[test]
    fn resolution_maps_tier_and_aspect_to_pixels() {
        assert_eq!(res(&req("16:9", "720P")), "1280x720");
        assert_eq!(res(&req("16:9", "1080P")), "1920x1080");
        assert_eq!(res(&req("4:3", "1080P")), "1920x1080");
        // Portrait transposes; square has no LTX size, so it goes portrait.
        assert_eq!(res(&req("9:16", "1080P")), "1080x1920");
        assert_eq!(res(&req("1:1", "720P")), "720x1280");
        assert_eq!(res(&req("16:9", "4K")), "3840x2160");
    }

    fn res(r: &VideoRequest) -> String {
        LtxVideoProvider::resolution(r)
    }

    #[test]
    fn duration_snaps_to_the_accepted_grid() {
        // 4/6/8… are accepted; 7 (and every odd length above 5) is refused
        // by the API, so it must never leave here.
        assert_eq!(LtxVideoProvider::duration(4), 4);
        assert_eq!(LtxVideoProvider::duration(6), 6);
        assert_eq!(LtxVideoProvider::duration(7), 6, "7s is rejected upstream");
        assert_eq!(LtxVideoProvider::duration(5), 4);
        assert_eq!(LtxVideoProvider::duration(9), 8);
        assert_eq!(LtxVideoProvider::duration(99), 20);
    }

    #[test]
    fn model_aliases_and_forward_compat() {
        let p = LtxVideoProvider;
        assert_eq!(p.resolve_model("ltx").as_deref(), Some("ltx-2-3-fast"));
        assert_eq!(p.resolve_model("ltx-pro").as_deref(), Some("ltx-2-3-pro"));
        assert_eq!(p.resolve_model("ltx-2-5").as_deref(), Some("ltx-2-5-fast"));
        assert_eq!(
            p.resolve_model("ltx-2-5-pro").as_deref(),
            Some("ltx-2-5-pro")
        );
        assert_eq!(
            p.resolve_model("ltx-2-4-fast").as_deref(),
            Some("ltx-2-4-fast"),
            "future ids pass through"
        );
        assert!(p.resolve_model("veo").is_none());
        // No `""` alias: a bare `provider: "ltx"` picks models().first().
        assert!(p.resolve_model("").is_none());
    }

    /// The wire body is the whole contract with LTX: only the six declared
    /// fields, the snapped duration, and no phantom knobs (a stray
    /// `guidance_scale` looked like it was tuning the render for weeks —
    /// the hosted API had been dropping it all along).
    #[test]
    fn body_carries_only_the_fields_the_api_declares() {
        let mut r = req("16:9", "1080P");
        r.duration_seconds = 7; // not on LTX's grid
        r.fps = Some(48);
        let b = LtxVideoProvider::body(&r);
        assert_eq!(b["duration"], 6, "7s would be rejected upstream");
        assert_eq!(b["fps"], 48);
        assert_eq!(b["generate_audio"], true);
        assert_eq!(b["resolution"], "1920x1080");
        assert!(b.get("guidance_scale").is_none(), "phantom field is back");
        assert!(b.get("image_uri").is_none(), "t2v carries no image");
        let keys: Vec<&str> = b.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(keys.len(), 6, "unexpected field(s): {keys:?}");
    }

    #[test]
    fn fps_defaults_and_audio_can_be_switched_off() {
        let mut r = req("16:9", "720P");
        assert_eq!(LtxVideoProvider::body(&r)["fps"], 25);
        r.generate_audio = false;
        assert_eq!(LtxVideoProvider::body(&r)["generate_audio"], false);
    }

    #[test]
    fn path_follows_the_init_image() {
        let mut r = req("16:9", "720P");
        assert_eq!(LtxVideoProvider::path(&r), "text-to-video");
        r.init_image = Some(InputImage {
            bytes: vec![1],
            mime: "image/png".into(),
        });
        assert_eq!(LtxVideoProvider::path(&r), "image-to-video");
    }
}
