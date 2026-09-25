//! Project-context discovery and system-prompt assembly.
//!
//! The agent's system prompt combines a base prompt (from config) with
//! runtime-discovered facts: cwd, git branch/status, and any CLAUDE.md
//! found by walking up from the cwd. Git is queried by shelling out to
//! the `git` binary — zero extra deps, and degrades gracefully when git
//! isn't installed or cwd isn't a repo.

use crate::error::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

// for Windows creation flag to hide the console window
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInfo {
    pub branch: String,
    pub head: String,
    pub is_dirty: bool,
    pub status_summary: String,
}

#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub cwd: PathBuf,
    pub git: Option<GitInfo>,
    pub project_instructions: Option<String>,
}

impl GitInfo {
    /// Parse pre-captured git command outputs into a GitInfo. Pure; trivial to test.
    pub fn from_outputs(branch: &str, head: &str, status_porcelain: &str) -> Self {
        let lines: Vec<&str> = status_porcelain.lines().filter(|l| !l.is_empty()).collect();
        let is_dirty = !lines.is_empty();
        let status_summary = if is_dirty {
            format!("{} file(s) changed", lines.len())
        } else {
            "clean".to_string()
        };
        GitInfo {
            branch: branch.trim().to_string(),
            head: head.trim().to_string(),
            is_dirty,
            status_summary,
        }
    }

