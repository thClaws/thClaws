//! Core message/content types mirroring the Anthropic wire format.
//!
//! These are the canonical in-memory shape. Provider modules (Anthropic, OpenAI)
//! are responsible for adapting their own wire formats to/from these types.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

/// A single piece of message content.
///
/// `#[serde(tag = "type", rename_all = "snake_case")]` produces the Anthropic wire
/// format: `{"type":"text","text":"..."}` / `{"type":"tool_use",...}` / etc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// Reasoning / chain-of-thought emitted by thinking models (DeepSeek
    /// v4-*, OpenAI o1/o3, Anthropic extended thinking). Captured so it can
    /// be echoed back on subsequent turns — DeepSeek's `reasoning_content`
    /// requirement and Anthropic's signed-thinking blocks both reject
    /// requests where prior thinking is missing from history.
    ///
    /// `signature` is only set by providers that emit one (Anthropic);
    /// OpenAI-compat reasoning_content has no signature, so it stays None.
    /// Providers that don't support thinking simply skip these blocks
    /// during serialization — see `messages_to_*` impls.
    Thinking {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    /// Inline image attached directly to a user message (paste / drag-drop
    /// in the chat composer). Distinct from images returned by the Read
    /// tool, which ride inside `ToolResult.content` as `ToolResultBlock::Image`
    /// — both wrap the same `ImageSource` payload, but the agent loop
    /// treats user-attached images as part of the prompt rather than a
    /// tool result. Vision-capable Anthropic / OpenAI / Gemini models
    /// receive the pixels directly; non-vision models see this block
    /// flattened to nothing (the sibling Text block carries the text).
    Image {
        source: ImageSource,
    },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }
}

/// Where an image's bytes come from. Today only inline base64; future
/// variants (URL, file-asset reference) plug in here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
}

/// One element of a multimodal `tool_result` content array. Mirrors the
/// nested-content shape the Anthropic API accepts for tool_results that
/// carry image attachments.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultBlock {
    Text { text: String },
    Image { source: ImageSource },
}

/// A tool's returned content. Either a plain string (the common case —
/// every existing tool falls here via `From<String>`) or a sequence of
/// mixed text/image blocks (the Read tool on an image file).
///
/// `#[serde(untagged)]` so it serializes as either `"content": "..."`
/// or `"content": [...]` — both are valid Anthropic wire shapes for a
/// tool_result, so the Anthropic provider gets multimodal support
/// "for free" via this enum's serde representation. Other providers
/// flatten via `to_text()` until they grow their own image handling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultBlock>),
}

impl From<String> for ToolResultContent {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}

impl From<&str> for ToolResultContent {
    fn from(s: &str) -> Self {
        Self::Text(s.to_string())
    }
}

impl ToolResultContent {
    /// Plain-text rendering for callers that can't display images
    /// (history list, compaction, OpenAI/Gemini/etc. before they grow
    /// multimodal support). Image blocks render as nothing — the Read
    /// tool always pairs each Image with a Text summary block, so the
    /// summary still reaches the model.
    pub fn to_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ToolResultBlock::Text { text } => Some(text.as_str()),
                    ToolResultBlock::Image { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text(s) => s.is_empty(),
            Self::Blocks(blocks) => blocks.is_empty(),
        }
    }
}

/// A single message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::text(text)],
        }
    }
}

/// A tool definition exposed to the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            "\"assistant\""
        );
    }

    #[test]
    fn text_block_anthropic_wire_shape() {
        let block = ContentBlock::text("hello");
        let j = serde_json::to_value(&block).unwrap();
        assert_eq!(j, serde_json::json!({"type": "text", "text": "hello"}));
    }

    #[test]
    fn tool_use_block_wire_shape() {
        let block = ContentBlock::ToolUse {
            id: "toolu_1".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path": "/tmp/x"}),
            thought_signature: None,
        };
        let j = serde_json::to_value(&block).unwrap();
        assert_eq!(
            j,
            serde_json::json!({
                "type": "tool_use",
                "id": "toolu_1",
                "name": "read_file",
                "input": {"path": "/tmp/x"}
            })
        );
    }

    #[test]
    fn tool_result_skips_is_error_when_false() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: "ok".into(),
            is_error: false,
        };
        let j = serde_json::to_value(&block).unwrap();
        assert_eq!(
            j,
            serde_json::json!({
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": "ok"
            })
        );
    }

    #[test]
    fn tool_result_includes_is_error_when_true() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: "boom".into(),
            is_error: true,
        };
        let j = serde_json::to_value(&block).unwrap();
        assert_eq!(j["is_error"], serde_json::Value::Bool(true));
    }

    #[test]
    fn message_roundtrip() {
        let m = Message::user("hi");
        let s = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn tool_result_text_serializes_as_string() {
        // Existing wire shape — must stay a bare string so v0.3.1
        // sessions deserialize unchanged after the v0.3.2 type change.
        let block = ContentBlock::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: "ok".into(),
            is_error: false,
        };
        let j = serde_json::to_value(&block).unwrap();
        assert_eq!(j["content"], serde_json::json!("ok"));
    }

    #[test]
    fn tool_result_blocks_serialize_as_array_anthropic_shape() {
        // Multimodal wire shape — array of typed sub-blocks. Anthropic
        // accepts this natively for tool_results that carry images.
        let block = ContentBlock::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: ToolResultContent::Blocks(vec![
                ToolResultBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    },
                },
                ToolResultBlock::Text {
                    text: "image: x.png".into(),
                },
            ]),
            is_error: false,
        };
        let j = serde_json::to_value(&block).unwrap();
        assert_eq!(
            j["content"],
            serde_json::json!([
                {
                    "type": "image",
                    "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}
                },
                {"type": "text", "text": "image: x.png"}
            ])
        );
    }

    #[test]
    fn tool_result_content_roundtrips_both_shapes() {
        let s_text = serde_json::to_string(&ToolResultContent::Text("hi".into())).unwrap();
        assert_eq!(s_text, "\"hi\"");
        let back_text: ToolResultContent = serde_json::from_str(&s_text).unwrap();
        assert!(matches!(back_text, ToolResultContent::Text(ref s) if s == "hi"));

        let blocks = ToolResultContent::Blocks(vec![ToolResultBlock::Text { text: "hi".into() }]);
        let s_blocks = serde_json::to_string(&blocks).unwrap();
        let back_blocks: ToolResultContent = serde_json::from_str(&s_blocks).unwrap();
        assert!(matches!(back_blocks, ToolResultContent::Blocks(ref v) if v.len() == 1));
    }

    #[test]
    fn content_block_image_serializes_anthropic_native_shape() {
        // ContentBlock::Image used directly in a user message —
        // serializes via the existing #[serde(tag = "type")] machinery.
        // Anthropic's user-content array accepts this exact shape, so
        // the provider needs zero extra serialization code.
        let block = ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            },
        };
        let j = serde_json::to_value(&block).unwrap();
        assert_eq!(
            j,
            serde_json::json!({
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}
            })
        );
    }

    #[test]
    fn to_text_drops_image_blocks_keeps_text() {
        let c = ToolResultContent::Blocks(vec![
            ToolResultBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "ZZZ".into(),
                },
            },
            ToolResultBlock::Text {
                text: "summary line".into(),
            },
        ]);
        // Image renders as nothing, sibling text carries the meaning —
        // so providers that flatten via to_text() (OpenAI/Gemini/Ollama
        // before they grow image support) still get the descriptive
        // text the model needs.
        assert_eq!(c.to_text(), "summary line");
    }
}
