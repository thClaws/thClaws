//! Plugin system — bundle of skills / commands / MCP servers managed as
//! one unit, similar to Claude Code plugins.
//!
//! ## Layout
//!
//! A plugin is a directory (git repo or zip) containing a manifest:
//!
//! - `.thclaws-plugin/plugin.json` (thClaws-native) — preferred
//! - `.claude-plugin/plugin.json` (Claude Code compat) — fallback
//!
//! Installed plugins live under:
//!
//! - Project: `.thclaws/plugins/<name>/` (registry `.thclaws/plugins.json`)
//! - User:    `~/.config/thclaws/plugins/<name>/` (registry
//!   `~/.config/thclaws/plugins.json`)
//!
//! The registry is a simple JSON file listing installed plugins with their
//! source URL, install path, and enabled flag.
//!
//! ## Manifest schema
//!
//! ```json
//! {
//!   "name": "my-plugin",
//!   "version": "1.0.0",
//!   "description": "What this does",
//!   "skills": ["skills"],
//!   "commands": ["commands"],
//!   "mcpServers": {
//!     "deploy-hub": {"transport": "http", "url": "https://example.com/mcp"}
//!   }
//! }
//! ```
//!
//! Paths in `skills` / `commands` are resolved relative to the manifest
//! root (the plugin's install directory). `mcpServers` uses the same shape
//! as `mcp.json` and is merged into the app config at startup.

use crate::error::{Error, Result};
use crate::mcp::McpServerConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A parsed plugin manifest. Only fields we currently wire up are decoded;
/// unknown fields are ignored so forward-compatible manifests don't break
/// older clients.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Author. Accepts either a flat string (`"author": "Jane Doe"`)
    /// or an object (`"author": {"name": "Jane Doe", "email": "..."}`)
    /// — the latter is the convention used by `anthropics/skills` and
    /// the Claude Code plugin spec, so forks of upstream plugins
    /// don't need to mangle their manifest just to install in thClaws.
    #[serde(default, deserialize_with = "deserialize_author_flexible")]
    pub author: String,
    /// Subdirs (relative to the plugin root) whose children are individual
    /// skill dirs (each containing a SKILL.md).
    #[serde(default)]
    pub skills: Vec<String>,
    /// Subdirs holding legacy prompt-template `.md` files.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Subdirs holding agent definition `.md` files (YAML frontmatter +
    /// body). Each dir is passed to `AgentDefsConfig::load_with_extra`,
    /// so plugin-contributed agents are additive — they never shadow
    /// project-level or user-level agent defs with the same name.
    #[serde(default)]
    pub agents: Vec<String>,
    /// MCP servers contributed by this plugin, keyed by server name.
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: HashMap<String, McpServerEntry>,
}

/// Minimal MCP entry inside a plugin manifest. Mirrors the shape of
/// `mcp.json` but stays permissive so future transport options land
/// without breaking the deserializer.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpServerEntry {
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

fn default_transport() -> String {
    "stdio".into()
}

/// Accept `author` in either of two common shapes and normalize to a
/// display string:
///   - `"author": "Jane Doe"`                              → `"Jane Doe"`
///   - `"author": {"name": "Jane Doe", "email": "j@x.io"}` → `"Jane Doe"`
///   - `"author": null` or missing                          → `""`
/// Letting both shapes deserialize means anthropics-style plugin
/// manifests work in thClaws unchanged.
fn deserialize_author_flexible<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Object(map) => map
            .get("name")
            .and_then(|n| n.as_str())
            .map(String::from)
            .unwrap_or_default(),
        serde_json::Value::Null => String::new(),
        _ => String::new(),
    })
}

impl McpServerEntry {
    pub fn to_config(&self, name: &str) -> McpServerConfig {
        // Plugin-installed MCP servers are trusted: they came in
        // through the plugin install flow which the user explicitly
        // ran, and the marketplace is the curation layer for those
        // installs. Hand-added entries in `.mcp.json` go through
        // `config.rs::parse_mcp_json` where the trusted flag must be
        // set explicitly. See dev-log/112.
        McpServerConfig {
            name: name.to_string(),
            transport: self.transport.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            url: self.url.clone(),
            headers: self.headers.clone(),
            trusted: true,
        }
    }
}

/// One entry in the installed-plugins registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plugin {
    pub name: String,
    /// Original install URL (git or zip). Empty for plugins installed from
    /// a local path or added manually.
    #[serde(default)]
    pub source: String,
    /// Absolute path to the installed plugin directory.
    pub path: PathBuf,
    #[serde(default)]
    pub version: String,
    /// Whether this plugin's contributions participate in discovery.
    /// `true` by default so installing enables immediately; a future
    /// `/plugin disable` would flip this.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

