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
//! 3. `.claude/skills/` (project Claude Code)
//! 4. `.thclaws/skills/` (project thClaws — highest priority)
//!
//! Plus any plugin-contributed skill dirs (see [`crate::plugins`]).
//!
//! The `Skill` tool returns the SKILL.md content with `{skill_dir}` replaced
//! by the absolute path to the skill directory, so script paths resolve.

use crate::error::{Error, Result};
use crate::tools::Tool;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub when_to_use: String,
    pub dir: PathBuf,
    pub content: String,
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
    pub fn discover_with_extra(extra: &[PathBuf]) -> Self {
        let mut store = Self::default();
        let mut dirs = Self::skill_dirs();
        for p in extra {
            dirs.push(p.clone());
        }
        for dir in dirs {
            if dir.exists() {
                store.load_dir(&dir);
            }
        }
        store
    }

    /// Skill directories in load order (later overrides earlier by name).
    fn skill_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(home) = crate::util::home_dir() {
            dirs.push(home.join(".claude/skills")); // user Claude Code
            dirs.push(home.join(".config/thclaws/skills")); // user thClaws
        }
        dirs.push(PathBuf::from(".claude/skills")); // project Claude Code
        dirs.push(PathBuf::from(".thclaws/skills")); // project thClaws (highest priority)
        dirs
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

    fn parse_skill(dir: &Path, skill_md: &Path) -> Option<SkillDef> {
        let raw = std::fs::read_to_string(skill_md).ok()?;
        let (frontmatter, body) = crate::memory::parse_frontmatter(&raw);

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

        // Replace {skill_dir} placeholder with actual absolute path.
        let abs_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let content = body.replace("{skill_dir}", &abs_dir.to_string_lossy());

        Some(SkillDef {
            name,
            description,
            when_to_use,
            dir: abs_dir,
            content,
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

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
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

        // List scripts if the scripts/ dir exists.
        let scripts_dir = skill.dir.join("scripts");
        let mut result = skill.content.clone();
        if scripts_dir.exists() {
            let scripts: Vec<String> = std::fs::read_dir(&scripts_dir)
                .ok()
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                        .map(|e| format!("  - {}", scripts_dir.join(e.file_name()).display()))
                        .collect()
                })
                .unwrap_or_default();
            if !scripts.is_empty() {
                result.push_str("\n\n## Available scripts\n");
                result.push_str(&scripts.join("\n"));
                result.push_str("\n\nUse Bash to execute these scripts. Do NOT rewrite them.");
            }
        }

        Ok(result)
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
            .content
            .contains("/scripts/deploy.sh"));
        // {skill_dir} replaced with actual path
        assert!(!store.get("deploy").unwrap().content.contains("{skill_dir}"));
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
}
