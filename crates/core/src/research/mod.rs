//! Background research: `/research <query>` turns the open web into
//! zettelkasten notes inside a KMS, outside the agent loop.
//!
//! One run (`pipeline_v2`, dev-plan/58):
//! 1. **Search** the query and a freshness-filtered variant of it, then
//!    rank the hits — the subject's own site and reference works first,
//!    at most two pages per domain ([`source_quality`]).
//! 2. **Digest** each page exactly once ([`digest`]): entities plus
//!    claims, every claim anchored to a verbatim quote that must appear
//!    in the fetched body or the claim is dropped. Cached per URL under
//!    `<kms>/.research/digests/`.
//! 3. **Gap rounds** ask the worker model for searches that would close
//!    what the entity table is missing, and stop when a round's share of
//!    new entities falls below `novelty_threshold` (deterministic — no
//!    LLM self-scoring), when nothing productive is left, at `max_iter`,
//!    or when 60% of the time budget is gone.
//! 4. **Plan** top-down ([`plan`]): the topic page's outline, then the
//!    notes it must link for depth, then claims attached by entity tag.
//! 5. **Write** ([`write`]) the topic page first, then one call per
//!    child note in parallel, each seeing the topic page's opening.
//!    Links are then made deterministically, citations resolved against
//!    the per-KMS registry, and cited sources archived.
//!
//! Everything lands in the KMS — pages, `sources/`, a `runs/` log —
//! never in the chat context. `pipeline` is the retired M6.39 page
//! pipeline, still reachable with `--legacy`.

pub mod digest;
pub mod graph;
pub mod jobs;
pub mod kms_writer;
pub mod llm_calls;
pub mod pipeline;
pub mod pipeline_v2;
pub mod plan;
pub mod registry;
pub mod source_quality;
#[cfg(test)]
pub(crate) mod test_helpers;
pub mod write;

use crate::cancel::CancelToken;
use crate::error::Result;
use futures::FutureExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// How far past its time budget a job may run before the watchdog fails
/// it. The pipeline deliberately lets the plan/write phases overrun — a
/// late page beats discarding thirty digested sources — so the ceiling
/// has to sit well above the budget to leave that room, while still
/// being a ceiling.
const BUDGET_GRACE: u32 = 3;

/// Render a budget the way the error message wants to read it.
fn format_budget(d: Duration) -> String {
    let mins = d.as_secs() / 60;
    if mins >= 60 {
        format!("for over {}h", mins / 60)
    } else {
        format!("for over {mins}m")
    }
}

/// Stable ID for a background research job. Format `research-<8-hex>`.
pub type JobId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// Spawned but pipeline hasn't started yet (rare — spawn → run is
    /// near-instant, but we transition through this so the job is
    /// queryable the moment `start` returns).
    Pending,
    /// Running. `JobView::phase` is the current step.
    Running,
    /// Pipeline completed normally — synthesized result is in KMS,
    /// `JobView::result_path` points at the raw note.
    Done,
    /// User called `cancel`, or main process is shutting down.
    Cancelled,
    /// Pipeline aborted with an error. Inspect `JobView::error`.
    Failed,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Pending => "pending",
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Failed => "failed",
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatus::Done | JobStatus::Cancelled | JobStatus::Failed
        )
    }
}

/// Tunable knobs for one research job. Defaults aim for "decent answer
/// in ~3-5 minutes with a mid-tier model"; user overrides via
/// `/research --min-iter / --max-iter / --score-threshold`.
#[derive(Debug, Clone)]
pub struct JobConfig {
    /// Minimum iterations before the score threshold can short-circuit
    /// the loop. Hard floor — Rust ignores any "score >= threshold"
    /// signal before this. Default 2.
    pub min_iter: u32,
    /// Maximum iterations regardless of score. Hard ceiling. Default 8.
    pub max_iter: u32,
    /// `--legacy` only: LLM-reported completeness score that ends the
    /// v1 loop early. v2 stops on `novelty_threshold` instead.
    pub score_threshold: f32,
    /// Subtopics per iteration. Default 4 — balances breadth vs cost.
    pub subtopics_per_iter: u32,
    /// Top-N pages per subtopic to read in detail (WebFetch). Default 3.
    pub fetch_top_n: u32,
    /// `--legacy` only: cap on pages the v1 pipeline emits. v2 uses
    /// `max_notes`.
    pub max_pages: u32,
    /// Per-LLM-call timeout (default 180s in v2). Research
    /// synthesis is a known long-running feature; this also feeds the
    /// provider's per-chunk idle ceiling via
    /// `StreamRequest::stream_chunk_timeout_override`, so it bypasses
    /// the user's normal `stream_chunk_timeout_secs` setting.
    pub llm_timeout: Duration,
    /// Total wall-clock budget. Search rounds stop once 60% of it is
    /// spent; the plan/write phases then run to completion rather than
    /// throwing away everything the run already read. Default 25 min.
    pub time_budget: Duration,
    /// Target KMS name. `None` = auto-derive a slug from the query and
    /// create the KMS if missing.
    pub kms_target: Option<String>,
    /// v2: ceiling on notes per run incl. the topic page. Clamped to
    /// [`plan::HARD_MAX_NOTES`]; default 30.
    pub max_notes: u32,
    /// v2: stop when a round's share of new entities falls below this
    /// (after `min_iter`). Default 0.35.
    pub novelty_threshold: f32,
    /// v2: never rewrite an existing note — add a dated `## Update`.
    pub append: bool,
    /// v2: digest + plan, write only the run log.
    pub dry_run: bool,
    /// Route to the M6.39 page pipeline instead of v2.
    pub legacy: bool,
    /// v2: model for the per-source digests (mechanical extraction —
    /// a fast non-reasoning model is right here). `None` = same model.
    pub digest_model: Option<String>,
    /// v2: language of note bodies, claim text and titles (BCP-47-ish
    /// code such as `th`, `en`, `ja`). Technical terms stay English.
    pub language: String,
    /// v2 refresh mode: the existing note this run must `update`; no
    /// MOC is written and the result path is that note.
    pub refresh_slug: Option<String>,
    /// Files-tab "Add to KMS as atomic notes": the alias of an archived
    /// `sources/<alias>.md` in `kms_target`. The run digests that one
    /// document (in windows) instead of searching the web, and writes
    /// the topic page over the ingest stub `pages/<alias>.md`.
    pub local_source: Option<String>,
    /// Fix the topic page's slug instead of deriving one from the query
    /// (viewer "Create page": the link `[[slug|text]]` is written into the
    /// source page before the run starts, so the run must land there).
    pub topic_slug: Option<String>,
}

