//! Configurable prompt templates.
//!
//! Every user-facing prompt used by the agent can be overridden by dropping a
//! markdown file into `.thclaws/prompt/<name>.md` (project level) or
//! `~/.config/thclaws/prompt/<name>.md` (user level). Project wins over user;
//! both win over the built-in default.
//!
//! Templates support `{variable}` substitution. Unknown placeholders are left
//! untouched so users notice typos.

use std::path::PathBuf;

const DIR: &str = "prompt";

/// Built-in default templates. These are the bytes of the markdown files under
/// `src/default_prompts/`, embedded at compile time. The same files should
/// serve as the canonical reference for authors writing overrides into
/// `.thclaws/prompt/`.
pub mod defaults {
    pub const SYSTEM: &str = include_str!("default_prompts/system.md");
    pub const LEAD: &str = include_str!("default_prompts/lead.md");
    pub const AGENT_TEAM: &str = include_str!("default_prompts/agent_team.md");
    pub const SUBAGENT: &str = include_str!("default_prompts/subagent.md");
    pub const WORKTREE: &str = include_str!("default_prompts/worktree.md");
    pub const COMPACTION: &str = include_str!("default_prompts/compaction.md");
    pub const COMPACTION_SYSTEM: &str = include_str!("default_prompts/compaction_system.md");
}

fn project_path(name: &str) -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".thclaws")
        .join(DIR)
        .join(format!("{name}.md"))
}

fn user_path(name: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("thclaws").join(DIR).join(format!("{name}.md")))
}

/// Load a prompt template by name. Returns the override content (project →
/// user) if present, otherwise the built-in default string. Branding
/// placeholders (`{product}`, `{support_email}`) are substituted before
/// returning so any prompt — built-in default, project override, user
/// override — picks up the active branding without per-callsite work.
pub fn load(name: &str, default: &str) -> String {
    let raw = if let Ok(s) = std::fs::read_to_string(project_path(name)) {
        s
    } else if let Some(p) = user_path(name) {
        std::fs::read_to_string(p).unwrap_or_else(|_| default.to_string())
    } else {
        default.to_string()
    };
    crate::branding::apply_template(&raw)
}

/// Replace `{key}` occurrences with the corresponding values. Unknown
/// placeholders are left in place so typos are visible.
pub fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// Load-and-render in one call.
pub fn render_named(name: &str, default: &str, vars: &[(&str, &str)]) -> String {
    render(&load(name, default), vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_substitutes_known_keys() {
        let out = render(
            "hello {name}, you are {role}",
            &[("name", "ada"), ("role", "lead")],
        );
        assert_eq!(out, "hello ada, you are lead");
    }

    #[test]
    fn render_leaves_unknown_keys_alone() {
        let out = render("hi {name} — {missing}", &[("name", "ada")]);
        assert_eq!(out, "hi ada — {missing}");
    }

    #[test]
    fn load_falls_back_to_default_when_no_override() {
        let out = load("__nonexistent_prompt_xyz__", "DEFAULT");
        assert_eq!(out, "DEFAULT");
    }

    #[test]
    fn load_applies_branding_to_product_placeholder() {
        // The default branding (open-core, no policy active) substitutes
        // `{product}` with "thClaws". Critical for system.md, which now
        // says "You are {product}" — without this substitution the agent
        // would literally introduce itself as "{product}".
        let template = "I am {product}.";
        let out = load("__nonexistent_for_test__", template);
        assert_eq!(out, "I am thClaws.");
    }

    #[test]
    fn load_applies_branding_to_default_system_prompt() {
        // The actual built-in system.md template starts with
        // "You are {product}, …" — confirm it round-trips through `load`
        // with the placeholder substituted. Test guards against future
        // bypasses of `branding::apply_template` in the load path.
        let out = load("__nonexistent_for_test__", defaults::SYSTEM);
        assert!(
            out.starts_with("You are thClaws,"),
            "system.md substitution missing — got: {}",
            out.lines().next().unwrap_or("")
        );
        assert!(
            !out.contains("{product}"),
            "{{product}} placeholder leaked into rendered prompt"
        );
    }

    #[test]
    fn default_system_prompt_distinguishes_todowrite_from_submitplan() {
        // Both surfaces must appear in the system prompt with their
        // distinct roles spelled out, so the model picks the right
        // tool without us having to teach it from scratch every turn.
        // Regression guard for M6.6 — the casual-vs-structured
        // distinction was the load-bearing addition.
        let s = defaults::SYSTEM;
        assert!(
            s.contains("SubmitPlan"),
            "SubmitPlan not mentioned: missing in system prompt"
        );
        assert!(
            s.contains("TodoWrite"),
            "TodoWrite not mentioned: missing in system prompt"
        );
        assert!(
            s.contains("scratchpad"),
            "TodoWrite must be framed as a scratchpad in the system prompt",
        );
        assert!(
            s.contains("sidebar"),
            "SubmitPlan's sidebar/visibility property must be named so the model knows when to use it",
        );
    }

    #[test]
    fn default_system_prompt_routes_user_plan_word_correctly() {
        // Users routinely say "plan to do X" colloquially — meaning
        // "let's organize this work", NOT "enter formal plan mode".
        // The system prompt must teach the model to decide on the
        // *work*, not the literal word — small jobs → TodoWrite,
        // big jobs (real per-step actions + runnable verifications)
        // → EnterPlanMode + SubmitPlan. Regression guard for M6.6:
        // user explicitly called this out as load-bearing.
        let s = defaults::SYSTEM;
        assert!(
            s.contains("Picking the right one when the user says \"plan\""),
            "section header for plan-routing missing: should be present in system prompt",
        );
        assert!(
            s.contains("Don't reflexively enter plan mode"),
            "anti-reflex guidance missing — model must not auto-enter plan mode on every \"plan\" mention",
        );
        assert!(
            s.contains("Small job") && s.contains("TodoWrite"),
            "small-job → TodoWrite branch missing",
        );
        assert!(
            s.contains("Big job") && s.contains("SubmitPlan"),
            "big-job → SubmitPlan branch missing",
        );
        // Concrete examples — these anchor the abstract rule.
        assert!(
            s.contains("plan to rename") || s.contains("plan to add"),
            "TodoWrite-side example missing",
        );
        assert!(
            s.contains("plan to build a webapp") || s.contains("plan to migrate"),
            "SubmitPlan-side example missing",
        );
    }

    #[test]
    fn default_system_prompt_tells_model_to_check_todos_md_at_session_start() {
        // The "resume from existing todos.md" behaviour was the user's
        // specific ask in M6.6 — the system prompt must instruct the
        // model to look for the file and surface incomplete items
        // before starting fresh work. Sharpened post-test (gpt-4.1
        // didn't follow the original conditional wording) — the
        // directive is now unconditional and front-loaded.
        let s = defaults::SYSTEM;
        assert!(
            s.contains(".thclaws/todos.md"),
            "system prompt must name the todos file path",
        );
        assert!(
            s.contains("BEFORE asking the user"),
            "must instruct to check todos.md BEFORE asking for context",
        );
        assert!(
            s.contains("ALWAYS check"),
            "must use unconditional ALWAYS framing",
        );
        assert!(
            s.to_lowercase().contains("incomplete"),
            "must mention incomplete items as the resume target",
        );
        assert!(
            s.contains("resume"),
            "must offer resume as the option for existing todos",
        );
        assert!(
            s.contains("Don't ask"),
            "must explicitly forbid asking when a todo file already has answers",
        );
    }
}
