//! Research pipeline driver.
//!
//! Orchestrates one job from start to KMS write:
//!
//! ```text
//!   initial WebSearch (broad)
//!     ↓
//!   extract_subtopics (LLM)
//!     ↓
//!   ┌→ parallel research_subtopic (search → fetch top-N → accumulate)
//!   │   ↓
//!   │  evaluate (LLM → score)
//!   │   ↓
//!   │  if iter < min_iter OR (iter < max_iter AND score < threshold):
//!   │     extract_next_subtopics (LLM, conditioned on eval notes)
//!   └─────────────────────┘
//!     ↓
//!   synthesize (LLM → markdown + [N] citations)
//!     ↓
//!   kms_writer::write
//! ```
//!
//! All four LLM helpers and both search tools are passed in by trait
//! object so the pipeline is fully unit-testable with mocks. The
//! production wiring in M6.39.2 will instantiate real `Provider` +
//! `WebSearchTool` + `WebFetchTool` and pass them through.

use super::llm_calls::{self, ResearchSource};
use super::{kms_writer, manager, JobConfig};
use crate::cancel::CancelToken;
use crate::error::{Error, Result};
use crate::providers::Provider;
use std::sync::Arc;

/// In-memory cap per fetched source body. Research is for gathering
/// **complete** references — short caps defeat the purpose. 1 MB
/// holds a long wiki article + comments + a typical 50-page PDF text
/// extract, while still bounding pathological pages (e.g. an entire
/// site dumped to one URL). At 30 accumulated sources × 1 MB worst
/// case = ~30 MB peak in memory per run; comfortable for desktop.
///
/// Synth prompts still truncate via `snippet(body, N)` when
/// composing. Caps are chosen so each call comfortably fits a 200 K
/// context window while giving the model as much real article body
/// as possible:
/// - 3 KB / source for `extract_subtopics` (seed only)
/// - 15 KB / source for `evaluate` and `plan_pages` (40 sources × 15 KB
///   ≈ 150 K tokens — fits Sonnet's 200 K with headroom)
/// - 20 KB / source for the broad `synthesize` answer (30 sources)
/// - **60 KB / source** for `write_research_page` and `verify_page`
///   — these are per-page (~5 cited sources) so total stays well
///   under 100 K tokens, and 60 KB covers basically every scraped
///   article including long-form pieces.
///
/// 1 M context models have massive headroom on top of these caps;
/// 200 K context models also fit comfortably.
///
/// The 1 MB on-disk cap above bounds memory; per-call truncation
/// bounds per-prompt token spend.
const MAX_SOURCE_BODY_CHARS: usize = 1_000_000;
/// Per-fetch byte budget for `WebFetch` calls. Same 1 MB ceiling as
/// the in-memory cap so they don't conflict. M6.39.8: HAL-backed
/// `WebScrape` is preferred when `HAL_API_KEY` is set (returns
/// cleaner markdown); this constant only governs the WebFetch
/// fallback path.
const FETCH_MAX_BYTES: u64 = 1024 * 1024;
const SEED_SEARCH_RESULTS: u32 = 10;

/// Trait abstraction over WebSearch / WebFetch so the pipeline can be
/// tested with deterministic fakes. Production impl wraps the real
/// `tools::WebSearchTool` / `tools::WebFetchTool` (M6.39.2 wires
/// these). One trait, two methods, both async — keeps the abstraction
/// layer thin.
#[async_trait::async_trait]
pub trait ResearchTools: Send + Sync {
    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchHit>>;
    async fn fetch(&self, url: &str) -> Result<String>;
    /// Same as `search` but restricted to the past year where the
    /// backend supports it (Tavily `time_range`, Brave `freshness`).
    /// Default: plain search, so mocks need no change.
    async fn search_recent(&self, query: &str, max_results: u32) -> Result<Vec<SearchHit>> {
        self.search(query, max_results).await
    }
}

/// Minimal search-result shape the pipeline actually uses. Real
/// WebSearchTool returns Markdown — the production [`ResearchTools`]
/// impl parses that back into structured hits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Run one job to completion. Returns the relative raw-note filename
/// on success (caller surfaces as `JobView::result_page`).
pub async fn run(
    job_id: &str,
    query: String,
    config: JobConfig,
    provider: Arc<dyn Provider>,
    model: String,
    cancel: CancelToken,
) -> Result<String> {
    let tools = production_tools();
    run_with_tools(job_id, query, config, provider, model, cancel, tools).await
}

