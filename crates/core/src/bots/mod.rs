//! dev-plan/59: many bots in one workspace.
//!
//! A **bot** is a whole agent installed at `.thclaws/bots/<slug>/` — its own
//! `AGENTS.md`, `settings.json` and `.thclaws/state/` (sessions, KMS, browser
//! profile). Nothing is shared with the host or with a sibling; `bots/` is a
//! shelf, not state.
//!
//! A bot runs as its **own process**: an ordinary `thclaws --serve` with cwd
//! set to the bot's folder. Process cwd is what the engine actually reads —
//! 130 `env::current_dir()` sites against 22 `current_workdir()` ones, and
//! `config.rs` uses none of the latter — so per-process gets identity,
//! config, sandbox, MCP and permission mode right for free, where N workers
//! inside one process would each load the host's settings while writing
//! sessions to their own folder.
//!
//! The **host** supervises those processes and runs no agent, no model and no
//! MCP of its own. It reaches every bot's folder, but it is deterministic
//! code, not something a web page can talk to — which is the whole reason the
//! supervisor is not itself an agent.
//!
//! Step 3 scope: one bot, spawned from a hand-edited `.thclaws/bots.json`,
//! proxied and restarted on crash. No migration, no UI, no `/cloud get`.

pub mod agent_paths;
pub mod desktop;
pub mod install;
pub mod migrate;
pub mod proxy;
pub mod supervisor;

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Hand-edited in Step 3; written by bot install from Step 5 on.
pub const CONFIG_REL: &str = ".thclaws/bots.json";

/// The shelf. Under `.thclaws/` rather than at the workspace root because
/// `cloud/pack.rs` strips by root-anchored prefix: one `.thclaws/bots/` entry
/// drops every bot from a publish, where a top-level `bots/` would have
/// matched no strip rule and uploaded every bot's cookies.
pub const SHELF_REL: &str = ".thclaws/bots";

/// `.thclaws/bots.json` — which bots this workspace has.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BotsConfig {
    pub version: u32,
    pub bots: Vec<BotDef>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BotDef {
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl BotDef {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.slug)
    }
}

/// Follow a bot that is itself a workspace down to the agent that actually
/// runs.
///
/// finding 12 left workspaces migrated twice, where `bots/<slug>/` is another
/// host and the real agent is a level further down. The window resolves the
/// gui-shell and file assets it serves from the bot folder, and stopping at
/// the first level found none of them — a 404 for every asset and a blank
/// panel, while the same workspace worked under `--serve`, where the innermost
/// process serves from its own cwd.
///
/// Bounded: a chain this long is already damage, and a loop here would hang a
/// request.
pub fn resolve_agent_dir(root: &Path, slug: &str) -> PathBuf {
    resolve_agent(root, slug).1
}

/// The agent that actually runs, and the workspace it runs in.
///
/// These are two different folders and confusing them is its own bug: what the
/// agent SHIPS — its gui-shell, its settings — is in the agent folder, while
/// the user's files are in the workspace above it. A doubly-migrated tree puts
/// them one level deeper than the shelf the window is holding, so both have to
/// be walked to rather than assumed.
pub fn resolve_agent(root: &Path, slug: &str) -> (PathBuf, PathBuf) {
    let mut ws = root.to_path_buf();
    let mut dir = bot_dir(&ws, slug);
    for _ in 0..4 {
        // `bots.json` inside a bot folder means that folder is a host.
        let Ok(cfg) = BotsConfig::load(&dir) else {
            break;
        };
        let Some(first) = cfg.bots.first() else {
            break;
        };
        let inner = bot_dir(&dir, &first.slug);
        if !inner.is_dir() {
            break;
        }
        ws = dir;
        dir = inner;
    }
    (ws, dir)
}

/// A slug names a directory and is the cwd a child process is spawned in, so
/// it is restricted to one plain path component. No dots at all, which
/// answers `.`, `..` and hidden-directory questions in one rule.
pub fn validate_slug(slug: &str) -> Result<()> {
    let bad = |why: &str| {
        Err(Error::Config(format!(
            "agent name '{slug}' is invalid: {why} (allowed: a-z, 0-9, '-', '_', starting with a letter or digit)"
        )))
    };
    if slug.is_empty() {
        return bad("empty");
    }
    if slug.len() > 64 {
        return bad("longer than 64 characters");
    }
    if !slug
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return bad("must start with a letter or digit");
    }
    if let Some(c) = slug
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
    {
        return bad(&format!("contains '{c}'"));
    }
    Ok(())
}

