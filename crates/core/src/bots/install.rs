//! dev-plan/59 Step 5: install a catalogue agent as a bot.
//!
//! Installing a bot is a **host** action, not a slash command. The plan
//! originally wrote this as "`/cloud get` installs to `.thclaws/bots/<slug>/`",
//! which predates the decision that the host runs no agent: a bot's sandbox
//! root is its own folder, so a bot cannot write to a sibling's — and that is
//! §4 working, not a gap. `/cloud get` run inside a bot still replaces that
//! bot, which is the coherent meaning of "get" from where it stands.
//!
//! The download itself is not re-implemented here. `cloud::cmd::get_lines`
//! carries the checks that matter — fail-closed on a missing SHA-256 or
//! agent-UUID header, refuse to overwrite a folder bound to a different
//! agent, carry the installer's own settings across the extraction — and
//! re-deriving any of those would be one copy drifting from the other.

use super::{bot_dir, BotDef, BotsConfig, CONFIG_REL};
use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Installed {
    pub slug: String,
    pub dir: PathBuf,
    /// The installer's progress log, as `/cloud get` renders it.
    pub lines: Vec<String>,
    /// `false` when the bot was already listed — the install was an update.
    pub newly_registered: bool,
}

/// Download `slug` from the catalogue into `.thclaws/bots/<slug>/` and list it
/// in `.thclaws/bots.json`. The registration only happens when the extraction
/// actually succeeded: a bot listed but not installed would be a child the
/// supervisor tries to spawn in an empty folder.
pub async fn install(
    workspace: &Path,
    slug: &str,
    version: Option<&str>,
    force: bool,
) -> Result<Installed> {
    super::validate_slug(slug)?;
    if !workspace.join(CONFIG_REL).exists() {
        return Err(Error::Config(format!(
            "{} is not a workspace that holds agents — `thclaws bots migrate` converts one, or start a \
             host in an empty directory.",
            workspace.display()
        )));
    }
    let dir = bot_dir(workspace, slug);
    // Whether the shelf already had this folder decides what a failure may
    // clean up: a failed FIRST install should leave the shelf exactly as it
    // was, but a failed UPDATE must not delete a bot's sessions and logins.
    let is_new = !dir.exists();
    std::fs::create_dir_all(&dir)?;

    let url = crate::cloud::persisted_url();
    let outcome = crate::cloud::cmd::get_lines(
        slug.to_string(),
        dir.clone(),
        version.map(str::to_string),
        force,
        url.as_deref(),
        None,
    )
    .await;

    if !outcome.ok {
        let not_found = outcome.lines.iter().any(|l| l.contains("status 404"));
        if is_new {
            // Nothing of the user's is in there — leaving it would put an
            // empty or half-extracted folder on the shelf for the host UI to
            // show as a bot.
            let _ = std::fs::remove_dir_all(&dir);
        }
        if not_found {
            return Err(Error::Config(format!(
                "There is no Agent Template named '{slug}'. Pick one from the list, or start \
                 a blank agent to make an empty one with that name."
            )));
        }
        return Err(Error::Tool(outcome.lines.join("\n")));
    }

    seed_workspace_gateway_choice(workspace, &dir);
    seed_workspace_model(workspace, &dir);
    let newly_registered = register(workspace, slug)?;
    Ok(Installed {
        slug: slug.to_string(),
        dir,
        lines: outcome.lines,
        newly_registered,
    })
}

/// `{ok, templates: [{slug, name, description}]}` from the catalogue this
/// workspace installs from, or `{ok: false, error}`. Errors are a value, not a
/// status, so the panel can still offer a blank agent when it cannot list.
pub async fn templates() -> serde_json::Value {
    let url = crate::cloud::resolve_cloud_url(crate::cloud::persisted_url().as_deref(), None);
    let client = crate::cloud::client::Client::new(&url, crate::cloud::token());
    match client.list_agents(false).await {
        Ok(list) => serde_json::json!({
            "ok": true,
            "templates": list
                .into_iter()
                .map(|a| serde_json::json!({
                    "slug": a.slug,
                    "name": a.name,
                    "description": a.description,
                }))
                .collect::<Vec<_>>(),
        }),
        Err(e) => serde_json::json!({ "ok": false, "error": e, "templates": [] }),
    }
}