    /// Shell out to git in `cwd`. Returns None if cwd is not a git repo
    /// or if git is not installed.
    pub fn from_cwd(cwd: &Path) -> Option<Self> {
        let run = |args: &[&str]| -> Option<String> {
            let mut cmd = Command::new("git");

            // for Windows creation flag to hide the console window
            #[cfg(target_os = "windows")]
            cmd.creation_flags(0x08000000);

            cmd.args(args).current_dir(cwd);

            let out = cmd.output().ok()?;

            if !out.status.success() {
                return None;
            }
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"])?;
        let head = run(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
        let status = run(&["status", "--porcelain"]).unwrap_or_default();
        Some(Self::from_outputs(&branch, &head, &status))
    }
}

impl ProjectContext {
    /// Discover everything rooted at `cwd`. Git info and CLAUDE.md are both
    /// optional — their absence is never an error.
    pub fn discover(cwd: &Path) -> Result<Self> {
        let git = GitInfo::from_cwd(cwd);
        let project_instructions = find_claude_md(cwd);
        Ok(Self {
            cwd: cwd.to_path_buf(),
            git,
            project_instructions,
        })
    }

    /// Append runtime-discovered context onto a base system prompt. Sections are
    /// added only when there's something to say; no empty headers.
    pub fn build_system_prompt(&self, base: &str) -> String {
        let mut parts: Vec<String> = Vec::new();

        if !base.trim().is_empty() {
            parts.push(base.trim().to_string());
        }

        // Anchor the model in real time. Without a current-date signal the
        // model treats its training cutoff as "now" and answers
        // "latest"/"recent"/news queries from stale memory (the scheduled
        // "latest AI news → year-old results" bug). State the date AND
        // tell it to search for anything time-sensitive.
        parts.push(format!(
            "# Environment\nToday's date: {} (UTC).\nYour training data has a cutoff, so it is \
             stale for anything time-sensitive — \"latest\", \"recent\", \"current\", news, \
             prices, releases, versions, who-holds-an-office. For those, use WebSearch / \
             WebFetch (or the browser tools) to get up-to-date information; do NOT answer from \
             memory.",
            crate::usage::today_str()
        ));

        // Under a workspace host the agent's folder and the user's files are
        // two different directories, and the model had no way to tell which is
        // which — it read the cwd line, ran `ls .thclaws/`, got the host's
        // folder and reported the layout as wrong. Both paths come from their
        // real sources rather than from `cwd`, which is the agent's folder on
        // this surface and the workspace on others.
        match crate::workdir::agent_dir() {
            Some(agent) => parts.push(format!(
                "# Working directory\n{root}\n\nThis workspace has a host, so two directories \
                 are in play. The user's files live at the workspace root above, and that is \
                 also where a shell runs. Your own folder is {agent} — your AGENTS.md, \
                 settings, sessions and anything you ship.\n\nA `.thclaws/…` path you hand a \
                 file tool (Read, Write, Edit, Glob, Grep) resolves in YOUR folder, so \
                 `.thclaws/x.json` is your own state. Two subpaths are shared and stay at the \
                 workspace root: `.thclaws/state/kms` and `.thclaws/state/logs`.\n\nA shell \
                 cannot follow that rule — its cwd is the workspace root, so a bare \
                 `.thclaws/` in a Bash command is the HOST's folder, not yours. Run what you \
                 ship through the variable instead: \
                 `python3 \"$THCLAWS_AGENT_DIR/.thclaws/scripts/x.py\"`.",
                root = crate::workdir::workspace_root().display(),
                agent = agent.display(),
            )),
            None => parts.push(format!("# Working directory\n{}", self.cwd.display())),
        }

        if let Some(git) = &self.git {
            parts.push(format!(
                "# Git\nBranch: {}\nHEAD:   {}\nStatus: {}",
                git.branch, git.head, git.status_summary
            ));
        }

        if let Some(instr) = &self.project_instructions {
            parts.push(format!("# Project instructions\n{}", instr.trim()));
        }

        parts.join("\n\n")
    }
}

/// Discover all project instructions following Claude Code's multi-source
/// model, plus the vendor-neutral [AGENTS.md] standard (Google / OpenAI /
/// Factory / Sourcegraph / Cursor) stewarded by the Agentic AI Foundation.
/// At every location we check for both `CLAUDE.md` and `AGENTS.md`; if both
/// exist we include both with `CLAUDE.md` first (per-vendor instructions
/// often refine a shared baseline).
///
/// Sources loaded (all concatenated, in order):
/// 1. `~/.claude/CLAUDE.md` / `~/.claude/AGENTS.md` / `~/.config/thclaws/CLAUDE.md` / `~/.config/thclaws/AGENTS.md` — user-level instructions
/// 2. Walk up from `start`: `CLAUDE.md` and `AGENTS.md` in each ancestor directory
/// 3. Project config dirs: `.claude/CLAUDE.md`, `.thclaws/CLAUDE.md`, `.thclaws/AGENTS.md`
/// 4. Rules dirs: `.claude/rules/*.md` then `.thclaws/rules/*.md` (each sorted alphabetically)
/// 5. `CLAUDE.local.md` / `AGENTS.local.md` — local overrides (gitignored, highest priority)
///
/// [AGENTS.md]: https://agents.md
pub fn find_claude_md(start: &Path) -> Option<String> {
    find_claude_md_with(start, load_claude_md_compat_flag())
}

/// Read the `claude_md_compat` flag from settings. Defaults to `false`
/// when the config can't be loaded (fresh install, malformed file).
/// Pulled into a helper so the three loaders + scanners share one
/// source of truth.
fn load_claude_md_compat_flag() -> bool {
    crate::config::AppConfig::load()
        .map(|c| c.claude_md_compat)
        .unwrap_or(false)
}

/// Test-injectable variant of [`find_claude_md`]. `claude_md_compat`
/// = whether to also load `~/.claude/CLAUDE.md` + `~/.claude/AGENTS.md`
/// (user-level Claude Code memory). Default behavior (`false`) skips
/// those — the user's Claude Code identity isn't generic agent
/// instructions and shouldn't bleed into thClaws's prompt.
/// `<…>/.thclaws/bots` — the shelf a bot sits on. Reaching it while walking
/// up means the walk has left the bot and entered the host.
fn is_bot_shelf(dir: &Path) -> bool {
    dir.file_name().is_some_and(|n| n == "bots")
        && dir
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|n| n == ".thclaws")
}

pub fn find_claude_md_with(start: &Path, claude_md_compat: bool) -> Option<String> {
    // Shared-agent mode (dev-plan/41): instructions are LOCKED to the
    // company brain's `AGENTS.md`. Every other source — working-dir
    // CLAUDE.md/AGENTS.md, the ancestor walk, user scope
    // (`~/.config/thclaws`, `~/.claude`), rules dirs, local overrides —
    // is ignored, so a member can't smuggle instruction overrides in via
    // any layer (also blunts prompt-injection on a company-billed agent).
    if let Some(agents_md) = crate::shared::shared_agents_md() {
        return std::fs::read_to_string(&agents_md)
            .ok()
            .filter(|s| !s.trim().is_empty());
    }

    let mut parts: Vec<String> = Vec::new();

    // 1. User-level instructions. Claude Code path first, then vendor-neutral
    // locations so a repo-shared AGENTS.md can extend (not replace) the user
    // baseline.
    if let Some(home) = crate::util::home_dir() {
        // M6.18 BUG M4: load CLAUDE before AGENTS at every scope. The
        // user-level thclaws-native pair was inverted (AGENTS first),
        // contradicting the "per-vendor instructions refine a shared
        // baseline" rationale used everywhere else.
        //
        // M6.39.5: `~/.claude/CLAUDE.md` and `~/.claude/AGENTS.md` are
        // gated on `claude_md_compat` — see field docstring on
        // AppConfig. Default (`false`) means thClaws doesn't pick up
        // the user's Claude Code identity by accident.
        let mut candidates: Vec<PathBuf> = Vec::new();
        if claude_md_compat {
            candidates.push(home.join(".claude/CLAUDE.md"));
            candidates.push(home.join(".claude/AGENTS.md"));
        }
        candidates.push(home.join(".config/thclaws/CLAUDE.md"));
        candidates.push(home.join(".config/thclaws/AGENTS.md"));
        for candidate in candidates {
            if let Ok(contents) = std::fs::read_to_string(&candidate) {
                parts.push(contents);
            }
        }
    }

    // 2. Walk up from start — CLAUDE.md + AGENTS.md at each ancestor.
    // Group the per-ancestor hits so that reversing the outer list flips
    // ancestor order (root-most first) without scrambling the within-
    // ancestor order (CLAUDE before AGENTS).
    let mut ancestor_groups: Vec<Vec<String>> = Vec::new();
    let mut cur = Some(start);
    while let Some(dir) = cur {
        // dev-plan/59: a bot lives at `<ws>/.thclaws/bots/<slug>/`, and above
        // it is the host — someone else's tree. Without this stop, every bot
        // would load the host's AGENTS.md, which after a v2→v3 migration is
        // the tombstone addressed to an OLD binary: every migrated bot would
        // open by telling its user the workspace needs upgrading.
        if is_bot_shelf(dir) {
            break;
        }
        let mut group: Vec<String> = Vec::new();
        for name in ["CLAUDE.md", "AGENTS.md"] {
            let candidate = dir.join(name);
            if candidate.exists() {
                if let Ok(contents) = std::fs::read_to_string(&candidate) {
                    group.push(contents);
                }
            }
        }
        if !group.is_empty() {
            ancestor_groups.push(group);
        }
        cur = dir.parent();
    }
    ancestor_groups.reverse(); // root-most ancestor first
    for group in ancestor_groups {
        parts.extend(group);
    }

    // 3. Project-level instructions files living inside the config dirs
    // (not at the cwd root — those were covered by the ancestor walk).
    // Checked in this order so later entries can refine earlier ones:
    //   .claude/CLAUDE.md  (Claude Code compat)
    //   .thclaws/CLAUDE.md
    //   .thclaws/AGENTS.md
    for path in [
        start.join(".claude/CLAUDE.md"),
        start.join(".thclaws/CLAUDE.md"),
        start.join(".thclaws/AGENTS.md"),
    ] {
        if path.exists() {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                parts.push(contents);
            }
        }
    }

