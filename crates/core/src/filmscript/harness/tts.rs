//! TTS synthesis for `say` lines — greenfield provider integration
//! (nothing else in the engine speaks). Two providers, three routes,
//! all proven by ear in the T0 spike (`dev-plan/52-t0-spike/FINDINGS.md`):
//!
//! 1. Gemini `gemini-3.1-flash-tts-preview`, direct `generateContent`
//!    (full `prebuiltVoiceConfig`) — the quality winner.
//! 2. The same model via OpenRouter `POST /api/v1/audio/speech` when
//!    only `OPENROUTER_API_KEY` is present — `response_format:"pcm"`
//!    is the ONLY accepted format (24kHz/16-bit mono, wrapped to WAV
//!    here); needs ≥$0.50 balance headroom.
//! 3. MiniMax `speech-02-hd` (`language_boost`) as the fallback voice.
//!
//! Output contract for the compiler's `E_AUDIO_OVERRUN` + Kie's
//! audio-ref constraint: mp3, padded to ≥2s (T0: "ไปทำงาน" came out
//! 0.94s — the pad is mandatory, not theoretical), `duration_ms` from
//! ffprobe on the final file (the padded length is what counts).

use super::{cache_dir, ffprobe_duration_ms, USER_AGENT};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

const MIN_AUDIO_SECS: f64 = 2.0;
const PAD_TO_SECS: &str = "2.2";