/// Test-injectable variant — same logic, takes the tools impl by Arc
/// so unit tests can pass a deterministic mock.
pub async fn run_with_tools(
    job_id: &str,
    query: String,
    config: JobConfig,
    provider: Arc<dyn Provider>,
    model: String,
    cancel: CancelToken,
    tools: Arc<dyn ResearchTools>,
) -> Result<String> {
    let mgr = manager();

    // ── 1. Initial broad search ─────────────────────────────────────
    check_alive(job_id, &cancel)?;
    mgr.update_phase(job_id, "iteration 1: initial search");
    let seed_hits = tools.search(&query, SEED_SEARCH_RESULTS).await?;
    let mut sources: Vec<ResearchSource> = Vec::new();
    accumulate(&mut sources, seed_hits, None);
    mgr.record_iteration(job_id, 1, sources.len() as u32, None);

    // ── 2. Extract initial subtopics ────────────────────────────────
    check_alive(job_id, &cancel)?;
    mgr.update_phase(job_id, "iteration 1: extracting subtopics");
    let mut subtopics = llm_calls::extract_subtopics(
        provider.as_ref(),
        &model,
        &query,
        &sources,
        config.subtopics_per_iter,
        config.llm_timeout,
        &cancel,
    )
    .await?;

    // last_score is now strictly per-iteration (recorded after each
    // evaluate then never read across iterations — the iter-start
    // broadcast passes `None` directly). last_notes still leaks past
    // the loop (used in the post-loop plan_pages pass).
    let mut last_notes = String::new();

    // ── 3. Iteration loop ───────────────────────────────────────────
    for iter in 1..=config.max_iter {
        check_alive(job_id, &cancel)?;
        if mgr.is_over_budget(job_id) {
            return Err(Error::Tool("research time budget exhausted".into()));
        }
        if subtopics.is_empty() {
            // LLM signaled "no further subtopics" — accept that as a
            // natural stop signal even if score < threshold.
            break;
        }
        mgr.update_phase(
            job_id,
            format!(
                "iteration {iter}/{}: searching {} subtopics",
                config.max_iter,
                subtopics.len()
            ),
        );

        // Parallel search across subtopics. Errors are tolerated — a
        // bad subtopic shouldn't kill the whole iteration.
        let mut hits_per_topic: Vec<Vec<SearchHit>> = Vec::with_capacity(subtopics.len());
        for st in &subtopics {
            check_alive(job_id, &cancel)?;
            match tools.search(st, config.subtopics_per_iter).await {
                Ok(hits) => hits_per_topic.push(hits),
                Err(_) => hits_per_topic.push(Vec::new()),
            }
        }

        // Fetch top-N from each subtopic's hits. Sequential within
        // subtopic + sequential across subtopics — keeps memory + HTTP
        // load predictable. Could parallelize later if needed.
        for hits in hits_per_topic {
            for hit in hits.into_iter().take(config.fetch_top_n as usize) {
                check_alive(job_id, &cancel)?;
                let body = match tools.fetch(&hit.url).await {
                    Ok(b) => b,
                    Err(_) => hit.snippet.clone(),
                };
                accumulate(
                    &mut sources,
                    vec![SearchHit {
                        title: hit.title,
                        url: hit.url,
                        snippet: body,
                    }],
                    None,
                );
            }
        }
        // Iter-start broadcast: this iter hasn't been evaluated yet
        // so we explicitly pass `None`, clearing any stale last_score
        // from the previous iter. The post-evaluate record_iteration
        // below carries this iter's real score. Without this, the
        // frontend per-iter history showed iter N labelled with iter
        // N-1's score (the broadcast snapshot at iter-start has
        // iterations_done=N and last_score=previous_eval).
        mgr.record_iteration(job_id, iter, sources.len() as u32, None);

        // ── 4. Evaluate ─────────────────────────────────────────────
        check_alive(job_id, &cancel)?;
        mgr.update_phase(
            job_id,
            format!("iteration {iter}/{}: evaluating coverage", config.max_iter),
        );
        let eval = llm_calls::evaluate(
            provider.as_ref(),
            &model,
            &query,
            &sources,
            config.llm_timeout,
            &cancel,
        )
        .await?;
        let last_score = Some(eval.score);
        last_notes = eval.notes.clone();
        mgr.record_iteration(job_id, iter, sources.len() as u32, last_score);

        // Stop conditions:
        // - hard floor: must run at least min_iter
        // - score threshold: between floor and ceiling
        // - hard ceiling: enforced by the for-loop bound
        let floor_passed = iter >= config.min_iter;
        let score_satisfied = eval.score >= config.score_threshold;
        if floor_passed && score_satisfied {
            break;
        }
        if iter == config.max_iter {
            break;
        }

        // ── 5. Generate next-round subtopics from eval notes ────────
        check_alive(job_id, &cancel)?;
        mgr.update_phase(
            job_id,
            format!("iteration {iter}/{}: planning next round", config.max_iter),
        );
        subtopics = llm_calls::extract_next_subtopics(
            provider.as_ref(),
            &model,
            &query,
            &sources,
            &eval.notes,
            config.subtopics_per_iter,
            config.llm_timeout,
            &cancel,
        )
        .await?;
    }

    // ── 6. Plan multi-page output ───────────────────────────────────
    check_alive(job_id, &cancel)?;
    mgr.update_phase(job_id, "planning KMS pages");
    let plan = llm_calls::plan_pages(
        provider.as_ref(),
        &model,
        &query,
        &sources,
        config.max_pages,
        config.llm_timeout,
        &cancel,
    )
    .await?;

    // ── 7. Resolve KMS name (auto-derive slug when --kms not passed)
    check_alive(job_id, &cancel)?;
    let kms_name = match &config.kms_target {
        Some(n) => n.clone(),
        None => {
            // M6.39.5: KMS name is the LLM-derived topic slug
            // verbatim, no `research-` prefix.
            llm_calls::derive_topic_slug(
                provider.as_ref(),
                &model,
                &query,
                config.llm_timeout,
                &cancel,
            )
            .await
            .unwrap_or_else(|_| "research".into())
        }
    };

    // Make sure the KMS exists before we kick off N parallel page
    // syntheses (each will write into it).
    let _ = kms_writer::resolve_or_create(&kms_name)?;

    // ── 8. Parallel per-page synthesis ──────────────────────────────
    check_alive(job_id, &cancel)?;
    mgr.update_phase(
        job_id,
        format!("synthesizing {} pages in parallel", plan.len()),
    );
    let today = kms_writer::today_str();
    // M6.39.11: simple_slug(query) used to feed the run-prefix in
    // page filenames; now bare slugs win (frontmatter has the
    // metadata). The variable is kept locally only if some downstream
    // step still needs it — none currently do.
    let _ = simple_slug(&query);

    let mut page_futures = Vec::with_capacity(plan.len());
    for page in &plan {
        let provider_ref = provider.clone();
        let model_owned = model.clone();
        let query_owned = query.clone();
        let page_owned = page.clone();
        let plan_owned = plan.clone();
        let sources_owned = sources.clone();
        let cancel_owned = cancel.clone();
        let timeout = config.llm_timeout;
        page_futures.push(tokio::spawn(crate::workdir::Scope::capture().run(
            async move {
                llm_calls::write_research_page(
                    provider_ref.as_ref(),
                    &model_owned,
                    &query_owned,
                    &page_owned,
                    &plan_owned,
                    &sources_owned,
                    timeout,
                    &cancel_owned,
                )
                .await
            },
        )));
    }
    let mut bodies: Vec<(usize, String)> = Vec::with_capacity(plan.len());
    for (idx, fut) in page_futures.into_iter().enumerate() {
        match fut.await {
            Ok(Ok(body)) => bodies.push((idx, body)),
            Ok(Err(e)) => return Err(e),
            Err(join_err) => {
                return Err(crate::error::Error::Tool(format!(
                    "page-synth task panicked: {join_err}"
                )));
            }
        }
    }

    // ── 9. Post-process + write each page ───────────────────────────
    check_alive(job_id, &cancel)?;
    mgr.update_phase(job_id, "writing pages to KMS");
    let mut all_cited_indices: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut last_page_path: Option<String> = None;
    // M6.39.7: shared (index, title, url) shape for the citation
    // helpers (ensure_sources_section + linkify_citations). Built
    // once per run; same for every page.
    let sources_meta: Vec<(u32, String, String, String)> = sources
        .iter()
        .map(|s| (s.index, s.title.clone(), s.url.clone(), String::new()))
        .collect();
    for (idx, body) in bodies {
        let page = &plan[idx];
        // M6.39.11: no cross-link rewriter — page filenames are bare
        // slugs now, so `[[karpathy]]` resolves directly.
        let mut rewritten = body;
        if !last_notes.is_empty() && idx == 0 {
            // Keep the iteration-evaluation notes attached to the
            // first page as a research-context appendix. Only the
            // first page gets it to avoid duplication across all
            // pages from the same run.
            rewritten.push_str("\n\n---\n\n## Research notes\n\n");
            rewritten.push_str(&last_notes);
        }
        // M6.39.7: deterministically rebuild the `## Sources` section
        // from actual `[N]` usage (LLM was inconsistent — sometimes
        // wrote a partial section, sometimes none). Then linkify
        // every inline `[N]` so it points at the cached source file
        // in <kms>/sources/. Order matters: ensure_sources_section
        // runs first so the regenerated section's `N.` numbered-list
        // entries don't get touched by the linkifier (which only
        // rewrites `[N]` patterns).
        rewritten = kms_writer::ensure_sources_section(&rewritten, &sources_meta);
        rewritten = kms_writer::linkify_citations(&rewritten, &sources_meta);

        // Accumulate cited indices across all pages so sources/ is
        // populated with every cited source from the run, not just
        // one page's.
        let cited = kms_writer::parse_citation_indices(&rewritten);
        all_cited_indices.extend(cited.iter().copied());

        // Verify pass — addresses the "LLM Wiki bakes in organised
        // persistent mistakes" critique. Each generated page is
        // audited against its cited sources before being written to
        // the KMS. Flagged claims (Partial / Unsupported /
        // NoCitation) get appended as a `## Verification` section
        // and the per-page `verification_score` lands in
        // frontmatter so readers / future verify passes can sort
        // pages by trustworthiness. Soft-failure: a verifier error
        // logs + records score=None rather than blowing up the
        // research run.
        let cited_sources: Vec<llm_calls::ResearchSource> = sources
            .iter()
            .filter(|s| cited.contains(&s.index))
            .cloned()
            .collect();
        let verification_score = if cited_sources.is_empty() {
            // Nothing cited → nothing to verify. Skip the LLM call
            // and write the page without a score (rather than
            // stamping a misleading 0.0).
            None
        } else {
            check_alive(job_id, &cancel)?;
            mgr.update_phase(job_id, format!("verifying page {}/{}", idx + 1, plan.len()));
            match llm_calls::verify_page(
                provider.as_ref(),
                &model,
                &rewritten,
                &cited_sources,
                config.llm_timeout,
                &cancel,
            )
            .await
            {
                Ok(report) => {
                    if let Some(section) = report.render_flagged_section() {
                        rewritten.push_str(&section);
                    }
                    Some(report.score)
                }
                Err(e) => {
                    eprintln!(
                        "[research] verify pass failed for {}: {e} — writing page without verification record",
                        page.slug
                    );
                    None
                }
            }
        };

        let path = kms_writer::write_research_page(
            &kms_name,
            &page.slug,
            &page.title,
            &page.topic,
            &query,
            &today,
            &rewritten,
            verification_score,
        )?;
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            last_page_path = Some(name.to_string());
        }
    }

    // ── 10. Append run section to _summary.md ───────────────────────
    let pages_for_summary: Vec<(String, String, String)> = plan
        .iter()
        .map(|p| (p.slug.clone(), p.title.clone(), p.topic.clone()))
        .collect();
    kms_writer::append_run_section(&kms_name, &today, &query, &pages_for_summary)?;

    // ── 11. Persist cited sources to <kms>/sources/ ─────────────────
    for s in &sources {
        if all_cited_indices.contains(&s.index) {
            let _ = kms_writer::write_source(
                &kms_name, &query, &today, s.index, &s.title, &s.url, &s.body,
            );
        }
    }

    // result_page = filename of the last-written page so /research
    // show <id> opens something meaningful. The summary section in
    // _summary.md links to all pages from this run.
    let result = match last_page_path {
        Some(name) => format!("{kms_name}/{name}"),
        None => format!("{kms_name}/_summary.md"),
    };
    Ok(result)
}

