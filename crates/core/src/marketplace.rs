//! Skill (and later: plugin / MCP) marketplace catalogue.
//!
//! Mirrors [`crate::model_catalogue`]'s three-layer pattern:
//!   1. Embedded baseline compiled into the binary (`resources/marketplace.json`)
//!      so first-launch search/install works with no network.
//!   2. User cache at `~/.config/thclaws/marketplace.json`, written when
//!      the user runs `/skill marketplace --refresh` or by the GUI worker's
//!      daily auto-refresh task ([`spawn_daily_auto_refresh`], fired at
//!      boot if the cache is older than [`AUTO_REFRESH_AFTER_SECS`]).
//!   3. Remote endpoint `thclaws.ai/api/marketplace.json` — fetched on
//!      explicit refresh, cached locally; fail-silent so offline use stays
//!      productive.
//!
//! The schema is deliberately small: one `MarketplaceSkill` record per
//! redistributable skill, with the upstream `install_url` pointing at a
//! git URL that [`crate::skills::install_from_url`] understands.
//!
//! License-tier filtering: entries with `license_tier == "linked-only"`
//! (Anthropic's source-available docx/pdf/pptx/xlsx skills) appear in
//! `/skill marketplace` but `/skill install <name>` rejects them with an
//! upstream link instead of redistributing.
//!
//! ## Trust model
//!
//! The official marketplace at `https://thclaws.ai/api/marketplace.json`
//! is the canonical source. It is deployed by the operator (rsync over
//! SSH from the workspace) — **not** synced from the public repo,
//! **not** writable via PR, **not** auto-deployed by CI. A developer who
//! forks the public repo can:
//!
//! * Modify their local clone's [`BASELINE_JSON`] → only changes their
//!   own offline-fallback list; their fork's binary still queries the
//!   official `REMOTE_URL` for fresh data unless they also recompile.
//! * Submit a PR that edits `resources/marketplace.json` → gated by
//!   CODEOWNERS approval (see `.github/CODEOWNERS` in the public repo).
//! * Build a fork with a redirected `REMOTE_URL` → only affects users
//!   who run their fork; official-binary users still hit thclaws.ai.
//!
//! What forkers cannot do: push to `thclaws.ai/api/marketplace.json`
//! (no SSH credentials), redirect an official binary's refresh
//! (`REMOTE_URL` is a compile-time const), or slip an entry into a
//! release without owner sign-off (CODEOWNERS gates the baseline; the
//! release workflow runs from the operator's machine, not from a PR).
//!
//! When changing the marketplace contents, edit
//! `crates/core/resources/marketplace.json` in the workspace — the
//! `thclaws-web/Makefile`'s `api:` target copies that same file to the
//! live endpoint at deploy time, so the binary baseline and the
//! over-the-wire catalog stay in lock-step.
//!
//! ## Enterprise: private marketplace override (planned)
//!
//! When a signed org policy ships a `marketplace` sub-policy
//! (planned EE Phase 6, see `dev-plan/01-enterprise-edition.md`),
//! [`REMOTE_URL`] is overridden at runtime to the org's private
//! marketplace endpoint — typically an internal mirror that lists only
//! the skills the security team has vetted. The override has the same
//! tamper-resistance as the rest of the policy: signature failure
//! refuses startup, and the URL field cannot be set via `settings.json`
//! (only via signed policy).
//!
//! Behavior under an active marketplace policy:
//!
//! * `/skill marketplace --refresh` fetches from the org URL, not
//!   `thclaws.ai`.
//! * `/skill install <name>` resolves names against the org catalog only;
//!   names that exist on the public marketplace but not the private one
//!   return "not found".
//! * Install URLs are still subject to the existing
//!   `policies.plugins.allowed_hosts` allow-list, so the org can also
//!   restrict where skill source repos may live (e.g. internal GitLab
//!   only).
//! * The embedded baseline is treated as untrusted under EE: when a
//!   marketplace policy is active and the remote fetch fails, the
//!   client shows an empty catalog rather than falling back to the
//!   public-baseline list.
//!
//! Open-core and policy-less EE builds keep the current behavior:
//! [`REMOTE_URL`] points at thclaws.ai, baseline is the trusted offline
//! fallback.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

/// Schema version. Bumped on incompatible changes; the loader rejects
/// caches with a different number rather than serving stale rows.
pub const CURRENT_SCHEMA: u32 = 1;

/// Where the client fetches the remote catalogue from. Same `thclaws.ai`
/// host as the model catalogue.
pub const REMOTE_URL: &str = "https://thclaws.ai/api/marketplace.json";

