//! `thclaws shell {new,preview,pack,check}` CLI subcommands
//! (dev-plan/39 Tier 2).
//!
//! Lives under `gui_shell/` rather than `cloud/cmd.rs` because these
//! verbs operate purely on local shell folders — they don't talk to
//! the catalog at all. `cloud publish` from inside an agent folder
//! still picks up any `shells/` subdir the user scaffolded.

use std::fs;
use std::path::{Path, PathBuf};

use include_dir::{include_dir, Dir};

/// Templates bundled at compile time. Each subdirectory is one
/// starter template: chat-enhanced / grid / form / dashboard / kanban
/// / document / report. Authors clone these via `thclaws shell new`.
static TEMPLATES: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/assets/shell-templates");

/// Public list of template ids — drives the `--help` for `shell new`
/// and the catalog's "Start from template" gallery (Tier 4D).
pub fn template_ids() -> Vec<&'static str> {
    TEMPLATES
        .dirs()
        .filter_map(|d| d.path().file_name().and_then(|n| n.to_str()))
        .collect()
}

/// `thclaws shell new <template> <dest>` — copy a starter template
/// out of the embedded bundle to `dest`. Refuses to clobber an
/// existing non-empty folder unless `force` is set.
pub fn shell_new(template: &str, dest: &Path, force: bool) -> Result<Vec<PathBuf>, String> {
    let tpl = TEMPLATES.get_dir(template).ok_or_else(|| {
        format!(
            "unknown template '{}'. Available: {}",
            template,
            template_ids().join(", ")
        )
    })?;

    if dest.exists() {
        let has_content = fs::read_dir(dest)
            .map_err(|e| format!("read_dir {}: {e}", dest.display()))?
            .next()
            .is_some();
        if has_content && !force {
            return Err(format!(
                "{} is not empty. Use --force to overwrite or pick a fresh path.",
                dest.display()
            ));
        }
    } else {
        fs::create_dir_all(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    }

    let mut written: Vec<PathBuf> = Vec::new();
    extract_dir(tpl, dest, dest, &mut written)?;
    Ok(written)
}

fn extract_dir(
    src: &Dir<'_>,
    root: &Path,
    out: &Path,
    written: &mut Vec<PathBuf>,
) -> Result<(), String> {
    for sub in src.dirs() {
        let rel = sub.path().strip_prefix(src.path()).unwrap_or(sub.path());
        let dst = out.join(rel);
        fs::create_dir_all(&dst).map_err(|e| format!("create {}: {e}", dst.display()))?;
        extract_dir(sub, root, &dst, written)?;
    }
    for f in src.files() {
        let rel = f
            .path()
            .strip_prefix(src.path())
            .unwrap_or_else(|_| Path::new(f.path().file_name().unwrap()));
        let dst = out.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        fs::write(&dst, f.contents()).map_err(|e| format!("write {}: {e}", dst.display()))?;
        written.push(dst);
    }
    Ok(())
}

/// `thclaws shell check <path>` — lint a shell folder. Returns a list
/// of (severity, message) so the caller can pretty-print them.
pub fn shell_check(path: &Path) -> Result<Vec<(Severity, String)>, String> {
    let mut findings: Vec<(Severity, String)> = Vec::new();

    let manifest_path = super::manifest::find_manifest(path).ok_or_else(|| {
        format!(
            "missing shell.json (or manifest.json) in {}",
            path.display()
        )
    })?;
    let raw = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("read {}: {e}", manifest_path.display()))?;
    let manifest: super::manifest::ShellManifest = serde_json::from_str(&raw)
        .map_err(|e| format!("parse {}: {e}", manifest_path.display()))?;

    if manifest.id.is_empty() {
        findings.push((Severity::Error, "shell.id is empty".into()));
    }
    if !manifest
        .id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        findings.push((
            Severity::Error,
            format!(
                "shell.id '{}' must be lowercase letters/digits/hyphens",
                manifest.id
            ),
        ));
    }
    if manifest.name.is_empty() {
        findings.push((Severity::Error, "shell.name is empty".into()));
    }
    if manifest.version.is_empty() {
        findings.push((Severity::Error, "shell.version is empty".into()));
    }
    if manifest.entry.is_empty() {
        findings.push((Severity::Error, "shell.entry is empty".into()));
    } else {
        let entry = path.join(&manifest.entry);
        if !entry.exists() {
            findings.push((
                Severity::Error,
                format!("entry file '{}' does not exist", manifest.entry),
            ));
        }
    }

    // Soft warnings.
    if manifest.permissions.is_empty() {
        findings.push((
            Severity::Warning,
            "no permissions declared — required for marketplace publish (Tier 3 enforcement)"
                .into(),
        ));
    }
    if manifest.description.is_empty() {
        findings.push((
            Severity::Warning,
            "shell.description is empty — catalog UI will fall back to a placeholder".into(),
        ));
    }

    // Heuristic: warn on cross-origin <script src=…> which the CSP
    // sandbox will block in Mode B.
    let entry = path.join(&manifest.entry);
    if let Ok(html) = fs::read_to_string(&entry) {
        for m in regex::Regex::new(r#"<script[^>]*src=["'](https?://[^"']+)["']"#)
            .unwrap()
            .captures_iter(&html)
        {
            findings.push((
                Severity::Warning,
                format!(
                    "cross-origin script load '{}' — may be blocked by the shell sandbox CSP",
                    &m[1]
                ),
            ));
        }
    }

    Ok(findings)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// `thclaws shell pack <src> <dest.html>` — bundle a shell folder
/// into a single self-contained HTML file (MVP: just emit the entry
/// HTML with `<style>` and `<script>` tags inlined from sibling
/// `style.css` / `script.js` if present; doesn't yet vendor remote
/// imports). Returns the written path.
pub fn shell_pack(src: &Path, dest: &Path) -> Result<PathBuf, String> {
    let manifest_path = super::manifest::find_manifest(src)
        .ok_or_else(|| format!("missing shell.json (or manifest.json) in {}", src.display()))?;
    let raw = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("read {}: {e}", manifest_path.display()))?;
    let manifest: super::manifest::ShellManifest = serde_json::from_str(&raw)
        .map_err(|e| format!("parse {}: {e}", manifest_path.display()))?;
    let entry = src.join(&manifest.entry);
    let html = fs::read_to_string(&entry).map_err(|e| format!("read {}: {e}", entry.display()))?;

    let mut packed = html;
    let css = src.join("style.css");
    if css.exists() {
        let s = fs::read_to_string(&css).map_err(|e| format!("read style.css: {e}"))?;
        let inject = format!("<style>{s}</style>");
        packed = packed.replacen("</head>", &format!("{inject}\n</head>"), 1);
    }
    let js = src.join("script.js");
    if js.exists() {
        let s = fs::read_to_string(&js).map_err(|e| format!("read script.js: {e}"))?;
        let inject = format!("<script>{s}</script>");
        packed = packed.replacen("</body>", &format!("{inject}\n</body>"), 1);
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    fs::write(dest, packed).map_err(|e| format!("write {}: {e}", dest.display()))?;
    Ok(dest.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // event_render sends "text" as a bare string and "done" as null; a
    // template that destructures them streams "undefined" or throws.
    #[test]
    fn templates_take_event_payloads_as_sent() {
        let mut n = 0;
        for d in TEMPLATES.dirs() {
            for f in d
                .files()
                .filter(|f| f.path().extension().is_some_and(|e| e == "html"))
            {
                let src = f.contents_utf8().unwrap();
                for bad in ["({ delta })", "({ message })", "ev.runId", "ev.text"] {
                    assert!(!src.contains(bad), "{} uses {bad}", f.path().display());
                }
                n += 1;
            }
        }
        assert!(n >= 7, "only {n} templates scanned");
    }

    #[test]
    fn template_ids_covers_seven_canonical() {
        let ids = template_ids();
        for name in [
            "chat-enhanced",
            "grid",
            "form",
            "dashboard",
            "kanban",
            "document",
            "report",
        ] {
            assert!(ids.contains(&name), "missing template {name}");
        }
    }

    #[test]
    fn shell_new_writes_expected_files() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("my-grid");
        let written = shell_new("grid", &dest, false).unwrap();
        assert!(written.iter().any(|p| p.ends_with("index.html")));
        assert!(written.iter().any(|p| p.ends_with("shell.json")));
        assert!(written.iter().any(|p| p.ends_with("README.md")));
        assert!(written.iter().any(|p| p.ends_with("mock.json")));
    }

    #[test]
    fn shell_new_refuses_to_overwrite_non_empty_without_force() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("existing");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("readme.txt"), "hello").unwrap();
        let err = shell_new("grid", &dest, false).unwrap_err();
        assert!(err.contains("not empty"));
    }

    #[test]
    fn shell_new_force_overwrites() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("existing");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("readme.txt"), "hello").unwrap();
        shell_new("grid", &dest, true).unwrap();
        assert!(dest.join("index.html").exists());
    }

    #[test]
    fn shell_new_unknown_template_errors() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("x");
        let err = shell_new("does-not-exist", &dest, false).unwrap_err();
        assert!(err.contains("unknown template"));
    }

    #[test]
    fn shell_check_passes_clean_template() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("ok");
        shell_new("grid", &dest, false).unwrap();
        let findings = shell_check(&dest).unwrap();
        // Grid template has permissions declared so should produce zero
        // errors (warnings might appear from cross-origin script
        // detection — guard against false positives by checking
        // severity only).
        assert!(
            findings.iter().all(|(s, _)| *s == Severity::Warning),
            "expected only warnings; got: {findings:?}"
        );
    }

    #[test]
    fn shell_check_flags_missing_entry() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("broken");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("shell.json"),
            r#"{"id":"x","name":"X","version":"0.1.0","description":"d",
              "entry":"missing.html","permissions":["agent.run"]}"#,
        )
        .unwrap();
        let findings = shell_check(&dir).unwrap();
        assert!(findings
            .iter()
            .any(|(s, m)| *s == Severity::Error && m.contains("missing.html")));
    }

    #[test]
    fn shell_pack_emits_self_contained_html() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("g");
        shell_new("grid", &src, false).unwrap();
        let dest = tmp.path().join("packed.html");
        shell_pack(&src, &dest).unwrap();
        let body = fs::read_to_string(&dest).unwrap();
        // Grid template embeds CSS+JS inline in index.html already, so
        // the packed output should at minimum be a valid HTML document.
        assert!(body.contains("<!doctype html>"));
        assert!(body.contains("</html>"));
    }
}
