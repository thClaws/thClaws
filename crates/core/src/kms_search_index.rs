//! KMS BM25 search index (dev-plan/36, reworked in dev-plan/64 phase 1).
//!
//! Tantivy index living at `<kms_root>/.index/`. The model-facing
//! `KmsSearch(query:)` tool and the per-turn KMS reminder read it; every
//! KMS write pushes into it, and [`ensure_fresh`] reconciles it with the
//! disk before a search, so a page edited outside thClaws is found too.
//!
//! ## Layout on disk
//!
//! ```text
//! <kms_root>/
//! ├── pages/ sources/         ← source of truth
//! └── .index/                 ← THIS module owns
//!     ├── meta.json, *.idx …  ← tantivy's own files
//!     └── manifest.json       ← ours: index_version + (mtime, len) per doc
//! ```
//!
//! ## Two kinds of text, two kinds of field
//!
//! Scripts written **with** spaces go through a plain word tokenizer
//! (Unicode alphanumerics, lower-cased, ASCII-folded so `metis` finds
//! `mētis`) into `title` / `topic` / `names` / `body`.
//!
//! Scripts written **without** spaces — Thai, Lao, Khmer, Myanmar, CJK —
//! are indexed as overlapping character pairs into `head_ng` / `body_ng`,
//! and queried as a phrase of consecutive pairs. That is substring
//! matching with BM25 on top, and it needs no dictionary. The previous
//! design segmented Thai with a dictionary that was still its 229-word
//! placeholder: 95% of a real vault's Thai terms were out-of-vocabulary,
//! whole clauses were indexed as single tokens, a word tokenized
//! differently in a query than in a sentence, and a spaceless query
//! became a strict phrase — so `แรงงาน` found 1 of the 13 pages that
//! contain it, with no error. A better dictionary shrinks that problem;
//! character pairs remove it, because there is no segmentation for the
//! query and the document to disagree about.
//!
//! ## Locking
//!
//! The tantivy writer holds a directory lock. It is created only for the
//! duration of a write and dropped after the commit; searching never
//! takes it. The desktop window and each agent's `--serve` child share a
//! vault, and when the writer was opened on the read path and kept for
//! the life of the process, the second process could not search at all.
//!
//! ## Feature gating
//!
//! Behind `kms_search_index` per dev-plan/36 D3. Release builds and
//! `make install` turn it on.

#![cfg(feature = "kms_search_index")]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tantivy::schema::{Field, Schema, FAST, INDEXED, STORED, STRING};
use tantivy::tokenizer::{
    AsciiFoldingFilter, LowerCaser, TextAnalyzer, Token, TokenStream, Tokenizer,
};
use tantivy::{doc, Index, IndexWriter, Term};

/// Tantivy memory budget for a writer. It lives only for one batch.
const WRITER_MEMORY_BUDGET: usize = 50_000_000;

/// Word tokenizer for scripts written with spaces.
const WORD_TOKENIZER: &str = "kms_words";
/// Character-pair tokenizer for scripts written without them.
const NGRAM_TOKENIZER: &str = "kms_pairs";

/// Bumped on any schema or tokenizer change. [`ensure_fresh`] rebuilds an
/// index whose manifest carries another version — or has no manifest at
/// all, which is an index of unknown version.
///
/// 3: character-pair fields for spaceless scripts, `names` (slug +
/// aliases), ASCII folding, per-document freshness in the manifest.
pub const INDEX_VERSION: u32 = 3;

/// Bytes of a single source file fed to the indexer. Sources are raw
/// archived material — an ingested log or CSV can be tens of MB, and
/// indexing the whole thing buys nothing (BM25 saturates long before)
/// while costing memory and index size. Pages are never capped; they
/// are hand/LLM-authored and bounded by construction.
const SOURCE_INDEX_MAX_BYTES: usize = 2 * 1024 * 1024;

/// What changed on a KMS page so the indexer knows whether to
/// upsert (re-add document) or delete (remove document by page
/// name). [`crate::kms`] write functions invoke [`on_page_mutated`]
/// after their on-disk operation succeeds.
#[derive(Debug, Clone)]
pub enum Op {
    Upsert,
    Delete,
}

/// Which layer a document belongs to. Hits carry their kind so the
/// caller can route the follow-up read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DocKind {
    Page,
    Source,
}

impl DocKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DocKind::Page => "page",
            DocKind::Source => "source",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "source" => DocKind::Source,
            _ => DocKind::Page,
        }
    }
}

/// Errors the indexer can surface. Distinct from [`crate::error::Error`]
/// so the calling KMS write path can choose to swallow vs propagate.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("index io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tantivy: {0}")]
    Tantivy(String),
    #[error("kms root missing or unreadable: {0}")]
    KmsRoot(String),
    /// Another process holds the index's write lock right now. Searching
    /// is still possible; writing has to wait.
    #[error("the search index is being written by another thClaws process")]
    Busy,
}

impl From<tantivy::TantivyError> for IndexError {
    fn from(e: tantivy::TantivyError) -> Self {
        match e {
            tantivy::TantivyError::LockFailure(..) => IndexError::Busy,
            other => IndexError::Tantivy(other.to_string()),
        }
    }
}

/// Compiled schema + field handles.
struct Fields {
    /// `"<kind>\u{1}<name>"` — the unique delete key.
    docid: Field,
    kind: Field,
    page: Field,
    title: Field,
    topic: Field,
    /// The document's own names, which are not in its text: the slug, the
    /// slug with its hyphens opened up, and `aliases:`. A research-built
    /// page has an English slug and a Thai title and body, so without
    /// this the one English handle on the page was the one thing not
    /// indexed — `related: ["metis"]` named a page search could not find.
    names: Field,
    tags: Field,
    category: Field,
    sources: Field,
    body: Field,
    /// Character pairs of title + topic + names.
    head_ng: Field,
    /// Character pairs of the body.
    body_ng: Field,
    #[allow(dead_code)]
    updated: Field,
}

fn docid(kind: DocKind, name: &str) -> String {
    format!("{}\u{1}{name}", kind.as_str())
}

fn build_schema() -> (Schema, Fields) {
    use tantivy::schema::{IndexRecordOption, TextFieldIndexing, TextOptions};
    let indexed = |tokenizer: &str| {
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(tokenizer)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
    };
    let mut sb = Schema::builder();
    let docid = sb.add_text_field("docid", STRING);
    let kind = sb.add_text_field("kind", STRING | STORED);
    let page = sb.add_text_field("page", STRING | STORED);
    let title = sb.add_text_field("title", indexed(WORD_TOKENIZER).set_stored());
    let topic = sb.add_text_field("topic", indexed(WORD_TOKENIZER).set_stored());
    let names = sb.add_text_field("names", indexed(WORD_TOKENIZER));
    // Body is indexed but NOT stored — the page lives on disk.
    let body = sb.add_text_field("body", indexed(WORD_TOKENIZER));
    let head_ng = sb.add_text_field("head_ng", indexed(NGRAM_TOKENIZER));
    let body_ng = sb.add_text_field("body_ng", indexed(NGRAM_TOKENIZER));
    let tags = sb.add_text_field("tags", STRING | STORED);
    let category = sb.add_text_field("category", STRING | STORED);
    let sources = sb.add_text_field("sources", STRING);
    let updated = sb.add_i64_field("updated", INDEXED | STORED | FAST);
    (
        sb.build(),
        Fields {
            docid,
            kind,
            page,
            title,
            topic,
            names,
            tags,
            category,
            sources,
            body,
            head_ng,
            body_ng,
            updated,
        },
    )
}

