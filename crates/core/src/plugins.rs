//! Plugin system — bundle of skills / commands / MCP servers managed as
//! one unit, similar to Claude Code plugins.
//!
//! ## Layout
//!
//! A plugin is a directory (git repo, a zip — remote URL or local file, or
//! a plain local folder) containing a manifest:
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
            engine_managed: false,
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
    /// Installed plugin directory. Absolute in memory; persisted
    /// relative to the registry file's own directory (so, normally just
    /// `plugins/<name>`), which is what lets the registry survive the
    /// workspace being synced to a hosted runner or moved on disk. See
    /// [`PluginRegistry::relative_to_anchor`].
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
        let mut registry: Self = serde_json::from_str(&contents)
            .map_err(|e| Error::Config(format!("parse {}: {e}", path.display())))?;
        // Stored form is relative to the registry's own directory (see
        // `save`); make it absolute for everything downstream, which
        // joins skill / command / agent subdirs onto it.
        if let Some(anchor) = path.parent() {
            for p in &mut registry.plugins {
                if p.path.is_relative() {
                    p.path = anchor.join(&p.path);
                }
            }
        }
        if let Ok(dir) = plugins_dir(user) {
            registry.rebase_to(&dir);
        }
        Ok(registry)
    }

    /// Last-resort repair for a LEGACY registry that still records an
    /// absolute path: re-point each entry at `<dir>/<name>` when a real
    /// plugin sits there.
    ///
    /// Registries written since the relative-path switch don't need this
    /// — but one written by an older build carries a path like
    /// `/Users/x/ws/old/.thclaws/plugins/p`, which resolves nowhere after
    /// a `/cloud push` to a Linux runner or a plain folder move. Every
    /// contribution is built by joining onto that path, so the plugin's
    /// skills silently vanish and `prune_orphaned` then deletes the entry
    /// outright.
    fn rebase_to(&mut self, dir: &Path) {
        for p in &mut self.plugins {
            let derived = dir.join(&p.name);
            if derived != p.path && read_manifest(&derived).is_ok() {
                p.path = derived;
            }
        }
    }

    /// Copy with every contained path rewritten relative to `anchor` —
    /// the directory holding the registry file — so the file describes
    /// itself instead of pinning the workspace to one machine's layout.
    /// Both scopes keep their plugins at `<anchor>/plugins/<name>`, so a
    /// stored path is just `plugins/<name>` either way.
    ///
    /// A path outside the anchor — a developer pointing at a checkout
    /// elsewhere — is left absolute, since there's nothing sensible to
    /// make it relative to.
    fn relative_to_anchor(&self, anchor: &Path) -> Self {
        let mut out = self.clone();
        for p in &mut out.plugins {
            if let Ok(rel) = p.path.strip_prefix(anchor) {
                p.path = rel.to_path_buf();
            }
        }
        out
    }

    pub fn save(&self, user: bool) -> Result<PathBuf> {
        let path = registry_path(user)?;
        let anchor = path
            .parent()
            .ok_or_else(|| Error::Config(format!("registry has no parent: {}", path.display())))?
            .to_path_buf();
        std::fs::create_dir_all(&anchor)?;
        let pretty = serde_json::to_string_pretty(&self.relative_to_anchor(&anchor))
            .map_err(|e| Error::Config(format!("serialize registry: {e}")))?;
        // M6.16 BUG M2: atomic write via tmp + rename. A crash mid-
        // `std::fs::write` would corrupt plugins.json — next launch
        // fails to deserialize, all installed plugins silently drop
        // out of discovery. Same shape as McpAllowlist::save (M6.15).
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &pretty)?;
        std::fs::rename(&tmp, &path)?;
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

