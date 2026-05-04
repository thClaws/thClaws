//! Open external URLs in the OS default browser, with a strict
//! http(s) safelist. Lifted from `gui.rs` to an always-on module in
//! M6.36 SERVE9h so the WS transport's `open_external` IPC arm can
//! validate + dispatch through `crate::ipc::handle_ipc`.
//!
//! ## Trust model
//!
//! Tool output (MCP, web search results) can produce URLs that flow
//! into the chat surface and end up here. We accept ONLY `http://` /
//! `https://` schemes with a real host — `file://`, `javascript:`,
//! custom schemes, malformed input all rejected. Without this gate, a
//! hostile MCP server could craft a chat link that launches arbitrary
//! local handlers (for example macOS's `x-apple-*://` schemes can
//! drive system apps without further prompts).

/// True when `s` parses as a real http(s) URL with a non-empty host.
pub fn is_safe_external_url(s: &str) -> bool {
    match url::Url::parse(s) {
        Ok(u) => matches!(u.scheme(), "http" | "https") && u.host_str().is_some(),
        Err(_) => false,
    }
}

/// Open the URL in the OS default browser. Caller is responsible for
/// validating via [`is_safe_external_url`] first; this function does
/// no validation. Best-effort — spawn failures are silent (the open
/// command might not exist on a headless server, which is fine since
/// `--serve` mode users probably don't want surprise external opens
/// from a remote URL anyway).
pub fn open_external_url(url: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_http_and_https() {
        assert!(is_safe_external_url("http://example.com"));
        assert!(is_safe_external_url("https://example.com/path"));
        assert!(is_safe_external_url("https://sub.example.com:8443/?q=1"));
    }

    #[test]
    fn rejects_dangerous_schemes() {
        assert!(!is_safe_external_url("file:///etc/passwd"));
        assert!(!is_safe_external_url("javascript:alert(1)"));
        assert!(!is_safe_external_url("ftp://example.com"));
        assert!(!is_safe_external_url("data:text/html,<script>"));
    }

    #[test]
    fn rejects_malformed_or_hostless() {
        assert!(!is_safe_external_url(""));
        assert!(!is_safe_external_url("not a url"));
        assert!(!is_safe_external_url("http://"));
    }
}