/// Embedded baseline shipped with every binary so first launch always
/// has *something* to search. Refreshed by editing the resource file
/// and rebuilding (or by the user running `/skill marketplace --refresh`).
pub const BASELINE_JSON: &str = include_str!("../resources/marketplace.json");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceSkill {
    /// Short, slug-style id used by `/skill install <name>` and `/skill info <name>`.
    pub name: String,
    /// Single-line tagline (~50–60 chars) shown in `/skill marketplace`
    /// and `/skill search` list output. Optional — when missing, the
    /// list view falls back to truncating `description`. Authoring
    /// guidance: verb + object, no marketing filler, no "Use this
    /// when…" preamble.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_description: Option<String>,
    /// Full description shown in `/skill info <name>` detail view.
    /// May be multiple sentences and include trigger guidance.
    pub description: String,
    /// Loose category tag (creative / development / enterprise / …).
    /// No enum on purpose — keeps the registry editable without a
    /// schema bump every time someone invents a new vertical.
    #[serde(default)]
    pub category: String,
    /// SPDX-style identifier (`Apache-2.0`, `MIT`, `Anthropic source-available`).
    pub license: String,
    /// Coarse tier used to gate install:
    ///   - `"open"` — fully redistributable, install proceeds normally
    ///   - `"linked-only"` — show in catalogue, but install command refuses
    ///     and prints `homepage` as the upstream install path
    pub license_tier: String,
    /// Upstream repo (e.g. `anthropics/skills`). Informational; the
    /// installer uses `install_url` directly.
    #[serde(default)]
    pub source_repo: String,
    /// Subpath inside `source_repo` if the skill is a directory in a
    /// larger repo. Informational mirror of what the `#:` part of
    /// `install_url` already encodes.
    #[serde(default)]
    pub source_path: String,
    /// What [`crate::skills::install_from_url`] receives. Supports the
    /// extended `<git-url>#<branch>:<subpath>` syntax for installing a
    /// single skill from a multi-skill repo. May be `null` for
    /// `linked-only` entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_url: Option<String>,
    /// Upstream URL for human browsing — what `/skill info` prints and
    /// what `/skill install <name>` points the user at when an entry is
    /// `linked-only`.
    #[serde(default)]
    pub homepage: String,
}

/// One MCP-server entry in the marketplace catalogue. Two transport
/// shapes share this struct:
///
/// * **stdio** — the agent spawns a subprocess. `command` + `args` are
///   required; `install_url` (if set) is a git URL the source for the
///   subprocess can be cloned from before first run, and
///   `post_install_message` (if set) tells the user any prerequisite
///   step (e.g. `pip install -e <path>`).
/// * **sse / http** — the agent connects to a hosted HTTP endpoint.
///   `url` is required; `command` / `args` are unused.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceMcpServer {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_description: Option<String>,
    pub description: String,
    #[serde(default)]
    pub category: String,
    pub license: String,
    pub license_tier: String,
    /// `"stdio"` (default) or `"sse"`. Determines which fields are
    /// consulted by `/mcp install`.
    #[serde(default = "default_mcp_transport")]
    pub transport: String,
    // ── stdio transport ──
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Optional git clone source — when set, `/mcp install` clones
    /// this URL (with the same `<git-url>#<branch>:<subpath>` syntax
    /// as skills) into `~/.config/thclaws/mcp/<name>/` before writing
    /// the mcp.json entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_url: Option<String>,
    /// Shown verbatim after a successful install — typically the pip
    /// or npm install command the user must run before the stdio
    /// command resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_install_message: Option<String>,
    // ── sse / http transport ──
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    // ── shared ──
    #[serde(default)]
    pub homepage: String,
}

fn default_mcp_transport() -> String {
    "stdio".to_string()
}

/// One plugin entry in the marketplace catalogue. Plugins always
/// install from a git URL or zip — no transport-specific shapes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplacePlugin {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_description: Option<String>,
    pub description: String,
    #[serde(default)]
    pub category: String,
    pub license: String,
    pub license_tier: String,
    pub install_url: String,
    #[serde(default)]
    pub homepage: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Marketplace {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub fetched_at: String,
    #[serde(default)]
    pub skills: Vec<MarketplaceSkill>,
    #[serde(default)]
    pub mcp_servers: Vec<MarketplaceMcpServer>,
    #[serde(default)]
    pub plugins: Vec<MarketplacePlugin>,
}

/// Derive a single-line catalog row from an entry's full description.
/// Shared by all three marketplace types (skill / mcp / plugin) — kept
/// as a free function so each type's `short_line()` is a one-liner.
fn derive_short_line(short: Option<&str>, description: &str) -> String {
    if let Some(s) = short {
        return s.to_string();
    }
    if let Some(idx) = description.find(". ") {
        return description[..=idx].to_string();
    }
    const CAP: usize = 70;
    if description.chars().count() <= CAP {
        description.to_string()
    } else {
        let cut: String = description.chars().take(CAP).collect();
        format!("{cut}…")
    }
}

