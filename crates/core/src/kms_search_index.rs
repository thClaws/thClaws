//! KMS BM25 search index (dev-plan/36 Tier 1).
//!
//! Tantivy index living at `<kms_root>/.index/`, rebuilt
//! incrementally on every KMS write. The model-facing `KmsSearch`
//! tool (Tier 2) routes `query:` requests through this; the
//! `pattern:` regex path stays unchanged + byte-identical.
//!
//! ## Layout on disk
//!
//! ```text
//! <kms_root>/
//! ├── pages/                  ← source of truth (markdown)
//! ├── .index/                 ← tantivy index dir (THIS module owns)
//! │   ├── meta.json           ← tantivy's own metadata
//! │   ├── *.fast / *.idx      ← tantivy segments
//! │   └── manifest.json       ← our metadata: index_version,
//! │                             built_at, last_full_rebuild_at
//! └── .index/vectors/         ← reserved for future dev-plan
//!                                semantic-search work; this Tier 1
//!                                NEVER touches it
//! ```
//!
//! ## Tokenizer
//!
//! Custom [`ThaiOrEnglishTokenizer`] segments Thai char regions via
//! [`crate::thai::segment`] and yields ASCII alphanumeric runs as
//! single tokens. The shared `LowerCaser` filter applies after
//! tokenization. English stemming is intentionally NOT in the
//! pipeline for v1 — applying en-stemmer to Thai tokens mangles
//! them; per-token language detection costs more than it's worth
//! for BM25 indexing.
//!
//! ## Feature gating
//!
//! Behind `kms_search_index` per dev-plan/36 D3 (opt-in forever).

#![cfg(feature = "kms_search_index")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use tantivy::schema::{Field, Schema, FAST, INDEXED, STORED, STRING};
use tantivy::tokenizer::{LowerCaser, TextAnalyzer, Token, TokenStream, Tokenizer};
use tantivy::{doc, Index, IndexWriter, Term};

/// Tantivy memory budget for the IndexWriter — 50 MB is plenty for
/// KMS-scale writes (single page per commit). Lower bound is 15 MB
/// per tantivy's own guidance.
const WRITER_MEMORY_BUDGET: usize = 50_000_000;

/// Custom tokenizer name registered with the index. Must match
/// what we set on schema fields via `set_tokenizer(...)`.
const TOKENIZER_NAME: &str = "thai_en";

/// Bumped on any schema or tokenizer change so existing on-disk
/// indexes auto-rebuild on next open (Tier 3 stale-manifest
/// detection). Start at 1; increment when we change the schema
/// or tokenizer in a non-backward-compatible way.
pub const INDEX_VERSION: u32 = 1;

/// What changed on a KMS page so the indexer knows whether to
/// upsert (re-add document) or delete (remove document by page
/// name). [`crate::kms`] write functions invoke [`on_page_mutated`]
/// after their on-disk operation succeeds.
#[derive(Debug, Clone)]
pub enum Op {
    Upsert,
    Delete,
}

/// Errors the indexer can surface. Distinct from [`crate::error::Error`]
/// so the calling KMS write path can choose to swallow (recoverable —
/// stale manifest, lock contention) vs propagate (catastrophic — disk
/// full, schema corruption).
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("index io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tantivy: {0}")]
    Tantivy(String),
    #[error("kms root missing or unreadable: {0}")]
    KmsRoot(String),
}

impl From<tantivy::TantivyError> for IndexError {
    fn from(e: tantivy::TantivyError) -> Self {
        IndexError::Tantivy(e.to_string())
    }
}

/// Compiled schema + field handles. Kept as a struct so callers
/// don't have to look up fields by name on every operation.
struct Fields {
    page: Field,
    title: Field,
    topic: Field,
    tags: Field,
    category: Field,
    sources: Field,
    body: Field,
    #[allow(dead_code)] // Tier 2 will use this for recency boost / range filter.
    updated: Field,
}