/// Date anchor for every research prompt. The worker model's own
/// sense of "latest" is frozen at its training cutoff; without this it
/// writes search queries for last year's releases and reports them as
/// current (a 2026 run on "Chinese AI companies" pinned every query to
/// 2025 and never found DeepSeek V4 / Qwen 3.8).
/// A prompt in two parts: what every call of one batch says, and what only
/// this call says (dev-plan/64 P2.6).
///
/// A run writes ~25 child notes and digests ~25 sources, and each prompt
/// used to open with its own item and end with ~6 KB of rules — so no two
/// shared a prefix, and ≈150k of ≈185k input chars in a measured run were
/// the same boilerplate billed 25 times at full price. `shared` goes out as
/// the system prompt: Anthropic caches that block, and the providers that
/// cache by prefix see an identical opening on every call after the first.
/// It must therefore be byte-identical across the batch — nothing about
/// the item may leak into it.
pub struct SplitPrompt {
    pub shared: String,
    pub item: String,
}

impl SplitPrompt {
    pub fn joined(&self) -> String {
        format!("{}\n\n{}", self.shared, self.item)
    }
}

pub fn today_context() -> String {
    let today = kms_writer::today_str();
    let year = today.get(..4).unwrap_or("");
    format!(
        "Today is {today}. Your training knowledge may be out of date: never \
         assume a version, release, price or leader you remember is still the \
         latest — only the sources say what is current. Prefer newer sources \
         when claims conflict, and treat {year} as the current year."
    )
}

/// Prompt sentence for the output language. Thai is the product default;
/// technical vocabulary is kept in English in every language so notes
/// stay searchable and don't transliterate model / API names.
/// Which language a text is written in, as far as the writer needs to know:
/// `th`, `en`, or `query` ("write in the language you were asked in") for
/// anything else. Thai wins on a modest share of the letters — a Thai
/// document about software is a third English by character count and is
/// still a Thai document.
pub fn detect_language(text: &str) -> &'static str {
    let (mut thai, mut latin, mut other) = (0usize, 0usize, 0usize);
    for c in text.chars().filter(|c| c.is_alphabetic()) {
        match c {
            '\u{0E00}'..='\u{0E7F}' => thai += 1,
            c if c.is_ascii() => latin += 1,
            _ => other += 1,
        }
    }
    let letters = thai + latin + other;
    if letters == 0 {
        "query"
    } else if thai * 100 >= letters * 15 {
        "th"
    } else if other * 100 >= letters * 15 {
        "query"
    } else {
        "en"
    }
}

/// dev-plan/64 D6: what `auto` means. It was the literal `th`, so an English
/// paper added to a KMS came back as Thai notes. An explicit `--lang` never
/// gets here; the `research_language` setting pins it; otherwise the
/// document being ingested decides, or failing that the query.
pub fn resolve_language(configured: &str, query: &str, document: Option<&str>) -> String {
    let c = configured.trim().to_ascii_lowercase();
    if !c.is_empty() && c != "auto" {
        return c;
    }
    if let Some(pinned) = crate::config::AppConfig::load()
        .ok()
        .and_then(|cfg| cfg.research_language)
    {
        return pinned;
    }
    detect_language(document.unwrap_or(query)).to_string()
}

pub fn language_rule(lang: &str) -> String {
    let keep = "Keep technical terms, product and model names, API and \
                code identifiers, and units in English — do not translate \
                or transliterate them.";
    match lang.trim().to_ascii_lowercase().as_str() {
        "th" | "thai" => format!("Write in Thai (ภาษาไทย). {keep}"),
        "en" | "english" => "Write in English.".to_string(),
        "query" | "auto" => "Write in the same language as the query.".to_string(),
        other => format!("Write in the language with code `{other}`. {keep}"),
    }
}

impl Default for JobConfig {
    fn default() -> Self {
        Self {
            min_iter: 2,
            max_iter: 4,
            score_threshold: 0.80,
            subtopics_per_iter: 4,
            fetch_top_n: 3,
            max_pages: 7,
            llm_timeout: Duration::from_secs(180),
            time_budget: Duration::from_secs(25 * 60),
            kms_target: None,
            max_notes: 30,
            novelty_threshold: 0.35,
            append: false,
            dry_run: false,
            legacy: false,
            digest_model: None,
            language: "auto".into(),
            refresh_slug: None,
            local_source: None,
            topic_slug: None,
        }
    }
}

/// Default digest model when none is given: the fast sibling of a
/// reasoning model when one is known, else the main model (`None`).
/// Product rule (2026-09-07): research runs on the model the user
/// chose. We never pick a different provider on their behalf — a
/// worker model is used only when `--worker-model` names one. The
/// speed difference is real (gemini-2.5-flash answers 4 concurrent
/// Thai calls in 5–7 s; deepseek-v4-flash queues them ~50 s apart) and
/// is surfaced as a log hint, not a silent switch.
pub fn default_digest_model(_main_model: &str) -> Option<String> {
    None
}