/// Whether `name` is safe to use as a single filename component under
/// the plugins directory. Allowed: ASCII alphanumeric + `.` `_` `-`,
/// non-empty, NOT `.` or `..`. Rejects path separators (`/` `\`),
/// control characters, leading dots beyond a single one (we allow
/// names like `dot.config` but reject `.`, `..`, `.hidden` would be
/// allowed since it has more after the dot — that's intentional, only
/// the bare dot-aliases are problematic for path resolution).
fn is_valid_plugin_name(name: &str) -> bool {
    let n = name.trim();
    if n.is_empty() || n == "." || n == ".." {
        return false;
    }
    n.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

fn registry_path(user: bool) -> Result<PathBuf> {
    if user {
        let home = crate::util::home_dir()
            .ok_or_else(|| Error::Config("cannot locate user home directory".into()))?;
        Ok(home.join(format!(
            ".config/{}/plugins.json",
            crate::profile::app_dir_name()
        )))
    } else {
        let cwd = std::env::current_dir()?;
        Ok(cwd.join(".thclaws/plugins.json"))
    }
}

fn plugins_dir(user: bool) -> Result<PathBuf> {
    if user {
        let home = crate::util::home_dir()
            .ok_or_else(|| Error::Config("cannot locate user home directory".into()))?;
        Ok(home.join(format!(
            ".config/{}/plugins",
            crate::profile::app_dir_name()
        )))
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
pub async fn install(url: &str, user: bool, force: bool) -> Result<Plugin> {
    // Org-policy gate: when `policies.plugins.enabled: true`, the URL
    // must match `allowed_hosts`. Open-core builds with no policy hit
    // `AllowDecision::NoPolicy` and pass through unchanged.
    crate::marketplace::ensure_fresh().await;
    if !crate::marketplace::install_url_allowed(url) {
        return Err(Error::Config(crate::marketplace::install_refusal("plugin")));
    }
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
    // M6.16 BUG M1: validate the name is a single safe path component
    // before joining it onto dest_parent. `Path::join` doesn't normalize
    // `..`, so an unchecked `"name": "../../etc/cron.d/x"` would
    // resolve outside the plugins dir on rename. Bounded by FS perms
    // in practice, but no reason to leave the trapdoor open.
    if !is_valid_plugin_name(&manifest.name) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(Error::Config(format!(
            "plugin manifest `name` '{}' is not a safe path component — \
             only [A-Za-z0-9._-] allowed, no '.' / '..' / path separators",
            manifest.name
        )));
    }

    // Move to final location. A *registered* plugin is a real conflict —
    // refuse unless `force`, telling the user to remove it first. But a
    // directory can linger with no registry entry (a prior install whose
    // plugins.json write was lost — deleted by a workspace sync, or a
    // failed rollback). That orphan is unreachable by `/plugin remove`
    // (registry-keyed) yet blocks reinstall, so always adopt it: clear
    // the stale dir and continue.
    let final_dir = dest_parent.join(&manifest.name);
    if final_dir.exists() {
        let registered = PluginRegistry::load(user)
            .ok()
            .and_then(|r| r.find(&manifest.name).cloned())
            .is_some();
        if registered && !force {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(Error::Config(format!(
                "plugin '{}' already installed at {} — run /plugin remove first, \
                 or reinstall with --force",
                manifest.name,
                final_dir.display()
            )));
        }
        if let Err(e) = std::fs::remove_dir_all(&final_dir) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(Error::Config(format!(
                "clear existing plugin dir {}: {e}",
                final_dir.display()
            )));
        }
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
        path: final_dir.clone(),
        version: manifest.version.clone(),
        enabled: true,
    };

    // M6.16.1 BUG L4: rollback the rename if the registry save fails
    // (FS full, permissions, JSON serialize error). Without rollback
    // the user lands in a half-installed state — files on disk under
    // the plugin's name, but registry doesn't know about them, so
    // /plugin show / list / disable / remove all act like the plugin
    // doesn't exist. Manual `rm -rf` is the only recovery.
    //
    // We defer the registry write to the very end so a failed load
    // (corrupt plugins.json) also rolls the rename back.
    let registry_result = (|| -> Result<()> {
        let mut registry = PluginRegistry::load(user)?;
        registry.upsert(plugin.clone());
        registry.save(user)?;
        Ok(())
    })();
    if let Err(e) = registry_result {
        // Best-effort rollback: drop the just-installed dir so the
        // user can retry. On rollback failure, surface BOTH the
        // original error AND the orphaned path so they have a clear
        // recovery action.
        let rollback_failed = std::fs::remove_dir_all(&final_dir).err();
        return Err(Error::Config(match rollback_failed {
            None => {
                format!("registry save failed ({e}); rolled back install (no files left on disk)")
            }
            Some(rb) => format!(
                "registry save failed ({e}); rollback ALSO failed ({rb}). \
                 Plugin files orphaned at {} — run `rm -rf` to clean up before retrying",
                final_dir.display()
            ),
        }));
    }

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
    find_installed_with_scope(name).map(|(p, _)| p)
}

/// Same as [`find_installed`] but also returns the scope (`true` =
/// user, `false` = project) the plugin was found under. Used by
/// `/plugin show` to surface scope so the user knows which `--user`
/// flag to pass to follow-up commands. M6.16.1 BUG L3.
pub fn find_installed_with_scope(name: &str) -> Option<(Plugin, bool)> {
    if let Ok(reg) = PluginRegistry::load(false) {
        if let Some(p) = reg.plugins.into_iter().find(|p| p.name == name) {
            return Some((p, false));
        }
    }
    if let Ok(reg) = PluginRegistry::load(true) {
        if let Some(p) = reg.plugins.into_iter().find(|p| p.name == name) {
            return Some((p, true));
        }
    }
    None
}

/// Remove an installed plugin: delete its files and drop from the registry.
/// Returns whether anything was actually removed.
pub fn remove(name: &str, user: bool) -> Result<bool> {
    let mut registry = PluginRegistry::load(user)?;
    if let Some(plugin) = registry.remove(name) {
        if plugin.path.exists() {
            std::fs::remove_dir_all(&plugin.path)
                .map_err(|e| Error::Config(format!("delete {}: {e}", plugin.path.display())))?;
        }
        registry.save(user)?;
        return Ok(true);
    }
    // No registry entry — but a stale dir can linger on disk when a prior
    // registry write was lost. Clear it too, so `/plugin remove` unsticks
    // the orphan that blocks reinstall instead of only touching the
    // registry. Name is validated first: it's joined onto the plugins dir.
    if is_valid_plugin_name(name) {
        let stale = plugins_dir(user)?.join(name);
        if stale.is_dir() {
            std::fs::remove_dir_all(&stale)
                .map_err(|e| Error::Config(format!("delete {}: {e}", stale.display())))?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Garbage-collect zombie registry entries: those whose `path` no
/// longer exists or whose `manifest()` can't be parsed (e.g. user
/// manually `rm -rf`'d the plugin dir). Walks both scopes, returns
/// the names of removed entries grouped by scope. Both registries
/// are saved if anything was removed. M6.16.1 BUG L2.
pub fn gc() -> Result<(Vec<String>, Vec<String>)> {
    let mut removed_project = Vec::new();
    let mut removed_user = Vec::new();
    for (user, removed) in [(false, &mut removed_project), (true, &mut removed_user)] {
        let Ok(mut registry) = PluginRegistry::load(user) else {
            continue;
        };
        let before = registry.plugins.len();
        registry.plugins.retain(|p| {
            // Keep entries whose dir exists AND whose manifest parses.
            // The manifest read is the more useful signal — a present
            // dir without a valid plugin.json is also broken.
            if !p.path.exists() {
                removed.push(p.name.clone());
                return false;
            }
            if read_manifest(&p.path).is_err() {
                removed.push(p.name.clone());
                return false;
            }
            true
        });
        if registry.plugins.len() != before {
            registry.save(user)?;
        }
    }
    Ok((removed_project, removed_user))
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

/// Resolve one manifest section's directories to absolute paths.
///
/// A section the manifest doesn't declare falls back to `conventional`
/// when that subdir exists on disk — Claude Code plugins rely on the
/// convention instead of declaring it, so an absent key means "look in
/// the usual place", not "contributes nothing". `conventional: None`
/// opts a section out (agents have no such convention here).
///
/// Every surface that asks what a plugin contributes — the loaders and
/// [`contributions`] alike — resolves through this, so a listing can't
/// disagree with what actually loads.
fn section_dirs(root: &Path, rels: &[String], conventional: Option<&str>) -> Vec<PathBuf> {
    if !rels.is_empty() {
        return rels.iter().map(|rel| root.join(rel)).collect();
    }
    match conventional {
        Some(name) => {
            let dir = root.join(name);
            if dir.is_dir() {
                vec![dir]
            } else {
                Vec::new()
            }
        }
        None => Vec::new(),
    }
}

/// Flatten all enabled plugins' skill directories into absolute paths.
/// Each entry is a directory that contains one-or-more `<skill>/SKILL.md`
/// subdirectories (compatible with [`crate::skills::SkillStore`] discovery).
///
/// Undeclared `skills` falls back to a conventional `skills/` subdir —
/// see [`section_dirs`] — so anthropics-style plugins install unchanged.
pub fn plugin_skill_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for plugin in installed_plugins_all_scopes() {
        let Ok(manifest) = plugin.manifest() else {
            continue;
        };
        dirs.extend(section_dirs(&plugin.path, &manifest.skills, Some("skills")));
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
        dirs.extend(section_dirs(
            &plugin.path,
            &manifest.commands,
            Some("commands"),
        ));
    }
    dirs
}

/// Flatten all enabled plugins' agent directories. Returned dirs feed
/// `AgentDefsConfig::load_with_extra`; plugin agents merge additively
/// and never clobber a user's or project's existing agent by name.
///
/// No conventional fallback: agents must be declared explicitly.
pub fn plugin_agent_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for plugin in installed_plugins_all_scopes() {
        let Ok(manifest) = plugin.manifest() else {
            continue;
        };
        dirs.extend(section_dirs(&plugin.path, &manifest.agents, None));
    }
    dirs
}

/// What a plugin actually contributes, counted on disk. The manifest
/// lists *directories*, not items, so "3 skills" needs a walk — a
/// management surface that showed `skills: ["skills"]` would be showing
/// the user our config shape instead of their plugin.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Contributions {
    pub skills: usize,
    pub commands: usize,
    pub agents: usize,
    /// MCP server names, not a count — the connectors surface pairs
    /// them up with live status by name.
    pub mcp_servers: Vec<String>,
}

pub fn contributions(plugin: &Plugin) -> Contributions {
    let Ok(manifest) = plugin.manifest() else {
        return Contributions::default();
    };
    // A skill is a directory holding a SKILL.md; commands and agents are
    // plain `.md` files. Anything else in those dirs isn't a contribution.
    let count = |dirs: &[PathBuf], want_dir: bool| -> usize {
        let mut n = 0;
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in entries.flatten() {
                let path = e.path();
                if want_dir {
                    if path.is_dir() && path.join("SKILL.md").is_file() {
                        n += 1;
                    }
                } else if path.extension().and_then(|x| x.to_str()) == Some("md") {
                    n += 1;
                }
            }
        }
        n
    };
    let mut mcp_servers: Vec<String> = manifest.mcp_servers.keys().cloned().collect();
    mcp_servers.sort();
    Contributions {
        skills: count(
            &section_dirs(&plugin.path, &manifest.skills, Some("skills")),
            true,
        ),
        commands: count(
            &section_dirs(&plugin.path, &manifest.commands, Some("commands")),
            false,
        ),
        agents: count(&section_dirs(&plugin.path, &manifest.agents, None), false),
        mcp_servers,
    }
}

/// `(server_name, plugin_name)` for every MCP server an enabled plugin
/// contributes. A surface that lists connectors has to be able to say
/// which plugin owns one — a plugin's server isn't in any mcp.json, so
/// "remove it" means uninstalling the plugin, not editing config.
pub fn plugin_mcp_server_owners() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for plugin in installed_plugins_all_scopes() {
        let Ok(manifest) = plugin.manifest() else {
            continue;
        };
        for name in manifest.mcp_servers.keys() {
            out.push((name.clone(), plugin.name.clone()));
        }
    }
    out
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
        // Read a local `.zip` off disk (no host required); otherwise fetch
        // it over HTTP. Mirrors the local-folder path so `/plugin install
        // ./pack.zip` works the same as a remote `.zip` URL.
        let bytes = match local_zip_file(url) {
            Some(path) => std::fs::read(&path)
                .map_err(|e| Error::Config(format!("read zip {}: {e}", path.display())))?,
            None => download_zip(url).await?,
        };
        extract_zip(&bytes, dest)
    } else if let Some(src) = local_source_dir(url) {
        // Install from a plain on-disk folder — no `git init` required.
        // Copies the working tree as-is (excluding `.git`), so uncommitted
        // edits ship too. A `#branch` request falls through to git_clone
        // below (explicit branch ⇒ the caller wants committed git state).
        copy_dir_all(&src, dest)
    } else {
        git_clone(url, dest).await
    }
}

