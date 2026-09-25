//! finding 11: make an agent's own paths survive the move into `bots/<slug>/`.
//!
//! In a v2 workspace the agent IS the workspace, so `python3
//! .thclaws/scripts/x.py` resolves next to the user's files and works. The
//! migration moves `.thclaws/` into the agent's folder and leaves the files at
//! the root, which is right — but a shell still runs at the root, so that same
//! command now reaches the HOST's `.thclaws/`, finds nothing, and the agent
//! reports an empty project rather than an error.
//!
//! Path resolution cannot fix a shell string (one cwd for the whole command),
//! so the engine hands the agent's folder over as `$THCLAWS_AGENT_DIR` and the
//! agent's own text is rewritten to use it. Migration does that in the same
//! pass that moves the files, so a workspace is never left in the broken
//! state; `thclaws bots fix-paths` does it for one already on disk.
//!
//! **The quoting is the whole difficulty.** The replacement goes INTO source
//! that is already a string in another language: a bare `"` closes the Python
//! or JavaScript literal it landed in. Writing this the naive way broke three
//! Python files and one JS template literal, each caught only by running
//! `py_compile` / `node --check` afterwards. [`quote_for`] is what decides,
//! and its tests are those four cases.

use regex::Regex;
use std::path::Path;

/// `.thclaws/` subpaths that belong to the WORKSPACE, not the agent: the
/// project KMS and the engine log, which every agent shares and which resolve
/// at the workspace root on purpose. Rewriting them would point an agent at a
/// private copy of the vault.
const SHARED: [&str; 2] = ["state/kms", "state/logs"];

/// Directories under the agent that hold what it ships. `state/` and
/// `sessions/` are deliberately absent: they are large, they are data, and
/// nothing in them is a path the agent executes.
const BUNDLED: [&str; 8] = [
    "scripts",
    "skills",
    "workflows",
    "agent_workflow",
    "gui-shell",
    "agents",
    "commands",
    "templates",
];

/// Skip anything bigger than this. A shipped script is kilobytes; a file this
/// size is data that happens to sit in a bundled folder.
const MAX_BYTES: u64 = 512 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Files changed.
    pub files: usize,
    /// Shell invocations rewritten.
    pub commands: usize,
    /// Python path literals rewritten.
    pub literals: usize,
    /// Files whose path literals were left alone because no import block was
    /// found to define `AGENT_DIR` after — reported rather than guessed at.
    pub skipped: Vec<String>,
}

impl Report {
    pub fn changed(&self) -> bool {
        self.files > 0
    }
}

/// True for the `.thclaws/…` tail of a path that is the agent's own.
fn agent_owned(rest: &str) -> bool {
    !SHARED
        .iter()
        .any(|s| rest == *s || rest.strip_prefix(s).is_some_and(|t| t.starts_with('/')))
}

/// Which quoting the replacement needs at `col` on `line`.
///
/// Returns the string to wrap the path in. Inside a double-quoted literal the
/// quotes must be escaped or they end it; inside single quotes or a JS
/// template literal a plain `"` is safe; outside any literal — a Markdown code
/// fence, which is most of what gets rewritten — it is safe too.
///
/// `$THCLAWS_AGENT_DIR` is used WITHOUT braces on purpose: `${…}` inside a JS
/// template literal is interpolation, and inside a Python f-string it is a
/// replacement field. Without braces neither language sees anything.
fn quote_for(line: &str, col: usize) -> &'static str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in line[..col].chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' | '\'' | '`' => match quote {
                Some(open) if open == ch => quote = None,
                Some(_) => {}
                None => quote = Some(ch),
            },
            _ => {}
        }
    }
    if quote == Some('"') {
        "\\\""
    } else {
        "\""
    }
}

/// Rewrite the shell invocations on one line. Returns the new line and how
/// many were rewritten.
fn rewrite_commands(re: &Regex, line: &str) -> (String, usize) {
    let mut out = String::with_capacity(line.len() + 32);
    let mut last = 0usize;
    let mut n = 0usize;
    for caps in re.captures_iter(line) {
        let whole = caps.get(0).expect("group 0 always matches");
        let runner = caps.get(1).expect("runner group");
        let path = caps.get(2).expect("path group");
        let rest = &path.as_str()[".thclaws/".len()..];
        if !agent_owned(rest) {
            continue;
        }
        let q = quote_for(line, whole.start());
        out.push_str(&line[last..whole.start()]);
        out.push_str(runner.as_str());
        out.push(' ');
        out.push_str(q);
        out.push_str("$THCLAWS_AGENT_DIR/");
        out.push_str(path.as_str());
        out.push_str(q);
        last = whole.end();
        n += 1;
    }
    out.push_str(&line[last..]);
    (out, n)
}

