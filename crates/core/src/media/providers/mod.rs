//! Concrete `ImageProvider` implementations (dev-plan/40, Tier 1).

pub mod dashscope_tts;
pub mod dashscope_video;
pub mod gemini;
pub mod iapp;
pub mod ltx;
pub mod omni;
pub mod openai;
pub mod qwen;

pub use dashscope_tts::DashScopeSpeechProvider;
pub use dashscope_video::DashScopeVideoProvider;
pub use gemini::{GeminiImageProvider, GeminiSpeechProvider};
pub use iapp::IappImageProvider;
pub use ltx::LtxVideoProvider;
pub use omni::OmniVideoProvider;
pub use openai::OpenAiImageProvider;
pub use qwen::QwenImageProvider;
