//! Legacy slash-command prompts (Claude Code–style).
//!
//! A command is a single markdown file under one of the standard dirs:
//!
//! 1. `.thclaws/commands/` (project-scoped — highest priority)
//! 2. `.claude/commands/` (Claude Code project compat)
//! 3. `~/.config/thclaws/commands/` (user global)
//! 4. `~/.claude/commands/` (Claude Code user compat — fallback)
//!
//! The filename stem is the command name (`deploy.md` → `/deploy`). Optional
//! YAML frontmatter carries a `description` and/or `whenToUse`. The markdown
//! body is inserted as a user prompt when the command fires, with the
//! `$ARGUMENTS` placeholder replaced by whatever the user typed after the
//! name.
//!
//! This is separate from [`crate::skills`] — skills carry their own
//! instructions + script bundle and are loaded via the `Skill` tool.
//! Commands are simpler: just a prompt template. Both are reachable as
//! `/<name>`; when both exist with the same name, skills win.

use crate::memory::parse_frontmatter;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct CommandDef {
    pub name: String,
    pub description: String,
    pub when_to_use: String,
    pub body: String,
    pub source: PathBuf,
}

impl CommandDef {
    /// Expand the command body: replace `$ARGUMENTS` with `args`, trimmed.
    /// If the body has no placeholder and the user supplied args, append
    /// them after a blank line so the model sees them.
    pub fn render(&self, args: &str) -> String {
        let args = args.trim();
        if self.body.contains("$ARGUMENTS") {
            self.body.replace("$ARGUMENTS", args)
        } else if args.is_empty() {
            self.body.clone()
        } else {
            format!("{}\n\n{args}", self.body.trim_end())
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CommandStore {
    pub commands: HashMap<String, CommandDef>,
}

impl CommandStore {
    pub fn discover() -> Self {
        Self::discover_with_extra(&[])
    }

    /// Discover commands, additionally walking each directory in `extra`.
    /// Used by the plugin system to surface commands contributed by
    /// installed plugins.
    pub fn discover_with_extra(extra: &[PathBuf]) -> Self {
        let mut store = Self::default();
        let mut dirs = Self::command_dirs();
        for p in extra {
            dirs.push(p.clone());
        }
        for dir in dirs {
            if dir.exists() {
                store.load_dir(&dir);
            }
        }
        // Built-ins fill any names the filesystem didn't define, so a
        // project/user command of the same name still wins.
        store.seed_builtins();
        store
    }

    /// Project → user precedence: earlier entries win, so a project-local
    /// `.thclaws/commands/deploy.md` overrides a global `~/.claude/commands/deploy.md`.
    fn command_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(home) = crate::util::home_dir() {
            dirs.push(home.join(".config/thclaws/commands"));
            dirs.push(home.join(".claude/commands"));
        }
        dirs.insert(0, PathBuf::from(".claude/commands"));
        dirs.insert(0, PathBuf::from(".thclaws/commands"));
        dirs
    }

    fn load_dir(&mut self, base: &Path) {
        let Ok(entries) = std::fs::read_dir(base) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            if let Some(cmd) = Self::parse(&path) {
                // Earlier dirs win: only insert if not already present.
                self.commands.entry(cmd.name.clone()).or_insert(cmd);
            }
        }
    }

    fn parse(path: &Path) -> Option<CommandDef> {
        let raw = std::fs::read_to_string(path).ok()?;
        let fallback = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        Some(Self::parse_from_str(fallback, &raw, path.to_path_buf()))
    }

    /// Parse a command from a raw markdown string (shared by filesystem
    /// discovery and built-in seeding). `fallback_name` is used as the
    /// command name when the frontmatter omits `name`.
    fn parse_from_str(fallback_name: &str, raw: &str, source: PathBuf) -> CommandDef {
        let (frontmatter, body) = parse_frontmatter(raw);
        let name = frontmatter
            .get("name")
            .cloned()
            .unwrap_or_else(|| fallback_name.to_string());
        CommandDef {
            name,
            description: frontmatter.get("description").cloned().unwrap_or_default(),
            when_to_use: frontmatter
                .get("whenToUse")
                .or_else(|| frontmatter.get("when_to_use"))
                .cloned()
                .unwrap_or_default(),
            body: body.trim_start().to_string(),
            source,
        }
    }

    /// Commands compiled into the binary so they work in any working
    /// directory (not just repos that ship a `.claude/commands/` file).
    /// Seeded AFTER filesystem discovery with `or_insert`, so a project or
    /// user `<name>.md` still overrides the built-in.
    fn seed_builtins(&mut self) {
        const BUILTINS: &[(&str, &str)] = &[
            ("quiz", include_str!("default_prompts/commands/quiz.md")),
            (
                "quiz-result",
                include_str!("default_prompts/commands/quiz-result.md"),
            ),
        ];
        for (fallback_name, raw) in BUILTINS {
            let def = Self::parse_from_str(
                fallback_name,
                raw,
                PathBuf::from(format!("<builtin>/{fallback_name}")),
            );
            self.commands.entry(def.name.clone()).or_insert(def);
        }
    }

    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.commands.keys().map(String::as_str).collect();
        names.sort();
        names
    }