/// Wraps a tantivy `Index` rooted at `<kms_root>/.index/`.
pub struct SearchIndex {
    kms_root: PathBuf,
    index: Index,
    fields: Fields,
    /// Serialises writes from this process. The tantivy writer itself is
    /// made per batch and dropped, so its directory lock is never held
    /// while idle — see the module docs.
    write_gate: Mutex<()>,
}

/// One scored hit from a query, in descending score order.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// BM25 score with the per-field boosts applied. Higher = better.
    pub score: f32,
    pub kind: DocKind,
    /// Page stem (no `.md`) for a page; source filename *with*
    /// extension for a source.
    pub page: String,
    pub title: Option<String>,
    pub topic: Option<String>,
    /// First ~200 chars of the body for at-a-glance context.
    pub snippet_preview: String,
}

/// Where a vault's search index lives (dev-plan/64 D7).
///
/// Beside the vault, not in it: `<scope>/kms/.index/<vault>/` for a vault at
/// `<scope>/kms/<vault>/`. The index is a few dozen binary files that change
/// on every write — inside the vault they were what fought a sync client,
/// cluttered an Obsidian folder and bloated a git history, and they are the
/// one thing in there that can always be rebuilt. `.research/` stays in the
/// vault on purpose: it holds the citation registry and checked evidence,
/// which must travel with it.
///
/// Every real scope root is a folder named `kms` (project, user, shared). A
/// vault anywhere else — a bare directory, as in tests — keeps its index
/// inside, so nothing is written into a parent this code does not own.
pub fn index_dir(kms_root: &Path) -> PathBuf {
    let in_scope = kms_root
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "kms");
    match (in_scope, kms_root.parent(), kms_root.file_name()) {
        (true, Some(scope), Some(name)) => scope.join(".index").join(name),
        _ => kms_root.join(".index"),
    }
}

/// Move an index left inside the vault by an older build to [`index_dir`].
/// A rename, and if that fails the old copy is simply removed: it is a
/// cache, and `ensure_fresh` rebuilds what is missing.
fn adopt_legacy_index(kms_root: &Path) {
    let legacy = kms_root.join(".index");
    let home = index_dir(kms_root);
    if legacy == home || !legacy.is_dir() {
        return;
    }
    let moved = !home.exists()
        && home
            .parent()
            .map(std::fs::create_dir_all)
            .transpose()
            .is_ok()
        && std::fs::rename(&legacy, &home).is_ok();
    if !moved {
        let _ = std::fs::remove_dir_all(&legacy);
    }
}

impl SearchIndex {
    /// Open an existing index at [`index_dir`] or create a fresh one. Takes
    /// no lock.
    pub fn open_or_create(kms_root: &Path) -> Result<Self, IndexError> {
        adopt_legacy_index(kms_root);
        let index_dir = index_dir(kms_root);
        std::fs::create_dir_all(&index_dir)?;
        let (schema, fields) = build_schema();

        // A cache: one this tantivy can't open (written by another version,
        // or torn) is dropped and rebuilt by `ensure_fresh`, not an error.
        let opened = index_dir
            .join("meta.json")
            .exists()
            .then(|| Index::open_in_dir(&index_dir).ok())
            .flatten();
        let index = match opened {
            Some(index) => index,
            None => {
                if index_dir.join("meta.json").exists() {
                    std::fs::remove_dir_all(&index_dir)?;
                    std::fs::create_dir_all(&index_dir)?;
                }
                Index::create_in_dir(&index_dir, schema)?
            }
        };
        index.tokenizers().register(
            WORD_TOKENIZER,
            TextAnalyzer::builder(WordTokenizer)
                .filter(LowerCaser)
                .filter(AsciiFoldingFilter)
                .build(),
        );
        index.tokenizers().register(
            NGRAM_TOKENIZER,
            TextAnalyzer::builder(PairTokenizer).build(),
        );
        Ok(Self {
            kms_root: kms_root.to_path_buf(),
            index,
            fields,
            write_gate: Mutex::new(()),
        })
    }

    /// Run one batch of writes under a short-lived writer and commit it.
    fn write<R>(
        &self,
        batch: impl FnOnce(&mut IndexWriter) -> Result<R, IndexError>,
    ) -> Result<R, IndexError> {
        let _gate = self
            .write_gate
            .lock()
            .map_err(|e| IndexError::Tantivy(format!("write gate poisoned: {e}")))?;
        let mut writer: IndexWriter = self.index.writer(WRITER_MEMORY_BUDGET)?;
        let out = batch(&mut writer)?;
        writer.commit()?;
        Ok(out)
    }

    /// Stage one document. The caller commits.
    fn stage(
        &self,
        writer: &mut IndexWriter,
        kind: DocKind,
        name: &str,
        frontmatter: &BTreeMap<String, String>,
        body: &str,
    ) -> Result<(), IndexError> {
        let id = docid(kind, name);
        writer.delete_term(Term::from_field_text(self.fields.docid, &id));

        let value = |k: &str| frontmatter.get(k).map(String::as_str).unwrap_or("");
        let title = value("title").trim_matches('"');
        let topic = value("topic").trim_matches('"');
        let stem = match kind {
            DocKind::Page => name,
            DocKind::Source => name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name),
        };
        let aliases = value("aliases").replace(['[', ']', '"', ','], " ");
        let names = format!("{stem} {} {aliases}", stem.replace(['-', '_'], " "));
        let head = format!("{title}\n{topic}\n{names}");