/// Providers the dispatcher can resolve (for the error message + D3
/// validation). Speech only in D2; music/SFX (AudioProvider, design B.1a)
/// are a later extension.
const KNOWN_PROVIDERS: &[&str] = &["elevenlabs", "openai", "gemini", "minimax", "dashscope"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceDef {
    pub provider: String,
    pub voice: String,
    /// The provider's model id — **the model, not just the provider,
    /// decides Thai** (`eleven_v3` works, `eleven_multilingual_v2` does not;
    /// `gpt-4o-mini-tts` works, `tts-1` does not — the Thai shootout,
    /// `docs/video-pipeline/tts-providers.md`). `None` = the provider's
    /// built-in default, preserving pre-D2 behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
}

/// `voices.json` — agent-shipped, user-editable, project-scoped at
/// `.thclaws/film/voices.json`. Built-in defaults cover the spec's
/// example ids so a bare workspace still compiles.
pub fn load_registry() -> BTreeMap<String, VoiceDef> {
    let mut reg: BTreeMap<String, VoiceDef> = BTreeMap::new();
    for (id, provider, voice) in builtin_voices(
        crate::media::registry::backend_reachable("gemini"),
        crate::media::registry::backend_reachable("dashscope"),
    ) {
        reg.insert(
            id.into(),
            VoiceDef {
                provider: provider.into(),
                voice: voice.into(),
                model: None,
                style: None,
            },
        );
    }
    if let Ok(s) = std::fs::read_to_string(super::film_root().join("voices.json")) {
        if let Ok(user) = serde_json::from_str::<BTreeMap<String, VoiceDef>>(&s) {
            reg.extend(user);
        }
    }
    reg
}

/// The built-in voice ids, on Gemini — or on Qwen3-TTS where only DashScope
/// is reachable (a gateway-locked install such as SIS).
fn builtin_voices(
    gemini: bool,
    dashscope: bool,
) -> [(&'static str, &'static str, &'static str); 3] {
    if !gemini && dashscope {
        [
            ("th-female-warm", "dashscope", "Cherry"),
            ("th-male-low", "dashscope", "Ethan"),
            ("narrator", "dashscope", "Ethan"),
        ]
    } else {
        [
            ("th-female-warm", "gemini", "Kore"),
            ("th-male-low", "gemini", "Charon"),
            ("narrator", "gemini", "Charon"),
        ]
    }
}

fn lang_name(lang: &str) -> &str {
    match lang {
        "th" => "Thai",
        "en" => "English",
        other => other,
    }
}

fn style_prompt(text: &str, lang: &str, tone_hint: Option<&str>, style: Option<&str>) -> String {
    let base = style.map(str::to_string).unwrap_or_else(|| {
        format!(
            "Speak in natural {} with a native accent, calm conversational drama tone",
            lang_name(lang)
        )
    });
    match tone_hint {
        Some(t) => format!("{base}, {t}: {text}"),
        None => format!("{base}: {text}"),
    }
}

/// Synthesize one line into `cache/tts/<asset_id>.mp3` (content-
/// addressed by the compiler's asset id — same voice+text+tone → same
/// file, no re-synthesis). Returns `(path, duration_ms)` of the final
/// padded mp3.
pub async fn synthesize(
    asset_id: &str,
    text: &str,
    voice_id: &str,
    lang: &str,
    tone_hint: Option<&str>,
) -> Result<(PathBuf, u64)> {
    let dir = cache_dir().join("tts");
    std::fs::create_dir_all(&dir)?;
    let out = dir.join(format!("{asset_id}.mp3"));
    if out.exists() {
        let ms = ffprobe_duration_ms(&out)?;
        return Ok((out, ms));
    }

    let registry = load_registry();
    let def = registry.get(voice_id).ok_or_else(|| {
        Error::Tool(format!(
            "voice id '{voice_id}' not in voices.json (known: {})",
            registry.keys().cloned().collect::<Vec<_>>().join(", ")
        ))
    })?;

    let prompt = style_prompt(text, lang, tone_hint, def.style.as_deref());
    let provider = provider_for(&def.provider).ok_or_else(|| {
        Error::Tool(format!(
            "voice '{voice_id}': unknown TTS provider '{}' ({})",
            def.provider,
            KNOWN_PROVIDERS.join("|")
        ))
    })?;
    if !crate::media::registry::backend_reachable(&def.provider) {
        let avail: Vec<&str> = KNOWN_PROVIDERS
            .iter()
            .copied()
            .filter(|p| crate::media::registry::backend_reachable(p))
            .collect();
        return Err(Error::Tool(format!(
            "voice '{voice_id}': TTS provider '{}' is not available on this deployment — use a voice on: {}",
            def.provider,
            if avail.is_empty() { "(none)".to_string() } else { avail.join(", ") }
        )));
    }
    let req = TtsRequest {
        text,
        prompt: &prompt,
        voice: &def.voice,
        model: def.model.as_deref(),
        lang,
        tone: tone_hint,
    };
    let TtsAudio { bytes: raw, ext } = provider.synthesize(&req).await?;

    let raw_path = dir.join(format!("{asset_id}.raw.{ext}"));
    std::fs::write(&raw_path, &raw)?;
    let raw_ms = ffprobe_duration_ms(&raw_path)?;

    // Normalize to mp3 + enforce Kie's 2–15s audio-ref floor.
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-y", "-v", "quiet", "-i"]).arg(&raw_path);
    if (raw_ms as f64) < MIN_AUDIO_SECS * 1000.0 {
        cmd.args(["-af", &format!("apad=whole_dur={PAD_TO_SECS}")]);
    }
    cmd.args(["-b:a", "128k"]).arg(&out);
    let status = cmd
        .status()
        .map_err(|e| Error::Tool(format!("ffmpeg: {e}")))?;
    let _ = std::fs::remove_file(&raw_path);
    if !status.success() {
        return Err(Error::Tool("ffmpeg failed normalizing TTS output".into()));
    }
    let ms = ffprobe_duration_ms(&out)?;
    Ok((out, ms))
}

/// One synthesis request, provider-neutral. Prompt-driven providers
/// (Gemini) read `prompt` (styled text); text-driven ones (MiniMax) read
/// `text` + `lang`. `model` overrides the provider default (per-voice, from
/// the Thai shootout).
pub struct TtsRequest<'a> {
    pub text: &'a str,
    pub prompt: &'a str,
    pub voice: &'a str,
    pub model: Option<&'a str>,
    pub lang: &'a str,
    pub tone: Option<&'a str>,
}

pub struct TtsAudio {
    pub bytes: Vec<u8>,
    pub ext: &'static str,
}

/// Compile/runtime capability contract per TTS provider (design part B).
/// `languages` is keyed on the effective (provider, model); the values
/// here are the built-in defaults. Consumed by D3 validation.
#[derive(Debug, Clone, Copy)]
pub struct TtsCaps {
    pub id: &'static str,
    pub languages: &'static [&'static str],
    pub out_ext: &'static str,
    pub supports_style_prompt: bool,
}

