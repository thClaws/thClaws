//! Image-provider registry (dev-plan/40, Tier 1).
//!
//! Maps a model string (full id, alias, or empty for the default) to the
//! provider that handles it. Provider order matters: Gemini is tried
//! first so the empty/default model resolves to it (backward-compatible
//! with the pre-Tier-1 Gemini-only tools).

use crate::error::{Error, Result};
use crate::media::provider::{ImageModelInfo, ImageProvider, SpeechProvider, VideoProvider};
use crate::media::providers::{
    DashScopeSpeechProvider, DashScopeVideoProvider, GeminiImageProvider, GeminiSpeechProvider,
    IappImageProvider, LtxVideoProvider, OmniVideoProvider, OpenAiImageProvider, QwenImageProvider,
};
use std::sync::Arc;

/// Image providers this install can reach, in resolution priority order.
/// A gateway-locked install keeps only those its gateway routes.
pub fn all() -> Vec<Arc<dyn ImageProvider>> {
    reachable(registered())
}

fn registered() -> Vec<Arc<dyn ImageProvider>> {
    vec![
        Arc::new(GeminiImageProvider),
        Arc::new(OpenAiImageProvider),
        Arc::new(QwenImageProvider),
        Arc::new(IappImageProvider),
    ]
}

/// Video providers this install can reach, in resolution priority order.
pub fn video_all() -> Vec<Arc<dyn VideoProvider>> {
    reachable(video_registered())
}

