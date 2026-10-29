//! WatchVideo — let a vision LLM *watch* a video. Extracts scene-aware,
//! deduplicated key frames (one ffmpeg pass: every scene change + a density
//! floor) and returns them as inline image blocks so the model sees the
//! pixels, plus an optional transcript (Groq Whisper; Qwen3-ASR through the
//! gateway on a locked install such as SIS). The dedup (downscaled
//! RGB diff against a sliding window of recent kept frames) drops near-
//! duplicates and A-B-A cutaways, so a static screencast collapses to one
//! frame and a fast-cut reel keeps each change — far fewer, more meaningful
//! frames than fixed-interval sampling.
//!
//! Local files only (the sandbox gates the path). Needs `ffmpeg`/`ffprobe`
//! on PATH; the transcript needs `GROQ_API_KEY` or the gateway.

use super::read::downscale_for_vision;
use super::{req_str, Tool};
use crate::error::{Error, Result};
use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DEFAULT_MAX_FRAMES: usize = 32;
const DEDUP_WINDOW: usize = 4;

pub struct WatchVideoTool;

fn run(cmd: &mut std::process::Command) -> std::io::Result<std::process::Output> {
    cmd.output()
}

fn ffprobe_f(video: &Path, entries: &str) -> Option<String> {
    let out = run(std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            entries,
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(video))
    .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn duration_secs(video: &Path) -> f64 {
    run(std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(video))
    .ok()
    .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
    .unwrap_or(0.0)
}

fn fps(video: &Path) -> f64 {
    ffprobe_f(video, "stream=avg_frame_rate")
        .and_then(|s| {
            let (n, d) = s.split_once('/')?;
            let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
            (d != 0.0).then_some(n / d)
        })
        .filter(|f| f.is_finite() && *f > 0.0)
        .unwrap_or(25.0)
}

/// 16×16 RGB signature for cheap pixel-diff dedup.
fn signature(path: &Path) -> Option<Vec<[u8; 3]>> {
    let img = image::open(path)
        .ok()?
        .resize_exact(16, 16, image::imageops::FilterType::Triangle);
    Some(img.to_rgb8().pixels().map(|p| p.0).collect())
}

/// % of pixels whose max channel delta exceeds `tol` — the same measure as
/// pixelmatch, robust to flat colours where a perceptual hash goes blind.
fn pct_diff(a: &[[u8; 3]], b: &[[u8; 3]]) -> f64 {
    let tol = 25i16;
    let changed = a
        .iter()
        .zip(b)
        .filter(|(x, y)| {
            (x[0] as i16 - y[0] as i16)
                .abs()
                .max((x[1] as i16 - y[1] as i16).abs())
                .max((x[2] as i16 - y[2] as i16).abs())
                > tol
        })
        .count();
    100.0 * changed as f64 / a.len().max(1) as f64
}

fn tmp_dir() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "thclaws-watch-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::create_dir_all(&d);
    d
}