/// Add a bot with no agent in it — the same thing as opening thClaws on a
/// new, empty folder. The bot's `--serve` writes its own settings template on
/// first start; this only makes the folder, gives it the workspace's gateway
/// choice so it doesn't open on "no API key", and lists it.
///
/// Never reuses a folder: one already on the shelf holds some bot's sessions,
/// and "empty" must not mean "adopt whatever was there".
pub fn create_blank(workspace: &Path, slug: &str) -> Result<Installed> {
    super::validate_slug(slug)?;
    if !workspace.join(CONFIG_REL).exists() {
        return Err(Error::Config(format!(
            "{} is not a workspace that holds agents — `thclaws bots migrate` converts one, or start a \
             host in an empty directory.",
            workspace.display()
        )));
    }
    let dir = bot_dir(workspace, slug);
    if dir.exists()
        || BotsConfig::load(workspace)?
            .bots
            .iter()
            .any(|b| b.slug == slug)
    {
        return Err(Error::Config(format!(
            "an agent named '{slug}' already exists in this workspace — pick another name"
        )));
    }
    std::fs::create_dir_all(&dir)?;
    // The first-run file a new folder gets, written before the gateway seed:
    // seeded first, the seed's two-key file made the engine see "settings
    // already exist" and skip the documented template.
    crate::config::ProjectConfig::ensure_default_exists_in(&dir);
    seed_workspace_gateway_choice(workspace, &dir);
    seed_workspace_model(workspace, &dir);
    let newly_registered = register(workspace, slug)?;
    Ok(Installed {
        slug: slug.to_string(),
        lines: vec![format!("Created a blank agent at {}.", dir.display())],
        dir,
        newly_registered,
    })
}

/// Give a freshly installed bot the workspace's gateway choice.
///
/// `gatewayProxy` is a project-level setting and stays one — a bot may
/// legitimately be pinned to BYOK. But a bot installed into a workspace whose
/// user is logged in and routing through the gateway should not open saying
/// "no API key": the credential is user-level and already reachable, only the
/// routing choice was missing. Copied at install, never afterwards, so a bot
/// the user later re-points keeps its own answer.
fn seed_workspace_gateway_choice(workspace: &Path, dest: &Path) {
    // Read the way `apply_to` reads a project: an explicit `gatewayProxy`
    // wins, and a non-empty legacy `gatewayUseFor` list means "on". Hosted
    // runners write only the list, so reading the flag alone found no choice
    // on the host or on `main`, and every agent added to a hosted workspace
    // opened without the gateway.
    let flag_in = |path: PathBuf| -> Option<bool> {
        let v = std::fs::read(path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())?;
        if let Some(b) = v.get("gatewayProxy").and_then(|b| b.as_bool()) {
            return Some(b);
        }
        v.get("gatewayUseFor")
            .and_then(|l| l.as_array())
            .filter(|l| {
                l.iter()
                    .any(|p| p.as_str().is_some_and(|s| !s.trim().is_empty()))
            })
            .map(|_| true)
    };
    let host_on = flag_in(workspace.join(".thclaws/settings.json"))
        // After a migration the workspace's original agent — and the choice
        // the user made in it — is `bots[0]`, not the host root, which the
        // migration leaves minimal. The first cut read only the host and so
        // never seeded for exactly the workspace that reported the bug.
        .or_else(|| {
            BotsConfig::load(workspace)
                .ok()
                .and_then(|c| c.bots.first().map(|b| b.slug.clone()))
                .map(|slug| bot_dir(workspace, &slug).join(".thclaws/settings.json"))
                .and_then(flag_in)
        })
        .or_else(|| {
            crate::config::AppConfig::load()
                .ok()
                .map(|c| c.gateway_proxy)
        })
        .unwrap_or(false);
    if !host_on {
        return;
    }
    let path = dest.join(".thclaws/settings.json");
    let mut base = std::fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(obj) = base.as_object_mut() else {
        return;
    };
    // A bundle that ships its own choice keeps it. A choice is a boolean
    // flag or a non-empty provider list — the same reading `apply_to` uses.
    // Not a bare key: the catalogue's own hello-world ships
    // `"gatewayUseFor": null`, and treating that null as a decision is why the
    // seed never fired on the real workspace even though its test passed.
    let pinned = obj.get("gatewayProxy").and_then(|v| v.as_bool()).is_some()
        || obj
            .get("gatewayUseFor")
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty());
    if pinned {
        return;
    }
    obj.insert("gatewayProxy".into(), serde_json::json!(true));
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if let Ok(body) = serde_json::to_string_pretty(obj) {
        let _ = std::fs::write(&path, body);
    }
}

