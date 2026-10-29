//! DashScope video provider (dev-plan/40) — Alibaba Model Studio
//! async video synthesis.
//!
//! happyhorse-1.0-t2v is a text→video model on DashScope's async
//! `video-generation/video-synthesis` endpoint: submit with
//! `X-DashScope-Async: enable` → `output.task_id`, then poll
//! `/api/v1/tasks/<id>` until `task_status: SUCCEEDED` and download
//! `output.video_url`. International endpoint `dashscope-intl.aliyuncs.com`
//! (verified to host the model); auth `Authorization: Bearer
//! DASHSCOPE_API_KEY`.
//!
//! Pricing is PER SECOND of output ($0.14/s at 720P, $0.24/s at 1080P;
//! recorded as `price_per_video_second_usd` in the catalogue). Desktop
//! users with a native DASHSCOPE_API_KEY work today; gateway per-second
//! metering is a follow-up.

use crate::error::{Error, Result};
use crate::media::provider::{
    ImageModelInfo, JobState, ProviderJobRef, VideoProvider, VideoRequest,
};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

const DASHSCOPE_BASE: &str = "https://dashscope-intl.aliyuncs.com";
const SUBMIT_PATH: &str = "/api/v1/services/aigc/video-generation/video-synthesis";

const MODELS: &[ImageModelInfo] = &[
    ImageModelInfo {
        id: "happyhorse-1.0-t2v",
        aliases: &["happyhorse", "happyhorse-1.0-t2v"],
        label: "HappyHorse 1.0 (text→video)",
    },
    ImageModelInfo {
        id: "happyhorse-1.0-i2v",
        aliases: &["happyhorse-i2v", "happyhorse-1.0-i2v"],
        label: "HappyHorse 1.0 (image→video)",
    },
    ImageModelInfo {
        id: "wan2.7-t2v",
        aliases: &["wan", "wan-t2v", "wan2.7", "wan2.7-t2v"],
        label: "Wan 2.7 (text→video)",
    },
    ImageModelInfo {
        id: "wan2.7-i2v",
        aliases: &["wan-i2v", "wan2.7-i2v"],
        label: "Wan 2.7 (image→video)",
    },
    ImageModelInfo {
        id: "wan2.6-t2v",
        aliases: &["wan2.6", "wan2.6-t2v"],
        label: "Wan 2.6 (text→video)",
    },
    ImageModelInfo {
        id: "wan2.6-i2v",
        aliases: &["wan2.6-i2v"],
        label: "Wan 2.6 (image→video)",
    },
    ImageModelInfo {
        id: "wan2.5-t2v-preview",
        aliases: &["wan2.5", "wan2.5-t2v-preview"],
        label: "Wan 2.5 preview (text→video, 5 or 10s)",
    },
    ImageModelInfo {
        id: "wan2.2-t2v-plus",
        aliases: &["wan2.2", "wan2.2-t2v-plus"],
        label: "Wan 2.2 Plus (text→video, 5s)",
    },
    ImageModelInfo {
        id: "wan2.1-t2v-turbo",
        aliases: &["wan2.1", "wan2.1-t2v-turbo"],
        label: "Wan 2.1 Turbo (text→video, 5s)",
    },
];

/// Wan before 2.7 speaks the older protocol (Model Studio text-/image-to-
/// video API reference): text→video sizes by `size: "W*H"`, image→video
/// takes `input.img_url` + `resolution`. HappyHorse and Wan 2.7 share the
/// newer one (`resolution` + `ratio`, frame in `input.media[]`).
fn legacy_wan(model: &str) -> bool {
    model.starts_with("wan") && !model.starts_with("wan2.7")
}

/// Legacy Wan duration rules: 2.5 takes 5 or 10, 2.2/2.1 are fixed at 5,
/// 2.6 takes 2–15.
fn legacy_duration(model: &str, secs: u32) -> u32 {
    if model.starts_with("wan2.5") {
        if secs >= 8 {
            10
        } else {
            5
        }
    } else if model.starts_with("wan2.2") || model.starts_with("wan2.1") {
        5
    } else {
        secs.clamp(2, 15)
    }
}

/// `W*H` for a legacy text→video model: the requested tier when the model
/// has it (2.2 Plus has no 720P, 2.1 Turbo no 1080P), in the caller's aspect.
fn legacy_size(model: &str, resolution: &str, aspect: &str) -> &'static str {
    let hd = match resolution {
        "1080P" => !model.starts_with("wan2.1"),
        _ => model.starts_with("wan2.2"),
    };
    match (hd, aspect) {
        (true, "9:16") => "1080*1920",
        (true, "1:1") => "1440*1440",
        (true, "4:3") => "1632*1248",
        (true, "3:4") => "1248*1632",
        (true, _) => "1920*1080",
        (false, "9:16") => "720*1280",
        (false, "1:1") => "960*960",
        (false, "4:3") => "1088*832",
        (false, "3:4") => "832*1088",
        (false, _) => "1280*720",
    }
}

