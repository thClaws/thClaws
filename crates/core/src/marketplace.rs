//! Skill (and later: plugin / MCP) marketplace catalogue.
//!
//! Mirrors [`crate::model_catalogue`]'s three-layer pattern:
//!   1. Embedded baseline compiled into the binary (`resources/marketplace.json`)
//!      so first-launch search/install works with no network.
//!   2. User cache at `~/.config/thclaws/marketplace.json`, written when
//!      the user runs `/skill marketplace --refresh` or via daily auto-refresh.
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

impl Marketplace {
    /// Parse a JSON body into a marketplace catalogue, rejecting wrong
    /// schemas. Used for both the embedded baseline and the user cache.
    pub fn from_json_str(body: &str) -> Option<Self> {
        let parsed: Self = serde_json::from_str(body).ok()?;
        if parsed.schema != CURRENT_SCHEMA {
            return None;
        }
        Some(parsed)
    }

    /// Look up a skill by exact-match name. Returns `None` if the user
    /// typed a name not in the catalogue (caller should suggest
    /// `/skill search` instead).
    pub fn find(&self, name: &str) -> Option<&MarketplaceSkill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Substring-match search across name, description, and category.
    /// Case-insensitive, ranked by where the match lands (name match
    /// beats description match beats category match).
    pub fn search(&self, query: &str) -> Vec<&MarketplaceSkill> {
        let q = query.to_lowercase();
        let mut hits: Vec<(u8, &MarketplaceSkill)> = Vec::new();
        for s in &self.skills {
            if s.name.to_lowercase().contains(&q) {
                hits.push((0, s));
            } else if s.description.to_lowercase().contains(&q) {
                hits.push((1, s));
            } else if s.category.to_lowercase().contains(&q) {
                hits.push((2, s));
            }
        }
        hits.sort_by_key(|(rank, _)| *rank);
        hits.into_iter().map(|(_, s)| s).collect()
    }

    pub fn find_mcp(&self, name: &str) -> Option<&MarketplaceMcpServer> {
        self.mcp_servers.iter().find(|s| s.name == name)
    }

    pub fn search_mcp(&self, query: &str) -> Vec<&MarketplaceMcpServer> {
        let q = query.to_lowercase();
        let mut hits: Vec<(u8, &MarketplaceMcpServer)> = Vec::new();
        for s in &self.mcp_servers {
            if s.name.to_lowercase().contains(&q) {
                hits.push((0, s));
            } else if s.description.to_lowercase().contains(&q) {
                hits.push((1, s));
            } else if s.category.to_lowercase().contains(&q) {
                hits.push((2, s));
            }
        }
        hits.sort_by_key(|(rank, _)| *rank);
        hits.into_iter().map(|(_, s)| s).collect()
    }

    pub fn find_plugin(&self, name: &str) -> Option<&MarketplacePlugin> {
        self.plugins.iter().find(|s| s.name == name)
    }

    pub fn search_plugin(&self, query: &str) -> Vec<&MarketplacePlugin> {
        let q = query.to_lowercase();
        let mut hits: Vec<(u8, &MarketplacePlugin)> = Vec::new();
        for s in &self.plugins {
            if s.name.to_lowercase().contains(&q) {
                hits.push((0, s));
            } else if s.description.to_lowercase().contains(&q) {
                hits.push((1, s));
            } else if s.category.to_lowercase().contains(&q) {
                hits.push((2, s));
            }
        }
        hits.sort_by_key(|(rank, _)| *rank);
        hits.into_iter().map(|(_, s)| s).collect()
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

/// Fetch the remote marketplace and, if it parses, write it to the
/// cache. Same fail-silent contract as `model_catalogue::refresh_from_remote`.
pub async fn refresh_from_remote() -> Result<RefreshOutcome, RefreshError> {
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| RefreshError::Http(e.to_string()))?
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
    let parsed = Marketplace::from_json_str(&body).ok_or(RefreshError::Parse)?;
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

#[derive(Debug)]
pub enum RefreshError {
    Http(String),
    Parse,
    Io(String),
    NoHome,
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshError::Http(e) => write!(f, "http: {e}"),
            RefreshError::Parse => write!(f, "remote payload didn't match marketplace schema"),
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
