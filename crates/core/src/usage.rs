//! Usage tracking — accumulate token usage per provider+model per project.
//!
//! Stored in `.thclaws/state/usage/{provider}/{model}.json` with daily
//! breakdowns. Each file is a JSON object keyed by date (YYYY-MM-DD) with
//! token counts.
//!
//! Example: `.thclaws/state/usage/anthropic/claude-sonnet-4-5.json`
//! ```json
//! {
//!   "2026-04-13": { "input": 15230, "output": 3420, "cache_write": 500, "cache_read": 12000, "requests": 8 },
//!   "2026-04-12": { "input": 8100, "output": 1200, "cache_write": 0, "cache_read": 4000, "requests": 3 }
//! }
//! ```

use crate::providers::Usage;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DailyUsage {
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub requests: u64,
    /// Thinking tokens; a part of `output`, not added to it.
    #[serde(default)]
    pub reasoning: u64,
}

impl DailyUsage {
    fn add(&mut self, d: &DailyUsage) {
        self.input += d.input;
        self.output += d.output;
        self.cache_write += d.cache_write;
        self.cache_read += d.cache_read;
        self.requests += d.requests;
        self.reasoning += d.reasoning;
    }

    /// `Nin (… cached)/Nout (… thinking) (N req)`: `in` is the whole prompt,
    /// cache included, as the gateway and the provider bill count it
    /// (`input` on disk is the uncached part).
    pub fn line(&self) -> String {
        let cache = match (self.cache_read, self.cache_write) {
            (0, 0) => String::new(),
            (r, 0) => format!(" ({r} cached)"),
            (0, w) => format!(" ({w} cache-write)"),
            (r, w) => format!(" ({r} cached, {w} cache-write)"),
        };
        let thinking = match self.reasoning {
            0 => String::new(),
            t => format!(" ({t} thinking)"),
        };
        format!(
            "{}in{cache}/{}out{thinking} ({} req)",
            self.input + self.cache_read + self.cache_write,
            self.output,
            self.requests
        )
    }
}

/// Per-model usage file: date → DailyUsage.
pub type ModelUsage = BTreeMap<String, DailyUsage>;

pub struct UsageTracker {
    root: PathBuf,
}

impl UsageTracker {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Default: `.thclaws/state/usage/` in cwd.
    pub fn default_path() -> PathBuf {
        PathBuf::from(".thclaws/state/usage")
    }

    fn model_path(&self, provider: &str, model: &str) -> PathBuf {
        // Sanitize model name for filesystem (e.g. "claude-sonnet-4-5" is fine,
        // but "ollama/llama3.2" needs the slash replaced).
        let safe_model = model.replace('/', "_");
        self.root.join(provider).join(format!("{safe_model}.json"))
    }

    /// Record a usage event for a provider+model.
    pub fn record(&self, provider: &str, model: &str, usage: &Usage) {
        if usage.input_tokens == 0 && usage.output_tokens == 0 {
            return;
        }
        let path = self.model_path(provider, model);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let today = today_str();

        // M3: serialize the whole read→modify→write across threads AND
        // processes. The per-model file is a read-modify-write: two
        // concurrent writers would both read the pre-update contents and
        // last-writer-wins, silently dropping one subagent's tokens
        // (Task is `parallelizable` and several thClaws instances may
        // share one `.thclaws/usage/`). An advisory exclusive flock on a
        // dedicated sidecar `.lock` file closes the lost-update window;
        // writing via temp-file + atomic rename closes the torn-read
        // window for the lock-free readers (`all_models`/`total`/`today`).
        // The lock file inode is stable (never renamed), so all writers
        // contend on the same inode. Best-effort throughout: a failed
        // open/lock skips this record rather than panicking — usage
        // accounting must never take down a turn.
        use fs2::FileExt;
        let lock_path = path.with_extension("json.lock");
        let Ok(lock_file) = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
        else {
            return;
        };
        if lock_file.lock_exclusive().is_err() {
            return;
        }

        // Read existing data (under lock).
        let mut data: ModelUsage = if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            BTreeMap::new()
        };

        // Accumulate into today's entry.
        let entry = data.entry(today).or_default();
        entry.input += usage.input_tokens as u64;
        entry.output += usage.output_tokens as u64;
        entry.cache_write += usage.cache_creation_input_tokens.unwrap_or(0) as u64;
        entry.cache_read += usage.cache_read_input_tokens.unwrap_or(0) as u64;
        entry.requests += 1;
        entry.reasoning += usage.reasoning_output_tokens.unwrap_or(0) as u64;