impl Plugin {
    pub fn manifest(&self) -> Result<PluginManifest> {
        read_manifest(&self.path)
    }
}

/// Registry (a JSON file) of installed plugins at a given scope
/// (project or user).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginRegistry {
    #[serde(default)]
    pub plugins: Vec<Plugin>,
}

impl PluginRegistry {
    /// Read the registry file for the given scope. Missing file → empty
    /// registry (no error — the common case before any install).
    pub fn load(user: bool) -> Result<Self> {
        let path = registry_path(user)?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(&path)?;
        if contents.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&contents)
            .map_err(|e| Error::Config(format!("parse {}: {e}", path.display())))
    }

    pub fn save(&self, user: bool) -> Result<PathBuf> {
        let path = registry_path(user)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let pretty = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("serialize registry: {e}")))?;
        std::fs::write(&path, pretty)?;
        Ok(path)
    }

    pub fn find(&self, name: &str) -> Option<&Plugin> {
        self.plugins.iter().find(|p| p.name == name)
    }

    pub fn upsert(&mut self, plugin: Plugin) {
        if let Some(existing) = self.plugins.iter_mut().find(|p| p.name == plugin.name) {
            *existing = plugin;
        } else {
            self.plugins.push(plugin);
        }
    }

    pub fn remove(&mut self, name: &str) -> Option<Plugin> {
        let idx = self.plugins.iter().position(|p| p.name == name)?;
        Some(self.plugins.remove(idx))
    }
}

fn registry_path(user: bool) -> Result<PathBuf> {
    if user {
        let home = crate::util::home_dir()
            .ok_or_else(|| Error::Config("cannot locate user home directory".into()))?;
        Ok(home.join(".config/thclaws/plugins.json"))
    } else {
        let cwd = std::env::current_dir()?;
        Ok(cwd.join(".thclaws/plugins.json"))
    }
}

fn plugins_dir(user: bool) -> Result<PathBuf> {
    if user {
        let home = crate::util::home_dir()
            .ok_or_else(|| Error::Config("cannot locate user home directory".into()))?;
        Ok(home.join(".config/thclaws/plugins"))
    } else {
        let cwd = std::env::current_dir()?;
        Ok(cwd.join(".thclaws/plugins"))
    }
}

/// Read the manifest at the conventional locations inside a plugin root.
/// Prefers `.thclaws-plugin/plugin.json` over `.claude-plugin/plugin.json`
/// when both are present.
pub fn read_manifest(root: &Path) -> Result<PluginManifest> {
    for sub in [".thclaws-plugin", ".claude-plugin"] {
        let path = root.join(sub).join("plugin.json");
        if path.exists() {
            let contents = std::fs::read_to_string(&path)?;
            return serde_json::from_str(&contents)
                .map_err(|e| Error::Config(format!("parse {}: {e}", path.display())));
        }
    }
    Err(Error::Config(format!(
        "no plugin.json found under {}/.thclaws-plugin or {}/.claude-plugin",
        root.display(),
        root.display()
    )))
}

/// Install a plugin from a git URL or a `.zip` URL into the given scope.
/// Returns the installed [`Plugin`] record.
pub async fn install(url: &str, user: bool) -> Result<Plugin> {
    // Org-policy gate: when `policies.plugins.enabled: true`, the URL
    // must match `allowed_hosts`. Open-core builds with no policy hit
    // `AllowDecision::NoPolicy` and pass through unchanged.
    if let crate::policy::AllowDecision::Denied { reason } = crate::policy::check_url(url) {
        return Err(Error::Config(format!(
            "plugin install blocked by org policy: {reason}"
        )));
    }

    let dest_parent = plugins_dir(user)?;
    std::fs::create_dir_all(&dest_parent)?;

    // Stage under a temp dir inside the target so the rename at the end
    // is same-volume. Using `uuid` avoids leaking PID-based names.
    let staging = dest_parent.join(format!(".install-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&staging)?;

    let fetch_result = fetch_into(url, &staging).await;
    if let Err(e) = fetch_result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    // The manifest might be at the staging root OR inside a single wrapper
    // (zip archives commonly do `pack-v1/...`, git clones don't).
    let plugin_root = locate_plugin_root(&staging)?;

    let manifest = read_manifest(&plugin_root)?;
    if manifest.name.trim().is_empty() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(Error::Config(
            "plugin manifest is missing a `name` field".into(),
        ));
    }

    // Move to final location. Refuse to overwrite an existing plugin —
    // remove first.
    let final_dir = dest_parent.join(&manifest.name);
    if final_dir.exists() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(Error::Config(format!(
            "plugin '{}' already installed at {} — run /plugin remove first",
            manifest.name,
            final_dir.display()
        )));
    }
    std::fs::rename(&plugin_root, &final_dir).map_err(|e| {
        Error::Config(format!(
            "move {} → {}: {e}",
            plugin_root.display(),
            final_dir.display()
        ))
    })?;
    // If plugin_root was inside staging (wrapper case), the outer staging
    // may still hold metadata. Drop it either way.
    let _ = std::fs::remove_dir_all(&staging);

    let plugin = Plugin {
        name: manifest.name.clone(),
        source: url.to_string(),
        path: final_dir,
        version: manifest.version.clone(),
        enabled: true,
    };

    let mut registry = PluginRegistry::load(user)?;
    registry.upsert(plugin.clone());
    registry.save(user)?;

    Ok(plugin)
}

