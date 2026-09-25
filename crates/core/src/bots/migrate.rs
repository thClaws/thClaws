//! dev-plan/59 Step 4: workspace v2 → v3.
//!
//! **v2:** a workspace IS an agent — `<ws>/.thclaws/` holds its identity and
//! state, `<ws>/` holds its files.
//! **v3:** `<ws>/.thclaws/bots/main/` is that agent, its files included, and
//! `<ws>/.thclaws/` belongs to the host.
//!
//! The user's working files move too, for continuity rather than security: a
//! workspace today is one place — the agent, its files, its history — and
//! leaving the files at the root would hand the user an agent living
//! somewhere other than its files. Not moving does not avoid a migration; it
//! produces one the user has to notice.
//!
//! The destination lives inside one of the things being moved, so the move
//! cannot be done in place. Everything goes to a staging directory beside
//! `.thclaws/` first, and a marker file records which phase we are in — a
//! half-migrated workspace holds the user's entire tree, so being resumable
//! is not optional.
//!
//! This IS armed, and has been since the desktop learned to run as a host.
//! `bin/app.rs` calls [`auto_migrate_if_v2`] when the desktop or `--serve`
//! opens a v2 folder, so opening one upgrades it in place. Two things hold it
//! back: [`auto_migrate_allowed`], which keeps a container from deciding on
//! its own unless `THCLAWS_AUTO_MIGRATE=1` opts it in, and — dev-plan/63 —
//! the folder picker, since a desktop started from an icon is standing in
//! whatever folder the OS chose and must not upgrade it. `thclaws bots
//! migrate` is the manual caller.

use super::{bot_dir, BotDef, BotsConfig};
use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// The layout version this migration produces. Deliberately NOT
/// `ProjectConfig::CURRENT_WORKSPACE_VERSION`: bumping that would make the
/// v1→v2 pass treat every v2 workspace as stale, move nothing, and stamp it
/// v3 — marking workspaces migrated that never were.
pub const HOST_WORKSPACE_VERSION: u32 = 4;

/// dev-plan/61: the layout versions of a workspace with a host.
/// - 3 — v0.126.0/v0.127.0: the user's files were moved into
///   `.thclaws/bots/main/`, hidden from Finder and from every other agent.
/// - 4 — the files stay at the root, which every agent shares; an agent folder
///   holds only the agent (settings, sessions, memory, identity).
///
/// Any version from 3 up has a host, so "is this already a host workspace"
/// reads [`FIRST_HOST_VERSION`]; only v3 needs its files put back.
pub const FIRST_HOST_VERSION: u32 = 3;

/// The bot a migrated workspace's agent becomes.
pub const MAIN_SLUG: &str = "main";

const STAGING: &str = ".thclaws-v3-migration";
const MARKER: &str = ".thclaws-v3-migration.marker";
/// Where `unmigrate` puts the host's `.thclaws/` — its agent list, state and
/// settings — instead of deleting it.
pub const HOST_BACKUP: &str = ".thclaws-v3-host";
/// Where `unnest` puts the bare outer host it promotes the inner tree out of.
pub const NESTED_BACKUP: &str = ".thclaws-nested-host";

/// dev-plan/61: what makes a folder an agent. The upgrade moves only these into
/// `.thclaws/bots/main/`; everything else is the user's and stays at the root,
/// where every agent reads and writes it.
const AGENT_ENTRIES: &[&str] = &[".thclaws", "AGENTS.md", "CLAUDE.md", "manifest.json"];

/// What an agent folder keeps when files the old upgrade moved are put back.
/// `.home` is the agent's own HOME under a host.
const AGENT_KEEPS: &[&str] = &[
    ".thclaws",
    ".home",
    "AGENTS.md",
    "CLAUDE.md",
    "manifest.json",
];

/// Hosted runners mount the workspace PVC twice — whole at `/workspace`, and
/// again at the engine's `$HOME` via `subPath: .home`. It is user
/// credentials, not agent content, and it is host-level: moving it into
/// `bots/main/` would break the second mount.
const KEEP_AT_ROOT: &[&str] = &[".home"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Moving root entries into staging.
    Collect,
    /// Staging into `.thclaws/bots/main/`.
    Install,
    /// Host files, identity, stored paths.
    Finalise,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Collect => "collect",
            Phase::Install => "install",
            Phase::Finalise => "finalise",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "collect" => Some(Phase::Collect),
            "install" => Some(Phase::Install),
            "finalise" => Some(Phase::Finalise),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// A v2 workspace ready to migrate.
    Migrate,
    /// A migration was interrupted; re-running continues from `Phase`.
    Resume(Phase),
    /// Already v3 — nothing to do.
    AlreadyV3,
}

#[derive(Debug)]
pub struct Plan {
    pub workspace: PathBuf,
    pub status: Status,
    /// Root entries that move into `.thclaws/bots/main/`.
    pub moves: Vec<String>,
    /// Root entries that stay where they are.
    pub keeps: Vec<String>,
    /// Schedule ids whose stored `cwd` points into this workspace and will be
    /// rewritten.
    pub schedules: Vec<String>,
    /// `true` when the workspace holds a git repository, which moves with
    /// everything else — the one thing a user is most likely to have another
    /// window open on.
    pub moves_git: bool,
}

impl Plan {
    pub fn is_noop(&self) -> bool {
        self.status == Status::AlreadyV3
    }
}

/// Read `workspaceVersion` straight from a `settings.json`, never through the
/// merged `AppConfig` — a user-level key would otherwise make every project
/// look migrated.
fn raw_workspace_version(settings: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(settings).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("workspaceVersion")?.as_u64().map(|n| n as u32)
}

fn marker_path(workspace: &Path) -> PathBuf {
    workspace.join(MARKER)
}