fn build_schema() -> (Schema, Fields) {
    let mut sb = Schema::builder();
    // `page` is the document identity — STRING (raw, single-value)
    // so we can `delete_term(page)` exactly. Stored so search
    // results can cite it.
    let page = sb.add_text_field("page", STRING | STORED);
    // Title / topic / body use the shared text indexing pipeline
    // (custom tokenizer + lowercaser), so they all reference
    // TOKENIZER_NAME. Field boosts (title × 4, topic × 2, body × 1
    // per dev-plan/36) are applied at query time via QueryParser,
    // not in the schema.
    let text_opts = tantivy::schema::TextOptions::default()
        .set_indexing_options(
            tantivy::schema::TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER_NAME)
                .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    let title = sb.add_text_field("title", text_opts.clone());
    let topic = sb.add_text_field("topic", text_opts.clone());
    // Body is indexed but NOT stored — the page lives on disk, we
    // don't duplicate it. Snippet generation in Tier 2 re-reads
    // from disk when needed.
    let body = sb.add_text_field(
        "body",
        tantivy::schema::TextOptions::default().set_indexing_options(
            tantivy::schema::TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER_NAME)
                .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions),
        ),
    );
    // Tags + category + sources are raw (STRING) for exact-match
    // filtering. tags + sources may have multiple values per page;
    // tantivy handles multi-value via add_text repeatedly on the
    // same field.
    let tags = sb.add_text_field("tags", STRING | STORED);
    let category = sb.add_text_field("category", STRING | STORED);
    let sources = sb.add_text_field("sources", STRING);
    // Updated as i64 unix-seconds for future range queries +
    // recency boost. FAST so range collectors can hit it without
    // re-reading docs.
    let updated = sb.add_i64_field("updated", INDEXED | STORED | FAST);

    let schema = sb.build();
    (
        schema,
        Fields {
            page,
            title,
            topic,
            tags,
            category,
            sources,
            body,
            updated,
        },
    )
}

/// Wraps a tantivy `Index` rooted at `<kms_root>/.index/`.
/// Construction either opens an existing index or creates a fresh
/// one + registers the tokenizer.
///
/// One `SearchIndex` per `(kms_root)` should suffice for the
/// process lifetime — tantivy `Index` is `Send + Sync` and its
/// internal lock contention is sub-microsecond per write.
pub struct SearchIndex {
    /// `<kms_root>` — the KMS this index belongs to. Kept so
    /// `search()` can re-read page bodies from `pages/<name>.md`
    /// for snippet generation (body field is indexed but NOT
    /// stored, so we can't ask tantivy to return it).
    kms_root: PathBuf,
    index: Index,
    fields: Fields,
    // Tantivy requires a long-lived IndexWriter for fast incremental
    // updates. Wrapped in a Mutex so concurrent on_page_mutated calls
    // don't race. Per-mutation commits are <10 ms — Mutex contention
    // is not a concern at KMS write rates.
    writer: Mutex<IndexWriter>,
}

/// One scored hit from a BM25 query. Returned by
/// [`SearchIndex::search`] in descending score order (limit-capped).
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// BM25 score with the per-field boosts applied. Higher = better.
    pub score: f32,
    /// Page name (stem, no `.md` extension). Use this with
    /// `KmsRead(page: …)` to fetch the full content.
    pub page: String,
    /// Page's frontmatter `title:` if present.
    pub title: Option<String>,
    /// Page's frontmatter `topic:` if present.
    pub topic: Option<String>,
    /// First ~200 chars of the page body for at-a-glance context.
    /// Tier 2 keeps this naive (just a prefix); Tier 4 may add
    /// query-term-aware highlighted snippets via tantivy's
    /// `SnippetGenerator` (requires body to be stored, which would
    /// double index disk usage; deferred).
    pub snippet_preview: String,
}