/// Put a freshly added bot on the model its workspace is using, unless the
/// bundle pins one this install can reach.
///
/// A catalogue pin is the author's choice for their own install: book-author
/// pins `deepseek-v4-pro`, which an org-locked tenant does not route, and a
/// blank agent would otherwise open on the credential-aware default rather
/// than the model the user picked. Copied at install, never afterwards, like
/// the gateway choice.
fn seed_workspace_model(workspace: &Path, dest: &Path) {
    let model_in = |path: PathBuf| -> Option<String> {
        let v = std::fs::read(path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())?;
        v.get("model")
            .and_then(|m| m.as_str())
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
    };
    // Same order as the gateway seed: host root, then the original agent a
    // migration moved the user's choice into, then the user level — where
    // a hosted runner's HOSTED_DEFAULT_MODEL lands.
    let current = model_in(workspace.join(".thclaws/settings.json"))
        .or_else(|| {
            BotsConfig::load(workspace)
                .ok()
                .and_then(|c| c.bots.first().map(|b| b.slug.clone()))
                .map(|slug| bot_dir(workspace, &slug).join(".thclaws/settings.json"))
                .and_then(model_in)
        })
        .or_else(|| crate::config::AppConfig::load().ok().map(|c| c.model));
    let Some(current) = current else {
        return;
    };
    let path = dest.join(".thclaws/settings.json");
    let mut base = std::fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(obj) = base.as_object_mut() else {
        return;
    };
    let pinned = obj
        .get("model")
        .and_then(|m| m.as_str())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    let gateway = obj.get("gatewayProxy").and_then(|v| v.as_bool()) == Some(true);
    if !takes_workspace_model(pinned.as_deref(), &current, |m| model_reachable(m, gateway)) {
        return;
    }
    obj.insert("model".into(), serde_json::json!(current));
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if let Ok(body) = serde_json::to_string_pretty(obj) {
        let _ = std::fs::write(&path, body);
    }
}

/// Whether a new bot should take the workspace's `current` model: always
/// when the bundle pins none, never when its pin is reachable here.
fn takes_workspace_model(
    pinned: Option<&str>,
    current: &str,
    reachable: impl Fn(&str) -> bool,
) -> bool {
    match pinned {
        None => true,
        Some(p) if p == current => false,
        Some(p) => !reachable(p),
    }
}

/// Can this install send `model` a request? The host's config, with the
/// gateway routes the bot was just given — the host root a migration left
/// minimal carries no `gatewayProxy`, so its own reading would say BYOK.
/// An unreadable config keeps the pin: not knowing is not "unreachable".
fn model_reachable(model: &str, gateway: bool) -> bool {
    let Some(kind) = crate::providers::ProviderKind::detect(model) else {
        return false;
    };
    let Ok(mut cfg) = crate::config::AppConfig::load() else {
        return true;
    };
    if gateway && cfg.gateway_use_for.is_empty() {
        cfg.gateway_use_for = crate::shared::gateway_routed_providers();
    }
    crate::providers::kind_is_reachable(&cfg, kind)
}