#[async_trait]
pub trait TtsProvider: Send + Sync {
    fn caps(&self) -> TtsCaps;
    async fn synthesize(&self, req: &TtsRequest<'_>) -> Result<TtsAudio>;
}

fn provider_for(name: &str) -> Option<Box<dyn TtsProvider>> {
    match name {
        "elevenlabs" => Some(Box::new(ElevenLabsTts)),
        "openai" => Some(Box::new(OpenAiTts)),
        "gemini" => Some(Box::new(GeminiTts)),
        "minimax" => Some(Box::new(MiniMaxTts)),
        "dashscope" => Some(Box::new(DashScopeTts)),
        _ => None,
    }
}

/// Qwen3-TTS through DashScope (`sis` on SIS). Thai speaks in `Auto` mode;
/// the tone hint steers delivery only on the instruct model.
struct DashScopeTts;

#[async_trait]
impl TtsProvider for DashScopeTts {
    fn caps(&self) -> TtsCaps {
        TtsCaps {
            id: "dashscope",
            languages: &["th", "en"],
            out_ext: "wav",
            supports_style_prompt: false,
        }
    }
    async fn synthesize(&self, req: &TtsRequest<'_>) -> Result<TtsAudio> {
        Ok(TtsAudio {
            bytes: crate::media::providers::dashscope_tts::synthesize_wav(
                req.model
                    .unwrap_or(crate::media::providers::dashscope_tts::DEFAULT_MODEL),
                req.text,
                req.voice,
                req.lang,
                req.tone,
            )
            .await?,
            ext: "wav",
        })
    }
}

struct ElevenLabsTts;

#[async_trait]
impl TtsProvider for ElevenLabsTts {
    fn caps(&self) -> TtsCaps {
        TtsCaps {
            id: "elevenlabs",
            languages: &["th", "en"],
            out_ext: "mp3",
            supports_style_prompt: false,
        }
    }
    async fn synthesize(&self, req: &TtsRequest<'_>) -> Result<TtsAudio> {
        Ok(TtsAudio {
            bytes: elevenlabs_tts(req.text, req.voice, req.model).await?,
            ext: "mp3",
        })
    }
}

struct OpenAiTts;

#[async_trait]
impl TtsProvider for OpenAiTts {
    fn caps(&self) -> TtsCaps {
        TtsCaps {
            id: "openai",
            languages: &["th", "en"],
            out_ext: "mp3",
            supports_style_prompt: true,
        }
    }
    async fn synthesize(&self, req: &TtsRequest<'_>) -> Result<TtsAudio> {
        Ok(TtsAudio {
            bytes: openai_tts(req.text, req.voice, req.model, req.tone).await?,
            ext: "mp3",
        })
    }
}

struct GeminiTts;

#[async_trait]
impl TtsProvider for GeminiTts {
    fn caps(&self) -> TtsCaps {
        TtsCaps {
            id: "gemini",
            languages: &["th", "en"],
            out_ext: "wav",
            supports_style_prompt: true,
        }
    }
    async fn synthesize(&self, req: &TtsRequest<'_>) -> Result<TtsAudio> {
        Ok(TtsAudio {
            bytes: gemini_tts(req.prompt, req.voice, req.model).await?,
            ext: "wav",
        })
    }
}

struct MiniMaxTts;

#[async_trait]
impl TtsProvider for MiniMaxTts {
    fn caps(&self) -> TtsCaps {
        TtsCaps {
            id: "minimax",
            languages: &["th", "en"],
            out_ext: "mp3",
            supports_style_prompt: false,
        }
    }
    async fn synthesize(&self, req: &TtsRequest<'_>) -> Result<TtsAudio> {
        Ok(TtsAudio {
            bytes: minimax_tts(req.text, req.voice, req.lang, req.model).await?,
            ext: "mp3",
        })
    }
}

fn env_key(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .expect("reqwest client")
}

const GEMINI_TTS_MODEL: &str = "gemini-3.1-flash-tts-preview";
const ELEVENLABS_TTS_MODEL: &str = "eleven_v3";
const OPENAI_TTS_MODEL: &str = "gpt-4o-mini-tts";

/// ElevenLabs TTS (the default Thai voice — best in the shootout). `voice`
/// is an ElevenLabs voice id; `model` overrides `eleven_v3` (the ONLY
/// Thai-capable model — `eleven_multilingual_v2` mis-speaks Thai). mp3 out.
async fn elevenlabs_tts(text: &str, voice: &str, model: Option<&str>) -> Result<Vec<u8>> {
    // BYOK-or-gateway (dev-plan/53 Stage D). Auth scheme differs by
    // route: ElevenLabs itself wants `xi-api-key`; the gateway auths on
    // `Authorization: Bearer` and injects the real xi-api-key upstream.
    let ep = crate::media::provider::resolve_endpoint(
        &["ELEVENLABS_API_KEY"],
        "https://api.elevenlabs.io",
        "elevenlabs",
    )?;
    let body = json!({
        "text": text,
        "model_id": model.unwrap_or(ELEVENLABS_TTS_MODEL),
        "voice_settings": { "stability": 0.45, "similarity_boost": 0.8 },
    });
    let mut req = crate::multi_tenant::attach_member(
        http().post(format!("{}/v1/text-to-speech/{voice}", ep.base_url)),
    )
    .header("Accept", "audio/mpeg");
    req = if ep.via_gateway {
        req.bearer_auth(&ep.api_key)
    } else {
        req.header("xi-api-key", &ep.api_key)
    };
    let resp = req
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("elevenlabs tts: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let t = resp.text().await.unwrap_or_default();
        return Err(Error::Tool(format!(
            "elevenlabs tts {status}: {}",
            t.chars().take(200).collect::<String>()
        )));
    }
    Ok(resp
        .bytes()
        .await
        .map_err(|e| Error::Tool(format!("elevenlabs tts body: {e}")))?
        .to_vec())
}

/// OpenAI TTS. Only `gpt-4o-mini-tts` speaks Thai (the older `tts-1` reads
/// it as gibberish); `tone` maps to the `instructions` style field. mp3 out.
async fn openai_tts(
    text: &str,
    voice: &str,
    model: Option<&str>,
    tone: Option<&str>,
) -> Result<Vec<u8>> {
    // BYOK-or-gateway: a real OPENAI_API_KEY → direct; else route via the
    // gateway `/openai` segment + attach the member. Note: the gateway
    // token meter 400s an audio-speech model that has no model_pricing
    // row (fail-closed, no silent unbilled call) until an openai-audio
    // media route + price is added.
    let ep = crate::media::provider::resolve_endpoint(
        &["OPENAI_API_KEY"],
        "https://api.openai.com",
        "openai",
    )?;
    let mut body = json!({
        "model": model.unwrap_or(OPENAI_TTS_MODEL),
        "voice": voice,
        "input": text,
        "response_format": "mp3",
    });
    if let Some(t) = tone {
        body.as_object_mut()
            .unwrap()
            .insert("instructions".into(), json!(t));
    }
    let resp =
        crate::multi_tenant::attach_member(http().post(format!("{}/v1/audio/speech", ep.base_url)))
            .bearer_auth(&ep.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("openai tts: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let t = resp.text().await.unwrap_or_default();
        return Err(Error::Tool(format!(
            "openai tts {status}: {}",
            t.chars().take(200).collect::<String>()
        )));
    }
    Ok(resp
        .bytes()
        .await
        .map_err(|e| Error::Tool(format!("openai tts body: {e}")))?
        .to_vec())
}

/// Gemini TTS: direct AI-Studio key first (full voice control), else
/// the OpenRouter route. Returns WAV bytes (PCM wrapped here). `model`
/// overrides [`GEMINI_TTS_MODEL`].
async fn gemini_tts(prompt: &str, voice: &str, model: Option<&str>) -> Result<Vec<u8>> {
    let model = model.unwrap_or(GEMINI_TTS_MODEL);
    // BYOK-or-gateway (dp53 Stage D + metering audit). A real
    // GEMINI_API_KEY → direct AI Studio; otherwise route the
    // `generateContent` call through the gateway `/google` segment,
    // which METERS it by tokens (the TTS model has a model_pricing row +
    // returns usageMetadata) and attributes it per member. This closes
    // the old hole where hosted TTS bypassed the gateway entirely.
    match crate::media::provider::resolve_endpoint(
        &["GEMINI_API_KEY"],
        "https://generativelanguage.googleapis.com",
        "google",
    ) {
        Ok(ep) => gemini_generate(prompt, voice, model, &ep).await,
        // No gemini key AND no gateway (desktop w/o gemini) → OpenRouter BYOK.
        // Never on a gateway-locked install: that would be the user's own
        // OpenRouter account, off the org's gateway.
        Err(e) => {
            if crate::shared::gateway_providers_locked() {
                return Err(e);
            }
            if let Some(key) = env_key("OPENROUTER_API_KEY").filter(|k| k != "gateway-placeholder")
            {
                return gemini_openrouter(prompt, voice, model, &key).await;
            }
            Err(e)
        }
    }
}

async fn gemini_generate(
    prompt: &str,
    voice: &str,
    model: &str,
    ep: &crate::media::provider::ResolvedEndpoint,
) -> Result<Vec<u8>> {
    let body = json!({
        "contents": [{ "parts": [{ "text": prompt }] }],
        "generationConfig": {
            "responseModalities": ["AUDIO"],
            "speechConfig": { "voiceConfig": { "prebuiltVoiceConfig": { "voiceName": voice } } }
        }
    });
    let url = format!("{}/v1beta/models/{model}:generateContent", ep.base_url);
    let mut rb = crate::multi_tenant::attach_member(http().post(url));
    // Gateway auths on `Authorization: Bearer <gw-key>` (it injects the
    // real Google key upstream); direct AI Studio wants `x-goog-api-key`.
    rb = if ep.via_gateway {
        rb.bearer_auth(&ep.api_key)
    } else {
        rb.header("x-goog-api-key", &ep.api_key)
    };
    let resp: serde_json::Value = rb
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("gemini tts: {e}")))?
        .json()
        .await
        .map_err(|e| Error::Tool(format!("gemini tts response: {e}")))?;