/// Flags shared by the REPL and GUI `/research` dispatchers.
#[derive(Debug, Clone, Default)]
pub struct StartFlags {
    pub kms_target: Option<String>,
    pub min_iter: Option<u32>,
    pub max_iter: Option<u32>,
    pub score_threshold_pct: Option<u32>,
    pub max_pages: Option<u32>,
    pub budget_time_secs: Option<u64>,
    pub max_notes: Option<u32>,
    pub novelty_pct: Option<u32>,
    pub append: bool,
    pub dry_run: bool,
    pub legacy: bool,
    pub digest_model: Option<String>,
    /// `--lang`; `None` keeps the default (`th`).
    pub language: Option<String>,
    /// KMSs attached to the session (`config.kms_active`), most
    /// recently attached last. Zettelkasten default: research goes into
    /// the last attached KMS unless `--kms <name>` or `--kms new`.
    pub attached_kms: Vec<String>,
}

impl JobConfig {
    pub fn from_flags(f: StartFlags) -> Self {
        let mut cfg = Self::default();
        cfg.kms_target = match f.kms_target.as_deref().map(str::trim) {
            Some("new") | Some("-") => None,
            Some(name) if !name.is_empty() => Some(name.to_string()),
            _ => f.attached_kms.last().cloned(),
        };
        if let Some(v) = f.min_iter {
            cfg.min_iter = v;
        }
        if let Some(v) = f.max_iter {
            cfg.max_iter = v;
        }
        if let Some(pct) = f.score_threshold_pct {
            cfg.score_threshold = (pct as f32 / 100.0).clamp(0.0, 1.0);
        }
        if let Some(v) = f.max_pages {
            cfg.max_pages = v;
        }
        if let Some(secs) = f.budget_time_secs {
            cfg.time_budget = Duration::from_secs(secs);
        }
        if let Some(v) = f.max_notes {
            cfg.max_notes = v.clamp(1, plan::HARD_MAX_NOTES);
        }
        if let Some(pct) = f.novelty_pct {
            cfg.novelty_threshold = (pct as f32 / 100.0).clamp(0.0, 1.0);
        }
        cfg.append = f.append;
        cfg.dry_run = f.dry_run;
        cfg.legacy = f.legacy;
        cfg.digest_model = f.digest_model;
        if let Some(l) = f
            .language
            .map(|l| l.trim().to_ascii_lowercase())
            .filter(|l| !l.is_empty())
        {
            cfg.language = l;
        }
        if cfg.legacy && f.max_iter.is_none() {
            cfg.max_iter = 8;
        }
        cfg
    }
}

/// Read-only snapshot of one job for status / list views.
#[derive(Debug, Clone)]
pub struct JobView {
    pub id: JobId,
    pub query: String,
    pub status: JobStatus,
    pub phase: String,
    pub iterations_done: u32,
    pub source_count: u32,
    /// Most recent `score: 0.X` from `evaluate()`. `None` until iter 1
    /// finishes.
    pub last_score: Option<f32>,
    pub started_at: std::time::SystemTime,
    pub finished_at: Option<std::time::SystemTime>,
    pub kms_target: Option<String>,
    /// On `Done`, the relative path inside the target KMS (e.g.
    /// `2026-05-09-obon-festival.md`). `None` until completion.
    pub result_page: Option<String>,
    pub error: Option<String>,
    /// dev-plan/64 P5.4: what this run has spent so far, refreshed as
    /// it goes. `None` before the first model call returns.
    ///
    /// Priced here rather than in the GUI because only the engine has
    /// the catalogue — and refreshed on a throttle, because pricing
    /// re-parses the baseline catalogue and reads two files, which is
    /// not something to do once per note.
    pub cost_usd: Option<f64>,
    /// dev-plan/64 P5.4: how many pages the run wrote. `None` until it
    /// finishes, because a half-finished count invites reading a run
    /// that died as a run that produced something.
    pub pages_written: Option<u32>,
    /// dev-plan/64 P5.5: the slash command that would run this job
    /// again, built when it was registered.
    ///
    /// Built here rather than reconstructed by the GUI, which cannot:
    /// a refresh's `query` is the display string `refresh: <title>`,
    /// not anything you could type. `None` where a correct retry is
    /// not expressible — an ingest whose file path the job never kept,
    /// a page researched from a selection that lives in another page's
    /// prose. A button that silently does the wrong thing is worse
    /// than no button.
    pub retry_command: Option<String>,
}

impl JobView {
    /// `mm:ss` (or `h:mm:ss`) since the job started, frozen at finish.
    pub fn elapsed_str(&self) -> String {
        let end = self.finished_at.unwrap_or_else(std::time::SystemTime::now);
        let s = end
            .duration_since(self.started_at)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
        if h > 0 {
            format!("{h}:{m:02}:{sec:02}")
        } else {
            format!("{m:02}:{sec:02}")
        }
    }
}

/// The command that would run this job again, or `None` when a
/// correct one cannot be written.
///
/// A retry is cheap where it is possible at all — digests are cached
/// per source, so a second attempt pays only for what the first never
/// got to — which is exactly why it is worth offering, and exactly why
/// it must re-run the *same* thing rather than something close to it.
fn retry_command_for(query: &str, config: &JobConfig) -> Option<String> {
    let kms = config.kms_target.as_deref();
    if let Some(slug) = &config.refresh_slug {
        let k = kms?;
        return Some(format!(
            "/research refresh {} {slug}",
            crate::repl::quote_slash_arg(k)
        ));
    }
    // An ingest kept the alias, not the path it was read from; a page
    // researched from a selection needs the selection, which lives in
    // another page's prose. Neither is reconstructible from here.
    if config.local_source.is_some() || config.topic_slug.is_some() {
        return None;
    }
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    Some(match kms {
        Some(k) => format!("/research --kms {} {q}", crate::repl::quote_slash_arg(k)),
        None => format!("/research {q}"),
    })
}

