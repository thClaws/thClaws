//! Non-streaming LLM helpers for the research pipeline.
//!
//! Each function below is a single LLM call: build a prompt, drain the
//! `Provider::stream()` for `TextDelta` events into a String, parse,
//! return a typed result. No tool use, no agent loop, no message
//! history — research uses the LLM as a pure callable.
//!
//! The 4 calls per iteration:
//! - [`extract_subtopics`] — query + seed → initial subtopics list
//! - [`extract_next_subtopics`] — query + accumulated + eval notes →
//!   next-round subtopics (close gaps named in notes)
//! - [`evaluate`] — query + accumulated → completeness score (0-1) +
//!   free-form notes
//! - [`synthesize`] — query + accumulated → final markdown with `[N]`
//!   citations + URL list
//!
//! All four are robust to LLM weirdness: if parsing fails, fall back
//! to a sensible default rather than aborting the pipeline. The score
//! parser accepts both `score: 0.7` and `Score: 0.7` (any case) on the
//! first non-empty line; the subtopics parsers accept `1.` or `-` or
//! `*` bullets.
//!
//! Per-call timeout is enforced via `tokio::time::timeout` — provider
//! hangs don't stall the whole pipeline.

use crate::cancel::CancelToken;
use crate::error::{Error, Result};
use crate::providers::{Provider, ProviderEvent, StreamRequest};
use crate::types::Message;
use futures::StreamExt;
use std::time::Duration;

/// One search result harvested by the pipeline. Pared down from the
/// full WebSearch tool output to what the LLM needs for synthesis.
/// Indexed citations refer to entries in this list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResearchSource {
    /// `1`-based citation index, stable for the lifetime of the job.
    pub index: u32,
    pub title: String,
    pub url: String,
    /// Combined snippet + (if WebFetch was run) extracted body excerpt.
    /// Truncated by the pipeline to `MAX_SOURCE_BODY_CHARS`.
    pub body: String,
}

/// LLM verdict on accumulated results — a continuous score in 0.0-1.0
/// plus free-form prose notes that feed the next round's subtopic
/// generation.
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    pub score: f32,
    /// Free-form prose. Used as input to [`extract_next_subtopics`]
    /// when the loop continues; surfaced in the final synthesis as
    /// "research notes" too.
    pub notes: String,
}

/// Verdict assigned to one factual claim during the verify pass —
/// whether the cited source actually supports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Source supports the claim with no daylight between them.
    Supported,
    /// Source touches the topic but doesn't strictly support the
    /// specific claim (close enough that the LLM hedges).
    Partial,
    /// Cited source does NOT support the claim — likely hallucination
    /// or miscitation.
    Unsupported,
    /// Claim has no `[N]` citation at all — provenance unknown.
    NoCitation,
}

/// One claim's verification result. `claim` is the rough text the
/// verifier extracted from the page; `citation` is the `[N]` index it
/// pointed at (None when the claim was uncited).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyItem {
    pub claim: String,
    pub citation: Option<u32>,
    pub verdict: Verdict,
    pub note: Option<String>,
}

/// Result of a verify pass over a generated research page. Pre-fix
/// the pipeline only had a single "coverage score" from the
/// evaluator (does the SOURCE SET answer the query?); this report
/// answers the orthogonal question — does the generated PAGE
/// faithfully reflect the cited sources, or did the synthesizer
/// hallucinate / miscite? Score is the fraction of claims rated
/// `Supported` (Partial / NoCitation / Unsupported all detract).
#[derive(Debug, Clone, PartialEq)]
pub struct VerifyReport {
    pub score: f32,
    pub items: Vec<VerifyItem>,
}

impl VerifyReport {
    /// Render the non-`Supported` items as a Markdown section to
    /// append at the end of the page body. Returns `None` when every
    /// claim was supported — no need to clutter clean pages with an
    /// empty verification section.
    pub fn render_flagged_section(&self) -> Option<String> {
        let flagged: Vec<&VerifyItem> = self
            .items
            .iter()
            .filter(|i| i.verdict != Verdict::Supported)
            .collect();
        if flagged.is_empty() {
            return None;
        }
        let mut s = String::from("\n\n---\n\n## Verification\n\n");
        s.push_str(&format!(
            "Auto-verification pass found {} claim(s) that don't strictly match their cited source. \
             Review before relying on the page for downstream decisions.\n\n",
            flagged.len()
        ));
        for item in flagged {
            let icon = match item.verdict {
                Verdict::Unsupported => "🚫",
                Verdict::Partial => "⚠️",
                Verdict::NoCitation => "❓",
                Verdict::Supported => "✓",
            };
            let cite = item
                .citation
                .map(|n| format!("[{n}]"))
                .unwrap_or_else(|| "(uncited)".to_string());
            let verdict_str = match item.verdict {
                Verdict::Supported => "supported",
                Verdict::Partial => "partial",
                Verdict::Unsupported => "unsupported",
                Verdict::NoCitation => "no citation",
            };
            s.push_str(&format!(
                "- {icon} **{verdict_str}** {cite}: {}",
                item.claim.trim()
            ));
            if let Some(note) = &item.note {
                if !note.trim().is_empty() {
                    s.push_str(&format!(" — _{}_", note.trim()));
                }
            }
            s.push('\n');
        }
        Some(s)
    }
}

/// M6.39.6: one row of the page plan returned by [`plan_pages`].
/// LLM groups accumulated sources into a small number of these
/// (entity / concept / comparison / how-to pages); the pipeline
/// then writes one KMS page per row, parallel-synthesized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PagePlan {
    /// Filesystem-safe slug, used as the page filename's stem and as
    /// the cross-link target (`[[slug]]`). Lowercase ASCII alphanum
    /// + hyphens.
    pub slug: String,
    /// Human-readable title, also used as link display text by
    /// default in `[[slug|title]]`.
    pub title: String,
    /// 1-2 sentence description of what this page covers — feeds
    /// the per-page synth prompt + becomes the index summary.
    pub topic: String,
    /// Citation indices (1-based) to include in this page. A source
    /// can appear in multiple pages.
    pub source_indices: Vec<u32>,
}

// ── Public API ───────────────────────────────────────────────────────