/// The image / video model ids this install can actually call, when an org
/// gateway narrows the providers; `None` = every provider is open. The tool
/// schemas append it so the model never picks one it can't reach (SIS asked
/// for ltx-2-5-pro off the static description and failed, 2026-10-22).
pub fn reachable_image_models() -> Option<String> {
    crate::shared::gateway_providers_locked().then(|| {
        all()
            .iter()
            .flat_map(|p| p.models().iter().map(|m| m.id))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

pub fn reachable_video_models() -> Option<String> {
    crate::shared::gateway_providers_locked().then(|| {
        video_all()
            .iter()
            .flat_map(|p| p.models().iter().map(|m| m.id))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

fn video_registered() -> Vec<Arc<dyn VideoProvider>> {
    vec![
        Arc::new(OmniVideoProvider),
        Arc::new(LtxVideoProvider),
        Arc::new(DashScopeVideoProvider),
    ]
}

fn reachable<P: ?Sized + HasId>(ps: Vec<Arc<P>>) -> Vec<Arc<P>> {
    let locked = crate::shared::gateway_providers_locked();
    if !locked {
        return ps;
    }
    let routed = crate::shared::gateway_routed_providers();
    ps.into_iter()
        .filter(|p| reachable_under(p.provider_id(), locked, &routed))
        .collect()
}

/// Gateway segment a media provider's calls go through; `None` = BYOK only.
fn media_segment(id: &str, locked: bool, routed: &[String]) -> Option<&'static str> {
    match id {
        "gemini" | "omni" => Some("google"),
        "openai" => Some("openai"),
        "ltx" => Some("ltx"),
        "elevenlabs" => Some("elevenlabs"),
        "minimax" => Some("minimax"),
        "kie" => Some("kie"),
        "groq" => Some("groq"),
        "qwen" | "dashscope" | "happyhorse" => {
            crate::media::provider::dashscope_segment_for(locked, routed)
        }
        _ => None,
    }
}

/// Whether a media backend (`gemini`, `elevenlabs`, `kie`, `dashscope`…)
/// is reachable here: always when unlocked; on a locked install only
/// through a segment its gateway routes.
pub fn backend_reachable(id: &str) -> bool {
    let locked = crate::shared::gateway_providers_locked();
    locked_reachable(id, locked, &crate::shared::gateway_routed_providers())
}

pub(crate) fn locked_reachable(id: &str, locked: bool, routed: &[String]) -> bool {
    reachable_under(id, locked, routed)
}

fn reachable_under(id: &str, locked: bool, routed: &[String]) -> bool {
    !locked || media_segment(id, locked, routed).is_some_and(|s| routed.iter().any(|r| r == s))
}

/// The error for an explicit provider that exists but this install can't reach.
fn unreachable_provider(kind: &str, provider: &str, available: &[&str]) -> Error {
    Error::Tool(if available.is_empty() {
        format!("{kind} provider {provider:?} is not available on this deployment — it has no {kind} provider")
    } else {
        format!(
            "{kind} provider {provider:?} is not available on this deployment — available: {}",
            available.join(", ")
        )
    })
}

/// Speech (text→speech) providers this install can reach, in resolution
/// priority order: Gemini first (default gemini-3.1-flash-tts-preview), then
/// DashScope Qwen3-TTS — the default where the gateway serves only that.
pub fn speech_all() -> Vec<Arc<dyn SpeechProvider>> {
    reachable(speech_registered())
}

fn speech_registered() -> Vec<Arc<dyn SpeechProvider>> {
    vec![
        Arc::new(GeminiSpeechProvider),
        Arc::new(DashScopeSpeechProvider),
    ]
}

/// Resolve a speech `provider`/`model` pair to a concrete provider + its
/// native model id. Same semantics as [`resolve`] but over speech
/// providers (default: Gemini).
pub fn resolve_speech(provider: &str, model: &str) -> Result<(Arc<dyn SpeechProvider>, String)> {
    resolve_speech_in(speech_all(), provider, model)
}

fn resolve_speech_in(
    avail: Vec<Arc<dyn SpeechProvider>>,
    provider: &str,
    model: &str,
) -> Result<(Arc<dyn SpeechProvider>, String)> {
    let provider = provider.trim();
    let model = model.trim();

    if !provider.is_empty() {
        let p = avail
            .iter()
            .find(|p| p.id().eq_ignore_ascii_case(provider))
            .cloned()
            .ok_or_else(|| {
                if speech_registered()
                    .iter()
                    .any(|p| p.id().eq_ignore_ascii_case(provider))
                {
                    return unreachable_provider("speech", provider, &ids(&avail));
                }
                Error::Tool(format!(
                    "unknown speech provider {provider:?} — known: {}",
                    ids(&avail).join(", ")
                ))
            })?;
        let resolved = if model.is_empty() {
            p.models()
                .first()
                .map(|m| m.id.to_string())
                .ok_or_else(|| Error::Tool(format!("provider {:?} exposes no models", p.id())))?
        } else {
            p.resolve_model(model).ok_or_else(|| {
                Error::Tool(format!(
                    "provider {:?} doesn't have speech model {model:?} — try one of: {}",
                    p.id(),
                    p.models()
                        .iter()
                        .map(|m| m.id)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?
        };
        return Ok((p, resolved));
    }

    for p in &avail {
        if let Some(resolved) = p.resolve_model(model) {
            return Ok((p.clone(), resolved));
        }
    }
    // A locked install may not carry the provider that claims "".
    if model.is_empty() {
        if let Some((p, m)) = avail
            .first()
            .and_then(|p| p.models().first().map(|m| (p, m)))
        {
            return Ok((p.clone(), m.id.to_string()));
        }
    }
    if avail.is_empty() {
        return Err(Error::Tool(
            "text-to-speech is not available on this deployment — it has no speech provider".into(),
        ));
    }
    Err(Error::Tool(format!(
        "unknown speech model {model:?} — known: {}",
        avail
            .iter()
            .flat_map(|p| p.models().iter().map(|m| format!("{}:{}", p.id(), m.id)))
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Resolve a video `provider`/`model` pair to a concrete provider + its
/// native model id. Same semantics as [`resolve`] but over video
/// providers (default: Veo).
pub fn resolve_video(provider: &str, model: &str) -> Result<(Arc<dyn VideoProvider>, String)> {
    resolve_video_in(video_all(), provider, model)
}

fn resolve_video_in(
    avail: Vec<Arc<dyn VideoProvider>>,
    provider: &str,
    model: &str,
) -> Result<(Arc<dyn VideoProvider>, String)> {
    let provider = provider.trim();
    let model = model.trim();

    if !provider.is_empty() {
        let p = avail
            .iter()
            .find(|p| p.id().eq_ignore_ascii_case(provider))
            .cloned()
            .ok_or_else(|| {
                if video_registered()
                    .iter()
                    .any(|p| p.id().eq_ignore_ascii_case(provider))
                {
                    return unreachable_provider("video", provider, &ids(&avail));
                }
                Error::Tool(format!(
                    "unknown video provider {provider:?} — known: {}",
                    ids(&avail).join(", ")
                ))
            })?;
        let resolved = if model.is_empty() {
            p.models()
                .first()
                .map(|m| m.id.to_string())
                .ok_or_else(|| Error::Tool(format!("provider {:?} exposes no models", p.id())))?
        } else {
            p.resolve_model(model).ok_or_else(|| {
                Error::Tool(format!(
                    "provider {:?} doesn't have video model {model:?} — try one of: {}",
                    p.id(),
                    p.models()
                        .iter()
                        .map(|m| m.id)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?
        };
        return Ok((p, resolved));
    }

    for p in &avail {
        if let Some(resolved) = p.resolve_model(model) {
            return Ok((p.clone(), resolved));
        }
    }
    // A locked install may not carry the provider that claims "".
    if model.is_empty() {
        if let Some((p, m)) = avail
            .first()
            .and_then(|p| p.models().first().map(|m| (p, m)))
        {
            return Ok((p.clone(), m.id.to_string()));
        }
    }
    Err(Error::Tool(format!(
        "unknown video model {model:?} — known: {}",
        avail
            .iter()
            .flat_map(|p| p.models().iter().map(|m| format!("{}:{}", p.id(), m.id)))
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Resolve a `model` string (or `provider`/`model` pair) to a concrete
/// provider + its native model id.
///
/// - `provider` set ⇒ pick that provider, then resolve `model` within it
///   (empty `model` ⇒ that provider's default).
/// - `provider` empty ⇒ first provider that claims `model` wins; empty
///   `model` falls to the default provider (Gemini).
pub fn resolve(provider: &str, model: &str) -> Result<(Arc<dyn ImageProvider>, String)> {
    resolve_in(all(), provider, model)
}

fn resolve_in(
    avail: Vec<Arc<dyn ImageProvider>>,
    provider: &str,
    model: &str,
) -> Result<(Arc<dyn ImageProvider>, String)> {
    let provider = provider.trim();
    let model = model.trim();

    if !provider.is_empty() {
        let p = avail
            .iter()
            .find(|p| p.id().eq_ignore_ascii_case(provider))
            .cloned()
            .ok_or_else(|| {
                if registered()
                    .iter()
                    .any(|p| p.id().eq_ignore_ascii_case(provider))
                {
                    return unreachable_provider("image", provider, &ids(&avail));
                }
                Error::Tool(format!(
                    "unknown image provider {provider:?} — known: {}",
                    ids(&avail).join(", ")
                ))
            })?;
        // Empty model with an explicit provider ⇒ that provider's first
        // (default) model, even if the provider doesn't alias "".
        let resolved = if model.is_empty() {
            p.models()
                .first()
                .map(|m| m.id.to_string())
                .ok_or_else(|| Error::Tool(format!("provider {:?} exposes no models", p.id())))?
        } else {
            p.resolve_model(model).ok_or_else(|| {
                Error::Tool(format!(
                    "provider {:?} doesn't have model {model:?} — try one of: {}",
                    p.id(),
                    model_ids_for(&p).join(", ")
                ))
            })?
        };
        return Ok((p, resolved));
    }

    for p in &avail {
        if let Some(resolved) = p.resolve_model(model) {
            return Ok((p.clone(), resolved));
        }
    }
    // A locked install may not carry the provider that claims "".
    if model.is_empty() {
        if let Some((p, m)) = avail
            .first()
            .and_then(|p| p.models().first().map(|m| (p, m)))
        {
            return Ok((p.clone(), m.id.to_string()));
        }
    }
    Err(Error::Tool(format!(
        "unknown image model {model:?} — known: {}",
        all_model_hints(&avail).join(", ")
    )))
}

trait HasId {
    fn provider_id(&self) -> &'static str;
}

impl HasId for dyn ImageProvider {
    fn provider_id(&self) -> &'static str {
        self.id()
    }
}

impl HasId for dyn SpeechProvider {
    fn provider_id(&self) -> &'static str {
        self.id()
    }
}

impl HasId for dyn VideoProvider {
    fn provider_id(&self) -> &'static str {
        self.id()
    }
}

fn ids<P: ?Sized + HasId>(ps: &[Arc<P>]) -> Vec<&'static str> {
    ps.iter().map(|p| p.provider_id()).collect()
}

fn model_ids_for(p: &Arc<dyn ImageProvider>) -> Vec<&'static str> {
    p.models().iter().map(|m| m.id).collect()
}

/// `provider:model` hints for error messages and (Tier 3) pickers.
fn all_model_hints(avail: &[Arc<dyn ImageProvider>]) -> Vec<String> {
    let mut out = Vec::new();
    for p in avail {
        for m in p.models() {
            out.push(format!("{}:{}", p.id(), m.id));
        }
    }
    out
}

/// Flat list of every (provider_id, model) for the Studio picker (Tier 3)
/// and tests. Allocates; not on a hot path.
pub fn list_models() -> Vec<(&'static str, ImageModelInfo)> {
    let mut out = Vec::new();
    for p in all() {
        for m in p.models() {
            out.push((p.id(), *m));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routed(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn unlocked_reaches_every_provider() {
        for id in [
            "gemini",
            "openai",
            "qwen",
            "iapp",
            "omni",
            "ltx",
            "dashscope",
        ] {
            assert!(reachable_under(id, false, &[]), "{id}");
        }
    }

    #[test]
    fn sis_lock_reaches_only_dashscope_media() {
        let r = routed(&["sis"]);
        assert!(reachable_under("qwen", true, &r));
        assert!(reachable_under("dashscope", true, &r));
        for id in ["gemini", "openai", "iapp", "omni", "ltx"] {
            assert!(!reachable_under(id, true, &r), "{id}");
        }
    }

    #[test]
    fn lock_on_another_upstream_reaches_its_own_media() {
        let r = routed(&["openai", "google"]);
        assert!(reachable_under("openai", true, &r));
        assert!(reachable_under("omni", true, &r));
        assert!(!reachable_under("qwen", true, &r));
        assert!(!reachable_under("dashscope", true, &r));
    }

    #[test]
    fn dashscope_segment_follows_the_lock() {
        use crate::media::provider::dashscope_segment_for as seg;
        assert_eq!(seg(false, &[]), Some("dashscope"));
        assert_eq!(seg(false, &routed(&["sis"])), Some("dashscope"));
        assert_eq!(seg(true, &routed(&["sis"])), Some("sis"));
        assert_eq!(seg(true, &routed(&["sis", "dashscope"])), Some("dashscope"));
        assert_eq!(seg(true, &routed(&["openai"])), None);
    }

    #[test]
    fn locked_defaults_resolve_to_what_the_gateway_serves() {
        let r = routed(&["sis"]);
        let img: Vec<_> = registered()
            .into_iter()
            .filter(|p| reachable_under(p.id(), true, &r))
            .collect();
        let vid: Vec<_> = video_registered()
            .into_iter()
            .filter(|p| reachable_under(p.id(), true, &r))
            .collect();
        assert_eq!(resolve_in(img.clone(), "", "").unwrap().0.id(), "qwen");
        assert_eq!(
            resolve_video_in(vid.clone(), "", "").unwrap().0.id(),
            "dashscope"
        );
        let gemini = resolve_in(img.clone(), "gemini", "")
            .err()
            .unwrap()
            .to_string();
        assert!(
            gemini.contains("not available") && gemini.contains("qwen"),
            "{gemini}"
        );
        let omni = resolve_video_in(vid.clone(), "omni", "")
            .err()
            .unwrap()
            .to_string();
        assert!(
            omni.contains("not available") && omni.contains("dashscope"),
            "{omni}"
        );
        assert!(resolve_in(img, "", "flash").is_err());
        assert!(resolve_video_in(vid, "", "fast").is_err());
    }

    #[test]
    fn speech_follows_the_lock() {
        let unlocked = resolve_speech_in(speech_registered(), "", "").unwrap();
        assert_eq!(
            (unlocked.0.id(), unlocked.1.as_str()),
            ("gemini", "gemini-3.1-flash-tts-preview")
        );
        let r = routed(&["sis"]);
        let avail: Vec<_> = speech_registered()
            .into_iter()
            .filter(|p| reachable_under(p.id(), true, &r))
            .collect();
        let (p, m) = resolve_speech_in(avail.clone(), "", "").unwrap();
        assert_eq!((p.id(), m.as_str()), ("dashscope", "qwen3-tts-flash"));
        assert_eq!(
            resolve_speech_in(avail.clone(), "", "qwen-tts-instruct")
                .unwrap()
                .1,
            "qwen3-tts-instruct-flash"
        );
        let gemini = resolve_speech_in(avail.clone(), "gemini", "")
            .err()
            .unwrap()
            .to_string();
        assert!(
            gemini.contains("not available") && gemini.contains("dashscope"),
            "{gemini}"
        );
        let none = resolve_speech_in(Vec::new(), "", "")
            .err()
            .unwrap()
            .to_string();
        assert!(none.contains("not available on this deployment"), "{none}");
    }
}