    let b64 = resp["candidates"][0]["content"]["parts"]
        .as_array()
        .and_then(|parts| {
            parts.iter().find_map(|p| {
                p["inlineData"]["data"]
                    .as_str()
                    .or_else(|| p["inline_data"]["data"].as_str())
            })
        })
        .ok_or_else(|| {
            Error::Tool(format!(
                "gemini tts gave no audio: {}",
                resp.to_string().chars().take(200).collect::<String>()
            ))
        })?;
    use base64::Engine;
    let pcm = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| Error::Tool(format!("gemini tts base64: {e}")))?;
    Ok(wrap_wav_24k_mono(&pcm))
}

/// OpenRouter's TTS endpoint only speaks `pcm` for Gemini models (T0:
/// mp3/wav rejected with "Gemini TTS only supports pcm").
async fn gemini_openrouter(prompt: &str, voice: &str, model: &str, key: &str) -> Result<Vec<u8>> {
    let body = json!({
        "model": format!("google/{model}"),
        "input": prompt,
        "voice": voice,
        "response_format": "pcm",
    });
    let resp = http()
        .post("https://openrouter.ai/api/v1/audio/speech")
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("openrouter tts: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::Tool(format!(
            "openrouter tts {status}: {}",
            text.chars().take(200).collect::<String>()
        )));
    }
    let pcm = resp
        .bytes()
        .await
        .map_err(|e| Error::Tool(format!("openrouter tts body: {e}")))?;
    Ok(wrap_wav_24k_mono(&pcm))
}