/// Hold the workspace's host lock. Two hosts on one workspace would both spawn
/// children and fight over `bots.json` and the address files; the migration
/// takes the same lock so it cannot move a tree a host is serving. The lock is
/// the OS's (`flock`), so a host that dies releases it without cleanup.
pub struct HostLock {
    _file: std::fs::File,
}

/// #226: the lock lives OUTSIDE the workspace. It used to be
/// `.thclaws/state/host.lock`, and the migration held it open while moving
/// `.thclaws` — Windows refuses to rename a directory with an open file
/// inside, so every upgrade on Windows failed with error 5, and the
/// manual `thclaws bots migrate` with it. Keyed by the folder's canonical
/// path, so every spelling of one folder meets the same lock.
fn host_lock_path(workspace: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mut key = canonical.to_string_lossy().into_owned();
    if cfg!(windows) {
        key = key.to_lowercase();
    }
    let digest = Sha256::digest(key.as_bytes());
    let name: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    let dir = crate::util::home_dir()
        .map(|h| h.join(".config/thclaws/locks"))
        .unwrap_or_else(|| std::env::temp_dir().join("thclaws-locks"));
    dir.join(format!("{name}.lock"))
}

/// Where hosts before #226 kept the lock.
const LEGACY_HOST_LOCK: &str = ".thclaws/state/host.lock";

pub fn lock_workspace(workspace: &Path, who: &str) -> Result<HostLock> {
    use fs2::FileExt;
    let busy = || {
        Error::Config(format!(
            "{} is already open by another thClaws host — {who} cannot run alongside it. Close \
             that window or `--serve` first.",
            workspace.display()
        ))
    };
    let path = host_lock_path(workspace);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    if file.try_lock_exclusive().is_err() {
        return Err(busy());
    }
    // An older thClaws serving this folder holds the old lock. Ask it, then
    // let go at once: holding it is exactly what broke the Windows upgrade.
    let legacy = workspace.join(LEGACY_HOST_LOCK);
    if legacy.exists() {
        if let Ok(old) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&legacy)
        {
            if old.try_lock_exclusive().is_err() {
                return Err(busy());
            }
            let _ = FileExt::unlock(&old);
        }
    }
    Ok(HostLock { _file: file })
}

pub fn shelf_dir(workspace: &Path) -> PathBuf {
    workspace.join(SHELF_REL)
}

pub fn bot_dir(workspace: &Path, slug: &str) -> PathBuf {
    shelf_dir(workspace).join(slug)
}

const EXAMPLE: &str = r#"{"version": 1, "bots": [{"slug": "main", "name": "Main"}]}"#;