    // 4. Rules directories — `.claude/rules/*.md` then `.thclaws/rules/*.md`,
    // each sorted alphabetically, concatenated in order so thClaws-native
    // rules can override Claude Code's.
    for rules_dir in [start.join(".claude/rules"), start.join(".thclaws/rules")] {
        if !rules_dir.is_dir() {
            continue;
        }
        let mut rule_files: Vec<PathBuf> = std::fs::read_dir(&rules_dir)
            .ok()
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default();
        rule_files.sort();
        for path in rule_files {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                parts.push(contents);
            }
        }
    }

    // 5. Local overrides (highest priority, typically gitignored). Check
    // both `CLAUDE.local.md` and `AGENTS.local.md`.
    for name in ["CLAUDE.local.md", "AGENTS.local.md"] {
        let local = start.join(name);
        if let Ok(contents) = std::fs::read_to_string(&local) {
            parts.push(contents);
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// Soft warning threshold (chars) for `CLAUDE.md` / `AGENTS.md`. Any single
/// file at or above this size gets flagged — it's not truncated (Claude
/// Code matches this behaviour), just surfaced so the user notices their
/// team-memory file has grown past the point where the model is likely to
/// read it carefully.
pub const CLAUDE_MD_WARN_BYTES: u64 = 40_000;

/// Metadata for one memory-file hit found during a `find_claude_md`-style
/// walk. Used by [`scan_claude_md_oversize`] to report warnings without
/// re-implementing the discovery order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeMdOversize {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Walk the same locations [`find_claude_md`] does and collect every
/// file's size. Used by `/context` to show per-contributor byte
/// counts so users can see which memory file is driving their token
/// spend. Pure filesystem walk — no read.
pub fn scan_claude_md_sizes(start: &Path) -> Vec<(PathBuf, u64)> {
    // Shared-agent mode (dev-plan/41): only the locked company AGENTS.md
    // is in the prompt, so /context must report just that — not member
    // files that are ignored.
    if let Some(agents_md) = crate::shared::shared_agents_md() {
        return match std::fs::metadata(&agents_md) {
            Ok(meta) if meta.is_file() => vec![(agents_md, meta.len())],
            _ => Vec::new(),
        };
    }
    let claude_md_compat = load_claude_md_compat_flag();
    let mut out: Vec<(PathBuf, u64)> = Vec::new();
    let mut check = |path: PathBuf| {
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.is_file() {
                out.push((path, meta.len()));
            }
        }
    };
    if let Some(home) = crate::util::home_dir() {
        // M6.18 BUG M4: CLAUDE before AGENTS at every scope so the
        // size scan matches find_claude_md's load order.
        // M6.39.5: `~/.claude/*` user-level files honor the same
        // `claude_md_compat` flag as find_claude_md — the size
        // scan reflects what's actually loaded into the prompt.
        if claude_md_compat {
            check(home.join(".claude/CLAUDE.md"));
            check(home.join(".claude/AGENTS.md"));
        }
        for candidate in [
            home.join(".config/thclaws/CLAUDE.md"),
            home.join(".config/thclaws/AGENTS.md"),
        ] {
            check(candidate);
        }
    }
    let mut cur = Some(start);
    while let Some(dir) = cur {
        for name in ["CLAUDE.md", "AGENTS.md"] {
            check(dir.join(name));
        }
        cur = dir.parent();
    }
    for path in [
        start.join(".claude/CLAUDE.md"),
        start.join(".thclaws/CLAUDE.md"),
        start.join(".thclaws/AGENTS.md"),
    ] {
        check(path);
    }
    for rules_dir in [start.join(".claude/rules"), start.join(".thclaws/rules")] {
        if let Ok(entries) = std::fs::read_dir(&rules_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|x| x.to_str()) == Some("md") {
                    check(path);
                }
            }
        }
    }
    // M6.18 BUG M3: also check AGENTS.local.md so the scan matches
    // find_claude_md (which loads BOTH local override files).
    // Pre-fix /context underreported memory contribution for
    // projects using AGENTS.local.md.
    check(start.join("CLAUDE.local.md"));
    check(start.join("AGENTS.local.md"));
    out
}

