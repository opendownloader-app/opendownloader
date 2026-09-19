//! Everything the relay refuses, and why.
//!
//! The checks here all run *before* a socket is opened to the upstream. That ordering
//! is the point: a relay that discovered it should not have made a request only after
//! making it would still have made it, and for the SSRF guard in particular the request
//! is the whole attack.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::config::Config;

/// A refused request: the status it answers with, the sentence the caller is given,
/// and the short tag that appears in the log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The `url` parameter is missing, unparseable, or not http(s).
    BadUrl(&'static str),
    /// The host has no address. Reported as a client error because the URL the client
    /// supplied names something that does not exist.
    Unresolvable,
    /// The compliance policy says no.
    Restricted,
    /// The host resolves into address space the relay must not reach.
    PrivateHost,
    /// The host is not on the operator's allow-list.
    HostNotAllowed,
    /// The upstream answered with a web document (HTML) from a host that is not a known
    /// extraction site, so the relay refuses to serve it. This is what stops the relay
    /// being used as a general web proxy while still letting it fetch media from anywhere.
    DocumentRefused,
    /// The response is, or becomes, larger than `max_bytes`.
    TooLarge(u64),
    /// `max_concurrent` upstream requests are already in flight.
    Busy,
    /// The upstream could not be reached or failed mid-handshake.
    Upstream(String),
}

impl Refusal {
    pub fn status(&self) -> StatusCode {
        match self {
            Refusal::BadUrl(_) | Refusal::Unresolvable => StatusCode::BAD_REQUEST,
            Refusal::Restricted
            | Refusal::PrivateHost
            | Refusal::HostNotAllowed
            | Refusal::DocumentRefused => StatusCode::FORBIDDEN,
            Refusal::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Refusal::Busy => StatusCode::SERVICE_UNAVAILABLE,
            Refusal::Upstream(_) => StatusCode::BAD_GATEWAY,
        }
    }

    /// The tag written to the log. Deliberately a fixed vocabulary, so `grep` over a
    /// relay's output counts refusals by kind without parsing prose.
    pub fn tag(&self) -> &'static str {
        match self {
            Refusal::BadUrl(_) => "bad-url",
            Refusal::Unresolvable => "unresolvable",
            Refusal::Restricted => "restricted",
            Refusal::PrivateHost => "private-host",
            Refusal::HostNotAllowed => "host-not-allowed",
            Refusal::DocumentRefused => "document-refused",
            Refusal::TooLarge(_) => "too-large",
            Refusal::Busy => "busy",
            Refusal::Upstream(_) => "upstream",
        }
    }

    pub fn body(&self) -> String {
        match self {
            Refusal::BadUrl(why) => {
                format!("bad request: {why}. Pass ?url= a percent-encoded absolute http(s) URL.\n")
            }
            Refusal::Unresolvable => "bad request: the URL's host does not resolve.\n".to_string(),
            Refusal::Restricted => concat!(
                "refused: this host is on opendownloader's compliance list.\n",
                "The relay enforces the same policy as the extension — DRM- and\n",
                "ToS-restricted hosts are not proxied. There is no setting that\n",
                "turns this off.\n"
            )
            .to_string(),
            Refusal::PrivateHost => concat!(
                "refused: that host resolves to a private, loopback, link-local or\n",
                "unspecified address. The relay will not be used to reach its own\n",
                "network. Set allow_private_hosts = true only if that is what you want.\n"
            )
            .to_string(),
            Refusal::HostNotAllowed => {
                "refused: that host is not in this relay's allow_hosts list.\n".to_string()
            }
            Refusal::DocumentRefused => concat!(
                "refused: the relay does not serve web pages from this host. It relays\n",
                "media and the APIs of the sites it extracts, not arbitrary documents —\n",
                "so it cannot be used as a general web proxy.\n"
            )
            .to_string(),
            Refusal::TooLarge(max) => {
                format!("too large: this relay is capped at {max} bytes per response.\n")
            }
            Refusal::Busy => concat!(
                "busy: this relay is already at its concurrent-request limit.\n",
                "Retry shortly, or raise max_concurrent.\n"
            )
            .to_string(),
            Refusal::Upstream(e) => format!("upstream error: {e}\n"),
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (
            self.status(),
            [("content-type", "text/plain; charset=utf-8")],
            self.body(),
        )
            .into_response()
    }
}

/// A URL that has passed every check that does not require DNS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub url: String,
    pub host: String,
    pub port: u16,
}