/// Thread-safe registry of running + recently-completed jobs.
///
/// Ownership: process-wide singleton, accessed via [`manager`]. Same
/// pattern as `goal_state` and `schedule` for symmetry, but each
/// research job carries its own `CancelToken` so cancellation is
/// per-job, not global.
pub struct ResearchManager {
    jobs: RwLock<HashMap<JobId, Arc<RwLock<JobInner>>>>,
    /// Broadcaster wired by `gui.rs` at session bootstrap (M6.39.3).
    /// Each phase change / iteration record / finalize fires this
    /// with a JSON payload shape identical to what
    /// `build_research_update_payload()` produces. CLI uses are no-op
    /// (broadcaster unset) — phases are visible via `/research list`
    /// and the auto-print on next REPL prompt.
    broadcaster: Mutex<Option<Box<dyn Fn(&[JobView]) + Send + Sync>>>,
}

#[derive(Debug)]
struct JobInner {
    view: JobView,
    cancel: CancelToken,
    /// Wall-clock deadline derived from `JobConfig::time_budget`.
    /// Pipeline checks this before each iteration; on overrun, sets
    /// `JobStatus::Failed` with `error = "time budget exhausted"`.
    deadline: Instant,
}

impl Default for ResearchManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ResearchManager {
    pub fn new() -> Self {
        Self {
            jobs: RwLock::new(HashMap::new()),
            broadcaster: Mutex::new(None),
        }
    }

    /// Install a broadcaster called after every state-mutating method
    /// (update_phase, record_iteration, finalize, cancel). The closure
    /// receives a fresh snapshot of all jobs in newest-first order.
    /// Same pattern as `goal_state::set_broadcaster`.
    pub fn set_broadcaster<F>(&self, f: F)
    where
        F: Fn(&[JobView]) + Send + Sync + 'static,
    {
        *self.broadcaster.lock().unwrap() = Some(Box::new(f));
    }

    fn broadcast(&self) {
        if let Some(cb) = self.broadcaster.lock().unwrap().as_ref() {
            let snapshot = self.list();
            cb(&snapshot);
        }
    }

    /// Register a new job and return its ID + cancel handle. Caller
    /// then drives the pipeline asynchronously and reports progress
    /// via [`update_phase`] / [`finalize`].
    pub fn register(&self, query: String, config: &JobConfig) -> (JobId, CancelToken) {
        let retry_command = retry_command_for(&query, config);
        let id = format!(
            "research-{}",
            uuid::Uuid::new_v4()
                .to_string()
                .split('-')
                .next()
                .unwrap_or("anon")
                .to_string()
        );
        let cancel = CancelToken::new();
        let inner = JobInner {
            view: JobView {
                id: id.clone(),
                query,
                status: JobStatus::Pending,
                phase: "spawned".into(),
                iterations_done: 0,
                source_count: 0,
                last_score: None,
                started_at: std::time::SystemTime::now(),
                finished_at: None,
                kms_target: config.kms_target.clone(),
                result_page: None,
                error: None,
                cost_usd: None,
                pages_written: None,
                retry_command,
            },
            cancel: cancel.clone(),
            deadline: Instant::now() + config.time_budget,
        };
        self.jobs
            .write()
            .unwrap()
            .insert(id.clone(), Arc::new(RwLock::new(inner)));
        self.broadcast();
        self.spawn_watchdog(&id, config.time_budget);
        (id, cancel)
    }

