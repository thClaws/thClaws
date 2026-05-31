//! Thai word segmentation for KMS BM25 indexing (dev-plan/36 Tier 1).
//!
//! Thai writes without whitespace word boundaries; the default
//! whitespace tokenizer indexes whole paragraphs as single tokens,
//! silently breaking BM25 search on Thai content. This module
//! provides a `newmm`-style maximum-matching segmenter backed by a
//! DAFSA dictionary (BurntSushi's `fst` crate) embedded via
//! `include_bytes!` at compile time.
//!
//! ## Crate boundary
//!
//! Tightly coupled to thClaws's tantivy `Tokenizer` integration
//! (`crate::kms_search_index`), so this lives inside `thclaws-core`
//! rather than as a separate crate. The public API is intentionally
//! small — `segment(&str) -> Vec<&str>` + `Segmenter::add_word` for
//! per-project dict supplements — to keep the surface easy to
//! replace with an external segmenter (lindera, icu_segmenter) if
//! the in-house path becomes a maintenance burden later.
//!
//! ## Feature gating
//!
//! Behind `kms_search_index` per dev-plan/36 D3 (opt-in forever).
//! Without the feature, the module is not compiled and KMS search
//! stays on the regex-only path.

#![cfg(feature = "kms_search_index")]

pub mod dict;
pub mod newmm;

pub use newmm::{segment, Segmenter};