        // Write back via temp-file + atomic rename so a concurrent reader
        // never observes a half-written file.
        if let Ok(json) = serde_json::to_string_pretty(&data) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, &json).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }

        // Best-effort unlock; Drop closes the fd which also releases the
        // flock per POSIX semantics, so a failed unlock can't deadlock
        // the next writer.
        let _ = lock_file.unlock();
    }

    /// Get total usage across all providers and models.
    pub fn total(&self) -> DailyUsage {
        let mut total = DailyUsage::default();
        for (_, _, data) in self.all_models() {
            for day in data.values() {
                total.add(day);
            }
        }
        total
    }

    /// Get today's usage across all providers and models.
    pub fn today(&self) -> DailyUsage {
        let today = today_str();
        let mut total = DailyUsage::default();
        for (_, _, data) in self.all_models() {
            if let Some(day) = data.get(&today) {
                total.add(day);
            }
        }
        total
    }

    /// List all (provider, model, data) tuples.
    pub fn all_models(&self) -> Vec<(String, String, ModelUsage)> {
        let mut out = Vec::new();
        let Ok(providers) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for prov_entry in providers.flatten() {
            if !prov_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let provider = prov_entry.file_name().to_string_lossy().into_owned();
            let Ok(models) = std::fs::read_dir(prov_entry.path()) else {
                continue;
            };
            for model_entry in models.flatten() {
                let path = model_entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let model = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .replace('_', "/");
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    if let Ok(data) = serde_json::from_str::<ModelUsage>(&contents) {
                        out.push((provider.clone(), model, data));
                    }
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        out
    }

    /// Format a summary for display.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        let today_key = today_str();

        let models = self.all_models();
        if models.is_empty() {
            return "No usage recorded.".into();
        }

        parts.push("## Usage by model".to_string());
        for (provider, model, data) in &models {
            let mut total = DailyUsage::default();
            for day in data.values() {
                total.add(day);
            }
            let today_str = data
                .get(&today_key)
                .map(|d| format!("today: {}", d.line()))
                .unwrap_or_else(|| "today: 0".into());
            parts.push(format!(
                "  {}/{}: {} — {}",
                provider,
                model,
                total.line(),
                today_str
            ));
        }

        let grand = self.total();
        let grand_today = self.today();
        parts.push(format!(
            "\n## Total: {} — today: {}",
            grand.line(),
            grand_today.line()
        ));

        parts.join("\n")
    }
}

/// LLM calls made outside the main agent loop — Task subagents,
/// compaction, workflow steps, GUI-shell `llm.complete`, research, KMS
/// auto-link. The gateway bills them to the session, but the turn's `Done`
/// usage never saw them, so the footer and `$ session` read less than the
/// web Usage page. Each one is noted here under its session
/// ([`crate::gateway_turn::current_session`]) and the session's next footer
/// takes them ([`take_side_usage`]).
#[derive(Debug, Clone, Default)]
pub struct SideUsage {
    pub calls: u32,
    pub usage: Usage,
    pub cost_usd: f64,
    /// Per call, for the per-model tracker.
    pub by_model: Vec<(String, Usage)>,
}

impl SideUsage {
    /// ` · +N side calls: …` for a `[tokens: …]` line.
    pub fn footer_note(&self) -> String {
        format!(
            " · +{} side call{}: {}",
            self.calls,
            if self.calls == 1 { "" } else { "s" },
            self.usage.tokens_note()
        )
    }

    /// Record every side call in the per-model tracker under `root`.
    pub fn record(&self, tracker: &UsageTracker) {
        for (model, u) in &self.by_model {
            let provider = crate::providers::ProviderKind::detect(model)
                .map(|k| k.name())
                .unwrap_or("unknown");
            tracker.record(provider, model, u);
        }
    }
}

static SIDE_USAGE: std::sync::Mutex<BTreeMap<String, SideUsage>> =
    std::sync::Mutex::new(BTreeMap::new());