// ── Helpers ──────────────────────────────────────────────────────────

fn check_alive(job_id: &str, cancel: &CancelToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(Error::Tool("research job cancelled".into()));
    }
    if manager().is_over_budget(job_id) {
        return Err(Error::Tool("research time budget exhausted".into()));
    }
    Ok(())
}

/// Append hits to the running source list, deduping by URL and
/// truncating bodies to `MAX_SOURCE_BODY_CHARS`. `start_index_hint` is
/// reserved for future explicit indexing; today we always assign next
/// available `index` (1-based, monotonic).
fn accumulate(
    sources: &mut Vec<ResearchSource>,
    hits: Vec<SearchHit>,
    _start_index_hint: Option<u32>,
) {
    for hit in hits {
        if hit.url.is_empty() {
            continue;
        }
        if sources.iter().any(|s| s.url == hit.url) {
            // Dedupe on URL — same page from different subtopic
            // searches doesn't get a second citation slot.
            continue;
        }
        let next_idx = sources.len() as u32 + 1;
        let body = if hit.snippet.len() > MAX_SOURCE_BODY_CHARS {
            let mut end = MAX_SOURCE_BODY_CHARS;
            while !hit.snippet.is_char_boundary(end) && end > 0 {
                end -= 1;
            }
            format!("{}…", &hit.snippet[..end])
        } else {
            hit.snippet
        };
        sources.push(ResearchSource {
            index: next_idx,
            title: hit.title,
            url: hit.url,
            body,
        });
    }
}