pub struct DashScopeVideoProvider;

impl DashScopeVideoProvider {
    fn resolution(req: &VideoRequest) -> &str {
        match req.resolution.as_str() {
            "1080P" | "1080p" => "1080P",
            _ => "720P",
        }
    }
    fn body(req: &VideoRequest) -> Result<Value> {
        let image = req
            .init_image
            .as_ref()
            .map(|img| format!("data:{};base64,{}", img.mime, B64.encode(&img.bytes)));
        if legacy_wan(&req.model) {
            let i2v = req.model.contains("-i2v");
            if i2v != image.is_some() {
                return Err(Error::Tool(if i2v {
                    format!(
                        "{} animates an image — pass one, or use a -t2v model",
                        req.model
                    )
                } else {
                    format!(
                        "{} is text→video — use wan2.6-i2v or wan2.7-i2v for an image",
                        req.model
                    )
                }));
            }
            let duration = legacy_duration(&req.model, req.duration_seconds);
            return Ok(match image {
                Some(url) => json!({
                    "model": req.model,
                    "input": { "prompt": req.prompt, "img_url": url },
                    "parameters": {
                        "resolution": Self::resolution(req),
                        "duration": duration,
                        "watermark": false,
                    },
                }),
                None => json!({
                    "model": req.model,
                    "input": { "prompt": req.prompt },
                    "parameters": {
                        "size": legacy_size(&req.model, Self::resolution(req), &req.aspect_ratio),
                        "duration": duration,
                        "watermark": false,
                    },
                }),
            });
        }
        // text→video is prompt-only + a `ratio` parameter; image→video
        // carries the source frame in `input.media[].first_frame` (a base64
        // data URI for a local image — verified accepted) and derives
        // aspect from the frame, so `ratio` is omitted.
        let mut input = json!({ "prompt": req.prompt });
        // happyhorse stamps "Happy Horse" in the corner unless told not to
        // (seen 2026-10-22); Model Studio's video API takes the same switch
        // for every model.
        let mut parameters = json!({
            "resolution": Self::resolution(req),
            "duration": req.duration_seconds,
            "watermark": false,
        });
        if let Some(url) = image {
            input["media"] = json!([{ "type": "first_frame", "url": url }]);
        } else {
            parameters["ratio"] = json!(req.aspect_ratio);
        }
        Ok(json!({
            "model": req.model,
            "input": input,
            "parameters": parameters,
        }))
    }

    fn client(timeout_secs: u64) -> Result<Client> {
        Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| Error::Tool(format!("http client: {e}")))
    }
}

