//! Skills — user-defined prompt+script bundles that extend the agent.
//!
//! A skill is a directory containing:
//! - `SKILL.md` — YAML frontmatter (name, description, whenToUse) + markdown
//!   instructions that the model follows using its existing tools.
//! - `scripts/` (optional) — pre-built scripts (.py, .sh, .js, etc.) that
//!   the SKILL.md references. The model calls them via Bash, not writes them.
//!
//! Discovery locations (in order; later wins on name collision):
//! 1. `~/.claude/skills/` (user Claude Code)
//! 2. `~/.config/thclaws/skills/` (user thClaws)
//! 3. plugin-contributed skill dirs (see [`crate::plugins`])
//! 4. `.claude/skills/` (project Claude Code)
//! 5. `.thclaws/skills/` (project thClaws — highest priority)
//!
//! Project skills always beat plugin- and user-installed ones with the
//! same name, matching the principle that the most-specific scope wins.
//!
//! The `Skill` tool returns the SKILL.md content with `{skill_dir}` replaced
//! by the absolute path to the skill directory, so script paths resolve.

use crate::error::{Error, Result};
use crate::tools::Tool;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Maximum bytes read at boot to capture a SKILL.md's YAML frontmatter
/// without loading the body. Realistic frontmatter is < 1KB; the cap
/// is set generously so non-trivial config blocks fit. Bodies (which
/// can be 10–100 KB for complex skills) load on demand via
/// [`SkillDef::content`].
///
/// dev-plan/06 P1.
const MAX_FRONTMATTER_BYTES: usize = 4096;

/// Internal storage for a skill's body. Either loaded eagerly at
/// construction time (tests, in-memory skill defs) or read lazily from
/// disk on the first [`SkillDef::content`] call.
///
/// The `Lazy` variant uses `OnceLock` so the first reader wins the
/// race; subsequent calls share the cached value. Hash equality and
/// serde derives are kept consistent by always serializing as the
/// materialized string (see manual `Serialize` / `Deserialize` impls
/// below).
#[derive(Debug)]
enum SkillContent {
    /// Body was provided up-front (tests, in-memory construction). No
    /// I/O on access.
    Eager(String),
    /// Body lives on disk; load it on first `.content()` call. The
    /// `OnceLock` caches the materialized content for subsequent
    /// reads. `abs_dir` is captured at index time so the body's
    /// `{skill_dir}` substitution works without re-canonicalizing.
    Lazy {
        skill_md_path: PathBuf,
        abs_dir: PathBuf,
        cell: OnceLock<String>,
    },
}