async fn minimax_tts(text: &str, voice: &str, lang: &str, model: Option<&str>) -> Result<Vec<u8>> {
    // BYOK-or-gateway (see openai_tts): real key → direct; else gateway
    // `/minimax` + member attribution (gateway 400s the un-priced t2a
    // model → fail-closed rather than unbilled).
    let ep = crate::media::provider::resolve_endpoint(
        &["MINIMAX_API_KEY"],
        "https://api.minimax.io",
        "minimax",
    )?;
    let boost = match lang {
        "th" => "Thai",
        "en" => "English",
        other => other,
    };
    let body = json!({
        "model": model.unwrap_or("speech-02-hd"),
        "text": text,
        "language_boost": boost,
        "voice_setting": { "voice_id": voice, "speed": 1.0, "vol": 1.0, "pitch": 0 },
        "audio_setting": { "sample_rate": 32000, "bitrate": 128000, "format": "mp3", "channel": 1 }
    });
    let resp: serde_json::Value =
        crate::multi_tenant::attach_member(http().post(format!("{}/v1/t2a_v2", ep.base_url)))
            .bearer_auth(&ep.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("minimax tts: {e}")))?
            .json()
            .await
            .map_err(|e| Error::Tool(format!("minimax tts response: {e}")))?;
    let hex = resp["data"]["audio"].as_str().ok_or_else(|| {
        Error::Tool(format!(
            "minimax tts gave no audio: {}",
            resp.to_string().chars().take(200).collect::<String>()
        ))
    })?;
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
        .collect::<std::result::Result<Vec<u8>, _>>()
        .map_err(|e| Error::Tool(format!("minimax tts hex: {e}")))
}

