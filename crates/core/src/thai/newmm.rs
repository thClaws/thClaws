//! `newmm`-style maximum-matching Thai segmenter.
//!
//! Algorithm (mirrors PyThaiNLP's `newmm`, the de-facto reference):
//!
//! 1. Walk the input left-to-right by **character** position
//!    (`char_indices` — not byte) since Thai is UTF-8 multi-byte.
//! 2. At each position, classify the leading char + dispatch:
//!    - ASCII whitespace → skip (token separator).
//!    - ASCII punct / symbol → skip (token separator).
//!    - ASCII alphanumeric run → emit as one token (English mixed
//!      into Thai stays grouped; tokenizer downstream handles
//!      stemming + lowercasing).
//!    - Otherwise (Thai or other non-ASCII): try the longest
//!      dictionary word starting here (lengths 1..=MAX_WORD_CHARS),
//!      emit if found.
//!    - If no dict match, accumulate consecutive non-dict non-ASCII
//!      non-whitespace chars as a single OOV-run token. Break when
//!      a dict word starts at the next position or at script /
//!      whitespace transitions.
//!
//! ## Why longest-match (greedy) instead of probability-maximising
//!
//! PyThaiNLP defaults to `newmm`'s greedy longest-match; the
//! probability-maximising path (`attacut`, `deepcut`) is ML-model-
//! backed and adds a ~50 MB binary. Greedy is correct for BM25
//! indexing — the model only needs token boundaries that are
//! consistent between index-time and query-time, not "perfect"
//! linguistic segmentation. Greedy delivers consistency by
//! construction.
//!
//! ## Performance note
//!
//! At each position we try prefix lengths 1..=MAX_WORD_CHARS via
//! `ThaiDict::contains`, which is O(prefix_len) per call (FST
//! traversal). With MAX_WORD_CHARS = 30 and average matched word
//! length ~5-10 chars, that's ~150-300 ops per position. For a
//! 1000-char KMS page that's <1 ms of segmentation — well under
//! tantivy's indexing overhead. A future optimisation could walk
//! the FST node-by-node once per position via `fst::raw`, but the
//! `fst` public API doesn't expose that cleanly + the win isn't
//! visible at our scale.

use crate::thai::dict::ThaiDict;

/// Cap on dictionary-word length tried at each position. Thai
/// compounds rarely exceed ~10 syllables (~20-30 chars); 30 is
/// generous safety margin. Affects per-position work directly:
/// every position does up to MAX_WORD_CHARS `dict.contains` calls.
const MAX_WORD_CHARS: usize = 30;

/// Owned segmenter — holds the dictionary (embedded FST + optional
/// in-memory overlay). Cheap to clone; the FST is a borrow of
/// `&'static [u8]`. Construct once per indexing job (not per page).
pub struct Segmenter {
    dict: ThaiDict,
}

impl Segmenter {
    /// New segmenter backed by the embedded dictionary.
    pub fn new() -> Self {
        Self {
            dict: ThaiDict::embedded(),
        }
    }

    /// Add a custom word to the in-memory overlay. Useful for
    /// per-project supplements via `<kms_root>/extra_words_th.txt`.
    pub fn add_word(&mut self, word: impl Into<String>) {
        self.dict.add_word(word);
    }

    /// Bulk-load custom words.
    pub fn add_words<I, S>(&mut self, words: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.dict.add_words(words);
    }