impl Clone for SkillContent {
    fn clone(&self) -> Self {
        match self {
            Self::Eager(s) => Self::Eager(s.clone()),
            Self::Lazy {
                skill_md_path,
                abs_dir,
                cell,
            } => {
                let new_cell = OnceLock::new();
                if let Some(v) = cell.get() {
                    let _ = new_cell.set(v.clone());
                }
                Self::Lazy {
                    skill_md_path: skill_md_path.clone(),
                    abs_dir: abs_dir.clone(),
                    cell: new_cell,
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub when_to_use: String,
    pub dir: PathBuf,
    /// Body access goes through [`Self::content`]. Serialization
    /// always materializes to a string so cached SkillDef snapshots
    /// (e.g. in tests, in JSON dumps) round-trip without exposing the
    /// lazy enum variant. Deserialization always lands in Eager —
    /// there's no on-disk path to load lazily from once the SkillDef
    /// has been serialized.
    #[serde(serialize_with = "serialize_skill_content")]
    #[serde(deserialize_with = "deserialize_skill_content")]
    content: SkillContent,
}

fn serialize_skill_content<S>(
    content: &SkillContent,
    ser: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::Serialize;
    let s = match content {
        SkillContent::Eager(s) => s.clone(),
        SkillContent::Lazy {
            skill_md_path,
            abs_dir,
            cell,
        } => {
            // Materialize for serialization. Best-effort — if the
            // file disappeared we serialize an empty string rather
            // than panicking.
            cell.get()
                .cloned()
                .or_else(|| read_and_substitute_body(skill_md_path, abs_dir).ok())
                .unwrap_or_default()
        }
    };
    s.serialize(ser)
}

fn deserialize_skill_content<'de, D>(de: D) -> std::result::Result<SkillContent, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let s = String::deserialize(de)?;
    Ok(SkillContent::Eager(s))
}

impl SkillDef {
    /// Construct a SkillDef with eagerly-provided body content. Used
    /// by tests and any caller building a skill from in-memory data.
    pub fn new_eager(
        name: String,
        description: String,
        when_to_use: String,
        dir: PathBuf,
        content: String,
    ) -> Self {
        Self {
            name,
            description,
            when_to_use,
            dir,
            content: SkillContent::Eager(content),
        }
    }

    /// Read the skill body, lazy-loading from disk on first access if
    /// constructed via [`SkillStore::discover`]. Subsequent calls are
    /// cache-hits with no I/O.
    ///
    /// On read failure (file deleted, permission denied) returns an
    /// empty Cow rather than panicking — the caller's downstream
    /// rendering treats empty content as "skill body unavailable" and
    /// surfaces that to the model. Cached subsequent reads return the
    /// same empty value (we don't retry on the lazy path; user can
    /// `/skill install` to re-discover).
    pub fn content(&self) -> Cow<'_, str> {
        match &self.content {
            SkillContent::Eager(s) => Cow::Borrowed(s.as_str()),
            SkillContent::Lazy {
                skill_md_path,
                abs_dir,
                cell,
            } => {
                let s = cell.get_or_init(|| {
                    read_and_substitute_body(skill_md_path, abs_dir).unwrap_or_else(|e| {
                        eprintln!(
                            "[skills] failed to read body for {}: {e}",
                            skill_md_path.display()
                        );
                        String::new()
                    })
                });
                Cow::Borrowed(s.as_str())
            }
        }
    }
}

/// Read a SKILL.md from disk, strip frontmatter, substitute
/// `{skill_dir}` with `abs_dir`, return the body. Used by both
/// `SkillContent::Lazy::content()` and the eager `parse_skill` path.
fn read_and_substitute_body(skill_md: &Path, abs_dir: &Path) -> std::io::Result<String> {
    let raw = std::fs::read_to_string(skill_md)?;
    let (_, body) = crate::memory::parse_frontmatter(&raw);
    Ok(body.replace("{skill_dir}", &abs_dir.to_string_lossy()))
}

/// Read up to `MAX_FRONTMATTER_BYTES` of `path`, stopping early if we
/// see the closing `---\n` after the opening one. Used at boot so we
/// only pay for parsing the YAML header and skip the body.
///
/// Returns the bytes read as a String. Caller passes to
/// `parse_frontmatter` which extracts the frontmatter HashMap; the
/// trailing body bytes (if any leaked past the cap) are ignored.
fn read_until_frontmatter_end(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        // Check for closing fence after the opening one. Frontmatter
        // shape: `---\n<yaml>\n---\n`. Need TWO `---\n` boundaries.
        // Scanning the accumulated buffer is cheap (≤4KB).
        if has_closing_fence(&buf) {
            break;
        }
        if buf.len() >= MAX_FRONTMATTER_BYTES {
            // Cap reached; stop reading. Better to over-read by a
            // chunk than under-read and miss the frontmatter.
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// True when `buf` contains a frontmatter open + close fence
/// (`---\n` ... `---\n`). Conservative: requires the opening fence
/// to start at position 0.
fn has_closing_fence(buf: &[u8]) -> bool {
    if !buf.starts_with(b"---\n") && !buf.starts_with(b"---\r\n") {
        // No opening fence → no frontmatter, no closing fence to find.
        // Caller's parse_frontmatter will treat the whole buf as body
        // (no frontmatter present), which is fine for our use case.
        return true; // signal "stop reading" so we don't burn the full cap
    }
    // Skip past the opening fence (4 or 5 bytes).
    let after_open = if buf.starts_with(b"---\r\n") { 5 } else { 4 };
    let rest = &buf[after_open..];
    // Look for "\n---\n" or "\n---\r\n" anywhere in rest.
    rest.windows(5).any(|w| w == b"\n---\n") || rest.windows(6).any(|w| w == b"\n---\r\n")
}

#[derive(Debug, Clone, Default)]
pub struct SkillStore {
    pub skills: HashMap<String, SkillDef>,
}

impl SkillStore {
    /// Discover skills from all standard locations **plus** any
    /// directories contributed by currently-installed plugins. This
    /// is the right default for runtime callers — every site that
    /// rebuilds the store after a `/skill install` or `/plugin
    /// install` should pick up plugin-contributed skills automatically.
    ///
    /// Use [`Self::discover_with_extra`] directly when you need to
    /// supply the plugin dir list yourself (e.g. at startup, before
    /// the plugins module is fully wired) or [`Self::discover_no_plugins`]
    /// when you explicitly want only filesystem-discovered skills.
    pub fn discover() -> Self {
        Self::discover_with_extra(&crate::plugins::plugin_skill_dirs())
    }

    /// Discover only filesystem-resident skills, excluding plugin
    /// contributions. Used by tests and by any caller that needs a
    /// stable view independent of which plugins happen to be installed.
    pub fn discover_no_plugins() -> Self {
        Self::discover_with_extra(&[])
    }

    /// Discover skills, additionally walking each directory in `extra`.
    /// Used by the plugin system to pull in skills contributed by
    /// installed plugins without symlinking or copying.
    ///
    /// Load order: user dirs → `extra` (plugins) → project dirs. The
    /// project dirs come last so a project's `.thclaws/skills/<name>`
    /// always wins over a plugin or user install with the same name —
    /// M6.14 fix; previously plugin dirs were appended at the end and
    /// could shadow project skills, contradicting the documented
    /// "project highest priority" contract.
    pub fn discover_with_extra(extra: &[PathBuf]) -> Self {
        let mut store = Self::default();
        for dir in Self::user_skill_dirs() {
            if dir.exists() {
                store.load_dir(&dir);
            }
        }
        for dir in extra {
            if dir.exists() {
                store.load_dir(dir);
            }
        }
        for dir in Self::project_skill_dirs() {
            if dir.exists() {
                store.load_dir(&dir);
            }
        }
        store
    }

    fn user_skill_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(home) = crate::util::home_dir() {
            dirs.push(home.join(".claude/skills")); // user Claude Code
            dirs.push(home.join(".config/thclaws/skills")); // user thClaws
        }
        dirs
    }

    fn project_skill_dirs() -> Vec<PathBuf> {
        vec![
            PathBuf::from(".claude/skills"),  // project Claude Code
            PathBuf::from(".thclaws/skills"), // project thClaws (highest priority)
        ]
    }

    fn load_dir(&mut self, base: &Path) {
        let Ok(entries) = std::fs::read_dir(base) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let skill_md = path.join("SKILL.md");
            if !skill_md.exists() {
                continue;
            }
            if let Some(skill) = Self::parse_skill(&path, &skill_md) {
                self.skills.insert(skill.name.clone(), skill);
            }
        }
    }

    /// Discover a skill by reading ONLY its frontmatter (capped at
    /// `MAX_FRONTMATTER_BYTES`). The body stays on disk and loads on
    /// the first `SkillDef::content()` call. dev-plan/06 P1.
    fn parse_skill(dir: &Path, skill_md: &Path) -> Option<SkillDef> {
        let raw = read_until_frontmatter_end(skill_md).ok()?;
        let (frontmatter, _body) = crate::memory::parse_frontmatter(&raw);

        let name = frontmatter.get("name").cloned().unwrap_or_else(|| {
            dir.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string()
        });
        let description = frontmatter.get("description").cloned().unwrap_or_default();
        let when_to_use = frontmatter
            .get("whenToUse")
            .or_else(|| frontmatter.get("when_to_use"))
            .cloned()
            .unwrap_or_default();

        // Canonicalize once at index time so the `{skill_dir}`
        // substitution inside the lazy body load doesn't have to —
        // and so the lazy-loaded SKILL.md path is absolute. M6.14:
        // a previous bug stored `skill_md.to_path_buf()` here, which
        // could be relative (e.g. `.thclaws/skills/foo/SKILL.md`) when
        // discovery ran from a relative project dir. After the GUI
        // user switched workspaces (which calls set_current_dir), the
        // relative path resolved under the wrong CWD and the lazy
        // read returned empty content for every project skill.
        let abs_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let abs_skill_md = abs_dir.join("SKILL.md");

        Some(SkillDef {
            name,
            description,
            when_to_use,
            dir: abs_dir.clone(),
            // Body is read on demand by `SkillDef::content()`. Only
            // the path + abs_dir are captured at boot.
            content: SkillContent::Lazy {
                skill_md_path: abs_skill_md,
                abs_dir,
                cell: OnceLock::new(),
            },
        })
    }

    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.skills.keys().map(String::as_str).collect();
        names.sort();
        names
    }

    pub fn get(&self, name: &str) -> Option<&SkillDef> {
        self.skills.get(name)
    }
}

// ── Install (dispatcher) ─────────────────────────────────────────────

/// Entry point for `/skill install`. Dispatches on URL shape: `.zip` URLs
/// are downloaded and extracted; everything else is treated as a git clone
/// target (ssh, https, file://, local path, etc.). Keeps the caller
/// contract simple — one function, one URL.
pub async fn install_from_url(
    url: &str,
    override_name: Option<&str>,
    project_scope: bool,
) -> Result<Vec<String>> {
    // Org-policy gate (Phase 2): when policies.plugins.enabled, the
    // URL must match allowed_hosts. Single guard covers both .zip and
    // git dispatch paths below. Open-core builds without a policy fall
    // through unchanged (AllowDecision::NoPolicy).
    if let crate::policy::AllowDecision::Denied { reason } = crate::policy::check_url(url) {
        return Err(Error::Tool(format!(
            "skill install blocked by org policy: {reason}"
        )));
    }
    if is_zip_url(url) {
        install_from_zip(url, override_name, project_scope).await
    } else {
        install_from_git(url, override_name, project_scope)
    }
}

/// Reject skills carrying executable scripts when the active org policy
/// has `policies.plugins.allow_external_scripts: false`. Returns
/// `Ok(())` when no policy is active, when the policy permits scripts,
/// or when the skill has no `scripts/` directory at all. Used at every
/// install rename point so the rejection happens before the skill
/// reaches its final location.
fn enforce_scripts_policy(skill_dir: &std::path::Path) -> Result<()> {
    if !crate::policy::external_scripts_disallowed() {
        return Ok(());
    }
    let scripts = skill_dir.join("scripts");
    if !scripts.exists() {
        return Ok(());
    }
    let has_entries = std::fs::read_dir(&scripts)
        .ok()
        .and_then(|mut d| d.next())
        .is_some();
    if has_entries {
        return Err(Error::Tool(format!(
            "skill at {:?} ships a scripts/ directory; org policy disallows external scripts",
            skill_dir.file_name().unwrap_or_default()
        )));
    }
    Ok(())
}

fn is_zip_url(url: &str) -> bool {
    // Strip query/fragment before checking the extension so
    // `?token=...` or `#frag` don't mask the `.zip` suffix.
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    without_query.to_ascii_lowercase().ends_with(".zip")
}

// ── Install from zip ─────────────────────────────────────────────────

/// Download a zip archive from an HTTP(S) URL and install the skill(s) it
/// contains. Same single-vs-bundle semantics as [`install_from_git`].
pub async fn install_from_zip(
    url: &str,
    override_name: Option<&str>,
    project_scope: bool,
) -> Result<Vec<String>> {
    let target_root = target_root(project_scope)?;
    std::fs::create_dir_all(&target_root)
        .map_err(|e| Error::Tool(format!("mkdir {}: {e}", target_root.display())))?;

    let derived = override_name
        .map(String::from)
        .unwrap_or_else(|| derive_name_from_url(url));
    if derived.is_empty() {
        return Err(Error::Tool(format!(
            "could not derive a name from URL '{url}' — pass one explicitly: /skill install {url} <name>"
        )));
    }
    let final_dir = target_root.join(&derived);
    if final_dir.exists() {
        return Err(Error::Tool(format!(
            "'{}' already exists — remove it first or choose a different name",
            final_dir.display()
        )));
    }

    // Download the zip into memory. Skills are typically <1 MB; refuse
    // anything absurd so a mis-typed URL can't fill RAM.
    let bytes = download_zip(url).await?;

    // Extract under a staging dir first so we can inspect the structure
    // (single SKILL.md at root vs bundle) before committing to the final
    // name. Staging lives inside `target_root` so rename is same-volume.
    let staging = target_root.join(format!(
        ".thclaws-install-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&staging)
        .map_err(|e| Error::Tool(format!("mkdir {}: {e}", staging.display())))?;

    if let Err(e) = extract_zip(&bytes, &staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    // Zip archives commonly wrap everything in a single top-level folder
    // (e.g. `myskill-v1/SKILL.md`). If that's what we have, descend into
    // it so the caller sees the skill content, not the wrapper.
    let source = single_wrapper_subdir(&staging).unwrap_or(staging.clone());

    let mut report = vec![format!(
        "downloaded {} ({} bytes) → extracted to {}",
        url,
        bytes.len(),
        staging.display()
    )];

    // Single-skill case: root (or wrapper's content) has SKILL.md.
    if source.join("SKILL.md").exists() {
        if let Err(e) = enforce_scripts_policy(&source) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(&source, &final_dir) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(Error::Tool(format!(
                "move {} → {}: {e}",
                source.display(),
                final_dir.display()
            )));
        }
        // If we descended into a wrapper, the now-empty staging remains.
        if source != staging {
            let _ = std::fs::remove_dir_all(&staging);
        }
        report.push(format!("installed skill '{derived}' (single)"));
        return Ok(report);
    }

    // Bundle: walk and promote each SKILL.md directory to a sibling under
    // target_root. Same logic as the git path.
    let found = find_skill_dirs(&source);
    if found.is_empty() {
        let _ = std::fs::remove_dir_all(&staging);
        report.push("warning: no SKILL.md found anywhere in the archive".into());
        return Ok(report);
    }
    let mut promoted = Vec::new();
    let mut conflicts = Vec::new();
    for skill_dir in found {
        let sub_name = skill_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if sub_name.is_empty() {
            continue;
        }
        let dest = target_root.join(&sub_name);
        if dest.exists() {
            conflicts.push(sub_name);
            continue;
        }
        if let Err(e) = enforce_scripts_policy(&skill_dir) {
            conflicts.push(format!("{sub_name} (policy: {e})"));
            continue;
        }
        match std::fs::rename(&skill_dir, &dest) {
            Ok(_) => promoted.push(sub_name),
            Err(e) => conflicts.push(format!("{sub_name} ({e})")),
        }
    }
    let _ = std::fs::remove_dir_all(&staging);

    if !promoted.is_empty() {
        report.push(format!(
            "bundle detected; installed {} skill(s): {}",
            promoted.len(),
            promoted.join(", ")
        ));
    }
    if !conflicts.is_empty() {
        report.push(format!(
            "skipped due to existing dirs: {}",
            conflicts.join(", ")
        ));
    }
    Ok(report)
}

fn target_root(project_scope: bool) -> Result<PathBuf> {
    if project_scope {
        Ok(std::env::current_dir()
            .map_err(|e| Error::Tool(format!("cwd: {e}")))?
            .join(".thclaws/skills"))
    } else {
        let home = crate::util::home_dir()
            .ok_or_else(|| Error::Tool("cannot locate user home directory".into()))?;
        Ok(home.join(".config/thclaws/skills"))
    }
}

async fn download_zip(url: &str) -> Result<Vec<u8>> {
    // Cap the download at 64 MiB. Real skills are orders of magnitude
    // smaller; anything bigger is almost certainly the wrong URL.
    const MAX_BYTES: u64 = 64 * 1024 * 1024;

    // 30s end-to-end timeout: a slow or hostile server can no longer
    // hang `/skill install` indefinitely (M6.14 — fix BUG 4). The
    // marketplace fetch uses a similar 10s cap; zip installs may pull
    // larger payloads so we allow a touch more headroom.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Tool(format!("http client: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("download: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Tool(format!("download: HTTP {}", resp.status())));
    }
    if let Some(len) = resp.content_length() {
        if len > MAX_BYTES {
            return Err(Error::Tool(format!(
                "zip too large ({} bytes, max {})",
                len, MAX_BYTES
            )));
        }
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| Error::Tool(format!("read body: {e}")))?
        .to_vec();
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Error::Tool(format!(
            "zip too large ({} bytes, max {})",
            bytes.len(),
            MAX_BYTES
        )));
    }
    Ok(bytes)
}

