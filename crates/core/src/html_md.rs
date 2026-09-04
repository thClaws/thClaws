//! HTML → Markdown for KMS source ingest.
//!
//! `/kms ingest <kms> <url>` used to write the fetched bytes straight
//! into `sources/<alias>.md`. For any real web page that means the
//! archive is a `<!doctype html>` blob: unreadable in the viewer,
//! useless as a BM25 document (every hit scores on `div`/`class`
//! noise), and it poisons the index summary, which is derived from the
//! source's first line.
//!
//! There is no HTML-to-text converter anywhere else in the tree and
//! pulling a readability crate in for one ingest path is not worth the
//! dependency, so this is a small, dependency-free converter: strip the
//! non-content elements, prefer `<main>`/`<article>` when the page
//! marks one, and emit block-level Markdown for the rest.
//!
//! Deliberately *not* a general-purpose HTML parser. It does not build
//! a DOM, does not handle malformed nesting cleverly, and does not try
//! to reproduce layout. It converts the ~95% of pages whose content is
//! ordinary block markup, and degrades to readable plain text on the
//! rest — which is still strictly better than storing tag soup.

/// Elements whose entire subtree is dropped: script/style carry no
/// prose, and nav/footer/aside/form are chrome that would otherwise
/// dominate the archived text.
const DROP_SUBTREE: &[&str] = &[
    "script", "style", "noscript", "svg", "head", "nav", "footer", "aside", "form", "iframe",
    "canvas", "template", "button", "select", "video", "audio",
];

/// Block-level elements that force a paragraph break.
const BLOCK: &[&str] = &[
    "p",
    "div",
    "section",
    "article",
    "main",
    "header",
    "ul",
    "ol",
    "dl",
    "dd",
    "dt",
    "table",
    "figure",
    "figcaption",
    "address",
    "details",
    "summary",
];

/// Convert an HTML document to Markdown. Returns `(title, markdown)` —
/// `title` comes from `<title>`, falling back to the first `<h1>`, and
/// is empty when the document has neither.
pub fn convert(html: &str) -> (String, String) {
    let title = extract_title(html);
    let scoped = main_content(html);
    let md = render(scoped);
    (title, tidy(&md))
}

/// True when the bytes look like an HTML document rather than
/// markdown/plain text. Checked before conversion so `/kms ingest` of a
/// URL serving markdown or JSON is left byte-exact.
pub fn looks_like_html(body: &str) -> bool {
    let head: String = body.chars().take(4096).collect::<String>().to_lowercase();
    let head = head.trim_start();
    head.starts_with("<!doctype html")
        || head.starts_with("<html")
        || (head.contains("<body") && head.contains('<'))
        || (head.contains("<div") && head.contains("</div>"))
        || head.contains("<meta ")
}

/// Narrow to the page's main content when it declares one. Cheap
/// substring scoping rather than DOM analysis: find the outermost
/// `<main>` or `<article>` and return its inner slice. Falls back to
/// `<body>`, then the whole document.
fn main_content(html: &str) -> &str {
    for tag in ["main", "article"] {
        if let Some(inner) = inner_of(html, tag) {
            // A nav-only <article> teaser block is not the content;
            // require some substance before trusting the scope.
            if inner.len() > 500 {
                return inner;
            }
        }
    }
    inner_of(html, "body").unwrap_or(html)
}

/// Inner slice of the first `<tag ...>` … matching `</tag>`, honouring
/// nesting so an outer `<div>`-wrapped `<article>` containing another
/// `<article>` returns the outer one's full body.
fn inner_of<'a>(html: &'a str, tag: &str) -> Option<&'a str> {
    let lower = html.to_ascii_lowercase();
    let open_pat = format!("<{tag}");
    let close_pat = format!("</{tag}");
    let open = lower.find(&open_pat)?;
    // Skip to the end of the opening tag.
    let body_start = open + lower[open..].find('>')? + 1;
    let mut depth = 1usize;
    let mut cursor = body_start;
    while depth > 0 {
        let next_open = lower[cursor..].find(&open_pat).map(|i| cursor + i);
        let next_close = lower[cursor..].find(&close_pat).map(|i| cursor + i)?;
        match next_open {
            Some(o) if o < next_close => {
                depth += 1;
                cursor = o + open_pat.len();
            }
            _ => {
                depth -= 1;
                if depth == 0 {
                    return Some(&html[body_start..next_close]);
                }
                cursor = next_close + close_pat.len();
            }
        }
    }
    None
}