/// Generate the first round of subtopics from a broad query + seed
/// search results. Returns `n` subtopic strings (clamped at the
/// configured limit). Falls back to `[query]` if parsing yields zero
/// subtopics.
pub async fn extract_subtopics(
    provider: &dyn Provider,
    model: &str,
    query: &str,
    seed: &[ResearchSource],
    n: u32,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<Vec<String>> {
    let prompt = build_extract_subtopics_prompt(query, seed, n);
    let raw = oneshot(provider, model, prompt, timeout, cancel).await?;
    let mut topics = parse_bulleted_list(&raw);
    topics.truncate(n as usize);
    if topics.is_empty() {
        topics.push(query.to_string());
    }
    Ok(topics)
}

/// Generate next-round subtopics targeting gaps named in `eval_notes`.
/// Same shape as [`extract_subtopics`] but conditioned on accumulated
/// research + the LLM's prior evaluation. Returns empty list when the
/// LLM can't think of anything more — pipeline takes that as a
/// natural stop signal.
pub async fn extract_next_subtopics(
    provider: &dyn Provider,
    model: &str,
    query: &str,
    sources: &[ResearchSource],
    eval_notes: &str,
    n: u32,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<Vec<String>> {
    let prompt = build_extract_next_subtopics_prompt(query, sources, eval_notes, n);
    let raw = oneshot(provider, model, prompt, timeout, cancel).await?;
    let mut topics = parse_bulleted_list(&raw);
    topics.truncate(n as usize);
    Ok(topics)
}

/// Score completeness of the accumulated source set against the query.
/// Returns a value in 0.0-1.0 plus free-form notes. The pipeline uses
/// `score >= threshold` AND `iter >= min_iter` as the early-stop
/// signal; notes feed [`extract_next_subtopics`] when the loop
/// continues.
///
/// The contract: the LLM's first non-empty line MUST start with
/// `score: <float>`. Everything after is treated as notes prose.
/// Parsing is permissive (case-insensitive, `0.7` or `7/10` accepted)
/// but if no number is found, defaults to `0.0` so the loop continues
/// rather than spuriously stopping.
pub async fn evaluate(
    provider: &dyn Provider,
    model: &str,
    query: &str,
    sources: &[ResearchSource],
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<Evaluation> {
    let prompt = build_evaluate_prompt(query, sources);
    let raw = oneshot(provider, model, prompt, timeout, cancel).await?;
    let (score, notes) = parse_score_and_notes(&raw);
    Ok(Evaluation { score, notes })
}

/// M6.39.6: plan a multi-page KMS write from accumulated sources.
/// LLM groups sources into ≤ `max_pages` coherent topics (entity,
/// concept, comparison, how-to, etc). Returns the page list ordered
/// however the LLM emitted; the pipeline writes them as siblings
/// inside the same research run with cross-links.
///
/// Falls back to a single "synthesis" page when JSON parsing fails —
/// the loop never aborts on bad LLM output.
pub async fn plan_pages(
    provider: &dyn Provider,
    model: &str,
    query: &str,
    sources: &[ResearchSource],
    max_pages: u32,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<Vec<PagePlan>> {
    let prompt = build_plan_pages_prompt(query, sources, max_pages);
    let raw = oneshot(provider, model, prompt, timeout, cancel).await?;
    let plan = parse_page_plan(&raw, max_pages, sources.len() as u32);
    if plan.is_empty() {
        // Permissive fallback — produce a single page with all
        // sources so the pipeline always has something to write.
        Ok(vec![PagePlan {
            slug: "synthesis".into(),
            title: format!("Synthesis: {}", trim_for_title(query, 60)),
            topic: format!("Combined research synthesis for: {query}"),
            source_indices: (1..=sources.len() as u32).collect(),
        }])
    } else {
        Ok(plan)
    }
}

/// Synthesize one page from a multi-page plan. The LLM is told what
/// other pages exist (so it can cross-link with `[[slug]]` syntax),
/// what this page covers, and which sources to draw on. Returns the
/// markdown body verbatim — the pipeline post-processes cross-links
/// before writing to disk.
pub async fn write_research_page(
    provider: &dyn Provider,
    model: &str,
    query: &str,
    this_page: &PagePlan,
    all_pages: &[PagePlan],
    sources: &[ResearchSource],
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<String> {
    let prompt = build_write_research_page_prompt(query, this_page, all_pages, sources);
    let raw = oneshot(provider, model, prompt, timeout, cancel).await?;
    Ok(raw.trim().to_string())
}

/// Final markdown report synthesizing accumulated sources into a
/// coherent answer with `[N]` inline citations + URL list at the end.
/// The pipeline writes this verbatim to the KMS.
pub async fn synthesize(
    provider: &dyn Provider,
    model: &str,
    query: &str,
    sources: &[ResearchSource],
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<String> {
    let prompt = build_synthesize_prompt(query, sources);
    let raw = oneshot(provider, model, prompt, timeout, cancel).await?;
    Ok(raw.trim().to_string())
}

/// Run a verify pass over a synthesised page: extract each factual
/// claim, check it against the cited source, return a structured
/// report. The synthesise step can hallucinate (claim a fact that
/// no source supports) or miscite (cite [3] for something only [1]
/// actually says); this pass catches both classes before the page is
/// committed to the KMS — addressing the well-known "LLM Wiki bakes
/// in organised persistent mistakes" critique of wiki-style KMS
/// patterns.
///
/// Cost: one LLM call per generated page (~+25% of total /research
/// LLM cost for a typical 4-page run). Soft-fails: returns an empty
/// report on parse failure rather than aborting the pipeline, so a
/// flaky verifier doesn't kill the whole research run.
pub async fn verify_page(
    provider: &dyn Provider,
    model: &str,
    page_body: &str,
    cited_sources: &[ResearchSource],
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<VerifyReport> {
    let prompt = build_verify_page_prompt(page_body, cited_sources);
    let raw = oneshot(provider, model, prompt, timeout, cancel).await?;
    Ok(parse_verify_report(&raw))
}

/// Derive a kebab-case KMS slug from a free-form query (max ~5 words).
/// Used when `JobConfig::kms_target` is `None`. LLM is the right tool
/// here because the query may be in any language; deriving a clean
/// English/transliterated slug from Thai prose with regex is fragile.
pub async fn derive_topic_slug(
    provider: &dyn Provider,
    model: &str,
    query: &str,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<String> {
    let prompt = build_derive_slug_prompt(query);
    let raw = oneshot(provider, model, prompt, timeout, cancel).await?;
    Ok(sanitize_slug(&raw))
}

// ── Prompt builders ──────────────────────────────────────────────────

fn build_extract_subtopics_prompt(query: &str, seed: &[ResearchSource], n: u32) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Research query: {query}\n\n\
         Seed search results ({} sources):\n",
        seed.len()
    ));
    for r in seed.iter().take(10) {
        s.push_str(&format!(
            "- {} ({})\n  {}\n",
            r.title,
            r.url,
            snippet(&r.body, 3000)
        ));
    }
    s.push_str(&format!(
        "\nGenerate {n} focused subtopics to research in parallel. \
         Each subtopic should be a specific search-engine query (not a question), \
         covering a distinct facet of the original query.\n\n\
         Output ONE subtopic per line, numbered or bulleted, no commentary. Example:\n\
         1. quantum error correction surface codes\n\
         2. superconducting qubit decoherence mechanisms\n\
         3. ...\n"
    ));
    s
}

fn build_extract_next_subtopics_prompt(
    query: &str,
    sources: &[ResearchSource],
    eval_notes: &str,
    n: u32,
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Original query: {query}\n\n\
         Accumulated so far ({} sources):\n",
        sources.len()
    ));
    for r in sources.iter().take(20) {
        s.push_str(&format!("[{}] {} ({})\n", r.index, r.title, r.url));
    }
    s.push_str(&format!(
        "\nPrior evaluation notes:\n{eval_notes}\n\n\
         Generate {n} next-round search queries to fill the gaps named above. \
         Avoid topics already well-covered. If no productive next searches come \
         to mind, output an empty response (the loop will stop).\n\n\
         Output ONE query per line, numbered or bulleted, no commentary."
    ));
    s
}

fn build_evaluate_prompt(query: &str, sources: &[ResearchSource]) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Original query: {query}\n\n\
         Accumulated sources ({} total):\n",
        sources.len()
    ));
    for r in sources.iter().take(40) {
        s.push_str(&format!(
            "[{}] {} ({})\n   {}\n\n",
            r.index,
            r.title,
            r.url,
            snippet(&r.body, 15000)
        ));
    }
    s.push_str(
        "Rate how completely the accumulated sources answer the query, \
         considering:\n\
         - **Source quantity**: enough distinct authoritative URLs (more is better)\n\
         - **Source diversity**: different domains / publishers, not all from one site\n\
         - **Fact verification**: key claims supported by 2+ sources, no obvious contradictions\n\
         - **Direct relevance**: sources address the query, not tangents\n\n\
         Output format — STRICT, the first non-empty line must be:\n\
         `score: 0.NN`\n\
         (a single number 0.0 to 1.0, where 1.0 means a complete, well-verified answer is possible)\n\n\
         Then on subsequent lines, write free-form notes explaining:\n\
         - What's covered well\n\
         - What's missing or under-supported\n\
         - Specific subtopics worth searching next\n\n\
         Be honest about uncertainty — if claims aren't well-verified, say so and score lower.",
    );
    s
}