    /// Fail a job that is still running long after its budget.
    ///
    /// `is_over_budget` is only ever *polled*, and the pipeline stops
    /// polling it once it leaves the search rounds — deliberately, so a
    /// run that already read thirty sources is not thrown away for
    /// crossing a timer mid-write. The cost of that choice was a job with
    /// no ceiling at all: a task that hangs, or panics and unwinds past
    /// `run_job`'s finalize, leaves the job `Running` for as long as the
    /// process lives. One sat for six and three-quarter hours on a
    /// twenty-five minute budget, with nothing in the UI to say it had
    /// stopped and no way to clear it.
    ///
    /// So the ceiling is the budget times [`BUDGET_GRACE`], not the
    /// budget: late is still allowed, forever is not.
    fn spawn_watchdog(&self, id: &JobId, budget: Duration) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // No runtime (tests, sync REPL paths) — nothing to spawn on.
            return;
        };
        let id = id.clone();
        handle.spawn(async move {
            tokio::time::sleep(budget.saturating_mul(BUDGET_GRACE)).await;
            let mgr = manager();
            if mgr.get(&id).is_some_and(|v| !v.status.is_terminal()) {
                mgr.cancel(&id);
                mgr.finalize(
                    &id,
                    JobStatus::Failed,
                    None,
                    Some(format!(
                        "no progress {} after the time budget — giving up",
                        format_budget(budget.saturating_mul(BUDGET_GRACE))
                    )),
                );
            }
        });
    }

    /// What the run has spent so far. Only ever moves forward, so a
    /// late `None` (a ledger read that found nothing) cannot blank a
    /// figure the owner has already seen.
    pub fn update_cost(&self, id: &str, cost: Option<f64>) {
        let Some(cost) = cost else { return };
        let changed = if let Some(j) = self.jobs.read().unwrap().get(id).cloned() {
            let mut g = j.write().unwrap();
            let moved = g.view.cost_usd != Some(cost);
            g.view.cost_usd = Some(cost);
            moved
        } else {
            false
        };
        if changed {
            self.broadcast();
        }
    }

    /// How many pages the run wrote, recorded once, at the end.
    pub fn record_pages_written(&self, id: &str, n: u32) {
        if let Some(j) = self.jobs.read().unwrap().get(id).cloned() {
            j.write().unwrap().view.pages_written = Some(n);
        }
    }

    pub fn update_phase(&self, id: &str, phase: impl Into<String>) {
        let changed = if let Some(j) = self.jobs.read().unwrap().get(id).cloned() {
            let mut g = j.write().unwrap();
            g.view.phase = phase.into();
            g.view.status = JobStatus::Running;
            true
        } else {
            false
        };
        if changed {
            self.broadcast();
        }
    }

    pub fn record_iteration(
        &self,
        id: &str,
        iteration: u32,
        source_count: u32,
        score: Option<f32>,
    ) {
        let changed = if let Some(j) = self.jobs.read().unwrap().get(id).cloned() {
            let mut g = j.write().unwrap();
            g.view.iterations_done = iteration;
            g.view.source_count = source_count;
            // Always assign — `None` clears, `Some` sets. Pre-fix
            // this only assigned on `Some`, which caused the previous
            // iter's score to bleed into the new iter's broadcast
            // snapshots: pipeline records the iter-start broadcast
            // before evaluate runs, so during that window
            // iterations_done points at iter N while last_score still
            // carries iter N-1's value. The frontend's per-iter
            // history then labelled the wrong score under iter N.
            // Pipeline now passes `None` at iter-start (no eval yet)
            // and the real score at the post-evaluate broadcast.
            g.view.last_score = score;
            true
        } else {
            false
        };
        if changed {
            self.broadcast();
        }
    }

    /// Mark a job terminal (Done / Cancelled / Failed) with optional
    /// result path or error. Idempotent — second call on a terminal
    /// job is a no-op.
    pub fn finalize(
        &self,
        id: &str,
        status: JobStatus,
        result_page: Option<String>,
        error: Option<String>,
    ) {
        debug_assert!(
            status.is_terminal(),
            "finalize called with non-terminal status"
        );
        let changed = if let Some(j) = self.jobs.read().unwrap().get(id).cloned() {
            let mut g = j.write().unwrap();
            if g.view.status.is_terminal() {
                false
            } else {
                g.view.status = status;
                g.view.result_page = result_page;
                g.view.error = error;
                g.view.finished_at = Some(std::time::SystemTime::now());
                g.view.phase = match status {
                    JobStatus::Done => "done".into(),
                    JobStatus::Cancelled => "cancelled".into(),
                    JobStatus::Failed => "failed".into(),
                    _ => g.view.phase.clone(),
                };
                true
            }
        } else {
            false
        };
        if changed {
            self.broadcast();
        }
    }

    /// Snapshot of one job. `None` if the id isn't known.
    pub fn get(&self, id: &str) -> Option<JobView> {
        self.jobs
            .read()
            .unwrap()
            .get(id)
            .map(|j| j.read().unwrap().view.clone())
    }

    /// Snapshot of every job (running + recently completed). Sorted by
    /// `started_at` descending so `/research list` shows newest first.
    pub fn list(&self) -> Vec<JobView> {
        let mut all: Vec<JobView> = self
            .jobs
            .read()
            .unwrap()
            .values()
            .map(|j| j.read().unwrap().view.clone())
            .collect();
        all.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        all
    }

    /// Signal a job's CancelToken. Pipeline checks the token between
    /// each await point and exits as `JobStatus::Cancelled`. Returns
    /// `false` if the id isn't known or the job is already terminal.
    pub fn cancel(&self, id: &str) -> bool {
        let Some(j) = self.jobs.read().unwrap().get(id).cloned() else {
            return false;
        };
        {
            let g = j.read().unwrap();
            if g.view.status.is_terminal() {
                return false;
            }
            g.cancel.cancel();
        }
        // Signalling the token is not enough on its own: it only lands if
        // something is still awaiting on this job's behalf. When the task
        // has already died, Cancel used to leave the job Running with no
        // way for the user to clear it. `finalize` ignores an already
        // terminal job, so the pipeline's own finalize still wins the race
        // if it is alive and exits cleanly.
        self.finalize(id, JobStatus::Cancelled, None, None);
        true
    }

    /// True if the job has exceeded its wall-clock budget. Pipeline
    /// checks this each iteration to short-circuit before running
    /// another expensive round of LLM + search calls.
    pub fn is_over_budget(&self, id: &str) -> bool {
        self.jobs
            .read()
            .unwrap()
            .get(id)
            .map(|j| Instant::now() > j.read().unwrap().deadline)
            .unwrap_or(false)
    }

    /// Drop terminal jobs older than `keep_recent`. Called periodically
    /// by the manager owner (or on `/research list` to clean up).
    pub fn prune_terminal(&self, keep_recent: Duration) {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(keep_recent)
            .unwrap_or(std::time::UNIX_EPOCH);
        self.jobs.write().unwrap().retain(|_, j| {
            let g = j.read().unwrap();
            !g.view.status.is_terminal() || g.view.finished_at.map(|t| t > cutoff).unwrap_or(true)
        });
    }
}

/// Process-wide manager. Lazy-initialized on first access.
pub fn manager() -> &'static ResearchManager {
    use std::sync::OnceLock;
    static M: OnceLock<ResearchManager> = OnceLock::new();
    M.get_or_init(ResearchManager::new)
}

/// Public entry point used by the slash dispatch in M6.39.2. Builds
/// the job, spawns the pipeline as a background task, returns the
/// JobId immediately. The caller doesn't await the task — status is
/// queried via the manager.
///
/// `provider` and `tools` are taken by Arc so the spawned task owns
/// independent handles. The pipeline closes over them for the duration
/// of the job.
pub async fn start(
    query: String,
    config: JobConfig,
    provider: Arc<dyn crate::providers::Provider>,
    model: String,
    digest_provider: Option<Arc<dyn crate::providers::Provider>>,
) -> Result<JobId> {
    let (id, cancel) = manager().register(query.clone(), &config);
    let id_for_task = id.clone();
    tokio::spawn(crate::workdir::Scope::capture().run(async move {
        run_job(
            id_for_task,
            cancel,
            query,
            config,
            provider,
            model,
            digest_provider,
            None,
        )
        .await;
    }));
    Ok(id)
}