fn is_zip_url(url: &str) -> bool {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    without_query.to_ascii_lowercase().ends_with(".zip")
}

/// If `url` names an existing local `.zip` file, return its path — so it can
/// be read off disk instead of fetched over HTTP. A `file://` prefix is
/// stripped (mirroring [`local_source_dir`]). Remote `.zip` URLs return
/// `None` (their string is never an existing local file), falling through to
/// `download_zip`.
fn local_zip_file(url: &str) -> Option<PathBuf> {
    let raw = url.strip_prefix("file://").unwrap_or(url);
    if !raw.to_ascii_lowercase().ends_with(".zip") {
        return None;
    }
    let p = PathBuf::from(raw);
    p.is_file().then_some(p)
}

/// If `url` points at an existing local directory, resolve it to the folder
/// to copy — otherwise `None` (so remote git URLs, scp-style `git@…`, and
/// `https://…` all fall through to `git_clone`). Honors a `#:subpath`
/// fragment for installing one plugin out of a local monorepo, but declines
/// (returns `None`) when a `#branch` is given so that explicit-branch
/// installs keep git semantics.
fn local_source_dir(url: &str) -> Option<PathBuf> {
    let raw = url.strip_prefix("file://").unwrap_or(url);
    let (base, branch, subpath) = crate::skills::parse_git_subpath(raw);
    if branch.is_some() {
        return None;
    }
    let base_dir = PathBuf::from(&base);
    if !base_dir.is_dir() {
        return None;
    }
    let resolved = match &subpath {
        Some(sub) => base_dir.join(sub),
        None => base_dir,
    };
    resolved.is_dir().then_some(resolved)
}