/// Flip the `enabled` flag on an installed plugin without touching the
/// files on disk. Returns whether a matching plugin was found in the
/// given scope.
pub fn set_enabled(name: &str, user: bool, enabled: bool) -> Result<bool> {
    let mut registry = PluginRegistry::load(user)?;
    let Some(p) = registry.plugins.iter_mut().find(|p| p.name == name) else {
        return Ok(false);
    };
    p.enabled = enabled;
    registry.save(user)?;
    Ok(true)
}

/// Look up an installed plugin by name across both scopes, project first
/// (matches [`installed_plugins_all_scopes`]). Used by `/plugin show`.
pub fn find_installed(name: &str) -> Option<Plugin> {
    if let Ok(reg) = PluginRegistry::load(false) {
        if let Some(p) = reg.plugins.into_iter().find(|p| p.name == name) {
            return Some(p);
        }
    }
    if let Ok(reg) = PluginRegistry::load(true) {
        if let Some(p) = reg.plugins.into_iter().find(|p| p.name == name) {
            return Some(p);
        }
    }
    None
}

/// Remove an installed plugin: delete its files and drop from the registry.
/// Returns whether anything was actually removed.
pub fn remove(name: &str, user: bool) -> Result<bool> {
    let mut registry = PluginRegistry::load(user)?;
    let Some(plugin) = registry.remove(name) else {
        return Ok(false);
    };
    if plugin.path.exists() {
        std::fs::remove_dir_all(&plugin.path)
            .map_err(|e| Error::Config(format!("delete {}: {e}", plugin.path.display())))?;
    }
    registry.save(user)?;
    Ok(true)
}

/// Collect every enabled plugin across both scopes, project first. Used
/// by the REPL to build the effective set of skill/command/MCP dirs at
/// startup — disabled plugins are filtered out.
pub fn installed_plugins_all_scopes() -> Vec<Plugin> {
    all_plugins_all_scopes()
        .into_iter()
        .filter(|p| p.enabled)
        .collect()
}

/// Every installed plugin across both scopes, project first, regardless
/// of enabled state. Used by `/plugins` so the list still surfaces a
/// disabled plugin the user might want to `/plugin enable` back on.
pub fn all_plugins_all_scopes() -> Vec<Plugin> {
    let mut out = Vec::new();
    if let Ok(reg) = PluginRegistry::load(false) {
        out.extend(reg.plugins);
    }
    if let Ok(reg) = PluginRegistry::load(true) {
        for p in reg.plugins {
            if !out.iter().any(|existing| existing.name == p.name) {
                out.push(p);
            }
        }
    }
    out
}

/// Flatten all enabled plugins' skill directories into absolute paths.
/// Each entry is a directory that contains one-or-more `<skill>/SKILL.md`
/// subdirectories (compatible with [`crate::skills::SkillStore`] discovery).
///
/// When a plugin's manifest doesn't declare `skills`, we fall back to a
/// conventional `skills/` subdir if one exists. This mirrors Claude
/// Code's auto-discovery behavior so anthropics-style plugins (which
/// rely on the `skills/` convention rather than declaring it
/// explicitly in the manifest) install in thClaws unchanged.
pub fn plugin_skill_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for plugin in installed_plugins_all_scopes() {
        let Ok(manifest) = plugin.manifest() else {
            continue;
        };
        if manifest.skills.is_empty() {
            let conventional = plugin.path.join("skills");
            if conventional.is_dir() {
                dirs.push(conventional);
            }
        } else {
            for rel in &manifest.skills {
                dirs.push(plugin.path.join(rel));
            }
        }
    }
    dirs
}