impl BotsConfig {
    pub fn load(workspace: &Path) -> Result<Self> {
        let path = workspace.join(CONFIG_REL);
        let raw = std::fs::read_to_string(&path).map_err(|e| {
            Error::Config(format!(
                "cannot read {}: {e}\nStep 3 takes a hand-edited file, e.g.\n  {EXAMPLE}",
                path.display()
            ))
        })?;
        let cfg: Self = serde_json::from_str(&raw)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(Error::Config(format!(
                "{CONFIG_REL}: version {} is not supported by this build (expected 1)",
                self.version
            )));
        }
        if self.bots.is_empty() {
            return Err(Error::Config(format!(
                "{CONFIG_REL}: no agents listed — every workspace has at least one"
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for def in &self.bots {
            validate_slug(&def.slug)?;
            if !seen.insert(def.slug.as_str()) {
                return Err(Error::Config(format!(
                    "{CONFIG_REL}: duplicate agent name '{}'",
                    def.slug
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shelf(root: &Path, slug: &str) -> PathBuf {
        let dir = bot_dir(root, slug);
        std::fs::create_dir_all(dir.join(".thclaws")).unwrap();
        std::fs::write(
            root.join(CONFIG_REL),
            format!(r#"{{"version":1,"bots":[{{"slug":"{slug}"}}]}}"#),
        )
        .unwrap();
        dir
    }

    /// finding 12: the window resolves the shell it serves from the bot
    /// folder, and a workspace migrated twice puts the real agent a level
    /// further down. Stopping at the first level served none of its assets.
    #[test]
    fn a_bot_that_is_itself_a_host_resolves_to_the_agent_inside_it() {
        let ws = tempfile::tempdir().unwrap();
        // An ordinary workspace: the bot folder IS the agent.
        let plain = shelf(ws.path(), "main");
        assert_eq!(resolve_agent_dir(ws.path(), "main"), plain);

        // Migrated twice: the bot folder is a host holding the agent.
        let inner = shelf(&plain, "main");
        assert_eq!(resolve_agent_dir(ws.path(), "main"), inner);

        // And once more, in case it ever happened three times.
        let deepest = shelf(&inner, "main");
        assert_eq!(resolve_agent_dir(ws.path(), "main"), deepest);

        // The workspace an agent runs in is the folder ABOVE it — where the
        // user's files are. Serving those from the agent folder is how the
        // chapter images 404'd.
        assert_eq!(resolve_agent(ws.path(), "main"), (inner.clone(), deepest));
        assert_eq!(
            resolve_agent(ws.path(), "main").0,
            inner,
            "one level above the agent, not the shelf the window holds"
        );
    }

    /// The slug becomes a directory name AND a child's cwd, so traversal
    /// and separator shapes are refused rather than sanitised.
    #[test]
    fn slug_rejects_traversal_and_separators() {
        for good in ["main", "research", "book-author", "bot_2", "a"] {
            assert!(validate_slug(good).is_ok(), "{good} should be allowed");
        }
        for bad in [
            "",
            "..",
            ".",
            ".hidden",
            "a/b",
            "a\\b",
            "../escape",
            "has space",
            "-leading",
            "UPPER/../x",
            "nul\0byte",
        ] {
            assert!(validate_slug(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(validate_slug(&"a".repeat(65)).is_err());
    }

    #[test]
    fn config_rejects_duplicates_and_unknown_versions() {
        let dir = tempfile::tempdir().unwrap();
        let write = |body: &str| {
            std::fs::create_dir_all(dir.path().join(".thclaws")).unwrap();
            std::fs::write(dir.path().join(CONFIG_REL), body).unwrap();
        };

        write(r#"{"version": 1, "bots": [{"slug": "main"}]}"#);
        let cfg = BotsConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.bots[0].display_name(), "main");

        write(r#"{"version": 2, "bots": [{"slug": "main"}]}"#);
        assert!(BotsConfig::load(dir.path()).is_err());

        write(r#"{"version": 1, "bots": [{"slug": "a"}, {"slug": "a"}]}"#);
        assert!(BotsConfig::load(dir.path()).is_err());

        write(r#"{"version": 1, "bots": []}"#);
        assert!(BotsConfig::load(dir.path()).is_err());

        write(r#"{"version": 1, "bots": [{"slug": "../evil"}]}"#);
        assert!(BotsConfig::load(dir.path()).is_err());
    }

    /// A second host on the same workspace must be refused, and the refusal
    /// must go away the moment the first one is dropped.
    #[test]
    fn one_host_per_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let first = lock_workspace(dir.path(), "a host").unwrap();
        let err = lock_workspace(dir.path(), "a migration")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("already open"), "{err}");
        drop(first);
        assert!(lock_workspace(dir.path(), "a host").is_ok());
    }

    /// #226: a migration moves `.thclaws`, and Windows will not rename a
    /// directory with an open file inside — so the lock it holds while
    /// moving must not be in the workspace at all.
    #[test]
    fn the_host_lock_is_not_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let _held = lock_workspace(dir.path(), "a migration").unwrap();
        assert!(!host_lock_path(dir.path()).starts_with(dir.path()));
        assert!(!dir.path().join(LEGACY_HOST_LOCK).exists());
        // Another spelling of the same folder meets the same lock.
        let other = dir.path().join(".").join("");
        assert_eq!(host_lock_path(&other), host_lock_path(dir.path()));
    }

    /// A host from before the move still holds the old lock; a migration
    /// must still see it.
    #[test]
    fn an_older_hosts_lock_is_still_honoured() {
        use fs2::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join(LEGACY_HOST_LOCK);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let old = std::fs::File::create(&legacy).unwrap();
        old.lock_exclusive().unwrap();
        assert!(lock_workspace(dir.path(), "a migration").is_err());
        FileExt::unlock(&old).unwrap();
        assert!(lock_workspace(dir.path(), "a migration").is_ok());
    }

    /// The message a user sees when they have not written the file yet is
    /// the only documentation Step 3 ships, so it must carry the shape.
    #[test]
    fn missing_config_explains_the_shape() {
        let dir = tempfile::tempdir().unwrap();
        let err = BotsConfig::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("bots.json"), "{err}");
        assert!(err.contains(r#""slug""#), "{err}");
    }
}