fn read_phase(workspace: &Path) -> Option<Phase> {
    let raw = std::fs::read_to_string(marker_path(workspace)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Phase::parse(v.get("phase")?.as_str()?)
}

fn write_phase(workspace: &Path, phase: Phase) -> Result<()> {
    let body = serde_json::json!({
        "marker": "thclaws-workspace-v3-migration",
        "phase": phase.as_str(),
        "workspace": workspace.display().to_string(),
        "note": "A workspace migration is in progress. Re-run `thclaws bots migrate` to finish it. Do not delete this file or the .thclaws-v3-migration/ folder by hand.",
    });
    std::fs::write(marker_path(workspace), serde_json::to_string_pretty(&body)?)?;
    Ok(())
}

/// Root entries this migration would move, in a stable order.
fn movable_entries(workspace: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(workspace)? {
        let name = entry?.file_name().to_string_lossy().to_string();
        if name == STAGING
            || name == MARKER
            || name == HOST_BACKUP
            || KEEP_AT_ROOT.contains(&name.as_str())
        {
            continue;
        }
        // dev-plan/61: the user's files stay at the root.
        if !AGENT_ENTRIES.contains(&name.as_str()) {
            continue;
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

/// Schedule ids whose `cwd` is inside `workspace`. User-level store, so only
/// entries belonging to THIS workspace may be touched.
fn schedules_under(workspace: &Path) -> Vec<String> {
    let Ok(store) = crate::schedule::ScheduleStore::load() else {
        return Vec::new();
    };
    store
        .schedules
        .iter()
        .filter(|s| s.cwd.starts_with(workspace))
        .map(|s| s.id.clone())
        .collect()
}

/// `<…>/.thclaws/bots/<this>` — the same shape `context::is_bot_shelf`
/// recognises. Any directory merely named `bots` (`~/projects/bots/foo`) is an
/// ordinary workspace.
pub fn is_inside_shelf(dir: &Path) -> bool {
    let parent = dir.parent();
    parent
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "bots")
        && parent
            .and_then(|p| p.parent())
            .and_then(|g| g.file_name())
            .is_some_and(|n| n == ".thclaws")
}

/// Directories that are never a workspace, whatever they contain: the home
/// directory, anything above it, the filesystem root. The classic desktop
/// ran its engine in the cwd it was launched from — the home directory,
/// from the Dock — before the folder picker was answered, so `~/.thclaws/`
/// exists on many machines and `~` looks exactly like a v2 agent. Migrating
/// it would move the whole home directory into `~/.thclaws/bots/main/`.
pub fn is_never_a_workspace(dir: &Path) -> bool {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    if dir.parent().is_none() {
        return true;
    }
    match crate::util::home_dir().and_then(|h| h.canonicalize().ok()) {
        Some(home) => home.starts_with(&dir),
        None => false,
    }
}

/// Whether the desktop should open `dir` as a workspace host: it is v3
/// already, or a v2 agent that auto-migration may upgrade. The folder
/// picker hands such a folder to a fresh process instead of pointing the
/// in-process engine at it.
pub fn opens_as_host(dir: &Path) -> bool {
    dir.join(super::CONFIG_REL).exists()
        || (auto_migrate_allowed()
            && !is_inside_shelf(dir)
            && !is_never_a_workspace(dir)
            && looks_like_v2_agent(dir))
}

/// `THCLAWS_AUTO_MIGRATE=0` turns auto-migration off anywhere; a supervised
/// child is inside a shelf and never asks. A container does not do it on its
/// own, but `THCLAWS_AUTO_MIGRATE=1` set explicitly lets it (dev-plan/60 4.3):
/// the cloud opts in one workspace at a time.
pub fn auto_migrate_allowed() -> bool {
    let flag = std::env::var("THCLAWS_AUTO_MIGRATE").ok();
    if flag.as_deref() == Some("0")
        || std::env::var("THCLAWS_SUPERVISED").ok().as_deref() == Some("1")
    {
        return false;
    }
    std::env::var("THCLAWS_INSIDE_DOCKER").ok().as_deref() != Some("1")
        || flag.as_deref() == Some("1")
}

pub fn plan(workspace: &Path) -> Result<Plan> {
    if crate::workdir::is_multiuser() {
        return Err(Error::Config(
            "workspace migration is not defined for a multiuser pod — its `.thclaws/` is shared \
             across tenants and uses a different per-user layout"
                .into(),
        ));
    }
    if !workspace.is_dir() {
        return Err(Error::Config(format!(
            "{} is not a directory",
            workspace.display()
        )));
    }
    // Migrating a bot would nest a shelf inside a shelf. The check is on the
    // path rather than on content because a bot folder looks exactly like the
    // workspace it came from.
    if is_inside_shelf(workspace) {
        return Err(Error::Config(format!(
            "{} is already an agent inside a workspace — migrate the workspace above it, not \
             the agent",
            workspace.display()
        )));
    }
    if is_never_a_workspace(workspace) {
        return Err(Error::Config(format!(
            "{} is the home directory (or above it) — a workspace is a project folder, and \
             migrating this would move everything under it",
            workspace.display()
        )));
    }

    let resume = read_phase(workspace);
    let host_settings = workspace.join(".thclaws/settings.json");
    let version = raw_workspace_version(&host_settings).unwrap_or(0);
    let shelf = workspace.join(super::SHELF_REL);

    let status = match resume {
        Some(phase) => Status::Resume(phase),
        None if version >= FIRST_HOST_VERSION && shelf.is_dir() => Status::AlreadyV3,
        None if version >= FIRST_HOST_VERSION => {
            return Err(Error::Config(format!(
                "{} declares workspaceVersion {version} but has no {} — its agents are missing, and \
                 migrating again would move the host's own tree into a new one. Restore the shelf \
                 or fix the version by hand.",
                workspace.display(),
                super::SHELF_REL
            )));
        }
        None if shelf.is_dir() => {
            return Err(Error::Config(format!(
                "{} already has a {} folder but declares workspaceVersion {version} — refusing to \
                 migrate a tree that is neither v2 nor v3. Move the shelf aside and re-run.",
                workspace.display(),
                super::SHELF_REL
            )));
        }
        None if marker_path(workspace).exists() => {
            return Err(Error::Config(format!(
                "{} exists but could not be read as a migration marker — resolve it by hand",
                marker_path(workspace).display()
            )));
        }
        None if workspace.join(STAGING).exists() => {
            return Err(Error::Config(format!(
                "{} exists without a migration marker — an earlier migration was interrupted and \
                 its marker was removed. Move that folder aside and re-run.",
                workspace.join(STAGING).display()
            )));
        }
        None => Status::Migrate,
    };

    let moves = if status == Status::AlreadyV3 {
        Vec::new()
    } else {
        movable_entries(workspace)?
    };
    let keeps = KEEP_AT_ROOT
        .iter()
        .filter(|n| workspace.join(n).exists())
        .map(|n| n.to_string())
        .collect();

    Ok(Plan {
        moves_git: moves.iter().any(|n| n == ".git")
            || workspace.join(STAGING).join(".git").exists(),
        workspace: workspace.to_path_buf(),
        status,
        moves,
        keeps,
        schedules: schedules_under(workspace),
    })
}

#[derive(Debug, Default)]
pub struct Report {
    pub moved: usize,
    pub bot_dir: PathBuf,
    pub minted_identity: bool,
    pub rewritten_schedules: Vec<String>,
    /// Paths in the agent's own text pointed back at its folder — see
    /// [`super::agent_paths`].
    pub fixed_paths: usize,
}

pub fn apply(plan: &Plan) -> Result<Report> {
    // The same lock a host holds, so a migration cannot move a tree a host
    // is serving — the gap §7.4 recorded as "nothing can detect one".
    let _lock = super::lock_workspace(&plan.workspace, "a migration")?;
    apply_locked(plan)
}

/// What happens when a v2 workspace is opened by a surface that migrates on
/// its own (dev-plan/59 §7.8).
#[derive(Debug)]
pub enum AutoOutcome {
    /// Not a v2 agent — nothing to do, and nothing was touched.
    NotV2,
    /// Already v3, possibly because another opener got there first.
    AlreadyV3,
    Migrated(Report, /* moves_git */ bool),
    /// Someone else holds the workspace and did not finish within the wait.
    Busy,
    Failed(String),
}

/// Migrate a v2 workspace the moment it is opened.
///
/// The lock is taken BEFORE the plan is made, and held through the move.
/// `apply` alone plans first and locks second, which is fine for a human
/// at a prompt but not for two openers racing: a plan made against a v2 tree
/// and applied after the other opener finished would sweep the new host root
/// — `.thclaws/` and the tombstone — into a second, nested bot. Under the
/// lock the plan sees whichever shape is true.
pub fn auto_migrate_if_v2(ws: &Path) -> AutoOutcome {
    auto_migrate_with_wait(ws, std::time::Duration::from_secs(5))
}

pub fn auto_migrate_with_wait(ws: &Path, wait: std::time::Duration) -> AutoOutcome {
    // Checked before the v2 test, which reads a v3 tree as "not v2": the
    // caller treats both as nothing-to-do, but the name should be true.
    if ws.join(super::CONFIG_REL).exists() {
        return AutoOutcome::AlreadyV3;
    }
    // A bot's folder looks exactly like a v2 workspace — it IS the workspace
    // that migrated — so the bot's own `--serve` reached this point and
    // logged "could not upgrade" on every start. Inside a shelf is not v2.
    if is_inside_shelf(ws) || is_never_a_workspace(ws) || !looks_like_v2_agent(ws) {
        return AutoOutcome::NotV2;
    }
    let deadline = std::time::Instant::now() + wait;
    let _lock = loop {
        match super::lock_workspace(ws, "the upgrade") {
            Ok(l) => break l,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(_) => return AutoOutcome::Busy,
        }
    };
    let p = match plan(ws) {
        Ok(p) => p,
        Err(e) => return AutoOutcome::Failed(e.to_string()),
    };
    if p.is_noop() {
        return AutoOutcome::AlreadyV3;
    }
    match apply_locked(&p) {
        Ok(r) => AutoOutcome::Migrated(r, p.moves_git),
        Err(e) => AutoOutcome::Failed(e.to_string()),
    }
}

fn apply_locked(plan: &Plan) -> Result<Report> {
    let ws = &plan.workspace;
    if plan.is_noop() {
        return Ok(Report {
            bot_dir: bot_dir(ws, MAIN_SLUG),
            ..Default::default()
        });
    }
    let start = match plan.status {
        Status::Resume(p) => p,
        _ => Phase::Collect,
    };
    let mut report = Report {
        bot_dir: bot_dir(ws, MAIN_SLUG),
        ..Default::default()
    };

    let staging = ws.join(STAGING);
    if start == Phase::Collect {
        write_phase(ws, Phase::Collect)?;
        std::fs::create_dir_all(&staging)?;
        for name in movable_entries(ws)? {
            let src = ws.join(&name);
            let dst = staging.join(&name);
            if dst.exists() {
                // A previous run moved it; the root copy is the newer one only
                // if the user put it back, which is not a case to guess at.
                continue;
            }
            std::fs::rename(&src, &dst).map_err(|e| {
                Error::Config(format!(
                    "cannot move {} into the migration staging folder: {e}",
                    src.display()
                ))
            })?;
            report.moved += 1;
        }
    }

    if start <= Phase::Install {
        write_phase(ws, Phase::Install)?;
        let dest = bot_dir(ws, MAIN_SLUG);
        std::fs::create_dir_all(ws.join(super::SHELF_REL))?;
        if staging.exists() {
            if dest.exists() {
                // Resumed after a partial install: merge what is left rather
                // than clobbering what already landed.
                for name in std::fs::read_dir(&staging)?
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name())
                {
                    let src = staging.join(&name);
                    let dst = dest.join(&name);
                    if !dst.exists() {
                        std::fs::rename(&src, &dst)?;
                    }
                }
                let _ = std::fs::remove_dir(&staging);
            } else {
                std::fs::rename(&staging, &dest).map_err(|e| {
                    Error::Config(format!(
                        "cannot install the staged workspace at {}: {e}",
                        dest.display()
                    ))
                })?;
            }
        }
    }

    write_phase(ws, Phase::Finalise)?;
    install_host_files(ws)?;
    report.minted_identity = mint_identity(ws)?;
    // The agent arrives with the `settings.json` the workspace had, which is
    // often two keys, while an agent added by hand gets the full documented
    // template. `ensure_default_exists_in` cannot close that gap — it returns
    // early because the file exists — so fill in what it is missing. Runs
    // after `mint_identity`, which writes the file even when there was none.
    let _ = crate::config::ProjectConfig::backfill_defaults_in(&bot_dir(ws, MAIN_SLUG));
    report.rewritten_schedules = rewrite_schedules(ws)?;
    // finding 11: the move just made every `.thclaws/…` the agent runs point
    // at the host's folder instead of its own. Repairing it here, in the pass
    // that broke it, is the difference between a workspace that keeps working
    // and one that opens to an empty panel with nothing to explain it.
    report.fixed_paths = super::agent_paths::fix(&bot_dir(ws, MAIN_SLUG), false)
        .map(|r| r.commands + r.literals)
        .unwrap_or(0);
    let _ = std::fs::remove_file(marker_path(ws));
    Ok(report)
}

#[derive(Debug, Default)]
pub struct UnmigrateReport {
    pub moved: usize,
    /// The host's `.thclaws/`, kept rather than deleted.
    pub host_backup: PathBuf,
    pub rewritten_schedules: Vec<String>,
}

/// Root entries a migrated workspace may hold besides the host's own tree.
fn host_root_entry_ok(ws: &Path, name: &str) -> bool {
    match name {
        ".thclaws" | ".DS_Store" | HOST_BACKUP => true,
        "AGENTS.md" => std::fs::read_to_string(ws.join(name)).is_ok_and(|s| s == TOMBSTONE),
        n => KEEP_AT_ROOT.contains(&n),
    }
}

/// dev-plan/60 4.3: v3 → v2, for one workspace that misbehaves after the
/// upgrade. Renames only — the agent at `.thclaws/bots/main/` goes back to the
/// root, and the host's `.thclaws/` is kept at [`HOST_BACKUP`]. Refused unless
/// `main` is the only agent: a second agent has nowhere to go in v2.
///
/// Resumable: interrupted after the host tree was set aside, a re-run finds
/// the agent inside the backup and finishes the move.
/// finding 11: an agent installed before agents learned `$THCLAWS_AGENT_DIR`.
///
/// It ships assets under its own `.thclaws/` and reaches them by a bare
/// relative path, which under a workspace host lands in the HOST's folder
/// instead. Nothing errors: the agent reports an empty project, so a GUI shell
/// draws its chrome around no data and the user sees a blank panel.
///
/// The signal is the convention's own name. An agent that ships anything and
/// never mentions the variable predates it. Only the bundled directories are
/// scanned — sessions and state can be large and say nothing about this.
pub fn stale_agents(root: &Path) -> Vec<(String, Option<String>)> {
    const BUNDLED: [&str; 5] = [
        "scripts",
        "skills",
        "workflows",
        "agent_workflow",
        "gui-shell",
    ];
    let Ok(cfg) = BotsConfig::load(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for def in cfg.bots {
        let dir = super::resolve_agent_dir(root, &def.slug);
        let dirs: Vec<std::path::PathBuf> = BUNDLED
            .iter()
            .map(|d| dir.join(".thclaws").join(d))
            .filter(|d| d.is_dir())
            .collect();
        if dirs.is_empty() {
            continue;
        }
        let mentions = dirs.iter().any(|d| {
            walkdir::WalkDir::new(d)
                .max_depth(6)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .filter(|e| e.metadata().map(|m| m.len() < 512 * 1024).unwrap_or(false))
                .any(|e| {
                    std::fs::read_to_string(e.path())
                        .map(|t| t.contains("THCLAWS_AGENT_DIR"))
                        .unwrap_or(false)
                })
        });
        if mentions {
            continue;
        }
        let id = std::fs::read_to_string(dir.join("manifest.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("id").and_then(|s| s.as_str()).map(|s| s.to_string()));
        out.push((def.slug, id));
    }
    out
}

/// What this workspace needs before its agents can work, said to the PAGE.
///
/// Both of these are written to stderr already, which a desktop launched from
/// its icon never shows. Both fail silently — an agent that cannot reach its
/// own files renders an empty project, not an error — so without this the user
/// gets a blank panel and nothing to act on.
///
/// Only under a host: off one, an agent's folder IS the workspace and neither
/// problem can arise.
pub fn workspace_notice_frames(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Some((slug, dir)) = nested_bot(root) {
        out.push(
            serde_json::json!({
                "type": "workspace_notice",
                "id": format!("nested:{slug}"),
                "title": format!("'{slug}' sits deeper than it needs to"),
                "detail": format!(
                    "An older migration ran twice, so {} is a host of its own and the agent \
                     runs a level below it. thClaws follows that, and nothing is missing — \
                     but the workspace carries an engine process it does not need.",
                    dir.display()
                ),
                "command": "thclaws bots unnest",
            })
            .to_string(),
        );
    }
    for (slug, id) in stale_agents(root) {
        // An agent from the catalogue is replaced by getting it again, which
        // also brings whatever else it has gained since. One the user wrote
        // has nowhere to come from, so its copy is repaired in place.
        let command = id.as_deref().map_or_else(
            || "thclaws bots fix-paths".to_string(),
            |i| format!("/cloud get {i}"),
        );
        out.push(
            serde_json::json!({
                "type": "workspace_notice",
                "id": format!("stale:{slug}"),
                "title": format!("'{slug}' cannot reach the files it ships"),
                "detail": "It was installed before agents learned to find their own folder \
                           under a workspace host, so its scripts and state look for themselves \
                           in the wrong place. It shows an empty project rather than an error.",
                "command": command,
            })
            .to_string(),
        );
    }
    out
}

/// finding 12: an agent folder that is itself a workspace.
///
/// An older migration ran twice. The second pass read a host that carried no
/// version stamp, took it for a v2 workspace, and swallowed the whole tree —
/// the user's files, the host's `.thclaws/` and the real agent inside it —
/// into a new `bots/<slug>/`, leaving a bare host at the root. Nothing is
/// lost, but the host then supervises a folder that is not an agent: no
/// `AGENTS.md`, no gui-shell, none of what the agent ships. It starts, and
/// then does nothing, with no error to explain it.
///
/// The signature is unambiguous: a bot folder does not hold a bots.json,
/// because bots do not nest.
pub fn nested_bot(ws: &Path) -> Option<(String, PathBuf)> {
    let cfg = BotsConfig::load(ws).ok()?;
    let [only] = cfg.bots.as_slice() else {
        return None;
    };
    let dir = bot_dir(ws, &only.slug);
    let nested = dir.join(super::CONFIG_REL).is_file() && dir.join(super::SHELF_REL).is_dir();
    nested.then(|| (only.slug.clone(), dir))
}

#[derive(Debug, Default)]
pub struct UnnestReport {
    pub moved: usize,
    /// The bare outer host, kept rather than deleted — the inner tree came out
    /// of it, so this is the one thing that could still hold something.
    pub host_backup: PathBuf,
    pub rewritten_schedules: Vec<String>,
}

/// Undo a double migration by promoting the inner workspace to be the
/// workspace. The inner tree is already the right shape — it holds the files,
/// `.home`, and a `.thclaws/` with the real settings and the agent under
/// `bots/` — so this only lifts it a level and keeps the bare outer host aside.
///
/// Refuses unless the outer root holds nothing but `.thclaws`, which is what
/// the second pass leaves behind. A root with files of its own is some other
/// shape, and promoting into it would collide.
pub fn unnest(ws: &Path) -> Result<UnnestReport> {
    if crate::workdir::is_multiuser() {
        return Err(Error::Config(
            "workspace migration is not defined for a multiuser pod".into(),
        ));
    }
    if marker_path(ws).exists() {
        return Err(Error::Config(format!(
            "{} is mid-migration — finish it with `thclaws bots migrate` first",
            ws.display()
        )));
    }
    let Some((slug, _)) = nested_bot(ws) else {
        return Err(Error::Config(format!(
            "{} is not a doubly-migrated workspace — its agent folder holds no {}",
            ws.display(),
            super::CONFIG_REL
        )));
    };
    let strays: Vec<String> = std::fs::read_dir(ws)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n != ".thclaws" && n != ".DS_Store")
        .collect();
    if !strays.is_empty() {
        return Err(Error::Config(format!(
            "{} holds {} at its root as well as a nested workspace — promoting the inner tree              would collide with them. Move them aside first.",
            ws.display(),
            strays.join(", ")
        )));
    }
    let backup = ws.join(NESTED_BACKUP);
    if backup.exists() {
        return Err(Error::Config(format!(
            "{} already exists from an earlier un-nesting — move it aside first",
            backup.display()
        )));
    }

    // Held before anything moves; the destination lives inside the source, so
    // the outer host goes aside first and the inner tree comes out of it.
    let _lock = super::lock_workspace(ws, "an un-nesting")?;
    std::fs::rename(ws.join(".thclaws"), &backup)?;
    let inner = backup
        .join(super::SHELF_REL.trim_start_matches(".thclaws/"))
        .join(&slug);
    let mut report = UnnestReport {
        host_backup: backup.clone(),
        ..Default::default()
    };
    for entry in std::fs::read_dir(&inner)? {
        let name = entry?.file_name();
        let dst = ws.join(&name);
        if dst.exists() {
            return Err(Error::Config(format!(
                "cannot promote {}: {} already exists",
                inner.join(&name).display(),
                dst.display()
            )));
        }
        std::fs::rename(inner.join(&name), &dst)?;
        report.moved += 1;
    }
    let _ = std::fs::remove_dir(&inner);
    if slug == MAIN_SLUG {
        report.rewritten_schedules = unrewrite_schedules(ws)?;
    }
    Ok(report)
}

pub fn unmigrate(ws: &Path) -> Result<UnmigrateReport> {
    if crate::workdir::is_multiuser() {
        return Err(Error::Config(
            "workspace migration is not defined for a multiuser pod".into(),
        ));
    }
    if is_inside_shelf(ws) {
        return Err(Error::Config(format!(
            "{} is an agent inside a workspace — run this on the workspace above it",
            ws.display()
        )));
    }
    if marker_path(ws).exists() {
        return Err(Error::Config(format!(
            "{} is mid-migration — finish it with `thclaws bots migrate` first",
            ws.display()
        )));
    }
    let backup = ws.join(HOST_BACKUP);
    let resuming = !ws.join(super::CONFIG_REL).exists()
        && backup
            .join(super::CONFIG_REL.trim_start_matches(".thclaws/"))
            .exists()
        && backup
            .join(super::SHELF_REL.trim_start_matches(".thclaws/"))
            .join(MAIN_SLUG)
            .is_dir();

    // Held on the host tree being set aside; the lock follows the open file.
    let _lock = if resuming {
        None
    } else {
        let cfg = BotsConfig::load(ws)?;
        let slugs: Vec<&str> = cfg.bots.iter().map(|b| b.slug.as_str()).collect();
        if slugs != [MAIN_SLUG] {
            return Err(Error::Config(format!(
                "{} holds the agents {} — only a workspace whose one agent is '{MAIN_SLUG}' can go \
                 back to a single-agent layout. Remove the others first.",
                ws.display(),
                slugs.join(", ")
            )));
        }
        let shelf = ws.join(super::SHELF_REL);
        let extra: Vec<String> = std::fs::read_dir(&shelf)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != MAIN_SLUG && n != ".DS_Store")
            .collect();
        if !extra.is_empty() {
            return Err(Error::Config(format!(
                "{} still has folders for {} — an agent removed from the list keeps its folder. \
                 Move them out of the way first.",
                shelf.display(),
                extra.join(", ")
            )));
        }
        if backup.exists() {
            return Err(Error::Config(format!(
                "{} already exists from an earlier reverse migration — move it aside first",
                backup.display()
            )));
        }
        // The root holds the user's files (dev-plan/61), so only a name the
        // agent folder also holds is a problem: moving it back would overwrite.
        let agent_dir = bot_dir(ws, MAIN_SLUG);
        let clashes: Vec<String> = std::fs::read_dir(&agent_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != ".thclaws" && !KEEP_AT_ROOT.contains(&n.as_str()))
            .filter(|n| ws.join(n).exists() && !host_root_entry_ok(ws, n))
            .collect();
        if !clashes.is_empty() {
            return Err(Error::Config(format!(
                "{} already has {} at its root, and the agent folder holds the same names. \
                 Move one of each aside first.",
                ws.display(),
                clashes.join(", ")
            )));
        }
        Some(super::lock_workspace(ws, "a reverse migration")?)
    };

    if !resuming {
        std::fs::rename(ws.join(".thclaws"), &backup)?;
        let tombstone = ws.join("AGENTS.md");
        if tombstone.exists() {
            std::fs::rename(&tombstone, backup.join("AGENTS.md.tombstone"))?;
        }
    }
    let agent = backup
        .join(super::SHELF_REL.trim_start_matches(".thclaws/"))
        .join(MAIN_SLUG);
    let mut report = UnmigrateReport {
        host_backup: backup.clone(),
        ..Default::default()
    };
    for entry in std::fs::read_dir(&agent)? {
        let name = entry?.file_name();
        let dst = ws.join(&name);
        if dst.exists() {
            // `.home` is the only name both trees can hold; the root's is the
            // one a runner mounts.
            if KEEP_AT_ROOT.contains(&name.to_string_lossy().as_ref()) {
                continue;
            }
            return Err(Error::Config(format!(
                "cannot move {} back: {} already exists",
                agent.join(&name).display(),
                dst.display()
            )));
        }
        std::fs::rename(agent.join(&name), &dst)?;
        report.moved += 1;
    }
    let _ = std::fs::remove_dir(&agent);
    report.rewritten_schedules = unrewrite_schedules(ws)?;
    Ok(report)
}

/// The reverse of [`rewrite_schedules`]: a schedule stored under
/// `.thclaws/bots/main/` points back at the same place under the root.
fn unrewrite_schedules(ws: &Path) -> Result<Vec<String>> {
    let Some(path) = crate::schedule::ScheduleStore::default_path() else {
        return Ok(Vec::new());
    };
    let mut store = crate::schedule::ScheduleStore::load_from(&path)?;
    let from = bot_dir(ws, MAIN_SLUG);
    let mut touched = Vec::new();
    for s in &mut store.schedules {
        let Ok(rel) = s.cwd.strip_prefix(&from) else {
            continue;
        };
        s.cwd = if rel.as_os_str().is_empty() {
            ws.to_path_buf()
        } else {
            ws.join(rel)
        };
        touched.push(s.id.clone());
    }
    if !touched.is_empty() {
        store.save_to(&path)?;
    }
    Ok(touched)
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    /// Entries moved from `.thclaws/bots/main/` back to the workspace root.
    pub moved: Vec<String>,
    /// Entries left in the agent folder because the root already has the name.
    pub clashes: Vec<String>,
    pub removed_tombstone: bool,
    /// The workspace was v3 and is now v4.
    pub stamped_v4: bool,
}

/// dev-plan/61: put back the files the first multi-agent upgrade moved, and
/// mark the workspace v4.
///
/// v0.126.0 and v0.127.0 moved the whole workspace, the user's files included,
/// into `.thclaws/bots/main/`, which hid them from Finder and from every other
/// agent. Everything in that folder that is not the agent itself goes back to
/// the root. Renames only; a name the root already has stays where it is and is
/// reported, never overwritten. Safe to run on every open: a workspace with
/// nothing to put back is untouched. The caller holds the workspace lock.
pub fn restore_shared_files(ws: &Path) -> Result<RestoreReport> {
    let mut report = RestoreReport::default();
    if !ws.join(super::CONFIG_REL).exists() {
        return Ok(report);
    }
    // Only a v3 workspace holds files the old upgrade moved. A v4 agent folder
    // may hold files on purpose, and they stay where they are.
    let host_settings = ws.join(".thclaws/settings.json");
    if raw_workspace_version(&host_settings).unwrap_or(0) != FIRST_HOST_VERSION {
        return Ok(report);
    }
    let main = bot_dir(ws, MAIN_SLUG);
    if !main.is_dir() {
        stamp_host_version(ws, HOST_WORKSPACE_VERSION)?;
        report.stamped_v4 = true;
        return Ok(report);
    }
    let tombstone = ws.join("AGENTS.md");
    if std::fs::read_to_string(&tombstone).is_ok_and(|s| s == TOMBSTONE) {
        std::fs::remove_file(&tombstone)?;
        report.removed_tombstone = true;
    }
    let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(&main)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();
    names.sort();
    for name in names {
        let shown = name.to_string_lossy().to_string();
        if AGENT_KEEPS.contains(&shown.as_str()) {
            continue;
        }
        let dst = ws.join(&name);
        if std::fs::symlink_metadata(&dst).is_ok() {
            report.clashes.push(shown);
            continue;
        }
        std::fs::rename(main.join(&name), &dst).map_err(|e| {
            Error::Config(format!(
                "cannot move {} back to the workspace root: {e}",
                main.join(&name).display()
            ))
        })?;
        report.moved.push(shown);
    }
    // A name the root already had stays in the agent folder and is reported
    // once; putting it back is the user's call, not something to retry on every
    // open.
    stamp_host_version(ws, HOST_WORKSPACE_VERSION)?;
    report.stamped_v4 = true;
    Ok(report)
}

/// Set `workspaceVersion` in the host's settings, keeping every other key.
fn stamp_host_version(ws: &Path, version: u32) -> Result<()> {
    let path = ws.join(".thclaws/settings.json");
    let mut base = std::fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = base.as_object_mut() {
        obj.insert("workspaceVersion".into(), serde_json::json!(version));
        std::fs::write(&path, serde_json::to_string_pretty(obj)?)?;
    }
    Ok(())
}

// Phases run in order, so `start <= Phase::Install` reads naturally.
impl PartialOrd for Phase {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Phase {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        fn rank(p: &Phase) -> u8 {
            match p {
                Phase::Collect => 0,
                Phase::Install => 1,
                Phase::Finalise => 2,
            }
        }
        rank(self).cmp(&rank(other))
    }
}

/// The host's own `.thclaws/`, its bot list, and the tombstone that explains
/// the layout to a binary too old to understand it.
fn install_host_files(ws: &Path) -> Result<()> {
    let thclaws = ws.join(".thclaws");
    std::fs::create_dir_all(thclaws.join("state"))?;
    let gi = thclaws.join("state/.gitignore");
    if !gi.exists() {
        std::fs::write(&gi, "*\n")?;
    }

    let settings = thclaws.join("settings.json");
    let mut base = std::fs::read(&settings)
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = base.as_object_mut() {
        obj.insert(
            "workspaceVersion".into(),
            serde_json::json!(HOST_WORKSPACE_VERSION),
        );
        obj.entry("_doc").or_insert(serde_json::json!(
            "This is the workspace HOST. It runs the agents in .thclaws/bots/. The agent that used to live here is .thclaws/bots/main/."
        ));
        std::fs::write(&settings, serde_json::to_string_pretty(obj)?)?;
    }

    let bots_json = ws.join(super::CONFIG_REL);
    if !bots_json.exists() {
        let cfg = BotsConfig {
            version: 1,
            bots: vec![BotDef {
                slug: MAIN_SLUG.into(),
                name: Some("Main".into()),
            }],
        };
        std::fs::write(&bots_json, serde_json::to_string_pretty(&cfg)?)?;
    }

    // T6: `workspaceVersion` alone is not a stop — an old binary boots
    // happily and scaffolds `state/` into the host tree. AGENTS.md is, because
    // an old binary loads it as project instructions like any other, and the
    // model then explains the situation in the user's own language instead of
    // building on top of what looks like an empty workspace.
    // dev-plan/61: no tombstone at the root any more. The root holds the
    // user's files, and an `AGENTS.md` there loads into every agent through
    // the ancestor walk. An older binary opening the root now finds the user's
    // files where they always were.
    Ok(())
}

const TOMBSTONE: &str = r#"# This workspace has been upgraded — nothing is missing

This folder is now a **workspace host**. It supervises bots; it is not a
project itself. The agent that used to live here — its files, its history,
its settings, its git repository — moved to:

    .thclaws/bots/main/

## If you are a model reading this

You are running a version of thClaws older than the one that upgraded this
workspace. **This directory is not an empty project.** You cannot see or edit
the user's work from here, and anything you create here will land outside
their project.

Do not proceed as if this were a new workspace. Tell the user, in their own
language, that:

1. Their workspace was upgraded to the multi-bot layout and nothing was lost.
2. Their project is at `.thclaws/bots/main/`.
3. Updating thClaws will open it normally again.
4. To keep working without updating, they can open `.thclaws/bots/main/`
   directly as the project folder.
"#;

/// `agent pack` refuses a folder without `agent.{id,name,description}`, so a
/// migrated workspace that never had an identity could not be published.
/// Derived from the workspace folder name — the only name the user has
/// already chosen for this thing — and only when absent.
fn mint_identity(ws: &Path) -> Result<bool> {
    let settings = bot_dir(ws, MAIN_SLUG).join(".thclaws/settings.json");
    let mut base = std::fs::read(&settings)
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(obj) = base.as_object_mut() else {
        return Ok(false);
    };
    let existing = obj.get("agent").and_then(|a| a.as_object()).cloned();
    let filled = |k: &str| {
        existing
            .as_ref()
            .and_then(|a| a.get(k))
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty())
    };
    let (has_id, has_name, has_desc) = (filled("id"), filled("name"), filled("description"));
    if has_id && has_name && has_desc {
        return Ok(false);
    }

    let folder = ws
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| MAIN_SLUG.to_string());
    let mut agent = existing.unwrap_or_default();
    if !has_id {
        agent.insert("id".into(), serde_json::json!(slugify(&folder)));
    }
    if !has_name {
        agent.insert("name".into(), serde_json::json!(folder));
    }
    if !has_desc {
        agent.insert(
            "description".into(),
            serde_json::json!(format!(
                "{folder} — migrated from a single-agent workspace."
            )),
        );
    }
    obj.insert("agent".into(), serde_json::Value::Object(agent));
    if let Some(parent) = settings.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&settings, serde_json::to_string_pretty(obj)?)?;
    Ok(true)
}