/// Add `slug` to `.thclaws/bots.json`. `true` when it was not already there.
pub fn register(workspace: &Path, slug: &str) -> Result<bool> {
    super::validate_slug(slug)?;
    let path = workspace.join(CONFIG_REL);
    let mut cfg = BotsConfig::load(workspace)?;
    if cfg.bots.iter().any(|b| b.slug == slug) {
        return Ok(false);
    }
    cfg.bots.push(BotDef {
        slug: slug.to_string(),
        name: None,
    });
    std::fs::write(&path, serde_json::to_string_pretty(&cfg)?)?;
    Ok(true)
}

/// Can `slug` be removed at all? Separated from [`deregister`] so a caller
/// that has to stop a process first can find out BEFORE stopping it — the
/// first cut stopped the bot and then refused, leaving a live host holding a
/// dead child.
pub fn can_deregister(workspace: &Path, slug: &str) -> Result<bool> {
    let cfg = BotsConfig::load(workspace)?;
    if !cfg.bots.iter().any(|b| b.slug == slug) {
        return Ok(false);
    }
    if cfg.bots.len() == 1 {
        return Err(Error::Config(format!(
            "'{slug}' is the only agent in this workspace, and a workspace always has at least one \
             — install another before removing it."
        )));
    }
    Ok(true)
}

