//! SSRF guard for the runner-side fetch tools (`WebFetch`, `WebScrape`,
//! `FetchImages`).
//!
//! A page the agent extracts is untrusted content, and in multiuser mode tool
//! calls are force-auto-approved — so an image/link like
//! `http://169.254.169.254/latest/meta-data/…` or `http://192.168.1.1/…` could
//! otherwise pivot the runner into its own network / cloud metadata. This
//! module refuses any URL whose host is (or resolves to) a private, loopback,
//! link-local, ULA, or otherwise non-public address, and rejects non-http(s)
//! schemes.
//!
//! Opt out with `THCLAWS_ALLOW_PRIVATE_FETCH=1` (e.g. local dev fetching your
//! own `localhost` server).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

/// Escape hatch for users who legitimately fetch localhost / LAN.
pub fn allow_private() -> bool {
    std::env::var("THCLAWS_ALLOW_PRIVATE_FETCH").ok().as_deref() == Some("1")
}

/// `Ok(())` if the URL is safe to fetch from the runner; `Err(reason)` otherwise.
/// Resolves hostnames and checks **every** resolved address, so a name pointing
/// at a private IP is blocked too. (DNS-rebinding between this check and the
/// actual connect is out of scope — this stops the common cases: literal
/// private IPs, `localhost`, and names resolving to internal ranges.)
pub async fn guard(url_str: &str) -> Result<(), String> {
    if allow_private() {
        return Ok(());
    }
    let url = url::Url::parse(url_str).map_err(|e| format!("bad url: {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        s => return Err(format!("scheme '{s}' is not fetchable")),
    }
    let host = url
        .host_str()
        .ok_or_else(|| "url has no host".to_string())?;

    // Literal IP in the URL — check directly, no DNS.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return check_ip(ip);
    }
    // Obvious loopback names before paying for DNS.
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return Err(blocked_msg("localhost"));
    }

    let host_owned = host.to_string();
    let port = url.port_or_known_default().unwrap_or(80);
    // to_socket_addrs blocks on DNS — keep it off the async worker.
    let addrs = tokio::task::spawn_blocking(move || {
        (host_owned.as_str(), port)
            .to_socket_addrs()
            .map(|it| it.map(|sa| sa.ip()).collect::<Vec<_>>())
    })
    .await
    .map_err(|e| format!("resolve join error: {e}"))?
    .map_err(|e| format!("resolve {host}: {e}"))?;

    if addrs.is_empty() {
        return Err(format!("{host} did not resolve"));
    }
    for ip in addrs {
        check_ip(ip)?;
    }
    Ok(())
}

/// A client builder whose every hop is checked, not just the URL the tool
/// was given: each redirect target, and the address actually dialled (so a
/// name that answers public to [`guard`] and private to the connect, or a
/// public page that 302s to `169.254.169.254`, is refused too).
pub fn guarded_client_builder() -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder();
    if allow_private() {
        return builder;
    }
    builder
        .dns_resolver(Arc::new(PublicOnlyResolver))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many redirects");
            }
            match check_literal_target(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(e),
            }
        }))
}

/// The DNS-free half of [`guard`] for a redirect hop: scheme, literal IPs and
/// loopback names. Names are left to [`PublicOnlyResolver`] at connect time.
fn check_literal_target(url: &reqwest::Url) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        s => return Err(format!("scheme '{s}' is not fetchable")),
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) => check_ip(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => check_ip(IpAddr::V6(ip)),
        Some(url::Host::Domain(d)) => {
            let lower = d.to_ascii_lowercase();
            if lower == "localhost" || lower.ends_with(".localhost") {
                Err(blocked_msg("localhost"))
            } else {
                Ok(())
            }
        }
        None => Err("url has no host".to_string()),
    }
}

struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let lookup = host.clone();
            let addrs: Vec<SocketAddr> = tokio::task::spawn_blocking(move || {
                (lookup.as_str(), 0)
                    .to_socket_addrs()
                    .map(|it| it.collect())
            })
            .await??;
            let public: Vec<SocketAddr> = addrs
                .into_iter()
                .filter(|a| check_ip(a.ip()).is_ok())
                .collect();
            if public.is_empty() {
                return Err(blocked_msg(&host).into());
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn blocked_msg(what: &str) -> String {
    format!(
        "refuses to fetch {what} — non-public target \
         (set THCLAWS_ALLOW_PRIVATE_FETCH=1 to allow local/LAN fetches)"
    )
}

/// True → blocked. Covers loopback, private, link-local (incl. 169.254 cloud
/// metadata), CGNAT, ULA, unspecified/broadcast/documentation, and
/// IPv4-mapped IPv6.
fn check_ip(ip: IpAddr) -> Result<(), String> {
    let blocked = match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                is_blocked_v4(mapped)
            } else {
                is_blocked_v6(v6)
            }
        }
    };
    if blocked {
        Err(blocked_msg(&ip.to_string()))
    } else {
        Ok(())
    }
}

fn is_blocked_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local() // 169.254.0.0/16 — AWS/GCP/Azure metadata
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_documentation()
        || (o[0] == 100 && (64..=127).contains(&o[1])) // 100.64.0.0/10 CGNAT
        || o[0] == 0 // 0.0.0.0/8
}

fn is_blocked_v6(v6: Ipv6Addr) -> bool {
    let seg = v6.segments();
    v6.is_loopback()
        || v6.is_unspecified()
        || (seg[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
        || (seg[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn guarded_client_refuses_a_redirect_into_metadata() {
        // Re-audit N12: only the first URL used to be checked. The local
        // server stands in for a public page (a literal IP never reaches the
        // resolver); its 302 must not be followed.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\n\r\n")
                .await;
        });
        let client = guarded_client_builder().build().unwrap();
        let err = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect_err("redirect into metadata must fail");
        assert!(err.is_redirect(), "{err:?}");
    }

    #[tokio::test]
    async fn public_only_resolver_drops_private_answers() {
        use reqwest::dns::Resolve;
        use std::str::FromStr;
        let name = reqwest::dns::Name::from_str("localhost").unwrap();
        assert!(PublicOnlyResolver.resolve(name).await.is_err());
    }

    #[test]
    fn redirect_hops_check_literals_and_schemes() {
        let bad = |u: &str| check_literal_target(&reqwest::Url::parse(u).unwrap()).is_err();
        assert!(bad("http://10.43.0.1/"));
        assert!(bad("http://[::1]/"));
        assert!(bad("http://foo.localhost/"));
        assert!(bad("file:///etc/passwd"));
        assert!(!bad("https://example.com/a"));
    }

    fn blocked(s: &str) -> bool {
        check_ip(s.parse().unwrap()).is_err()
    }

    #[test]
    fn blocks_internal_addresses() {
        for ip in [
            "127.0.0.1",
            "169.254.169.254", // cloud metadata
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "100.64.0.1", // CGNAT
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1", // IPv4-mapped loopback
            "::ffff:169.254.169.254",
        ] {
            assert!(blocked(ip), "should block {ip}");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:2800:220::1"] {
            assert!(!blocked(ip), "should allow {ip}");
        }
    }

    #[tokio::test]
    async fn guard_rejects_scheme_and_literal_private() {
        assert!(guard("ftp://example.com/x").await.is_err());
        assert!(guard("http://127.0.0.1/x").await.is_err());
        assert!(guard("http://169.254.169.254/meta").await.is_err());
        assert!(guard("http://localhost:3000/x").await.is_err());
    }
}