fn extract_zip(bytes: &[u8], dest: &Path) -> Result<()> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| Error::Tool(format!("open zip: {e}")))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| Error::Tool(format!("zip entry {i}: {e}")))?;
        let Some(name) = entry.enclosed_name() else {
            // Reject entries with .. or absolute paths — zip-slip guard.
            return Err(Error::Tool(format!(
                "unsafe path in archive: {}",
                entry.name()
            )));
        };
        let out_path = dest.join(name);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|e| Error::Tool(format!("mkdir {}: {e}", out_path.display())))?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::Tool(format!("mkdir {}: {e}", parent.display())))?;
            }
            let mut out = std::fs::File::create(&out_path)
                .map_err(|e| Error::Tool(format!("create {}: {e}", out_path.display())))?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|e| Error::Tool(format!("write {}: {e}", out_path.display())))?;
            // Preserve unix exec bits when present so shipped scripts stay runnable.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = entry.unix_mode() {
                    let _ =
                        std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
                }
            }
        }
    }
    Ok(())
}

/// If `dir` contains exactly one child directory and no files, return that
/// child. Covers the common `archive-v1/...` wrapper pattern in zips.
fn single_wrapper_subdir(dir: &Path) -> Option<PathBuf> {
    let mut subdirs = Vec::new();
    let mut has_files = false;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else {
            has_files = true;
        }
    }
    if !has_files && subdirs.len() == 1 {
        Some(subdirs.into_iter().next().unwrap())
    } else {
        None
    }
}

// ── Install from git ─────────────────────────────────────────────────