/// Remove `slug` from `.thclaws/bots.json`. The folder is left on disk unless
/// `purge`: a bot's folder holds its sessions, its KMS and its browser
/// logins, so deleting it is a separate decision from "stop listing it".
pub fn deregister(workspace: &Path, slug: &str, purge: bool) -> Result<bool> {
    if !can_deregister(workspace, slug)? {
        return Ok(false);
    }
    let path = workspace.join(CONFIG_REL);
    let mut cfg = BotsConfig::load(workspace)?;
    cfg.bots.retain(|b| b.slug != slug);
    std::fs::write(&path, serde_json::to_string_pretty(&cfg)?)?;
    if purge {
        let dir = bot_dir(workspace, slug);
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir)?;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v3_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        super::super::migrate::mint_new_workspace(dir.path(), "main").unwrap();
        dir
    }

    #[test]
    fn a_blank_bot_is_an_empty_listed_folder() {
        let ws = v3_workspace();
        let made = create_blank(ws.path(), "scratch").unwrap();
        assert!(made.newly_registered);
        assert_eq!(made.dir, bot_dir(ws.path(), "scratch"));
        assert!(made.dir.is_dir());
        // No agent: nothing the catalogue would have put there.
        assert!(!made.dir.join("AGENTS.md").exists());
        assert!(!made.dir.join("manifest.json").exists());
        // …but the same documented settings file a new folder gets.
        let settings = std::fs::read_to_string(made.dir.join(".thclaws/settings.json")).unwrap();
        assert!(settings.contains("\"_doc\""), "the full first-run template");
        serde_json::from_str::<serde_json::Value>(&settings).expect("valid JSON");
        let cfg = BotsConfig::load(ws.path()).unwrap();
        assert_eq!(
            cfg.bots.iter().map(|b| b.slug.as_str()).collect::<Vec<_>>(),
            vec!["main", "scratch"]
        );
    }

    #[test]
    fn a_new_bot_runs_on_the_model_the_workspace_is_using() {
        let ws = v3_workspace();
        let main = bot_dir(ws.path(), "main");
        std::fs::create_dir_all(main.join(".thclaws")).unwrap();
        std::fs::write(
            main.join(".thclaws/settings.json"),
            r#"{"model":"sis/qwen3.8-flash"}"#,
        )
        .unwrap();
        let model_of = |dir: &Path| -> serde_json::Value {
            serde_json::from_str::<serde_json::Value>(
                &std::fs::read_to_string(dir.join(".thclaws/settings.json")).unwrap(),
            )
            .unwrap()["model"]
                .clone()
        };

        // The blank template says `"model": null` — no choice in it.
        let blank = create_blank(ws.path(), "scratch").unwrap();
        assert_eq!(model_of(&blank.dir), "sis/qwen3.8-flash");

        // A bundle with no pin, and one whose pin is the same model.
        for (slug, body) in [
            ("plain", r#"{"x":1}"#),
            ("same", r#"{"model":"sis/qwen3.8-flash"}"#),
        ] {
            let dir = bot_dir(ws.path(), slug);
            std::fs::create_dir_all(dir.join(".thclaws")).unwrap();
            std::fs::write(dir.join(".thclaws/settings.json"), body).unwrap();
            seed_workspace_model(ws.path(), &dir);
            assert_eq!(model_of(&dir), "sis/qwen3.8-flash", "{slug}");
        }
    }

    #[test]
    fn a_reachable_pin_is_kept_and_an_unreachable_one_is_not() {
        let reach = |m: &str| m == "deepseek-v4-pro";
        assert!(takes_workspace_model(None, "sis/qwen3.8-flash", reach));
        assert!(!takes_workspace_model(
            Some("deepseek-v4-pro"),
            "sis/qwen3.8-flash",
            reach
        ));
        assert!(takes_workspace_model(
            Some("claude-opus-4-1"),
            "sis/qwen3.8-flash",
            reach
        ));
        assert!(!takes_workspace_model(
            Some("sis/qwen3.8-flash"),
            "sis/qwen3.8-flash",
            |_| false
        ));
    }

    fn write_settings(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn proxy_of(dir: &Path) -> Option<bool> {
        let raw = std::fs::read(dir.join(".thclaws/settings.json")).ok()?;
        serde_json::from_slice::<serde_json::Value>(&raw)
            .ok()?
            .get("gatewayProxy")
            .and_then(|b| b.as_bool())
    }

    /// A hosted runner writes only the legacy `gatewayUseFor` list, on the
    /// host and on every agent. An agent added there has to open on the
    /// gateway, or its first turn fails on a placeholder key.
    #[test]
    fn an_agent_added_to_a_hosted_workspace_gets_the_gateway() {
        let ws = v3_workspace();
        write_settings(
            &ws.path().join(".thclaws/settings.json"),
            r#"{"workspaceVersion":3,"gatewayUseFor":["anthropic","openai"]}"#,
        );
        let made = create_blank(ws.path(), "scratch").unwrap();
        assert_eq!(proxy_of(&made.dir), Some(true));
    }

    /// The first start after an upgrade: the host's settings are the minimal
    /// file the migration wrote, and the list lives in `main`.
    #[test]
    fn the_gateway_list_in_main_counts_when_the_host_has_none() {
        let ws = v3_workspace();
        write_settings(
            &ws.path().join(".thclaws/settings.json"),
            r#"{"workspaceVersion":3}"#,
        );
        write_settings(
            &bot_dir(ws.path(), "main").join(".thclaws/settings.json"),
            r#"{"gatewayUseFor":["deepseek"]}"#,
        );
        let made = create_blank(ws.path(), "helper").unwrap();
        assert_eq!(proxy_of(&made.dir), Some(true));
    }

    /// An explicit `gatewayProxy: false` is a choice, and it beats a list.
    #[test]
    fn an_explicit_byok_choice_on_the_host_is_not_overridden_by_a_list() {
        let ws = v3_workspace();
        write_settings(
            &ws.path().join(".thclaws/settings.json"),
            r#"{"gatewayProxy":false,"gatewayUseFor":["anthropic"]}"#,
        );
        let made = create_blank(ws.path(), "byok").unwrap();
        assert_ne!(proxy_of(&made.dir), Some(true));
    }

    #[test]
    fn a_blank_bot_never_takes_over_an_existing_folder() {
        let ws = v3_workspace();
        let dir = bot_dir(ws.path(), "old");
        std::fs::create_dir_all(dir.join(".thclaws/sessions")).unwrap();
        std::fs::write(dir.join(".thclaws/sessions/s.jsonl"), "{}").unwrap();
        let err = create_blank(ws.path(), "old").unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
        assert!(
            dir.join(".thclaws/sessions/s.jsonl").exists(),
            "left untouched"
        );
        assert!(!BotsConfig::load(ws.path())
            .unwrap()
            .bots
            .iter()
            .any(|b| b.slug == "old"));
        // A listed slug is taken too, folder or not.
        assert!(create_blank(ws.path(), "main").is_err());
        // And the slug rules still apply.
        assert!(create_blank(ws.path(), "../up").is_err());
    }

    #[test]
    fn a_blank_bot_needs_a_multi_bot_workspace() {
        let plain = tempfile::tempdir().unwrap();
        let err = create_blank(plain.path(), "scratch")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a workspace that holds agents"), "{err}");
        assert!(!plain.path().join(".thclaws").exists());
    }

    #[test]
    fn register_is_idempotent_and_ordered() {
        let ws = v3_workspace();
        assert!(register(ws.path(), "research").unwrap());
        assert!(!register(ws.path(), "research").unwrap());
        let cfg = BotsConfig::load(ws.path()).unwrap();
        assert_eq!(
            cfg.bots.iter().map(|b| b.slug.as_str()).collect::<Vec<_>>(),
            vec!["main", "research"]
        );
    }

    #[test]
    fn register_refuses_a_slug_that_is_not_a_folder_name() {
        let ws = v3_workspace();
        assert!(register(ws.path(), "../escape").is_err());
        assert!(register(ws.path(), "a/b").is_err());
    }

    /// A bot's folder holds its sessions, KMS and browser logins, so removing
    /// it from the list and deleting it are two different decisions.
    #[test]
    fn deregister_keeps_the_folder_unless_purge() {
        let ws = v3_workspace();
        register(ws.path(), "research").unwrap();
        let dir = bot_dir(ws.path(), "research");
        std::fs::create_dir_all(dir.join(".thclaws/state")).unwrap();
        std::fs::write(dir.join(".thclaws/state/sessions.jsonl"), "{}").unwrap();

        assert!(deregister(ws.path(), "research", false).unwrap());
        assert!(dir.exists(), "the folder outlives being delisted");
        assert!(!deregister(ws.path(), "research", false).unwrap());

        register(ws.path(), "research").unwrap();
        assert!(deregister(ws.path(), "research", true).unwrap());
        assert!(!dir.exists());
    }

    /// Every workspace has a host and at least one bot; removing the last one
    /// would leave a host with nothing to supervise.
    #[test]
    fn the_last_bot_cannot_be_removed() {
        let ws = v3_workspace();
        let err = deregister(ws.path(), "main", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("only agent"), "{err}");
        assert_eq!(BotsConfig::load(ws.path()).unwrap().bots.len(), 1);
    }

    /// A download that never lands must leave the shelf exactly as it was —
    /// an empty folder there is a bot as far as the host UI is concerned.
    #[tokio::test]
    async fn a_failed_first_install_leaves_no_folder_behind() {
        let ws = v3_workspace();
        // No token configured, so the download refuses before any bytes move.
        let _g = crate::kms::test_env_lock();
        let prev = std::env::var("THCLAWS_CLOUD_TOKEN").ok();
        std::env::set_var("THCLAWS_CLOUD_TOKEN", "");
        std::env::set_var("THCLAWS_DISABLE_KEYCHAIN", "1");

        let before: Vec<_> = std::fs::read_dir(ws.path().join(super::super::SHELF_REL))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        let _ = install(ws.path(), "no-such-agent", None, false).await;
        let after: Vec<_> = std::fs::read_dir(ws.path().join(super::super::SHELF_REL))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        assert_eq!(before.len(), after.len(), "{before:?} -> {after:?}");
        assert_eq!(
            BotsConfig::load(ws.path()).unwrap().bots.len(),
            1,
            "a failed install must not register a bot"
        );

        match prev {
            Some(v) => std::env::set_var("THCLAWS_CLOUD_TOKEN", v),
            None => std::env::remove_var("THCLAWS_CLOUD_TOKEN"),
        }
    }

    /// A bot installed into a workspace that routes through the gateway
    /// should not open saying "no API key". The credential is user-level and
    /// already reachable; only the routing choice was missing.
    #[test]
    fn a_new_bot_inherits_the_workspaces_gateway_choice() {
        let ws = v3_workspace();
        std::fs::write(
            ws.path().join(".thclaws/settings.json"),
            r#"{"workspaceVersion":3,"gatewayProxy":true}"#,
        )
        .unwrap();
        let bot = bot_dir(ws.path(), "fresh");
        std::fs::create_dir_all(bot.join(".thclaws")).unwrap();
        std::fs::write(bot.join(".thclaws/settings.json"), r#"{"model":"x"}"#).unwrap();

        seed_workspace_gateway_choice(ws.path(), &bot);
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(bot.join(".thclaws/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["gatewayProxy"], true);
        assert_eq!(v["model"], "x", "the bundle's own keys survive");

        // A bundle that ships its own answer keeps it.
        let pinned = bot_dir(ws.path(), "pinned");
        std::fs::create_dir_all(pinned.join(".thclaws")).unwrap();
        std::fs::write(
            pinned.join(".thclaws/settings.json"),
            r#"{"gatewayProxy":false}"#,
        )
        .unwrap();
        seed_workspace_gateway_choice(ws.path(), &pinned);
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(pinned.join(".thclaws/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["gatewayProxy"], false);

        // The catalogue's hello-world ships `"gatewayUseFor": null` — a key
        // with no choice in it. It must still be seeded.
        let nullish = bot_dir(ws.path(), "nullish");
        std::fs::create_dir_all(nullish.join(".thclaws")).unwrap();
        std::fs::write(
            nullish.join(".thclaws/settings.json"),
            r#"{"gatewayUseFor":null,"model":"m"}"#,
        )
        .unwrap();
        seed_workspace_gateway_choice(ws.path(), &nullish);
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(nullish.join(".thclaws/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["gatewayProxy"], true, "a null key is not a decision");

        // The migrated shape: the host root is minimal and the flag lives in
        // bots[0]. This is the workspace that reported the bug, and the first
        // cut of the seed read only the host and never fired for it.
        let mig = v3_workspace();
        std::fs::write(
            mig.path().join(".thclaws/settings.json"),
            r#"{"workspaceVersion":3}"#,
        )
        .unwrap();
        let main = bot_dir(mig.path(), "main");
        std::fs::create_dir_all(main.join(".thclaws")).unwrap();
        std::fs::write(
            main.join(".thclaws/settings.json"),
            r#"{"workspaceVersion":2,"gatewayProxy":true}"#,
        )
        .unwrap();
        let newbie = bot_dir(mig.path(), "newbie");
        std::fs::create_dir_all(newbie.join(".thclaws")).unwrap();
        std::fs::write(newbie.join(".thclaws/settings.json"), "{}").unwrap();
        seed_workspace_gateway_choice(mig.path(), &newbie);
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(newbie.join(".thclaws/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["gatewayProxy"], true, "must follow the original agent");

        // A workspace that is not on the gateway seeds nothing.
        let off = v3_workspace();
        let b = bot_dir(off.path(), "b");
        std::fs::create_dir_all(b.join(".thclaws")).unwrap();
        std::fs::write(b.join(".thclaws/settings.json"), "{}").unwrap();
        seed_workspace_gateway_choice(off.path(), &b);
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(b.join(".thclaws/settings.json")).unwrap(),
        )
        .unwrap();
        assert!(v.get("gatewayProxy").is_none());
    }

    #[tokio::test]
    async fn install_refuses_a_workspace_that_is_not_v3() {
        let dir = tempfile::tempdir().unwrap();
        let err = install(dir.path(), "demo", None, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a workspace that holds agents"), "{err}");
    }
}