#[async_trait]
impl VideoProvider for DashScopeVideoProvider {
    fn id(&self) -> &'static str {
        "dashscope"
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
        // Forward-compat: accept any future `happyhorse-*` / Wan video id.
        if raw.starts_with("happyhorse")
            || (raw.starts_with("wan") && (raw.contains("-t2v") || raw.contains("-i2v")))
        {
            return Some(raw.to_string());
        }
        None
    }

    async fn submit(&self, req: &VideoRequest) -> Result<ProviderJobRef> {
        let body = Self::body(req)?;
        let ep = crate::media::provider::resolve_dashscope_endpoint(DASHSCOPE_BASE)?;
        let url = format!("{}{}", ep.base_url.trim_end_matches('/'), SUBMIT_PATH);
        let client = Self::client(60)?;
        let resp = crate::multi_tenant::attach_member(client.post(&url))
            .bearer_auth(&ep.api_key)
            .header("X-DashScope-Async", "enable")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("dashscope video submit http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let b = resp.text().await.unwrap_or_default();
            return Err(Error::Tool(format!(
                "dashscope video submit http {status}: {}",
                b.chars().take(400).collect::<String>()
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("dashscope video submit not json: {e}")))?;
        let task_id = v
            .pointer("/output/task_id")
            .and_then(|t| t.as_str())
            .ok_or_else(|| Error::Tool("dashscope video submit missing output.task_id".into()))?;
        Ok(ProviderJobRef {
            op: task_id.to_string(),
        })
    }

    async fn poll(&self, job: &ProviderJobRef) -> Result<JobState> {
        let ep = crate::media::provider::resolve_dashscope_endpoint(DASHSCOPE_BASE)?;
        let url = format!(
            "{}/api/v1/tasks/{}",
            ep.base_url.trim_end_matches('/'),
            job.op
        );
        let client = Self::client(30)?;
        let resp = crate::multi_tenant::attach_member(client.get(&url))
            .bearer_auth(&ep.api_key)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("dashscope video poll http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let b = resp.text().await.unwrap_or_default();
            return Err(Error::Tool(format!(
                "dashscope video poll http {status}: {}",
                b.chars().take(400).collect::<String>()
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("dashscope video poll not json: {e}")))?;
        let status = v
            .pointer("/output/task_status")
            .and_then(|s| s.as_str())
            .unwrap_or("UNKNOWN");
        match status {
            "PENDING" | "RUNNING" => Ok(JobState::Running { pct: None }),
            "SUCCEEDED" => {
                let video_url = v
                    .pointer("/output/video_url")
                    .and_then(|u| u.as_str())
                    .or_else(|| v.pointer("/output/results/0/url").and_then(|u| u.as_str()))
                    .ok_or_else(|| {
                        Error::Tool("dashscope video done but no output.video_url".into())
                    })?;
                let dl = Self::client(180)?;
                let bytes = dl
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
                    .pointer("/output/message")
                    .and_then(|m| m.as_str())
                    .unwrap_or(other);
                Ok(JobState::Failed {
                    msg: format!("dashscope video {other}: {msg}"),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(model: &str, image: bool, secs: u32, res: &str, aspect: &str) -> VideoRequest {
        VideoRequest {
            model: model.into(),
            prompt: "p".into(),
            init_image: image.then(|| crate::media::provider::InputImage {
                bytes: vec![1],
                mime: "image/png".into(),
            }),
            aspect_ratio: aspect.into(),
            duration_seconds: secs,
            resolution: res.into(),
            fps: None,
            generate_audio: true,
        }
    }

    fn body(model: &str, image: bool, secs: u32, res: &str, aspect: &str) -> Value {
        DashScopeVideoProvider::body(&req(model, image, secs, res, aspect)).unwrap()
    }

    #[test]
    fn happyhorse_and_wan27_keep_the_new_protocol() {
        let b = body("happyhorse-1.0-t2v", false, 8, "720P", "16:9");
        assert_eq!(b["parameters"]["resolution"], "720P");
        assert_eq!(b["parameters"]["ratio"], "16:9");
        assert_eq!(b["parameters"]["duration"], 8);
        let b = body("wan2.7-i2v", true, 6, "1080P", "16:9");
        assert_eq!(b["input"]["media"][0]["type"], "first_frame");
        assert!(b["parameters"].get("ratio").is_none());
        assert!(b["input"].get("img_url").is_none());
    }

    #[test]
    fn legacy_wan_t2v_sizes_by_w_h() {
        let b = body("wan2.6-t2v", false, 6, "720P", "9:16");
        assert_eq!(b["parameters"]["size"], "720*1280");
        assert_eq!(b["parameters"]["duration"], 6);
        assert!(b["parameters"].get("resolution").is_none());
        // 2.2 Plus has no 720P tier; 2.1 Turbo no 1080P.
        assert_eq!(
            body("wan2.2-t2v-plus", false, 8, "720P", "16:9")["parameters"]["size"],
            "1920*1080"
        );
        assert_eq!(
            body("wan2.1-t2v-turbo", false, 8, "1080P", "1:1")["parameters"]["size"],
            "960*960"
        );
    }

    #[test]
    fn legacy_wan_durations_follow_each_model() {
        let d =
            |m: &str, s: u32| body(m, false, s, "720P", "16:9")["parameters"]["duration"].clone();
        assert_eq!(d("wan2.5-t2v-preview", 4), 5);
        assert_eq!(d("wan2.5-t2v-preview", 8), 10);
        assert_eq!(d("wan2.2-t2v-plus", 8), 5);
        assert_eq!(d("wan2.1-t2v-turbo", 4), 5);
    }

    #[test]
    fn legacy_wan_i2v_takes_img_url() {
        let b = body("wan2.6-i2v", true, 5, "720P", "16:9");
        assert!(b["input"]["img_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
        assert_eq!(b["parameters"]["resolution"], "720P");
        assert!(
            DashScopeVideoProvider::body(&req("wan2.6-i2v", false, 5, "720P", "16:9")).is_err()
        );
        assert!(DashScopeVideoProvider::body(&req("wan2.6-t2v", true, 5, "720P", "16:9")).is_err());
    }

    #[test]
    fn wan_models_resolve() {
        let p = DashScopeVideoProvider;
        assert_eq!(p.resolve_model("wan").as_deref(), Some("wan2.7-t2v"));
        assert_eq!(p.resolve_model("wan-i2v").as_deref(), Some("wan2.7-i2v"));
        assert_eq!(p.resolve_model("wan2.8-t2v").as_deref(), Some("wan2.8-t2v"));
        assert!(p.resolve_model("wan2.7-image").is_none());
    }

    #[test]
    fn every_video_request_turns_the_watermark_off() {
        for (model, image) in [
            ("happyhorse-1.0-t2v", false),
            ("happyhorse-1.0-i2v", true),
            ("wan2.7-t2v", false),
            ("wan2.2-t2v-plus", false),
            ("wan2.2-i2v-plus", true),
        ] {
            assert_eq!(
                body(model, image, 5, "720P", "16:9")["parameters"]["watermark"],
                false,
                "{model}"
            );
        }
    }
}