impl SearchIndex {
    /// Open an existing index at `<kms_root>/.index/` or create a
    /// fresh one. Registers the custom `thai_en` tokenizer. Cheap to
    /// call on already-existing indexes (~ms range).
    pub fn open_or_create(kms_root: &Path) -> Result<Self, IndexError> {
        let index_dir = kms_root.join(".index");
        std::fs::create_dir_all(&index_dir)?;
        let (schema, fields) = build_schema();

        let index = if index_dir.join("meta.json").exists() {
            Index::open_in_dir(&index_dir)?
        } else {
            Index::create_in_dir(&index_dir, schema)?
        };

        // Custom tokenizer must be registered before any write or
        // read — the tokenizer is keyed by name in the schema, and
        // tantivy resolves it from the manager at open time.
        index.tokenizers().register(
            TOKENIZER_NAME,
            TextAnalyzer::builder(ThaiOrEnglishTokenizer::new())
                .filter(LowerCaser)
                .build(),
        );

        let writer = index.writer(WRITER_MEMORY_BUDGET)?;
        Ok(Self {
            kms_root: kms_root.to_path_buf(),
            index,
            fields,
            writer: Mutex::new(writer),
        })
    }

    /// Upsert a page by name. Deletes any existing document with
    /// the same `page` term first (so multiple calls don't duplicate)
    /// then adds the new one. Commits before returning.
    pub fn upsert_page(
        &self,
        page_name: &str,
        frontmatter: &std::collections::BTreeMap<String, String>,
        body: &str,
    ) -> Result<(), IndexError> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|e| IndexError::Tantivy(format!("writer mutex poisoned: {e}")))?;
        writer.delete_term(Term::from_field_text(self.fields.page, page_name));

        let title = frontmatter.get("title").map(String::as_str).unwrap_or("");
        let topic = frontmatter.get("topic").map(String::as_str).unwrap_or("");
        let category = frontmatter
            .get("category")
            .map(String::as_str)
            .unwrap_or("");
        let updated = current_unix_secs();

        // Build doc. Tags + sources may be comma-separated strings
        // in frontmatter; split + add multi-value.
        let mut document = doc!(
            self.fields.page => page_name,
            self.fields.title => title,
            self.fields.topic => topic,
            self.fields.category => category,
            self.fields.body => body,
            self.fields.updated => updated,
        );
        for tag in split_csv(frontmatter.get("tags").map(String::as_str).unwrap_or("")) {
            document.add_text(self.fields.tags, &tag);
        }
        for src in split_csv(frontmatter.get("sources").map(String::as_str).unwrap_or("")) {
            document.add_text(self.fields.sources, &src);
        }
        writer.add_document(document)?;
        writer.commit()?;
        Ok(())
    }

    /// Delete a page by name. Commits before returning.
    pub fn delete_page(&self, page_name: &str) -> Result<(), IndexError> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|e| IndexError::Tantivy(format!("writer mutex poisoned: {e}")))?;
        writer.delete_term(Term::from_field_text(self.fields.page, page_name));
        writer.commit()?;
        Ok(())
    }

    /// Document count (post-commit). For diagnostics + tests.
    pub fn num_docs(&self) -> Result<u64, IndexError> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        Ok(searcher.num_docs())
    }

    /// BM25-ranked search across title (boost ×4), topic (×2), and
    /// body (×1). Optionally narrowed by tags (any-of semantics —
    /// page matches if it has ANY of the requested tags) and/or
    /// category (exact match). Snippet preview is the first ~200
    /// chars of the page body, re-read from disk per hit.
    ///
    /// Returns up to `limit` hits in descending score order. An
    /// empty `query_str` returns no hits (don't accidentally
    /// surface the entire KMS to the model).
    pub fn search(
        &self,
        query_str: &str,
        tags_filter: &[String],
        category_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, IndexError> {
        use tantivy::collector::TopDocs;
        use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
        use tantivy::schema::IndexRecordOption;

        let q = query_str.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 50);

        let reader = self.index.reader()?;
        let searcher = reader.searcher();

        // QueryParser over title + topic + body with per-field boosts.
        // Field boosts (title × 4, topic × 2, body × 1 per dev-plan/36)
        // are configured here, not in the schema, so we can revisit
        // without rebuilding indexes.
        let mut parser = QueryParser::for_index(
            &self.index,
            vec![self.fields.title, self.fields.topic, self.fields.body],
        );
        parser.set_field_boost(self.fields.title, 4.0);
        parser.set_field_boost(self.fields.topic, 2.0);
        // body is the default 1.0; no call needed.

        let base_query = parser
            .parse_query(q)
            .map_err(|e| IndexError::Tantivy(format!("query parse: {e}")))?;

        // Compose with filters via BooleanQuery::Must. Tantivy
        // requires concrete Vec<(Occur, Box<dyn Query>)>; build
        // incrementally so we only allocate when filters are set.
        let filters_active = !tags_filter.is_empty() || category_filter.is_some();
        let final_query: Box<dyn Query> = if filters_active {
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            clauses.push((Occur::Must, base_query));
            // Tags: any-of (OR semantics). Build a nested BooleanQuery
            // of Should clauses so the outer Must demands "at least
            // one tag matches", and tantivy's TopDocs scoring stays
            // pure-BM25 on the main query (filters don't influence
            // ranking — they just narrow the candidate set).
            if !tags_filter.is_empty() {
                let tag_clauses: Vec<(Occur, Box<dyn Query>)> = tags_filter
                    .iter()
                    .map(|t| {
                        let tq = TermQuery::new(
                            tantivy::Term::from_field_text(self.fields.tags, t.trim()),
                            IndexRecordOption::Basic,
                        );
                        (Occur::Should, Box::new(tq) as Box<dyn Query>)
                    })
                    .collect();
                clauses.push((Occur::Must, Box::new(BooleanQuery::new(tag_clauses))));
            }
            if let Some(c) = category_filter {
                let cq = TermQuery::new(
                    tantivy::Term::from_field_text(self.fields.category, c.trim()),
                    IndexRecordOption::Basic,
                );
                clauses.push((Occur::Must, Box::new(cq)));
            }
            Box::new(BooleanQuery::new(clauses))
        } else {
            base_query
        };

        let top_docs = searcher
            .search(&*final_query, &TopDocs::with_limit(limit))
            .map_err(|e| IndexError::Tantivy(format!("search: {e}")))?;

        let mut hits = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let retrieved: tantivy::TantivyDocument = searcher
                .doc(doc_address)
                .map_err(|e| IndexError::Tantivy(format!("doc fetch: {e}")))?;
            let page = first_text(&retrieved, self.fields.page).unwrap_or_default();
            let title = first_text(&retrieved, self.fields.title);
            let topic = first_text(&retrieved, self.fields.topic);
            let snippet_preview = self.read_snippet_preview(&page);
            hits.push(SearchHit {
                score,
                page,
                title: title.filter(|s| !s.is_empty()),
                topic: topic.filter(|s| !s.is_empty()),
                snippet_preview,
            });
        }
        Ok(hits)
    }

    /// Read up to ~200 chars from `<kms_root>/pages/<page>.md`
    /// (frontmatter stripped) for the snippet preview. Returns an
    /// empty string on I/O error — snippet is best-effort, the
    /// page+score are the load-bearing fields.
    fn read_snippet_preview(&self, page_stem: &str) -> String {
        let path = self.kms_root.join("pages").join(format!("{page_stem}.md"));
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return String::new();
        };
        let (_fm, body) = crate::kms::parse_frontmatter(&raw);
        // Take the first ~200 chars, clamped to a char boundary +
        // single-line. Multi-line / Markdown rendering belongs in
        // the calling tool's formatting code, not here.
        let oneline: String = body
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .chars()
            .take(200)
            .collect();
        oneline
    }
}