        let mut document = doc!(
            self.fields.docid => id.as_str(),
            self.fields.kind => kind.as_str(),
            self.fields.page => name,
            self.fields.title => title,
            self.fields.topic => topic,
            self.fields.names => names.as_str(),
            self.fields.category => value("category"),
            self.fields.body => body,
            self.fields.head_ng => head.as_str(),
            self.fields.body_ng => body,
            self.fields.updated => current_unix_secs(),
        );
        for tag in split_csv(value("tags")) {
            document.add_text(self.fields.tags, &tag);
        }
        for src in split_csv(value("sources")) {
            document.add_text(self.fields.sources, &src);
        }
        writer.add_document(document)?;
        Ok(())
    }

    /// Upsert a page by name. Commits before returning.
    pub fn upsert_page(
        &self,
        page_name: &str,
        frontmatter: &BTreeMap<String, String>,
        body: &str,
    ) -> Result<(), IndexError> {
        self.upsert(DocKind::Page, page_name, frontmatter, body)
    }

    /// Upsert a document in either layer. `name` is the page stem for
    /// [`DocKind::Page`] and the source filename *including* extension
    /// for [`DocKind::Source`].
    pub fn upsert(
        &self,
        kind: DocKind,
        name: &str,
        frontmatter: &BTreeMap<String, String>,
        body: &str,
    ) -> Result<(), IndexError> {
        self.write(|w| self.stage(w, kind, name, frontmatter, body))
    }

    /// Delete a page by name. Commits before returning.
    pub fn delete_page(&self, page_name: &str) -> Result<(), IndexError> {
        self.delete(DocKind::Page, page_name)
    }

    /// Delete a document in either layer. Commits before returning.
    pub fn delete(&self, kind: DocKind, name: &str) -> Result<(), IndexError> {
        self.write(|w| {
            w.delete_term(Term::from_field_text(self.fields.docid, &docid(kind, name)));
            Ok(())
        })
    }

    /// Document count (post-commit). For diagnostics + tests.
    pub fn num_docs(&self) -> Result<u64, IndexError> {
        Ok(self.index.reader()?.searcher().num_docs())
    }

    /// Ranked search over both layers. See [`Self::search_scoped`].
    pub fn search(
        &self,
        query_str: &str,
        tags_filter: &[String],
        category_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, IndexError> {
        self.search_scoped(query_str, tags_filter, category_filter, limit, None)
    }

    /// The query, as a disjunction of per-term clauses.
    ///
    /// Built by hand rather than with tantivy's `QueryParser`, for two
    /// reasons. The parser treats punctuation in a user's words as query
    /// syntax (`C++`, `title:x`, an unbalanced quote) and errors. And it
    /// turns a spaceless run into a strict phrase over whatever the
    /// tokenizer produced, which is exactly where Thai went missing.
    fn build_query(&self, q: &str) -> Option<Box<dyn tantivy::query::Query>> {
        use tantivy::query::{BooleanQuery, BoostQuery, Occur, PhraseQuery, Query, TermQuery};
        use tantivy::schema::IndexRecordOption;

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let mut should = |q: Box<dyn Query>, boost: f32| {
            clauses.push((Occur::Should, Box::new(BoostQuery::new(q, boost))));
        };

        // Words: whatever the word analyzer makes of the query.
        let mut words = self.index.tokenizers().get(WORD_TOKENIZER)?;
        let mut stream = words.token_stream(q);
        let mut tokens: Vec<String> = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        for tok in &tokens {
            for (field, boost) in [
                (self.fields.title, 4.0),
                (self.fields.names, 4.0),
                (self.fields.topic, 2.0),
                (self.fields.body, 1.0),
            ] {
                should(
                    Box::new(TermQuery::new(
                        Term::from_field_text(field, tok),
                        IndexRecordOption::WithFreqs,
                    )),
                    boost,
                );
            }
        }

        // Spaceless runs: consecutive character pairs, as a phrase. One
        // pair is a term; a single character has no pair and is skipped.
        for run in spaceless_runs(&normalize_spaceless(q)) {
            let pairs = char_pairs(&run);
            for (field, boost) in [(self.fields.head_ng, 4.0), (self.fields.body_ng, 1.0)] {
                let terms: Vec<Term> = pairs
                    .iter()
                    .map(|p| Term::from_field_text(field, p))
                    .collect();
                match terms.len() {
                    0 => {}
                    1 => should(
                        Box::new(TermQuery::new(
                            terms.into_iter().next().expect("one term"),
                            IndexRecordOption::WithFreqs,
                        )),
                        boost,
                    ),
                    _ => should(Box::new(PhraseQuery::new(terms)), boost),
                }
            }
        }

        if clauses.is_empty() {
            None
        } else {
            Some(Box::new(BooleanQuery::new(clauses)))
        }
    }

    /// BM25-ranked search: title and names ×4, topic ×2, body ×1, with
    /// the character-pair fields boosted to match. `kind_filter`
    /// restricts results to one layer; tags are any-of; category is
    /// exact. An empty query returns nothing rather than everything.
    pub fn search_scoped(
        &self,
        query_str: &str,
        tags_filter: &[String],
        category_filter: Option<&str>,
        limit: usize,
        kind_filter: Option<DocKind>,
    ) -> Result<Vec<SearchHit>, IndexError> {
        use tantivy::collector::TopDocs;
        use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
        use tantivy::schema::IndexRecordOption;

        let Some(base_query) = self.build_query(query_str.trim()) else {
            return Ok(Vec::new());
        };
        let limit = limit.clamp(1, 50);
        let searcher = self.index.reader()?.searcher();

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, base_query)];
        let exact = |field: Field, value: &str| -> Box<dyn Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(field, value.trim()),
                IndexRecordOption::Basic,
            ))
        };
        if !tags_filter.is_empty() {
            let any: Vec<(Occur, Box<dyn Query>)> = tags_filter
                .iter()
                .map(|t| (Occur::Should, exact(self.fields.tags, t)))
                .collect();
            clauses.push((Occur::Must, Box::new(BooleanQuery::new(any))));
        }
        if let Some(c) = category_filter {
            clauses.push((Occur::Must, exact(self.fields.category, c)));
        }
        if let Some(k) = kind_filter {
            clauses.push((Occur::Must, exact(self.fields.kind, k.as_str())));
        }
        let final_query = BooleanQuery::new(clauses);

        let top_docs = searcher
            .search(&final_query, &TopDocs::with_limit(limit).order_by_score())
            .map_err(|e| IndexError::Tantivy(format!("search: {e}")))?;

        let mut hits = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let retrieved: tantivy::TantivyDocument = searcher
                .doc(doc_address)
                .map_err(|e| IndexError::Tantivy(format!("doc fetch: {e}")))?;
            let page = first_text(&retrieved, self.fields.page).unwrap_or_default();
            let kind = first_text(&retrieved, self.fields.kind)
                .map(|s| DocKind::parse(&s))
                .unwrap_or(DocKind::Page);
            let snippet_preview = self.read_snippet_preview(kind, &page);
            hits.push(SearchHit {
                score,
                kind,
                title: first_text(&retrieved, self.fields.title).filter(|s| !s.is_empty()),
                topic: first_text(&retrieved, self.fields.topic).filter(|s| !s.is_empty()),
                page,
                snippet_preview,
            });
        }
        Ok(hits)
    }

    /// First line of real prose, ~200 chars, for the hit preview.
    /// Best-effort: empty on any I/O error.
    fn read_snippet_preview(&self, kind: DocKind, name: &str) -> String {
        let path = match kind {
            DocKind::Page => self.kms_root.join("pages").join(format!("{name}.md")),
            DocKind::Source => self.kms_root.join("sources").join(name),
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return String::new();
        };
        let (_fm, body) = crate::kms::parse_frontmatter(&raw);
        let mut in_fence = false;
        body.lines()
            .map(str::trim)
            .find(|l| {
                if l.starts_with("```") || l.starts_with("~~~") {
                    in_fence = !in_fence;
                    return false;
                }
                !in_fence
                    && !l.is_empty()
                    && !l.starts_with('#')
                    && !l.starts_with('>')
                    && !l.starts_with("---")
                    && !l.starts_with("<!--")
            })
            .unwrap_or("")
            .chars()
            .take(200)
            .collect()
    }
}

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

// ── Registry ─────────────────────────────────────────────────────────

/// One opened `SearchIndex` per `kms_root` for the process. Opening is
/// cheap and takes no lock; the cache exists so concurrent writers in
/// this process queue on one `write_gate` instead of racing each other
/// for tantivy's directory lock.
fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<SearchIndex>>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, Arc<SearchIndex>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cached `SearchIndex` for `kms_root`, opened on first use.
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

/// Drop the cached `SearchIndex` for `kms_root` if any. Idempotent.
pub fn drop_cached(kms_root: &Path) {
    let canonical = kms_root
        .canonicalize()
        .unwrap_or_else(|_| kms_root.to_path_buf());
    if let Ok(mut reg) = registry().lock() {
        reg.remove(&canonical);
    }
}