/// What became of the audio track. Kept distinct so a failed or impossible
/// transcription is never reported as a silent video.
enum Transcript {
    Text { text: String, model: &'static str },
    NoAudio,
    Unavailable(String),
    Failed(String),
}

impl Transcript {
    fn note(&self) -> String {
        match self {
            Transcript::Text { text, model } if text.trim().is_empty() => {
                format!("\n\n(no transcript — {model} heard no speech in the audio)")
            }
            Transcript::Text { text, model } => format!("\n\n--- transcript ({model}) ---\n{text}"),
            Transcript::NoAudio => "\n\n(no transcript — the video has no audio track)".into(),
            Transcript::Unavailable(why) => {
                format!("\n\n(no transcript — the video has audio, but {why})")
            }
            Transcript::Failed(why) => format!(
                "\n\n(no transcript — the video has audio, but transcription failed: {why})"
            ),
        }
    }
}

const ASR_MODEL: &str = "qwen3-asr-flash";
/// qwen3-asr-flash takes ≤5 min / ≤10 MB per request; 2-minute 16 kHz mono
/// chunks are ~3.8 MB (~5 MB as base64).
const ASR_CHUNK_SECS: u32 = 120;

fn has_audio(video: &Path) -> bool {
    run(std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(video))
    .ok()
    .map(|o| !o.stdout.is_empty())
    .unwrap_or(false)
}

/// A moving picture to look at. An mp3/m4a's cover art is a video stream
/// too, flagged `attached_pic`; it is one still, not something to watch.
fn has_picture(video: &Path) -> bool {
    let Some(s) = ffprobe_f(video, "stream=codec_type:stream_disposition=attached_pic") else {
        return false;
    };
    let lines: Vec<&str> = s.lines().map(str::trim).collect();
    lines.contains(&"video") && !lines.contains(&"1")
}

async fn transcribe(video: &Path, dir: &Path, lang: Option<&str>) -> Transcript {
    if !has_audio(video) {
        return Transcript::NoAudio;
    }
    // On an AI Server box the recording stays on the box: its own
    // asr-studio transcribes, and Groq or the cloud gateway — which would
    // upload it — are refused unless explicitly allowed (dev-plan/71).
    if let Some(d) = crate::aiserver::cached() {
        if let Some(asr) = d.asr.as_ref().filter(|a| a.live()) {
            return match aiserver_transcript(video, dir, lang, asr).await {
                Ok(t) => Transcript::Text {
                    text: t,
                    model: "AI Server ASR",
                },
                Err(e) => Transcript::Failed(e),
            };
        }
        if !std::env::var("THCLAWS_ALLOW_CLOUD_ASR").is_ok_and(|v| v.trim() == "1")
            && !crate::providers::thclaws_gateway::cloud_allowed_on_aiserver()
        {
            return Transcript::Unavailable(
                "this AI Server box has no speech recognition running (start ASR Studio in the \
                 hub), and audio is not sent to the cloud from here (THCLAWS_ALLOW_CLOUD_ASR=1 \
                 allows it)"
                    .into(),
            );
        }
    }
    // A locked install transcribes through its own DashScope segment (SIS:
    // Qwen3-ASR); elsewhere Groq Whisper, BYOK or via the gateway.
    if crate::shared::gateway_providers_locked() {
        let Some(seg) = crate::media::provider::dashscope_media_segment() else {
            return Transcript::Unavailable(
                "this deployment has no speech-recognition model".into(),
            );
        };
        return match dashscope_transcript(video, dir, lang, seg).await {
            Ok(t) => Transcript::Text {
                text: t,
                model: ASR_MODEL,
            },
            Err(e) => Transcript::Failed(e),
        };
    }
    match groq_transcript(video, dir, lang).await {
        Ok(Some(t)) => Transcript::Text {
            text: t,
            model: "whisper-large-v3",
        },
        Ok(None) => Transcript::Unavailable(
            "no transcription service is set up (set GROQ_API_KEY or enable the thClaws Gateway)"
                .into(),
        ),
        Err(e) => Transcript::Failed(e),
    }
}

/// asr-studio `POST /v1/transcribe` (AISERVER-API-GUIDE §14). Loopback
/// only, so its self-signed certificate is accepted for that host alone.
async fn aiserver_transcript(
    video: &Path,
    dir: &Path,
    lang: Option<&str>,
    asr: &crate::aiserver::Service,
) -> std::result::Result<String, String> {
    let wav = dir.join("audio.wav");
    extract_wav(video, &wav)?;
    let bytes = std::fs::read(&wav).map_err(|e| e.to_string())?;
    let loopback = asr.base_url.contains("://127.0.0.1") || asr.base_url.contains("://localhost");
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(asr.tls_selfsigned && loopback)
        .build()
        .map_err(|e| e.to_string())?;
    let mut form = reqwest::multipart::Form::new()
        .part(
            "audio",
            reqwest::multipart::Part::bytes(bytes)
                .file_name("audio.wav")
                .mime_str("audio/wav")
                .map_err(|e| e.to_string())?,
        )
        .text("diarize", "false");
    if let Some(l) = lang.filter(|l| !l.is_empty() && *l != "auto") {
        form = form.text("language", l.to_string());
    }
    let resp = client
        .post(format!(
            "{}/v1/transcribe",
            asr.base_url.trim_end_matches('/')
        ))
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("asr-studio: {e}"))?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "asr-studio HTTP {code}: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("asr-studio: {e}"))?;
    Ok(v.get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string())
}