/// Parse and vet the `url` parameter, then run the policy and allow-list checks.
///
/// Split out from the handler and kept free of I/O so the refusal table can be
/// asserted in unit tests without a network, in the same spirit as `dl-core`.
pub fn check(cfg: &Config, raw: &str) -> Result<Target, Refusal> {
    let target = parse_target(raw)?;

    // The relay enforces the compliance policy server-side, and this is the reason the
    // crate is allowed to exist at all: a relay that skipped this check would be a way
    // to launder the refusal the rest of the product is built on. Someone who cannot
    // download a DRM host through the extension must not be able to download it by
    // standing a relay in front of the same URL.
    if dl_core::policy::is_restricted(&target.url, &target.url) {
        return Err(Refusal::Restricted);
    }

    if !cfg.allow_hosts.is_empty()
        && !cfg
            .allow_hosts
            .iter()
            .any(|allowed| host_matches(&target.host, &allowed.to_ascii_lowercase()))
    {
        return Err(Refusal::HostNotAllowed);
    }

    Ok(target)
}

/// Split an absolute http(s) URL into the pieces the guard needs.
///
/// Hand-rolled for the same reason `dl_core::policy::host_of` is: the `url` crate drags
/// in `idna` and its Unicode tables for a job that is a handful of `split`s. That
/// function is `pub(crate)` to `dl-core` and `dl-core` is off-limits to this crate, so
/// the logic is mirrored here rather than the two being merged.
pub fn parse_target(raw: &str) -> Result<Target, Refusal> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Refusal::BadUrl("no url given"));
    }

    let (scheme, rest) = raw
        .split_once("://")
        .ok_or(Refusal::BadUrl("not an absolute URL"))?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" => 80,
        "https" => 443,
        // file:, data:, gopher: and friends are how an SSRF turns into a file read.
        _ => return Err(Refusal::BadUrl("only http and https are relayed")),
    };

    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // Userinfo is dropped rather than forwarded: the relay sends no credentials
    // upstream, and `user@host` is also a classic way to disguise the real host.
    let authority = authority.rsplit('@').next().unwrap_or(authority);

    let (host, port_text) = match authority.strip_prefix('[') {
        // IPv6 literal: the brackets delimit the host, and any port follows them.
        Some(after) => {
            let (inside, tail) = after
                .split_once(']')
                .ok_or(Refusal::BadUrl("unterminated IPv6 literal"))?;
            (inside, tail.strip_prefix(':'))
        }
        None => match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        },
    };

    if host.is_empty() {
        return Err(Refusal::BadUrl("no host in URL"));
    }
    let port = match port_text {
        None | Some("") => default_port,
        Some(p) => p.parse().map_err(|_| Refusal::BadUrl("bad port"))?,
    };

    Ok(Target {
        url: raw.to_string(),
        host: host.to_ascii_lowercase(),
        port,
    })
}

/// Resolve the host and refuse if *any* address it answers with is one the relay
/// must not reach.
///
/// Every address is checked, not just the first: a name that resolves to one public
/// and one loopback address is the standard way to smuggle a request past a guard that
/// only looks at `addrs[0]`.
///
/// This is a resolve-then-connect check and therefore racy in principle — the client
/// resolves the name again — but rebinding across the two lookups requires control of
/// the zone's TTL and still cannot reach anything the connect itself would not.
/// Operators who need a hard guarantee should give the relay an egress firewall.
pub async fn host_is_reachable(host: &str, port: u16) -> Result<(), Refusal> {
    // `localhost` is refused by name as well as by address, because a resolver can be
    // told to point it anywhere and the name is still the operator's own machine.
    if host == "localhost" || host.ends_with(".localhost") {
        return Err(Refusal::PrivateHost);
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_forbidden(ip) {
            Err(Refusal::PrivateHost)
        } else {
            Ok(())
        };
    }

    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| Refusal::Unresolvable)?;
    let mut any = false;
    for addr in addrs {
        any = true;
        if is_forbidden(addr.ip()) {
            return Err(Refusal::PrivateHost);
        }
    }
    if any {
        Ok(())
    } else {
        Err(Refusal::Unresolvable)
    }
}

/// True for any address the relay must not be pointed at.
///
/// The list is "everything that is not somewhere on the public internet" rather than a
/// blocklist of known-sensitive endpoints: link-local `169.254.169.254` is the famous
/// cloud-metadata address, but enumerating it and its siblings would leave every other
/// internal service reachable.
pub fn is_forbidden(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_v4(v4),
        // `::ffff:127.0.0.1` is loopback wearing an IPv6 hat; unwrap before judging it.
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_forbidden_v4(v4),
            None => is_forbidden_v6(v6),
        },
    }
}