fn extract_title(html: &str) -> String {
    if let Some(inner) = inner_of(html, "title") {
        let t = tidy_inline(&decode_entities(&strip_tags(inner)));
        if !t.is_empty() {
            return t;
        }
    }
    if let Some(inner) = inner_of(html, "h1") {
        return tidy_inline(&decode_entities(&strip_tags(inner)));
    }
    String::new()
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// One list frame — `ordered` picks the marker, `n` is the running
/// item number for ordered lists.
struct ListFrame {
    ordered: bool,
    n: usize,
}

/// The single pass. Walks the input character by character, emitting
/// Markdown as tags open and close. State is a handful of counters
/// rather than a tree — sufficient because every construct we emit is
/// decided by the tag that opens it plus the current list/pre depth.
fn render(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0usize;
    let mut lists: Vec<ListFrame> = Vec::new();
    let mut pre_depth = 0usize;
    let mut skip_until: Option<String> = None;
    let mut skip_depth = 0usize;
    let mut pending_link: Option<String> = None;
    let mut link_text = String::new();
    // Table state — cells accumulate until </tr>, rows until </table>.
    let mut in_table = 0usize;
    let mut row: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut header_row = false;
    let mut cell = String::new();
    let mut in_cell = false;

    while i < html.len() {
        if bytes[i] != b'<' {
            let mut j = i + 1;
            while j < html.len() && bytes[j] != b'<' {
                j += 1;
            }
            let text = &html[i..j];
            i = j;
            if skip_until.is_some() {
                continue;
            }
            let decoded = decode_entities(text);
            let piece = if pre_depth > 0 {
                decoded
            } else {
                collapse_ws(&decoded)
            };
            if piece.is_empty() {
                continue;
            }
            if in_cell {
                cell.push_str(&piece);
            } else if pending_link.is_some() {
                link_text.push_str(&piece);
            } else {
                push_text(&mut out, &piece, pre_depth > 0);
            }
            continue;
        }

        // A '<' that isn't a tag start (bare "a < b") is literal text.
        let Some(close_rel) = html[i..].find('>') else {
            let text = decode_entities(&html[i..]);
            push_text(&mut out, &collapse_ws(&text), false);
            break;
        };
        let raw_tag = &html[i + 1..i + close_rel];
        i += close_rel + 1;

        if raw_tag.starts_with('!') {
            continue; // comment / doctype
        }
        let closing = raw_tag.starts_with('/');
        let name_src = if closing { &raw_tag[1..] } else { raw_tag };
        let name: String = name_src
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }

        // Subtree skipping.
        if let Some(target) = &skip_until {
            if &name == target {
                if closing {
                    skip_depth -= 1;
                    if skip_depth == 0 {
                        skip_until = None;
                    }
                } else if !raw_tag.trim_end().ends_with('/') {
                    skip_depth += 1;
                }
            }
            continue;
        }
        if !closing && DROP_SUBTREE.contains(&name.as_str()) {
            if !raw_tag.trim_end().ends_with('/') {
                skip_until = Some(name.clone());
                skip_depth = 1;
            }
            continue;
        }

        match (closing, name.as_str()) {
            (false, "br") => {
                if in_cell {
                    cell.push(' ');
                } else {
                    out.push('\n');
                }
            }
            (false, "hr") => push_block(&mut out, "\n---\n"),
            (false, "img") => {
                let alt = attr(raw_tag, "alt").unwrap_or_default();
                if let Some(src) = attr(raw_tag, "src") {
                    let piece = format!("![{}]({})", tidy_inline(&alt), src.trim());
                    if in_cell {
                        cell.push_str(&piece);
                    } else {
                        push_text(&mut out, &piece, false);
                    }
                }
            }
            (false, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") => {
                let level: usize = name[1..].parse().unwrap_or(1);
                push_block(&mut out, "");
                out.push_str(&"#".repeat(level));
                out.push(' ');
            }
            (true, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") => push_block(&mut out, ""),
            (false, "pre") => {
                pre_depth += 1;
                push_block(&mut out, "```\n");
            }
            (true, "pre") => {
                pre_depth = pre_depth.saturating_sub(1);
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n\n");
            }
            (false, "code") if pre_depth == 0 => out.push('`'),
            (true, "code") if pre_depth == 0 => out.push('`'),
            (false, "strong" | "b") => out.push_str("**"),
            (true, "strong" | "b") => out.push_str("**"),
            (false, "em" | "i") => out.push('*'),
            (true, "em" | "i") => out.push('*'),
            (false, "blockquote") => push_block(&mut out, "\n> "),
            (true, "blockquote") => push_block(&mut out, ""),
            (false, "ul") => {
                push_block(&mut out, "");
                lists.push(ListFrame {
                    ordered: false,
                    n: 0,
                });
            }
            (false, "ol") => {
                push_block(&mut out, "");
                lists.push(ListFrame {
                    ordered: true,
                    n: 0,
                });
            }
            (true, "ul" | "ol") => {
                lists.pop();
                push_block(&mut out, "");
            }
            (false, "li") => {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                let depth = lists.len().saturating_sub(1);
                out.push_str(&"  ".repeat(depth));
                match lists.last_mut() {
                    Some(f) if f.ordered => {
                        f.n += 1;
                        out.push_str(&format!("{}. ", f.n));
                    }
                    _ => out.push_str("- "),
                }
            }
            (true, "li") => out.push('\n'),
            (false, "a") => {
                if let Some(href) = attr(raw_tag, "href") {
                    let href = href.trim();
                    // In-page anchors and javascript: handlers add
                    // nothing to an archived copy — keep the text.
                    if !href.is_empty()
                        && !href.starts_with('#')
                        && !href.starts_with("javascript:")
                    {
                        pending_link = Some(href.to_string());
                        link_text.clear();
                    }
                }
            }
            (true, "a") => {
                if let Some(href) = pending_link.take() {
                    let text = tidy_inline(&link_text);
                    let piece = if text.is_empty() {
                        String::new()
                    } else {
                        format!("[{text}]({href})")
                    };
                    link_text.clear();
                    if in_cell {
                        cell.push_str(&piece);
                    } else {
                        push_text(&mut out, &piece, false);
                    }
                }
            }
            (false, "table") => {
                in_table += 1;
                rows.clear();
                header_row = false;
                push_block(&mut out, "");
            }
            (true, "table") => {
                in_table = in_table.saturating_sub(1);
                out.push_str(&render_table(&rows, header_row));
                rows.clear();
            }
            (false, "tr") => row.clear(),
            (true, "tr") => {
                if !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                }
            }
            (false, "th") => {
                header_row = header_row || rows.is_empty();
                in_cell = true;
                cell.clear();
            }
            (false, "td") => {
                in_cell = true;
                cell.clear();
            }
            (true, "th" | "td") => {
                in_cell = false;
                row.push(tidy_inline(&cell));
                cell.clear();
            }
            (false, other) if BLOCK.contains(&other) && in_table == 0 => push_block(&mut out, ""),
            (true, other) if BLOCK.contains(&other) && in_table == 0 => push_block(&mut out, ""),
            _ => {}
        }
    }
    out
}