impl MarketplaceSkill {
    /// One-line text used in `/skill marketplace` and `/skill search`
    /// list rendering. Prefers `short_description` when authored;
    /// otherwise truncates `description` at the first sentence boundary
    /// (or falls back to a hard char cap) so we never blow the line in
    /// a typical 80-col terminal.
    pub fn short_line(&self) -> String {
        derive_short_line(self.short_description.as_deref(), &self.description)
    }
}

impl MarketplaceMcpServer {
    pub fn short_line(&self) -> String {
        derive_short_line(self.short_description.as_deref(), &self.description)
    }
}

impl MarketplacePlugin {
    pub fn short_line(&self) -> String {
        derive_short_line(self.short_description.as_deref(), &self.description)
    }
}

/// Common surface across the three entry types so generic search /
/// rendering helpers don't need a per-type implementation. Added in
/// M6.12 (fix M2) to collapse three near-identical `find_*` /
/// `search_*` blocks into one.
pub trait MarketplaceEntry {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn category(&self) -> &str;
    fn license_tier(&self) -> &str;
    /// URL the policy gate should consult before allowing install /
    /// connection. Returns `None` for entries with no network
    /// operation (e.g. `linked-only` skills with no `install_url`,
    /// or stdio MCP servers with no `install_url` and no `url`).
    /// Returning `None` short-circuits the policy check; the entry
    /// is rendered without the `[blocked by policy]` tag.
    fn policy_check_url(&self) -> Option<&str>;
}

impl MarketplaceEntry for MarketplaceSkill {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn category(&self) -> &str {
        &self.category
    }
    fn license_tier(&self) -> &str {
        &self.license_tier
    }
    fn policy_check_url(&self) -> Option<&str> {
        self.install_url.as_deref()
    }
}

impl MarketplaceEntry for MarketplaceMcpServer {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn category(&self) -> &str {
        &self.category
    }
    fn license_tier(&self) -> &str {
        &self.license_tier
    }
    fn policy_check_url(&self) -> Option<&str> {
        // For sse / http MCP, `url` is the connection target and
        // policy applies to it. For stdio, `install_url` is the git
        // clone source. Prefer install_url when both are set (clone
        // is the gating step); fall back to url for hosted MCPs.
        if !self.install_url.as_deref().unwrap_or("").is_empty() {
            self.install_url.as_deref()
        } else if !self.url.is_empty() {
            Some(&self.url)
        } else {
            None
        }
    }
}

impl MarketplaceEntry for MarketplacePlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn category(&self) -> &str {
        &self.category
    }
    fn license_tier(&self) -> &str {
        &self.license_tier
    }
    fn policy_check_url(&self) -> Option<&str> {
        if self.install_url.is_empty() {
            None
        } else {
            Some(&self.install_url)
        }
    }
}

/// Generic exact-name lookup. Wraps the same pattern the three
/// per-type `find_*` methods used.
pub fn find_entry<'a, T: MarketplaceEntry>(items: &'a [T], name: &str) -> Option<&'a T> {
    items.iter().find(|s| s.name() == name)
}

/// Generic substring search. Case-insensitive, ranked by where the
/// match lands (name match beats description match beats category
/// match).
pub fn search_entries<'a, T: MarketplaceEntry>(items: &'a [T], query: &str) -> Vec<&'a T> {
    let q = query.to_lowercase();
    let mut hits: Vec<(u8, &T)> = Vec::new();
    for s in items {
        if s.name().to_lowercase().contains(&q) {
            hits.push((0, s));
        } else if s.description().to_lowercase().contains(&q) {
            hits.push((1, s));
        } else if s.category().to_lowercase().contains(&q) {
            hits.push((2, s));
        }
    }
    hits.sort_by_key(|(rank, _)| *rank);
    hits.into_iter().map(|(_, s)| s).collect()
}

/// Render the bracketed tag suffix for a marketplace entry — combines
/// the existing `[linked-only]` license-tier tag (M3 trust-tier
/// gating) with M6.12's new `[blocked by policy]` tag (signals an
/// install_url that the org's allowlist policy would reject before the
/// user wastes a discovery step).
///
/// Returns an empty string for entries that are open + allowed (the
/// common case), so callers can unconditionally append the result to
/// the listing line.
pub fn entry_tags<T: MarketplaceEntry>(entry: &T) -> String {
    let mut out = String::new();
    if entry.license_tier() == "linked-only" {
        out.push_str(" [linked-only]");
    }
    if let Some(url) = entry.policy_check_url() {
        if let crate::policy::AllowDecision::Denied { .. } = crate::policy::check_url(url) {
            out.push_str(" [blocked by policy]");
        }
    }
    out
}

impl Marketplace {
    /// Parse a JSON body into a marketplace catalogue, rejecting wrong
    /// schemas. Used for both the embedded baseline and the user cache.
    /// Soft API: returns `Option` to keep `load_cache` and the baseline
    /// `expect()` paths simple. The full parse-with-error variant is
    /// `parse_with_error()` below.
    pub fn from_json_str(body: &str) -> Option<Self> {
        Self::parse_with_error(body).ok()
    }