/// Stop-gap query→slug used for the raw-note filename when we need
/// something filesystem-safe before the LLM-derived `topic_slug` (used
/// for KMS naming). Just lowercase + collapse non-alphanumeric to `-`.
/// The KMS name itself uses [`llm_calls::derive_topic_slug`] for a
/// nicer multilingual result.
fn simple_slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "query".into()
    } else {
        // Keep filenames reasonable.
        trimmed.chars().take(60).collect()
    }
}

/// Production wiring (M6.39.2). Wraps the real `WebSearchTool` +
/// `WebFetchTool` from the agent's tool registry, parsing
/// WebSearch's Markdown response back into structured hits.
pub fn production_tools() -> Arc<dyn ResearchTools> {
    Arc::new(ProductionTools::new())
}

pub struct ProductionTools {
    search: crate::tools::WebSearchTool,
    fetch: crate::tools::WebFetchTool,
    /// M6.39.8: HAL-backed web scraper, used when `HAL_API_KEY` is
    /// set. Returns clean Markdown directly from HAL's headless-browser
    /// scrape — far better archive quality than WebFetch's HTML→
    /// Markdown conversion. Falls back to `WebFetchTool` when the key
    /// isn't available or the call fails.
    scrape: crate::tools::WebScrapeTool,
}

impl ProductionTools {
    pub fn new() -> Self {
        Self {
            search: crate::tools::WebSearchTool::default(),
            fetch: crate::tools::WebFetchTool::new(),
            scrape: crate::tools::WebScrapeTool::new(),
        }
    }
}