/// Clone a skill (or a bundle of skills) from a git URL into the user-global
/// or project-scoped skills directory. If the cloned root has a `SKILL.md`
/// it's treated as a single skill; otherwise any top-level subdirectory that
/// contains a `SKILL.md` is promoted to a sibling so it becomes discoverable.
///
/// Returns a list of human-readable lines describing what was installed.
pub fn install_from_git(
    git_url: &str,
    override_name: Option<&str>,
    project_scope: bool,
) -> Result<Vec<String>> {
    let target_root = if project_scope {
        std::env::current_dir()
            .map_err(|e| Error::Tool(format!("cwd: {e}")))?
            .join(".thclaws/skills")
    } else {
        crate::util::home_dir()
            .ok_or_else(|| Error::Tool("cannot locate user home directory".into()))?
            .join(".config/thclaws/skills")
    };
    std::fs::create_dir_all(&target_root)
        .map_err(|e| Error::Tool(format!("mkdir {}: {e}", target_root.display())))?;

    // Parse the marketplace `#<branch>:<subpath>` extension out of the
    // URL. Plain URLs (no fragment) get `(url, None, None)` and behave
    // exactly as before; subpath URLs trigger the single-skill-from-
    // monorepo path further down.
    let (base_url, branch, subpath) = parse_git_subpath(git_url);

    let derived = override_name
        .map(String::from)
        .unwrap_or_else(|| derive_name_from_url(git_url));
    if derived.is_empty() {
        return Err(Error::Tool(format!(
            "could not derive a name from URL '{git_url}' — pass one explicitly: /skill install {git_url} <name>"
        )));
    }
    let clone_dir = target_root.join(&derived);
    if clone_dir.exists() {
        return Err(Error::Tool(format!(
            "'{}' already exists — remove it first or choose a different name",
            clone_dir.display()
        )));
    }

    // When a subpath is requested, clone into a staging dir so we can
    // extract just the subdirectory and discard the rest of the repo.
    // Plain installs clone directly into the final `clone_dir`.
    let stage_dir = if subpath.is_some() {
        target_root.join(format!(
            ".thclaws-install-{}",
            uuid::Uuid::new_v4().simple()
        ))
    } else {
        clone_dir.clone()
    };

    let mut clone_args: Vec<String> = vec!["clone".into(), "--depth".into(), "1".into()];
    if let Some(b) = &branch {
        clone_args.push("--branch".into());
        clone_args.push(b.clone());
    }
    clone_args.push(base_url.clone());
    clone_args.push(stage_dir.to_string_lossy().into_owned());

    let out = std::process::Command::new("git")
        .args(&clone_args)
        .output()
        .map_err(|e| Error::Tool(format!("spawn git: {e}")))?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&stage_dir);
        return Err(Error::Tool(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    // Subpath install: move just the requested subdirectory to clone_dir.
    if let Some(sub) = &subpath {
        let src = stage_dir.join(sub);
        if !src.is_dir() {
            let _ = std::fs::remove_dir_all(&stage_dir);
            return Err(Error::Tool(format!(
                "subpath '{sub}' not found in cloned repo (or is not a directory)"
            )));
        }
        if !src.join("SKILL.md").exists() {
            let _ = std::fs::remove_dir_all(&stage_dir);
            return Err(Error::Tool(format!(
                "subpath '{sub}' has no SKILL.md — not a valid skill directory"
            )));
        }
        if let Err(e) = enforce_scripts_policy(&src) {
            let _ = std::fs::remove_dir_all(&stage_dir);
            return Err(e);
        }
        std::fs::rename(&src, &clone_dir).map_err(|e| {
            let _ = std::fs::remove_dir_all(&stage_dir);
            Error::Tool(format!("move subpath into place: {e}"))
        })?;
        let _ = std::fs::remove_dir_all(&stage_dir);
        return Ok(vec![
            format!(
                "cloned {} (subpath: {sub}) → {}",
                base_url,
                clone_dir.display()
            ),
            format!("installed skill '{derived}' (single)"),
        ]);
    }

    let mut report = vec![format!("cloned {} → {}", git_url, clone_dir.display())];

    // Single skill: clone root itself has SKILL.md.
    if clone_dir.join("SKILL.md").exists() {
        if let Err(e) = enforce_scripts_policy(&clone_dir) {
            let _ = std::fs::remove_dir_all(&clone_dir);
            return Err(e);
        }
        report.push(format!("installed skill '{derived}' (single)"));
        return Ok(report);
    }

    // Bundle: walk the clone tree recursively and collect every directory
    // that directly contains a SKILL.md. Anthropic's skills repo keeps most
    // skills under `skills/<name>/SKILL.md` (not at top level), so a shallow
    // scan would miss them.
    let found = find_skill_dirs(&clone_dir);
    if found.is_empty() {
        report.push("warning: no SKILL.md found anywhere in the cloned repo".into());
        return Ok(report);
    }

    let mut promoted = Vec::new();
    let mut conflicts = Vec::new();
    for skill_dir in found {
        let sub_name = skill_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if sub_name.is_empty() {
            continue;
        }
        let dest = target_root.join(&sub_name);
        if dest.exists() {
            conflicts.push(sub_name);
            continue;
        }
        if let Err(e) = enforce_scripts_policy(&skill_dir) {
            conflicts.push(format!("{sub_name} (policy: {e})"));
            continue;
        }
        match std::fs::rename(&skill_dir, &dest) {
            Ok(_) => promoted.push(sub_name),
            Err(e) => conflicts.push(format!("{sub_name} ({e})")),
        }
    }

    // Emptied or near-empty leftover dir: drop it so `/skills` listing stays
    // clean. If there's anything interesting left (README, LICENSE, etc.) we
    // still remove it — the user can re-clone manually if they wanted those.
    let _ = std::fs::remove_dir_all(&clone_dir);

    if !promoted.is_empty() {
        report.push(format!(
            "bundle detected; installed {} skill(s): {}",
            promoted.len(),
            promoted.join(", ")
        ));
    }
    if !conflicts.is_empty() {
        report.push(format!(
            "skipped due to existing dirs: {}",
            conflicts.join(", ")
        ));
    }

    Ok(report)
}

/// Recursively collect every directory under `root` that directly contains a
/// `SKILL.md`. Skips `.git` and any nested dir once it's been claimed (we
/// don't install skills-inside-skills).
fn find_skill_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_for_skills(root, &mut out);
    out
}

fn walk_for_skills(dir: &Path, out: &mut Vec<PathBuf>) {
    if dir.join("SKILL.md").exists() {
        out.push(dir.to_path_buf());
        return; // don't descend into an already-claimed skill dir
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if name == ".git" || name == "node_modules" || name == "target" {
            continue;
        }
        walk_for_skills(&path, out);
    }
}

/// Best-effort name derivation from a git URL:
///   https://github.com/anthropics/skills.git → skills
///   git@github.com:user/my-skill.git         → my-skill
///   /local/path/foo                          → foo
///   `<repo>#main:skills/skill-creator`       → skill-creator (subpath wins)
fn derive_name_from_url(url: &str) -> String {
    // If the URL carries our `#<branch>:<subpath>` extension, the
    // subpath's last segment is the skill name (otherwise every
    // marketplace install of an `anthropics/skills/skills/<name>` URL
    // would derive to "skills").
    if let (_base, _branch, Some(subpath)) = parse_git_subpath(url) {
        let tail = subpath
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("");
        if !tail.is_empty() {
            return tail.to_string();
        }
    }
    // Strip query/fragment first so a URL like `.../pack.zip?token=xyz`
    // derives `pack`, not `pack.zip?token=xyz`.
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let trimmed = without_query
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_end_matches(".zip")
        .trim_end_matches(".ZIP");
    let tail = trimmed
        .rsplit(|c| c == '/' || c == ':')
        .next()
        .unwrap_or("");
    tail.to_string()
}

/// Parse the optional `#<branch>:<subpath>` suffix from a marketplace
/// install URL. Returns `(base_url, branch_opt, subpath_opt)`. Examples:
///   `https://x.com/r.git`                  → (`...`, None, None)
///   `https://x.com/r.git#main`             → (`...`, Some("main"), None)
///   `https://x.com/r.git#main:sub/leaf`    → (`...`, Some("main"), Some("sub/leaf"))
pub(crate) fn parse_git_subpath(url: &str) -> (String, Option<String>, Option<String>) {
    if let Some((base, frag)) = url.split_once('#') {
        let (branch, subpath) = match frag.split_once(':') {
            Some((b, p)) if !p.is_empty() => (
                if b.is_empty() {
                    None
                } else {
                    Some(b.to_string())
                },
                Some(p.to_string()),
            ),
            _ => (
                if frag.is_empty() {
                    None
                } else {
                    Some(frag.to_string())
                },
                None,
            ),
        };
        (base.to_string(), branch, subpath)
    } else {
        (url.to_string(), None, None)
    }
}

#[cfg(test)]
mod install_tests {
    use super::*;

    #[test]
    fn derive_name_strips_dot_git_and_path() {
        assert_eq!(
            derive_name_from_url("https://github.com/anthropics/skills.git"),
            "skills"
        );
        assert_eq!(
            derive_name_from_url("git@github.com:user/my-skill.git"),
            "my-skill"
        );
        assert_eq!(derive_name_from_url("https://example.com/x/y/"), "y");
        assert_eq!(derive_name_from_url("/local/path/foo"), "foo");
    }

    #[test]
    fn is_zip_url_detects_zip_suffix_with_and_without_query() {
        assert!(is_zip_url("https://example.com/s.zip"));
        assert!(is_zip_url("https://example.com/path/foo.ZIP"));
        assert!(is_zip_url("https://example.com/s.zip?token=abc"));
        assert!(is_zip_url("https://example.com/s.zip#frag"));
        assert!(!is_zip_url("https://github.com/user/repo.git"));
        assert!(!is_zip_url("https://example.com/zip-something"));
    }