/// Walk the same locations [`find_claude_md`] does and collect any
/// file ≥ [`CLAUDE_MD_WARN_BYTES`]. Pure filesystem walk — no read —
/// so it's cheap enough to call at every session startup.
pub fn scan_claude_md_oversize(start: &Path) -> Vec<ClaudeMdOversize> {
    // Shared-agent mode (dev-plan/41): only the locked company AGENTS.md
    // is loaded — scan only that.
    if let Some(agents_md) = crate::shared::shared_agents_md() {
        return match std::fs::metadata(&agents_md) {
            Ok(meta) if meta.is_file() && meta.len() >= CLAUDE_MD_WARN_BYTES => {
                vec![ClaudeMdOversize {
                    path: agents_md,
                    bytes: meta.len(),
                }]
            }
            _ => Vec::new(),
        };
    }
    let claude_md_compat = load_claude_md_compat_flag();
    let mut out = Vec::new();
    let mut check = |path: PathBuf| {
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.is_file() && meta.len() >= CLAUDE_MD_WARN_BYTES {
                out.push(ClaudeMdOversize {
                    path,
                    bytes: meta.len(),
                });
            }
        }
    };

    if let Some(home) = crate::util::home_dir() {
        // M6.18 BUG M4: CLAUDE before AGENTS at every scope so the
        // size scan matches find_claude_md's load order.
        // M6.39.5: gate user-home `~/.claude/*` on claude_md_compat
        // so the oversize warning at startup doesn't fire for files
        // that aren't actually loaded into the prompt.
        if claude_md_compat {
            check(home.join(".claude/CLAUDE.md"));
            check(home.join(".claude/AGENTS.md"));
        }
        for candidate in [
            home.join(".config/thclaws/CLAUDE.md"),
            home.join(".config/thclaws/AGENTS.md"),
        ] {
            check(candidate);
        }
    }

    let mut cur = Some(start);
    while let Some(dir) = cur {
        for name in ["CLAUDE.md", "AGENTS.md"] {
            check(dir.join(name));
        }
        cur = dir.parent();
    }

    for path in [
        start.join(".claude/CLAUDE.md"),
        start.join(".thclaws/CLAUDE.md"),
        start.join(".thclaws/AGENTS.md"),
    ] {
        check(path);
    }

    for rules_dir in [start.join(".claude/rules"), start.join(".thclaws/rules")] {
        if let Ok(entries) = std::fs::read_dir(&rules_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|x| x.to_str()) == Some("md") {
                    check(path);
                }
            }
        }
    }

    // M6.18 BUG M3: also check AGENTS.local.md so the scan matches
    // find_claude_md (which loads BOTH local override files).
    // Pre-fix /context underreported memory contribution for
    // projects using AGENTS.local.md.
    check(start.join("CLAUDE.local.md"));
    check(start.join("AGENTS.local.md"));
    out
}