fn build_plan_pages_prompt(query: &str, sources: &[ResearchSource], max_pages: u32) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Original research query: {query}\n\n\
         Accumulated sources ({} total):\n",
        sources.len()
    ));
    for r in sources {
        s.push_str(&format!(
            "[{}] {} ({})\n   {}\n\n",
            r.index,
            r.title,
            r.url,
            snippet(&r.body, 15000)
        ));
    }
    s.push_str(&format!(
        "Plan a KMS page layout that captures these findings.\n\n\
         **Page count is a CEILING, not a target.** {} pages maximum — but \
         output FEWER if the material doesn't naturally split that many ways. \
         Forcing pages to fill a quota produces thin, repetitive pages that \
         hurt the KMS more than they help. Common honest counts:\n\
         - Narrow factual question → 1 page is fine (just one concept)\n\
         - One entity with light surrounding context → 1-2 pages\n\
         - Topic with a few distinct facets → 3-4 pages\n\
         - Broad topic with multiple entities + comparisons → up to {0}\n\n\
         Each page must cover ONE coherent thing. Pick the shape that fits:\n\
         - **Entity** page: a person, organization, paper, product (slug = the entity name)\n\
         - **Concept** page: a single idea / definition / mechanism\n\
         - **Comparison** page: X vs Y\n\
         - **How-to / Pattern** page: a procedure, recipe, or design pattern\n\
         - **Timeline** page: chronological events\n\n\
         Reject thin pages: if a candidate page would have <3 sources or \
         repeats material covered better by another page in the plan, drop \
         it. Granular pages with cross-links beat one mega-page; thin pages \
         padded to hit a count are worse than either.\n\n\
         Slug rules:\n\
         - Lowercase ASCII alphanumeric + hyphens (`a-z`, `0-9`, `-`)\n\
         - 2-5 words ideally\n\
         - Stable: an entity gets the same slug across runs (e.g. `andrej-karpathy`)\n\n\
         Output ONE JSON array, no commentary, no markdown code fence:\n\n\
         [\n  \
           {{\"slug\": \"...\", \"title\": \"...\", \"topic\": \"...\", \"source_indices\": [1, 3, 7]}},\n  \
           ...\n\
         ]\n\n\
         A source can appear in multiple pages if relevant. `topic` is \
         1-2 sentences describing what the page covers — it becomes the \
         KMS index summary so make it informative.",
        max_pages
    ));
    s
}

fn build_write_research_page_prompt(
    query: &str,
    this_page: &PagePlan,
    all_pages: &[PagePlan],
    sources: &[ResearchSource],
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "You are writing ONE page in a multi-page research output for the \
         original query: {query}\n\n\
         === This page ===\n\
         Slug: {}\n\
         Title: {}\n\
         Covers: {}\n\n",
        this_page.slug, this_page.title, this_page.topic
    ));
    if all_pages.len() > 1 {
        s.push_str(
            "=== Other pages in this run (cross-link with `[[slug]]` or `[[slug|display]]`) ===\n",
        );
        for p in all_pages {
            if p.slug == this_page.slug {
                continue;
            }
            s.push_str(&format!("- [[{}]] — {} — {}\n", p.slug, p.title, p.topic));
        }
        s.push('\n');
    }
    let cited: std::collections::HashSet<u32> = this_page.source_indices.iter().copied().collect();
    s.push_str("=== Sources to use (cite as [N]) ===\n");
    for r in sources {
        if !cited.contains(&r.index) {
            continue;
        }
        s.push_str(&format!(
            "[{}] {} ({})\n   {}\n\n",
            r.index,
            r.title,
            r.url,
            snippet(&r.body, 60000)
        ));
    }
    s.push_str(
        "Write the page body as Markdown:\n\
         - **Lead with a 1-2 sentence ABSTRACT** as the very first prose line, \
         BEFORE any `##` subtopic headings, before any list bullets, before any \
         blockquotes. The abstract names the topic + key finding in one self-\
         contained sentence. It becomes the KMS index summary; make it informative.\n\
         - Do NOT start with a heading (no `# Title` / `## Overview`) — start \
         directly with prose.\n\
         - After the abstract, use `##` headings to organize subtopics.\n\
         - Inline `[N]` citations after every factual claim.\n\
         - Cross-link to OTHER PAGES with `[[slug]]` or `[[slug|display text]]` \
         when this page mentions an entity / concept that has its own page in \
         the list above. Don't force it — only link when the reference is \
         genuinely useful to a future reader.\n\
         - **Do NOT write a `## Sources` section yourself** — the pipeline \
         appends a canonical Sources section automatically based on which \
         `[N]` you cited, so any manual one would be replaced. Just use `[N]` \
         inline citations and stop.\n\
         - Answer in the same language as the original query.\n\n\
         Output ONLY the markdown body — no preamble, no JSON, no commentary.",
    );
    s
}

fn build_synthesize_prompt(query: &str, sources: &[ResearchSource]) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Original query: {query}\n\n\
         Sources ({} total):\n",
        sources.len()
    ));
    for r in sources {
        s.push_str(&format!(
            "[{}] {} ({})\n   {}\n\n",
            r.index,
            r.title,
            r.url,
            snippet(&r.body, 20000)
        ));
    }
    s.push_str(
        "Write a comprehensive answer to the original query as Markdown. \
         Requirements:\n\
         - **Lead with a 1-2 sentence ABSTRACT** as the very first prose \
         line, BEFORE any `##` subtopic headings, before any list bullets, \
         before any blockquotes. The abstract must name the topic + state \
         your overall finding in one self-contained sentence (e.g. \
         \"OBON is a Japanese summer festival honoring ancestors, observed \
         13–16 August with regional variation in customs and dates [1][3].\"). \
         Do NOT start with a heading like `# Research:` or `## Overview` — \
         start directly with prose. This abstract is extracted verbatim as \
         the KMS index summary; if it's a heading or a meta-line, the index \
         loses signal.\n\
         - At least 200 words total\n\
         - Inline `[N]` citations after every factual claim, mapping to the source list above\n\
         - Cite multiple sources for important claims when available\n\
         - After the abstract, use Markdown headings (`##`) to organize by subtopic\n\
         - When sources contradict, surface the disagreement rather than picking arbitrarily\n\n\
         End with a `## Sources` section listing every cited source as `[N] <url>` (one per line). \
         Answer in the same language as the original query.",
    );
    s
}