/// Extract the first text value for `field` from a retrieved doc,
/// or None if the field has no value (or wasn't stored).
fn first_text(doc: &tantivy::TantivyDocument, field: Field) -> Option<String> {
    use tantivy::schema::Value;
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Split a comma-or-whitespace-separated frontmatter value into
/// individual values. Trims each. Used for `tags:` + `sources:`.
fn split_csv(s: &str) -> Vec<String> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn current_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Process-wide registry of opened `SearchIndex` instances, one per
/// `kms_root`. Required because tantivy's `IndexWriter` holds a
/// directory-level lock — concurrent `SearchIndex::open_or_create`
/// calls on the same root would collide with `LockBusy`. The
/// registry hands out `Arc<SearchIndex>` so callers share one
/// long-lived writer per KMS; the writer's internal `Mutex`
/// serialises concurrent upserts cleanly.
///
/// Lifetime: process-lifetime. Indexes stay open until process exit;
/// the 50 MB writer-memory budget per KMS is acceptable since a
/// single process rarely interacts with more than a handful of KMSes.
fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<SearchIndex>>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, Arc<SearchIndex>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Get the cached `SearchIndex` for `kms_root`, creating + caching
/// it on first call. Subsequent calls return the same `Arc` (cheap
/// clone — internal writer Mutex serialises real work).
///
/// Tests that need to drop the index between calls (e.g. to verify
/// full_rebuild's `remove_dir_all` works) can call
/// [`drop_cached`] explicitly.
pub fn get_or_open(kms_root: &Path) -> Result<Arc<SearchIndex>, IndexError> {
    let canonical = kms_root
        .canonicalize()
        .unwrap_or_else(|_| kms_root.to_path_buf());
    let mut reg = registry()
        .lock()
        .map_err(|e| IndexError::Tantivy(format!("registry mutex poisoned: {e}")))?;
    if let Some(idx) = reg.get(&canonical) {
        return Ok(idx.clone());
    }
    let idx = Arc::new(SearchIndex::open_or_create(&canonical)?);
    reg.insert(canonical, idx.clone());
    Ok(idx)
}

/// Drop the cached `SearchIndex` for `kms_root` if any. Used by
/// `full_rebuild` (which deletes `.index/` and needs to reopen) and
/// by tests that exercise re-open semantics. Idempotent; no-op when
/// no cached entry exists.
pub fn drop_cached(kms_root: &Path) {
    let canonical = kms_root
        .canonicalize()
        .unwrap_or_else(|_| kms_root.to_path_buf());
    if let Ok(mut reg) = registry().lock() {
        reg.remove(&canonical);
    }
}

/// Notify the indexer that a page in `kms_root` mutated. Opens (or
/// re-uses the cached) `<kms_root>/.index/` and applies `op`.
/// Errors are logged but never propagated — a write that succeeded
/// shouldn't roll back because the index happened to be stale.
///
/// `page_name` is the page's stem (filename without `.md`),
/// matching what [`crate::kms::Kms::list_pages`] returns.
pub fn on_page_mutated(kms_root: &Path, page_name: &str, op: Op) {
    if let Err(e) = on_page_mutated_inner(kms_root, page_name, op) {
        eprintln!(
            "\x1b[33m[kms-search-index] {} page='{}' error: {}\x1b[0m",
            kms_root.display(),
            page_name,
            e
        );
    }
}

fn on_page_mutated_inner(kms_root: &Path, page_name: &str, op: Op) -> Result<(), IndexError> {
    let idx = get_or_open(kms_root)?;
    match op {
        Op::Delete => idx.delete_page(page_name),
        Op::Upsert => {
            let page_path = kms_root.join("pages").join(format!("{page_name}.md"));
            let raw = std::fs::read_to_string(&page_path)?;
            let (fm, body) = crate::kms::parse_frontmatter(&raw);
            idx.upsert_page(page_name, &fm, &body)
        }
    }
}

/// Tier 1.C: full rebuild from scratch. Drops `.index/`, walks
/// `<kms_root>/pages/`, re-indexes every page. Used by the
/// `/kms reindex` slash command in Tier 3 and by the auto-recovery
/// path on stale-manifest detection.
pub fn full_rebuild(kms_root: &Path) -> Result<usize, IndexError> {
    // Drop the cached SearchIndex (and its IndexWriter) first so the
    // directory lock is released before we wipe + recreate the dir.
    // Without this, `remove_dir_all` on the locked `.index/` either
    // fails on Windows or leaves a stale lock file behind.
    drop_cached(kms_root);
    let index_dir = kms_root.join(".index");
    if index_dir.exists() {
        std::fs::remove_dir_all(&index_dir)?;
    }
    let idx = get_or_open(kms_root)?;
    let pages_dir = kms_root.join("pages");
    if !pages_dir.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in std::fs::read_dir(&pages_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let raw = std::fs::read_to_string(&path)?;
        let (fm, body) = crate::kms::parse_frontmatter(&raw);
        idx.upsert_page(&stem, &fm, &body)?;
        count += 1;
    }
    Ok(count)
}

/// Custom tantivy `Tokenizer`. Splits the input by script (ASCII
/// vs non-ASCII) and yields tokens accordingly:
///
/// - ASCII whitespace + punct → separators (no token emitted)
/// - ASCII alphanumeric run → one token (English words / numbers)
/// - Non-ASCII run → segment via `crate::thai::segment` → one
///   token per resulting Thai word
#[derive(Clone)]
struct ThaiOrEnglishTokenizer;

impl ThaiOrEnglishTokenizer {
    fn new() -> Self {
        Self
    }
}

impl Tokenizer for ThaiOrEnglishTokenizer {
    type TokenStream<'a> = ThaiOrEnglishTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        // We pre-compute all tokens here rather than streaming —
        // simpler API surface, KMS pages are bounded (~10 KB
        // typically; tantivy's own en-tokenizer pre-computes too).
        let segmenter = crate::thai::Segmenter::new();
        let words = segmenter.segment(text);
        let mut tokens = Vec::with_capacity(words.len());
        let mut position = 0;
        for word in words {
            // Compute byte offsets back into `text` from the &str
            // slice — `Segmenter::segment` returns borrows of
            // `text` so str pointer arithmetic is valid here.
            let offset_from = unsafe { word.as_ptr().offset_from(text.as_ptr()) as usize };
            let offset_to = offset_from + word.len();
            tokens.push(Token {
                offset_from,
                offset_to,
                position,
                text: word.to_string(),
                position_length: 1,
            });
            position += 1;
        }
        ThaiOrEnglishTokenStream {
            tokens,
            cursor: 0,
            current: None,
        }
    }
}

