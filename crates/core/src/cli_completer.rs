//! Tab-completion + inline hints for slash commands in the CLI REPL.
//!
//! Backed by `crate::repl::built_in_commands()` so the CLI list never drifts
//! from the GUI's `/` popup. Two readline hooks combine for the UX:
//!
//! * `Completer` — Tab cycles through matching commands.
//! * `Hinter` — as you type `/he`, faded ghost-text shows the remainder
//!   (`lp`); Right-arrow accepts.
//!
//! Only top-level command names participate; subcommand/argument completion
//! is a follow-up.

use std::borrow::Cow;

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper, Result};

pub(crate) struct SlashCompleter;

impl SlashCompleter {
    /// Common prefix-match logic used by both `complete` and `hint`. Returns
    /// `Some(typed_chars_after_slash)` when the cursor is inside a
    /// slash-prefixed first token and there's at least one char after the
    /// slash; otherwise `None`.
    fn slash_prefix(line: &str, pos: usize) -> Option<&str> {
        let prefix = line.get(..pos)?;
        if !prefix.starts_with('/') || prefix.contains(char::is_whitespace) {
            return None;
        }
        Some(&prefix[1..])
    }
}

impl Completer for SlashCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> Result<(usize, Vec<Pair>)> {
        let Some(typed) = Self::slash_prefix(line, pos) else {
            return Ok((pos, Vec::new()));
        };
        let candidates = crate::repl::built_in_commands()
            .iter()
            .filter(|c| c.name.starts_with(typed))
            .map(|c| Pair {
                display: format!("/{:<14} {}", c.name, c.description),
                replacement: format!("/{} ", c.name),
            })
            .collect();
        Ok((0, candidates))
    }
}

impl Hinter for SlashCompleter {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        // Require at least one char after the slash so we don't pick a
        // hint arbitrarily out of all 30+ commands.
        let typed = Self::slash_prefix(line, pos)?;
        if typed.is_empty() {
            return None;
        }
        crate::repl::built_in_commands()
            .iter()
            .find(|c| c.name.starts_with(typed))
            .map(|c| c.name[typed.len()..].to_string())
    }
}

impl Highlighter for SlashCompleter {
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        // ANSI dim so the ghost-text reads as a suggestion, not real input.
        Cow::Owned(format!("\x1b[2m{hint}\x1b[0m"))
    }
}

impl Validator for SlashCompleter {}
impl Helper for SlashCompleter {}