/// Files-tab "Add to KMS as atomic notes": turn one archived source
/// (`<kms>/sources/<alias>.md`) into a topic page plus atomic child
/// notes through the research v2 digest → plan → write path. The job
/// shows in the Research sidebar like any run.
pub async fn start_ingest(
    kms: String,
    alias: String,
    base: JobConfig,
    provider: Arc<dyn crate::providers::Provider>,
    model: String,
    digest_provider: Option<Arc<dyn crate::providers::Provider>>,
) -> Result<JobId> {
    let kref = crate::kms::resolve(&kms)
        .ok_or_else(|| crate::error::Error::Tool(format!("no KMS named '{kms}'")))?;
    let path = kref.root.join("sources").join(format!("{alias}.md"));
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| crate::error::Error::Tool(format!("read {}: {e}", path.display())))?;
    let title = pipeline_v2::local_source_title(&raw, &alias);
    let mut cfg = base;
    cfg.kms_target = Some(kms);
    cfg.local_source = Some(alias);
    cfg.legacy = false;
    let (id, cancel) = manager().register(title.clone(), &cfg);
    let id_for_task = id.clone();
    tokio::spawn(crate::workdir::Scope::capture().run(async move {
        run_job(
            id_for_task,
            cancel,
            title,
            cfg,
            provider,
            model,
            digest_provider,
            None,
        )
        .await;
    }));
    Ok(id)
}

/// One job to completion, finalizing the manager entry either way.
async fn run_job(
    id: JobId,
    cancel: CancelToken,
    query: String,
    config: JobConfig,
    provider: Arc<dyn crate::providers::Provider>,
    model: String,
    digest_provider: Option<Arc<dyn crate::providers::Provider>>,
    tools: Option<Arc<dyn pipeline::ResearchTools>>,
) {
    let mut cfg = config;
    if cfg.legacy {
        // v1 prompts carry whole source bodies; keep its old ceiling.
        cfg.llm_timeout = cfg.llm_timeout.max(Duration::from_secs(900));
    }
    // The viewer's "create page" writes a `status: researching` placeholder
    // before the job starts, so the link it just made resolves at once.
    let placeholder = cfg.kms_target.clone().zip(cfg.topic_slug.clone());
    let run = async {
        if cfg.legacy {
            pipeline::run(&id, query, cfg, provider, model, cancel.clone()).await
        } else {
            let tools = tools.unwrap_or_else(pipeline::production_tools);
            let digest = digest_provider.map(|p| {
                let m = cfg.digest_model.clone().unwrap_or_else(|| model.clone());
                (p, m)
            });
            pipeline_v2::run_with_tools(
                &id,
                query,
                cfg,
                provider,
                model,
                cancel.clone(),
                tools,
                digest,
            )
            .await
        }
    };
    // A panic here used to unwind straight past the finalize below, so the
    // job stayed `Running` forever — the pipeline's most confusing failure
    // mode, because it looks exactly like a slow model and there is
    // nothing to read. Turn it into a job that says what happened.
    let run = llm_calls::track_usage(run);
    let outcome = match std::panic::AssertUnwindSafe(run).catch_unwind().await {
        Ok(outcome) => outcome,
        Err(panic) => {
            let what = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "no message".into());
            Err(crate::error::Error::Tool(format!(
                "research pipeline panicked: {what}"
            )))
        }
    };
    match outcome {
        Ok(result_page) => {
            manager().finalize(&id, JobStatus::Done, Some(result_page), None);
        }
        Err(e) => {
            let s = format!("{e}");
            let status = if cancel.is_cancelled() {
                JobStatus::Cancelled
            } else {
                JobStatus::Failed
            };
            manager().finalize(&id, status, None, Some(s));
            if let Some((kms, slug)) = placeholder {
                remove_abandoned_placeholder(&kms, &slug);
            }
        }
    }
}