/// Append text, inserting a separating space when the previous
/// character would otherwise glue two words together.
fn push_text(out: &mut String, text: &str, raw: bool) {
    if text.is_empty() {
        return;
    }
    if raw {
        out.push_str(text);
        return;
    }
    let needs_space =
        text.starts_with(' ') && matches!(out.chars().last(), Some(c) if c != ' ' && c != '\n');
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        if needs_space {
            out.push(' ');
        }
        return;
    }
    if needs_space {
        out.push(' ');
    }
    out.push_str(trimmed);
}

/// Start a new block: ensure exactly one blank line separates it from
/// whatever came before, then append `lead`.
fn push_block(out: &mut String, lead: &str) {
    while out.ends_with(' ') {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with("\n\n") {
        if out.ends_with('\n') {
            out.push('\n');
        } else {
            out.push_str("\n\n");
        }
    }
    out.push_str(lead);
}

fn render_table(rows: &[Vec<String>], header: bool) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if width == 0 {
        return String::new();
    }
    let cell = |r: &Vec<String>, i: usize| -> String {
        r.get(i).map(|s| s.replace('|', "\\|")).unwrap_or_default()
    };
    // The first row is the header row either way: with `<th>` it is
    // explicit, and without it GFM still requires one, so the first
    // data row is promoted rather than dropped.
    let _ = header;
    let (head, body) = (&rows[0], &rows[1..]);
    let mut out = String::from("\n");
    out.push_str(&format!(
        "| {} |\n",
        (0..width)
            .map(|i| cell(head, i))
            .collect::<Vec<_>>()
            .join(" | ")
    ));
    out.push_str(&format!("|{}|\n", " --- |".repeat(width)));
    for r in body {
        out.push_str(&format!(
            "| {} |\n",
            (0..width)
                .map(|i| cell(r, i))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    out.push('\n');
    out
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0usize;
    loop {
        let rel = lower[from..].find(name)?;
        let at = from + rel;
        // Must be preceded by whitespace and followed by '=' (after
        // optional spaces) — otherwise it's a substring of another
        // attribute (`data-src` when looking for `src`).
        let prev_ok = at == 0 || lower.as_bytes()[at - 1].is_ascii_whitespace();
        let after = at + name.len();
        let eq = lower[after..].find(|c: char| !c.is_ascii_whitespace());
        if prev_ok {
            if let Some(off) = eq {
                if lower[after + off..].starts_with('=') {
                    let vstart = after + off + 1;
                    let rest = tag[vstart..].trim_start();
                    let offset = tag.len() - rest.len();
                    let quote = rest.chars().next()?;
                    return if quote == '"' || quote == '\'' {
                        let end = rest[1..].find(quote)?;
                        Some(decode_entities(&tag[offset + 1..offset + 1 + end]))
                    } else {
                        let end = rest
                            .find(|c: char| c.is_ascii_whitespace())
                            .unwrap_or(rest.len());
                        Some(decode_entities(&tag[offset..offset + end]))
                    };
                }
            }
        }
        from = at + name.len();
        if from >= lower.len() {
            return None;
        }
    }
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

fn tidy_inline(s: &str) -> String {
    collapse_ws(s).trim().to_string()
}

/// Collapse runs of blank lines, drop trailing spaces, and cap
/// consecutive blank lines at one.
fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blanks = 0usize;
    for line in s.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_start_matches('\n').to_string()
}

/// Decode the entity set that actually shows up in prose. Numeric
/// (`&#8212;` / `&#x2014;`) plus the named entities a page is likely to
/// carry; anything else is left verbatim rather than mangled.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < s.len() {
        if bytes[i] != b'&' {
            let mut j = i + 1;
            while j < s.len() && !s.is_char_boundary(j) {
                j += 1;
            }
            out.push_str(&s[i..j]);
            i = j;
            continue;
        }
        let Some(semi) = s[i..].find(';').filter(|n| *n <= 10) else {
            out.push('&');
            i += 1;
            continue;
        };
        let ent = &s[i + 1..i + semi];
        let decoded = if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
        } else if let Some(dec) = ent.strip_prefix('#') {
            dec.parse::<u32>().ok().and_then(char::from_u32)
        } else {
            named_entity(ent)
        };
        match decoded {
            Some(c) => {
                out.push(c);
                i += semi + 1;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

fn named_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201C}',
        "rdquo" => '\u{201D}',
        "bull" => '•',
        "middot" => '·',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "deg" => '°',
        "plusmn" => '±',
        "times" => '×',
        "divide" => '÷',
        "laquo" => '«',
        "raquo" => '»',
        "eacute" => 'é',
        "egrave" => 'è',
        "agrave" => 'à',
        "ccedil" => 'ç',
        "uuml" => 'ü',
        "ouml" => 'ö',
        "auml" => 'ä',
        "szlig" => 'ß',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_script_and_style() {
        let (_, md) = convert(
            "<html><body><script>var x = '<p>fake</p>';</script>\
             <style>p{color:red}</style><p>Real content.</p></body></html>",
        );
        assert!(!md.contains("var x"), "script leaked: {md}");
        assert!(!md.contains("color:red"), "style leaked: {md}");
        assert!(md.contains("Real content."), "content lost: {md}");
    }

    #[test]
    fn headings_and_paragraphs() {
        let (_, md) = convert("<body><h1>Title</h1><p>One.</p><h2>Sub</h2><p>Two.</p></body>");
        assert!(md.contains("# Title"), "{md}");
        assert!(md.contains("## Sub"), "{md}");
        assert!(md.contains("One."), "{md}");
        assert!(md.contains("Two."), "{md}");
    }

    #[test]
    fn lists_render_as_markdown() {
        let (_, md) = convert("<body><ul><li>alpha</li><li>beta</li></ul></body>");
        assert!(md.contains("- alpha"), "{md}");
        assert!(md.contains("- beta"), "{md}");
    }

    #[test]
    fn ordered_lists_number() {
        let (_, md) = convert("<body><ol><li>first</li><li>second</li></ol></body>");
        assert!(md.contains("1. first"), "{md}");
        assert!(md.contains("2. second"), "{md}");
    }

    #[test]
    fn links_become_markdown() {
        let (_, md) =
            convert(r#"<body><p>See <a href="https://x.test/a">the docs</a>.</p></body>"#);
        assert!(md.contains("[the docs](https://x.test/a)"), "{md}");
    }

    #[test]
    fn anchor_only_links_keep_text_without_href() {
        let (_, md) = convert(r##"<body><p>Jump to <a href="#top">top</a>.</p></body>"##);
        assert!(md.contains("top"), "{md}");
        assert!(!md.contains("](#top)"), "in-page anchor kept: {md}");
    }

    #[test]
    fn entities_decode() {
        let (_, md) =
            convert("<body><p>A &amp; B &mdash; C &#8212; D &#x2014; E&nbsp;F</p></body>");
        assert!(md.contains("A & B"), "{md}");
        assert_eq!(md.matches('—').count(), 3, "{md}");
    }

    #[test]
    fn title_from_title_tag_then_h1() {
        let (t, _) =
            convert("<html><head><title>Doc Title</title></head><body><h1>H</h1></body></html>");
        assert_eq!(t, "Doc Title");
        let (t2, _) = convert("<html><body><h1>Just H1</h1></body></html>");
        assert_eq!(t2, "Just H1");
    }

    #[test]
    fn nav_and_footer_dropped() {
        let (_, md) = convert(
            "<body><nav><a href='/x'>Home</a><a href='/y'>About</a></nav>\
             <p>Body text.</p><footer>© 2026 Someone</footer></body>",
        );
        assert!(md.contains("Body text."), "{md}");
        assert!(!md.contains("About"), "nav leaked: {md}");
        assert!(!md.contains("2026 Someone"), "footer leaked: {md}");
    }

    #[test]
    fn main_scope_wins_over_chrome() {
        let filler = "x".repeat(600);
        let html = format!(
            "<body><div id=sidebar><p>SIDEBAR JUNK</p></div>\
             <main><p>{filler}</p></main></body>"
        );
        let (_, md) = convert(&html);
        assert!(md.contains(&filler), "main content lost");
        assert!(!md.contains("SIDEBAR JUNK"), "sidebar leaked: {md}");
    }

    #[test]
    fn pre_block_becomes_fence() {
        let (_, md) = convert("<body><pre>let x = 1;\nlet y = 2;</pre></body>");
        assert!(md.contains("```"), "{md}");
        assert!(md.contains("let x = 1;\nlet y = 2;"), "{md}");
    }

    #[test]
    fn tables_render_gfm() {
        let (_, md) = convert(
            "<body><table><tr><th>A</th><th>B</th></tr>\
             <tr><td>1</td><td>2</td></tr></table></body>",
        );
        assert!(md.contains("| A | B |"), "{md}");
        assert!(md.contains("| 1 | 2 |"), "{md}");
        assert!(md.contains("--- |"), "{md}");
    }

    #[test]
    fn looks_like_html_discriminates() {
        assert!(looks_like_html(
            "<!DOCTYPE html><html><body>hi</body></html>"
        ));
        assert!(looks_like_html("<html lang=\"en\">"));
        assert!(!looks_like_html("# A markdown doc\n\nWith prose."));
        assert!(!looks_like_html("{\"key\": \"value\"}"));
    }

    #[test]
    fn thai_text_survives() {
        let (_, md) = convert("<body><p>ระบบจัดการความรู้ของ thClaws</p></body>");
        assert!(md.contains("ระบบจัดการความรู้ของ thClaws"), "{md}");
    }

    #[test]
    fn no_blank_line_runs() {
        let (_, md) = convert("<body><p>a</p><div></div><div></div><p>b</p></body>");
        assert!(!md.contains("\n\n\n"), "blank runs survived: {md:?}");
    }

    #[test]
    fn bare_less_than_is_not_a_tag() {
        let (_, md) = convert("<body><p>if a &lt; b then</p></body>");
        assert!(md.contains("if a < b then"), "{md}");
    }

    #[test]
    fn attr_does_not_match_substring() {
        let tag = r#"img data-src="/lazy.png" src="/real.png""#;
        assert_eq!(attr(tag, "src").as_deref(), Some("/real.png"));
    }
}