struct ThaiOrEnglishTokenStream {
    tokens: Vec<Token>,
    cursor: usize,
    current: Option<Token>,
}

impl TokenStream for ThaiOrEnglishTokenStream {
    fn advance(&mut self) -> bool {
        if self.cursor < self.tokens.len() {
            self.current = Some(self.tokens[self.cursor].clone());
            self.cursor += 1;
            true
        } else {
            self.current = None;
            false
        }
    }
    fn token(&self) -> &Token {
        self.current
            .as_ref()
            .expect("token() called before advance()")
    }
    fn token_mut(&mut self) -> &mut Token {
        self.current
            .as_mut()
            .expect("token_mut() called before advance()")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn empty_fm() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    fn fm_with(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Tier 1.C round-trip via the registry: open a fresh index,
    /// upsert a page, confirm num_docs reflects it. Validates schema
    /// construction, tokenizer registration, write, and commit.
    /// Uses `get_or_open` (the production path) — tests that bypass
    /// it via `SearchIndex::open_or_create` directly would collide
    /// with the registry's cached writer.
    #[test]
    fn upsert_then_num_docs_returns_one() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("pages")).unwrap();
        let idx = get_or_open(tmp.path()).unwrap();
        idx.upsert_page(
            "test-page",
            &fm_with(&[("title", "Test"), ("topic", "demo")]),
            "body text here",
        )
        .unwrap();
        assert_eq!(idx.num_docs().unwrap(), 1);
        drop_cached(tmp.path());
    }