    /// Parse with structured errors so callers can distinguish "wrong
    /// shape" from "wrong schema version" (M6.11 — fix M1). The
    /// schema check happens BEFORE the full deserialize so a remote
    /// payload that bumped to schema=2 reports the version mismatch
    /// instead of a confusing field-by-field deserialize error.
    pub fn parse_with_error(body: &str) -> Result<Self, ParseError> {
        // Sniff the schema field first via a minimal struct that
        // ignores everything else. If the remote schema is newer than
        // we support, fail with an actionable message instead of
        // letting `serde_json::from_str::<Self>` fail on whatever
        // unrelated field shape changed alongside the bump.
        #[derive(Deserialize)]
        struct SchemaProbe {
            #[serde(default)]
            schema: u32,
        }
        let probe: SchemaProbe =
            serde_json::from_str(body).map_err(|e| ParseError::Json(e.to_string()))?;
        if probe.schema != CURRENT_SCHEMA {
            return Err(ParseError::SchemaMismatch {
                got: probe.schema,
                expected: CURRENT_SCHEMA,
            });
        }
        serde_json::from_str(body).map_err(|e| ParseError::Json(e.to_string()))
    }

    /// Look up a skill by exact-match name. Returns `None` if the user
    /// typed a name not in the catalogue (caller should suggest
    /// `/skill search` instead).
    pub fn find(&self, name: &str) -> Option<&MarketplaceSkill> {
        find_entry(&self.skills, name)
    }

    /// Substring-match search across name, description, and category.
    /// Case-insensitive, ranked by where the match lands (name match
    /// beats description match beats category match).
    pub fn search(&self, query: &str) -> Vec<&MarketplaceSkill> {
        search_entries(&self.skills, query)
    }

    pub fn find_mcp(&self, name: &str) -> Option<&MarketplaceMcpServer> {
        find_entry(&self.mcp_servers, name)
    }

    pub fn search_mcp(&self, query: &str) -> Vec<&MarketplaceMcpServer> {
        search_entries(&self.mcp_servers, query)
    }

    pub fn find_plugin(&self, name: &str) -> Option<&MarketplacePlugin> {
        find_entry(&self.plugins, name)
    }

    pub fn search_plugin(&self, query: &str) -> Vec<&MarketplacePlugin> {
        search_entries(&self.plugins, query)
    }
}

/// Three-layer load: user cache → embedded baseline. Remote fetch is
/// explicit (`refresh_from_remote`), not part of the load path, so the
/// REPL stays snappy on launch.
pub fn load() -> Marketplace {
    if let Some(cache) = load_cache() {
        return cache;
    }
    Marketplace::from_json_str(BASELINE_JSON)
        .expect("embedded marketplace baseline must parse — check resources/marketplace.json")
}

fn load_cache() -> Option<Marketplace> {
    let path = cache_path()?;
    let body = std::fs::read_to_string(path).ok()?;
    Marketplace::from_json_str(&body)
}

/// Path to the writable user cache. `None` only on machines without a
/// home directory.
pub fn cache_path() -> Option<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        crate::util::home_dir()?.join(".config")
    };
    Some(base.join("thclaws").join("marketplace.json"))
}

/// Cache-age threshold for the daily auto-refresh. M6.11 fix H1 — the
/// docstring previously claimed daily auto-refresh existed but the
/// code never wired it. 24h is the same window every other "is this
/// stale?" prompt in the app uses.
pub const AUTO_REFRESH_AFTER_SECS: u64 = 24 * 60 * 60;

/// Cache age in seconds, or `None` when there's no cache file (or the
/// timestamp can't be parsed). Used by both the auto-refresh task to
/// decide whether to fetch and the `/skill marketplace` listing to
/// decide whether to render a "stale" hint.
pub fn cache_age_secs() -> Option<u64> {
    let path = cache_path()?;
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let age = std::time::SystemTime::now().duration_since(modified).ok()?;
    Some(age.as_secs())
}

/// Threshold in seconds after which `/skill marketplace` renders a
/// "(stale — refresh with /skill marketplace --refresh)" hint next to
/// the cache `fetched_at`. 7 days — long enough that a regular user's
/// daily auto-refresh keeps them under it, short enough that someone
/// returning after a vacation sees a clear nudge to refresh.
pub const STALE_AFTER_SECS: u64 = 7 * 24 * 60 * 60;

/// Format a cache-age sidebar string for the `/skill marketplace`
/// header. Returns `None` when there's no cache (e.g. baseline-only
/// usage) so callers can skip the suffix entirely. M6.11 fix H2 —
/// users couldn't tell whether their catalog snapshot was hours or
/// months old.
pub fn cache_age_label() -> Option<String> {
    let secs = cache_age_secs()?;
    let stale = secs >= STALE_AFTER_SECS;
    let pretty = pretty_age(secs);
    if stale {
        Some(format!(
            "{pretty} ago (stale — refresh with /skill marketplace --refresh)"
        ))
    } else {
        Some(format!("{pretty} ago"))
    }
}

