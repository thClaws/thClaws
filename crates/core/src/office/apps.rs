//! dev-plan/70: ArtCraft's office apps (GridCraft, WordCraft, DeckCraft) as
//! engine-managed MCP servers.
//!
//! The `<app>-cli` binaries ship beside the engine (desktop EE bundle) or in
//! `/opt/craft/bin` (runner image). Being present is the opt-in; settings
//! `officeApps` narrows or empties the set. Each server is spawned confined
//! to the workspace (dev-plan/49 confiner), Word and Deck additionally with
//! their own `--automation-*-root` flags, and every office file a tool call
//! saves is rewritten by [`super::fixup`] so Thai renders right in Office.

use crate::mcp::McpServerConfig;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// (settings key, MCP server name / binary stem, has its own folder roots)
pub const APPS: &[(&str, &str, bool)] = &[
    ("grid", "gridcraft", false),
    ("word", "wordcraft", true),
    ("deck", "deckcraft", true),
];

pub fn is_office_server(name: &str) -> bool {
    APPS.iter().any(|(_, server, _)| *server == name)
}

fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}-cli.exe")
    } else {
        format!("{stem}-cli")
    }
}

/// `THCLAWS_OFFICE_BIN_DIR`, else `craft/` beside the engine binary (desktop
/// bundle), else `/opt/craft/bin` (runner image).
fn bin_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let mut candidates = Vec::new();
        if let Some(d) = std::env::var_os("THCLAWS_OFFICE_BIN_DIR").filter(|d| !d.is_empty()) {
            candidates.push(PathBuf::from(d));
        }
        if let Some(d) = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| p.join("craft")))
        {
            candidates.push(d);
        }
        candidates.push(PathBuf::from("/opt/craft/bin"));
        candidates
            .into_iter()
            .find(|d| APPS.iter().any(|(_, s, _)| d.join(exe_name(s)).is_file()))
    })
    .as_deref()
}

/// Apps whose binary is installed. `None` from settings means all of them;
/// `THCLAWS_OFFICE_APPS` (comma list, empty = none) overrides both.
pub fn enabled(setting: Option<&[String]>) -> Vec<&'static str> {
    let Some(dir) = bin_dir() else {
        return Vec::new();
    };
    let env = std::env::var("THCLAWS_OFFICE_APPS").ok();
    let wanted: Option<Vec<String>> = match env.as_deref() {
        Some(s) => Some(
            s.split(',')
                .map(|p| p.trim().to_ascii_lowercase())
                .filter(|p| !p.is_empty())
                .collect(),
        ),
        None => setting.map(|s| s.iter().map(|p| p.trim().to_ascii_lowercase()).collect()),
    };
    APPS.iter()
        .filter(|(key, server, _)| {
            wanted
                .as_ref()
                .is_none_or(|w| w.iter().any(|x| x == key || x == server))
                && dir.join(exe_name(server)).is_file()
        })
        .map(|(_, server, _)| *server)
        .collect()
}

/// The injected server configs. Roots and confinement are added at spawn
/// ([`spawn_command`]) so they follow the workspace the server starts in.
pub fn mcp_configs(setting: Option<&[String]>) -> Vec<McpServerConfig> {
    let Some(dir) = bin_dir() else {
        return Vec::new();
    };
    enabled(setting)
        .into_iter()
        .map(|server| McpServerConfig {
            name: server.into(),
            transport: "stdio".into(),
            command: dir.join(exe_name(server)).to_string_lossy().into_owned(),
            args: vec!["mcp".into()],
            env: std::collections::HashMap::new(),
            url: String::new(),
            headers: std::collections::HashMap::new(),
            trusted: false,
            engine_managed: true,
        })
        .collect()
}

/// The command an engine-managed office server is spawned with: its own
/// folder roots (Word, Deck) on the current workspace, run through the OS
/// confiner when one is configured. `None` for any other server.
pub fn spawn_command(config: &McpServerConfig) -> Option<tokio::process::Command> {
    if !config.engine_managed {
        return None;
    }
    let &(_, _, has_roots) = APPS.iter().find(|(_, s, _)| *s == config.name)?;
    let workspace = crate::workdir::current_workdir();
    let mut args = config.args.clone();
    if has_roots {
        let root = workspace.to_string_lossy().into_owned();
        args.extend([
            "--automation-read-root".into(),
            root.clone(),
            "--automation-write-root".into(),
            root,
        ]);
    }
    let line = shell_words::join(
        std::iter::once(config.command.as_str()).chain(args.iter().map(String::as_str)),
    );
    let mut cmd = crate::confine::current_policy()
        .filter(confiner_works)
        .and_then(|policy| crate::confine::confined_command(&line, &policy))
        .unwrap_or_else(|| {
            let mut c = tokio::process::Command::new(&config.command);
            c.args(&args);
            c
        });
    cmd.current_dir(&workspace);
    Some(cmd)
}

/// Bash can fall back when the confiner bails at runtime (it prints a
/// sentinel and runs unconfined), but on an MCP server that sentinel lands in
/// the JSON-RPC stream and kills the handshake — seen on SIS runners, where
/// the Landlock selftest passes and the real ruleset fails with EBADFD. So
/// prove the confiner on a no-op once before trusting it with a server.
fn confiner_works(policy: &crate::confine::ConfinePolicy) -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let Some(cmd) = crate::confine::confined_command("true", policy) else {
            return false;
        };
        let std = cmd.as_std();
        let mut probe = std::process::Command::new(std.get_program());
        probe
            .args(std.get_args())
            .stdin(std::process::Stdio::null());
        for (k, v) in std.get_envs() {
            match v {
                Some(v) => probe.env(k, v),
                None => probe.env_remove(k),
            };
        }
        match probe.output() {
            Ok(out) => {
                out.status.success()
                    && !crate::confine::output_shows_no_enforce(&String::from_utf8_lossy(
                        &out.stdout,
                    ))
                    && !crate::confine::output_shows_no_enforce(&String::from_utf8_lossy(
                        &out.stderr,
                    ))
            }
            Err(_) => false,
        }
    })
}