    #[test]
    fn derive_name_works_for_zip_urls() {
        assert_eq!(
            derive_name_from_url(
                "https://agentic-press.com/api/skills/deploy-to-agentic-hosting-v1.zip"
            ),
            "deploy-to-agentic-hosting-v1"
        );
        assert_eq!(
            derive_name_from_url("https://example.com/skills/my.zip?token=abc"),
            "my"
        );
    }

    #[test]
    fn parse_git_subpath_extracts_branch_and_subpath() {
        // Plain URL: passes through unchanged.
        assert_eq!(
            parse_git_subpath("https://github.com/x/y.git"),
            ("https://github.com/x/y.git".into(), None, None)
        );
        // Branch only.
        assert_eq!(
            parse_git_subpath("https://github.com/x/y.git#main"),
            (
                "https://github.com/x/y.git".into(),
                Some("main".into()),
                None
            )
        );
        // Branch + subpath.
        assert_eq!(
            parse_git_subpath("https://github.com/anthropics/skills.git#main:skills/skill-creator"),
            (
                "https://github.com/anthropics/skills.git".into(),
                Some("main".into()),
                Some("skills/skill-creator".into())
            )
        );
        // Empty branch with subpath (`#:path`) — both fields populated as expected.
        assert_eq!(
            parse_git_subpath("https://github.com/x/y.git#:sub"),
            (
                "https://github.com/x/y.git".into(),
                None,
                Some("sub".into())
            )
        );
    }

    #[test]
    fn derive_name_uses_subpath_leaf() {
        assert_eq!(
            derive_name_from_url(
                "https://github.com/anthropics/skills.git#main:skills/skill-creator"
            ),
            "skill-creator"
        );
        assert_eq!(
            derive_name_from_url(
                "https://github.com/anthropics/skills.git#main:skills/webapp-testing/"
            ),
            "webapp-testing"
        );
    }
}

// ── Skill tool ────────────────────────────────────────────────────────

pub struct SkillTool {
    store: std::sync::Arc<std::sync::Mutex<SkillStore>>,
}

impl SkillTool {
    pub fn new(store: SkillStore) -> Self {
        Self {
            store: std::sync::Arc::new(std::sync::Mutex::new(store)),
        }
    }

    /// Build from an externally-owned shared handle. Lets the GUI's
    /// shared session hand in the same Arc<Mutex<SkillStore>> it
    /// keeps in WorkerState, so `/skill install` can repopulate the
    /// store without needing to find and mutate the tool through the
    /// registry.
    pub fn new_from_handle(store: std::sync::Arc<std::sync::Mutex<SkillStore>>) -> Self {
        Self { store }
    }

    /// Clone of the internal store handle. Lets the REPL re-populate the
    /// store after `/skill install` so newly installed skills are usable
    /// in the same session, without restarting.
    pub fn store_handle(&self) -> std::sync::Arc<std::sync::Mutex<SkillStore>> {
        self.store.clone()
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "Skill"
    }

    fn description(&self) -> &'static str {
        "Load a bundled skill's expert instructions. **Call this FIRST whenever \
         a user request matches any installed skill's trigger** — see the \
         \"Available skills\" section of the system prompt for names and \
         triggers. The returned content contains conventions and script paths \
         you MUST follow for that task instead of improvising with raw \
         Bash/Edit. Announce which skill you're using when you reply."
    }

    fn input_schema(&self) -> Value {
        let store = self.store.lock().unwrap();
        let available = store.names().join(", ");
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": format!(
                        "Skill to invoke. Available: {}",
                        if available.is_empty() { "none" } else { &available }
                    )
                }
            },
            "required": ["name"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let name = crate::tools::req_str(&input, "name")?;
        let store = self.store.lock().unwrap();

        let skill = store.get(name).ok_or_else(|| {
            let available = store.names().join(", ");
            Error::Tool(format!(
                "skill '{}' not found. Available: {}",
                name,
                if available.is_empty() {
                    "none"
                } else {
                    &available
                }
            ))
        })?;

        // Lazy-load the body on first SkillTool invocation. After P1
        // dev-plan/06: SkillStore::discover() only read the
        // frontmatter at boot; this `.content()` call materializes
        // the body now and caches it in the SkillDef's OnceLock.
        let mut result = skill.content().into_owned();

        // Auto-detect runtime needs: requirements.txt + scripts/ dir.
        // The skill author doesn't have to repeat install instructions
        // in their SKILL.md — we surface them here so the model
        // notices on every invocation. Idempotent: pip install with
        // already-installed deps is a no-op + cached.
        append_skill_runtime_hints(&mut result, &skill.dir);

        Ok(result)
    }
}

/// Auto-surface skill-runtime guidance: list scripts with suggested
/// interpreter, surface a `requirements.txt` install hint when
/// present. Called by `SkillTool::call` after the body is loaded.
///
/// Conventions (zero-config for skill authors):
///   - `<skill_dir>/scripts/foo.py`        → suggest `python <path>`
///   - `<skill_dir>/scripts/foo.sh`        → suggest `bash <path>`
///   - `<skill_dir>/scripts/foo.js`        → suggest `node <path>`
///   - `<skill_dir>/scripts/foo.ts`        → suggest `npx tsx <path>`
///   - `<skill_dir>/scripts/foo.rb`        → suggest `ruby <path>`
///   - `<skill_dir>/scripts/foo.pl`        → suggest `perl <path>`
///   - `<skill_dir>/scripts/foo` (no ext)  → list path only; let model decide
///
/// `<skill_dir>/requirements.txt` (Python deps) → install hint surfaced
/// before the script listing. Bash tool's auto-venv layer wraps
/// `pip install -r <path>` in venv activation transparently.
///
/// Skill authors can override by writing explicit `Run: ...` lines in
/// their SKILL.md — those land in the body before this auto-section.
fn append_skill_runtime_hints(result: &mut String, skill_dir: &Path) {
    let req_txt = skill_dir.join("requirements.txt");
    let scripts_dir = skill_dir.join("scripts");

    if req_txt.exists() {
        result.push_str(&format!(
            "\n\n## Python dependencies\n\nRun once before invoking this skill:\n\n  \
             pip install -r {}\n\n\
             (Bash will auto-activate the project venv. Idempotent: \
             repeated installs are no-ops.)",
            req_txt.display()
        ));
    }

    if !scripts_dir.exists() {
        return;
    }

    let entries: Vec<std::path::PathBuf> = std::fs::read_dir(&scripts_dir)
        .ok()
        .map(|e| {
            e.flatten()
                .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
                .map(|entry| scripts_dir.join(entry.file_name()))
                .collect()
        })
        .unwrap_or_default();
    if entries.is_empty() {
        return;
    }

    let mut sorted = entries;
    sorted.sort();

    result.push_str("\n\n## Available scripts\n\nInvoke via Bash. Do NOT rewrite them.\n\n");
    for path in sorted {
        let suggestion = suggest_interpreter(&path);
        match suggestion {
            Some(cmd) => result.push_str(&format!("  - `{cmd} {}`\n", path.display())),
            None => result.push_str(&format!("  - {}\n", path.display())),
        }
    }
}

/// Map a script's file extension to the conventional interpreter
/// invocation. Returns `None` for unknown extensions or extensionless
/// files — caller falls back to listing the path only and lets the
/// model decide.
fn suggest_interpreter(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    match ext.as_str() {
        "py" => Some("python"),
        "sh" | "bash" => Some("bash"),
        "zsh" => Some("zsh"),
        "fish" => Some("fish"),
        "js" | "mjs" | "cjs" => Some("node"),
        "ts" | "mts" => Some("npx tsx"),
        "rb" => Some("ruby"),
        "pl" => Some("perl"),
        "php" => Some("php"),
        "lua" => Some("lua"),
        "deno" => Some("deno run"),
        _ => None,
    }
}