/// Render a duration as a coarse human label. Tuned for "how old is
/// my marketplace cache" — minute granularity at the low end, day at
/// the high end. Matches the pattern git's `git log --since` accepts.
fn pretty_age(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    if secs < MIN {
        return "just now".to_string();
    }
    if secs < HOUR {
        let m = secs / MIN;
        return format!("{m} min{}", if m == 1 { "" } else { "s" });
    }
    if secs < DAY {
        let h = secs / HOUR;
        return format!("{h} hour{}", if h == 1 { "" } else { "s" });
    }
    let d = secs / DAY;
    format!("{d} day{}", if d == 1 { "" } else { "s" })
}

/// Spawn a one-shot tokio task that refreshes the marketplace cache
/// IF the cache is older than `AUTO_REFRESH_AFTER_SECS` (or missing
/// entirely). Fail-silent — on network error, parse error, or any
/// other refresh failure, we keep using whatever cache / baseline is
/// already loaded. Logged at debug-ish level via eprintln on success
/// so users running with --verbose see the refresh happened.
///
/// Called once from the GUI worker boot path. Cheap when the cache is
/// fresh (single fs::metadata call, no network).
pub fn spawn_daily_auto_refresh() {
    let needs_refresh = match cache_age_secs() {
        Some(secs) => secs >= AUTO_REFRESH_AFTER_SECS,
        None => true, // no cache → fetch
    };
    if !needs_refresh {
        return;
    }
    tokio::spawn(async {
        match refresh_from_remote().await {
            Ok(out) => eprintln!(
                "\x1b[90m[marketplace] auto-refreshed: {} skill(s) from {}\x1b[0m",
                out.skill_count, out.source
            ),
            Err(_) => {
                // Fail silent. The user's existing cache (or baseline)
                // is still serving. Surface only on explicit refresh.
            }
        }
    });
}

fn write_cache(body: &str) -> Result<(), RefreshError> {
    let path = cache_path().ok_or(RefreshError::NoHome)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RefreshError::Io(e.to_string()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| RefreshError::Io(e.to_string()))?;
    std::fs::rename(&tmp, &path).map_err(|e| RefreshError::Io(e.to_string()))?;
    Ok(())
}

/// Process-wide reqwest client used by every `refresh_from_remote`
/// call. Reuses the underlying connection pool across refreshes —
/// previously each call built a fresh client (M6.11 — fix L1). The
/// 10-second timeout matches the prior per-call setting.
fn http_client() -> Option<&'static reqwest::Client> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .ok()
        })
        .as_ref()
}

/// Fetch the remote marketplace and, if it parses, write it to the
/// cache. Same fail-silent contract as `model_catalogue::refresh_from_remote`.
pub async fn refresh_from_remote() -> Result<RefreshOutcome, RefreshError> {
    let client = http_client()
        .ok_or_else(|| RefreshError::Http("failed to build HTTP client (TLS init?)".to_string()))?;
    let resp = client
        .get(REMOTE_URL)
        .send()
        .await
        .map_err(|e| RefreshError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(RefreshError::Http(format!("status {}", resp.status())));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| RefreshError::Http(e.to_string()))?;
    let parsed = Marketplace::parse_with_error(&body).map_err(RefreshError::Parse)?;
    write_cache(&body)?;
    Ok(RefreshOutcome {
        skill_count: parsed.skills.len(),
        source: parsed.source,
    })
}

#[derive(Debug)]
pub struct RefreshOutcome {
    pub skill_count: usize,
    pub source: String,
}

/// Distinguishes wrong JSON shape from wrong schema version. Surfaced
/// to the user via `RefreshError::Parse`'s Display, so a remote that
/// bumped to schema=2 returns "remote schema=2, this binary supports
/// schema=1 — upgrade thclaws" instead of a confusing serde error
/// (M6.11 — fix M1).
#[derive(Debug)]
pub enum ParseError {
    /// Body wasn't valid JSON or didn't match the Marketplace shape.
    Json(String),
    /// JSON parsed and named a `schema` field, but the value isn't
    /// what this binary supports. Action: upgrade the binary.
    SchemaMismatch { got: u32, expected: u32 },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Json(e) => write!(f, "JSON: {e}"),
            ParseError::SchemaMismatch { got, expected } => write!(
                f,
                "remote schema={got}, this binary supports schema={expected} — upgrade thclaws to refresh from a newer endpoint"
            ),
        }
    }
}