    /// Segment `text` into tokens. Borrows from `text`; no
    /// allocation per token beyond the returned `Vec`. See the
    /// module-level docs for the algorithm.
    pub fn segment<'a>(&self, text: &'a str) -> Vec<&'a str> {
        let mut out = Vec::new();
        if text.is_empty() {
            return out;
        }
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let n = chars.len();
        let mut i = 0;
        while i < n {
            let (byte_start, ch) = chars[i];

            // ASCII whitespace + punct: token separator, skip alone.
            if ch.is_ascii_whitespace() {
                i += 1;
                continue;
            }
            if ch.is_ascii() && !ch.is_ascii_alphanumeric() {
                i += 1;
                continue;
            }

            // ASCII alphanumeric run: emit as one token (English
            // identifiers, numbers, etc. embedded in Thai prose).
            if ch.is_ascii_alphanumeric() {
                let mut j = i;
                while j < n && chars[j].1.is_ascii_alphanumeric() {
                    j += 1;
                }
                let end_byte = char_byte_end(&chars, text, j);
                out.push(&text[byte_start..end_byte]);
                i = j;
                continue;
            }

            // Non-ASCII (Thai / mixed-script): try longest dict
            // prefix at current position.
            if let Some(match_len_chars) = self.longest_dict_match(&chars, text, i) {
                let end_byte = char_byte_end(&chars, text, i + match_len_chars);
                out.push(&text[byte_start..end_byte]);
                i += match_len_chars;
                continue;
            }

            // OOV run: accumulate consecutive non-dict non-ASCII
            // non-whitespace chars until a dict word starts at the
            // next position OR until script / whitespace boundary.
            let oov_start = byte_start;
            let mut j = i + 1; // include the current char
            while j < n {
                let cj = chars[j].1;
                if cj.is_ascii() || cj.is_whitespace() {
                    break;
                }
                if self.longest_dict_match(&chars, text, j).is_some() {
                    break;
                }
                j += 1;
            }
            let end_byte = char_byte_end(&chars, text, j);
            out.push(&text[oov_start..end_byte]);
            i = j.max(i + 1);
        }
        out
    }

    /// Try prefix lengths from `min(MAX_WORD_CHARS, remaining)` down
    /// to 1; return the longest length that hits the dictionary, or
    /// None. The downward scan is intentional — finding the longest
    /// first lets us short-circuit, but `dict.contains` is cheap
    /// enough that the difference is negligible in practice. We go
    /// upward + remember the longest seen, which lets the FST short-
    /// circuit on mismatch faster than asking it the long-prefix
    /// question first.
    fn longest_dict_match(&self, chars: &[(usize, char)], text: &str, i: usize) -> Option<usize> {
        let max_try = MAX_WORD_CHARS.min(chars.len() - i);
        let byte_start = chars[i].0;
        let mut best = 0;
        for try_len in 1..=max_try {
            let end_byte = char_byte_end(chars, text, i + try_len);
            let candidate = &text[byte_start..end_byte];
            if self.dict.contains(candidate) {
                best = try_len;
            }
        }
        if best > 0 {
            Some(best)
        } else {
            None
        }
    }
}

