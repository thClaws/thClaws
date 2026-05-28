//! Format thClaws output for Facebook Messenger.
//!
//! Messenger rendering constraints (dev-plan/31):
//! - A single text message is capped at **2 000 characters** (vs
//!   LINE's 5 000 and Telegram's 4 096). Long replies are split into
//!   multiple Send API calls rather than truncated.
//! - No markdown / code-fence rendering — fences show as literal
//!   text. ANSI escapes must be stripped (same as LINE).
//! - We send the **final** assistant text only; intermediate
//!   tool-call narration is stripped.
//!
//! ANSI + tool-narration stripping is shared with the LINE adapter
//! ([`crate::line::filter::clean_for_stream`]) so the two surfaces
//! stay consistent and there's one place to fix narration leaks.

use crate::line::filter::clean_for_stream;

/// Hard ceiling — Messenger rejects single text messages above this.
pub const MESSENGER_MAX_CHARS: usize = 2_000;

/// Chunk just below the ceiling so a "(1/3)" style prefix (added by a
/// future Tier) always fits. 1 900 leaves headroom under 2 000.
pub const CHUNK_AT: usize = 1_900;

/// Clean (ANSI + tool-narration strip) then split into Messenger-sized
/// chunks. Prefers to break at the last newline within the window so
/// chunks don't sever mid-line; falls back to a hard char-boundary cut
/// for a single unbroken run longer than `CHUNK_AT`. UTF-8 safe.
///
/// Returns at least one chunk for non-empty input; an empty / all-
/// whitespace body yields an empty vec so the caller skips the send.
pub fn chunks_for_messenger(body: &str) -> Vec<String> {
    let cleaned = clean_for_stream(body);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut rest: &str = trimmed;
    while !rest.is_empty() {
        if rest.chars().count() <= CHUNK_AT {
            chunks.push(rest.to_string());
            break;
        }
        // Byte index of the CHUNK_AT-th char boundary.
        let split_byte = rest
            .char_indices()
            .nth(CHUNK_AT)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let window = &rest[..split_byte];
        // Prefer the last newline in the window; otherwise hard-cut at
        // the char boundary so we never panic on a multi-byte char.
        let cut = window.rfind('\n').map(|i| i + 1).unwrap_or(split_byte);
        chunks.push(rest[..cut].trim_end().to_string());
        rest = rest[cut..].trim_start();
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(chunks_for_messenger("hello"), vec!["hello".to_string()]);
    }

    #[test]
    fn empty_or_whitespace_yields_no_chunks() {
        assert!(chunks_for_messenger("").is_empty());
        assert!(chunks_for_messenger("   \n\n ").is_empty());
    }

    #[test]
    fn strips_ansi_and_tool_narration() {
        let input = "\x1b[2m[tool: Read /tmp/x]\x1b[0m\n⏺ Read(/x)\nThe answer is 42.";
        assert_eq!(
            chunks_for_messenger(input),
            vec!["The answer is 42.".to_string()]
        );
    }

    #[test]
    fn long_text_splits_into_multiple_chunks() {
        let body = "x".repeat(5_000);
        let chunks = chunks_for_messenger(&body);
        assert!(chunks.len() >= 3);
        for c in &chunks {
            assert!(c.chars().count() <= MESSENGER_MAX_CHARS);
        }
        // No characters lost across the split.
        let total: usize = chunks.iter().map(|c| c.chars().count()).sum();
        assert_eq!(total, 5_000);
    }

    #[test]
    fn prefers_newline_break() {
        // First line is under CHUNK_AT; build a body that forces a
        // split and verify the break lands on the newline, not mid-line.
        let line_a = "a".repeat(1_800);
        let line_b = "b".repeat(1_800);
        let body = format!("{line_a}\n{line_b}");
        let chunks = chunks_for_messenger(&body);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], line_a);
        assert_eq!(chunks[1], line_b);
    }

    #[test]
    fn chunking_is_char_boundary_safe_for_thai() {
        // Thai chars are 3 bytes UTF-8; a byte-based cut would panic.
        let body = "ก".repeat(5_000);
        let chunks = chunks_for_messenger(&body);
        let total: usize = chunks.iter().map(|c| c.chars().count()).sum();
        assert_eq!(total, 5_000);
        for c in &chunks {
            assert!(c.chars().count() <= MESSENGER_MAX_CHARS);
        }
    }
}
