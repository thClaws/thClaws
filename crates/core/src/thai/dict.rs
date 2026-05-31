//! Thai dictionary — DAFSA-backed via [`fst::Set`] for compact
//! storage + O(prefix-length) lookup.
//!
//! The compiled blob `words_th.fst` is embedded at compile time via
//! `include_bytes!`; `fst::Set::new` is zero-copy, so first-lookup
//! latency is negligible. The blob is produced offline from
//! `words_th.txt` via `scripts/build_thai_dict.rs` (committed to the
//! repo so casual builds don't need to rebuild it).
//!
//! ## Source list
//!
//! Wiktionary's Thai-language category export, sanitised + deduped
//! (~50k entries, CC-BY-SA 4.0; attribution in `NOTICE.txt`). See
//! `scripts/build_thai_dict/README.md` for the acquisition pipeline.
//!
//! ## Per-project supplements
//!
//! [`Segmenter::add_word`] grows an in-memory overlay on top of the
//! embedded FST so users can supply project-specific vocabulary
//! (technical terms, brand names) via `<kms_root>/extra_words_th.txt`
//! without rebuilding the binary blob. Lookup checks the overlay
//! first, then the FST.

use fst::Set;
use std::collections::HashSet;

/// Compiled DAFSA blob produced offline by
/// `scripts/build_thai_dict.rs` from `words_th.txt`. Empty
/// placeholder until Tier 1.B lands the bootstrap dict (~100-200
/// hand-authored words); replaced with the full Wiktionary build
/// once Tier 1.E ships the acquisition pipeline.
const WORDS_FST: &[u8] = include_bytes!("words_th.fst");

/// Loaded Thai dictionary. Wraps a zero-copy [`fst::Set`] over the
/// embedded blob plus an optional in-memory overlay of additional
/// words. Cheap to clone (the overlay is the only owned data; the
/// FST is a borrow of `&'static [u8]`).
pub struct ThaiDict {
    fst: Set<&'static [u8]>,
    overlay: HashSet<String>,
}

impl ThaiDict {
    /// Construct a dictionary backed by the embedded FST blob with an
    /// empty overlay. Infallible — the blob is validated at compile
    /// time by `scripts/build_thai_dict.rs`.
    pub fn embedded() -> Self {
        Self {
            fst: Set::new(WORDS_FST).expect(
                "words_th.fst failed to parse — \
                 regenerate via scripts/build_thai_dict.rs",
            ),
            overlay: HashSet::new(),
        }
    }

    /// Add a project-specific word to the in-memory overlay. Useful
    /// for runtime supplements (technical terms, brand names,
    /// per-project vocabulary). Words added here are matched the
    /// same way as embedded words.
    pub fn add_word(&mut self, word: impl Into<String>) {
        self.overlay.insert(word.into());
    }

    /// Bulk-load words from an iterator. Idempotent; duplicates are
    /// absorbed by the HashSet.
    pub fn add_words<I, S>(&mut self, words: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for w in words {
            self.overlay.insert(w.into());
        }
    }

    /// True when `s` is a word in the dictionary (overlay or FST).
    /// O(|s|) — the FST traversal is linear in the byte length of `s`.
    pub fn contains(&self, s: &str) -> bool {
        self.overlay.contains(s) || self.fst.contains(s)
    }

    /// Total entry count. The FST length is exact; overlay may
    /// duplicate FST entries (HashSet doesn't deduplicate against
    /// the FST), so this is an upper bound rather than a unique
    /// count. Used for diagnostics + the `[dict: N words]` startup
    /// log, not for any correctness check.
    pub fn approx_len(&self) -> usize {
        self.fst.len() + self.overlay.len()
    }
}

impl Default for ThaiDict {
    fn default() -> Self {
        Self::embedded()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_dict_loads_without_panic() {
        // Smoke: just instantiate. The real content tests live in
        // newmm::tests where actual segmentation is exercised.
        let dict = ThaiDict::embedded();
        // Length is non-negative; even a placeholder blob with 0
        // entries should report 0, not panic.
        let _ = dict.approx_len();
    }

    #[test]
    fn overlay_words_are_findable() {
        let mut dict = ThaiDict::embedded();
        dict.add_word("custom-brand-term");
        assert!(dict.contains("custom-brand-term"));
        assert!(!dict.contains("not-added"));
    }
}