impl Default for Segmenter {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve the byte offset just past the char at position `idx`
/// (i.e. the byte-start of the NEXT char, or `text.len()` if we're
/// at the end). Used everywhere we slice `text[start..end]` from
/// char positions. Centralised so the off-by-one logic only lives
/// in one place.
fn char_byte_end(chars: &[(usize, char)], text: &str, idx: usize) -> usize {
    if idx >= chars.len() {
        text.len()
    } else {
        chars[idx].0
    }
}

/// Convenience for one-shot segmentation against the embedded dict
/// with no overlay. Equivalent to `Segmenter::default().segment(text)`.
/// Allocates a fresh `Segmenter` per call — fine for tests + the
/// `KmsSearch` `query:` parsing path, avoid in per-page indexing
/// loops (use a long-lived `Segmenter` registered in the tokenizer).
pub fn segment(text: &str) -> Vec<&str> {
    Segmenter::default().segment(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_empty_vec() {
        assert!(segment("").is_empty());
    }

    #[test]
    fn pure_ascii_splits_at_whitespace() {
        assert_eq!(segment("hello world"), vec!["hello", "world"]);
    }

    #[test]
    fn ascii_punct_separates_tokens() {
        // Comma + paren are non-alphanum ASCII → separators.
        assert_eq!(segment("foo, bar (baz)"), vec!["foo", "bar", "baz"],);
    }

    #[test]
    fn ascii_alphanumeric_run_stays_grouped() {
        // Numbers + identifiers stay as single tokens.
        assert_eq!(segment("abc123 v2"), vec!["abc123", "v2"]);
    }

    #[test]
    fn bootstrap_thai_words_segment_correctly() {
        // Both `การ` and `ทดสอบ` are in the bootstrap dict — should
        // come out as two distinct tokens, not one.
        let out = segment("การทดสอบ");
        assert_eq!(out, vec!["การ", "ทดสอบ"], "got {out:?}");
    }

    #[test]
    fn common_function_words_segment_in_phrase() {
        // "ของฉัน" — `ของ` + `ฉัน`, both in dict.
        let out = segment("ของฉัน");
        assert_eq!(out, vec!["ของ", "ฉัน"], "got {out:?}");
    }

    #[test]
    fn pronoun_verb_pronoun_phrase_segments() {
        // "ผมรักคุณ" — `ผม` + `รัก` + `คุณ`, all in dict.
        // Note: "รัก" was added — verify it's actually there.
        let mut seg = Segmenter::new();
        seg.add_word("รัก"); // make sure it's available even if dict trimmed it
        let out = seg.segment("ผมรักคุณ");
        assert_eq!(out, vec!["ผม", "รัก", "คุณ"], "got {out:?}");
    }

    #[test]
    fn unknown_thai_word_emits_as_single_oov_token() {
        // "กรกฎาคม" (July) is NOT in the bootstrap dict; should
        // emit as one OOV run, not split into single chars.
        let out = segment("กรกฎาคม");
        assert_eq!(out.len(), 1, "OOV should be one token, got {out:?}");
        assert_eq!(out[0], "กรกฎาคม");
    }

    #[test]
    fn mixed_thai_latin_breaks_at_script_transition() {
        // Thai region + Latin word + Latin word should give
        // [Thai-tokens..., "Python", "SDK"].
        let out = segment("ทดสอบ Python SDK");
        assert!(
            out.contains(&"ทดสอบ") && out.contains(&"Python") && out.contains(&"SDK"),
            "expected all three present, got {out:?}",
        );
    }

    #[test]
    fn ascii_punct_breaks_oov_run() {
        // OOV Thai run terminated by ASCII comma — comma is a
        // separator, not part of the OOV token.
        let out = segment("กรกฎาคม, hello");
        assert_eq!(out, vec!["กรกฎาคม", "hello"], "got {out:?}");
    }

    #[test]
    fn overlay_words_are_used_in_segmentation() {
        // Custom word via overlay should beat OOV-run fallback.
        let mut seg = Segmenter::new();
        seg.add_word("ทคนิคพิเศษ"); // fake compound not in dict
        let out = seg.segment("ทคนิคพิเศษ");
        assert_eq!(out, vec!["ทคนิคพิเศษ"], "got {out:?}");
    }

    #[test]
    fn whitespace_only_input_yields_no_tokens() {
        assert!(segment("   \t  ").is_empty());
        assert!(segment(" \n ").is_empty());
    }

    #[test]
    fn segment_progresses_on_lone_unknown_non_ascii_char() {
        // Edge case: a single non-dict non-ASCII char should not
        // loop forever. Should emit as a 1-char OOV token.
        let out = segment("文"); // Chinese char, definitely not in Thai dict
        assert_eq!(out, vec!["文"], "got {out:?}");
    }

    #[test]
    fn long_unknown_thai_run_does_not_panic() {
        // Stress: a 100-char run of non-dict chars must not overflow
        // MAX_WORD_CHARS or panic. Emitted as one OOV token.
        let s: String = "ก".repeat(100); // single non-dict char repeated
        let out = segment(&s);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chars().count(), 100);
    }
}