/// Catalogue ids are lowercase letters, digits and hyphens.
fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() {
        MAIN_SLUG.to_string()
    } else {
        s.chars().take(64).collect()
    }
}

/// Schedules store an absolute `cwd` and the daemon refuses one that no
/// longer exists, so every entry pointing into this workspace has to follow
/// the agent. The store is user-level: only this workspace's entries may be
/// touched.
fn rewrite_schedules(ws: &Path) -> Result<Vec<String>> {
    let Some(path) = crate::schedule::ScheduleStore::default_path() else {
        return Ok(Vec::new());
    };
    let mut store = crate::schedule::ScheduleStore::load_from(&path)?;
    let dest = bot_dir(ws, MAIN_SLUG);
    let mut touched = Vec::new();
    for s in &mut store.schedules {
        if s.cwd == dest || s.cwd.starts_with(&dest) {
            continue; // already rewritten
        }
        let Ok(rel) = s.cwd.strip_prefix(ws) else {
            continue;
        };
        // A schedule pointing at the workspace root strips to an empty path,
        // and `join("")` would leave a trailing separator in the stored value.
        s.cwd = if rel.as_os_str().is_empty() {
            dest.clone()
        } else {
            dest.join(rel)
        };
        touched.push(s.id.clone());
    }
    if !touched.is_empty() {
        store.save_to(&path)?;
    }
    Ok(touched)
}