/// Office files a tool result says were written: every `"path"`/`"files"`
/// string ending in .xlsx/.pptx/.docx, resolved against the workspace and
/// kept only when it stays inside it.
pub fn saved_paths(result: &str, workspace: &Path) -> Vec<PathBuf> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE
        .get_or_init(|| regex::Regex::new(r#""((?:[^"\\]|\\.)+\.(?i:xlsx|pptx|docx))""#).unwrap());
    let root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let mut out: Vec<PathBuf> = Vec::new();
    for m in re.captures_iter(result) {
        let raw = serde_json::from_str::<String>(&format!("\"{}\"", &m[1]))
            .unwrap_or_else(|_| m[1].to_string());
        let p = Path::new(&raw);
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            workspace.join(p)
        };
        if let Ok(c) = p.canonicalize() {
            if c.starts_with(&root) && c.is_file() && !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

/// Tools that only open or look at a file. Their results name the file too,
/// and fixing it then would rewrite a document the user never saved.
fn reads_only(tool: &str) -> bool {
    ["open_", "inspect_", "read_", "get_", "list_", "render_"]
        .iter()
        .any(|p| tool.starts_with(p))
        || matches!(
            tool,
            "screenshot" | "parity" | "ui_inspect" | "evaluate_formula"
        )
}

/// The tool that loads a file back into Word or Deck. GridCraft needs none:
/// its PDF goes through DocConvert, which reads the fixed file from disk.
fn reopen_tool(server: &str) -> Option<&'static str> {
    match server {
        "wordcraft" => Some("open_document"),
        "deckcraft" => Some("open_presentation"),
        _ => None,
    }
}

/// Called after every tool call on an office server: fix the files it saved,
/// then load each fixed file back into the app. Word and Deck export PDF from
/// the document they hold, so without the reload a PDF made after the save
/// misses the fixes (seen on SIS: 125% leading in the .pptx, not in its PDF).
/// Failures are logged, never surfaced — the file the app wrote is still there.
pub async fn after_call(client: &crate::mcp::McpClient, tool: &str, result: &str) {
    let server = client.name();
    if !is_office_server(server) || reads_only(tool) {
        return;
    }
    let paths = saved_paths(result, &crate::workdir::current_workdir());
    if paths.is_empty() {
        return;
    }
    let opts = super::fixup::FixupOptions::default();
    let fixed = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .filter(|p| match super::fixup::fixup_file(p, &opts) {
                Ok(changed) => changed,
                Err(e) => {
                    eprintln!(
                        "\x1b[33m[office] Thai fixup failed for {}: {e}\x1b[0m",
                        p.display()
                    );
                    false
                }
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let Some(open) = reopen_tool(server) else {
        return;
    };
    for p in fixed {
        let args = serde_json::json!({ "path": p.to_string_lossy() });
        if let Err(e) = client.call_tool(open, args).await {
            eprintln!(
                "\x1b[33m[office] reopening {} after the fixup failed: {e}\x1b[0m",
                p.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_paths_keeps_office_files_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join("out")).unwrap();
        std::fs::write(ws.join("out/งบ.xlsx"), b"x").unwrap();
        std::fs::write(ws.join("deck.pptx"), b"x").unwrap();
        std::fs::write(dir.path().join("outside.docx"), b"x").unwrap();
        let outside = dir
            .path()
            .join("outside.docx")
            .to_string_lossy()
            .into_owned();
        let result = format!(
            r#"{{"path": "out/งบ.xlsx", "bytes": 1}} {{"files": ["deck.pptx", "slide1.png"]}} {{"path": "{outside}"}} {{"path": "../outside.docx"}} {{"path": "missing.docx"}}"#
        );
        let got = saved_paths(&result, &ws);
        let ws = ws.canonicalize().unwrap();
        assert_eq!(got, vec![ws.join("out/งบ.xlsx"), ws.join("deck.pptx")]);
    }

    #[test]
    fn opening_or_reading_a_file_does_not_fix_it() {
        for tool in [
            "open_document",
            "open_workbook",
            "inspect_slide",
            "read_range",
            "get_text",
        ] {
            assert!(reads_only(tool), "{tool}");
        }
        for tool in [
            "save_workbook",
            "save_document",
            "export",
            "batch",
            "execute_command",
        ] {
            assert!(!reads_only(tool), "{tool}");
        }
    }

    #[test]
    fn only_engine_managed_office_servers_get_a_spawn_command() {
        let mut cfg = McpServerConfig {
            name: "wordcraft".into(),
            transport: "stdio".into(),
            command: "/opt/craft/bin/wordcraft-cli".into(),
            args: vec!["mcp".into()],
            env: Default::default(),
            url: String::new(),
            headers: Default::default(),
            trusted: false,
            engine_managed: false,
        };
        assert!(
            spawn_command(&cfg).is_none(),
            "a wordcraft from mcp.json is spawned as written"
        );
        cfg.engine_managed = true;
        let cmd = spawn_command(&cfg).expect("engine-managed office server");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter().any(|a| a == "--automation-write-root"),
            "{args:?}"
        );
        cfg.name = "browser".into();
        assert!(spawn_command(&cfg).is_none());
    }
}
