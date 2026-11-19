//! Artwork-only HTTP client: every destination (direct literal, DNS answer or redirect hop) must be a public address.
//!
//! The policy lives in a wrapping resolver, so the addresses that were validated are exactly the ones the connector
//! dials; there is never a second lookup. Environment proxies are disabled so they cannot bypass the check.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{Connector, DefaultConnector, NextTimeout};

/// Redirect hops followed per artwork fetch.
pub const MAX_REDIRECTS: usize = 10;

/// Typed refusal carried inside `ureq::Error::Other`; never retried.
#[derive(Debug)]
pub struct PolicyError;

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("artwork destination is not a public address") }
}

impl std::error::Error for PolicyError {}

pub fn is_policy_error(e: &ureq::Error) -> bool {
    matches!(e, ureq::Error::Other(inner) if inner.is::<PolicyError>())
}

fn v4_in(ip: Ipv4Addr, base: [u8; 4], prefix: u32) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    u32::from(ip) & mask == u32::from(Ipv4Addr::from(base)) & mask
}

fn v4_allowed(ip: Ipv4Addr) -> bool {
    const BLOCKED: [([u8; 4], u32); 15] = [
        ([0, 0, 0, 0], 8), ([10, 0, 0, 0], 8), ([100, 64, 0, 0], 10), ([127, 0, 0, 0], 8), ([169, 254, 0, 0], 16),
        ([172, 16, 0, 0], 12), ([192, 0, 0, 0], 24), ([192, 0, 2, 0], 24), ([192, 88, 99, 0], 24), ([192, 168, 0, 0], 16),
        ([198, 18, 0, 0], 15), ([198, 51, 100, 0], 24), ([203, 0, 113, 0], 24), ([224, 0, 0, 0], 4), ([240, 0, 0, 0], 4),
    ];
    !BLOCKED.iter().any(|&(b, p)| v4_in(ip, b, p))
}

fn v6_allowed(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() { return v4_allowed(v4); }
    let s = ip.segments();
    if s[0] & 0xe000 != 0x2000 { return false; }                    // only 2000::/3
    if s[0] == 0x2001 && (s[1] <= 0x01ff || s[1] == 0x0db8) { return false; }   // 2001::/23, 2001:db8::/32
    if s[0] == 0x2002 { return false; }                              // 2002::/16
    if s[0] == 0x3fff && s[1] <= 0x0fff { return false; }            // 3fff::/20
    true
}

/// The conservative public-destination policy, kept as a table in `tests/fixtures/network-address-policy.json`.
pub fn address_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_allowed(v4),
        IpAddr::V6(v6) => v6_allowed(v6),
    }
}

/// Wraps a resolver and refuses the whole resolution when any returned address is disallowed.
#[derive(Debug)]
pub struct PublicOnly<R> { inner: R, allow: fn(IpAddr) -> bool }

impl<R> PublicOnly<R> {
    pub fn new(inner: R) -> Self { PublicOnly { inner, allow: address_allowed } }
    #[cfg(test)]
    fn with_policy(inner: R, allow: fn(IpAddr) -> bool) -> Self { PublicOnly { inner, allow } }
}

impl<R: Resolver> Resolver for PublicOnly<R> {
    fn resolve(&self, uri: &Uri, config: &Config, timeout: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let addrs = self.inner.resolve(uri, config, timeout)?;
        if addrs.iter().any(|a| !(self.allow)(a.ip())) { return Err(ureq::Error::Other(Box::new(PolicyError))); }
        Ok(addrs)
    }
}

fn build(timeout: Duration, connector: impl Connector, resolver: impl Resolver) -> ureq::Agent {
    let config = Config::builder()
        .timeout_global(Some(timeout))
        .proxy(None)
        .max_redirects(0)
        .build();
    ureq::Agent::with_parts(config, connector, resolver)
}

/// The production artwork agent: public destinations only, no proxy, no automatic redirects.
pub fn agent(timeout: Duration) -> ureq::Agent {
    build(timeout, DefaultConnector::default(), PublicOnly::new(DefaultResolver::default()))
}

/// Test-only agent that additionally lets loopback test servers through.
#[cfg(test)]
pub fn loopback_agent(timeout: Duration) -> ureq::Agent {
    build(timeout, DefaultConnector::default(), PublicOnly::with_policy(DefaultResolver::default(), |ip| ip.is_loopback() || address_allowed(ip)))
}

/// Why a fetch ended without a response body.
#[derive(Debug, PartialEq, Eq)]
pub enum GetError { Final, Retry }

/// The `url` crate treats `http:///x` as host `x` (WHATWG); an authority must be written out explicitly.
pub(crate) fn explicit_authority(raw: &str) -> bool {
    raw.split_once("://").is_some_and(|(_, rest)| !rest.starts_with(['/', '\\', '?', '#']))
}

fn check_url(url: &str) -> Result<url::Url, ()> {
    if !explicit_authority(url) { return Err(()); }
    let uri = url::Url::parse(url).map_err(|_| ())?;
    let scheme = uri.scheme();
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) { return Err(()); }
    if !uri.username().is_empty() || uri.password().is_some() || uri.host_str().is_none_or(|h| h.is_empty()) { return Err(()); }
    Ok(uri)
}