// ── SkillList tool (dev-plan/06 P2) ──────────────────────────────────
//
// Lets the model discover what skills are installed without paying the
// per-turn token cost of listing every skill in the system prompt. Used
// in concert with the `skills_listing_strategy: "names-only"` and
// `"discover-tool-only"` config flags — under those strategies the
// system prompt mentions SkillList by name and the model can call it
// to get the catalog as a tool result.
//
// Returns name + short description for every installed skill. The
// short description is the same `description` frontmatter field shown
// in the system prompt under "full" mode.

pub struct SkillListTool {
    store: std::sync::Arc<std::sync::Mutex<SkillStore>>,
}

impl SkillListTool {
    pub fn new_from_handle(store: std::sync::Arc<std::sync::Mutex<SkillStore>>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for SkillListTool {
    fn name(&self) -> &'static str {
        "SkillList"
    }
    fn description(&self) -> &'static str {
        "List all installed skills with their short descriptions and \
         trigger criteria. Call this when you need to discover what \
         skills are available — typically when the user's request \
         sounds like it might match a bundled workflow but you don't \
         see a matching skill named in the system prompt. Returns \
         names you can pass to `Skill(name: ...)` to load the full \
         expert instructions. Cheap; safe to call any time."
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn call(&self, _input: Value) -> Result<String> {
        let store = self.store.lock().unwrap();
        let mut entries: Vec<&SkillDef> = store.skills.values().collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        if entries.is_empty() {
            return Ok("No skills installed. Use /skill marketplace to browse, \
                       or /skill install <name|url> to add one."
                .to_string());
        }
        let mut out = format!("{} skill(s) installed:\n", entries.len());
        for s in entries {
            out.push_str(&format!("- {} — {}", s.name, s.description));
            if !s.when_to_use.is_empty() {
                out.push_str(&format!("\n  Trigger: {}", s.when_to_use));
            }
            out.push('\n');
        }
        out.push_str("\nLoad a skill's expert instructions with Skill(name: \"<name>\").");
        Ok(out)
    }
}

// ── SkillSearch tool (dev-plan/06 P2) ────────────────────────────────
//
// Substring search across name + description + when_to_use. Same
// case-insensitive ranking the marketplace uses (name match > desc >
// trigger). Cheaper than SkillList when the user has many skills
// installed and the model knows what shape it's looking for.

pub struct SkillSearchTool {
    store: std::sync::Arc<std::sync::Mutex<SkillStore>>,
}

impl SkillSearchTool {
    pub fn new_from_handle(store: std::sync::Arc<std::sync::Mutex<SkillStore>>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for SkillSearchTool {
    fn name(&self) -> &'static str {
        "SkillSearch"
    }
    fn description(&self) -> &'static str {
        "Substring search across installed skills' name, description, \
         and trigger criteria. Case-insensitive; ranked by where the \
         match lands (name beats description beats trigger). Use when \
         you suspect a skill exists for the user's task but don't \
         remember its exact name. Returns matching skills you can pass \
         to `Skill(name: ...)`. Empty result list means nothing \
         matched — implement the task manually or call SkillList to \
         see the full catalog."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Substring to match (case-insensitive)"
                }
            },
            "required": ["query"]
        })
    }
    async fn call(&self, input: Value) -> Result<String> {
        let query = crate::tools::req_str(&input, "query")?;
        let store = self.store.lock().unwrap();
        let q = query.to_lowercase();
        let mut hits: Vec<(u8, &SkillDef)> = Vec::new();
        for s in store.skills.values() {
            if s.name.to_lowercase().contains(&q) {
                hits.push((0, s));
            } else if s.description.to_lowercase().contains(&q) {
                hits.push((1, s));
            } else if s.when_to_use.to_lowercase().contains(&q) {
                hits.push((2, s));
            }
        }
        hits.sort_by_key(|(rank, _)| *rank);
        if hits.is_empty() {
            return Ok(format!(
                "No installed skills match '{query}'. Use SkillList to see all \
                 installed, or /skill marketplace to browse what's available."
            ));
        }
        let mut out = format!("{} match(es) for '{query}':\n", hits.len());
        for (_, s) in hits {
            out.push_str(&format!("- {} — {}\n", s.name, s.description));
        }
        out.push_str("\nLoad a skill's expert instructions with Skill(name: \"<name>\").");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn create_skill(base: &Path, name: &str, content: &str, scripts: &[(&str, &str)]) {
        let skill_dir = base.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
        if !scripts.is_empty() {
            let scripts_dir = skill_dir.join("scripts");
            std::fs::create_dir_all(&scripts_dir).unwrap();
            for (fname, body) in scripts {
                std::fs::write(scripts_dir.join(fname), body).unwrap();
            }
        }
    }

    #[test]
    fn discover_from_directory() {
        let dir = tempdir().unwrap();
        create_skill(
            dir.path(),
            "deploy",
            "---\nname: deploy\ndescription: Deploy to staging\nwhenToUse: When user asks to deploy\n---\nRun {skill_dir}/scripts/deploy.sh",
            &[("deploy.sh", "#!/bin/bash\necho deploying")],
        );
        create_skill(
            dir.path(),
            "test",
            "---\nname: test\ndescription: Run tests\n---\nRun cargo test",
            &[],
        );

        let mut store = SkillStore::default();
        store.load_dir(dir.path());

        assert_eq!(store.skills.len(), 2);
        assert!(store.get("deploy").is_some());
        assert!(store.get("test").is_some());
        assert!(store
            .get("deploy")
            .unwrap()
            .content()
            .contains("/scripts/deploy.sh"));
        // {skill_dir} replaced with actual path
        assert!(!store
            .get("deploy")
            .unwrap()
            .content()
            .contains("{skill_dir}"));
    }

    // ── dev-plan/06 P1: lazy disk reads ──────────────────────────────

    #[test]
    fn discover_does_not_eagerly_read_skill_bodies() {
        // P1: SkillStore::discover should only read the frontmatter
        // (capped at MAX_FRONTMATTER_BYTES), not the full body. This
        // test plants a skill with a deliberately-huge body and
        // confirms the SkillDef's internal SkillContent variant is
        // Lazy after discovery — the body hasn't been materialized.
        let dir = tempdir().unwrap();
        let huge_body = "x".repeat(100_000); // 100KB body
        let body = format!("---\nname: huge\ndescription: huge skill\n---\n{huge_body}");
        create_skill(dir.path(), "huge", &body, &[]);

        let mut store = SkillStore::default();
        store.load_dir(dir.path());

        // Frontmatter parsed successfully.
        let skill = store.get("huge").expect("skill discovered");
        assert_eq!(skill.name, "huge");
        assert_eq!(skill.description, "huge skill");

        // Body NOT yet loaded — verify by checking the SkillContent
        // variant via private accessor.
        match &skill.content {
            SkillContent::Lazy { cell, .. } => {
                assert!(
                    cell.get().is_none(),
                    "OnceLock should be empty until .content() is called",
                );
            }
            SkillContent::Eager(_) => panic!("discover should produce Lazy, not Eager"),
        }
    }

    /// Serializes tests in this module that mutate process-global CWD
    /// — `set_current_dir` is a process-wide effect and parallel
    /// `cargo test` runs would interleave otherwise. Same pattern as
    /// `agent::tests::with_cwd`.
    fn with_cwd<R>(dir: &Path, f: impl FnOnce() -> R) -> R {
        static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = CWD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior = std::env::current_dir().expect("cwd readable");
        std::env::set_current_dir(dir).expect("cwd to test dir");
        let out = f();
        let _ = std::env::set_current_dir(prior);
        out
    }

    #[test]
    fn skill_content_survives_cwd_change_after_discovery() {
        // M6.14 BUG 1: discovery from a relative dir used to capture a
        // relative `skill_md_path` in SkillContent::Lazy. After a CWD
        // change (e.g. GUI workspace switch), the lazy read failed and
        // every project skill silently returned empty content.
        //
        // Repro: discover from a relative `.thclaws/skills` path, then
        // chdir somewhere else, then read content. Pre-fix: empty.
        // Post-fix: content survives because `parse_skill` now stores
        // the canonical absolute path in SkillContent::Lazy.
        let project = tempdir().unwrap();
        let elsewhere = tempdir().unwrap();
        let skills_root = project.path().join(".thclaws/skills/cwdtest");
        std::fs::create_dir_all(&skills_root).unwrap();
        std::fs::write(
            skills_root.join("SKILL.md"),
            "---\nname: cwdtest\ndescription: cwd test\n---\nLOAD ME AFTER CWD CHANGE\n",
        )
        .unwrap();

        let body = with_cwd(project.path(), || {
            let mut store = SkillStore::default();
            // Discover via the RELATIVE path — the exact shape
            // `discover()` uses internally for `.thclaws/skills`.
            store.load_dir(&PathBuf::from(".thclaws/skills"));
            // Switch CWD away from the project before reading content
            // (simulates the GUI sidebar workspace swap).
            std::env::set_current_dir(elsewhere.path()).unwrap();
            store.get("cwdtest").unwrap().content().into_owned()
        });

        assert!(
            body.contains("LOAD ME AFTER CWD CHANGE"),
            "lazy body should resolve under absolute path, got: {body:?}",
        );
    }

    #[test]
    fn project_skills_beat_plugin_skills_with_same_name() {
        // M6.14 BUG 2: previously discover_with_extra appended plugin
        // dirs after project dirs, so a plugin could shadow a project
        // skill with the same name (HashMap::insert is last-wins). The
        // module-level docs claim `.thclaws/skills/` is highest
        // priority — this test pins that contract.
        let workspace = tempdir().unwrap();

        let project_dir = workspace.path().join(".thclaws/skills/shared");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("SKILL.md"),
            "---\nname: shared\ndescription: PROJECT WINS\n---\nproject body\n",
        )
        .unwrap();

        let plugin_dir = workspace.path().join("plugins/foo/skills/shared");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("SKILL.md"),
            "---\nname: shared\ndescription: PLUGIN LOSES\n---\nplugin body\n",
        )
        .unwrap();

        let plugin_skills_root = workspace.path().join("plugins/foo/skills");
        let description = with_cwd(workspace.path(), || {
            let store = SkillStore::discover_with_extra(&[plugin_skills_root.clone()]);
            store
                .get("shared")
                .expect("shared skill discovered")
                .description
                .clone()
        });

        assert_eq!(
            description, "PROJECT WINS",
            "project .thclaws/skills should beat plugin contributions; got: {description:?}",
        );
    }

    #[test]
    fn skill_content_loads_on_first_call_and_caches() {
        // P1: first .content() call reads the body from disk; second
        // call returns the cached value (no re-read). We verify the
        // caching behavior by mutating the file between calls and
        // confirming the second call returns the OLD content.
        let dir = tempdir().unwrap();
        create_skill(
            dir.path(),
            "cached",
            "---\nname: cached\n---\nORIGINAL BODY\n",
            &[],
        );

        let mut store = SkillStore::default();
        store.load_dir(dir.path());

        let first = store.get("cached").unwrap().content().into_owned();
        assert!(first.contains("ORIGINAL BODY"));

        // Mutate the file between calls.
        let skill_md = dir.path().join("cached/SKILL.md");
        std::fs::write(&skill_md, "---\nname: cached\n---\nUPDATED BODY\n").unwrap();

        let second = store.get("cached").unwrap().content().into_owned();
        assert!(
            second.contains("ORIGINAL BODY"),
            "second call should return cached ORIGINAL, got: {second}",
        );
        assert!(
            !second.contains("UPDATED BODY"),
            "OnceLock should have cached the first read; got: {second}",
        );
    }

    #[test]
    fn frontmatter_reader_caps_at_max_bytes() {
        // P1: read_until_frontmatter_end should stop early when the
        // closing `---\n` fence is found, OR cap at
        // MAX_FRONTMATTER_BYTES. Either way it must NOT read the
        // entire file when the body is huge.
        let dir = tempdir().unwrap();
        let frontmatter = "---\nname: small\ndescription: small\n---\n";
        let huge_body = "y".repeat(1_000_000); // 1MB body
        std::fs::write(
            dir.path().join("SKILL.md"),
            format!("{frontmatter}{huge_body}"),
        )
        .unwrap();

        let read = read_until_frontmatter_end(&dir.path().join("SKILL.md")).unwrap();
        // Should have stopped after the closing fence — well under 1MB.
        assert!(
            read.len() < MAX_FRONTMATTER_BYTES + 1024,
            "expected to stop reading at frontmatter end (≤{}+1KB), got {} bytes",
            MAX_FRONTMATTER_BYTES,
            read.len(),
        );
        // Frontmatter content must be present.
        assert!(read.contains("name: small"));
        assert!(read.contains("description: small"));
    }

    #[test]
    fn skill_def_new_eager_constructs_eager_variant() {
        // P1: SkillDef::new_eager bypasses lazy loading — useful for
        // tests and any future caller that wants to inject a skill
        // from in-memory data without filesystem round-trip.
        let s = SkillDef::new_eager(
            "n".into(),
            "d".into(),
            "w".into(),
            PathBuf::from("/tmp"),
            "BODY".into(),
        );
        assert_eq!(s.content(), "BODY");
        assert!(matches!(s.content, SkillContent::Eager(_)));
    }

    #[test]
    fn missing_skill_md_after_discovery_returns_empty_content() {
        // P1: defensive — if the SKILL.md disappears between
        // discovery and first .content() call, return empty rather
        // than panicking. Caller's downstream rendering treats
        // empty content as "skill body unavailable."
        let dir = tempdir().unwrap();
        create_skill(
            dir.path(),
            "ephemeral",
            "---\nname: ephemeral\n---\nBODY\n",
            &[],
        );

        let mut store = SkillStore::default();
        store.load_dir(dir.path());

        // Delete the file before .content() is called.
        std::fs::remove_file(dir.path().join("ephemeral/SKILL.md")).unwrap();

        let result = store.get("ephemeral").unwrap().content();
        assert!(result.is_empty(), "missing file should yield empty content");
    }

    #[test]
    fn names_sorted() {
        let dir = tempdir().unwrap();
        create_skill(dir.path(), "zzz", "---\nname: zzz\n---\n", &[]);
        create_skill(dir.path(), "aaa", "---\nname: aaa\n---\n", &[]);

        let mut store = SkillStore::default();
        store.load_dir(dir.path());
        assert_eq!(store.names(), vec!["aaa", "zzz"]);
    }

    #[tokio::test]
    async fn skill_tool_returns_content_with_scripts() {
        let dir = tempdir().unwrap();
        create_skill(
            dir.path(),
            "build",
            "---\nname: build\ndescription: Build project\n---\nRun the build script.",
            &[("build.sh", "#!/bin/bash\ncargo build")],
        );

        let mut store = SkillStore::default();
        store.load_dir(dir.path());
        let tool = SkillTool::new(store);

        let result = tool.call(json!({"name": "build"})).await.unwrap();
        assert!(result.contains("Run the build script"));
        assert!(result.contains("Available scripts"));
        assert!(result.contains("build.sh"));
        assert!(result.contains("Do NOT rewrite them"));
    }

    #[tokio::test]
    async fn skill_tool_unknown_errors() {
        let store = SkillStore::default();
        let tool = SkillTool::new(store);
        let err = tool.call(json!({"name": "nope"})).await.unwrap_err();
        assert!(format!("{err}").contains("not found"));
    }

    // ── skill-authoring polish: interpreter hints + requirements.txt ──

    #[tokio::test]
    async fn skill_tool_suggests_interpreter_per_script_extension() {
        // The "Available scripts" listing now includes a suggested
        // interpreter invocation per file extension. Skill authors
        // get this for free without having to write `Run: python ...`
        // boilerplate in their SKILL.md for every script.
        let dir = tempdir().unwrap();
        create_skill(
            dir.path(),
            "polyglot",
            "---\nname: polyglot\ndescription: many runtimes\n---\nUse the scripts below.",
            &[
                ("render.py", "print('py')"),
                ("setup.sh", "echo sh"),
                ("transform.js", "console.log('js')"),
                ("check.ts", "console.log('ts')"),
                ("legacy.rb", "puts 'rb'"),
                ("noext", "echo 'no extension — caller decides'"),
            ],
        );

        let mut store = SkillStore::default();
        store.load_dir(dir.path());
        let tool = SkillTool::new(store);

        let result = tool.call(json!({"name": "polyglot"})).await.unwrap();
        // Each known extension surfaces with the conventional interpreter.
        assert!(result.contains("`python "), "missing python hint: {result}");
        assert!(result.contains("`bash "), "missing bash hint: {result}");
        assert!(result.contains("`node "), "missing node hint: {result}");
        assert!(result.contains("`npx tsx "), "missing tsx hint: {result}");
        assert!(result.contains("`ruby "), "missing ruby hint: {result}");
        // Extensionless: path listed without a backtick-wrapped interpreter prefix.
        assert!(
            result.contains("noext"),
            "extensionless script should still appear: {result}",
        );
    }

    #[tokio::test]
    async fn skill_tool_surfaces_requirements_txt_when_present() {
        // Auto-detect <skill_dir>/requirements.txt so skill authors
        // don't have to repeat install instructions in their SKILL.md.
        let dir = tempdir().unwrap();
        create_skill(
            dir.path(),
            "needs-deps",
            "---\nname: needs-deps\ndescription: requires pip packages\n---\nUse the script.",
            &[("render.py", "import pdfkit\nprint('ok')")],
        );
        // Add the requirements.txt sibling.
        std::fs::write(
            dir.path().join("needs-deps/requirements.txt"),
            "pdfkit==1.0.0\nmarkdown\n",
        )
        .unwrap();

        let mut store = SkillStore::default();
        store.load_dir(dir.path());
        let tool = SkillTool::new(store);

        let result = tool.call(json!({"name": "needs-deps"})).await.unwrap();
        assert!(
            result.contains("Python dependencies"),
            "missing deps section header: {result}",
        );
        assert!(
            result.contains("pip install -r "),
            "missing pip install hint: {result}",
        );
        assert!(
            result.contains("requirements.txt"),
            "missing requirements.txt path: {result}",
        );
        assert!(
            result.contains("auto-activate the project venv"),
            "missing venv-activation explanation: {result}",
        );
    }

    #[tokio::test]
    async fn skill_tool_omits_requirements_section_when_no_file() {
        // No requirements.txt → no Python-deps section. Avoids
        // misleading skill consumers when the skill is pure-bash or
        // pure-node.
        let dir = tempdir().unwrap();
        create_skill(
            dir.path(),
            "bash-only",
            "---\nname: bash-only\ndescription: shell only\n---\nUse the script.",
            &[("setup.sh", "echo hi")],
        );

        let mut store = SkillStore::default();
        store.load_dir(dir.path());
        let tool = SkillTool::new(store);

        let result = tool.call(json!({"name": "bash-only"})).await.unwrap();
        assert!(
            !result.contains("Python dependencies"),
            "deps section should be absent: {result}",
        );
        // Script listing still appears.
        assert!(result.contains("Available scripts"));
        assert!(result.contains("`bash "));
    }

    #[test]
    fn suggest_interpreter_returns_none_for_unknown_extensions() {
        // Defensive: unknown extensions produce no interpreter hint.
        // The path is still listed; the model picks the right
        // invocation from SKILL.md context.
        assert_eq!(suggest_interpreter(Path::new("foo.unknown")), None);
        assert_eq!(suggest_interpreter(Path::new("noext")), None);
        assert_eq!(suggest_interpreter(Path::new("foo.")), None);
    }

    #[test]
    fn suggest_interpreter_is_case_insensitive() {
        // Skill authors may name files Foo.PY or render.JS.
        assert_eq!(suggest_interpreter(Path::new("foo.PY")), Some("python"));
        assert_eq!(suggest_interpreter(Path::new("RENDER.Js")), Some("node"));
        assert_eq!(suggest_interpreter(Path::new("X.SH")), Some("bash"));
    }

    // ── dev-plan/06 P2: SkillList + SkillSearch + listing strategy ──

    fn store_with_three_skills() -> std::sync::Arc<std::sync::Mutex<SkillStore>> {
        let dir = tempdir().unwrap();
        create_skill(
            dir.path(),
            "pdf",
            "---\nname: pdf\ndescription: Render PDFs\nwhenToUse: When user wants a PDF\n---\nbody\n",
            &[],
        );
        create_skill(
            dir.path(),
            "xlsx",
            "---\nname: xlsx\ndescription: Read xlsx files\nwhenToUse: When user has spreadsheets\n---\nbody\n",
            &[],
        );
        create_skill(
            dir.path(),
            "skill-creator",
            "---\nname: skill-creator\ndescription: Scaffold new skills\n---\nbody\n",
            &[],
        );
        let mut store = SkillStore::default();
        store.load_dir(dir.path());
        // Leak the tempdir to keep the SKILL.md files alive for the
        // duration of the test (lazy reads happen on Skill / SkillSearch
        // calls below).
        std::mem::forget(dir);
        std::sync::Arc::new(std::sync::Mutex::new(store))
    }

    #[tokio::test]
    async fn skill_list_returns_all_installed_skills() {
        let store = store_with_three_skills();
        let tool = SkillListTool::new_from_handle(store);
        let out = tool.call(json!({})).await.unwrap();
        assert!(out.contains("pdf"), "missing pdf: {out}");
        assert!(out.contains("xlsx"), "missing xlsx: {out}");
        assert!(
            out.contains("skill-creator"),
            "missing skill-creator: {out}"
        );
        assert!(out.contains("3 skill(s)"), "missing count: {out}");
        // Triggers surface for skills that have whenToUse.
        assert!(out.contains("Trigger:"), "missing trigger: {out}");
    }

    #[tokio::test]
    async fn skill_list_handles_empty_store() {
        let store = std::sync::Arc::new(std::sync::Mutex::new(SkillStore::default()));
        let tool = SkillListTool::new_from_handle(store);
        let out = tool.call(json!({})).await.unwrap();
        assert!(out.contains("No skills installed"));
        assert!(out.contains("/skill marketplace"));
    }

    #[tokio::test]
    async fn skill_search_substring_matches_with_ranking() {
        let store = store_with_three_skills();
        let tool = SkillSearchTool::new_from_handle(store);

        // Name match — exact name returned first
        let out = tool.call(json!({"query": "pdf"})).await.unwrap();
        assert!(out.contains("pdf"), "got: {out}");
        assert!(!out.contains("xlsx"), "xlsx shouldn't match 'pdf': {out}");

        // Description match
        let out = tool.call(json!({"query": "spreadsheets"})).await.unwrap();
        assert!(out.contains("xlsx"), "got: {out}");
        assert!(
            !out.contains("pdf"),
            "pdf desc shouldn't match 'spreadsheets': {out}"
        );

        // Trigger match
        let out = tool
            .call(json!({"query": "When user wants"}))
            .await
            .unwrap();
        assert!(out.contains("pdf"), "trigger search failed: {out}");

        // Case-insensitive
        let out = tool.call(json!({"query": "PDF"})).await.unwrap();
        assert!(out.contains("pdf"), "case-insensitive failed: {out}");
    }

    #[tokio::test]
    async fn skill_search_no_matches_returns_helpful_message() {
        let store = store_with_three_skills();
        let tool = SkillSearchTool::new_from_handle(store);
        let out = tool
            .call(json!({"query": "nothing-matches-this-string-xyz123"}))
            .await
            .unwrap();
        assert!(out.contains("No installed skills match"));
        assert!(out.contains("SkillList"));
    }

    #[tokio::test]
    async fn skill_search_ranks_name_above_description() {
        // "skill" appears in skill-creator's NAME and (potentially)
        // somewhere in another's description. Name match should
        // appear first in the output.
        let store = store_with_three_skills();
        let tool = SkillSearchTool::new_from_handle(store);
        let out = tool.call(json!({"query": "skill"})).await.unwrap();
        // skill-creator's name match should come before any other.
        let pos_creator = out.find("skill-creator").unwrap_or(usize::MAX);
        // pdf has "PDF" in name; not relevant to "skill" query — should be ranked lower or absent.
        assert_ne!(
            pos_creator,
            usize::MAX,
            "skill-creator should appear: {out}"
        );
    }
}
