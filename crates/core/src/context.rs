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

        parts.push(format!("# Working directory\n{}", self.cwd.display()));

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
    let mut parts: Vec<String> = Vec::new();

    // 1. User-level instructions. Claude Code path first, then vendor-neutral
    // locations so a repo-shared AGENTS.md can extend (not replace) the user
    // baseline.
    if let Some(home) = crate::util::home_dir() {
        for candidate in [
            home.join(".claude/CLAUDE.md"),
            home.join(".claude/AGENTS.md"),
            home.join(".config/thclaws/AGENTS.md"),
            home.join(".config/thclaws/CLAUDE.md"),
        ] {
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
    let mut out: Vec<(PathBuf, u64)> = Vec::new();
    let mut check = |path: PathBuf| {
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.is_file() {
                out.push((path, meta.len()));
            }
        }
    };
    if let Some(home) = crate::util::home_dir() {
        for candidate in [
            home.join(".claude/CLAUDE.md"),
            home.join(".claude/AGENTS.md"),
            home.join(".config/thclaws/AGENTS.md"),
            home.join(".config/thclaws/CLAUDE.md"),
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
    check(start.join("CLAUDE.local.md"));
    out
}

/// Walk the same locations [`find_claude_md`] does and collect any
/// file ≥ [`CLAUDE_MD_WARN_BYTES`]. Pure filesystem walk — no read —
/// so it's cheap enough to call at every session startup.
pub fn scan_claude_md_oversize(start: &Path) -> Vec<ClaudeMdOversize> {
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
        for candidate in [
            home.join(".claude/CLAUDE.md"),
            home.join(".claude/AGENTS.md"),
            home.join(".config/thclaws/AGENTS.md"),
            home.join(".config/thclaws/CLAUDE.md"),
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

    check(start.join("CLAUDE.local.md"));
    out
}

#[cfg(test)]
mod tests {
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
        assert!(p.starts_with("# Working directory"));
    }
}