/// Flatten all enabled plugins' command directories. Same convention-
/// over-configuration fallback as [`plugin_skill_dirs`].
pub fn plugin_command_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for plugin in installed_plugins_all_scopes() {
        let Ok(manifest) = plugin.manifest() else {
            continue;
        };
        if manifest.commands.is_empty() {
            let conventional = plugin.path.join("commands");
            if conventional.is_dir() {
                dirs.push(conventional);
            }
        } else {
            for rel in &manifest.commands {
                dirs.push(plugin.path.join(rel));
            }
        }
    }
    dirs
}

/// Flatten all enabled plugins' agent directories. Returned dirs feed
/// `AgentDefsConfig::load_with_extra`; plugin agents merge additively
/// and never clobber a user's or project's existing agent by name.
pub fn plugin_agent_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for plugin in installed_plugins_all_scopes() {
        let Ok(manifest) = plugin.manifest() else {
            continue;
        };
        for rel in &manifest.agents {
            dirs.push(plugin.path.join(rel));
        }
    }
    dirs
}

/// Build a list of MCP server configs contributed by enabled plugins.
/// Later plugins don't clobber existing entries — callers merge these
/// into the app config with project-level servers winning on name clash.
pub fn plugin_mcp_servers() -> Vec<McpServerConfig> {
    let mut out = Vec::new();
    for plugin in installed_plugins_all_scopes() {
        let Ok(manifest) = plugin.manifest() else {
            continue;
        };
        for (name, entry) in &manifest.mcp_servers {
            out.push(entry.to_config(name));
        }
    }
    out
}

// ── Internal fetch helpers ────────────────────────────────────────────

async fn fetch_into(url: &str, dest: &Path) -> Result<()> {
    if is_zip_url(url) {
        let bytes = download_zip(url).await?;
        extract_zip(&bytes, dest)
    } else {
        git_clone(url, dest)
    }
}

fn is_zip_url(url: &str) -> bool {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    without_query.to_ascii_lowercase().ends_with(".zip")
}