// ── Push path: a KMS write tells the index ───────────────────────────

/// Notify the indexer that a page in `kms_root` mutated. Errors are
/// logged but never propagated — a write that succeeded shouldn't roll
/// back because the index could not follow. [`ensure_fresh`] picks up
/// whatever this misses (another process holding the lock, say).
pub fn on_page_mutated(kms_root: &Path, page_name: &str, op: Op) {
    if let Err(e) = on_page_mutated_inner(kms_root, page_name, op) {
        log_push_error(kms_root, "page", page_name, &e);
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

/// Same contract as [`on_page_mutated`] for the `sources/` layer.
/// `file_name` includes the extension (`spec.txt`, not `spec`).
pub fn on_source_mutated(kms_root: &Path, file_name: &str, op: Op) {
    if let Err(e) = on_source_mutated_inner(kms_root, file_name, op) {
        log_push_error(kms_root, "source", file_name, &e);
    }
}

fn on_source_mutated_inner(kms_root: &Path, file_name: &str, op: Op) -> Result<(), IndexError> {
    let idx = get_or_open(kms_root)?;
    match op {
        Op::Delete => idx.delete(DocKind::Source, file_name),
        Op::Upsert => {
            let path = kms_root.join("sources").join(file_name);
            let (fm, body) = read_source_for_index(&path)?;
            idx.upsert(DocKind::Source, file_name, &fm, &body)
        }
    }
}

fn log_push_error(kms_root: &Path, what: &str, name: &str, e: &IndexError) {
    // Busy is routine in a workspace with several agents, and the next
    // search reconciles it; it is not worth a yellow line per write.
    if matches!(e, IndexError::Busy) {
        return;
    }
    eprintln!(
        "\x1b[33m[kms-search-index] {} {what}='{name}' error: {e}\x1b[0m",
        kms_root.display()
    );
}

/// Read a source for indexing: frontmatter when it has any, body capped
/// at [`SOURCE_INDEX_MAX_BYTES`] on a character boundary.
fn read_source_for_index(path: &Path) -> Result<(BTreeMap<String, String>, String), IndexError> {
    let raw = std::fs::read_to_string(path)?;
    let (fm, body) = crate::kms::parse_frontmatter(&raw);
    let body = if body.len() > SOURCE_INDEX_MAX_BYTES {
        let mut end = SOURCE_INDEX_MAX_BYTES;
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        body[..end].to_string()
    } else {
        body
    };
    Ok((fm, body))
}

// ── Freshness: the index against the disk ────────────────────────────

/// `.index/manifest.json`. `docs` maps `"page/<stem>"` /
/// `"source/<file>"` to the `(mtime seconds, length)` it was indexed at.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    index_version: u32,
    #[serde(default)]
    last_full_rebuild_at: i64,
    #[serde(default)]
    docs: BTreeMap<String, (i64, u64)>,
}

fn manifest_path(kms_root: &Path) -> PathBuf {
    index_dir(kms_root).join("manifest.json")
}

fn read_manifest(kms_root: &Path) -> Option<Manifest> {
    serde_json::from_str(&std::fs::read_to_string(manifest_path(kms_root)).ok()?).ok()
}

fn write_manifest(kms_root: &Path, m: &Manifest) {
    if let Ok(json) = serde_json::to_string(m) {
        let _ = std::fs::write(manifest_path(kms_root), json);
    }
}

/// One indexable file on disk.
struct DocFile {
    kind: DocKind,
    /// Page stem, or source filename with extension.
    name: String,
    path: PathBuf,
    stamp: (i64, u64),
}

impl DocFile {
    fn key(&self) -> String {
        format!("{}/{}", self.kind.as_str(), self.name)
    }
}

/// Every page and source the index should hold. Symlinks are never
/// followed, and `_`/`.`-prefixed files (`_catalog.json`) are the KMS's
/// own bookkeeping, not material.
fn scan_docs(kms_root: &Path) -> Vec<DocFile> {
    let mut out = Vec::new();
    for (kind, dir) in [(DocKind::Page, "pages"), (DocKind::Source, "sources")] {
        let Ok(rd) = std::fs::read_dir(kms_root.join(dir)) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if file_name.starts_with('_') || file_name.starts_with('.') {
                continue;
            }
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            let name = match kind {
                DocKind::Page if ext == "md" => match path.file_stem().and_then(|s| s.to_str()) {
                    Some(stem) => stem.to_string(),
                    None => continue,
                },
                DocKind::Source if crate::kms::SOURCE_EXTENSIONS.iter().any(|e| *e == ext) => {
                    file_name.to_string()
                }
                _ => continue,
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            out.push(DocFile {
                kind,
                name,
                path,
                stamp: (mtime, meta.len()),
            });
        }
    }
    out.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    out
}

/// Stage one file. A source that is not valid UTF-8 is skipped rather
/// than failing the batch — one bad archive must not leave the KMS
/// unsearchable. Returns whether it was staged.
fn stage_file(idx: &SearchIndex, w: &mut IndexWriter, f: &DocFile) -> Result<bool, IndexError> {
    let read = match f.kind {
        DocKind::Page => std::fs::read_to_string(&f.path)
            .map(|raw| crate::kms::parse_frontmatter(&raw))
            .map_err(IndexError::from),
        DocKind::Source => read_source_for_index(&f.path),
    };
    match read {
        Ok((fm, body)) => {
            idx.stage(w, f.kind, &f.name, &fm, &body)?;
            Ok(true)
        }
        Err(e) => {
            eprintln!(
                "\x1b[33m[kms-search-index] skipping {} '{}': {e}\x1b[0m",
                f.kind.as_str(),
                f.name
            );
            Ok(false)
        }
    }
}

/// Rebuild from scratch: drop `.index/`, index every page and source in
/// one commit, write the manifest. Returns the number of documents.
///
/// The manifest is written *here*. It used to be written only by the
/// search tool after a rebuild that the tool itself had triggered, so an
/// index built any other way — incrementally by KMS writes, or by
/// `/kms reindex` — had none, and the version check that reads it could
/// never fire: a tokenizer fix would have reached no existing vault.
pub fn full_rebuild(kms_root: &Path) -> Result<usize, IndexError> {
    drop_cached(kms_root);
    let _ = std::fs::remove_dir_all(kms_root.join(".index"));
    let index_dir = index_dir(kms_root);
    if index_dir.exists() {
        std::fs::remove_dir_all(&index_dir)?;
    }
    let idx = get_or_open(kms_root)?;
    let files = scan_docs(kms_root);
    let mut manifest = Manifest {
        index_version: INDEX_VERSION,
        last_full_rebuild_at: current_unix_secs(),
        docs: BTreeMap::new(),
    };
    idx.write(|w| {
        for f in &files {
            if stage_file(&idx, w, f)? {
                manifest.docs.insert(f.key(), f.stamp);
            }
        }
        Ok(())
    })?;
    write_manifest(kms_root, &manifest);
    Ok(manifest.docs.len())
}

/// What [`ensure_fresh`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Freshness {
    /// `Some(n)`: the index was rebuilt from scratch with `n` documents.
    pub rebuilt: Option<usize>,
    /// Documents re-indexed because the file changed or was new.
    pub updated: usize,
    /// Documents dropped because the file is gone.
    pub removed: usize,
    /// Another process held the write lock, so the index is as it was.
    pub busy: bool,
}