fn build_verify_page_prompt(page_body: &str, cited_sources: &[ResearchSource]) -> String {
    let mut s = String::new();
    s.push_str(
        "You are a verification auditor. Read the GENERATED PAGE below \
         and check every factual claim against the CITED SOURCES. The \
         page was synthesised by another LLM and may contain real \
         hallucinations — `[N]` citations attached to claims the source \
         doesn't actually make. Your job is to flag THOSE — not honest \
         paraphrases.\n\n\
         **Be generous about paraphrasing.** Synthesis necessarily restates \
         ideas in different words; that is not a defect. A faithful \
         paraphrase that expands or simplifies the source's wording, adds \
         synonyms, or merges adjacent sentences IS supported. Only flag \
         when meaning actually diverges.\n\n",
    );
    s.push_str("=== GENERATED PAGE ===\n");
    s.push_str(page_body.trim());
    s.push_str("\n\n=== CITED SOURCES ===\n");
    for r in cited_sources {
        s.push_str(&format!(
            "[{}] {} ({})\n   {}\n\n",
            r.index,
            r.title,
            r.url,
            snippet(&r.body, 60000)
        ));
    }
    s.push_str(
        "For each factual claim in the page (definitions, dates, numbers, \
         attributions, capability statements, comparisons), classify it:\n\n\
         **`Supported`** — the source's content backs the claim's meaning. \
         This INCLUDES:\n\
         - The page restates the source idea in different/expanded words\n\
         - The page adds explanatory synonyms or merges adjacent source \
           sentences (e.g. source says \"store and recall past experiences\"; \
           page says \"store, retain, and recall past experiences, \
           interactions, and knowledge\" — still Supported)\n\
         - The page reorders or rephrases a definition\n\
         - The page applies the source's general claim to a specific case \
           the source clearly implies\n\n\
         **`Partial`** — only flag when a LOAD-BEARING detail diverges from \
         the source. Examples of load-bearing:\n\
         - Quantitative claim: \"100x faster\" vs source's \"significantly faster\"\n\
         - Date / version: \"released 2024\" vs source's \"released 2023\"\n\
         - Named entity: \"GPT-4\" vs source's \"Claude 3\"; \"OpenAI\" vs \
           source's \"Anthropic\"\n\
         - Scope quantifier: \"all agents\" vs source's \"some agents\"; \
           \"always\" vs source's \"sometimes\"\n\
         Do NOT flag Partial just because the page's wording differs from \
         the source's wording.\n\n\
         **`Unsupported`** — only flag when:\n\
         - The source's content directly CONTRADICTS the claim, OR\n\
         - The source body in this prompt is substantive AND addresses an \
           entirely different topic than what the claim asserts.\n\
         Hallucinated citations (model invented a connection that isn't \
         in the source's content) belong here.\n\n\
         **`NoCitation`** — the page makes a load-bearing factual assertion \
         with no `[N]` attached at all. Only flag genuinely uncited factual \
         claims; do not flag opinion lines, hedges (\"may be\"), questions, \
         page headings, or the auto-generated `## Sources` block.\n\n\
         **When in doubt, default to `Supported`.** A small amount of false \
         negatives is much less harmful than a flood of false positives.\n\n\
         **If a source's body content here is mostly navigation, header, \
         footer, or otherwise non-substantive** (fewer than ~300 chars of \
         actual prose about the topic), SKIP every claim citing that \
         source — do NOT include them in `items`. Absence of evidence in \
         the snippet does not mean the source contradicts the claim. \
         Treat those claims as if they didn't exist for scoring purposes.\n\n\
         Output STRICT JSON, nothing else. Schema:\n\
         ```\n\
         {\n\
           \"score\": 0.NN,                  // fraction of total IN-SCOPE factual claims that are Supported (0.0-1.0). Skipped claims (above) do not count for or against the score.\n\
           \"items\": [\n\
             {\n\
               \"claim\": \"short paraphrase of the flagged sentence\",\n\
               \"citation\": 3 | null,       // [N] index if cited, null if NoCitation\n\
               \"verdict\": \"Partial\" | \"Unsupported\" | \"NoCitation\",\n\
               \"note\": \"brief reason — quote the load-bearing divergence\" | null\n\
             }\n\
           ]\n\
         }\n\
         ```\n\
         Only LIST the flagged ones (Partial / Unsupported / NoCitation). \
         If the page is well-supported, output `{\"score\": 1.0, \"items\": []}`. \
         Output JSON ONLY — no Markdown fences, no preamble, no trailing prose.",
    );
    s
}

/// Parse the verifier's JSON output into a [`VerifyReport`]. Soft-
/// fails on any parse problem (returns an empty zero-score report)
/// so a flaky verifier doesn't kill the research run. Strips
/// Markdown fences if the LLM wrapped its JSON in ```json blocks
/// despite the prompt asking it not to.
fn parse_verify_report(raw: &str) -> VerifyReport {
    let stripped = strip_json_fences(raw.trim());
    let Ok(v) = serde_json::from_str::<serde_json::Value>(stripped) else {
        return VerifyReport {
            score: 0.0,
            items: Vec::new(),
        };
    };
    let score = v
        .get("score")
        .and_then(|s| s.as_f64())
        .map(|f| f.clamp(0.0, 1.0) as f32)
        .unwrap_or(0.0);
    let items: Vec<VerifyItem> = v
        .get("items")
        .and_then(|i| i.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let claim = entry.get("claim").and_then(|c| c.as_str())?.to_string();
                    let citation = entry
                        .get("citation")
                        .and_then(|c| c.as_u64())
                        .map(|n| n as u32);
                    let verdict = match entry.get("verdict").and_then(|v| v.as_str())? {
                        "Supported" => Verdict::Supported,
                        "Partial" => Verdict::Partial,
                        "Unsupported" => Verdict::Unsupported,
                        "NoCitation" => Verdict::NoCitation,
                        _ => return None,
                    };
                    let note = entry
                        .get("note")
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string());
                    Some(VerifyItem {
                        claim,
                        citation,
                        verdict,
                        note,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    VerifyReport { score, items }
}

/// Strip ```json fences (or plain ```) if the LLM wrapped its JSON
/// despite being told not to. Returns the inner content or the
/// original string if no fences detected.
fn strip_json_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    // ```json … ``` or ``` … ```
    if let Some(rest) = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
    {
        if let Some(end) = rest.rfind("```") {
            return rest[..end].trim();
        }
    }
    trimmed
}

fn build_derive_slug_prompt(query: &str) -> String {
    format!(
        "Convert this research query to a short kebab-case slug suitable as a \
         knowledge-base name. 2-5 lowercase ASCII words separated by hyphens. \
         No quotes, no punctuation, no commentary — just the slug.\n\n\
         Examples:\n\
         - 'How to use Tavily API' → tavily-api\n\
         - 'ค้นหาข่าว OBON' → obon-festival-news\n\
         - 'recent advances in quantum error correction' → quantum-error-correction\n\n\
         Query: {query}\n\
         Slug:"
    )
}

// ── Parsing helpers ──────────────────────────────────────────────────

/// Parse a numbered/bulleted list out of LLM prose. Accepts:
/// - `1. topic`
/// - `1) topic`
/// - `- topic`
/// - `* topic`
/// - bare `topic` lines (non-empty, non-prefixed)
///
/// Strips trailing punctuation and whitespace. Skips blank lines and
/// lines that look like commentary (start with `#`, `Note:`, etc.).
fn parse_bulleted_list(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Skip Markdown headers and obvious commentary preambles.
        if line.starts_with('#')
            || line.to_ascii_lowercase().starts_with("note:")
            || line.to_ascii_lowercase().starts_with("here are")
        {
            continue;
        }
        let stripped = strip_list_marker(line);
        let cleaned = stripped.trim_end_matches(|c: char| c == '.' || c == ',' || c == ';');
        if !cleaned.is_empty() {
            out.push(cleaned.to_string());
        }
    }
    out
}

fn strip_list_marker(line: &str) -> &str {
    let mut chars = line.char_indices();
    if let Some((_, c)) = chars.next() {
        // Numbered: `1.`, `12.`, `1)`, `12)`
        if c.is_ascii_digit() {
            let mut last_digit_end = c.len_utf8();
            for (i, c2) in chars.by_ref() {
                if c2.is_ascii_digit() {
                    last_digit_end = i + c2.len_utf8();
                } else if c2 == '.' || c2 == ')' {
                    return line[i + c2.len_utf8()..].trim_start();
                } else {
                    break;
                }
            }
            // No marker found — keep the line as-is
            let _ = last_digit_end;
            return line;
        }
        // Bullet
        if c == '-' || c == '*' || c == '•' {
            return line[c.len_utf8()..].trim_start();
        }
    }
    line
}

