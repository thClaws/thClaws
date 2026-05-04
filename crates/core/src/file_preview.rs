//! Filesystem-preview helpers powering the Files-tab IPC arms (and the
//! `--serve` mode equivalents). All three lifted from `gui.rs` in M6.36
//! SERVE9i so the WS transport's `file_list` / `file_read` / `file_write`
//! IPC arms can call them from the always-on dispatch table:
//!
//! - [`ospath`] — Windows path slash translator (no-op elsewhere)
//! - [`csv_to_markdown_table`] — CSV → GFM pipe-table for in-iframe preview
//! - [`render_markdown_to_html`] — themed standalone HTML doc wrapping
//!   GFM-rendered markdown (sandboxed iframe consumer)

use base64::Engine;

/// Convert a frontend-supplied path (always slash-separated, since it
/// comes from JSON / the React tree) to the OS-native form before
/// passing it to filesystem APIs. No-op on macOS/Linux. On Windows,
/// translates `/` → `\` so paths like `src/api/foo.ts` resolve via
/// `Sandbox::check` instead of being rejected as malformed.
pub fn ospath(path: &str) -> String {
    #[cfg(not(target_os = "windows"))]
    {
        path.to_string()
    }
    #[cfg(target_os = "windows")]
    {
        path.replace('/', "\\")
    }
}

/// Convert a CSV string to a GFM markdown pipe-table. First row is
/// treated as the header. Pipe characters in cells are escaped (`\|`)
/// so they don't break the row structure. Empty input → empty string.
/// Used by `file_read` to preview spreadsheet extracts via the same
/// markdown→HTML pipeline as `.md` files.
pub fn csv_to_markdown_table(csv: &str) -> String {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(csv.as_bytes());
    let rows: Vec<Vec<String>> = rdr
        .records()
        .filter_map(|r| r.ok())
        .map(|r| {
            r.iter()
                .map(|c| c.replace('|', "\\|").replace('\n', " "))
                .collect()
        })
        .collect();
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut out = String::new();
    let pad = |row: &[String], cols: usize| {
        let mut line = String::from("|");
        for i in 0..cols {
            line.push(' ');
            line.push_str(row.get(i).map(String::as_str).unwrap_or(""));
            line.push_str(" |");
        }
        line.push('\n');
        line
    };
    out.push_str(&pad(&rows[0], cols));
    let mut sep = String::from("|");
    for _ in 0..cols {
        sep.push_str(" --- |");
    }
    sep.push('\n');
    out.push_str(&sep);
    for row in &rows[1..] {
        out.push_str(&pad(row, cols));
    }
    out
}