impl Freshness {
    pub fn changed(&self) -> bool {
        self.rebuilt.is_some() || self.updated > 0 || self.removed > 0
    }
}

/// Bring the index up to date with the disk. Call before a search.
///
/// - No manifest, or one from another [`INDEX_VERSION`]: full rebuild.
/// - Otherwise one `stat` per file against the manifest, re-indexing
///   only what changed. This is what makes a page edited in Obsidian —
///   or written while another process held the lock — findable; the
///   index used to learn of changes only from thClaws' own writes.
///
/// If another process holds the write lock the index is left as it is
/// and `busy` is set: a slightly stale search beats none.
pub fn ensure_fresh(kms_root: &Path) -> Result<Freshness, IndexError> {
    let mut report = Freshness::default();
    let current = read_manifest(kms_root).filter(|m| m.index_version == INDEX_VERSION);
    let Some(mut manifest) = current else {
        match full_rebuild(kms_root) {
            Ok(n) => report.rebuilt = Some(n),
            Err(IndexError::Busy) if index_dir(kms_root).join("meta.json").exists() => {
                report.busy = true;
            }
            Err(e) => return Err(e),
        }
        return Ok(report);
    };

    let files = scan_docs(kms_root);
    let on_disk: BTreeMap<String, &DocFile> = files.iter().map(|f| (f.key(), f)).collect();
    let stale: Vec<&DocFile> = files
        .iter()
        .filter(|f| manifest.docs.get(&f.key()) != Some(&f.stamp))
        .collect();
    let gone: Vec<String> = manifest
        .docs
        .keys()
        .filter(|k| !on_disk.contains_key(*k))
        .cloned()
        .collect();
    if stale.is_empty() && gone.is_empty() {
        return Ok(report);
    }

    let idx = get_or_open(kms_root)?;
    let outcome = idx.write(|w| {
        for f in &stale {
            // Recorded even when skipped, or an unreadable file would be
            // retried on every search.
            stage_file(&idx, w, f)?;
            manifest.docs.insert(f.key(), f.stamp);
        }
        for key in &gone {
            if let Some((kind, name)) = key.split_once('/') {
                w.delete_term(Term::from_field_text(
                    idx.fields.docid,
                    &docid(DocKind::parse(kind), name),
                ));
            }
            manifest.docs.remove(key);
        }
        Ok(())
    });
    match outcome {
        Ok(()) => {
            report.updated = stale.len();
            report.removed = gone.len();
            write_manifest(kms_root, &manifest);
        }
        Err(IndexError::Busy) => report.busy = true,
        Err(e) => return Err(e),
    }
    Ok(report)
}

// ── Tokenizers ───────────────────────────────────────────────────────

/// Text as the character-pair side sees it: zero-width and other
/// invisible format characters removed, and Thai combining marks in one
/// order. Thai web pages and PDFs are full of U+200B as a line-break
/// hint, which is not part of the Thai block and would cut a word in two;
/// and the same syllable arrives with its vowel and tone mark in either
/// order (NFC does not settle it — above vowels are combining class 0).
/// Applied to documents and queries alike, so they agree.
fn normalize_spaceless(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let mut out = String::with_capacity(text.len());
    let mut marks: Vec<char> = Vec::new();
    let flush = |marks: &mut Vec<char>, out: &mut String| {
        marks.sort_by_key(|c| crate::research::digest::thai_mark_rank(*c));
        out.extend(marks.drain(..));
    };
    for c in text.nfc() {
        if matches!(
            c,
            '\u{200B}'..='\u{200F}' | '\u{2060}' | '\u{00AD}' | '\u{FEFF}'
        ) {
            continue;
        }
        if crate::research::digest::thai_mark_rank(c) > 0 {
            marks.push(c);
            continue;
        }
        flush(&mut marks, &mut out);
        out.push(c);
    }
    flush(&mut marks, &mut out);
    out
}

/// Maximal runs of spaceless-script characters in `text`.
fn spaceless_runs(text: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if crate::kms::is_spaceless_script(c) {
            cur.push(c);
        } else if !cur.is_empty() {
            runs.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        runs.push(cur);
    }
    runs
}

/// Overlapping character pairs of a run: `แรงงาน` → `แร รง งง งา าน`.
/// A one-character run is its own single "pair".
fn char_pairs(run: &str) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    match chars.len() {
        0 => Vec::new(),
        1 => vec![run.to_string()],
        _ => chars.windows(2).map(|w| w.iter().collect()).collect(),
    }
}

fn token_stream_of(tokens: Vec<Token>) -> VecTokenStream {
    VecTokenStream {
        tokens,
        cursor: 0,
        current: None,
    }
}

/// Words of scripts written with spaces: maximal runs of Unicode
/// alphanumerics (plus combining accents), excluding spaceless scripts —
/// those belong to [`PairTokenizer`]. The ASCII/non-ASCII split this
/// replaces shredded `mētis` into `m`, `ē`, `tis`.
#[derive(Clone)]
struct WordTokenizer;

impl Tokenizer for WordTokenizer {
    type TokenStream<'a> = VecTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        let is_word = |c: char| {
            (c.is_alphanumeric() || ('\u{0300}'..='\u{036F}').contains(&c))
                && !crate::kms::is_spaceless_script(c)
        };
        let mut tokens = Vec::new();
        let mut start: Option<usize> = None;
        let push = |from: usize, to: usize, tokens: &mut Vec<Token>| {
            tokens.push(Token {
                offset_from: from,
                offset_to: to,
                position: tokens.len(),
                text: text[from..to].to_string(),
                position_length: 1,
            });
        };
        for (i, c) in text.char_indices() {
            match (is_word(c), start) {
                (true, None) => start = Some(i),
                (false, Some(from)) => {
                    push(from, i, &mut tokens);
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(from) = start {
            push(from, text.len(), &mut tokens);
        }
        token_stream_of(tokens)
    }
}

/// Overlapping character pairs of every spaceless run, positions running
/// consecutively inside a run and skipping one between runs — so a phrase
/// of pairs matches a substring of one run and never bridges two.
#[derive(Clone)]
struct PairTokenizer;

impl Tokenizer for PairTokenizer {
    type TokenStream<'a> = VecTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        let mut tokens = Vec::new();
        let mut position = 0usize;
        for run in spaceless_runs(&normalize_spaceless(text)) {
            for pair in char_pairs(&run) {
                tokens.push(Token {
                    // Offsets would index the normalised copy, not `text`;
                    // nothing reads them (no highlighting on these fields).
                    offset_from: 0,
                    offset_to: 0,
                    position,
                    text: pair,
                    position_length: 1,
                });
                position += 1;
            }
            position += 1;
        }
        token_stream_of(tokens)
    }
}

struct VecTokenStream {
    tokens: Vec<Token>,
    cursor: usize,
    current: Option<Token>,
}