/// Extract `score: 0.X` from the first non-empty line, return remainder
/// as notes. Permissive: accepts any case (`Score`, `SCORE`), accepts
/// `7/10` style as `0.7`, defaults to 0.0 on parse failure (so the
/// loop keeps going rather than spuriously stopping).
fn parse_score_and_notes(text: &str) -> (f32, String) {
    let mut lines = text.lines();
    let first = loop {
        match lines.next() {
            Some(l) if l.trim().is_empty() => continue,
            Some(l) => break l,
            None => return (0.0, String::new()),
        }
    };
    let lower = first.trim().to_ascii_lowercase();
    let (score, first_was_score_line) = if let Some(rest) = lower.strip_prefix("score:") {
        (parse_score_value(rest.trim()).unwrap_or(0.0), true)
    } else if let Some(rest) = lower.strip_prefix("score") {
        // Accept `score 0.7` (no colon) defensively. Only count as a
        // score line if the rest actually parses to a number — bare
        // prose starting with the word "Score" shouldn't get eaten.
        match parse_score_value(rest.trim()) {
            Some(v) => (v, true),
            None => (find_inline_score(text).unwrap_or(0.0), false),
        }
    } else {
        // No `score:` prefix — try to find one elsewhere in the body,
        // and keep the first line as part of the notes (it's prose
        // the LLM produced; losing it would discard signal the next
        // round wants).
        (find_inline_score(text).unwrap_or(0.0), false)
    };
    let mut notes_lines: Vec<&str> = lines.collect();
    if !first_was_score_line {
        notes_lines.insert(0, first);
    }
    let notes = notes_lines.join("\n").trim().to_string();
    (clamp01(score), notes)
}

fn find_inline_score(text: &str) -> Option<f32> {
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(idx) = lower.find("score:") {
            let rest = &line[idx + "score:".len()..];
            if let Some(s) = parse_score_value(rest.trim()) {
                return Some(s);
            }
        }
    }
    None
}

fn parse_score_value(s: &str) -> Option<f32> {
    // Accept `0.7`, `0.75`, `7/10`, `75%`, `75 / 100`.
    let token: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '/' || *c == '%')
        .collect();
    if token.is_empty() {
        return None;
    }
    if let Some((num, denom)) = token.split_once('/') {
        let num: f32 = num.parse().ok()?;
        let denom: f32 = denom.parse().ok()?;
        if denom > 0.0 {
            return Some(num / denom);
        }
        return None;
    }
    if let Some(stripped) = token.strip_suffix('%') {
        let n: f32 = stripped.parse().ok()?;
        return Some(n / 100.0);
    }
    token.parse().ok()
}

fn clamp01(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 1.0)
    }
}

/// The `--legacy` pipeline's page slug. It kept ASCII only and fell back to
/// the literal `research`, so every Thai, Chinese or Arabic page of a run
/// was named `research` and each one overwrote the last (dev-plan/64
/// P3.10). Same rules as the v2 slugger now; the fallback is only for a
/// name with nothing usable in it at all.
fn sanitize_slug(raw: &str) -> String {
    let slug = super::digest::sanitize_slug(&raw.replace('_', " "));
    if slug.is_empty() {
        "research".into()
    } else {
        slug
    }
}

fn snippet(body: &str, max: usize) -> String {
    if body.len() <= max {
        body.to_string()
    } else {
        let mut end = max;
        while !body.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &body[..end])
    }
}