#[cfg(test)]
mod tests {

    /// dev-plan/59: the ancestor walk must stop at the shelf. A bot at
    /// `<ws>/.thclaws/bots/<slug>/` is not part of the host's project, and
    /// the host's AGENTS.md after a migration is a tombstone addressed to an
    /// older binary — loading it would make every migrated bot open by
    /// telling its user the workspace is broken.
    #[test]
    fn a_bot_does_not_inherit_the_hosts_instructions() {
        let ws = tempfile::tempdir().unwrap();
        let bot = ws.path().join(".thclaws/bots/main");
        std::fs::create_dir_all(&bot).unwrap();
        std::fs::write(
            ws.path().join("AGENTS.md"),
            "TOMBSTONE: this workspace has been upgraded",
        )
        .unwrap();
        std::fs::write(bot.join("AGENTS.md"), "I am the research bot.").unwrap();

        let loaded = find_claude_md_with(&bot, false).unwrap_or_default();
        assert!(loaded.contains("I am the research bot."), "{loaded}");
        assert!(
            !loaded.contains("TOMBSTONE"),
            "the host's instructions leaked into a bot:\n{loaded}"
        );

        // A bot with no instructions of its own inherits nothing either.
        let bare = ws.path().join(".thclaws/bots/bare");
        std::fs::create_dir_all(&bare).unwrap();
        let loaded = find_claude_md_with(&bare, false).unwrap_or_default();
        assert!(!loaded.contains("TOMBSTONE"), "{loaded}");
    }