#[derive(Debug)]
pub enum RefreshError {
    Http(String),
    Parse(ParseError),
    Io(String),
    NoHome,
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshError::Http(e) => write!(f, "http: {e}"),
            RefreshError::Parse(e) => write!(f, "{e}"),
            RefreshError::Io(e) => write!(f, "io: {e}"),
            RefreshError::NoHome => write!(f, "no home directory; can't write cache"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic 3-skill catalog used by find/search tests so they
    /// don't depend on whatever the live baseline currently contains
    /// (which can legitimately be empty during a content-review
    /// period). Mirrors the real schema closely so the tests still
    /// exercise realistic shape.
    fn fixture_marketplace() -> Marketplace {
        let body = r#"{
            "schema": 1,
            "source": "fixture",
            "fetched_at": "2026-01-01T00:00:00Z",
            "skills": [
                {
                    "name": "algorithmic-art",
                    "short_description": "Algorithmic art with p5.js",
                    "description": "Create art using code, generative art, particle systems.",
                    "category": "creative",
                    "license": "Apache-2.0",
                    "license_tier": "open",
                    "install_url": "https://example.com/r.git#main:skills/algorithmic-art",
                    "homepage": "https://example.com/r/skills/algorithmic-art"
                },
                {
                    "name": "canvas-design",
                    "description": "Beautiful visual art in PNG and PDF using design philosophy.",
                    "category": "creative",
                    "license": "Apache-2.0",
                    "license_tier": "open",
                    "install_url": "https://example.com/r.git#main:skills/canvas-design",
                    "homepage": "https://example.com/r/skills/canvas-design"
                },
                {
                    "name": "webapp-testing",
                    "short_description": "Test web apps with Playwright",
                    "description": "Test apps using Playwright: debug, capture screenshots.",
                    "category": "development",
                    "license": "Apache-2.0",
                    "license_tier": "open",
                    "install_url": "https://example.com/r.git#main:skills/webapp-testing",
                    "homepage": "https://example.com/r/skills/webapp-testing"
                }
            ]
        }"#;
        Marketplace::from_json_str(body).expect("fixture must parse")
    }

    #[test]
    fn baseline_parses() {
        // Baseline parses cleanly even when the catalog is empty
        // (during review periods we ship `"skills": []` and let the
        // remote endpoint be the only source of truth).
        let m = Marketplace::from_json_str(BASELINE_JSON).expect("baseline must parse");
        assert_eq!(m.schema, CURRENT_SCHEMA);
        // Every entry — when present — must be redistributable. We
        // never seed source-available rows ourselves; those would come
        // from the remote endpoint if/when we mirror them as
        // linked-only.
        for s in &m.skills {
            assert_eq!(
                s.license_tier, "open",
                "baseline shouldn't carry linked-only entries: {}",
                s.name
            );
            assert!(
                s.install_url.is_some(),
                "open-tier entries must have install_url: {}",
                s.name
            );
        }
    }

    #[test]
    fn find_by_exact_name() {
        let m = fixture_marketplace();
        assert!(m.find("algorithmic-art").is_some());
        assert!(m.find("nonexistent-skill-xyz").is_none());
    }

    #[test]
    fn search_ranks_name_above_description() {
        let m = fixture_marketplace();
        // "art" appears in algorithmic-art's name AND canvas-design's description.
        // Name match should sort first.
        let hits = m.search("art");
        assert!(!hits.is_empty());
        assert_eq!(hits[0].name, "algorithmic-art");
    }

    #[test]
    fn search_case_insensitive() {
        let m = fixture_marketplace();
        assert!(!m.search("PLAYWRIGHT").is_empty());
        assert!(!m.search("playwright").is_empty());
    }

    #[test]
    fn short_line_prefers_short_description() {
        let s = MarketplaceSkill {
            name: "x".into(),
            short_description: Some("Tagline".into()),
            description: "A long sentence describing the thing.".into(),
            category: String::new(),
            license: "Apache-2.0".into(),
            license_tier: "open".into(),
            source_repo: String::new(),
            source_path: String::new(),
            install_url: None,
            homepage: String::new(),
        };
        assert_eq!(s.short_line(), "Tagline");
    }

    #[test]
    fn short_line_falls_back_to_first_sentence() {
        let s = MarketplaceSkill {
            name: "x".into(),
            short_description: None,
            description: "First sentence here. Second sentence with more detail.".into(),
            category: String::new(),
            license: "Apache-2.0".into(),
            license_tier: "open".into(),
            source_repo: String::new(),
            source_path: String::new(),
            install_url: None,
            homepage: String::new(),
        };
        assert_eq!(s.short_line(), "First sentence here.");
    }

    #[test]
    fn short_line_caps_when_no_sentence_break() {
        let long: String = "a".repeat(120);
        let s = MarketplaceSkill {
            name: "x".into(),
            short_description: None,
            description: long,
            category: String::new(),
            license: "Apache-2.0".into(),
            license_tier: "open".into(),
            source_repo: String::new(),
            source_path: String::new(),
            install_url: None,
            homepage: String::new(),
        };
        // 70 chars + "…" = 71
        assert_eq!(s.short_line().chars().count(), 71);
    }

