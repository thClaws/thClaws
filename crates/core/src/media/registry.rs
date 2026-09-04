//! Image-provider registry (dev-plan/40, Tier 1).
//!
//! Maps a model string (full id, alias, or empty for the default) to the
//! provider that handles it. Provider order matters: Gemini is tried
//! first so the empty/default model resolves to it (backward-compatible
//! with the pre-Tier-1 Gemini-only tools).

use crate::error::{Error, Result};
use crate::media::provider::{ImageModelInfo, ImageProvider, SpeechProvider, VideoProvider};
use crate::media::providers::{
    DashScopeVideoProvider, GeminiImageProvider, GeminiSpeechProvider, IappImageProvider,
    LtxVideoProvider, OpenAiImageProvider, QwenImageProvider, VeoVideoProvider,
};
use std::sync::Arc;

/// All registered image providers, in resolution priority order.
pub fn all() -> Vec<Arc<dyn ImageProvider>> {
    vec![
        Arc::new(GeminiImageProvider),
        Arc::new(OpenAiImageProvider),
        Arc::new(QwenImageProvider),
        Arc::new(IappImageProvider),
    ]
}

/// All registered video providers, in resolution priority order.
pub fn video_all() -> Vec<Arc<dyn VideoProvider>> {
    vec![
        Arc::new(VeoVideoProvider),
        Arc::new(LtxVideoProvider),
        Arc::new(DashScopeVideoProvider),
    ]
}

/// All registered speech (text→speech) providers, in resolution priority
/// order. Gemini only for now (default: gemini-3.1-flash-tts-preview).
pub fn speech_all() -> Vec<Arc<dyn SpeechProvider>> {
    vec![Arc::new(GeminiSpeechProvider)]
}

/// Resolve a speech `provider`/`model` pair to a concrete provider + its
/// native model id. Same semantics as [`resolve`] but over speech
/// providers (default: Gemini).
pub fn resolve_speech(provider: &str, model: &str) -> Result<(Arc<dyn SpeechProvider>, String)> {
    let provider = provider.trim();
    let model = model.trim();

    if !provider.is_empty() {
        let p = speech_all()
            .into_iter()
            .find(|p| p.id().eq_ignore_ascii_case(provider))
            .ok_or_else(|| {
                Error::Tool(format!(
                    "unknown speech provider {provider:?} — known: {}",
                    speech_all()
                        .iter()
                        .map(|p| p.id())
                        .collect::<Vec<_>>()
                        .join(", ")
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

    for p in speech_all() {
        if let Some(resolved) = p.resolve_model(model) {
            return Ok((p, resolved));
        }
    }
    Err(Error::Tool(format!(
        "unknown speech model {model:?} — known: {}",
        speech_all()
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
    let provider = provider.trim();
    let model = model.trim();

    if !provider.is_empty() {
        let p = video_all()
            .into_iter()
            .find(|p| p.id().eq_ignore_ascii_case(provider))
            .ok_or_else(|| {
                Error::Tool(format!(
                    "unknown video provider {provider:?} — known: {}",
                    video_all()
                        .iter()
                        .map(|p| p.id())
                        .collect::<Vec<_>>()
                        .join(", ")
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

    for p in video_all() {
        if let Some(resolved) = p.resolve_model(model) {
            return Ok((p, resolved));
        }
    }
    Err(Error::Tool(format!(
        "unknown video model {model:?} — known: {}",
        video_all()
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
    let provider = provider.trim();
    let model = model.trim();

    if !provider.is_empty() {
        let p = all()
            .into_iter()
            .find(|p| p.id().eq_ignore_ascii_case(provider))
            .ok_or_else(|| {
                Error::Tool(format!(
                    "unknown image provider {provider:?} — known: {}",
                    provider_ids().join(", ")
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

    for p in all() {
        if let Some(resolved) = p.resolve_model(model) {
            return Ok((p, resolved));
        }
    }
    Err(Error::Tool(format!(
        "unknown image model {model:?} — known: {}",
        all_model_hints().join(", ")
    )))
}

fn provider_ids() -> Vec<&'static str> {
    all().iter().map(|p| p.id()).collect()
}

fn model_ids_for(p: &Arc<dyn ImageProvider>) -> Vec<&'static str> {
    p.models().iter().map(|m| m.id).collect()
}

/// `provider:model` hints for error messages and (Tier 3) pickers.
fn all_model_hints() -> Vec<String> {
    let mut out = Vec::new();
    for p in all() {
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