/// Delete the placeholder page a run that did not finish left behind.
///
/// Nothing used to: `pages/herbert-simon.md` outlived the run that died
/// writing it, sat in the index and the graph advertising "this page is
/// being written by /research", and was read by every later planner as a
/// note that already covered Herbert Simon. Only a page that still says
/// `status: researching` is touched — a run that got as far as writing
/// the real page has replaced that, and keeps its work.
fn remove_abandoned_placeholder(kms: &str, slug: &str) {
    let Some(kref) = crate::kms::resolve(kms) else {
        return;
    };
    let Ok(path) = kref.page_path(slug) else {
        return;
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let (fm, _) = crate::kms::parse_frontmatter(&raw);
    if fm.get("status").map(|s| s.trim()) == Some("researching") {
        let _ = crate::kms::delete_page(&kref, slug);
    }
}

/// `/research refresh`: re-research existing notes of `kms` and merge
/// what is new into them. Jobs are registered up front (so `/research
/// list` shows the queue) and run one after another in a single task.
/// `slugs` empty ⇒ every non-MOC note whose `updated` is older than
/// `older_than_days` (or has none).
pub async fn start_refresh(
    kms: String,
    slugs: Vec<String>,
    older_than_days: u32,
    base: JobConfig,
    provider: Arc<dyn crate::providers::Provider>,
    model: String,
    digest_provider: Option<Arc<dyn crate::providers::Provider>>,
    tools: Option<Arc<dyn pipeline::ResearchTools>>,
) -> Result<Vec<(JobId, String)>> {
    let kref = crate::kms::resolve(&kms)
        .ok_or_else(|| crate::error::Error::Tool(format!("no KMS named '{kms}'")))?;
    let known = graph::load_known(&kref);
    let today = kms_writer::today_str();
    let targets: Vec<&graph::KnownNote> = if slugs.is_empty() {
        known
            .iter()
            // A hub, not a label. `kind: moc` also lands on a one-page
            // ingest — the topic-page branch writes it with an empty
            // `related:` — and filtering on the label hid every such
            // page from a refresh for good. What this wants to skip is
            // a page other pages hang off, which is the list, not the
            // name.
            .filter(|k| !k.has_children && !k.slug.starts_with('_'))
            .filter(|k| match &k.updated {
                Some(u) => days_between(u, &today) >= older_than_days as i64,
                None => true,
            })
            .collect()
    } else {
        let mut v = Vec::new();
        for s in &slugs {
            let k = known.iter().find(|k| &k.slug == s).ok_or_else(|| {
                crate::error::Error::Tool(format!("no note '{s}' in KMS '{kms}'"))
            })?;
            v.push(k);
        }
        v
    };
    if targets.is_empty() {
        return Err(crate::error::Error::Tool(format!(
            "nothing to refresh in '{kms}' (no note older than {older_than_days} days)"
        )));
    }
    let mut queue = Vec::new();
    for k in &targets {
        let mut cfg = base.clone();
        cfg.kms_target = Some(kms.clone());
        cfg.refresh_slug = Some(k.slug.clone());
        cfg.max_iter = cfg.max_iter.min(2);
        let query = k.title.clone();
        let (id, cancel) = manager().register(format!("refresh: {query}"), &cfg);
        queue.push((id, cancel, query, cfg, k.slug.clone()));
    }
    let ids: Vec<(JobId, String)> = queue
        .iter()
        .map(|(id, _, _, _, s)| (id.clone(), s.clone()))
        .collect();
    tokio::spawn(crate::workdir::Scope::capture().run(async move {
        for (id, cancel, query, cfg, _) in queue {
            run_job(
                id,
                cancel,
                query,
                cfg,
                provider.clone(),
                model.clone(),
                digest_provider.clone(),
                tools.clone(),
            )
            .await;
        }
    }));
    Ok(ids)
}

fn days_between(from_ymd: &str, to_ymd: &str) -> i64 {
    use chrono::NaiveDate;
    let p = |s: &str| NaiveDate::parse_from_str(s.get(..10).unwrap_or(s), "%Y-%m-%d").ok();
    match (p(from_ymd), p(to_ymd)) {
        (Some(a), Some(b)) => (b - a).num_days(),
        _ => i64::MAX,
    }
}

#[cfg(test)]
mod tests {

    /// A retry has to re-run the same thing, not something close to
    /// it — and say nothing where that is impossible.
    #[test]
    fn retry_command_re_runs_what_ran_or_offers_nothing() {
        let base = JobConfig::default();

        // A refresh: the query is the display string `refresh: <title>`,
        // so the command has to come from the config's slug instead.
        let mut refresh = base.clone();
        refresh.kms_target = Some("Age of Abundance (v2)".into());
        refresh.refresh_slug = Some("jevons-paradox".into());
        assert_eq!(
            retry_command_for(
                "refresh: \u{e1b}\u{e0f}\u{e34}\u{e17}\u{e23}\u{e23}\u{e28}\u{e19}\u{e4c}",
                &refresh
            )
            .as_deref(),
            Some("/research refresh \"Age of Abundance (v2)\" jevons-paradox")
        );
        // A KMS name with spaces has to come back quoted, or the retry
        // researches the first word and treats the rest as the query.
        let mut plain = base.clone();
        plain.kms_target = Some("Age of Abundance (v2)".into());
        assert_eq!(
            retry_command_for("what changed in 2026", &plain).as_deref(),
            Some("/research --kms \"Age of Abundance (v2)\" what changed in 2026")
        );

        assert_eq!(
            retry_command_for("loose query", &base).as_deref(),
            Some("/research loose query")
        );

        // Neither of these can be written correctly from what the job
        // kept, so neither gets a button.
        let mut ingest = base.clone();
        ingest.local_source = Some("some-alias".into());
        assert!(retry_command_for("q", &ingest).is_none());
        let mut selection = base.clone();
        selection.topic_slug = Some("herbert-simon".into());
        assert!(retry_command_for("q", &selection).is_none());
        assert!(retry_command_for("   ", &base).is_none());
    }
    /// dev-plan/64 D6. `auto` was the literal `th`: an English paper added to
    /// a KMS came back as Thai notes.
    #[test]
    fn research_writes_in_the_language_of_what_it_was_given() {
        assert_eq!(
            detect_language("ยุคที่ความฉลาดล้นเหลือ — เราจะรู้ได้อย่างไรว่าควรสร้างอะไร"),
            "th"
        );
        // A Thai document about software is a third English and still Thai.
        assert_eq!(
            detect_language(
                "Claude Code เปิดให้ subagent spawn subagent ของตัวเองใน v2.1.172 โดยมี cap ห้าชั้น"
            ),
            "th"
        );
        assert_eq!(
            detect_language("The Use of Knowledge in Society, Hayek 1945"),
            "en"
        );
        assert_eq!(detect_language("知識管理とは何か"), "query");
        assert_eq!(detect_language("2026 — 51%"), "query");

        // An explicit flag is never second-guessed; `auto` follows the
        // document before the query.
        assert_eq!(resolve_language("en", "ยุคที่ความฉลาดล้นเหลือ", None), "en");
        assert_eq!(
            resolve_language("auto", "hayek-1945", Some("ความรู้กระจายอยู่ในหัวของคนจำนวนมาก")),
            "th"
        );
        assert_eq!(
            resolve_language(
                "auto",
                "ครัวเรือนยากจนแฝง",
                Some("Hidden poverty is measured by expenditure.")
            ),
            "en"
        );
        assert_eq!(JobConfig::default().language, "auto");
    }

    use super::*;

    #[test]
    fn register_creates_pending_job_with_unique_id() {
        let mgr = ResearchManager::new();
        let cfg = JobConfig::default();
        let (a, _) = mgr.register("query a".into(), &cfg);
        let (b, _) = mgr.register("query b".into(), &cfg);
        assert_ne!(a, b);
        assert!(a.starts_with("research-"));
        assert_eq!(mgr.get(&a).unwrap().status, JobStatus::Pending);
    }

    #[test]
    fn update_phase_flips_to_running() {
        let mgr = ResearchManager::new();
        let (id, _) = mgr.register("q".into(), &JobConfig::default());
        mgr.update_phase(&id, "iteration 1");
        let v = mgr.get(&id).unwrap();
        assert_eq!(v.status, JobStatus::Running);
        assert_eq!(v.phase, "iteration 1");
    }

    #[test]
    fn finalize_is_idempotent_on_terminal_job() {
        let mgr = ResearchManager::new();
        let (id, _) = mgr.register("q".into(), &JobConfig::default());
        mgr.finalize(&id, JobStatus::Done, Some("note.md".into()), None);
        mgr.finalize(&id, JobStatus::Failed, None, Some("late err".into()));
        let v = mgr.get(&id).unwrap();
        assert_eq!(v.status, JobStatus::Done);
        assert_eq!(v.result_page.as_deref(), Some("note.md"));
        assert!(v.error.is_none());
    }

    #[test]
    fn cancel_signals_token_and_returns_true() {
        let mgr = ResearchManager::new();
        let (id, cancel) = mgr.register("q".into(), &JobConfig::default());
        assert!(!cancel.is_cancelled());
        assert!(mgr.cancel(&id));
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn cancel_returns_false_for_terminal_job() {
        let mgr = ResearchManager::new();
        let (id, _) = mgr.register("q".into(), &JobConfig::default());
        mgr.finalize(&id, JobStatus::Done, None, None);
        assert!(!mgr.cancel(&id));
    }

    #[test]
    fn cancel_returns_false_for_unknown_id() {
        let mgr = ResearchManager::new();
        assert!(!mgr.cancel("research-nope"));
    }

    #[test]
    fn list_sorted_newest_first() {
        let mgr = ResearchManager::new();
        let cfg = JobConfig::default();
        let (a, _) = mgr.register("first".into(), &cfg);
        std::thread::sleep(Duration::from_millis(10));
        let (b, _) = mgr.register("second".into(), &cfg);
        let v = mgr.list();
        assert_eq!(v[0].id, b);
        assert_eq!(v[1].id, a);
    }

    #[test]
    fn record_iteration_updates_counters() {
        let mgr = ResearchManager::new();
        let (id, _) = mgr.register("q".into(), &JobConfig::default());
        mgr.record_iteration(&id, 1, 8, Some(0.4));
        mgr.record_iteration(&id, 2, 17, Some(0.78));
        let v = mgr.get(&id).unwrap();
        assert_eq!(v.iterations_done, 2);
        assert_eq!(v.source_count, 17);
        assert_eq!(v.last_score, Some(0.78));
    }

    #[test]
    fn job_status_terminal_classification() {
        assert!(!JobStatus::Pending.is_terminal());
        assert!(!JobStatus::Running.is_terminal());
        assert!(JobStatus::Done.is_terminal());
        assert!(JobStatus::Cancelled.is_terminal());
        assert!(JobStatus::Failed.is_terminal());
    }

    #[test]
    fn default_config_matches_documented_knobs() {
        // Pin the defaults — these are user-facing and changing them
        // shifts the `/research` UX. Update this test deliberately if
        // we tune them.
        let c = JobConfig::default();
        assert_eq!(c.min_iter, 2);
        assert_eq!(c.max_iter, 4, "v2: seed + 3 gap rounds");
        assert!((c.score_threshold - 0.80).abs() < f32::EPSILON);
        assert_eq!(c.subtopics_per_iter, 4);
        assert_eq!(c.fetch_top_n, 3);
        assert_eq!(c.max_pages, 7);
        assert_eq!(c.max_notes, 30);
        assert!((c.novelty_threshold - 0.35).abs() < f32::EPSILON);
        assert_eq!(c.llm_timeout.as_secs(), 180);
        assert_eq!(c.time_budget.as_secs(), 25 * 60);
        assert!(!c.legacy && !c.append && !c.dry_run);
        // dev-plan/64 D6: not "th" — `auto`, resolved per run from the
        // document or the query.
        assert_eq!(c.language, "auto");
        let en = JobConfig::from_flags(StartFlags {
            language: Some(" EN ".into()),
            ..Default::default()
        });
        assert_eq!(en.language, "en");
        assert!(language_rule("th").contains("Thai") && language_rule("th").contains("English"));
        assert_eq!(language_rule("en"), "Write in English.");
        // legacy keeps its old ceiling when the flag is set without --max-iter
        let l = JobConfig::from_flags(StartFlags {
            legacy: true,
            ..Default::default()
        });
        assert_eq!(l.max_iter, 8);
    }

    /// Zettelkasten default: no `--kms` ⇒ the most recently attached KMS;
    /// `--kms new` forces a fresh, query-derived KMS; an explicit name wins.
    #[test]
    fn kms_target_defaults_to_last_attached_kms() {
        let attached = vec!["labour".to_string(), "z-image-model".to_string()];
        let c = JobConfig::from_flags(StartFlags {
            attached_kms: attached.clone(),
            ..Default::default()
        });
        assert_eq!(c.kms_target.as_deref(), Some("z-image-model"));
        let c = JobConfig::from_flags(StartFlags {
            attached_kms: attached.clone(),
            kms_target: Some("new".into()),
            ..Default::default()
        });
        assert_eq!(c.kms_target, None);
        let c = JobConfig::from_flags(StartFlags {
            attached_kms: attached,
            kms_target: Some("other".into()),
            ..Default::default()
        });
        assert_eq!(c.kms_target.as_deref(), Some("other"));
        let c = JobConfig::from_flags(StartFlags::default());
        assert_eq!(c.kms_target, None);
    }
}