fn trim_for_title(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

/// Parse a `[ {...}, {...} ]`-shaped JSON page plan out of the LLM's
/// response. The LLM is told to emit ONE JSON array, no markdown code
/// fence, but real models do all sorts of things — wrap in ```json
/// fences, prepend "Here's the plan:", emit nested arrays. This
/// function:
/// 1. Finds the first `[` and last `]` in the response.
/// 2. Tries to parse that slice as JSON.
/// 3. If parse fails, returns empty (caller falls back to single page).
/// 4. For each entry, normalizes slug (sanitize), title (trim),
///    source_indices (filter to range 1..=source_count, dedupe).
/// 5. Drops entries with empty slug AFTER normalization, or with
///    no source_indices.
/// 6. Caps total page count at `max_pages`.
fn parse_page_plan(raw: &str, max_pages: u32, source_count: u32) -> Vec<PagePlan> {
    use serde::Deserialize;
    let Some(start) = raw.find('[') else {
        return Vec::new();
    };
    let Some(end) = raw.rfind(']') else {
        return Vec::new();
    };
    if end <= start {
        return Vec::new();
    }
    let slice = &raw[start..=end];

    #[derive(Deserialize)]
    struct RawPage {
        #[serde(default)]
        slug: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        topic: String,
        #[serde(default)]
        source_indices: Vec<u32>,
    }
    let parsed: Vec<RawPage> = match serde_json::from_str(slice) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let mut out: Vec<PagePlan> = Vec::new();
    let mut seen_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in parsed {
        let slug = sanitize_slug(&p.slug);
        if slug.is_empty() || slug == "research" {
            // sanitize_slug returns "research" as a fallback for
            // unparseable input; that's not a real entity, skip.
            continue;
        }
        if !seen_slugs.insert(slug.clone()) {
            // LLM emitted duplicate slug — skip second occurrence.
            continue;
        }
        let title = if p.title.trim().is_empty() {
            slug.clone()
        } else {
            p.title.trim().to_string()
        };
        let topic = if p.topic.trim().is_empty() {
            String::new()
        } else {
            p.topic.trim().to_string()
        };
        // Filter source_indices to valid 1-based range; dedupe.
        let mut indices: Vec<u32> = p
            .source_indices
            .into_iter()
            .filter(|i| *i >= 1 && *i <= source_count)
            .collect();
        indices.sort_unstable();
        indices.dedup();
        if indices.is_empty() {
            continue;
        }
        out.push(PagePlan {
            slug,
            title,
            topic,
            source_indices: indices,
        });
        if out.len() >= max_pages as usize {
            break;
        }
    }
    out
}

// ── Provider oneshot wrapper ────────────────────────────────────────

/// Drive a single LLM call to completion: build a `StreamRequest` with
/// no tools / no system prompt / one user message, drain the stream
/// for `TextDelta` events into a String, return. Cancellation (via
/// `CancelToken`) and timeout are honored; both produce
/// `Error::Tool("…cancelled" / "…timed out")`.
/// Public alias for callers outside this module that need `oneshot`
/// inside a spawned future (same body; keeps `oneshot` itself
/// private-ish). Used by `kms_verify`'s entailment pass, which wants
/// the same thinking-off, timeout-override shape.
pub async fn oneshot_pub(
    provider: &dyn Provider,
    model: &str,
    prompt: String,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<String> {
    oneshot(provider, model, prompt, timeout, cancel).await
}

/// What a run's LLM calls came to, per model — a run uses up to two (the
/// worker and the digest model), priced differently.
#[derive(Debug, Default, Clone)]
pub struct RunUsage {
    pub by_model: std::collections::BTreeMap<String, ModelUsage>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ModelUsage {
    pub calls: u32,
    /// Uncached input, as providers report it.
    pub input: u64,
    pub cached: u64,
    pub cache_written: u64,
    pub output: u64,
}

impl RunUsage {
    fn add(&mut self, model: &str, u: &crate::providers::Usage) {
        let m = self.by_model.entry(model.to_string()).or_default();
        m.calls += 1;
        m.input += u.input_tokens as u64;
        m.cached += u.cache_read_input_tokens.unwrap_or(0) as u64;
        m.cache_written += u.cache_creation_input_tokens.unwrap_or(0) as u64;
        m.output += u.output_tokens as u64;
    }

    pub fn calls(&self) -> u32 {
        self.by_model.values().map(|m| m.calls).sum()
    }

    /// `None` when any model that was used has no price — a partial sum
    /// would read as the cost of the run.
    pub fn cost_usd(&self) -> Option<f64> {
        let catalogue = crate::model_catalogue::EffectiveCatalogue::load();
        let cap = |n: u64| n.min(u32::MAX as u64) as u32;
        self.by_model
            .iter()
            .map(|(model, m)| {
                catalogue.compute_cost_usd(
                    model,
                    &crate::model_catalogue::TokenUsage {
                        prompt_tokens: cap(m.input + m.cached),
                        completion_tokens: cap(m.output),
                        cached_input_tokens: cap(m.cached),
                        cache_creation_tokens: cap(m.cache_written),
                        reasoning_tokens: 0,
                    },
                )
            })
            .sum()
    }

    /// Frontmatter lines for the run log; empty when nothing was recorded.
    pub fn frontmatter(&self) -> String {
        if self.by_model.is_empty() {
            return String::new();
        }
        let (mut input, mut cached, mut output) = (0u64, 0u64, 0u64);
        for m in self.by_model.values() {
            input += m.input;
            cached += m.cached;
            output += m.output;
        }
        let mut s = format!(
            "llm_calls: {}\ninput_tokens: {input}\ncached_input_tokens: {cached}\noutput_tokens: {output}\n",
            self.calls()
        );
        if let Some(c) = self.cost_usd() {
            s.push_str(&format!("cost_usd: {c:.4}\n"));
        }
        s
    }
}

tokio::task_local! {
    static RUN_USAGE: std::sync::Arc<std::sync::Mutex<RunUsage>>;
}

/// Run `fut` with a usage ledger that every `oneshot` inside it adds to.
/// Task-local, so it follows the run through `join_all` and
/// `buffer_unordered` (same task) without a parameter on every call; a call
/// made outside any ledger — a test, another feature — records nothing.
pub async fn track_usage<F: std::future::Future>(fut: F) -> F::Output {
    RUN_USAGE
        .scope(
            std::sync::Arc::new(std::sync::Mutex::new(RunUsage::default())),
            fut,
        )
        .await
}

/// The ledger so far, for the run log.
pub fn usage_so_far() -> RunUsage {
    RUN_USAGE
        .try_with(|u| u.lock().map(|g| g.clone()).unwrap_or_default())
        .unwrap_or_default()
}

/// What a call is for, which decides whether the model thinks first.
///
/// Every research call ran with thinking off, on one measurement: a digest
/// took 77–89 s and 5–13k reasoning tokens with it, 4–7 s without. True of
/// digests — extraction is mechanical, and there are twenty of them. It was
/// then applied to calls that are nothing like a digest. An auditor forced
/// to answer without thinking flagged different sentences on every run,
/// agreed with itself on 1 sentence of 18, and twice listed a sentence as
/// unsupported while its own reason concluded "so it is supported".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    /// Extraction: digests, slugs, queries. Never thinks.
    Mechanical,
    /// A verdict on text that already exists: an audit finding, a grounding
    /// proposal. Thinks unless `research_thinking` is `off`.
    Judgement,
    /// The note plan. It is a judgement, and it was given thinking with the
    /// others — and one plan call went from 48.7 s to 357 s, on a refresh
    /// whose plan the code overrides anyway. Thinks only when
    /// `research_thinking` is `all`, like a note body.
    Planning,
    /// A note body. Thinks only when `research_thinking` is `all`.
    Writing,
}

impl CallKind {
    fn thinking_budget(self) -> u32 {
        let setting = crate::config::AppConfig::load()
            .ok()
            .and_then(|c| c.research_thinking)
            .unwrap_or_else(|| "judgement".into());
        let on = match (self, setting.as_str()) {
            (Self::Mechanical, _) | (_, "off") => false,
            (Self::Judgement, _) => true,
            (Self::Planning | Self::Writing, s) => s == "all",
        };
        if on {
            crate::providers::ThinkingLevel::LOW_BUDGET
        } else {
            0
        }
    }
}

pub(super) async fn oneshot(
    provider: &dyn Provider,
    model: &str,
    prompt: String,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<String> {
    oneshot_with_system(
        provider,
        model,
        None,
        prompt,
        timeout,
        cancel,
        CallKind::Mechanical,
    )
    .await
}

/// The note plan. See [`CallKind::Planning`].
pub(super) async fn oneshot_planning(
    provider: &dyn Provider,
    model: &str,
    prompt: String,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<String> {
    oneshot_with_system(
        provider,
        model,
        None,
        prompt,
        timeout,
        cancel,
        CallKind::Planning,
    )
    .await
}

/// One call whose answer is a judgement. See [`CallKind`].
pub async fn oneshot_judgement(
    provider: &dyn Provider,
    model: &str,
    prompt: String,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<String> {
    oneshot_with_system(
        provider,
        model,
        None,
        prompt,
        timeout,
        cancel,
        CallKind::Judgement,
    )
    .await
}

/// One call of a batch whose calls share an opening. See [`SplitPrompt`].
pub async fn oneshot_split(
    provider: &dyn Provider,
    model: &str,
    prompt: super::SplitPrompt,
    timeout: Duration,
    cancel: &CancelToken,
    kind: CallKind,
) -> Result<String> {
    oneshot_with_system(
        provider,
        model,
        Some(prompt.shared),
        prompt.item,
        timeout,
        cancel,
        kind,
    )
    .await
}

async fn oneshot_with_system(
    provider: &dyn Provider,
    model: &str,
    system: Option<String>,
    prompt: String,
    timeout: Duration,
    cancel: &CancelToken,
    kind: CallKind,
) -> Result<String> {
    let req = StreamRequest {
        model: model.to_string(),
        system,
        messages: vec![Message::user(prompt)],
        tools: Vec::new(),
        max_tokens: 16384,
        // Per call kind — see [`CallKind`]. A digest on deepseek-v4 spent
        // 5–13k reasoning tokens (77–89 s) thinking versus 4–7 s not, which
        // is why mechanical calls never do.
        thinking_budget: Some(kind.thinking_budget()),
        // Research synthesizes long pages — the model may go silent
        // for minutes mid-stream. Force the per-chunk idle ceiling to
        // the pipeline's `llm_timeout` (default 900s) regardless of
        // the user's `stream_chunk_timeout_secs` setting.
        stream_chunk_timeout_override: Some(timeout),
    };
    let stream_fut = provider.stream(req);
    let mut stream = match tokio::time::timeout(timeout, stream_fut).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(Error::Tool(
                "research LLM call timed out building stream".into(),
            ))
        }
    };

    let mut text = String::new();
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Tool("research LLM call cancelled".into()));
        }
        let next = tokio::select! {
            ev = tokio::time::timeout(timeout, stream.next()) => ev,
            _ = cancel.cancelled() => {
                return Err(Error::Tool("research LLM call cancelled".into()));
            }
        };
        match next {
            Ok(Some(Ok(ProviderEvent::TextDelta(s)))) => text.push_str(&s),
            Ok(Some(Ok(ProviderEvent::MessageStop { usage, .. }))) => {
                if let Some(u) = usage {
                    crate::usage::note_side_usage(model, &u);
                    let _ = RUN_USAGE.try_with(|l| {
                        if let Ok(mut g) = l.lock() {
                            g.add(model, &u);
                        }
                    });
                }
                break;
            }
            Ok(Some(Ok(_))) => {} // ignore non-text events (tool use, thinking, etc.)
            Ok(Some(Err(e))) => return Err(e),
            Ok(None) => break, // stream ended without explicit MessageStop
            Err(_) => {
                return Err(Error::Tool(
                    "research LLM call timed out reading stream".into(),
                ))
            }
        }
    }
    Ok(text)
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    /// Thinking is decided by what the call is for. Digests never think; a
    /// judgement does unless told not to; a note body only when asked.
    #[test]
    fn a_judgement_thinks_and_a_digest_does_not() {
        let _h = crate::research::test_helpers::scoped_home();
        let low = crate::providers::ThinkingLevel::LOW_BUDGET;
        assert_eq!(CallKind::Mechanical.thinking_budget(), 0);
        assert_eq!(CallKind::Judgement.thinking_budget(), low);
        assert_eq!(CallKind::Writing.thinking_budget(), 0);
        assert_eq!(
            CallKind::Planning.thinking_budget(),
            0,
            "a plan that thinks took 357 s"
        );

        let dir = std::env::current_dir().unwrap().join(".thclaws");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), r#"{"research_thinking":"all"}"#).unwrap();
        assert_eq!(CallKind::Writing.thinking_budget(), low);
        assert_eq!(CallKind::Planning.thinking_budget(), low);
        assert_eq!(
            CallKind::Mechanical.thinking_budget(),
            0,
            "never, whatever the setting"
        );
        std::fs::write(dir.join("settings.json"), r#"{"research_thinking":"off"}"#).unwrap();
        assert_eq!(CallKind::Judgement.thinking_budget(), 0);
    }

    use super::*;

    // ── parse_bulleted_list ────────────────────────────────────────

    #[test]
    fn parse_bulleted_list_handles_numbered() {
        let v = parse_bulleted_list(
            "1. quantum error correction\n2. superconducting qubits\n3. surface codes",
        );
        assert_eq!(
            v,
            vec![
                "quantum error correction",
                "superconducting qubits",
                "surface codes"
            ]
        );
    }

    #[test]
    fn parse_bulleted_list_handles_dash_and_star() {
        let v = parse_bulleted_list("- alpha\n* beta\n• gamma\n- delta.");
        assert_eq!(v, vec!["alpha", "beta", "gamma", "delta"]);
    }

    #[test]
    fn parse_bulleted_list_skips_headers_and_commentary() {
        let v = parse_bulleted_list(
            "Here are the subtopics:\n# Subtopics\n1. alpha\nNote: be careful\n2. beta",
        );
        assert_eq!(v, vec!["alpha", "beta"]);
    }

    #[test]
    fn parse_bulleted_list_accepts_paren_form() {
        let v = parse_bulleted_list("1) alpha\n2) beta");
        assert_eq!(v, vec!["alpha", "beta"]);
    }

    #[test]
    fn parse_bulleted_list_returns_empty_for_empty_input() {
        assert!(parse_bulleted_list("").is_empty());
        assert!(parse_bulleted_list("\n\n  \n").is_empty());
    }

    // ── parse_verify_report ─────────────────────────────────────

    #[test]
    fn parse_verify_handles_clean_json() {
        let raw = r#"{"score": 0.85, "items": [
            {"claim": "X is 100x faster", "citation": 3, "verdict": "Unsupported", "note": "[3] says faster but not 100x"},
            {"claim": "Y was released in 2024", "citation": null, "verdict": "NoCitation", "note": null}
        ]}"#;
        let r = parse_verify_report(raw);
        assert!((r.score - 0.85).abs() < 1e-6);
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.items[0].verdict, Verdict::Unsupported);
        assert_eq!(r.items[0].citation, Some(3));
        assert_eq!(r.items[1].verdict, Verdict::NoCitation);
        assert_eq!(r.items[1].citation, None);
    }

    #[test]
    fn parse_verify_strips_markdown_fences() {
        // LLMs sometimes wrap JSON in ```json blocks despite being
        // told not to. Parser tolerates it instead of returning the
        // soft-fail zero report.
        let raw = "```json\n{\"score\": 1.0, \"items\": []}\n```";
        let r = parse_verify_report(raw);
        assert!((r.score - 1.0).abs() < 1e-6);
        assert!(r.items.is_empty());
    }

    #[test]
    fn parse_verify_soft_fails_on_garbage() {
        // Provider returned prose instead of JSON → score=0, empty
        // items. Caller writes the page without a verification
        // record rather than blowing up the whole research run.
        let raw = "I cannot verify this page because no sources were provided.";
        let r = parse_verify_report(raw);
        assert_eq!(r.score, 0.0);
        assert!(r.items.is_empty());
    }

    #[test]
    fn verify_report_skips_section_when_all_supported() {
        // All-clean page → no `## Verification` clutter at the
        // bottom. Only flagged items trigger the section.
        let r = VerifyReport {
            score: 1.0,
            items: vec![VerifyItem {
                claim: "X".into(),
                citation: Some(1),
                verdict: Verdict::Supported,
                note: None,
            }],
        };
        assert!(r.render_flagged_section().is_none());
    }

    #[test]
    fn verify_report_renders_flagged_items() {
        let r = VerifyReport {
            score: 0.5,
            items: vec![
                VerifyItem {
                    claim: "Supported claim".into(),
                    citation: Some(1),
                    verdict: Verdict::Supported,
                    note: None,
                },
                VerifyItem {
                    claim: "Hallucinated claim".into(),
                    citation: Some(3),
                    verdict: Verdict::Unsupported,
                    note: Some("source does not say this".into()),
                },
                VerifyItem {
                    claim: "Uncited assertion".into(),
                    citation: None,
                    verdict: Verdict::NoCitation,
                    note: None,
                },
            ],
        };
        let section = r
            .render_flagged_section()
            .expect("flagged section expected");
        assert!(section.contains("## Verification"));
        assert!(section.contains("unsupported"));
        assert!(section.contains("Hallucinated claim"));
        assert!(section.contains("source does not say this"));
        assert!(section.contains("no citation"));
        assert!(section.contains("Uncited assertion"));
        // Supported items are NOT listed — only flagged ones.
        assert!(!section.contains("Supported claim"));
    }

    #[test]
    fn parse_verify_clamps_score_to_unit_interval() {
        // Hostile / buggy LLM returns score > 1.0 or negative —
        // clamp rather than passing nonsense through to frontmatter.
        let r = parse_verify_report(r#"{"score": 1.5, "items": []}"#);
        assert!((r.score - 1.0).abs() < 1e-6);
        let r2 = parse_verify_report(r#"{"score": -0.3, "items": []}"#);
        assert_eq!(r2.score, 0.0);
    }

    // ── parse_score_and_notes ─────────────────────────────────────

    #[test]
    fn parse_score_handles_decimal() {
        let (s, n) = parse_score_and_notes("score: 0.7\n\nWhat's covered: …");
        assert!((s - 0.7).abs() < 1e-6);
        assert!(n.starts_with("What's covered"));
    }

    #[test]
    fn parse_score_handles_case_insensitive() {
        let (s, _) = parse_score_and_notes("Score: 0.85");
        assert!((s - 0.85).abs() < 1e-6);
        let (s2, _) = parse_score_and_notes("SCORE: 0.42");
        assert!((s2 - 0.42).abs() < 1e-6);
    }

    #[test]
    fn parse_score_handles_fraction_form() {
        let (s, _) = parse_score_and_notes("score: 7/10\nnotes here");
        assert!((s - 0.7).abs() < 1e-6);
    }

    #[test]
    fn parse_score_handles_percent_form() {
        let (s, _) = parse_score_and_notes("score: 75%\nnotes");
        assert!((s - 0.75).abs() < 1e-6);
    }

    #[test]
    fn parse_score_clamps_above_one() {
        let (s, _) = parse_score_and_notes("score: 1.4\nover-eager LLM");
        assert!((s - 1.0).abs() < 1e-6);
    }

    #[test]
    fn parse_score_defaults_to_zero_on_failure() {
        let (s, n) = parse_score_and_notes("I think the answer is great");
        assert_eq!(s, 0.0);
        // Whole text becomes notes when no score line is found.
        assert!(n.contains("great"));
    }

    #[test]
    fn parse_score_finds_inline_when_first_line_isnt_score() {
        let (s, _) = parse_score_and_notes(
            "Coverage looks decent.\nSeveral sources agree.\nscore: 0.6\nnotes",
        );
        assert!((s - 0.6).abs() < 1e-6);
    }

    #[test]
    fn parse_score_skips_leading_blanks() {
        let (s, _) = parse_score_and_notes("\n\n   \nscore: 0.55");
        assert!((s - 0.55).abs() < 1e-6);
    }

    // ── sanitize_slug ──────────────────────────────────────────────

    #[test]
    fn slug_handles_thai_to_default() {
        // LLM SHOULD return ASCII; if it doesn't, we degrade to
        // "research" rather than emit non-ASCII filenames.
        // This asserted `"research"` — the bug, written down as the spec.
        assert_eq!(sanitize_slug("ค้นหาข่าว"), "ค้นหาข่าว");
        assert_ne!(sanitize_slug("ค้นหาข่าว"), sanitize_slug("ราคาทองคำ"));
        assert_eq!(sanitize_slug("   "), "research");
    }

    #[test]
    fn slug_kebab_cases_words() {
        assert_eq!(
            sanitize_slug("Quantum Error Correction"),
            "quantum-error-correction"
        );
        assert_eq!(sanitize_slug("tavily_api_usage"), "tavily-api-usage");
    }

    #[test]
    fn slug_strips_leading_trailing_hyphens() {
        assert_eq!(sanitize_slug("--alpha-beta--"), "alpha-beta");
    }

    #[test]
    fn slug_collapses_consecutive_separators() {
        assert_eq!(sanitize_slug("foo   bar___baz"), "foo-bar-baz");
    }

    // ── snippet ────────────────────────────────────────────────────

    #[test]
    fn snippet_truncates_at_char_boundary() {
        let s = snippet("hello world this is too long", 11);
        assert!(s.ends_with('…'));
        // Truncation must not split a multibyte char.
        let _ = s.chars().count(); // doesn't panic
    }

    #[test]
    fn snippet_passes_through_short_strings() {
        assert_eq!(snippet("short", 100), "short");
    }

    // ── plan_pages prompt directives ───────────────────────────────

    /// M6.39.6: pin the "ceiling not target" framing of max_pages so a
    /// future "smooth out the wording" refactor can't regress to the
    /// original "Output N pages" form. User reported the LLM was
    /// over-splitting topics to fill the quota — page count must be
    /// honest about source density, not a target.
    #[test]
    fn plan_pages_prompt_treats_max_as_ceiling_not_target() {
        let sources = vec![ResearchSource {
            index: 1,
            title: "T".into(),
            url: "https://x.example".into(),
            body: "b".into(),
        }];
        let prompt = build_plan_pages_prompt("q", &sources, 5);
        let lower = prompt.to_ascii_lowercase();
        assert!(
            lower.contains("ceiling, not a target"),
            "prompt must frame max_pages as a ceiling"
        );
        assert!(
            lower.contains("output fewer"),
            "prompt must explicitly permit fewer pages"
        );
        // "Reject thin pages" rule guards against quota-padding.
        assert!(
            lower.contains("reject thin pages"),
            "prompt must forbid thin pages padded to hit count"
        );
        // The numeric ceiling is still mentioned (was 5 in this test).
        assert!(prompt.contains("5 pages maximum"));
    }

    // ── plan_pages JSON parser ─────────────────────────────────────

    #[test]
    fn parse_page_plan_basic() {
        let raw = r#"[
            {"slug": "concept", "title": "Concept", "topic": "Core idea", "source_indices": [1, 2]},
            {"slug": "karpathy", "title": "Karpathy", "topic": "About him", "source_indices": [3]}
        ]"#;
        let plan = parse_page_plan(raw, 7, 5);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].slug, "concept");
        assert_eq!(plan[0].source_indices, vec![1, 2]);
        assert_eq!(plan[1].slug, "karpathy");
    }

    #[test]
    fn parse_page_plan_strips_markdown_fence() {
        // Real models often wrap JSON in ```json ... ``` despite the
        // "no code fence" instruction. The first `[` and last `]`
        // anchor the parse, so the fence prefix/suffix is harmless.
        let raw = "Here's the plan:\n```json\n[{\"slug\":\"foo\",\"title\":\"Foo\",\"topic\":\"a\",\"source_indices\":[1]}]\n```\nDone.";
        let plan = parse_page_plan(raw, 7, 5);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].slug, "foo");
    }

    #[test]
    fn parse_page_plan_caps_at_max_pages() {
        let raw = r#"[
            {"slug":"a","title":"A","topic":"","source_indices":[1]},
            {"slug":"b","title":"B","topic":"","source_indices":[1]},
            {"slug":"c","title":"C","topic":"","source_indices":[1]},
            {"slug":"d","title":"D","topic":"","source_indices":[1]}
        ]"#;
        let plan = parse_page_plan(raw, 2, 5);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].slug, "a");
        assert_eq!(plan[1].slug, "b");
    }

    #[test]
    fn parse_page_plan_filters_invalid_source_indices() {
        // Sources have indices 1..=3; LLM emits 5 (out of range)
        // and 0 (invalid). Both filtered.
        let raw = r#"[
            {"slug":"a","title":"A","topic":"","source_indices":[1, 0, 5, 2]}
        ]"#;
        let plan = parse_page_plan(raw, 7, 3);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].source_indices, vec![1, 2]);
    }

    #[test]
    fn parse_page_plan_dedupes_slugs() {
        let raw = r#"[
            {"slug":"foo","title":"Foo","topic":"","source_indices":[1]},
            {"slug":"foo","title":"Foo Again","topic":"","source_indices":[2]}
        ]"#;
        let plan = parse_page_plan(raw, 7, 3);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].title, "Foo");
    }

    #[test]
    fn parse_page_plan_drops_pages_with_no_sources() {
        let raw = r#"[
            {"slug":"a","title":"A","topic":"","source_indices":[]},
            {"slug":"b","title":"B","topic":"","source_indices":[1]}
        ]"#;
        let plan = parse_page_plan(raw, 7, 3);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].slug, "b");
    }

    #[test]
    fn parse_page_plan_returns_empty_on_bad_json() {
        assert!(parse_page_plan("totally not json", 7, 3).is_empty());
        assert!(parse_page_plan("", 7, 3).is_empty());
        assert!(parse_page_plan("[ malformed ]", 7, 3).is_empty());
    }

    #[test]
    fn parse_page_plan_normalizes_slug() {
        // sanitize_slug lowercases + kebab-cases
        let raw =
            r#"[{"slug":"Andrej Karpathy","title":"Karpathy","topic":"","source_indices":[1]}]"#;
        let plan = parse_page_plan(raw, 7, 3);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].slug, "andrej-karpathy");
    }

    #[test]
    fn parse_page_plan_skips_unparseable_slug_fallback() {
        // A slug with nothing usable in it falls back to "research", which
        // names no topic, and is skipped.
        let raw = r#"[{"slug":"!!! ???","title":"X","topic":"","source_indices":[1]}]"#;
        assert!(parse_page_plan(raw, 7, 3).is_empty());
        // This test used to feed a Thai slug here and require the plan to be
        // empty: a `--legacy` run planned no page for any non-ASCII topic.
        let raw = r#"[{"slug":"ค้นหา","title":"X","topic":"","source_indices":[1]}]"#;
        let plan = parse_page_plan(raw, 7, 3);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].slug, "ค้นหา");
    }

    /// M6.39.5: pin the synthesize prompt's "lead with abstract"
    /// instruction. The KMS auto-index pulls a page's summary from
    /// `first_meaningful_line(body)`; that line MUST be substantive
    /// prose (the LLM-written abstract) for the index to be useful
    /// to the next session deciding which page to KmsRead. If this
    /// instruction drifts away the index regresses to "Research:
    /// <query>"-style summaries that signal nothing.
    #[test]
    fn synthesize_prompt_requires_abstract_first() {
        let prompt = build_synthesize_prompt("what is OBON", &[]);
        // The instruction must mention "abstract" specifically.
        let lower = prompt.to_ascii_lowercase();
        assert!(
            lower.contains("abstract"),
            "synthesize prompt must require an abstract"
        );
        // It must mention that the abstract goes BEFORE headings
        // (otherwise LLM may emit `## Overview\n<abstract>` which
        // first_meaningful_line strips back to `Overview`).
        assert!(
            lower.contains("before any `##`") || lower.contains("before any ##"),
            "synthesize prompt must say abstract goes before ## headings"
        );
        // It must explicitly forbid leading with `# Research:` or a
        // heading — the pre-fix body shape produced exactly that.
        assert!(
            lower.contains("do not start with a heading"),
            "synthesize prompt must forbid heading-first opening"
        );
    }
}