fn extract_wav(video: &Path, wav: &Path) -> std::result::Result<(), String> {
    let out = run(std::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(video)
        .args(["-vn", "-ar", "16000", "-ac", "1"])
        .arg(wav)
        .args(["-hide_banner", "-loglevel", "error"]))
    .map_err(|e| format!("ffmpeg: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err("ffmpeg could not extract the audio track".into())
    }
}

async fn dashscope_transcript(
    video: &Path,
    dir: &Path,
    lang: Option<&str>,
    segment: &str,
) -> std::result::Result<String, String> {
    let ep = crate::media::provider::resolve_endpoint(
        &["DASHSCOPE_API_KEY"],
        "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        segment,
    )
    .map_err(|e| e.to_string())?;
    let chunk_dir = dir.join("asr");
    std::fs::create_dir_all(&chunk_dir).map_err(|e| e.to_string())?;
    let out = run(std::process::Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(video)
        .args([
            "-vn",
            "-ar",
            "16000",
            "-ac",
            "1",
            "-f",
            "segment",
            "-segment_time",
        ])
        .arg(ASR_CHUNK_SECS.to_string())
        .arg(chunk_dir.join("part_%03d.wav"))
        .args(["-hide_banner", "-loglevel", "error"]))
    .map_err(|e| format!("ffmpeg: {e}"))?;
    if !out.status.success() {
        return Err("ffmpeg could not extract the audio track".into());
    }
    let mut parts: Vec<PathBuf> = std::fs::read_dir(&chunk_dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "wav"))
        .collect();
    parts.sort();
    let client = reqwest::Client::new();
    let url = format!("{}/chat/completions", ep.base_url.trim_end_matches('/'));
    let mut texts = Vec::new();
    for p in parts {
        let bytes = std::fs::read(&p).map_err(|e| e.to_string())?;
        let mut body = json!({
            "model": ASR_MODEL,
            "messages": [{ "role": "user", "content": [{
                "type": "input_audio",
                "input_audio": { "data": format!(
                    "data:audio/wav;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(&bytes)
                ) }
            }]}],
            "stream": false,
        });
        if let Some(l) = lang.filter(|l| !l.is_empty() && *l != "auto") {
            body["asr_options"] = json!({ "language": l });
        }
        let resp = crate::multi_tenant::attach_member(client.post(&url))
            .bearer_auth(&ep.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("{ASR_MODEL}: {e}"))?;
        let status = resp.status();
        let v: Value = resp.json().await.map_err(|e| format!("{ASR_MODEL}: {e}"))?;
        if !status.is_success() {
            let msg = v
                .pointer("/error/message")
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("");
            return Err(format!("{ASR_MODEL} HTTP {}: {msg}", status.as_u16()));
        }
        if let Some(t) = v
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
        {
            if !t.trim().is_empty() {
                texts.push(t.trim().to_string());
            }
        }
    }
    Ok(texts.join("\n"))
}