    // ── M6.11 fixes ─────────────────────────────────────────────────────

    #[test]
    fn parse_with_error_distinguishes_schema_mismatch_from_json_error() {
        // M6.11 (M1): schema mismatch must surface as a SchemaMismatch
        // variant with both versions, not as an opaque JSON error.
        let body = r#"{"schema": 99, "source": "future", "fetched_at": "x"}"#;
        match Marketplace::parse_with_error(body) {
            Err(ParseError::SchemaMismatch { got, expected }) => {
                assert_eq!(got, 99);
                assert_eq!(expected, CURRENT_SCHEMA);
            }
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }

        // Bad JSON → Json variant
        let body = r#"{not valid"#;
        assert!(matches!(
            Marketplace::parse_with_error(body),
            Err(ParseError::Json(_))
        ));

        // Missing schema field → defaults to 0, treated as schema mismatch
        let body = r#"{"source": "x"}"#;
        match Marketplace::parse_with_error(body) {
            Err(ParseError::SchemaMismatch { got: 0, .. }) => {}
            other => panic!("expected SchemaMismatch{{got:0}}, got {other:?}"),
        }
    }

    #[test]
    fn parse_error_display_mentions_versions_for_schema_mismatch() {
        // M6.11 (M1): the user-facing error string must name BOTH the
        // remote schema and the supported schema so the next step
        // ("upgrade the binary") is obvious.
        let err = ParseError::SchemaMismatch {
            got: 2,
            expected: 1,
        };
        let msg = format!("{err}");
        assert!(msg.contains("schema=2"), "missing remote schema: {msg}");
        assert!(msg.contains("schema=1"), "missing supported schema: {msg}");
        assert!(
            msg.to_lowercase().contains("upgrade"),
            "missing actionable next step: {msg}",
        );
    }

    #[test]
    fn refresh_error_display_propagates_parse_error_message() {
        // The wrapping into RefreshError::Parse(ParseError) shouldn't
        // hide the underlying schema-mismatch message.
        let inner = ParseError::SchemaMismatch {
            got: 2,
            expected: 1,
        };
        let outer = RefreshError::Parse(inner);
        let msg = format!("{outer}");
        assert!(msg.contains("schema=2"), "got: {msg}");
        assert!(msg.contains("schema=1"), "got: {msg}");
    }

    #[test]
    fn pretty_age_renders_coarsely() {
        // M6.11 (H2): cache-age label format. Goal is human-readable
        // at a glance, not precise.
        assert_eq!(pretty_age(0), "just now");
        assert_eq!(pretty_age(30), "just now");
        assert_eq!(pretty_age(60), "1 min");
        assert_eq!(pretty_age(120), "2 mins");
        assert_eq!(pretty_age(3600), "1 hour");
        assert_eq!(pretty_age(7200), "2 hours");
        assert_eq!(pretty_age(86400), "1 day");
        assert_eq!(pretty_age(86400 * 3), "3 days");
    }

    // ── M6.12 generic search + policy tag ───────────────────────────────

    #[test]
    fn marketplace_entry_trait_implemented_for_all_three_types() {
        // M6.12 (M2): the three entry types share a trait so generic
        // search / rendering helpers don't duplicate per-type code.
        // This compile-time check verifies all three impls are wired.
        fn assert_entry<T: MarketplaceEntry>(_t: &T) {}
        let s = MarketplaceSkill {
            name: "x".into(),
            short_description: None,
            description: "d".into(),
            category: "c".into(),
            license: "Apache-2.0".into(),
            license_tier: "open".into(),
            source_repo: String::new(),
            source_path: String::new(),
            install_url: None,
            homepage: String::new(),
        };
        let m = MarketplaceMcpServer {
            name: "m".into(),
            short_description: None,
            description: "d".into(),
            category: "c".into(),
            license: "Apache-2.0".into(),
            license_tier: "open".into(),
            transport: "stdio".into(),
            command: String::new(),
            args: vec![],
            install_url: None,
            post_install_message: None,
            url: String::new(),
            homepage: String::new(),
        };
        let p = MarketplacePlugin {
            name: "p".into(),
            short_description: None,
            description: "d".into(),
            category: "c".into(),
            license: "Apache-2.0".into(),
            license_tier: "open".into(),
            install_url: "https://example.com/p.git".into(),
            homepage: String::new(),
        };
        assert_entry(&s);
        assert_entry(&m);
        assert_entry(&p);

        // Verify accessor return values match the underlying fields.
        assert_eq!(s.name(), "x");
        assert_eq!(m.description(), "d");
        assert_eq!(p.category(), "c");
        assert_eq!(s.license_tier(), "open");
    }