fn is_forbidden_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_unspecified()
        || ip.is_broadcast()
        // 100.64.0.0/10, carrier-grade NAT — not routable on the public internet and,
        // on a hosted box, frequently the provider's own management network.
        || (ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]))
}

fn is_forbidden_v6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // fc00::/7 unique-local.
        || (ip.segments()[0] & 0xfe00) == 0xfc00
        // fe80::/10 link-local.
        || (ip.segments()[0] & 0xffc0) == 0xfe80
}

/// Resolve a `Location` against the URL that produced it.
///
/// A redirect may be absolute, root-relative or path-relative, and all three are common.
/// The result is fed straight back through [`check`] and [`host_is_reachable`], so this
/// only has to produce something absolute — it does not have to decide whether it is
/// safe, and deliberately does not try.
pub fn resolve_redirect(base: &str, location: &str) -> Result<String, Refusal> {
    let location = location.trim();
    if location.is_empty() {
        return Err(Refusal::BadUrl("redirect with an empty Location"));
    }
    // Absolute already: it has a scheme. Whether that scheme is one we relay is
    // `parse_target`'s decision, not this function's.
    if let Some(i) = location.find("://") {
        if location[..i]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
        {
            return Ok(location.to_string());
        }
    }

    let (scheme, rest) = base
        .split_once("://")
        .ok_or(Refusal::BadUrl("redirect from a URL with no scheme"))?;
    // Scheme-relative: //host/path
    if let Some(tail) = location.strip_prefix("//") {
        return Ok(format!("{scheme}://{tail}"));
    }

    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let origin = format!("{scheme}://{authority}");

    if let Some(tail) = location.strip_prefix('/') {
        return Ok(format!("{origin}/{tail}"));
    }

    // Path-relative, honouring ./ and ../
    let dir = match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    };
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for segment in location.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    Ok(format!("{origin}/{}", parts.join("/")))
}

/// Boundary-aware suffix match, mirroring `dl_core::policy`'s: an allow-list entry
/// covers the host and its subdomains, and never a lookalike that merely ends the
/// same way.
fn host_matches(host: &str, allowed: &str) -> bool {
    host == allowed
        || (host.len() > allowed.len()
            && host.ends_with(allowed)
            && host.as_bytes()[host.len() - allowed.len() - 1] == b'.')
}

/// Whether `host` (or a subdomain of it) is on `list`. Case-insensitive on the list side;
/// the host is expected to already be lowercase.
pub fn host_on_list(list: &[String], host: &str) -> bool {
    list.iter()
        .any(|entry| host_matches(host, &entry.to_ascii_lowercase()))
}