impl TokenStream for VecTokenStream {
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
    #[test]
    fn an_index_this_version_cannot_open_is_rebuilt() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("v");
        std::fs::create_dir_all(vault.join("pages")).unwrap();
        let dir = index_dir(&vault);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), "{not an index").unwrap();
        let idx = SearchIndex::open_or_create(&vault).unwrap();
        drop(idx);
        let meta = std::fs::read_to_string(dir.join("meta.json")).unwrap();
        assert!(meta.contains("segments"), "{meta}");
    }

    /// dev-plan/64 D7. In a real scope the index sits beside the vault, an
    /// index an older build left inside is adopted rather than rebuilt, and
    /// the vault folder ends up holding nothing a sync client would fight.
    #[test]
    fn the_index_lives_beside_the_vault_and_a_legacy_one_is_adopted() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("kms").join("งานวิจัย (v2)");
        std::fs::create_dir_all(vault.join("pages")).unwrap();
        std::fs::write(
            vault.join("pages/a.md"),
            "---\ntitle: A\n---\nความฉลาดล้นเหลือ\n",
        )
        .unwrap();
        assert_eq!(index_dir(&vault), tmp.path().join("kms/.index/งานวิจัย (v2)"));
        // A bare directory is not a scope: nothing is written outside it.
        let bare = tmp.path().join("loose");
        assert_eq!(index_dir(&bare), bare.join(".index"));

        // An older build's index, inside the vault.
        let legacy = vault.join(".index");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("marker"), "kept").unwrap();
        adopt_legacy_index(&vault);
        assert!(!legacy.exists(), "gone from the vault");
        assert_eq!(
            std::fs::read_to_string(index_dir(&vault).join("marker")).unwrap(),
            "kept"
        );
        std::fs::remove_file(index_dir(&vault).join("marker")).unwrap();

        let idx = SearchIndex::open_or_create(&vault).unwrap();
        drop(idx);
        full_rebuild(&vault).unwrap();
        drop_cached(&vault);
        assert!(index_dir(&vault).join("meta.json").exists());
        let inside: Vec<String> = std::fs::read_dir(&vault)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(inside, vec!["pages"], "the vault holds only the vault");
    }

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
    fn full_rebuild_indexes_sources_alongside_pages() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("pages")).unwrap();
        std::fs::create_dir_all(tmp.path().join("sources")).unwrap();
        std::fs::write(
            tmp.path().join("pages/note.md"),
            "---\ntitle: Note\n---\n\ncurated prose\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("sources/raw-spec.txt"),
            "the widget frobnicator handles retries\n",
        )
        .unwrap();

        let n = full_rebuild(tmp.path()).unwrap();
        assert_eq!(n, 2, "both layers indexed");

        let idx = get_or_open(tmp.path()).unwrap();
        // The archived source is findable. Before v2 the index held
        // pages only, so ingested material was unreachable by BM25 as
        // well as by regex.
        let hits = idx.search("frobnicator", &[], None, 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].kind, DocKind::Source);
        assert_eq!(hits[0].page, "raw-spec.txt");
    }

    #[test]
    fn source_and_page_can_share_a_stem() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("pages")).unwrap();
        std::fs::create_dir_all(tmp.path().join("sources")).unwrap();
        // Exactly what `/kms ingest` produces: a page and a source
        // under the same alias. Document identity used to be the bare
        // page name, so one would have deleted the other.
        std::fs::write(tmp.path().join("pages/spec.md"), "shared marker page\n").unwrap();
        std::fs::write(tmp.path().join("sources/spec.md"), "shared marker source\n").unwrap();

        full_rebuild(tmp.path()).unwrap();
        let idx = get_or_open(tmp.path()).unwrap();
        let hits = idx.search("marker", &[], None, 10).unwrap();
        assert_eq!(hits.len(), 2, "one clobbered the other: {hits:?}");
    }

    #[test]
    fn kind_filter_narrows_to_one_layer() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("pages")).unwrap();
        std::fs::create_dir_all(tmp.path().join("sources")).unwrap();
        std::fs::write(tmp.path().join("pages/a.md"), "kubernetes notes\n").unwrap();
        std::fs::write(tmp.path().join("sources/b.md"), "kubernetes manual\n").unwrap();
        full_rebuild(tmp.path()).unwrap();
        let idx = get_or_open(tmp.path()).unwrap();

        let pages = idx
            .search_scoped("kubernetes", &[], None, 10, Some(DocKind::Page))
            .unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].page, "a");

        let sources = idx
            .search_scoped("kubernetes", &[], None, 10, Some(DocKind::Source))
            .unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].page, "b.md");

        assert_eq!(idx.search("kubernetes", &[], None, 10).unwrap().len(), 2);
    }

    #[test]
    fn source_stem_words_are_searchable() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sources")).unwrap();
        std::fs::write(
            tmp.path().join("sources/auth-token-refresh.md"),
            "body with no matching words\n",
        )
        .unwrap();
        full_rebuild(tmp.path()).unwrap();
        let idx = get_or_open(tmp.path()).unwrap();
        // A source with no frontmatter gets a de-slugged title so its
        // own filename is a searchable handle.
        let hits = idx.search("token refresh", &[], None, 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
    }

    #[test]
    fn oversized_source_is_truncated_not_rejected() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sources")).unwrap();
        let mut big = "filler ".repeat(400_000); // ~2.8 MB
        big.push_str("needle-at-the-end");
        std::fs::write(tmp.path().join("sources/huge.log"), &big).unwrap();

        let n = full_rebuild(tmp.path()).unwrap();
        assert_eq!(n, 1, "oversized source still indexed");
        let idx = get_or_open(tmp.path()).unwrap();
        assert!(!idx.search("filler", &[], None, 5).unwrap().is_empty());
        // Past the cap, so not indexed — the point is that the rebuild
        // succeeded rather than choking on the file.
        assert!(idx
            .search("needle-at-the-end", &[], None, 5)
            .unwrap()
            .is_empty());
    }

    /// `search()` had no coverage, so the tantivy 0.26 collector change
    /// (`TopDocs::with_limit` is a builder now; ordering comes from
    /// `order_by_score`) would only have been caught by the compiler —
    /// which says nothing about whether results still come back ranked.
    #[test]
    fn search_returns_hits_ranked_by_relevance() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("pages")).unwrap();
        let idx = get_or_open(tmp.path()).unwrap();
        idx.upsert_page(
            "strong",
            &fm_with(&[("title", "Rust ownership"), ("topic", "rust")]),
            "ownership ownership ownership borrow checker",
        )
        .unwrap();
        idx.upsert_page(
            "weak",
            &fm_with(&[("title", "Misc notes"), ("topic", "misc")]),
            "a single mention of ownership among other words",
        )
        .unwrap();
        idx.upsert_page(
            "unrelated",
            &fm_with(&[("title", "Cooking"), ("topic", "food")]),
            "pasta and tomatoes",
        )
        .unwrap();

        let hits = idx.search("ownership", &[], None, 10).unwrap();
        assert_eq!(hits.len(), 2, "only the two ownership pages match");
        assert_eq!(hits[0].page, "strong", "denser match must rank first");
        assert!(
            hits[0].score >= hits[1].score,
            "scores must come back in descending order: {:?}",
            hits.iter().map(|h| (&h.page, h.score)).collect::<Vec<_>>()
        );

        // limit is honoured, and the truncation keeps the top hit.
        let capped = idx.search("ownership", &[], None, 1).unwrap();
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].page, "strong");

        assert!(idx.search("zzzznomatch", &[], None, 10).unwrap().is_empty());
        drop_cached(tmp.path());
    }

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

    /// Thai as it is written — no spaces — must be findable by a word from
    /// the middle of a sentence. The test this replaces indexed Thai with
    /// spaces typed in by hand and never searched; it passed while a real
    /// vault returned 1 of the 13 pages containing `แรงงาน`.
    #[test]
    fn a_thai_word_is_found_inside_unspaced_prose() {
        let tmp = tempdir().unwrap();
        let idx = get_or_open(tmp.path()).unwrap();
        let pages = [
            ("labour", "กฎหมายแรงงานไทยกำหนดให้นายจ้างจ่ายค่าล่วงเวลาแก่ลูกจ้าง"),
            ("automation", "ระบบอัตโนมัติเข้ามาแทนแรงงานในหลายอุตสาหกรรม"),
            ("market", "ผลต่อตลาดแรงงานยังไม่ชัดเจนเมื่อแรงงานปรับตัว"),
            ("weather", "พยากรณ์อากาศวันนี้ฝนตกหนักในภาคใต้"),
        ];
        for (name, body) in pages {
            idx.upsert_page(name, &empty_fm(), body).unwrap();
        }
        let found = |q: &str| -> Vec<String> {
            let mut v: Vec<String> = idx
                .search(q, &[], None, 10)
                .unwrap()
                .into_iter()
                .map(|h| h.page)
                .collect();
            v.sort();
            v
        };
        // Every page that contains the string, and only those.
        assert_eq!(found("แรงงาน"), vec!["automation", "labour", "market"]);
        assert_eq!(found("ค่าล่วงเวลา"), vec!["labour"]);
        assert_eq!(found("ฝนตก"), vec!["weather"]);
        assert!(found("เศรษฐกิจ").is_empty());
        // Two characters is one pair; still a match.
        assert_eq!(found("ฝน"), vec!["weather"]);
        drop_cached(tmp.path());
    }

    /// The query and the page may encode the same Thai differently:
    /// zero-width spaces as line-break hints, a vowel and a tone mark in
    /// either order. They are the same text and must match.
    #[test]
    fn thai_matches_across_encodings() {
        let tmp = tempdir().unwrap();
        let idx = get_or_open(tmp.path()).unwrap();
        idx.upsert_page(
            "living",
            &empty_fm(),
            "มาตรฐาน\u{200B}การ\u{200B}ครองชีพ และ เก\u{0E48}\u{0E34}ย",
        )
        .unwrap();
        assert_eq!(
            idx.search("มาตรฐานการครองชีพ", &[], None, 5).unwrap().len(),
            1
        );
        assert_eq!(
            idx.search("เก\u{0E34}\u{0E48}ย", &[], None, 5)
                .unwrap()
                .len(),
            1
        );
        drop_cached(tmp.path());
    }

    /// Chinese and Japanese have no spaces either.
    #[test]
    fn cjk_is_found_by_substring() {
        let tmp = tempdir().unwrap();
        let idx = get_or_open(tmp.path()).unwrap();
        idx.upsert_page("jp", &empty_fm(), "日本語のテストです")
            .unwrap();
        idx.upsert_page("zh", &empty_fm(), "人工智能改变世界")
            .unwrap();
        let pages = |q: &str| -> Vec<String> {
            idx.search(q, &[], None, 5)
                .unwrap()
                .into_iter()
                .map(|h| h.page)
                .collect()
        };
        assert_eq!(pages("テスト"), vec!["jp"]);
        assert_eq!(pages("智能"), vec!["zh"]);
        drop_cached(tmp.path());
    }

    /// A page is found by its own slug and by its aliases — neither is in
    /// its text. A research-built page has an English slug over a Thai
    /// title and body, so the slug was the only English handle on it, and
    /// it was the one field not searched.
    #[test]
    fn a_page_is_found_by_its_slug_and_aliases() {
        let tmp = tempdir().unwrap();
        let idx = get_or_open(tmp.path()).unwrap();
        idx.upsert_page(
            "baumols-cost-disease",
            &fm_with(&[
                ("title", "โรคต้นทุนของโบมอล"),
                ("aliases", "[\"cost disease\", \"ต้นทุนบานปลาย\"]"),
            ]),
            "ภาคบริการที่ผลิตภาพโตช้าจะแพงขึ้นเรื่อย ๆ",
        )
        .unwrap();
        idx.upsert_page(
            "metis",
            &fm_with(&[("title", "mētis — ความรู้เชิงปฏิบัติ")]),
            "ความรู้ที่ได้จากการลงมือทำ",
        )
        .unwrap();
        let first = |q: &str| {
            idx.search(q, &[], None, 5)
                .unwrap()
                .first()
                .map(|h| h.page.clone())
        };
        assert_eq!(
            first("baumols-cost-disease").as_deref(),
            Some("baumols-cost-disease")
        );
        assert_eq!(
            first("baumol cost").as_deref(),
            Some("baumols-cost-disease")
        );
        assert_eq!(
            first("cost disease").as_deref(),
            Some("baumols-cost-disease")
        );
        assert_eq!(
            first("ต้นทุนบานปลาย").as_deref(),
            Some("baumols-cost-disease")
        );
        // ASCII folding: the title says `mētis`.
        assert_eq!(first("metis").as_deref(), Some("metis"));
        assert_eq!(first("Mētis").as_deref(), Some("metis"));
        drop_cached(tmp.path());
    }

    /// A user's words are words. `QueryParser` read these as syntax and
    /// returned an error instead of results.
    #[test]
    fn punctuation_in_a_query_is_not_syntax() {
        let tmp = tempdir().unwrap();
        let idx = get_or_open(tmp.path()).unwrap();
        idx.upsert_page("cpp", &empty_fm(), "notes on C++ templates and title: case")
            .unwrap();
        for q in [
            "C++",
            "title:templates",
            "\"unbalanced",
            "(templates",
            "templates AND",
            "a:b:c",
        ] {
            let hits = idx.search(q, &[], None, 5);
            assert!(hits.is_ok(), "`{q}` errored: {:?}", hits.err());
        }
        assert_eq!(idx.search("templates?", &[], None, 5).unwrap().len(), 1);
        assert!(idx.search("   ", &[], None, 5).unwrap().is_empty());
        assert!(idx.search("!!!", &[], None, 5).unwrap().is_empty());
        drop_cached(tmp.path());
    }

    fn write_page_file(root: &Path, stem: &str, body: &str) {
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages").join(format!("{stem}.md")), body).unwrap();
    }

    /// An index with no manifest is an index of unknown version, and gets
    /// rebuilt. This is the gate that was dead: every real index was
    /// built by KMS writes, which wrote no manifest, so "no manifest" plus
    /// "tantivy files exist" read as "up to date" forever — and a
    /// tokenizer fix could never have reached an existing vault.
    #[test]
    fn an_index_without_a_manifest_is_rebuilt() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_page_file(root, "a", "alpha content");
        // Built the way production builds it: by a write, no manifest.
        on_page_mutated(root, "a", Op::Upsert);
        assert!(root.join(".index/meta.json").exists());
        assert!(!manifest_path(root).exists());

        let report = ensure_fresh(root).unwrap();
        assert_eq!(report.rebuilt, Some(1));
        let m = read_manifest(root).expect("manifest written by the rebuild");
        assert_eq!(m.index_version, INDEX_VERSION);
        assert!(m.docs.contains_key("page/a"));
        // Steady state: nothing to do.
        assert_eq!(ensure_fresh(root).unwrap(), Freshness::default());
        drop_cached(root);
    }

    #[test]
    fn a_manifest_from_another_version_triggers_a_rebuild() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_page_file(root, "a", "alpha content");
        full_rebuild(root).unwrap();
        let mut m = read_manifest(root).unwrap();
        m.index_version = INDEX_VERSION - 1;
        write_manifest(root, &m);
        assert_eq!(ensure_fresh(root).unwrap().rebuilt, Some(1));
        assert_eq!(read_manifest(root).unwrap().index_version, INDEX_VERSION);
        drop_cached(root);
    }

    /// `/kms reindex` calls `full_rebuild` directly; it must leave a
    /// manifest behind, or the next search rebuilds all over again.
    #[test]
    fn full_rebuild_writes_the_manifest() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_page_file(root, "a", "alpha");
        write_page_file(root, "b", "beta");
        assert_eq!(full_rebuild(root).unwrap(), 2);
        assert_eq!(ensure_fresh(root).unwrap(), Freshness::default());
        drop_cached(root);
    }

    /// A page edited, added or deleted outside thClaws — in Obsidian, in
    /// vim — is reflected at the next search. The index used to hear only
    /// about thClaws' own writes, so deleted text stayed findable and
    /// added text never became so.
    #[test]
    fn edits_made_outside_thclaws_are_picked_up() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_page_file(root, "note", "the original wording");
        write_page_file(root, "doomed", "soon to be deleted");
        full_rebuild(root).unwrap();
        let idx = get_or_open(root).unwrap();
        assert_eq!(idx.search("original", &[], None, 5).unwrap().len(), 1);

        // Different length, so the stamp differs even within one second.
        write_page_file(
            root,
            "note",
            "completely rewritten text, in an external editor",
        );
        write_page_file(root, "fresh", "a page created by hand");
        std::fs::remove_file(root.join("pages/doomed.md")).unwrap();

        let report = ensure_fresh(root).unwrap();
        assert_eq!((report.updated, report.removed), (2, 1), "{report:?}");
        let idx = get_or_open(root).unwrap();
        assert!(idx.search("original", &[], None, 5).unwrap().is_empty());
        assert_eq!(idx.search("rewritten", &[], None, 5).unwrap().len(), 1);
        assert_eq!(idx.search("hand", &[], None, 5).unwrap().len(), 1);
        assert!(idx.search("deleted", &[], None, 5).unwrap().is_empty());
        assert_eq!(idx.num_docs().unwrap(), 2);
        drop_cached(root);
    }

    /// Searching must work while another process holds the write lock —
    /// the desktop window and each agent's `--serve` child share a vault.
    /// The writer used to be opened on the read path and kept for the
    /// life of the process, so the second process could not search at all.
    #[test]
    fn searching_works_while_another_process_holds_the_write_lock() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_page_file(root, "a", "alpha content");
        full_rebuild(root).unwrap();

        // "The other process": a second handle on the same directory,
        // holding a writer the way a long write would.
        let other = Index::open_in_dir(root.join(".index")).unwrap();
        let _held: IndexWriter = other.writer(WRITER_MEMORY_BUDGET).unwrap();

        let idx = get_or_open(root).unwrap();
        assert_eq!(
            idx.search("alpha", &[], None, 5).unwrap().len(),
            1,
            "read path needs no lock"
        );
        assert!(matches!(
            idx.upsert_page("b", &empty_fm(), "beta"),
            Err(IndexError::Busy)
        ));
        // A change on disk cannot be indexed right now; that is reported,
        // not an error, and the index still answers.
        write_page_file(root, "c", "gamma, written while locked out");
        let report = ensure_fresh(root).unwrap();
        assert!(report.busy, "{report:?}");
        assert_eq!(idx.search("alpha", &[], None, 5).unwrap().len(), 1);

        // Once the lock is free the next pass catches up.
        drop(_held);
        assert_eq!(ensure_fresh(root).unwrap().updated, 1);
        assert_eq!(
            get_or_open(root)
                .unwrap()
                .search("gamma", &[], None, 5)
                .unwrap()
                .len(),
            1
        );
        drop_cached(root);
    }

    /// This process does not sit on the lock between writes.
    #[test]
    fn the_write_lock_is_released_after_each_write() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let idx = get_or_open(root).unwrap();
        idx.upsert_page("a", &empty_fm(), "alpha").unwrap();
        let other = Index::open_in_dir(root.join(".index")).unwrap();
        let w: Result<IndexWriter, _> = other.writer(WRITER_MEMORY_BUDGET);
        assert!(w.is_ok(), "lock still held after a committed write");
        drop_cached(root);
    }

    /// Recall against a real vault, measured rather than argued. Ignored
    /// by default; point `KMS_RECALL_VAULT` at a KMS root and run with
    /// `--ignored --nocapture`. The vault is copied, never touched. Ground
    /// truth is a literal substring match over each page file — the same
    /// yardstick that put the previous index at 106/164 on these queries.
    #[test]
    #[ignore]
    fn recall_against_a_real_vault() {
        let Ok(vault) = std::env::var("KMS_RECALL_VAULT") else {
            eprintln!("set KMS_RECALL_VAULT to a KMS root");
            return;
        };
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        for dir in ["pages", "sources"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            for e in std::fs::read_dir(Path::new(&vault).join(dir))
                .unwrap()
                .flatten()
            {
                if e.path().is_file() {
                    std::fs::copy(e.path(), root.join(dir).join(e.file_name())).unwrap();
                }
            }
        }
        let n = full_rebuild(root).unwrap();
        let idx = get_or_open(root).unwrap();
        let pages: Vec<(String, String)> = std::fs::read_dir(root.join("pages"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
            .map(|e| {
                (
                    e.path().file_stem().unwrap().to_string_lossy().into_owned(),
                    std::fs::read_to_string(e.path()).unwrap(),
                )
            })
            .collect();
        eprintln!("indexed {n} documents, {} pages", pages.len());

        let queries = [
            "แรงงาน",
            "ตัวชี้วัด",
            "เศรษฐกิจ",
            "ค่าจ้าง",
            "นโยบาย",
            "เงินเฟ้อ",
            "อัตโนมัติ",
            "ประเทศไทย",
            "ความยากจน",
            "ครัวเรือน",
            "รายได้",
            "ผลิตภาพ",
            "ค่าครองชีพ",
            "มาตรฐานการครองชีพ",
            "ความฉลาด",
        ];
        let (mut want_total, mut got_total, mut extra_total) = (0usize, 0usize, 0usize);
        for q in queries {
            let truth: std::collections::BTreeSet<&str> = pages
                .iter()
                .filter(|(_, raw)| raw.contains(q))
                .map(|(stem, _)| stem.as_str())
                .collect();
            let hits: std::collections::BTreeSet<String> = idx
                .search_scoped(q, &[], None, 50, Some(DocKind::Page))
                .unwrap()
                .into_iter()
                .map(|h| h.page)
                .collect();
            let found = truth.iter().filter(|t| hits.contains(**t)).count();
            let missed: Vec<&&str> = truth.iter().filter(|t| !hits.contains(**t)).collect();
            let extra: Vec<&String> = hits
                .iter()
                .filter(|h| !truth.contains(h.as_str()))
                .collect();
            eprintln!(
                "{q:<22} truth {:>2}  found {:>2}  missed {missed:?}  extra {extra:?}",
                truth.len(),
                found
            );
            want_total += truth.len();
            got_total += found;
            extra_total += extra.len();
        }
        eprintln!("RECALL {got_total}/{want_total}; hits outside ground truth: {extra_total}");
        drop_cached(root);
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