    #[test]
    fn upsert_same_page_twice_does_not_duplicate() {
        let tmp = tempdir().unwrap();
        let idx = get_or_open(tmp.path()).unwrap();
        idx.upsert_page("p", &empty_fm(), "first").unwrap();
        idx.upsert_page("p", &empty_fm(), "second").unwrap();
        assert_eq!(idx.num_docs().unwrap(), 1);
        drop_cached(tmp.path());
    }

    #[test]
    fn delete_removes_document() {
        let tmp = tempdir().unwrap();
        let idx = get_or_open(tmp.path()).unwrap();
        idx.upsert_page("p", &empty_fm(), "body").unwrap();
        assert_eq!(idx.num_docs().unwrap(), 1);
        idx.delete_page("p").unwrap();
        assert_eq!(idx.num_docs().unwrap(), 0);
        drop_cached(tmp.path());
    }

    /// Re-opening the same root after drop returns the same data
    /// (persistence) but a fresh `SearchIndex` instance.
    #[test]
    fn open_or_create_is_idempotent_across_drop() {
        let tmp = tempdir().unwrap();
        {
            let idx = get_or_open(tmp.path()).unwrap();
            idx.upsert_page("p", &empty_fm(), "body").unwrap();
            drop_cached(tmp.path()); // releases the directory lock
        }
        let idx = get_or_open(tmp.path()).unwrap();
        assert_eq!(idx.num_docs().unwrap(), 1);
        drop_cached(tmp.path());
    }