/// Resolves a redirect `Location` against `current`; refuses unsupported schemes, userinfo and https-to-http.
pub fn next_hop(current: &str, location: &str) -> Result<String, ()> {
    if location.bytes().any(|b| b.is_ascii_control()) { return Err(()); }
    let cur = check_url(current)?;
    let next = cur.join(location).map_err(|_| ())?;
    let nu = check_url(next.as_str())?;
    if cur.scheme().eq_ignore_ascii_case("https") && nu.scheme().eq_ignore_ascii_case("http") { return Err(()); }
    Ok(next.to_string())
}

/// GETs `url`, following at most `MAX_REDIRECTS` validated redirects within one shared `timeout`.
/// Each hop is checked before it is requested; the agent's resolver enforces the address policy per hop.
pub fn get(agent: &ureq::Agent, url: &str, timeout: Duration) -> Result<ureq::Body, GetError> {
    let deadline = Instant::now() + timeout;
    let mut current = url.to_string();
    check_url(&current).map_err(|_| GetError::Final)?;
    for _ in 0..=MAX_REDIRECTS {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() { return Err(GetError::Retry); }
        let resp = agent.get(&current).config().timeout_global(Some(remaining)).build().call().map_err(|e| {
            if is_policy_error(&e) { GetError::Final } else { GetError::Retry }
        })?;
        let status = resp.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let loc = resp.headers().get("location").and_then(|v| v.to_str().ok()).ok_or(GetError::Final)?;
            current = next_hop(&current, loc).map_err(|_| GetError::Final)?;
            continue;
        }
        return if (200..300).contains(&status) { Ok(resp.into_body()) } else { Err(GetError::Final) };
    }
    Err(GetError::Final)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::{Arc, Mutex};
    use ureq::unversioned::transport::{ConnectionDetails, Transport};

    #[derive(Debug)]
    struct Fixed(Mutex<std::collections::VecDeque<Vec<SocketAddr>>>);
    impl Fixed {
        fn new(rounds: Vec<Vec<&str>>) -> Self {
            Fixed(Mutex::new(rounds.into_iter().map(|r| r.into_iter().map(|a| a.parse().unwrap()).collect()).collect()))
        }
    }
    impl Resolver for Fixed {
        fn resolve(&self, _: &Uri, _: &Config, _: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
            let round = self.0.lock().unwrap().pop_front().unwrap_or_default();
            let mut v = self.empty();
            for a in round { v.push(a); }
            Ok(v)
        }
    }

    /// Records every address a connection was attempted to, then fails (or dials for real when `real`).
    #[derive(Debug, Clone)]
    struct Counting { log: Arc<Mutex<Vec<SocketAddr>>>, real: bool }
    impl Connector for Counting {
        type Out = Box<dyn Transport>;
        fn connect(&self, details: &ConnectionDetails, chained: Option<()>) -> Result<Option<Self::Out>, ureq::Error> {
            self.log.lock().unwrap().extend(details.addrs.iter().copied());
            if self.real { DefaultConnector::default().connect(details, chained) } else { Err(ureq::Error::ConnectionFailed) }
        }
    }

    fn uri() -> Uri { "http://art.example/c.png".parse().unwrap() }

    fn resolve(r: &impl Resolver) -> Result<ResolvedSocketAddrs, ureq::Error> {
        r.resolve(&uri(), &Config::default(), NextTimeout::default())
    }

    #[test]
    fn shared_address_table_matches_the_policy() {
        let table: serde_json::Value = serde_json::from_str(include_str!("../../tests/fixtures/network-address-policy.json")).unwrap();
        let rows = table.as_array().unwrap();
        assert!(rows.len() > 20);
        for row in rows {
            let a = row["address"].as_str().unwrap();
            let ip: IpAddr = a.parse().unwrap_or_else(|_| panic!("bad fixture address {a}"));
            assert_eq!(address_allowed(ip), row["allowed"].as_bool().unwrap(), "{a}");
        }
    }

    #[test]
    fn mixed_public_and_private_answers_are_refused_whole() {
        let r = PublicOnly::new(Fixed::new(vec![vec!["93.184.216.34:80", "10.0.0.7:80"], vec!["[2606:4700::1]:80", "[fe80::1]:80"]]));
        assert!(is_policy_error(&resolve(&r).unwrap_err()));
        assert!(is_policy_error(&resolve(&r).unwrap_err()));
    }

    #[test]
    fn validated_addresses_are_the_ones_returned_and_rebinding_is_caught_per_lookup() {
        let r = PublicOnly::new(Fixed::new(vec![vec!["93.184.216.34:80"], vec!["127.0.0.1:80"]]));
        let first = resolve(&r).unwrap();
        assert_eq!(&first[..], &["93.184.216.34:80".parse::<SocketAddr>().unwrap()][..]);
        assert!(is_policy_error(&resolve(&r).unwrap_err()));
    }

    #[test]
    fn forbidden_resolutions_never_reach_the_connector() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let a = build(Duration::from_secs(2), Counting { log: log.clone(), real: false }, PublicOnly::new(Fixed::new(vec![vec!["93.184.216.34:80", "192.168.0.9:80"]])));
        assert_eq!(get(&a, "http://art.example/c.png", Duration::from_secs(2)).err(), Some(GetError::Final));
        let a = build(Duration::from_secs(2), Counting { log: log.clone(), real: false }, PublicOnly::new(Fixed::new(vec![vec!["127.0.0.1:80"]])));
        assert_eq!(get(&a, "http://127.0.0.1/c.png", Duration::from_secs(2)).err(), Some(GetError::Final));
        assert!(log.lock().unwrap().is_empty(), "{:?}", log.lock().unwrap());
    }

    #[test]
    fn public_destination_reaches_the_connector_and_failure_is_retryable() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let a = build(Duration::from_secs(2), Counting { log: log.clone(), real: false }, PublicOnly::new(Fixed::new(vec![vec!["93.184.216.34:80"]])));
        assert_eq!(get(&a, "http://art.example/c.png", Duration::from_secs(2)).err(), Some(GetError::Retry));
        assert_eq!(*log.lock().unwrap(), vec!["93.184.216.34:80".parse::<SocketAddr>().unwrap()]);
    }

    fn server(responses: Vec<String>) -> (u16, std::thread::JoinHandle<()>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            for resp in responses {
                let Ok((mut s, _)) = l.accept() else { return };
                let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
                let mut head = Vec::new();
                let mut b = [0u8; 256];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut b) { Ok(n) if n > 0 => head.extend_from_slice(&b[..n]), _ => break }
                }
                let _ = s.write_all(resp.as_bytes());
            }
        });
        (port, h)
    }

    fn loopback_counting(log: &Arc<Mutex<Vec<SocketAddr>>>) -> ureq::Agent {
        build(Duration::from_secs(5), Counting { log: log.clone(), real: true }, PublicOnly::with_policy(DefaultResolver::default(), |ip| ip.is_loopback() || address_allowed(ip)))
    }

    #[test]
    fn redirect_to_a_private_destination_is_refused_before_connecting() {
        let (port, h) = server(vec!["HTTP/1.1 302 Found\r\nLocation: http://192.168.1.5:9/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()]);
        let log = Arc::new(Mutex::new(Vec::new()));
        let r = get(&loopback_counting(&log), &format!("http://127.0.0.1:{port}/a"), Duration::from_secs(5));
        assert_eq!(r.err(), Some(GetError::Final));
        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].ip().is_loopback());
        h.join().unwrap();
    }

    #[test]
    fn relative_redirect_is_followed_and_body_is_returned() {
        let (port, h) = server(vec![
            "HTTP/1.1 301 Moved\r\nLocation: /b\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into(),
        ]);
        let log = Arc::new(Mutex::new(Vec::new()));
        let body = get(&loopback_counting(&log), &format!("http://127.0.0.1:{port}/a"), Duration::from_secs(5)).ok().unwrap();
        assert_eq!(body.into_reader().bytes().count(), 2);
        h.join().unwrap();
    }

    #[test]
    fn redirect_loop_stops_after_the_hop_limit() {
        let resp = "HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
        let (port, h) = server(vec![resp; MAX_REDIRECTS + 1]);
        let log = Arc::new(Mutex::new(Vec::new()));
        let r = get(&loopback_counting(&log), &format!("http://127.0.0.1:{port}/a"), Duration::from_secs(5));
        assert_eq!(r.err(), Some(GetError::Final));
        assert_eq!(log.lock().unwrap().len(), MAX_REDIRECTS + 1);
        h.join().unwrap();
    }

    #[test]
    fn redirect_targets_are_vetted() {
        assert_eq!(next_hop("http://a.example/x", "/y").unwrap(), "http://a.example/y");
        assert_eq!(next_hop("http://a.example/x", "https://b.example/z").unwrap(), "https://b.example/z");
        assert_eq!(next_hop("http://a.example/x", "http://b.example/z").unwrap(), "http://b.example/z");
        assert!(next_hop("https://a.example/x", "http://b.example/z").is_err(), "downgrade");
        assert!(next_hop("https://a.example/x", "//b.example/z").is_ok(), "scheme-relative keeps https");
        assert!(next_hop("http://a.example/x", "ftp://b.example/z").is_err());
        assert!(next_hop("http://a.example/x", "file:///etc/passwd").is_err());
        assert!(next_hop("http://a.example/x", "http://user:pw@b.example/z").is_err());
        assert!(next_hop("http://a.example/x", "http://b.example/z\r\nX: y").is_err());
    }

    #[test]
    fn unsupported_or_credentialed_start_urls_are_final() {
        let a = loopback_agent(Duration::from_secs(1));
        for u in ["ftp://127.0.0.1/x", "file:///etc/passwd", "http://u:p@127.0.0.1/x", "http:///x", "not a url"] {
            assert_eq!(get(&a, u, Duration::from_secs(1)).err(), Some(GetError::Final), "{u}");
        }
    }
}