/// Recursively copy `src` into `dest`, skipping any `.git` metadata. Real
/// subdirectories recurse; files (and symlinks, which are followed) are
/// copied with their permissions. Symlinked directories are skipped to
/// avoid cycles.
fn copy_dir_all(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let target = dest.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_dir_all(&path, &target)?;
        } else if ft.is_file() || (ft.is_symlink() && path.is_file()) {
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

async fn download_zip(url: &str) -> Result<Vec<u8>> {
    const MAX_BYTES: u64 = 64 * 1024 * 1024;
    // M6.16 BUG M4: 30 s end-to-end timeout. Mirrors the M6.14 fix
    // that landed for skills::download_zip — same shape, never copy-
    // pasted across. A hostile / slow server can no longer hang
    // `/plugin install` indefinitely.
    let client = reqwest::Client::builder()
        .redirect(crate::policy::allowlist::install_redirect_policy())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Config(format!("http client: {e}")))?;
    let mut req = client.get(url);
    if let Some(token) = crate::marketplace::auth_token_for(url) {
        req = req.bearer_auth(token);
    }
    let resp = req
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

async fn git_clone(url: &str, dest: &Path) -> Result<()> {
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

    // M6.16 BUG M3: 60 s timeout via tokio::process + tokio::time::
    // timeout. Pre-fix this was a sync std::process::Command::output()
    // that blocked indefinitely on a hung git server (TLS stall, DNS
    // hang, hostile peer). 60 s gives a real clone room (the largest
    // plugin repos take ~20–30 s on a fresh ARM macOS at depth 1).
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(&args).kill_on_drop(true);
    let out = match tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            let _ = std::fs::remove_dir_all(&stage_dir);
            return Err(Error::Config(format!("spawn git: {e}")));
        }
        Err(_) => {
            // Tokio aborted the future; kill_on_drop on the Command
            // takes care of the child when `cmd` drops.
            let _ = std::fs::remove_dir_all(&stage_dir);
            return Err(Error::Config("git clone timed out after 60s".into()));
        }
    };
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

    /// A Claude Code plugin declares no `skills` / `commands` — it
    /// relies on the directory convention. `plugin_skill_dirs` honoured
    /// that but `contributions` read the empty manifest lists directly,
    /// so `/plugin list` reported `skills: 0` for a plugin whose skill
    /// was loading fine. Both go through `section_dirs` now.
    #[test]
    fn contributions_counts_conventional_dirs_a_bare_manifest_omits() {
        let dir = tempdir().unwrap();
        write_manifest(dir.path(), r#"{"name": "bare", "version": "1.0.0"}"#);

        let skill = dir.path().join("skills/last30days");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: last30days\n---\n").unwrap();
        // A dir without SKILL.md is not a skill.
        std::fs::create_dir_all(dir.path().join("skills/not-a-skill")).unwrap();

        std::fs::create_dir_all(dir.path().join("commands")).unwrap();
        std::fs::write(dir.path().join("commands/brief.md"), "# brief").unwrap();

        // Agents have no conventional fallback, so this stays uncounted
        // — the listing must match what `plugin_agent_dirs` loads.
        std::fs::create_dir_all(dir.path().join("agents")).unwrap();
        std::fs::write(dir.path().join("agents/researcher.md"), "# researcher").unwrap();

        let plugin = Plugin {
            name: "bare".into(),
            source: String::new(),
            path: dir.path().to_path_buf(),
            version: "1.0.0".into(),
            enabled: true,
        };
        let got = contributions(&plugin);
        assert_eq!(got.skills, 1, "conventional skills/ must be counted");
        assert_eq!(got.commands, 1, "conventional commands/ must be counted");
        assert_eq!(got.agents, 0, "agents have no conventional fallback");
    }

    /// The counts a listing shows have to be the ones the loaders act
    /// on, whether the manifest declares its dirs or leans on the
    /// convention. Guards the two from drifting apart again.
    #[test]
    fn section_dirs_backs_both_the_loader_and_the_listing() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("skills")).unwrap();

        // Declared wins, and is returned even when absent from disk —
        // an explicit path is the author's claim, not a guess.
        let declared = section_dirs(dir.path(), &["custom".to_string()], Some("skills"));
        assert_eq!(declared, vec![dir.path().join("custom")]);

        let fallback = section_dirs(dir.path(), &[], Some("skills"));
        assert_eq!(fallback, vec![dir.path().join("skills")]);

        // Convention only applies when the dir is really there.
        assert!(section_dirs(dir.path(), &[], Some("commands")).is_empty());
        // Opted out entirely.
        assert!(section_dirs(dir.path(), &[], None).is_empty());
    }

    /// The full disk round-trip: what `save` writes, and what `load`
    /// hands back after the registry has been picked up and moved —
    /// which is exactly what `/cloud push` does to it. Pinned to the
    /// user scope (HOME-based) to avoid CWD races with parallel tests.
    #[test]
    fn save_writes_relative_and_load_reanchors_it() {
        let guard = scoped_user_home();
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
        let installed = home.join(".config/thclaws/plugins/travelling");
        std::fs::create_dir_all(&installed).unwrap();
        write_manifest(&installed, r#"{"name": "travelling"}"#);

        let mut reg = PluginRegistry::default();
        reg.upsert(Plugin {
            name: "travelling".into(),
            source: String::new(),
            path: installed.clone(),
            version: "1.0.0".into(),
            enabled: true,
        });
        let saved = reg.save(true).expect("save");

        // On disk: no absolute path, so nothing pins this file to one
        // machine's layout.
        let body = std::fs::read_to_string(&saved).unwrap();
        assert!(
            body.contains(r#""path": "plugins/travelling""#),
            "expected a registry-relative path, got: {body}"
        );
        assert!(
            !body.contains(home.to_str().unwrap()),
            "absolute path leaked into the registry: {body}"
        );

        // Back in memory: absolute again, so skill / command / agent
        // dirs resolve.
        let loaded = PluginRegistry::load(true).expect("load");
        assert_eq!(loaded.plugins[0].path, installed);
        drop(guard);
    }

    #[test]
    fn paths_persist_relative_to_the_registry_directory() {
        // The registry file's own directory is the anchor, which is the
        // same rule in both scopes: `<ws>/.thclaws` for a project,
        // `~/.config/thclaws` for the user, plugins under `plugins/`
        // either way.
        let ws = tempdir().unwrap();
        let anchor = ws.path().join(".thclaws");
        let installed = anchor.join("plugins/thai-book-production");
        std::fs::create_dir_all(&installed).unwrap();

        let reg = PluginRegistry {
            plugins: vec![
                Plugin {
                    name: "thai-book-production".into(),
                    source: String::new(),
                    path: installed.clone(),
                    version: "1.9.9".into(),
                    enabled: true,
                },
                // A checkout outside the workspace — nothing sensible to
                // make it relative to, so it stays absolute and keeps
                // working for plugin development in place.
                Plugin {
                    name: "dev-checkout".into(),
                    source: String::new(),
                    path: PathBuf::from("/opt/src/dev-checkout"),
                    version: String::new(),
                    enabled: true,
                },
            ],
        };

        let stored = reg.relative_to_anchor(&anchor);
        assert_eq!(
            stored.plugins[0].path,
            PathBuf::from("plugins/thai-book-production")
        );
        assert_eq!(
            stored.plugins[1].path,
            PathBuf::from("/opt/src/dev-checkout")
        );

        // What `load` does with the stored form: relative entries are
        // re-anchored wherever the registry now sits — a Linux runner
        // after `/cloud push`, or a moved folder.
        let moved = tempdir().unwrap().path().join("runner/.thclaws");
        let rehydrated: Vec<PathBuf> = stored
            .plugins
            .iter()
            .map(|p| {
                if p.path.is_relative() {
                    moved.join(&p.path)
                } else {
                    p.path.clone()
                }
            })
            .collect();
        assert_eq!(rehydrated[0], moved.join("plugins/thai-book-production"));
        assert_eq!(rehydrated[1], PathBuf::from("/opt/src/dev-checkout"));
    }

    #[test]
    fn rebase_repoints_a_registry_that_travelled_between_machines() {
        // What `/cloud push` produces: the files ride along under
        // `.thclaws/plugins/<name>`, but plugins.json still records the
        // absolute path from the machine that installed it.
        let dir = tempdir().unwrap();
        let installed = dir.path().join("thai-book-production");
        std::fs::create_dir_all(&installed).unwrap();
        write_manifest(&installed, r#"{"name": "thai-book-production"}"#);

        let mut reg = PluginRegistry {
            plugins: vec![Plugin {
                name: "thai-book-production".into(),
                source: String::new(),
                path: PathBuf::from(
                    "/Users/someone/ws/other/.thclaws/plugins/thai-book-production",
                ),
                version: "1.9.9".into(),
                enabled: true,
            }],
        };
        reg.rebase_to(dir.path());
        assert_eq!(reg.plugins[0].path, installed);

        // A name with no plugin dir beside it keeps whatever was
        // recorded, so `prune_orphaned` can still report it as missing
        // instead of this silently inventing a path.
        let mut orphan = PluginRegistry {
            plugins: vec![Plugin {
                name: "gone".into(),
                source: String::new(),
                path: PathBuf::from("/Users/someone/ws/other/.thclaws/plugins/gone"),
                version: String::new(),
                enabled: true,
            }],
        };
        orphan.rebase_to(dir.path());
        assert_eq!(
            orphan.plugins[0].path,
            PathBuf::from("/Users/someone/ws/other/.thclaws/plugins/gone")
        );
    }

    #[test]
    fn local_source_dir_resolves_folder_and_subpath() {
        let dir = tempdir().unwrap();
        // Bare folder path → itself.
        assert_eq!(
            local_source_dir(dir.path().to_str().unwrap()),
            Some(dir.path().to_path_buf())
        );
        // `file://` prefix is stripped.
        assert_eq!(
            local_source_dir(&format!("file://{}", dir.path().display())),
            Some(dir.path().to_path_buf())
        );
        // `#:subpath` selects a child dir.
        let sub = dir.path().join("plugin-a");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(
            local_source_dir(&format!("{}#:plugin-a", dir.path().display())),
            Some(sub)
        );
        // Explicit branch declines (keeps git semantics).
        assert_eq!(
            local_source_dir(&format!("{}#main", dir.path().display())),
            None
        );
        // Non-existent path and remote URLs are not local dirs.
        assert_eq!(local_source_dir("/no/such/plugin/dir"), None);
        assert_eq!(local_source_dir("https://github.com/x/y.git"), None);
    }

    #[test]
    fn copy_dir_all_copies_tree_and_skips_git() {
        let src = tempdir().unwrap();
        write_manifest(src.path(), r#"{"name": "folder-plugin"}"#);
        std::fs::create_dir_all(src.path().join("skills/a")).unwrap();
        std::fs::write(src.path().join("skills/a/SKILL.md"), "# a").unwrap();
        // .git must NOT be copied.
        std::fs::create_dir_all(src.path().join(".git")).unwrap();
        std::fs::write(src.path().join(".git/config"), "x").unwrap();

        let dest = tempdir().unwrap();
        let out = dest.path().join("folder-plugin");
        copy_dir_all(src.path(), &out).unwrap();

        assert_eq!(read_manifest(&out).unwrap().name, "folder-plugin");
        assert!(out.join("skills/a/SKILL.md").is_file());
        assert!(!out.join(".git").exists());
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

    #[test]
    fn local_zip_file_matches_existing_file_not_remote_url() {
        let dir = tempdir().unwrap();
        let zip = dir.path().join("pack.zip");
        std::fs::write(&zip, b"PK\x03\x04").unwrap();
        // Existing local .zip → resolved (bare path + file:// prefix).
        assert_eq!(local_zip_file(zip.to_str().unwrap()), Some(zip.clone()));
        assert_eq!(
            local_zip_file(&format!("file://{}", zip.display())),
            Some(zip.clone())
        );
        // Remote URL string is never an existing local file.
        assert_eq!(local_zip_file("https://example.com/a.zip"), None);
        // Non-.zip and missing paths decline.
        assert_eq!(
            local_zip_file(dir.path().join("nope.zip").to_str().unwrap()),
            None
        );
        assert_eq!(
            local_zip_file(zip.with_extension("tar").to_str().unwrap()),
            None
        );
    }

    /// M6.16 BUG M1: `manifest.name` must be a single safe path
    /// component before being joined onto the plugins dir. Pre-fix a
    /// malicious plugin could escape the install root via `../`-laden
    /// names. `Path::join` doesn't normalize `..`, so the rename in
    /// install() would land outside the intended dir.
    #[test]
    fn rejects_unsafe_plugin_names() {
        assert!(!is_valid_plugin_name(""));
        assert!(!is_valid_plugin_name("."));
        assert!(!is_valid_plugin_name(".."));
        assert!(!is_valid_plugin_name("../foo"));
        assert!(!is_valid_plugin_name("../../etc/cron.d/x"));
        assert!(!is_valid_plugin_name("foo/bar"));
        assert!(!is_valid_plugin_name(r"foo\bar"));
        assert!(!is_valid_plugin_name("foo\0null"));
        assert!(!is_valid_plugin_name("space inside"));
        assert!(!is_valid_plugin_name("emoji-🦀"));
    }

    #[test]
    fn accepts_typical_plugin_names() {
        assert!(is_valid_plugin_name("foo"));
        assert!(is_valid_plugin_name("foo-bar"));
        assert!(is_valid_plugin_name("foo_bar"));
        assert!(is_valid_plugin_name("foo.bar"));
        assert!(is_valid_plugin_name("Foo123"));
        assert!(is_valid_plugin_name(".hidden")); // leading dot is fine; only bare `.`/`..` rejected
        assert!(is_valid_plugin_name("a")); // single char
    }

    /// M6.16 BUG M2: PluginRegistry::save now uses tmp + rename so a
    /// crash mid-write can't leave a half-written plugins.json that
    /// fails to deserialize on next launch (which would silently drop
    /// every installed plugin from discovery).
    ///
    /// We can't directly trigger a crash, but we can verify the
    /// expected on-disk layout: the save path exists, the .tmp file
    /// does NOT (it was renamed away), and the contents round-trip.
    /// Serialize tests that mutate process-global env vars (HOME) so
    /// they don't race against parallel sibling tests. Shares the
    /// crate-wide lock with kms / oauth tests for the same reason —
    /// `set_var` / `set_current_dir` are process-wide effects.
    struct UserScopeGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_home: Option<String>,
        _dir: tempfile::TempDir,
    }

    impl Drop for UserScopeGuard {
        fn drop(&mut self) {
            match &self.prev_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn scoped_user_home() -> UserScopeGuard {
        let lock = crate::kms::test_env_lock();
        let prev = std::env::var("HOME").ok();
        let dir = tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        UserScopeGuard {
            _lock: lock,
            prev_home: prev,
            _dir: dir,
        }
    }

    /// M6.16.1 BUG L2: `gc()` removes registry entries whose plugin
    /// directory is missing. Pinned via the user scope (HOME-based)
    /// to avoid CWD races with parallel sibling tests.
    #[test]
    fn gc_removes_entries_with_missing_dir() {
        let guard = scoped_user_home();
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());

        // One real plugin (with a valid manifest), one zombie (path
        // doesn't exist on disk).
        let real_path = home.join("real-plugin");
        write_manifest(&real_path, r#"{"name": "real"}"#);
        let zombie_path = home.join("vanished-plugin"); // never created

        let mut reg = PluginRegistry::default();
        reg.upsert(Plugin {
            name: "real".into(),
            source: String::new(),
            path: real_path.clone(),
            version: "1".into(),
            enabled: true,
        });
        reg.upsert(Plugin {
            name: "zombie".into(),
            source: String::new(),
            path: zombie_path,
            version: "1".into(),
            enabled: true,
        });
        reg.save(true).unwrap();

        let (proj, user) = gc().unwrap();
        assert!(proj.is_empty(), "no project-scope changes expected");
        assert_eq!(user, vec!["zombie".to_string()]);

        // After gc: only "real" remains in user scope.
        let reloaded = PluginRegistry::load(true).unwrap();
        assert_eq!(reloaded.plugins.len(), 1);
        assert_eq!(reloaded.plugins[0].name, "real");

        drop(guard);
    }

    #[test]
    fn registry_save_atomic_uses_tmp_then_rename() {
        let guard = scoped_user_home();
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());

        let mut reg = PluginRegistry::default();
        reg.upsert(Plugin {
            name: "atomic-test".into(),
            source: "https://example.com/x.git".into(),
            path: home.join("atomic-test"),
            version: "1.0.0".into(),
            enabled: true,
        });
        let saved_path = reg.save(true).expect("save");

        // .tmp must NOT linger after a successful rename.
        let tmp = saved_path.with_extension("json.tmp");
        assert!(
            !tmp.exists(),
            "save left .tmp file behind: {}",
            tmp.display()
        );
        // The real file must be there with valid JSON.
        let body = std::fs::read_to_string(&saved_path).expect("read");
        assert!(body.contains("\"atomic-test\""));

        // Round-trip via load.
        let reloaded = PluginRegistry::load(true).expect("reload");
        assert_eq!(reloaded.plugins.len(), 1);
        assert_eq!(reloaded.plugins[0].name, "atomic-test");

        drop(guard);
    }
}