/// Mint a fresh v3 workspace: a host with one empty bot. Used when
/// `--supervisor` is pointed at a directory that has no workspace in it yet —
/// never at one that already holds a v2 agent, which needs [`apply`].
pub fn mint_new_workspace(ws: &Path, slug: &str) -> Result<PathBuf> {
    super::validate_slug(slug)?;
    let dest = bot_dir(ws, slug);
    std::fs::create_dir_all(&dest)?;
    std::fs::create_dir_all(ws.join(".thclaws/state"))?;
    let bots_json = ws.join(super::CONFIG_REL);
    if !bots_json.exists() {
        let cfg = BotsConfig {
            version: 1,
            bots: vec![BotDef {
                slug: slug.to_string(),
                name: None,
            }],
        };
        std::fs::write(&bots_json, serde_json::to_string_pretty(&cfg)?)?;
    }
    let settings = ws.join(".thclaws/settings.json");
    if !settings.exists() {
        std::fs::write(
            &settings,
            serde_json::to_string_pretty(&serde_json::json!({
                "workspaceVersion": HOST_WORKSPACE_VERSION,
                "_doc": "This is the workspace HOST. It runs the agents in .thclaws/bots/.",
            }))?,
        )?;
    }
    Ok(dest)
}

/// Does this directory hold a v2 workspace — an agent at its root that a
/// supervisor must not silently step over?
pub fn looks_like_v2_agent(ws: &Path) -> bool {
    if ws.join(super::CONFIG_REL).exists() {
        return false;
    }
    ws.join("AGENTS.md").exists()
        || ws.join("manifest.json").exists()
        || ws.join(".thclaws/settings.json").exists()
        || ws.join(".thclaws/state").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// `~/.thclaws/` exists on most desktops (the classic app ran in the
    /// Dock's cwd before the picker was answered), so `~` passes every
    /// content check. It must never be migrated — automatically or by hand.
    #[test]
    fn home_and_above_are_never_a_workspace() {
        let _g = crate::kms::test_env_lock();
        let fake_home = tempfile::tempdir().unwrap();
        let home = fake_home.path().join("Users").join("someone");
        std::fs::create_dir_all(home.join(".thclaws/state")).unwrap();
        std::fs::write(home.join(".thclaws/settings.json"), "{}").unwrap();
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home);

        assert!(
            looks_like_v2_agent(&home),
            "the shape that made this dangerous"
        );
        assert!(is_never_a_workspace(&home));
        assert!(is_never_a_workspace(home.parent().unwrap()));
        assert!(is_never_a_workspace(Path::new("/")));
        assert!(matches!(
            auto_migrate_with_wait(&home, Duration::from_millis(10)),
            AutoOutcome::NotV2
        ));
        assert!(
            !home.join(".thclaws/bots").exists() && !home.join(STAGING).exists(),
            "nothing moved"
        );
        let err = plan(&home).unwrap_err().to_string();
        assert!(err.contains("home directory"), "{err}");
        assert!(!opens_as_host(&home));

        // A project under the home directory is an ordinary workspace.
        let proj = home.join("projects").join("thing");
        std::fs::create_dir_all(proj.join(".thclaws")).unwrap();
        std::fs::write(proj.join("AGENTS.md"), "# t").unwrap();
        assert!(!is_never_a_workspace(&proj));
        assert!(opens_as_host(&proj));

        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    /// A workspace shaped like a real one: project files, a git repo, the
    /// agent's `.thclaws/` with runtime state in it, and the hosted-runner
    /// `.home/` that must not move.
    fn v2_workspace(name: &str) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join(name);
        let w = |rel: &str, body: &str| {
            let p = ws.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        w("AGENTS.md", "# The agent\n");
        w("manifest.json", r#"{"name":"demo","version":"1.0.0"}"#);
        w("output/report.md", "findings\n");
        w("src/main.py", "print('hi')\n");
        w(".gitignore", "target/\n");
        w(".git/config", "[core]\n");
        w(".git/objects/ab/cdef", "blob");
        w(".thclaws/settings.json", r#"{"workspaceVersion":2}"#);
        w(".thclaws/state/sessions/sess-1.jsonl", "{}\n");
        w(
            ".thclaws/state/browser-profile/Cookies",
            "a real login lives here",
        );
        w(".home/.config/thclaws/settings.json", "{}");
        (root, ws)
    }

    #[test]
    fn a_container_migrates_only_when_told_to() {
        let _g = crate::kms::test_env_lock();
        let keys = [
            "THCLAWS_AUTO_MIGRATE",
            "THCLAWS_INSIDE_DOCKER",
            "THCLAWS_SUPERVISED",
        ];
        let prev: Vec<_> = keys.iter().map(|k| std::env::var(k).ok()).collect();
        let set = |auto: Option<&str>, docker: bool, supervised: bool| {
            match auto {
                Some(v) => std::env::set_var(keys[0], v),
                None => std::env::remove_var(keys[0]),
            }
            if docker {
                std::env::set_var(keys[1], "1")
            } else {
                std::env::remove_var(keys[1])
            }
            if supervised {
                std::env::set_var(keys[2], "1")
            } else {
                std::env::remove_var(keys[2])
            }
        };
        set(None, false, false);
        assert!(auto_migrate_allowed(), "a desktop upgrades on open");
        set(Some("0"), false, false);
        assert!(!auto_migrate_allowed(), "0 turns it off anywhere");
        set(None, true, false);
        assert!(
            !auto_migrate_allowed(),
            "a container does not decide on its own"
        );
        set(Some("1"), true, false);
        assert!(auto_migrate_allowed(), "the cloud opts one workspace in");
        set(Some("1"), true, true);
        assert!(
            !auto_migrate_allowed(),
            "an agent inside a shelf never migrates"
        );
        for (k, v) in keys.iter().zip(prev) {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    /// An upgraded agent must end up with the same first-run file as one added
    /// by hand. It arrives with whatever the workspace had — two keys is
    /// normal — and `ensure_default_exists_in` will not fill it, because that
    /// returns early precisely when the file exists.
    ///
    /// `gatewayProxy` is deliberately not asserted: the credential check that
    /// adds it is skipped under `cfg!(test)`, so it appears in a real run with
    /// a cloud token, never here.
    #[test]
    fn a_migrated_agent_gets_the_first_run_settings_a_new_one_gets() {
        let (_root, ws) = v2_workspace("filled");
        // A value the user chose, which the backfill must leave alone.
        std::fs::write(
            ws.join(".thclaws/settings.json"),
            r#"{"workspaceVersion":2,"maxTokens":999}"#,
        )
        .unwrap();
        apply(&plan(&ws).unwrap()).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&read(&ws.join(".thclaws/bots/main/.thclaws/settings.json")))
                .unwrap();
        for key in [
            "teamEnabled",
            "shellTabEnabled",
            "imageToolsEnabled",
            "halEnabled",
            "browserEnabled",
            "model",
            "permissions",
            "maxIterations",
        ] {
            assert!(settings.get(key).is_some(), "backfill missed {key}");
        }
        assert_eq!(settings["maxTokens"], 999, "a value the user set is kept");
        assert_eq!(settings["workspaceVersion"], 2);
        assert!(
            settings["agent"]["id"].is_string(),
            "the identity is still minted"
        );
    }

    #[test]
    fn unmigrate_puts_the_agent_back_at_the_root() {
        let (_root, ws) = v2_workspace("round-trip");
        apply(&plan(&ws).unwrap()).unwrap();
        assert!(ws.join(".thclaws/bots/main/AGENTS.md").exists());

        let report = unmigrate(&ws).unwrap();
        assert!(report.moved > 0);
        assert_eq!(
            read(&ws.join("AGENTS.md")),
            "# The agent\n",
            "the tombstone is gone, the agent's own is back"
        );
        // The agent's own settings come back. Renames only, so the identity the
        // upgrade minted for it stays, which publishing needs anyway.
        let settings: serde_json::Value =
            serde_json::from_str(&read(&ws.join(".thclaws/settings.json"))).unwrap();
        assert_eq!(settings["workspaceVersion"], 2);
        assert_eq!(settings["agent"]["id"], "round-trip");
        assert_eq!(read(&ws.join(".git/config")), "[core]\n");
        assert_eq!(
            read(&ws.join(".thclaws/state/sessions/sess-1.jsonl")),
            "{}\n"
        );
        assert_eq!(read(&ws.join("output/report.md")), "findings\n");
        assert!(
            ws.join(".home/.config/thclaws/settings.json").exists(),
            ".home never moved"
        );
        assert!(!ws.join(super::super::CONFIG_REL).exists());
        assert!(
            report.host_backup.join("bots.json").exists(),
            "the host tree is kept, not deleted"
        );
        assert!(looks_like_v2_agent(&ws));

        // And it can be upgraded again, leaving the old backup where it is.
        let again = plan(&ws).unwrap();
        assert!(!again.moves.iter().any(|n| n == HOST_BACKUP));
        apply(&again).unwrap();
        assert!(ws.join(".thclaws/bots/main/AGENTS.md").exists());
        assert!(ws.join(HOST_BACKUP).is_dir());
    }

    #[test]
    fn unmigrate_resumes_after_the_host_tree_was_set_aside() {
        let (_root, ws) = v2_workspace("interrupted");
        apply(&plan(&ws).unwrap()).unwrap();
        // The first rename happened, then the process died.
        std::fs::rename(ws.join(".thclaws"), ws.join(HOST_BACKUP)).unwrap();
        if ws.join("AGENTS.md").exists() {
            std::fs::rename(
                ws.join("AGENTS.md"),
                ws.join(HOST_BACKUP).join("AGENTS.md.tombstone"),
            )
            .unwrap();
        }
        let report = unmigrate(&ws).unwrap();
        assert!(report.moved > 0);
        assert_eq!(read(&ws.join("AGENTS.md")), "# The agent\n");
        assert!(ws.join(".thclaws/state/sessions/sess-1.jsonl").exists());
    }

    #[test]
    fn unmigrate_refuses_a_workspace_with_another_agent() {
        let (_root, ws) = v2_workspace("two-agents");
        apply(&plan(&ws).unwrap()).unwrap();
        std::fs::write(
            ws.join(super::super::CONFIG_REL),
            r#"{"version":1,"bots":[{"slug":"main"},{"slug":"research"}]}"#,
        )
        .unwrap();
        let err = unmigrate(&ws).unwrap_err().to_string();
        assert!(err.contains("main, research"), "{err}");
        assert!(
            ws.join(".thclaws/bots/main/AGENTS.md").exists(),
            "nothing moved"
        );
        assert!(!ws.join(HOST_BACKUP).exists());
    }

    #[test]
    fn unmigrate_refuses_a_name_both_the_root_and_the_agent_hold() {
        let (_root, ws) = v2_workspace("stray");
        apply(&plan(&ws).unwrap()).unwrap();
        // A file the user makes at the root is theirs and never blocks anything.
        std::fs::write(ws.join("new-at-root.txt"), "made after the upgrade").unwrap();
        // The same name as something in the agent folder would be overwritten.
        std::fs::write(ws.join("manifest.json"), "the user's own").unwrap();
        let err = unmigrate(&ws).unwrap_err().to_string();
        assert!(err.contains("manifest.json"), "{err}");
        assert!(!err.contains("new-at-root.txt"), "{err}");
        assert!(ws.join(".thclaws/bots.json").exists(), "nothing moved");
        assert_eq!(read(&ws.join("manifest.json")), "the user's own");
    }

    /// dev-plan/61: the layout v0.126.0/v0.127.0 left behind — the user's
    /// files inside `.thclaws/bots/main/` and a tombstone at the root — is put
    /// back on open, without overwriting anything the root already has.
    #[test]
    fn files_the_old_upgrade_hid_go_back_to_the_root() {
        let (_root, ws) = v2_workspace("old-layout");
        apply(&plan(&ws).unwrap()).unwrap();
        stamp_host_version(&ws, FIRST_HOST_VERSION).unwrap();
        let bot = ws.join(".thclaws/bots/main");
        for name in ["output", "src", ".git", ".gitignore"] {
            std::fs::rename(ws.join(name), bot.join(name)).unwrap();
        }
        std::fs::write(ws.join("AGENTS.md"), TOMBSTONE).unwrap();
        std::fs::write(ws.join("notes.md"), "made at the root").unwrap();
        std::fs::write(bot.join("notes.md"), "from before").unwrap();

        let r = restore_shared_files(&ws).unwrap();
        assert_eq!(r.moved, vec![".git", ".gitignore", "output", "src"]);
        assert_eq!(r.clashes, vec!["notes.md"]);
        assert!(r.removed_tombstone);
        assert!(r.stamped_v4);
        assert_eq!(
            raw_workspace_version(&ws.join(".thclaws/settings.json")),
            Some(4)
        );

        assert_eq!(read(&ws.join("output/report.md")), "findings\n");
        assert!(ws.join("src/main.py").exists());
        assert!(ws.join(".git/config").exists());
        assert!(!ws.join("AGENTS.md").exists(), "the tombstone is gone");
        assert_eq!(
            read(&ws.join("notes.md")),
            "made at the root",
            "never overwritten"
        );
        assert_eq!(
            read(&bot.join("notes.md")),
            "from before",
            "left where it was"
        );
        for rel in ["AGENTS.md", "manifest.json", ".thclaws/settings.json"] {
            assert!(bot.join(rel).exists(), "the agent keeps {rel}");
        }

        let again = restore_shared_files(&ws).unwrap();
        assert!(again.moved.is_empty() && !again.removed_tombstone && !again.stamped_v4);
        assert_eq!(
            read(&bot.join("notes.md")),
            "from before",
            "v4 never pulls it again"
        );
    }

    #[test]
    fn restoring_leaves_a_single_agent_workspace_alone() {
        let (_root, ws) = v2_workspace("plain");
        let r = restore_shared_files(&ws).unwrap();
        assert!(r.moved.is_empty() && r.clashes.is_empty() && !r.removed_tombstone);
        assert_eq!(read(&ws.join("AGENTS.md")), "# The agent\n");
        assert!(!ws.join(".thclaws/bots").exists());
    }

    /// A user's own `AGENTS.md` at the root is not the tombstone and stays.
    #[test]
    fn restoring_keeps_a_users_own_agents_md() {
        let (_root, ws) = v2_workspace("own-agents-md");
        apply(&plan(&ws).unwrap()).unwrap();
        stamp_host_version(&ws, FIRST_HOST_VERSION).unwrap();
        std::fs::write(ws.join("AGENTS.md"), "# Our project rules\n").unwrap();
        let r = restore_shared_files(&ws).unwrap();
        assert!(!r.removed_tombstone);
        assert_eq!(read(&ws.join("AGENTS.md")), "# Our project rules\n");
    }

    /// A v4 agent folder may hold files on purpose; nothing is pulled out.
    #[test]
    fn a_v4_workspace_is_never_rearranged() {
        let (_root, ws) = v2_workspace("v4");
        apply(&plan(&ws).unwrap()).unwrap();
        let bot = ws.join(".thclaws/bots/main");
        std::fs::write(bot.join("agent-notes.md"), "kept with the agent").unwrap();
        let r = restore_shared_files(&ws).unwrap();
        assert!(r.moved.is_empty() && !r.stamped_v4);
        assert!(bot.join("agent-notes.md").exists());
        assert!(!ws.join("agent-notes.md").exists());
    }

    /// A v3 workspace is still a host workspace to `plan`, not "neither v2 nor v3".
    #[test]
    fn a_v3_workspace_is_already_a_host_workspace() {
        let (_root, ws) = v2_workspace("still-v3");
        apply(&plan(&ws).unwrap()).unwrap();
        stamp_host_version(&ws, FIRST_HOST_VERSION).unwrap();
        assert_eq!(plan(&ws).unwrap().status, Status::AlreadyV3);
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn moves_only_the_agent_and_leaves_the_files_at_the_root() {
        let (_root, ws) = v2_workspace("my-project");
        let plan = plan(&ws).unwrap();
        assert_eq!(plan.status, Status::Migrate);
        // dev-plan/61: only what makes the folder an agent moves.
        assert_eq!(plan.moves, vec![".thclaws", "AGENTS.md", "manifest.json"]);
        assert!(!plan.moves_git, "the repo stays with the user's files");
        assert!(plan.keeps.contains(&".home".to_string()));

        let report = apply(&plan).unwrap();
        let bot = ws.join(".thclaws/bots/main");
        assert_eq!(report.bot_dir, bot);

        for rel in [
            "AGENTS.md",
            "manifest.json",
            ".thclaws/settings.json",
            ".thclaws/state/sessions/sess-1.jsonl",
            ".thclaws/state/browser-profile/Cookies",
        ] {
            assert!(bot.join(rel).exists(), "the agent should hold {rel}");
        }
        // The user's files, and their repo, stay exactly where they were.
        for rel in [
            "output/report.md",
            "src/main.py",
            ".gitignore",
            ".git/config",
            ".git/objects/ab/cdef",
        ] {
            assert!(ws.join(rel).exists(), "{rel} must stay at the root");
            assert!(
                !bot.join(rel).exists(),
                "{rel} must not move into the agent"
            );
        }
        assert_eq!(read(&ws.join("output/report.md")), "findings\n");

        assert!(ws.join(".home/.config/thclaws/settings.json").exists());
        assert!(!bot.join(".home").exists());

        let host: serde_json::Value =
            serde_json::from_str(&read(&ws.join(".thclaws/settings.json"))).unwrap();
        assert_eq!(host["workspaceVersion"], 4, "a new upgrade lands on v4");
        let bots: serde_json::Value =
            serde_json::from_str(&read(&ws.join(".thclaws/bots.json"))).unwrap();
        assert_eq!(bots["bots"][0]["slug"], "main");
        assert!(ws.join(".thclaws/state").is_dir());

        // No tombstone: an `AGENTS.md` at the root would load into every agent.
        assert!(!ws.join("AGENTS.md").exists());

        assert!(!ws.join(STAGING).exists());
        assert!(!ws.join(MARKER).exists());
    }

    #[test]
    fn a_second_run_is_a_no_op() {
        let (_root, ws) = v2_workspace("my-project");
        apply(&plan(&ws).unwrap()).unwrap();
        let before = read(&ws.join("output/report.md"));

        let again = plan(&ws).unwrap();
        assert_eq!(again.status, Status::AlreadyV3);
        assert!(again.is_noop());
        let report = apply(&again).unwrap();
        assert_eq!(report.moved, 0);
        assert_eq!(read(&ws.join("output/report.md")), before);
        assert!(!ws.join(".thclaws/bots/main/.thclaws/bots").exists());
    }

    /// A half-migrated workspace holds the user's entire tree, so being
    /// resumable is the whole point. Each case is the state a `kill -9` would
    /// leave behind at that phase.
    #[test]
    fn resumes_from_a_kill_at_any_phase() {
        // Killed during collect: one agent entry staged, marker says collect.
        let (_r1, ws) = v2_workspace("proj");
        write_phase(&ws, Phase::Collect).unwrap();
        std::fs::create_dir_all(ws.join(STAGING)).unwrap();
        std::fs::rename(ws.join(".thclaws"), ws.join(STAGING).join(".thclaws")).unwrap();
        let p = plan(&ws).unwrap();
        assert_eq!(p.status, Status::Resume(Phase::Collect));
        apply(&p).unwrap();
        let bot = ws.join(".thclaws/bots/main");
        assert!(bot.join(".thclaws/settings.json").exists());
        assert!(bot.join("AGENTS.md").exists());
        assert!(ws.join("output/report.md").exists());
        assert!(ws.join(".git/config").exists());
        assert!(!ws.join(STAGING).exists());

        // Killed during install: everything staged, nothing installed.
        let (_r2, ws) = v2_workspace("proj");
        std::fs::create_dir_all(ws.join(STAGING)).unwrap();
        for name in movable_entries(&ws).unwrap() {
            std::fs::rename(ws.join(&name), ws.join(STAGING).join(&name)).unwrap();
        }
        write_phase(&ws, Phase::Install).unwrap();
        let p = plan(&ws).unwrap();
        assert_eq!(p.status, Status::Resume(Phase::Install));
        apply(&p).unwrap();
        assert!(ws.join(".thclaws/bots/main/AGENTS.md").exists());
        assert!(ws.join("src/main.py").exists());
        assert!(ws.join(".thclaws/bots.json").exists());

        // Killed during install AFTER a partial merge: both dirs present.
        let (_r3, ws) = v2_workspace("proj");
        std::fs::create_dir_all(ws.join(STAGING)).unwrap();
        for name in movable_entries(&ws).unwrap() {
            std::fs::rename(ws.join(&name), ws.join(STAGING).join(&name)).unwrap();
        }
        let bot = ws.join(".thclaws/bots/main");
        std::fs::create_dir_all(&bot).unwrap();
        std::fs::rename(
            ws.join(STAGING).join("manifest.json"),
            bot.join("manifest.json"),
        )
        .unwrap();
        write_phase(&ws, Phase::Install).unwrap();
        apply(&plan(&ws).unwrap()).unwrap();
        assert!(bot.join("manifest.json").exists());
        assert!(bot.join("AGENTS.md").exists());
        assert!(!ws.join(STAGING).exists());

        // Killed during finalise: the move is done, host files are not.
        let (_r4, ws) = v2_workspace("proj");
        std::fs::create_dir_all(ws.join(STAGING)).unwrap();
        for name in movable_entries(&ws).unwrap() {
            std::fs::rename(ws.join(&name), ws.join(STAGING).join(&name)).unwrap();
        }
        let bot = ws.join(".thclaws/bots/main");
        std::fs::create_dir_all(bot.parent().unwrap()).unwrap();
        std::fs::rename(ws.join(STAGING), &bot).unwrap();
        write_phase(&ws, Phase::Finalise).unwrap();
        let p = plan(&ws).unwrap();
        assert_eq!(p.status, Status::Resume(Phase::Finalise));
        apply(&p).unwrap();
        assert!(ws.join(".thclaws/bots.json").exists());
        assert!(!ws.join("AGENTS.md").exists(), "no tombstone at the root");
        assert!(ws.join(".thclaws/bots/main/AGENTS.md").exists());
        assert!(!ws.join(".thclaws/bots/main/.thclaws/bots").exists());
    }

    #[test]
    fn refuses_a_tree_it_cannot_read_safely() {
        // A bot is not a workspace.
        let (_r, ws) = v2_workspace("proj");
        let bot = ws.join(".thclaws/bots/research");
        std::fs::create_dir_all(&bot).unwrap();
        assert!(plan(&bot).is_err());

        // A shelf without a v3 stamp is neither shape.
        let err = plan(&ws).unwrap_err().to_string();
        assert!(err.contains("neither v2 nor v3"), "{err}");

        // A directory that merely sits under something called `bots` is an
        // ordinary workspace — the refusal is about `.thclaws/bots` only.
        let plain = tempfile::tempdir().unwrap();
        let proj = plain.path().join("bots/myproj");
        std::fs::create_dir_all(proj.join(".thclaws")).unwrap();
        std::fs::write(proj.join("AGENTS.md"), "x").unwrap();
        assert_eq!(plan(&proj).unwrap().status, Status::Migrate);

        // A v3 stamp with no shelf is a broken tree, not a v2 one: migrating
        // would move the host's own `.thclaws/` into a fresh `bots/main/`.
        let (_r3, ws3) = v2_workspace("proj");
        std::fs::write(
            ws3.join(".thclaws/settings.json"),
            r#"{"workspaceVersion":3}"#,
        )
        .unwrap();
        let err = plan(&ws3).unwrap_err().to_string();
        assert!(err.contains("agents are missing"), "{err}");

        // Staging with no marker means someone removed the marker by hand.
        let (_r2, ws2) = v2_workspace("proj");
        std::fs::create_dir_all(ws2.join(STAGING)).unwrap();
        let err = plan(&ws2).unwrap_err().to_string();
        assert!(err.contains("without a migration marker"), "{err}");
    }

    /// `agent pack` refuses a folder with no `agent.{id,name,description}`,
    /// so a migrated workspace that never had one could not be published.
    #[test]
    fn mints_an_identity_only_when_one_is_missing() {
        let (_r, ws) = v2_workspace("My Cool Project");
        let report = apply(&plan(&ws).unwrap()).unwrap();
        assert!(report.minted_identity);
        let s: serde_json::Value =
            serde_json::from_str(&read(&ws.join(".thclaws/bots/main/.thclaws/settings.json")))
                .unwrap();
        assert_eq!(s["agent"]["id"], "my-cool-project");
        assert_eq!(s["agent"]["name"], "My Cool Project");
        assert!(s["agent"]["description"].as_str().is_some());
        // The version the agent carried is untouched.
        assert_eq!(s["workspaceVersion"], 2);

        // An agent that already has an identity keeps it exactly.
        let (_r2, ws2) = v2_workspace("other");
        std::fs::write(
            ws2.join(".thclaws/settings.json"),
            r#"{"workspaceVersion":2,"agent":{"id":"chosen","name":"Chosen","description":"d","uuid":"u-1"}}"#,
        )
        .unwrap();
        let report = apply(&plan(&ws2).unwrap()).unwrap();
        assert!(!report.minted_identity);
        let s: serde_json::Value = serde_json::from_str(&read(
            &ws2.join(".thclaws/bots/main/.thclaws/settings.json"),
        ))
        .unwrap();
        assert_eq!(s["agent"]["id"], "chosen");
        assert_eq!(s["agent"]["uuid"], "u-1");
    }

    #[test]
    fn slugify_produces_catalogue_safe_ids() {
        assert_eq!(slugify("My Cool Project"), "my-cool-project");
        assert_eq!(slugify("__weird__"), "weird");
        assert_eq!(slugify("ไทย"), "main");
        assert_eq!(slugify(""), "main");
        assert_eq!(slugify("a.b_c"), "a-b-c");
    }

    /// The schedule store is user-level and its daemon refuses a `cwd` that no
    /// longer exists, so this workspace's entries must follow the agent — and
    /// nobody else's may be touched.
    #[test]
    fn rewrites_only_this_workspaces_schedules() {
        let _g = crate::kms::test_env_lock();
        let home = tempfile::tempdir().unwrap();
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", home.path());

        let (_r, ws) = v2_workspace("proj");
        let other = home.path().join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        let store = serde_json::json!({
            "version": 1,
            "schedules": [
                {"id":"mine","cron":"0 9 * * *","prompt":"p","cwd": ws.display().to_string(),"enabled":true},
                {"id":"nested","cron":"0 9 * * *","prompt":"p","cwd": ws.join("output").display().to_string(),"enabled":true},
                {"id":"theirs","cron":"0 9 * * *","prompt":"p","cwd": other.display().to_string(),"enabled":true}
            ]
        });
        let path = home.path().join(".config/thclaws/schedules.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&store).unwrap()).unwrap();

        let p = plan(&ws).unwrap();
        assert_eq!(p.schedules, vec!["mine".to_string(), "nested".to_string()]);
        let report = apply(&p).unwrap();
        assert_eq!(report.rewritten_schedules.len(), 2);

        let after: serde_json::Value = serde_json::from_str(&read(&path)).unwrap();
        let cwd = |i: usize| after["schedules"][i]["cwd"].as_str().unwrap().to_string();
        let bot = ws.join(".thclaws/bots/main");
        assert_eq!(cwd(0), bot.display().to_string());
        assert_eq!(cwd(1), bot.join("output").display().to_string());
        assert_eq!(cwd(2), other.display().to_string());

        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    /// The automatic entry point: a v2 workspace becomes v3 on first open, a
    /// second open finds it so, something else is left alone, and a holder
    /// of the lock makes it wait and then give up rather than move anything.
    #[test]
    fn auto_migrate_runs_once_and_yields_to_a_holder() {
        let (_r, ws) = v2_workspace("proj");
        let wait = Duration::from_millis(200);
        assert!(matches!(
            auto_migrate_with_wait(&ws, wait),
            AutoOutcome::Migrated(_, false)
        ));
        assert!(ws.join(super::super::CONFIG_REL).exists());
        assert!(matches!(
            auto_migrate_with_wait(&ws, wait),
            AutoOutcome::AlreadyV3
        ));

        let empty = tempfile::tempdir().unwrap();
        assert!(matches!(
            auto_migrate_with_wait(empty.path(), wait),
            AutoOutcome::NotV2
        ));

        // The bot's own folder, as its own `--serve` sees it: a v2-shaped tree
        // inside a shelf. Silently not v2 — not a failure to report.
        let bot = ws.join(".thclaws/bots/main");
        assert!(bot.join("AGENTS.md").exists());
        assert!(matches!(
            auto_migrate_with_wait(&bot, wait),
            AutoOutcome::NotV2
        ));
        assert!(
            !empty.path().join(".thclaws").exists(),
            "nothing minted, nothing moved"
        );

        let (_r2, held) = v2_workspace("held");
        let _holder = super::super::lock_workspace(&held, "a host").unwrap();
        let t0 = Instant::now();
        assert!(matches!(
            auto_migrate_with_wait(&held, wait),
            AutoOutcome::Busy
        ));
        assert!(
            t0.elapsed() >= wait,
            "it waited for the holder before giving up"
        );
        assert!(held.join("AGENTS.md").exists() && !held.join(".thclaws/bots").exists());
    }

    #[test]
    fn a_new_workspace_is_minted_as_a_host_plus_one_bot() {
        let dir = tempfile::tempdir().unwrap();
        let dest = mint_new_workspace(dir.path(), MAIN_SLUG).unwrap();
        assert!(dest.ends_with(".thclaws/bots/main"));
        assert!(dest.is_dir());
        let cfg = BotsConfig::load(dir.path()).unwrap();
        assert_eq!(cfg.bots[0].slug, "main");
        assert!(!looks_like_v2_agent(dir.path()));
    }

    /// The one thing `--supervisor` must never do is start a host over an
    /// agent that still lives at the root.
    #[test]
    fn a_v2_workspace_is_recognised_before_a_host_starts_over_it() {
        let (_r, ws) = v2_workspace("proj");
        assert!(looks_like_v2_agent(&ws));
        apply(&plan(&ws).unwrap()).unwrap();
        assert!(!looks_like_v2_agent(&ws), "a migrated workspace is not v2");

        let empty = tempfile::tempdir().unwrap();
        assert!(!looks_like_v2_agent(empty.path()));
    }
}