/// Point a Python file's own `.thclaws/…` literals at the agent's folder.
///
/// These are the script reading its own state — `Path(".thclaws/book.json")`
/// — and they resolve against the process cwd, which is the workspace root.
/// The replacement is an ordinary Python expression with balanced quotes, so
/// unlike the shell rewrite it carries no quoting hazard; what it does need is
/// `AGENT_DIR` defined, which goes in after the import block.
fn rewrite_py_literals(re: &Regex, text: &str) -> (String, usize) {
    let mut n = 0usize;
    let out = re.replace_all(text, |caps: &regex::Captures| {
        let call = &caps[1];
        let quote = &caps[2];
        let path = &caps[3];
        if !agent_owned(&path[".thclaws/".len()..]) {
            return caps[0].to_string();
        }
        n += 1;
        format!("{call}(AGENT_DIR / {quote}{path}{quote}")
    });
    (out.into_owned(), n)
}

/// Insert `AGENT_DIR` after the last top-level import. Returns None when the
/// file has none — better to leave it alone and say so than to guess a place.
fn define_agent_dir(text: &str) -> Option<String> {
    if text.contains("AGENT_DIR = ") {
        return Some(text.to_string());
    }
    let lines: Vec<&str> = text.lines().collect();
    let last_import = lines.iter().rposition(|l| {
        (l.starts_with("import ") || l.starts_with("from ")) && !l.contains("__future__")
    })?;
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let mut at = last_import + 1;
    if !lines
        .iter()
        .any(|l| l.starts_with("import os") || l.starts_with("from os"))
    {
        out.insert(at, "import os".to_string());
        at += 1;
    }
    out.insert(
        at,
        "\n# finding 11: what the agent ships lives in ITS folder, not at the\n\
         # workspace root — the user's files and the shared KMS still do.\n\
         AGENT_DIR = Path(os.environ.get(\"THCLAWS_AGENT_DIR\") or \".\")"
            .to_string(),
    );
    Some(out.join("\n") + if text.ends_with('\n') { "\n" } else { "" })
}

fn is_candidate(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string());
    let stem_ext = |p: &Path| -> Option<String> {
        let ext = p.extension()?.to_string_lossy().to_string();
        if ext == "tmpl" {
            Path::new(p.file_stem()?)
                .extension()
                .map(|e| e.to_string_lossy().to_string())
        } else {
            Some(ext)
        }
    };
    if name.is_some_and(|n| n.starts_with('.')) {
        return false;
    }
    matches!(
        stem_ext(path).as_deref(),
        Some("md") | Some("js") | Some("py")
    )
}

/// Rewrite one agent folder in place. With `dry_run` nothing is written and
/// the report says what would have changed.
pub fn fix(agent_dir: &Path, dry_run: bool) -> crate::error::Result<Report> {
    let shell = Regex::new(r#"\b(python3?|node|bash|sh) (\.thclaws/[^\s"'`)]+)"#)
        .map_err(|e| crate::error::Error::Tool(format!("shell pattern: {e}")))?;
    let pylit = Regex::new(r#"(Path|open)\((["'])(\.thclaws/[^"']*)["']"#)
        .map_err(|e| crate::error::Error::Tool(format!("literal pattern: {e}")))?;

    let mut roots: Vec<std::path::PathBuf> = BUNDLED
        .iter()
        .map(|d| agent_dir.join(".thclaws").join(d))
        .filter(|d| d.is_dir())
        .collect();
    // The agent's own instructions live at its root, not under `.thclaws/`.
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let p = agent_dir.join(name);
        if p.is_file() {
            roots.push(p);
        }
    }

    let mut report = Report::default();
    for root in roots {
        for entry in walkdir::WalkDir::new(&root)
            .max_depth(8)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            if !is_candidate(path) {
                continue;
            }
            if entry
                .metadata()
                .map(|m| m.len() > MAX_BYTES)
                .unwrap_or(true)
            {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };

            let mut cmds = 0usize;
            let rewritten: Vec<String> = text
                .lines()
                .map(|l| {
                    let (line, n) = rewrite_commands(&shell, l);
                    cmds += n;
                    line
                })
                .collect();
            let mut body = rewritten.join("\n");
            if text.ends_with('\n') {
                body.push('\n');
            }

            let mut lits = 0usize;
            if path.extension().is_some_and(|e| e == "py") {
                let (next, n) = rewrite_py_literals(&pylit, &body);
                if n > 0 {
                    match define_agent_dir(&next) {
                        Some(with_def) => {
                            body = with_def;
                            lits = n;
                        }
                        // No import block to define it after. Leaving the
                        // literals alone keeps the file running as it did.
                        None => report.skipped.push(path.to_string_lossy().to_string()),
                    }
                }
            }

            if cmds + lits == 0 || body == text {
                continue;
            }
            if !dry_run {
                std::fs::write(path, &body)?;
            }
            report.files += 1;
            report.commands += cmds;
            report.literals += lits;
        }
    }
    Ok(report)
}