    pub fn get(&self, name: &str) -> Option<&CommandDef> {
        self.commands.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_file_extracts_frontmatter_and_body() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("deploy.md");
        std::fs::write(
            &path,
            "---\ndescription: Deploy stuff\n---\n\nDeploy to $ARGUMENTS now.\n",
        )
        .unwrap();
        let cmd = CommandStore::parse(&path).unwrap();
        assert_eq!(cmd.name, "deploy");
        assert_eq!(cmd.description, "Deploy stuff");
        assert_eq!(cmd.body, "Deploy to $ARGUMENTS now.");
    }

    #[test]
    fn render_substitutes_arguments() {
        let cmd = CommandDef {
            name: "deploy".into(),
            description: String::new(),
            when_to_use: String::new(),
            body: "Deploy to $ARGUMENTS now.".into(),
            source: PathBuf::new(),
        };
        assert_eq!(cmd.render("staging"), "Deploy to staging now.");
        assert_eq!(cmd.render("  staging  "), "Deploy to staging now.");
    }

    #[test]
    fn render_without_placeholder_appends_args() {
        let cmd = CommandDef {
            name: "deploy".into(),
            description: String::new(),
            when_to_use: String::new(),
            body: "Please deploy the app.".into(),
            source: PathBuf::new(),
        };
        assert_eq!(cmd.render(""), "Please deploy the app.");
        assert_eq!(cmd.render("to prod"), "Please deploy the app.\n\nto prod");
    }

    #[test]
    fn earlier_dir_wins_on_name_collision() {
        let proj = tempdir().unwrap();
        let user = tempdir().unwrap();
        std::fs::write(proj.path().join("hello.md"), "Project version of hello.").unwrap();
        std::fs::write(user.path().join("hello.md"), "User version of hello.").unwrap();

        let mut store = CommandStore::default();
        // Earlier dir loads first.
        store.load_dir(proj.path());
        store.load_dir(user.path());
        assert_eq!(
            store.get("hello").unwrap().body,
            "Project version of hello."
        );
    }

    #[test]
    fn builtin_quiz_is_seeded() {
        let mut store = CommandStore::default();
        store.seed_builtins();
        let q = store.get("quiz").expect("built-in /quiz should be seeded");
        assert_eq!(q.name, "quiz");
        assert!(
            q.body.contains("QuizRender"),
            "quiz body should call QuizRender"
        );
        assert!(
            q.body.contains("$ARGUMENTS"),
            "quiz body should take $ARGUMENTS"
        );
    }

    #[test]
    fn builtin_quiz_result_is_seeded() {
        let mut store = CommandStore::default();
        store.seed_builtins();
        let q = store
            .get("quiz-result")
            .expect("built-in /quiz-result should be seeded");
        assert_eq!(q.name, "quiz-result");
        assert!(
            q.body.contains("_scores"),
            "quiz-result body should read the _scores log"
        );
        assert!(
            q.body.contains("$ARGUMENTS"),
            "quiz-result body should take $ARGUMENTS"
        );
    }

    #[test]
    fn filesystem_command_overrides_builtin() {
        let mut store = CommandStore::default();
        store.commands.insert(
            "quiz".into(),
            CommandDef {
                name: "quiz".into(),
                description: "proj".into(),
                when_to_use: String::new(),
                body: "OVERRIDDEN".into(),
                source: PathBuf::from("proj/quiz.md"),
            },
        );
        store.seed_builtins();
        assert_eq!(store.get("quiz").unwrap().body, "OVERRIDDEN");
    }
}