impl Default for ProductionTools {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ResearchTools for ProductionTools {
    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchHit>> {
        use crate::tools::Tool;
        let raw = self
            .search
            .call(serde_json::json!({
                "query": query,
                "max_results": max_results,
            }))
            .await?;
        Ok(parse_websearch_markdown(&raw))
    }
    async fn search_recent(&self, query: &str, max_results: u32) -> Result<Vec<SearchHit>> {
        use crate::tools::Tool;
        let raw = self
            .search
            .call(serde_json::json!({
                "query": query,
                "max_results": max_results,
                "freshness": "year",
            }))
            .await?;
        Ok(parse_websearch_markdown(&raw))
    }
    async fn fetch(&self, url: &str) -> Result<String> {
        use crate::tools::Tool;
        // M6.39.8: HAL-first with WebFetch fallback. Research is for
        // gathering complete references — truncation defeats the
        // purpose, and HAL's scrape returns better-structured markdown
        // than WebFetch's HTML conversion. Try HAL when its key is
        // available; on failure or absence, fall back to WebFetch.
        if hal_key_available() {
            match self.scrape.call(serde_json::json!({"url": url})).await {
                Ok(json_str) => {
                    if let Some(markdown) = extract_hal_content(&json_str) {
                        return Ok(markdown);
                    }
                    // HAL returned but parse failed — fall through to
                    // WebFetch rather than silently surfacing the JSON
                    // envelope as the source body.
                }
                Err(_) => {
                    // HAL failed (rate limit, network, etc.) — fall
                    // through to WebFetch. Best-effort archive: the
                    // synth prompt only needs SOMETHING; sources/ is
                    // a useful-but-secondary cache.
                }
            }
        }
        // WebFetch path: pass FETCH_MAX_BYTES as the byte budget.
        // Synth prompts further truncate via `snippet(body, N)`, so
        // larger fetched bodies don't bloat LLM cost. Sources/
        // archive gets the full markdown.
        self.fetch
            .call(serde_json::json!({
                "url": url,
                "max_bytes": FETCH_MAX_BYTES,
            }))
            .await
    }
}

/// Check whether `HAL_API_KEY` is set + non-empty in the live process
/// env. Pulled out of `fetch` so the same logic is testable.
fn hal_key_available() -> bool {
    std::env::var("HAL_API_KEY")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Extract the `content` field (Markdown) out of HAL's
/// `/scrape/v1/url` JSON response. The `WebScrapeTool` returns the
/// full envelope as pretty-printed JSON; we want just the markdown
/// for the sources/ archive. Returns `None` if the JSON doesn't have
/// a `content` field — caller falls back to WebFetch.
fn extract_hal_content(json_str: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let content = v.get("content").and_then(serde_json::Value::as_str)?;
    if content.trim().is_empty() {
        return None;
    }
    // Optionally prepend the title as an H1 so the source file has a
    // nice header. HAL's content sometimes already includes it; keep
    // it simple and just return content verbatim.
    Some(content.to_string())
}

/// Parse `WebSearchTool`'s Markdown output back into structured hits.
/// Output shape (M6.38.8):
/// ```text
/// Source: Tavily (web search)
///
/// Answer: ...
///
/// 1. Title (https://url)
///    snippet text
/// 2. Title (https://url)
///    snippet text
/// ```
///
/// We skip the leading `Source:` header + optional `Answer:` block,
/// then walk numbered entries. A line like `12. Some title (https://x)`
/// starts a new hit; subsequent indented lines are appended as snippet
/// body.
fn parse_websearch_markdown(body: &str) -> Vec<SearchHit> {
    let mut hits: Vec<SearchHit> = Vec::new();
    let mut current: Option<SearchHit> = None;
    let mut snippet_buf = String::new();

    for line in body.lines() {
        let trimmed = line.trim();
        // Skip header / empty lines / "Answer:" preamble.
        if trimmed.starts_with("Source:") || trimmed.starts_with("Answer:") {
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if let Some((title, url)) = parse_numbered_entry(trimmed) {
            if let Some(mut prev) = current.take() {
                prev.snippet = std::mem::take(&mut snippet_buf).trim().to_string();
                hits.push(prev);
            }
            current = Some(SearchHit {
                title,
                url,
                snippet: String::new(),
            });
            snippet_buf.clear();
        } else if current.is_some() {
            // Indented continuation = snippet body.
            if !snippet_buf.is_empty() {
                snippet_buf.push(' ');
            }
            snippet_buf.push_str(trimmed);
        }
    }
    if let Some(mut last) = current {
        last.snippet = snippet_buf.trim().to_string();
        hits.push(last);
    }
    hits
}

/// Recognize `<number>. <title> (<url>)` — the WebSearch entry shape.
/// Returns `(title, url)` on a hit, `None` otherwise. `url` validated
/// loosely (must look like http(s)).
fn parse_numbered_entry(line: &str) -> Option<(String, String)> {
    let mut iter = line.char_indices();
    // Consume leading digits + dot.
    let mut last_digit_end = 0;
    let mut saw_digit = false;
    let mut saw_dot = false;
    for (i, c) in iter.by_ref() {
        if c.is_ascii_digit() {
            last_digit_end = i + c.len_utf8();
            saw_digit = true;
        } else if c == '.' && saw_digit {
            saw_dot = true;
            break;
        } else {
            return None;
        }
    }
    // The loop ends either on the dot or by running out of line, and only
    // the first is a numbered entry. Without `saw_dot`, a line of nothing
    // but digits fell through with `last_digit_end` at the end of the
    // string and indexed one past it — `"11"` asked for byte 3 of 2. That
    // panic unwound the whole research job, which then hung as Running
    // with nothing to show for it.
    if !saw_digit || !saw_dot {
        return None;
    }
    let after_dot = line[last_digit_end + 1..].trim_start();
    // Find the last `(http...)` — title is everything before, URL is
    // inside the parens.
    let open = after_dot.rfind(" (http")?;
    let title = after_dot[..open].trim().to_string();
    let url_with_close = &after_dot[open + 2..];
    if !url_with_close.ends_with(')') {
        return None;
    }
    let url = url_with_close[..url_with_close.len() - 1].to_string();
    if title.is_empty() {
        return None;
    }
    Some((title, url))
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{EventStream, Provider, ProviderEvent, StreamRequest};
    use crate::types::Message;
    use async_trait::async_trait;
    use futures::stream;
    use std::sync::Mutex;

    /// Mock provider that returns a queue of canned responses, one per
    /// `stream()` call. Each canned response is a single TextDelta
    /// followed by MessageStop.
    struct MockProvider {
        responses: Mutex<Vec<String>>,
        calls: Mutex<u32>,
    }

    impl MockProvider {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().map(String::from).collect()),
                calls: Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn stream(&self, _req: StreamRequest) -> Result<EventStream> {
            *self.calls.lock().unwrap() += 1;
            let mut q = self.responses.lock().unwrap();
            let body = if q.is_empty() {
                "score: 0.9\nfully covered".into()
            } else {
                q.remove(0)
            };
            let events: Vec<Result<ProviderEvent>> = vec![
                Ok(ProviderEvent::MessageStart {
                    model: "mock".into(),
                }),
                Ok(ProviderEvent::TextDelta(body)),
                Ok(ProviderEvent::MessageStop {
                    stop_reason: Some("end_turn".into()),
                    usage: None,
                }),
            ];
            Ok(Box::pin(stream::iter(events)))
        }
    }

    /// Mock tools that return canned hits. Counts search/fetch calls.
    struct MockTools {
        hits_per_query: Mutex<Vec<Vec<SearchHit>>>,
        fetch_returns: Mutex<Vec<String>>,
        search_calls: Mutex<u32>,
        fetch_calls: Mutex<u32>,
    }

    impl MockTools {
        fn new(hits: Vec<Vec<SearchHit>>, fetch_returns: Vec<&str>) -> Self {
            Self {
                hits_per_query: Mutex::new(hits),
                fetch_returns: Mutex::new(fetch_returns.into_iter().map(String::from).collect()),
                search_calls: Mutex::new(0),
                fetch_calls: Mutex::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl ResearchTools for MockTools {
        async fn search(&self, _query: &str, _max: u32) -> Result<Vec<SearchHit>> {
            *self.search_calls.lock().unwrap() += 1;
            let mut q = self.hits_per_query.lock().unwrap();
            if q.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(q.remove(0))
            }
        }
        async fn fetch(&self, _url: &str) -> Result<String> {
            *self.fetch_calls.lock().unwrap() += 1;
            let mut q = self.fetch_returns.lock().unwrap();
            if q.is_empty() {
                Ok("body content".into())
            } else {
                Ok(q.remove(0))
            }
        }
    }

    fn hit(title: &str, url: &str) -> SearchHit {
        SearchHit {
            title: title.into(),
            url: url.into(),
            snippet: format!("snippet for {title}"),
        }
    }

    use crate::research::test_helpers::scoped_home;

    /// Happy path: one query, score ≥ 0.75 after iter 2 (hard floor),
    /// loop stops, synthesizes, writes to KMS.
    #[tokio::test]
    async fn happy_path_stops_at_floor_and_writes_kms() {
        let _g = scoped_home();
        let provider = Arc::new(MockProvider::new(vec![
            // iter 1 extract_subtopics
            "1. subtopic-a\n2. subtopic-b",
            // iter 1 evaluate (low score → continue)
            "score: 0.5\nneeds more sources",
            // iter 1 extract_next_subtopics
            "1. subtopic-c\n2. subtopic-d",
            // iter 2 evaluate (high score → stop)
            "score: 0.85\ncovered",
            // M6.39.6: plan_pages — single-page plan to keep test small
            r#"[{"slug":"main","title":"Main","topic":"top-level","source_indices":[1,2]}]"#,
            // derive_topic_slug
            "test-topic",
            // write_research_page (one page)
            "Main page abstract [1].\n\n## Body\nMore here [2].\n\n## Sources\n[1] https://a.example\n[2] https://b.example",
        ]));
        let tools = Arc::new(MockTools::new(
            vec![
                vec![
                    hit("Seed A", "https://a.example"),
                    hit("Seed B", "https://b.example"),
                ],
                vec![hit("Topic A", "https://c.example")],
                vec![hit("Topic B", "https://d.example")],
                vec![hit("Topic C", "https://e.example")],
                vec![hit("Topic D", "https://f.example")],
            ],
            vec![
                "page A body",
                "page B body",
                "page C body",
                "page D body",
                "page E body",
            ],
        ));
        let cfg = JobConfig {
            min_iter: 2,
            max_iter: 8,
            score_threshold: 0.75,
            subtopics_per_iter: 2,
            fetch_top_n: 1,
            max_pages: 7,
            llm_timeout: std::time::Duration::from_secs(5),
            time_budget: std::time::Duration::from_secs(60),
            kms_target: None,
            ..JobConfig::default()
        };
        let cancel = CancelToken::new();
        let (id, _) = manager().register("test query".into(), &cfg);
        let outcome = run_with_tools(
            &id,
            "test query".into(),
            cfg,
            provider.clone(),
            "mock-model".into(),
            cancel,
            tools.clone(),
        )
        .await
        .unwrap();
        // M6.39.5: KMS name is the LLM-derived slug verbatim, no
        // `research-` prefix. The mock provider returns "test-topic"
        // as its derive_topic_slug response.
        assert!(
            outcome.starts_with("test-topic/"),
            "outcome should be `<slug>/<filename>.md`, got: {outcome}"
        );
        assert!(
            !outcome.contains("research-test-topic"),
            "KMS name should not be prefixed with `research-`, got: {outcome}"
        );
        assert!(outcome.ends_with(".md"));
    }

    /// Hard floor enforcement: even if iter 1 scores ≥ threshold, the
    /// loop must run min_iter rounds.
    #[tokio::test]
    async fn hard_floor_blocks_early_exit() {
        let _g = scoped_home();
        let provider = Arc::new(MockProvider::new(vec![
            "1. topic-a\n2. topic-b",      // extract_subtopics (iter 1)
            "score: 0.95\nlooks complete", // evaluate (iter 1) — high but floor blocks
            "1. topic-c\n2. topic-d",      // extract_next_subtopics
            "score: 0.95\nstill complete", // evaluate (iter 2) — now passes
            // M6.39.6: plan_pages — single-page plan
            r#"[{"slug":"main","title":"Main","topic":"top","source_indices":[1]}]"#,
            "topic",       // derive_topic_slug
            "Body [1].\n", // write_research_page (1 page)
        ]));
        let tools = Arc::new(MockTools::new(
            vec![vec![hit("S", "https://s.example")]; 5],
            vec!["b"; 10],
        ));
        let cfg = JobConfig {
            min_iter: 2,
            max_iter: 5,
            score_threshold: 0.75,
            subtopics_per_iter: 2,
            fetch_top_n: 1,
            max_pages: 7,
            llm_timeout: std::time::Duration::from_secs(5),
            time_budget: std::time::Duration::from_secs(60),
            kms_target: None,
            ..JobConfig::default()
        };
        let cancel = CancelToken::new();
        let (id, _) = manager().register("query".into(), &cfg);
        let outcome = run_with_tools(
            &id,
            "query".into(),
            cfg,
            provider.clone(),
            "mock-model".into(),
            cancel,
            tools,
        )
        .await
        .unwrap();
        // Hard floor honored — both iterations ran even though iter 1 score was high.
        let v = manager().get(&id).unwrap();
        assert_eq!(v.iterations_done, 2);
        assert!(outcome.starts_with("topic/"));
    }

    /// Hard ceiling enforcement: low score for many iterations, loop
    /// must stop at max_iter.
    #[tokio::test]
    async fn hard_ceiling_stops_at_max_iter() {
        let _g = scoped_home();
        let mut canned: Vec<&str> = Vec::new();
        canned.push("1. t-a\n2. t-b"); // initial extract
        for _ in 0..3 {
            canned.push("score: 0.2\nstill missing things"); // evaluate
            canned.push("1. more-a\n2. more-b"); // next subtopics
        }
        canned.pop(); // last iteration doesn't get next-subtopics call (loop exits after eval)
        canned.push(r#"[{"slug":"x","title":"X","topic":"t","source_indices":[1]}]"#); // plan_pages
        canned.push("topic"); // derive_topic_slug
        canned.push("Body [1].\n"); // write_research_page (1 page)
        let provider = Arc::new(MockProvider::new(canned));
        let tools = Arc::new(MockTools::new(
            vec![vec![hit("S", "https://s.example")]; 10],
            vec!["b"; 10],
        ));
        let cfg = JobConfig {
            min_iter: 2,
            max_iter: 3,
            score_threshold: 0.75,
            subtopics_per_iter: 2,
            fetch_top_n: 1,
            max_pages: 7,
            llm_timeout: std::time::Duration::from_secs(5),
            time_budget: std::time::Duration::from_secs(60),
            kms_target: None,
            ..JobConfig::default()
        };
        let cancel = CancelToken::new();
        let (id, _) = manager().register("query".into(), &cfg);
        run_with_tools(
            &id,
            "query".into(),
            cfg,
            provider.clone(),
            "mock-model".into(),
            cancel,
            tools,
        )
        .await
        .unwrap();
        let v = manager().get(&id).unwrap();
        assert_eq!(v.iterations_done, 3);
    }

    /// Cancellation: cancel before first iteration, pipeline returns
    /// Err so the spawn task marks Cancelled.
    #[tokio::test]
    async fn cancel_before_loop_returns_error() {
        let _g = scoped_home();
        let provider = Arc::new(MockProvider::new(vec!["1. a\n2. b"]));
        let tools = Arc::new(MockTools::new(
            vec![vec![hit("S", "https://s.example")]],
            vec![],
        ));
        let cfg = JobConfig::default();
        let cancel = CancelToken::new();
        cancel.cancel();
        let (id, _) = manager().register("q".into(), &cfg);
        let err = run_with_tools(&id, "q".into(), cfg, provider, "mock".into(), cancel, tools)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("cancelled"));
    }

    /// Empty next-subtopics list = LLM signals "no more to search" =
    /// natural stop, even if score is below threshold.
    #[tokio::test]
    async fn empty_next_subtopics_breaks_loop() {
        let _g = scoped_home();
        let provider = Arc::new(MockProvider::new(vec![
            "1. a\n2. b",                                                     // extract initial
            "score: 0.4\nincomplete but stuck",                               // eval (iter 1)
            "", // next subtopics — empty
            r#"[{"slug":"x","title":"X","topic":"t","source_indices":[1]}]"#, // plan_pages
            "slug", // derive_topic_slug
            "Final synth body [1].\n", // write_research_page
        ]));
        let tools = Arc::new(MockTools::new(
            vec![vec![hit("S", "https://s.example")]; 5],
            vec!["b"; 10],
        ));
        let cfg = JobConfig {
            min_iter: 1, // floor=1 so iter 1 can early-exit if next-subtopics empty
            max_iter: 8,
            score_threshold: 0.9,
            subtopics_per_iter: 2,
            fetch_top_n: 1,
            max_pages: 7,
            llm_timeout: std::time::Duration::from_secs(5),
            time_budget: std::time::Duration::from_secs(60),
            kms_target: None,
            ..JobConfig::default()
        };
        let cancel = CancelToken::new();
        let (id, _) = manager().register("q".into(), &cfg);
        run_with_tools(&id, "q".into(), cfg, provider, "mock".into(), cancel, tools)
            .await
            .unwrap();
    }

    #[test]
    fn accumulate_dedupes_by_url() {
        let mut sources = Vec::new();
        accumulate(
            &mut sources,
            vec![
                hit("A", "https://a.example"),
                hit("B", "https://b.example"),
                hit("A again", "https://a.example"),
            ],
            None,
        );
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].index, 1);
        assert_eq!(sources[1].index, 2);
    }

    #[test]
    fn accumulate_keeps_bodies_up_to_max_cap() {
        // M6.39.7: cap is now 100 KB so sources/ archive files have
        // meaningful content, not 8 KB / 1500 chars of fragment.
        // Bodies under the cap are kept intact; ones over get
        // truncated with `…`.
        let mut sources = Vec::new();
        let under_cap = "x".repeat(10_000);
        accumulate(
            &mut sources,
            vec![SearchHit {
                title: "under".into(),
                url: "https://under.example".into(),
                snippet: under_cap.clone(),
            }],
            None,
        );
        assert_eq!(sources[0].body, under_cap, "under-cap body untouched");
        assert!(!sources[0].body.ends_with('…'));
        // Now an over-cap body to confirm truncation still kicks in.
        let big_body = "y".repeat(MAX_SOURCE_BODY_CHARS + 5_000);
        accumulate(
            &mut sources,
            vec![SearchHit {
                title: "big".into(),
                url: "https://big.example".into(),
                snippet: big_body,
            }],
            None,
        );
        // The big-body source is the second one we appended.
        let big = sources.iter().find(|s| s.title == "big").unwrap();
        assert!(big.body.len() <= MAX_SOURCE_BODY_CHARS + 4); // + 4 for "…" UTF-8
        assert!(big.body.ends_with('…'));
    }

    #[test]
    fn accumulate_skips_empty_urls() {
        let mut sources = Vec::new();
        accumulate(
            &mut sources,
            vec![SearchHit {
                title: "no url".into(),
                url: "".into(),
                snippet: "body".into(),
            }],
            None,
        );
        assert!(sources.is_empty());
    }

    #[test]
    fn simple_slug_basic() {
        assert_eq!(simple_slug("Hello World"), "hello-world");
        assert_eq!(simple_slug("foo  --  bar"), "foo-bar");
        assert_eq!(simple_slug("  trim  "), "trim");
        assert_eq!(simple_slug("###"), "query"); // fallback
    }

    #[test]
    fn simple_slug_caps_at_60() {
        let long_query = "the quick brown fox jumps over the lazy dog and then continues running through the forest";
        assert!(simple_slug(long_query).len() <= 60);
    }

    #[test]
    fn production_tools_constructs() {
        // Smoke test only — actual search/fetch require network +
        // configured keys, exercised via end-to-end manual testing.
        let _t = production_tools();
    }

    /// M6.39.8: HAL-key detection helper. The fetch path checks this
    /// to decide between HAL `WebScrape` (preferred when key is set)
    /// and `WebFetch` (fallback).
    #[test]
    fn hal_key_available_reflects_env() {
        // HAL_API_KEY is read by the prompt builder's services section,
        // so this test must serialise against the crate-wide env lock —
        // pre-fix it ran lockless and intermittently flipped HAL_API_KEY
        // out from under the prompt-builder test in repl::tests.
        let _g = crate::kms::test_env_lock();
        let prev = std::env::var("HAL_API_KEY").ok();

        std::env::remove_var("HAL_API_KEY");
        assert!(!hal_key_available(), "no var → unavailable");

        std::env::set_var("HAL_API_KEY", "");
        assert!(!hal_key_available(), "empty var → unavailable");

        std::env::set_var("HAL_API_KEY", "   ");
        assert!(!hal_key_available(), "whitespace-only var → unavailable");

        std::env::set_var("HAL_API_KEY", "hal_realkey");
        assert!(hal_key_available(), "non-empty → available");

        match prev {
            Some(v) => std::env::set_var("HAL_API_KEY", v),
            None => std::env::remove_var("HAL_API_KEY"),
        }
    }

    /// M6.39.8: parser for HAL's `/scrape/v1/url` envelope. The
    /// pipeline pulls just the `content` markdown out — the full
    /// envelope (title, url, metadata, scraped_at) wraps it but
    /// research only wants the body for sources/.
    #[test]
    fn extract_hal_content_pulls_markdown_field() {
        // Build the envelope at runtime so escape sequences are real.
        let envelope = serde_json::json!({
            "url": "https://example.com",
            "title": "Example",
            "content": "# Example\n\nFull markdown body here.",
            "metadata": {},
            "scraped_at": "2026-05-09T10:00:00Z"
        })
        .to_string();
        let extracted = extract_hal_content(&envelope).unwrap();
        assert_eq!(extracted, "# Example\n\nFull markdown body here.");
    }

    #[test]
    fn extract_hal_content_returns_none_on_empty_content() {
        let envelope = serde_json::json!({"content": ""}).to_string();
        assert!(extract_hal_content(&envelope).is_none());
        let envelope_ws = serde_json::json!({"content": "   \n  "}).to_string();
        assert!(extract_hal_content(&envelope_ws).is_none());
    }

    #[test]
    fn extract_hal_content_returns_none_on_missing_field() {
        let envelope = r#"{"url": "x", "title": "t"}"#;
        assert!(extract_hal_content(envelope).is_none());
    }

    #[test]
    fn extract_hal_content_returns_none_on_bad_json() {
        assert!(extract_hal_content("not json").is_none());
        assert!(extract_hal_content("").is_none());
    }

    #[test]
    fn parse_numbered_entry_basic() {
        let (t, u) = parse_numbered_entry("1. Hello world (https://example.com/a)").unwrap();
        assert_eq!(t, "Hello world");
        assert_eq!(u, "https://example.com/a");
    }

    #[test]
    fn parse_numbered_entry_multidigit_index() {
        let (t, u) = parse_numbered_entry("12. Title (https://x.example)").unwrap();
        assert_eq!(t, "Title");
        assert_eq!(u, "https://x.example");
    }

    #[test]
    fn parse_numbered_entry_url_with_path_and_query() {
        let (t, u) = parse_numbered_entry(
            "3. Some long title with parens (extra) (https://example.com/path?q=1&r=2)",
        )
        .unwrap();
        assert_eq!(t, "Some long title with parens (extra)");
        assert_eq!(u, "https://example.com/path?q=1&r=2");
    }

    #[test]
    fn parse_numbered_entry_rejects_non_numbered() {
        assert!(parse_numbered_entry("Hello world").is_none());
        assert!(parse_numbered_entry("- bullet (https://x.example)").is_none());
        assert!(parse_numbered_entry("1. no url here").is_none());
    }

    /// A line of nothing but digits used to index one byte past the end
    /// of the line and panic. Because the digests all run on one task
    /// under `join_all`, that panic took the whole research job with it —
    /// and it unwound past the job's finalize, so the run sat `Running`
    /// for hours with no error anywhere. Seen twice in one session on a
    /// real search result: `byte index 3 is out of bounds of "11"`.
    #[test]
    fn a_line_of_only_digits_is_not_an_entry() {
        for line in ["11", "3", "0", "999999", " 42 ", "7\t"] {
            assert!(
                parse_numbered_entry(line.trim()).is_none(),
                "bare digits must not parse: {line:?}"
            );
        }
        // The dot alone is not enough either — there has to be a URL.
        assert!(parse_numbered_entry("11.").is_none());
        assert!(parse_numbered_entry("11. ").is_none());
        // And the normal shape still parses.
        let (t, u) = parse_numbered_entry("11. Title (https://x.example)").unwrap();
        assert_eq!(t, "Title");
        assert_eq!(u, "https://x.example");
    }

    #[test]
    fn parse_websearch_markdown_full_output() {
        let body = "Source: Tavily (web search)\n\
                    \n\
                    Answer: Some synthesized answer.\n\
                    \n\
                    1. First title (https://a.example)\n   \
                    First snippet line\n   \
                    second snippet line\n\
                    2. Second title (https://b.example)\n   \
                    Second snippet";
        let hits = parse_websearch_markdown(body);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "First title");
        assert_eq!(hits[0].url, "https://a.example");
        assert!(hits[0].snippet.contains("First snippet line"));
        assert!(hits[0].snippet.contains("second snippet line"));
        assert_eq!(hits[1].title, "Second title");
        assert_eq!(hits[1].url, "https://b.example");
    }

    #[test]
    fn parse_websearch_markdown_no_results() {
        let body = "Source: DuckDuckGo (web search)\n\nNo results found.";
        let hits = parse_websearch_markdown(body);
        assert!(hits.is_empty());
    }

    #[test]
    fn parse_websearch_markdown_after_fallback() {
        let body = "Source: DuckDuckGo (web search) — fallback after tavily: HTTP 429\n\
                    \n\
                    1. Only title (https://x.example)\n   \
                    snippet";
        let hits = parse_websearch_markdown(body);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Only title");
    }

    #[allow(dead_code)]
    fn touch_message_assoc(_: Message) {}
}