    /// full_rebuild walks pages/, indexes each .md, returns count.
    /// Implicitly tests that full_rebuild correctly drops the cache
    /// before deleting the on-disk index dir.
    #[test]
    fn full_rebuild_indexes_all_pages_under_root() {
        let tmp = tempdir().unwrap();
        let pages = tmp.path().join("pages");
        std::fs::create_dir_all(&pages).unwrap();
        for (name, body) in &[
            ("a", "---\ntitle: A\n---\nbody a"),
            ("b", "---\ntitle: B\n---\nbody b"),
            ("c", "no frontmatter just body"),
        ] {
            std::fs::write(pages.join(format!("{name}.md")), body).unwrap();
        }
        let n = full_rebuild(tmp.path()).unwrap();
        assert_eq!(n, 3);
        let idx = get_or_open(tmp.path()).unwrap();
        assert_eq!(idx.num_docs().unwrap(), 3);
        drop_cached(tmp.path());
    }

    /// on_page_mutated upserts a page reading from disk and Delete
    /// removes it. Pin the production path end-to-end: write a real
    /// .md file, fire the hook, observe the indexed doc; delete the
    /// hook, observe the doc gone.
    #[test]
    fn on_page_mutated_round_trip_via_disk() {
        let tmp = tempdir().unwrap();
        let pages = tmp.path().join("pages");
        std::fs::create_dir_all(&pages).unwrap();
        let page_path = pages.join("test.md");
        std::fs::write(
            &page_path,
            "---\ntitle: Test page\ntopic: demo\n---\nbody contents",
        )
        .unwrap();

        on_page_mutated(tmp.path(), "test", Op::Upsert);
        {
            let idx = get_or_open(tmp.path()).unwrap();
            assert_eq!(idx.num_docs().unwrap(), 1);
        }
        on_page_mutated(tmp.path(), "test", Op::Delete);
        {
            let idx = get_or_open(tmp.path()).unwrap();
            assert_eq!(idx.num_docs().unwrap(), 0);
        }
        drop_cached(tmp.path());
    }

    /// Thai content tokenizes through `ThaiOrEnglishTokenizer`
    /// without panicking + ends up indexed.
    #[test]
    fn thai_content_indexes_without_panic() {
        let tmp = tempdir().unwrap();
        let idx = get_or_open(tmp.path()).unwrap();
        idx.upsert_page("thai-page", &empty_fm(), "การทดสอบ ระบบ ใหม่")
            .unwrap();
        assert_eq!(idx.num_docs().unwrap(), 1);
        drop_cached(tmp.path());
    }

    #[test]
    fn split_csv_handles_commas_and_whitespace() {
        assert_eq!(split_csv("a, b, c"), vec!["a", "b", "c"]);
        assert_eq!(split_csv("a b c"), vec!["a", "b", "c"]);
        assert_eq!(split_csv(""), Vec::<String>::new());
        assert_eq!(split_csv("  ,  ,  "), Vec::<String>::new());
    }
}

// Re-export PathBuf so the `pub fn` signatures above don't force
// callers to import std::path explicitly. (Inlined-doc hygiene.)
#[allow(unused_imports)]
use std::path::PathBuf as _PathBuf;