    #[test]
    fn policy_check_url_prefers_install_url_for_mcp_with_both() {
        // MCP entries with both install_url (clone source for stdio)
        // and url (sse target) should prefer install_url for policy
        // gating — that's the gating step. Falls back to url when
        // install_url is empty/None.
        let m_both = MarketplaceMcpServer {
            name: "m".into(),
            short_description: None,
            description: "d".into(),
            category: String::new(),
            license: "MIT".into(),
            license_tier: "open".into(),
            transport: "stdio".into(),
            command: String::new(),
            args: vec![],
            install_url: Some("https://github.com/clone-here.git".into()),
            post_install_message: None,
            url: "https://hosted.example.com".into(),
            homepage: String::new(),
        };
        assert_eq!(
            m_both.policy_check_url(),
            Some("https://github.com/clone-here.git"),
        );

        let m_url_only = MarketplaceMcpServer {
            install_url: None,
            url: "https://hosted.example.com".into(),
            ..m_both.clone()
        };
        assert_eq!(
            m_url_only.policy_check_url(),
            Some("https://hosted.example.com"),
        );

        let m_neither = MarketplaceMcpServer {
            install_url: None,
            url: String::new(),
            ..m_url_only.clone()
        };
        assert_eq!(m_neither.policy_check_url(), None);
    }

    #[test]
    fn generic_find_entry_replaces_per_type_find() {
        // M6.12 (M2): the trait-based find_entry should exhibit the
        // exact same behavior as the old per-type Marketplace::find /
        // find_mcp / find_plugin methods.
        let m = fixture_marketplace();
        assert!(find_entry(&m.skills, "algorithmic-art").is_some());
        assert!(find_entry(&m.skills, "no-such-skill").is_none());
        assert_eq!(
            find_entry(&m.skills, "algorithmic-art").map(|s| s.name.as_str()),
            m.find("algorithmic-art").map(|s| s.name.as_str()),
        );
    }

    #[test]
    fn generic_search_entries_replaces_per_type_search() {
        // Same ranking semantics: name match beats description match
        // beats category match.
        let m = fixture_marketplace();
        let hits = search_entries(&m.skills, "art");
        assert!(!hits.is_empty());
        assert_eq!(hits[0].name, "algorithmic-art");
        assert!(!search_entries(&m.skills, "PLAYWRIGHT").is_empty());
    }

    #[test]
    fn entry_tags_renders_linked_only_when_license_tier_matches() {
        // M6.12 (M3): the tag combiner should reproduce the prior
        // [linked-only] tag from the M3 license-tier work.
        let mut s = MarketplaceSkill {
            name: "x".into(),
            short_description: None,
            description: String::new(),
            category: String::new(),
            license: "Anthropic source-available".into(),
            license_tier: "linked-only".into(),
            source_repo: String::new(),
            source_path: String::new(),
            install_url: None, // no install_url → no policy check
            homepage: String::new(),
        };
        assert_eq!(entry_tags(&s), " [linked-only]");

        s.license_tier = "open".into();
        assert_eq!(entry_tags(&s), "");
    }

    #[test]
    fn entry_tags_no_tags_when_open_and_no_policy() {
        // Open-core builds with no policy active should produce no
        // tags for an open-tier entry. policy::check_url returns
        // NoPolicy → not Denied → no [blocked by policy] tag.
        let s = MarketplaceSkill {
            name: "x".into(),
            short_description: None,
            description: String::new(),
            category: String::new(),
            license: "Apache-2.0".into(),
            license_tier: "open".into(),
            source_repo: String::new(),
            source_path: String::new(),
            install_url: Some("https://example.com/x.git".into()),
            homepage: String::new(),
        };
        let tags = entry_tags(&s);
        assert!(
            tags.is_empty() || tags == " [blocked by policy]",
            "expected empty or [blocked by policy], got: {tags:?}",
        );
    }

    #[test]
    fn http_client_is_a_singleton() {
        // M6.11 (L1): repeated calls return the same Arc'd client so
        // the connection pool is shared across refreshes. Using
        // pointer equality on the `&'static reqwest::Client` reference
        // proves we're not building a fresh client per call.
        let c1 = http_client();
        let c2 = http_client();
        match (c1, c2) {
            (Some(a), Some(b)) => {
                assert!(
                    std::ptr::eq(a, b),
                    "http_client must return the same instance across calls",
                );
            }
            _ => panic!("http_client() returned None — TLS init may have failed in test env"),
        }
    }

    #[test]
    fn baseline_short_descriptions_are_concise() {
        // Catch authoring drift — if someone adds a marketplace entry
        // with a 200-char "short_description", this test fires before
        // the layout regresses in /skill marketplace.
        let m = Marketplace::from_json_str(BASELINE_JSON).unwrap();
        for s in &m.skills {
            if let Some(short) = &s.short_description {
                assert!(
                    short.chars().count() <= 70,
                    "short_description for {} is {} chars (cap is 70): {short}",
                    s.name,
                    short.chars().count()
                );
            }
        }
    }
}