    /// The stop is specific to the shelf: an ordinary `bots/` folder in
    /// someone's project must keep inheriting its parents.
    #[test]
    fn an_ordinary_bots_folder_is_not_a_shelf() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("bots/worker");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "PROJECT RULES").unwrap();
        let loaded = find_claude_md_with(&nested, false).unwrap_or_default();
        assert!(loaded.contains("PROJECT RULES"), "{loaded}");
    }
    use super::*;
    use tempfile::tempdir;

    /// Serializes tests that mutate `$HOME`. Reuses
    /// `kms::test_env_lock` so HOME-touching tests across all modules
    /// share the same mutex — otherwise a `context::tests` test could
    /// rewrite HOME while a `kms::tests` test still reads it.
    struct HomeGuard {
        prev: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn new(home: &Path) -> Self {
            let lock = crate::kms::test_env_lock();
            let prev = std::env::var("HOME").ok();
            std::env::set_var("HOME", home);
            Self { prev, _lock: lock }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn git_info_from_outputs_clean() {
        let g = GitInfo::from_outputs("main\n", "abc1234\n", "");
        assert_eq!(g.branch, "main");
        assert_eq!(g.head, "abc1234");
        assert!(!g.is_dirty);
        assert_eq!(g.status_summary, "clean");
    }

    #[test]
    fn git_info_from_outputs_dirty() {
        let status = " M file.rs\n?? new.txt\n M other.rs\n";
        let g = GitInfo::from_outputs("feature", "def5678", status);
        assert!(g.is_dirty);
        assert_eq!(g.status_summary, "3 file(s) changed");
    }

    #[test]
    fn git_info_from_cwd_returns_none_for_non_repo() {
        let dir = tempdir().unwrap();
        assert!(GitInfo::from_cwd(dir.path()).is_none());
    }

    #[test]
    fn scan_claude_md_oversize_flags_big_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "x".repeat(50_000)).unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "small").unwrap();
        let hits = scan_claude_md_oversize(dir.path());
        let paths: Vec<_> = hits.iter().map(|h| h.path.clone()).collect();
        assert!(paths.contains(&dir.path().join("CLAUDE.md")));
        assert!(!paths.contains(&dir.path().join("AGENTS.md")));
    }

    #[test]
    fn scan_claude_md_oversize_silent_for_missing_files() {
        let dir = tempdir().unwrap();
        assert!(scan_claude_md_oversize(dir.path()).is_empty());
    }

    #[test]
    fn find_claude_md_finds_file_in_cwd() {
        let home = tempdir().unwrap();
        let _guard = HomeGuard::new(home.path());
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "be concise").unwrap();
        assert_eq!(find_claude_md(dir.path()).as_deref(), Some("be concise"));
    }

    #[test]
    fn find_claude_md_walks_up_to_find_ancestor() {
        let home = tempdir().unwrap();
        let _guard = HomeGuard::new(home.path());
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "root rules").unwrap();
        assert_eq!(find_claude_md(&nested).as_deref(), Some("root rules"));
    }

    #[test]
    fn find_claude_md_returns_none_when_absent() {
        let home = tempdir().unwrap();
        let _guard = HomeGuard::new(home.path());
        let dir = tempdir().unwrap();
        assert!(find_claude_md(dir.path()).is_none());
    }

    /// M6.39.5: by default (`claude_md_compat = false`) thClaws does
    /// NOT load the user-level `~/.claude/CLAUDE.md` file. This file
    /// is the user's Claude Code identity (Pinn.AI bias, Thai-first
    /// instructions, "use Claude Code's MCP tools" — none of that
    /// applies to thClaws).
    #[test]
    fn find_claude_md_with_skips_user_claude_md_by_default() {
        let home = tempdir().unwrap();
        let _guard = HomeGuard::new(home.path());
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude/CLAUDE.md"),
            "Claude Code identity",
        )
        .unwrap();
        let dir = tempdir().unwrap();
        // Default false → user-home claude file NOT loaded.
        assert!(find_claude_md_with(dir.path(), false).is_none());
    }

    #[test]
    fn find_claude_md_with_loads_user_claude_md_when_compat_true() {
        let home = tempdir().unwrap();
        let _guard = HomeGuard::new(home.path());
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude/CLAUDE.md"),
            "Claude Code identity",
        )
        .unwrap();
        let dir = tempdir().unwrap();
        // Opt-in → original Claude Code parity behavior preserved.
        let out = find_claude_md_with(dir.path(), true).unwrap();
        assert!(out.contains("Claude Code identity"));
    }

    #[test]
    fn find_claude_md_with_always_loads_thclaws_native_user_file() {
        let home = tempdir().unwrap();
        let _guard = HomeGuard::new(home.path());
        std::fs::create_dir_all(home.path().join(".config/thclaws")).unwrap();
        std::fs::write(
            home.path().join(".config/thclaws/CLAUDE.md"),
            "thClaws-native user config",
        )
        .unwrap();
        let dir = tempdir().unwrap();
        // Both flag values must load the thClaws-native user file —
        // it's specifically thClaws's home, not Claude Code's.
        let out_default = find_claude_md_with(dir.path(), false).unwrap();
        assert!(out_default.contains("thClaws-native user config"));
        let out_compat = find_claude_md_with(dir.path(), true).unwrap();
        assert!(out_compat.contains("thClaws-native user config"));
    }

    #[test]
    fn find_claude_md_with_always_loads_project_dotclaude_dir() {
        let home = tempdir().unwrap();
        let _guard = HomeGuard::new(home.path());
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(
            dir.path().join(".claude/CLAUDE.md"),
            "project-shared instructions",
        )
        .unwrap();
        // Project-level `.claude/CLAUDE.md` (committed in the repo)
        // is NOT gated — it's repo-shared instructions, not user
        // identity. Both flag values must load it.
        let out_default = find_claude_md_with(dir.path(), false).unwrap();
        assert!(out_default.contains("project-shared instructions"));
        let out_compat = find_claude_md_with(dir.path(), true).unwrap();
        assert!(out_compat.contains("project-shared instructions"));
    }

    #[test]
    fn find_claude_md_finds_agents_md_at_cwd() {
        let home = tempdir().unwrap();
        let _guard = HomeGuard::new(home.path());
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "vendor-neutral rules").unwrap();
        assert_eq!(
            find_claude_md(dir.path()).as_deref(),
            Some("vendor-neutral rules")
        );
    }

    #[test]
    fn find_claude_md_includes_both_when_both_exist() {
        let home = tempdir().unwrap();
        let _guard = HomeGuard::new(home.path());
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "claude rules").unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "agent rules").unwrap();
        let out = find_claude_md(dir.path()).unwrap();
        // Both present, CLAUDE.md first.
        assert!(out.contains("claude rules"));
        assert!(out.contains("agent rules"));
        assert!(
            out.find("claude rules").unwrap() < out.find("agent rules").unwrap(),
            "CLAUDE.md should come before AGENTS.md"
        );
    }

    #[test]
    fn find_claude_md_walks_up_to_find_agents_md() {
        let home = tempdir().unwrap();
        let _guard = HomeGuard::new(home.path());
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "monorepo rules").unwrap();
        assert_eq!(find_claude_md(&nested).as_deref(), Some("monorepo rules"));
    }

    #[test]
    fn find_claude_md_picks_up_thclaws_agents_md() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".thclaws")).unwrap();
        std::fs::write(
            dir.path().join(".thclaws/AGENTS.md"),
            "thclaws-native rules",
        )
        .unwrap();
        let out = find_claude_md(dir.path()).unwrap();
        assert!(out.contains("thclaws-native rules"));
    }

    #[test]
    fn find_claude_md_picks_up_thclaws_rules_dir() {
        let dir = tempdir().unwrap();
        let rules = dir.path().join(".thclaws/rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("01-style.md"), "prefer terse names").unwrap();
        std::fs::write(rules.join("02-tests.md"), "tests alongside code").unwrap();
        let out = find_claude_md(dir.path()).unwrap();
        assert!(out.contains("prefer terse names"));
        assert!(out.contains("tests alongside code"));
        // Sorted — 01 rule appears before 02.
        assert!(
            out.find("prefer terse names").unwrap() < out.find("tests alongside code").unwrap()
        );
    }

    #[test]
    fn find_claude_md_picks_up_agents_local_md_override() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "shared").unwrap();
        std::fs::write(dir.path().join("AGENTS.local.md"), "local-only").unwrap();
        let out = find_claude_md(dir.path()).unwrap();
        assert!(out.contains("shared"));
        assert!(out.contains("local-only"));
        // Local override comes last (wins by being appended).
        assert!(out.find("shared").unwrap() < out.find("local-only").unwrap());
    }

    #[test]
    fn build_system_prompt_without_git_or_instructions() {
        let ctx = ProjectContext {
            cwd: PathBuf::from("/tmp/proj"),
            git: None,
            project_instructions: None,
        };
        let p = ctx.build_system_prompt("Base prompt.");
        assert!(p.starts_with("Base prompt."));
        assert!(p.contains("# Working directory"));
        assert!(p.contains("/tmp/proj"));
        assert!(!p.contains("# Git"));
        assert!(!p.contains("# Project instructions"));
    }

    #[test]
    fn build_system_prompt_with_all_sections() {
        let ctx = ProjectContext {
            cwd: PathBuf::from("/tmp/proj"),
            git: Some(GitInfo {
                branch: "main".into(),
                head: "abc1234".into(),
                is_dirty: false,
                status_summary: "clean".into(),
            }),
            project_instructions: Some("use tabs".into()),
        };
        let p = ctx.build_system_prompt("You are helpful.");
        assert!(p.contains("You are helpful."));
        assert!(p.contains("# Git"));
        assert!(p.contains("Branch: main"));
        assert!(p.contains("HEAD:   abc1234"));
        assert!(p.contains("Status: clean"));
        assert!(p.contains("# Project instructions"));
        assert!(p.contains("use tabs"));
    }

    #[test]
    fn build_system_prompt_omits_empty_base() {
        let ctx = ProjectContext {
            cwd: PathBuf::from("/tmp/proj"),
            git: None,
            project_instructions: None,
        };
        let p = ctx.build_system_prompt("");
        // Empty base leaves no leading blank — the prompt starts cleanly
        // with the first real section (the date-anchored environment).
        assert!(p.starts_with("# Environment"));
        assert!(p.contains("Today's date:"));
        assert!(p.contains("# Working directory"));
    }
}