/// Where the untouched copy goes before an automatic repair writes anything.
/// Under `state/` so a publish strips it and a sync keeps it.
const BACKUP_REL: &str = ".thclaws/state/pre-agent-dir";

/// Copy the bundled folders aside, once. Returns false when a backup is
/// already there — the repair is idempotent, so a second one would only
/// overwrite the original with already-rewritten text.
fn back_up(agent_dir: &Path) -> std::io::Result<bool> {
    let dst_root = agent_dir.join(BACKUP_REL);
    if dst_root.exists() {
        return Ok(false);
    }
    for name in BUNDLED {
        let src = agent_dir.join(".thclaws").join(name);
        if !src.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&src)
            .max_depth(8)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let Ok(rel) = entry.path().strip_prefix(agent_dir) else {
                continue;
            };
            let dst = dst_root.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &dst)?;
        }
    }
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let src = agent_dir.join(name);
        if src.is_file() {
            std::fs::create_dir_all(&dst_root)?;
            std::fs::copy(&src, dst_root.join(name))?;
        }
    }
    Ok(true)
}

/// Repair every agent in this workspace that still addresses its own files by
/// a bare relative path, keeping the original aside first.
///
/// Automatic on purpose. The symptom is an empty panel with no error, the
/// cause is invisible from the UI, and the repair is the same one migration
/// already runs — leaving it to a command the user has to hear about is what
/// made this cost people their work. The copy under [`BACKUP_REL`] is the
/// answer to doing it unasked.
///
/// Returns what was repaired, for the caller to report.
pub fn repair_stale(root: &Path) -> Vec<(String, Report)> {
    let mut out = Vec::new();
    for (slug, _) in super::migrate::stale_agents(root) {
        let dir = super::resolve_agent_dir(root, &slug);
        if let Err(e) = back_up(&dir) {
            // Without the copy, do not write. A broken agent that still has
            // its original text is recoverable; one silently rewritten wrong
            // is not.
            crate::util::log_line(&format!(
                "[bots] '{slug}' needs its paths repaired but could not be copied aside first \
                 ({e}) — left as it is; `thclaws bots fix-paths` after fixing the permissions"
            ));
            continue;
        }
        match fix(&dir, false) {
            Ok(r) if r.changed() => out.push((slug, r)),
            Ok(_) => {}
            Err(e) => crate::util::log_line(&format!("[bots] '{slug}' paths not repaired: {e}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four shapes that broke when this was first written by hand, each
    /// caught only by compiling the rewritten file afterwards.
    #[test]
    fn quoting_follows_the_literal_it_lands_in() {
        // Markdown / a bare shell line: nothing encloses it.
        assert_eq!(quote_for("python3 .thclaws/scripts/x.py", 0), "\"");
        // Inside a Python or JS double-quoted string: a bare quote would end it.
        assert_eq!(
            quote_for(r#"    "run `python3 .thclaws/x.py`","#, 17),
            "\\\""
        );
        // Single-quoted: a double quote is ordinary.
        assert_eq!(quote_for(r#"    'python3 .thclaws/x.py',"#, 5), "\"");
        // JS template literal: a double quote is ordinary there too.
        assert_eq!(quote_for("  dispatch(`!python3 .thclaws/x.py`)", 13), "\"");
    }

    #[test]
    fn a_closed_literal_does_not_count_as_enclosing() {
        // The string before it opened AND closed, so the match is outside.
        assert_eq!(
            quote_for(r#"cmd = "prefix" + python3 .thclaws/x.py"#, 25),
            "\""
        );
    }

    #[test]
    fn shared_subpaths_are_left_at_the_workspace_root() {
        assert!(!agent_owned("state/kms"));
        assert!(!agent_owned("state/kms/vault/page.md"));
        assert!(!agent_owned("state/logs/engine.log"));
        // Not a prefix match on a longer name.
        assert!(agent_owned("state/kmsx/thing"));
        assert!(agent_owned("book.json"));
        assert!(agent_owned("scripts/book.py"));
    }

    #[test]
    fn a_command_becomes_agent_relative() {
        let re = Regex::new(r#"\b(python3?|node|bash|sh) (\.thclaws/[^\s"'`)]+)"#).unwrap();
        let (out, n) = rewrite_commands(&re, "run `python3 .thclaws/scripts/book.py status`");
        assert_eq!(n, 1);
        assert_eq!(
            out,
            "run `python3 \"$THCLAWS_AGENT_DIR/.thclaws/scripts/book.py\" status`"
        );
    }

    #[test]
    fn the_shared_kms_is_not_rewritten() {
        let re = Regex::new(r#"\b(python3?|node|bash|sh) (\.thclaws/[^\s"'`)]+)"#).unwrap();
        let (out, n) = rewrite_commands(&re, "python3 .thclaws/state/kms/tool.py");
        assert_eq!(n, 0);
        assert_eq!(out, "python3 .thclaws/state/kms/tool.py");
    }

    #[test]
    fn a_python_literal_gets_the_agent_dir() {
        let re = Regex::new(r#"(Path|open)\((["'])(\.thclaws/[^"']*)["']"#).unwrap();
        let (out, n) = rewrite_py_literals(&re, r#"BIBLE = Path(".thclaws/book.json")"#);
        assert_eq!(n, 1);
        // The call is kept so the parenthesis still balances; `Path` of a
        // `Path` is the same path.
        assert_eq!(out, r#"BIBLE = Path(AGENT_DIR / ".thclaws/book.json")"#);
    }

    #[test]
    fn agent_dir_is_defined_after_the_imports() {
        let src = "import json\nfrom pathlib import Path\n\nX = 1\n";
        let out = define_agent_dir(src).unwrap();
        assert!(out.contains("import os"));
        assert!(out.contains("AGENT_DIR = Path(os.environ.get(\"THCLAWS_AGENT_DIR\") or \".\")"));
        // After the imports, before the code.
        let def = out.find("AGENT_DIR = ").unwrap();
        assert!(def > out.find("from pathlib").unwrap());
        assert!(def < out.find("X = 1").unwrap());
    }

    #[test]
    fn a_file_with_no_imports_is_reported_not_guessed_at() {
        assert!(define_agent_dir("X = 1\n").is_none());
    }

    #[test]
    fn already_fixed_is_left_alone() {
        let src = "import os\nfrom pathlib import Path\nAGENT_DIR = Path(\".\")\n";
        assert_eq!(define_agent_dir(src).unwrap(), src);
    }

    /// The copy is taken once. A second one would capture already-rewritten
    /// text and quietly become useless as a way back.
    #[test]
    fn the_backup_is_the_original_and_is_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join(".thclaws/skills");
        std::fs::create_dir_all(&skills).unwrap();
        let f = skills.join("SKILL.md");
        let original = "run `python3 .thclaws/scripts/book.py`\n";
        std::fs::write(&f, original).unwrap();

        assert!(back_up(dir.path()).unwrap(), "first copy is taken");
        fix(dir.path(), false).unwrap();
        assert!(!back_up(dir.path()).unwrap(), "second is refused");

        let kept = dir.path().join(BACKUP_REL).join(".thclaws/skills/SKILL.md");
        assert_eq!(
            std::fs::read_to_string(&kept).unwrap(),
            original,
            "the copy still holds the text as it was"
        );
        assert!(std::fs::read_to_string(&f)
            .unwrap()
            .contains("$THCLAWS_AGENT_DIR"));
    }

    #[test]
    fn rewriting_twice_changes_nothing_the_second_time() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = dir.path().join(".thclaws/skills");
        std::fs::create_dir_all(&scripts).unwrap();
        let f = scripts.join("SKILL.md");
        std::fs::write(&f, "run `python3 .thclaws/scripts/book.py`\n").unwrap();

        let first = fix(dir.path(), false).unwrap();
        assert_eq!(first.commands, 1);
        let after = std::fs::read_to_string(&f).unwrap();
        assert!(after.contains("$THCLAWS_AGENT_DIR/.thclaws/scripts/book.py"));

        let second = fix(dir.path(), false).unwrap();
        assert_eq!(second.commands, 0, "the rewritten path no longer matches");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), after);
    }
}