/// Minimal RIFF/WAV header around raw 24kHz/16-bit mono PCM — what
/// both Gemini routes emit.
fn wrap_wav_24k_mono(pcm: &[u8]) -> Vec<u8> {
    let sample_rate: u32 = 24_000;
    let byte_rate = sample_rate * 2;
    let data_len = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn builtin_voices_move_to_dashscope_only_when_gemini_is_unreachable() {
        assert!(builtin_voices(true, true).iter().all(|v| v.1 == "gemini"));
        assert!(builtin_voices(true, false).iter().all(|v| v.1 == "gemini"));
        let sis = builtin_voices(false, true);
        assert!(sis.iter().all(|v| v.1 == "dashscope"));
        assert!(provider_for("dashscope").is_some());
        assert!(KNOWN_PROVIDERS.contains(&"dashscope"));
    }

    use super::*;

    #[test]
    fn builtin_registry_covers_spec_ids() {
        let r = load_registry();
        assert_eq!(r["th-female-warm"].voice, "Kore");
        assert_eq!(r["th-male-low"].voice, "Charon");
    }

    #[test]
    fn every_known_provider_resolves() {
        for p in KNOWN_PROVIDERS {
            assert!(provider_for(p).is_some(), "unresolved provider: {p}");
        }
        assert!(provider_for("bogus").is_none());
    }

    #[test]
    fn style_prompt_folds_tone() {
        let p = style_prompt("สวัสดี", "th", Some("หงุดหงิด"), None);
        assert!(p.contains("Thai"));
        assert!(p.contains("หงุดหงิด: สวัสดี"), "{p}");
    }

    #[test]
    fn wav_header_math() {
        let wav = wrap_wav_24k_mono(&[0u8; 48_000]); // 1s of 24kHz/16-bit
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(wav.len(), 44 + 48_000);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 48_000);
    }
}