/// `Ok(None)`: no Groq key and no gateway — transcription isn't set up.
async fn groq_transcript(
    video: &Path,
    dir: &Path,
    lang: Option<&str>,
) -> std::result::Result<Option<String>, String> {
    // BYOK-or-gateway (dev-plan/53 Stage D): a real GROQ_API_KEY posts
    // to Groq directly; a gateway key routes via `<gw>/groq/audio/…`
    // (per-second metered).
    let Ok(ep) = crate::media::provider::resolve_endpoint(
        &["GROQ_API_KEY"],
        "https://api.groq.com/openai/v1",
        "groq",
    ) else {
        return Ok(None);
    };
    let wav = dir.join("audio.wav");
    extract_wav(video, &wav)?;
    let bytes = std::fs::read(&wav).map_err(|e| e.to_string())?;
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;
    let mut form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", "whisper-large-v3")
        .text("response_format", "text");
    if let Some(l) = lang.filter(|l| *l != "auto") {
        form = form.text("language", l.to_string());
    }
    let resp = crate::multi_tenant::attach_member(
        reqwest::Client::new().post(format!("{}/audio/transcriptions", ep.base_url)),
    )
    .bearer_auth(&ep.api_key)
    .multipart(form)
    .send()
    .await
    .map_err(|e| format!("whisper: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("whisper HTTP {}", resp.status().as_u16()));
    }
    Ok(Some(
        resp.text()
            .await
            .map(|t| t.trim().to_string())
            .unwrap_or_default(),
    ))
}

#[async_trait]
impl Tool for WatchVideoTool {
    fn name(&self) -> &'static str {
        "WatchVideo"
    }

    fn description(&self) -> &'static str {
        "Watch a local video file: extracts scene-aware, deduplicated key frames \
         and returns them as inline images so you can SEE the video (not just its \
         transcript), plus an audio transcript when a speech-recognition route is \
         available (Whisper via GROQ_API_KEY/gateway, or Qwen3-ASR). Use it \
         to review/critique a video, check a generated clip, or answer questions \
         about what happens on screen. Audio-only files (mp3, m4a, wav, a phone \
         recording of a meeting) are accepted too and return the transcript alone. Args: path (required), scene (0-1 \
         sensitivity, lower=more frames, default 0.3), fps_floor (>=1 frame every \
         N sec, default 1.0), max_frames (default 32), dedup (% pixels changed to \
         count as new, default 8), lang (Whisper language, default auto)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to a local video or audio file"},
                "scene": {"type": "number", "description": "Scene-change sensitivity 0-1 (default 0.3)"},
                "fps_floor": {"type": "number", "description": "At least one frame every N seconds (default 1.0)"},
                "max_frames": {"type": "integer", "description": "Cap on frames returned (default 32)"},
                "dedup": {"type": "number", "description": "% of pixels that must change for a new frame (default 8)"},
                "lang": {"type": "string", "description": "Whisper language e.g. th/en/auto (default auto)"}
            },
            "required": ["path"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        // Frame extraction is local + free, but the Whisper transcript is
        // a paid, gateway-metered call — gate it like other spend tools.
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        // Text-only fallback: real output is images, via call_multimodal.
        let _ = req_str(&input, "path")?;
        Ok(
            "WatchVideo returns image frames — invoke it via the agent loop (call_multimodal)."
                .into(),
        )
    }

    async fn call_multimodal(&self, input: Value) -> Result<ToolResultContent> {
        let raw = req_str(&input, "path")?;
        let video = crate::sandbox::Sandbox::check(raw)?;
        if crate::filmscript::harness::check_av_tools().is_err() {
            return Err(Error::Tool("ffmpeg/ffprobe not found on PATH".into()));
        }
        let scene = input.get("scene").and_then(Value::as_f64).unwrap_or(0.30);
        let fps_floor = input
            .get("fps_floor")
            .and_then(Value::as_f64)
            .unwrap_or(1.0)
            .max(0.1);
        let max_frames = input
            .get("max_frames")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_FRAMES as u64) as usize;
        let dedup = input.get("dedup").and_then(Value::as_f64).unwrap_or(8.0);
        let lang = input.get("lang").and_then(Value::as_str);

        let dir = tmp_dir();
        let dur = duration_secs(&video);
        let name = video
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("video");

        // A phone recording of a meeting is audio only: transcribe it
        // rather than fail on frame extraction.
        if !has_picture(&video) {
            let transcript = transcribe(&video, &dir, lang).await;
            let _ = std::fs::remove_dir_all(&dir);
            if matches!(transcript, Transcript::NoAudio) {
                return Err(Error::Tool(format!(
                    "{name} has neither a video nor an audio stream"
                )));
            }
            return Ok(ToolResultContent::Blocks(vec![ToolResultBlock::Text {
                text: format!(
                    "Listened to {name} — {dur:.0}s, audio only (no picture to watch).{}",
                    transcript.note()
                ),
            }]));
        }
        let every_n = (fps(&video) * fps_floor).round().max(1.0) as u64;

        // One chronological pass: scene changes OR a density floor.
        let status = run(std::process::Command::new("ffmpeg")
            .args(["-i"])
            .arg(&video)
            .args([
                "-vf",
                &format!("select='gt(scene,{scene})+not(mod(n,{every_n}))',scale=640:-2"),
                "-vsync",
                "vfr",
            ])
            .arg(dir.join("raw_%05d.jpg"))
            .args(["-hide_banner", "-loglevel", "error"]))
        .map_err(|e| Error::Tool(format!("ffmpeg: {e}")))?;
        if !status.status.success() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(Error::Tool(format!(
                "ffmpeg frame extraction failed: {}",
                String::from_utf8_lossy(&status.stderr)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )));
        }

        let mut raw: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|e| Error::Tool(format!("read frames: {e}")))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|x| x == "jpg").unwrap_or(false))
            .collect();
        raw.sort();
        let extracted = raw.len();

        // Dedup against a sliding window of recent kept signatures.
        let mut kept: Vec<PathBuf> = Vec::new();
        let mut recent: Vec<Vec<[u8; 3]>> = Vec::new();
        for p in raw {
            let Some(sig) = signature(&p) else { continue };
            let dup = recent.iter().any(|r| pct_diff(&sig, r) <= dedup);
            if !dup {
                recent.push(sig);
                if recent.len() > DEDUP_WINDOW {
                    recent.remove(0);
                }
                kept.push(p);
            }
        }
        // Cap: thin uniformly so survivors stay spread across the video.
        if kept.len() > max_frames && max_frames > 0 {
            let step = kept.len() as f64 / max_frames as f64;
            let keep: std::collections::BTreeSet<usize> = (0..max_frames)
                .map(|i| (i as f64 * step) as usize)
                .collect();
            kept = kept
                .into_iter()
                .enumerate()
                .filter(|(i, _)| keep.contains(i))
                .map(|(_, p)| p)
                .collect();
        }

        let transcript = transcribe(&video, &dir, lang).await;

        // Build the result: a summary + each kept frame as an image block.
        let mut blocks: Vec<ToolResultBlock> = Vec::new();
        let mut summary = format!(
            "Watched {name} — {:.0}s, {} key frames (scene-aware, deduped from {} extracted). \
             Frames are in chronological order below.",
            dur,
            kept.len(),
            extracted
        );
        summary.push_str(&transcript.note());
        blocks.push(ToolResultBlock::Text { text: summary });

        for p in &kept {
            let Ok(bytes) = std::fs::read(p) else {
                continue;
            };
            let (out, mime): (Vec<u8>, &str) = match downscale_for_vision(&bytes, "image/jpeg") {
                Ok(Some((b, m))) => (b, m),
                _ => (bytes, "image/jpeg"),
            };
            blocks.push(ToolResultBlock::Image {
                source: ImageSource::Base64 {
                    media_type: mime.to_string(),
                    data: base64::engine::general_purpose::STANDARD.encode(&out),
                },
            });
        }

        let _ = std::fs::remove_dir_all(&dir);
        if kept.is_empty() {
            return Err(Error::Tool(
                "no frames extracted — is this a valid video?".into(),
            ));
        }
        Ok(ToolResultContent::Blocks(blocks))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_failed_transcription_is_never_reported_as_silence() {
        let no_audio = Transcript::NoAudio.note();
        assert!(no_audio.contains("no audio track"));
        for t in [
            Transcript::Failed("gateway 403".into()),
            Transcript::Unavailable("no route".into()),
            Transcript::Text {
                text: " ".into(),
                model: ASR_MODEL,
            },
        ] {
            let n = t.note();
            assert!(!n.contains("no audio track"), "{n}");
        }
        assert!(Transcript::Failed("x".into()).note().contains("has audio"));
        assert!(Transcript::Text {
            text: "สวัสดี".into(),
            model: ASR_MODEL
        }
        .note()
        .contains("สวัสดี"));
    }

    use super::*;

    #[test]
    fn audio_only_and_cover_art_are_not_pictures() {
        if crate::filmscript::harness::check_av_tools().is_err() {
            return;
        }
        let dir = tmp_dir();
        let ff = |args: &[&str], out: &str| {
            let ok = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-y"])
                .args(args)
                .arg(dir.join(out))
                .status()
                .unwrap()
                .success();
            assert!(ok, "ffmpeg {out}");
            dir.join(out)
        };
        let tone = ["-f", "lavfi", "-i", "sine=d=1"];
        let m4a = ff(&[&tone[..], &["-c:a", "aac"]].concat(), "a.m4a");
        let png = ff(
            &["-f", "lavfi", "-i", "color=red:s=64x64", "-frames:v", "1"],
            "c.png",
        );
        let png = png.to_str().unwrap();
        let mp3 = ff(
            &[
                &tone[..],
                &[
                    "-i",
                    png,
                    "-map",
                    "0",
                    "-map",
                    "1",
                    "-c:a",
                    "libmp3lame",
                    "-disposition:v",
                    "attached_pic",
                ],
            ]
            .concat(),
            "b.mp3",
        );
        let mp4 = ff(
            &[
                &tone[..],
                &["-f", "lavfi", "-i", "color=blue:s=64x64:d=1", "-shortest"],
            ]
            .concat(),
            "d.mp4",
        );
        assert!(!has_picture(&m4a));
        assert!(!has_picture(&mp3));
        assert!(has_picture(&mp4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pct_diff_bounds() {
        let black = vec![[0u8, 0, 0]; 256];
        let white = vec![[255u8, 255, 255]; 256];
        assert_eq!(pct_diff(&black, &black), 0.0);
        assert!(pct_diff(&black, &white) > 99.0);
    }

    // Live: WatchVideo on a real clip → image blocks + Groq transcript.
    // `cargo test watch_reel_clip_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn watch_reel_clip_live() {
        let path = std::env::var("WATCH_CLIP").unwrap_or_else(|_| {
            "/Volumes/Data01/agentic-workspace/dev-plan/52-ltx-lab/reel/clips/grok/shot-4.mp4"
                .into()
        });
        let r = WatchVideoTool
            .call_multimodal(json!({ "path": path, "lang": "th", "max_frames": 8 }))
            .await
            .expect("watch");
        let ToolResultContent::Blocks(b) = r else {
            panic!("expected blocks")
        };
        let imgs = b
            .iter()
            .filter(|x| matches!(x, ToolResultBlock::Image { .. }))
            .count();
        let summary = b
            .iter()
            .find_map(|x| match x {
                ToolResultBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap();
        println!("\n=== {imgs} image blocks ===\n{summary}\n");
        if summary.contains("audio only") {
            assert!(summary.contains("--- transcript"), "no transcript");
        } else {
            assert!(imgs > 0, "no frames returned");
        }
    }
}