/// Note one side call's usage against the current session.
pub fn note_side_usage(model: &str, u: &Usage) {
    if u.input_tokens == 0 && u.output_tokens == 0 && u.cache_read_input_tokens.unwrap_or(0) == 0 {
        return;
    }
    let cost = crate::model_catalogue::EffectiveCatalogue::load()
        .compute_cost_usd(model, &crate::model_catalogue::TokenUsage::from_usage(u))
        .unwrap_or(0.0);
    // In a multiuser pod a call outside any session scope cannot be
    // pinned to a member; booking it under "" would show it in whichever
    // member's footer came next.
    let key = match crate::gateway_turn::current_session() {
        Some(k) => k,
        None if crate::workdir::is_multiuser() => return,
        None => String::new(),
    };
    let mut g = SIDE_USAGE.lock().unwrap_or_else(|p| p.into_inner());
    let e = g.entry(key).or_default();
    e.calls += 1;
    e.usage.accumulate(u);
    e.cost_usd += cost;
    e.by_model.push((model.to_string(), u.clone()));
}

/// Everything noted for `session` (plus calls no session was known for)
/// since the last take.
pub fn take_side_usage(session: &str) -> Option<SideUsage> {
    let mut g = SIDE_USAGE.lock().unwrap_or_else(|p| p.into_inner());
    let mut out = SideUsage::default();
    let unscoped = if crate::workdir::is_multiuser() {
        None
    } else {
        Some("")
    };
    for key in std::iter::once(session).chain(unscoped) {
        if let Some(e) = g.remove(key) {
            out.calls += e.calls;
            out.usage.accumulate(&e.usage);
            out.cost_usd += e.cost_usd;
            out.by_model.extend(e.by_model);
        }
    }
    (out.calls > 0).then_some(out)
}