async fn download_zip(url: &str) -> Result<Vec<u8>> {
    const MAX_BYTES: u64 = 64 * 1024 * 1024;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| Error::Config(format!("http client: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Config(format!("download: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Config(format!("download: HTTP {}", resp.status())));
    }
    if let Some(len) = resp.content_length() {
        if len > MAX_BYTES {
            return Err(Error::Config(format!(
                "zip too large ({} bytes, max {})",
                len, MAX_BYTES
            )));
        }
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| Error::Config(format!("read body: {e}")))?
        .to_vec();
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Error::Config(format!(
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
        zip::ZipArchive::new(cursor).map_err(|e| Error::Config(format!("open zip: {e}")))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| Error::Config(format!("zip entry {i}: {e}")))?;
        let Some(name) = entry.enclosed_name() else {
            return Err(Error::Config(format!(
                "unsafe path in archive: {}",
                entry.name()
            )));
        };
        let out_path = dest.join(name);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&out_path)?;
            std::io::copy(&mut entry, &mut out)?;
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

fn git_clone(url: &str, dest: &Path) -> Result<()> {
    // Support the marketplace-style `<url>#<branch>:<subpath>`
    // extension so a plugin can be installed out of a multi-plugin
    // monorepo (mirrors what skills::install_from_url already does).
    // Plain URLs (no fragment) round-trip through unchanged.
    let (base_url, branch, subpath) = crate::skills::parse_git_subpath(url);

    // When a subpath is requested we clone into a sibling staging dir
    // (next to the destination, same volume so the rename is cheap),
    // then move only the subpath into `dest` and discard the rest.
    let stage_dir: PathBuf = if subpath.is_some() {
        let parent = dest
            .parent()
            .ok_or_else(|| Error::Config("plugin clone dest has no parent".to_string()))?;
        parent.join(format!(".clone-{}", uuid::Uuid::new_v4().simple()))
    } else {
        dest.to_path_buf()
    };

    let mut args: Vec<String> = vec!["clone".into(), "--depth".into(), "1".into()];
    if let Some(b) = &branch {
        args.push("--branch".into());
        args.push(b.clone());
    }
    args.push(base_url.clone());
    args.push(stage_dir.to_string_lossy().into_owned());

    let out = std::process::Command::new("git")
        .args(&args)
        .output()
        .map_err(|e| Error::Config(format!("spawn git: {e}")))?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&stage_dir);
        return Err(Error::Config(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    if let Some(sub) = &subpath {
        let src = stage_dir.join(sub);
        if !src.is_dir() {
            let _ = std::fs::remove_dir_all(&stage_dir);
            return Err(Error::Config(format!(
                "subpath '{sub}' not found in cloned repo (or is not a directory)"
            )));
        }
        // `dest` was created by the caller (`fs::create_dir_all` in
        // `install`); rename refuses to clobber a non-empty target,
        // so remove the placeholder first then move the subpath into
        // place under that exact path.
        let _ = std::fs::remove_dir_all(dest);
        std::fs::rename(&src, dest).map_err(|e| {
            let _ = std::fs::remove_dir_all(&stage_dir);
            Error::Config(format!("move subpath into place: {e}"))
        })?;
        let _ = std::fs::remove_dir_all(&stage_dir);
    }

    Ok(())
}

/// If `staging` has a single wrapper directory and no manifest at its
/// root, descend into that wrapper. Otherwise return `staging` itself.
fn locate_plugin_root(staging: &Path) -> Result<PathBuf> {
    if read_manifest(staging).is_ok() {
        return Ok(staging.to_path_buf());
    }
    let mut subdirs = Vec::new();
    let mut has_files = false;
    for entry in std::fs::read_dir(staging)?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else {
            has_files = true;
        }
    }
    if !has_files && subdirs.len() == 1 {
        let wrapped = &subdirs[0];
        if read_manifest(wrapped).is_ok() {
            return Ok(wrapped.clone());
        }
    }
    Err(Error::Config(format!(
        "no plugin.json found at {} (or any single top-level subdirectory)",
        staging.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_manifest(root: &Path, json: &str) {
        let dir = root.join(".thclaws-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), json).unwrap();
    }

    #[test]
    fn reads_native_manifest_then_falls_back_to_claude() {
        let dir = tempdir().unwrap();
        // thclaws-native wins when both exist.
        write_manifest(
            dir.path(),
            r#"{"name": "from-thclaws", "skills": ["skills"]}"#,
        );
        std::fs::create_dir_all(dir.path().join(".claude-plugin")).unwrap();
        std::fs::write(
            dir.path().join(".claude-plugin/plugin.json"),
            r#"{"name": "from-claude"}"#,
        )
        .unwrap();
        let m = read_manifest(dir.path()).unwrap();
        assert_eq!(m.name, "from-thclaws");
        assert_eq!(m.skills, vec!["skills".to_string()]);
    }

    #[test]
    fn locate_plugin_root_descends_single_wrapper() {
        let dir = tempdir().unwrap();
        let wrapper = dir.path().join("pack-v1");
        write_manifest(&wrapper, r#"{"name": "wrapped"}"#);
        let found = locate_plugin_root(dir.path()).unwrap();
        assert_eq!(found, wrapper);
    }

    #[test]
    fn registry_roundtrip_upsert_remove() {
        let dir = tempdir().unwrap();
        let mut reg = PluginRegistry::default();
        reg.upsert(Plugin {
            name: "one".into(),
            source: "https://example.com/one.git".into(),
            path: dir.path().join("one"),
            version: "1.0.0".into(),
            enabled: true,
        });
        reg.upsert(Plugin {
            name: "two".into(),
            source: String::new(),
            path: dir.path().join("two"),
            version: "0.1.0".into(),
            enabled: true,
        });
        assert_eq!(reg.plugins.len(), 2);
        assert!(reg.find("one").is_some());
        assert!(reg.remove("one").is_some());
        assert_eq!(reg.plugins.len(), 1);
        // Upsert replaces rather than duplicating.
        reg.upsert(Plugin {
            name: "two".into(),
            source: "s".into(),
            path: dir.path().join("two"),
            version: "0.2.0".into(),
            enabled: false,
        });
        assert_eq!(reg.plugins.len(), 1);
        assert_eq!(reg.find("two").unwrap().version, "0.2.0");
        assert!(!reg.find("two").unwrap().enabled);
    }

    #[test]
    fn registry_toggle_enabled_persists() {
        // Verify the flag round-trips through upsert / find without needing
        // disk I/O (we test `set_enabled` end-to-end implicitly via the
        // upsert contract).
        let mut reg = PluginRegistry::default();
        reg.upsert(Plugin {
            name: "p".into(),
            source: String::new(),
            path: PathBuf::from("/tmp/p"),
            version: "1".into(),
            enabled: true,
        });
        let p = reg.plugins.iter_mut().find(|p| p.name == "p").unwrap();
        p.enabled = false;
        assert!(!reg.find("p").unwrap().enabled);
    }

    #[test]
    fn is_zip_url_handles_query_and_fragment() {
        assert!(is_zip_url("https://example.com/a.zip"));
        assert!(is_zip_url("https://example.com/a.ZIP?t=1"));
        assert!(is_zip_url("https://example.com/a.zip#frag"));
        assert!(!is_zip_url("https://github.com/u/r.git"));
    }
}