/// Convert a markdown string to a full standalone HTML document
/// (sandboxed iframe consumer). GFM extensions: tables, strikethrough,
/// task lists, autolinks, footnotes, header ids. Raw HTML in source is
/// stripped (`render.unsafe_ = false`) so a `<script>` in a `.md` file
/// can't escape the iframe sandbox.
///
/// `theme` must be the *resolved* value (`"light"` or `"dark"`); the
/// frontend resolves `"system"` before sending so this function never
/// inspects an OS signal. Default = dark for back-compat when caller
/// passes anything else.
pub fn render_markdown_to_html(md: &str, theme: &str) -> String {
    let mut opts = comrak::ComrakOptions::default();
    opts.extension.table = true;
    opts.extension.strikethrough = true;
    opts.extension.tasklist = true;
    opts.extension.autolink = true;
    opts.extension.footnotes = true;
    opts.extension.header_ids = Some(String::new());
    opts.render.unsafe_ = false;
    let body = comrak::markdown_to_html(md, &opts);

    let (fg, bg, muted, accent, code_bg, border, color_scheme) = if theme == "light" {
        (
            "#1a1a1a", "#ffffff", "#606366", "#2867c4", "#f3f4f6", "#d0d7de", "light",
        )
    } else {
        (
            "#e6e6e6", "#1a1a1a", "#9aa0a6", "#6cb0ff", "#2a2a2a", "#333", "dark",
        )
    };

    format!(
        r##"<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  :root {{
    color-scheme: {color_scheme};
    --fg: {fg};
    --bg: {bg};
    --muted: {muted};
    --accent: {accent};
    --code-bg: {code_bg};
    --border: {border};
  }}
  html, body {{ margin: 0; padding: 0; }}
  body {{
    font: 14px/1.65 -apple-system, BlinkMacSystemFont, "Segoe UI",
          "Helvetica Neue", Arial, "Noto Sans Thai", sans-serif;
    color: var(--fg); background: var(--bg); padding: 24px 32px;
    max-width: 880px; margin: 0 auto;
  }}
  h1, h2, h3, h4, h5, h6 {{ line-height: 1.25; margin: 1.4em 0 0.5em; }}
  h1 {{ font-size: 1.8em; border-bottom: 1px solid var(--border); padding-bottom: 0.3em; }}
  h2 {{ font-size: 1.4em; border-bottom: 1px solid var(--border); padding-bottom: 0.25em; }}
  h3 {{ font-size: 1.2em; }}
  p, ul, ol, blockquote, pre, table {{ margin: 0.8em 0; }}
  a {{ color: var(--accent); text-decoration: none; }}
  a:hover {{ text-decoration: underline; }}
  code {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
          font-size: 0.92em; background: var(--code-bg);
          padding: 2px 5px; border-radius: 3px; }}
  pre {{ background: var(--code-bg); padding: 12px 14px; border-radius: 6px;
         overflow-x: auto; }}
  pre code {{ background: transparent; padding: 0; font-size: 0.9em; }}
  blockquote {{ margin: 0.8em 0; padding: 0 1em; color: var(--muted);
                border-left: 3px solid var(--border); }}
  table {{ border-collapse: collapse; }}
  th, td {{ border: 1px solid var(--border); padding: 6px 12px; text-align: left; }}
  th {{ background: var(--code-bg); font-weight: 600; }}
  hr {{ border: 0; border-top: 1px solid var(--border); margin: 2em 0; }}
  img {{ max-width: 100%; height: auto; }}
  ul.contains-task-list {{ list-style: none; padding-left: 1em; }}
  .task-list-item input[type="checkbox"] {{ margin-right: 0.5em; }}
</style>
</head><body>
{body}
</body></html>"##,
        body = body
    )
}

/// Base64-encode a binary file's bytes for the `file_content` envelope's
/// `content` field. Pure convenience wrapper around the standard
/// engine — saved as a top-level helper so the IPC layer doesn't have
/// to import the base64 crate just for this one call.
pub fn encode_bytes_b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ospath_is_noop_on_unix() {
        // CI runs Unix; Windows behavior is checked at compile time
        // via the cfg branches.
        #[cfg(not(target_os = "windows"))]
        {
            assert_eq!(ospath("src/api/foo.ts"), "src/api/foo.ts");
            assert_eq!(ospath(""), "");
        }
    }

    #[test]
    fn csv_to_markdown_renders_headers_and_rows() {
        let csv = "name,age\nAlice,30\nBob,25";
        let md = csv_to_markdown_table(csv);
        assert!(md.contains("| name | age |"));
        assert!(md.contains("| --- | --- |"));
        assert!(md.contains("| Alice | 30 |"));
        assert!(md.contains("| Bob | 25 |"));
    }

    #[test]
    fn csv_to_markdown_escapes_pipe_characters() {
        let csv = "col1,col2\n\"a|b\",c";
        let md = csv_to_markdown_table(csv);
        assert!(md.contains("a\\|b"));
    }

    #[test]
    fn csv_to_markdown_empty_input_yields_empty_string() {
        assert_eq!(csv_to_markdown_table(""), "");
    }

    #[test]
    fn render_markdown_includes_body_and_theme_palette() {
        let html = render_markdown_to_html("# Hello\n\nworld", "light");
        assert!(html.contains("<h1"));
        assert!(html.contains(">Hello"));
        assert!(html.contains(">world"));
        assert!(html.contains("color-scheme: light"));
    }

    #[test]
    fn render_markdown_strips_raw_html_for_safety() {
        // unsafe_ = false → comrak refuses to emit raw HTML in source
        // markdown. The exact rendering varies (comrak emits an HTML
        // comment placeholder), but the live <script> tag must NOT
        // appear. Pin the safety invariant rather than the exact
        // rendering — sandboxed iframe is the actual defense.
        let html = render_markdown_to_html("# Hi\n\n<script>alert(1)</script>", "dark");
        assert!(
            !html.contains("<script>alert"),
            "live <script> survived render: {html}"
        );
    }
}