/// Today, where the user is.
///
/// This was the UTC date. In Bangkok that is yesterday until 07:00, so a
/// page edited at 00:11 on the 20th was logged, stamped `updated:` and
/// `verified:` on the 19th, and a research prompt written that morning told
/// the model it was the day before. Every date the KMS writes comes through
/// here, and the person reading those dates reads them on a local calendar.
pub(crate) fn today_str() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Best-effort per-workspace usage ledger.
///
/// Appends one JSON line per *finalized agent turn* — the main loop OR a
/// Task subagent — to `<cwd>/.thclaws/state/usage.jsonl`. A reader can sum the
/// lines to get the true token + USD cost of a whole task **including
/// subagent work**, which the per-model [`UsageTracker`] aggregates by
/// day (hard to isolate one run) and the session JSONL omits entirely.
///
/// `role` is `"main"` for the orchestrator or the subagent's name. The
/// cost is computed from the model catalogue (`null` for unknown / tier-
/// billed models). Never returns an error: usage telemetry must not be
/// able to break a turn.
pub fn append_usage_ledger(
    cwd: &std::path::Path,
    role: &str,
    provider: &str,
    model: &str,
    usage: &Usage,
) {
    if usage.input_tokens == 0 && usage.output_tokens == 0 {
        return;
    }
    let dir = cwd.join(".thclaws").join("state");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let cost = crate::model_catalogue::EffectiveCatalogue::load().compute_cost_usd(
        model,
        &crate::model_catalogue::TokenUsage::from_usage(usage),
    );
    let line = serde_json::json!({
        "type": "usage",
        "ts": epoch_secs(),
        "role": role,
        "provider": provider,
        "model": model,
        "input": usage.input_tokens,
        "output": usage.output_tokens,
        "cache_read": usage.cache_read_input_tokens.unwrap_or(0),
        "cache_write": usage.cache_creation_input_tokens.unwrap_or(0),
        "reasoning": usage.reasoning_output_tokens.unwrap_or(0),
        "cost_usd": cost,
    })
    .to_string();
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("usage.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn record_and_read() {
        let dir = tempdir().unwrap();
        let tracker = UsageTracker::new(dir.path().to_path_buf());

        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 200,
            cache_creation_input_tokens: Some(50),
            cache_read_input_tokens: Some(300),
            reasoning_output_tokens: None,
        };
        tracker.record("anthropic", "claude-sonnet-4-5", &usage);
        tracker.record("anthropic", "claude-sonnet-4-5", &usage);

        let today = tracker.today();
        assert_eq!(today.input, 2000);
        assert_eq!(today.output, 400);
        assert_eq!(today.cache_write, 100);
        assert_eq!(today.cache_read, 600);
        assert_eq!(today.requests, 2);
    }

    #[test]
    fn ledger_appends_parseable_lines_with_roles() {
        let dir = tempdir().unwrap();
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 200,
            cache_creation_input_tokens: Some(50),
            cache_read_input_tokens: Some(300),
            reasoning_output_tokens: None,
        };
        // One main turn + two subagent turns into the same workspace.
        append_usage_ledger(dir.path(), "main", "anthropic", "claude-sonnet-4-6", &usage);
        append_usage_ledger(
            dir.path(),
            "planner",
            "anthropic",
            "claude-sonnet-4-6",
            &usage,
        );
        append_usage_ledger(
            dir.path(),
            "verifier",
            "anthropic",
            "claude-sonnet-4-6",
            &usage,
        );

        let body = std::fs::read_to_string(dir.path().join(".thclaws/state/usage.jsonl")).unwrap();
        let lines: Vec<_> = body.lines().collect();
        assert_eq!(lines.len(), 3, "one ledger line per finalized turn");

        let mut total_in = 0u64;
        let mut roles = Vec::new();
        for l in &lines {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert_eq!(v["type"], "usage");
            total_in += v["input"].as_u64().unwrap();
            roles.push(v["role"].as_str().unwrap().to_string());
        }
        // Summing the ledger captures parent + subagent token usage.
        assert_eq!(total_in, 3000);
        assert!(roles.contains(&"main".to_string()));
        assert!(roles.contains(&"planner".to_string()));
        assert!(roles.contains(&"verifier".to_string()));
    }

    #[test]
    fn ledger_skips_zero_usage() {
        let dir = tempdir().unwrap();
        append_usage_ledger(dir.path(), "main", "anthropic", "m", &Usage::default());
        assert!(!dir.path().join(".thclaws/state/usage.jsonl").exists());
    }

    #[test]
    fn multiple_models() {
        let dir = tempdir().unwrap();
        let tracker = UsageTracker::new(dir.path().to_path_buf());

        tracker.record(
            "anthropic",
            "claude-sonnet-4-5",
            &Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                reasoning_output_tokens: None,
            },
        );
        tracker.record(
            "openai",
            "gpt-4o",
            &Usage {
                input_tokens: 200,
                output_tokens: 100,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                reasoning_output_tokens: None,
            },
        );

        let models = tracker.all_models();
        assert_eq!(models.len(), 2);

        let total = tracker.total();
        assert_eq!(total.input, 300);
        assert_eq!(total.output, 150);
        assert_eq!(total.requests, 2);
    }

    #[test]
    fn zero_usage_not_recorded() {
        let dir = tempdir().unwrap();
        let tracker = UsageTracker::new(dir.path().to_path_buf());

        tracker.record("anthropic", "claude-sonnet-4-5", &Usage::default());
        assert!(tracker.all_models().is_empty());
    }

    #[test]
    fn summary_format() {
        let dir = tempdir().unwrap();
        let tracker = UsageTracker::new(dir.path().to_path_buf());

        tracker.record(
            "anthropic",
            "claude-sonnet-4-5",
            &Usage {
                input_tokens: 1000,
                output_tokens: 200,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                reasoning_output_tokens: None,
            },
        );

        let summary = tracker.summary();
        assert!(summary.contains("anthropic/claude-sonnet-4-5"));
        assert!(summary.contains("1000in/200out"));
    }

    #[test]
    fn today_str_valid_format() {
        let d = today_str();
        assert_eq!(d.len(), 10); // YYYY-MM-DD
        assert_eq!(&d[4..5], "-");
        assert_eq!(&d[7..8], "-");
    }

    #[test]
    fn the_line_counts_cache_in_input_and_thinking_in_output() {
        let d = DailyUsage {
            input: 1000,
            output: 200,
            cache_write: 50,
            cache_read: 300,
            requests: 2,
            reasoning: 120,
        };
        assert_eq!(
            d.line(),
            "1350in (300 cached, 50 cache-write)/200out (120 thinking) (2 req)"
        );
    }

    #[tokio::test]
    async fn side_calls_are_taken_once_by_their_session() {
        let u = Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_input_tokens: Some(900),
            ..Usage::default()
        };
        // Scoped, not process-wide: other tests set the process-wide one.
        crate::gateway_turn::scope_session("sess-side-a".into(), async {
            note_side_usage("some-unpriced-model", &u);
            note_side_usage("some-unpriced-model", &u);
        })
        .await;
        assert!(take_side_usage("sess-side-b").is_none());
        let got = take_side_usage("sess-side-a").expect("noted");
        assert_eq!(got.calls, 2);
        assert_eq!(got.usage.prompt_tokens(), 2000);
        assert_eq!(
            got.footer_note(),
            " · +2 side calls: 2000in (1800 cached)/40out"
        );
        assert!(take_side_usage("sess-side-a").is_none());
    }
}