/// Whether a `Content-Type` names a web document — HTML or XHTML. This, not a domain list,
/// is what separates "fetch a media file or a site's API" from "browse an arbitrary
/// website through our IP": the abuse an open proxy enables is reading pages, and pages are
/// these types. A `charset` or other parameter after the type is ignored.
pub fn is_document_type(content_type: Option<&str>) -> bool {
    match content_type {
        Some(value) => {
            let essence = value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            essence == "text/html" || essence == "application/xhtml+xml"
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_html_and_xhtml_count_as_documents() {
        // The abuse an open proxy enables is reading web pages; these are the page types.
        assert!(is_document_type(Some("text/html")));
        assert!(is_document_type(Some("text/html; charset=utf-8")));
        assert!(is_document_type(Some("application/xhtml+xml")));
        assert!(is_document_type(Some("TEXT/HTML")));
        // Media, manifests, JSON and direct-download bytes must all pass.
        assert!(!is_document_type(Some("video/mp4")));
        assert!(!is_document_type(Some("audio/mpeg")));
        assert!(!is_document_type(Some("application/vnd.apple.mpegurl")));
        assert!(!is_document_type(Some("application/dash+xml")));
        assert!(!is_document_type(Some("application/json")));
        assert!(!is_document_type(Some("application/octet-stream")));
        assert!(!is_document_type(None));
    }

    #[test]
    fn a_document_host_list_matches_subdomains() {
        let list = vec!["bilibili.com".to_string(), "vimeo.com".to_string()];
        assert!(host_on_list(&list, "www.bilibili.com"));
        assert!(host_on_list(&list, "bilibili.com"));
        assert!(host_on_list(&list, "player.vimeo.com"));
        assert!(!host_on_list(&list, "example.com"));
        // The classic suffix-spoof must not match.
        assert!(!host_on_list(&list, "notbilibili.com"));
        assert!(!host_on_list(&list, "bilibili.com.evil.test"));
    }

    #[test]
    fn document_refused_is_a_forbidden() {
        assert_eq!(Refusal::DocumentRefused.status(), StatusCode::FORBIDDEN);
        assert_eq!(Refusal::DocumentRefused.tag(), "document-refused");
    }

    fn cfg() -> Config {
        Config::default()
    }

    fn allowing(hosts: &[&str]) -> Config {
        Config {
            allow_hosts: hosts.iter().map(|h| h.to_string()).collect(),
            ..Config::default()
        }
    }

    #[test]
    fn a_redirect_is_resolved_whether_it_is_absolute_or_relative() {
        let base = "https://cdn.example.com/a/b/file.mp4?x=1";
        assert_eq!(
            resolve_redirect(base, "https://other.example/c.mp4").unwrap(),
            "https://other.example/c.mp4"
        );
        assert_eq!(
            resolve_redirect(base, "//other.example/c.mp4").unwrap(),
            "https://other.example/c.mp4"
        );
        assert_eq!(
            resolve_redirect(base, "/root.mp4").unwrap(),
            "https://cdn.example.com/root.mp4"
        );
        assert_eq!(
            resolve_redirect(base, "next.mp4").unwrap(),
            "https://cdn.example.com/a/b/next.mp4"
        );
        assert_eq!(
            resolve_redirect(base, "../up.mp4").unwrap(),
            "https://cdn.example.com/a/up.mp4"
        );
        assert!(resolve_redirect(base, "  ").is_err());
    }

    #[test]
    fn a_redirect_to_a_refused_destination_is_still_refused() {
        // The whole reason redirects are followed by hand: the destination goes back
        // through the same checks as the original URL, so neither the compliance policy
        // nor the address-space guard can be laundered through a 302.
        let base = "https://cdn.example.com/a.mp4";
        let restricted = resolve_redirect(base, "https://www.netflix.com/v.mp4").unwrap();
        assert_eq!(check(&cfg(), &restricted), Err(Refusal::Restricted));

        let scheme = resolve_redirect(base, "file:///etc/passwd").unwrap();
        assert!(matches!(check(&cfg(), &scheme), Err(Refusal::BadUrl(_))));
    }

    #[test]
    fn an_ordinary_url_is_split_into_host_and_default_port() {
        let t = parse_target("https://cdn.Example.com/a/b.mp4?x=1").expect("valid URL");
        assert_eq!(t.host, "cdn.example.com");
        assert_eq!(t.port, 443);
        let t = parse_target("http://cdn.example.com/a.mp4").expect("valid URL");
        assert_eq!(t.port, 80);
    }

    #[test]
    fn an_explicit_port_userinfo_and_ipv6_literals_are_understood() {
        assert_eq!(parse_target("https://h:8443/a").expect("valid").port, 8443);
        let t = parse_target("https://user:pw@cdn.example.com/a").expect("valid");
        assert_eq!(t.host, "cdn.example.com");
        let t = parse_target("http://[2606:4700::1111]:8080/a").expect("valid");
        assert_eq!(t.host, "2606:4700::1111");
        assert_eq!(t.port, 8080);
    }

    #[test]
    fn a_non_http_scheme_is_a_bad_request() {
        for raw in [
            "file:///etc/passwd",
            "ftp://example.com/a",
            "gopher://example.com/a",
        ] {
            let err = check(&cfg(), raw).expect_err("must be refused");
            assert_eq!(err.status(), StatusCode::BAD_REQUEST, "{raw}");
        }
    }

    #[test]
    fn a_missing_or_relative_url_is_a_bad_request() {
        for raw in ["", "   ", "/a/b.mp4", "cdn.example.com/a.mp4", "https://"] {
            let err = check(&cfg(), raw).expect_err("must be refused");
            assert_eq!(err.status(), StatusCode::BAD_REQUEST, "{raw:?}");
        }
    }

    #[test]
    fn a_restricted_host_is_refused_with_403() {
        let err = check(&cfg(), "https://www.netflix.com/x.mp4").expect_err("must be refused");
        assert_eq!(err, Refusal::Restricted);
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert!(err.body().contains("compliance"));

        for raw in [
            "https://x.nflxvideo.net/range/0-100",
            "https://nflxvideo.net/a.mp4",
            "https://open.spotify.com/a.mp3",
        ] {
            assert_eq!(
                check(&cfg(), raw).expect_err("must be refused").status(),
                StatusCode::FORBIDDEN,
                "{raw}"
            );
        }
    }

    #[test]
    fn an_ordinary_host_passes_the_policy_check() {
        let t = check(&cfg(), "https://cdn.example.com/a.mp4").expect("allowed");
        assert_eq!(t.host, "cdn.example.com");
    }

    #[test]
    fn an_empty_allow_hosts_permits_any_host_the_policy_permits() {
        assert!(check(&cfg(), "https://anything.example.org/a.mp4").is_ok());
    }

    #[test]
    fn a_non_empty_allow_hosts_admits_the_host_and_its_subdomains_only() {
        let cfg = allowing(&["example.com"]);
        assert!(check(&cfg, "https://example.com/a.mp4").is_ok());
        assert!(check(&cfg, "https://cdn.example.com/a.mp4").is_ok());
        assert_eq!(
            check(&cfg, "https://other.org/a.mp4")
                .expect_err("must be refused")
                .status(),
            StatusCode::FORBIDDEN
        );
        // The lookalike case a naive `ends_with` gets wrong.
        assert_eq!(
            check(&cfg, "https://notexample.com/a.mp4")
                .expect_err("must be refused")
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            check(&cfg, "https://example.com.evil.io/a.mp4")
                .expect_err("must be refused")
                .status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn the_allow_list_does_not_override_the_compliance_policy() {
        let cfg = allowing(&["netflix.com"]);
        assert_eq!(
            check(&cfg, "https://www.netflix.com/x.mp4").expect_err("must be refused"),
            Refusal::Restricted
        );
    }

    #[test]
    fn private_loopback_and_link_local_v4_addresses_are_forbidden() {
        for raw in [
            "127.0.0.1",
            "127.13.13.13",
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "100.100.0.1",
        ] {
            let ip: IpAddr = raw.parse().expect("test address parses");
            assert!(is_forbidden(ip), "{raw} should be forbidden");
        }
    }

    #[test]
    fn private_loopback_and_link_local_v6_addresses_are_forbidden() {
        for raw in [
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            let ip: IpAddr = raw.parse().expect("test address parses");
            assert!(is_forbidden(ip), "{raw} should be forbidden");
        }
    }

    #[test]
    fn public_addresses_are_allowed() {
        for raw in [
            "1.1.1.1",
            "8.8.8.8",
            "172.32.0.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
        ] {
            let ip: IpAddr = raw.parse().expect("test address parses");
            assert!(!is_forbidden(ip), "{raw} should be allowed");
        }
    }

    #[tokio::test]
    async fn a_bare_private_ip_literal_is_refused_without_any_lookup() {
        for host in ["127.0.0.1", "192.168.0.10", "::1", "169.254.169.254"] {
            assert_eq!(
                host_is_reachable(host, 80).await,
                Err(Refusal::PrivateHost),
                "{host}"
            );
        }
    }

    #[tokio::test]
    async fn localhost_is_refused_by_name() {
        assert_eq!(
            host_is_reachable("localhost", 80).await,
            Err(Refusal::PrivateHost)
        );
        assert_eq!(
            host_is_reachable("app.localhost", 80).await,
            Err(Refusal::PrivateHost)
        );
    }

    #[tokio::test]
    async fn a_public_ip_literal_needs_no_network_to_be_accepted() {
        assert_eq!(host_is_reachable("1.1.1.1", 443).await, Ok(()));
        assert_eq!(host_is_reachable("2606:4700:4700::1111", 443).await, Ok(()));
    }

    #[test]
    fn each_refusal_maps_to_its_documented_status() {
        assert_eq!(Refusal::BadUrl("x").status(), StatusCode::BAD_REQUEST);
        assert_eq!(Refusal::Unresolvable.status(), StatusCode::BAD_REQUEST);
        assert_eq!(Refusal::Restricted.status(), StatusCode::FORBIDDEN);
        assert_eq!(Refusal::PrivateHost.status(), StatusCode::FORBIDDEN);
        assert_eq!(Refusal::HostNotAllowed.status(), StatusCode::FORBIDDEN);
        assert_eq!(Refusal::TooLarge(1).status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(Refusal::Busy.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            Refusal::Upstream("x".into()).status(),
            StatusCode::BAD_GATEWAY
        );
    }
}
