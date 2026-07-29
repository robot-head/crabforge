//! Deciding whether a URL may be called at all.
//!
//! Webhook targets are supplied by users, and the forge fetches them from
//! inside its own network. That is a server-side request forgery primitive
//! handed out as a feature: without a guard, `http://169.254.169.254/` reaches
//! a cloud metadata service, and `http://127.0.0.1:9092` reaches the broker.
//!
//! Crabka's gateway solves this with an operator-configured allow-list of
//! hosts, which is right for an operator's own integrations and wrong here —
//! a forge cannot enumerate in advance every service its users may legitimately
//! call. So this is a deny-list of destinations, applied *after* resolution,
//! because a name under an attacker's control can resolve wherever they like.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TargetError {
    #[error("webhook URLs must be http or https")]
    BadScheme,
    #[error("that URL is not valid")]
    Malformed,
    #[error("that address is not reachable from the forge")]
    Blocked,
    #[error("that host does not resolve")]
    Unresolvable,
}

/// Check the parts of a URL that do not need name resolution.
///
/// Run when a webhook is saved, so an obviously bad URL is rejected while
/// someone is looking at the form rather than silently failing later.
pub fn check_url(url: &str) -> Result<(), TargetError> {
    let rest = if let Some(rest) = url.strip_prefix("https://") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://") {
        rest
    } else {
        return Err(TargetError::BadScheme);
    };

    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    if host.is_empty() {
        return Err(TargetError::Malformed);
    }

    // A literal address can be judged now. A name cannot: it is judged after
    // resolution, immediately before the request.
    if let Ok(ip) = host_of(host).parse::<IpAddr>()
        && !is_public(ip)
    {
        return Err(TargetError::Blocked);
    }
    Ok(())
}

/// The host part of an authority, without any port.
///
/// Splitting on `:` is wrong for IPv6: `[::1]:8080` would yield `[`, and the
/// address would then fail to parse and be waved through as if it were a
/// hostname — turning the whole guard off for every IPv6 literal.
fn host_of(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6, with or without a port.
        return rest.split(']').next().unwrap_or_default();
    }
    // More than one colon and no brackets means a bare IPv6 literal, which
    // cannot carry a port — `::1` must not be read as host `:` port `1`.
    if authority.matches(':').count() > 1 {
        return authority;
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => authority,
    }
}

/// The port in an authority, if it names one.
fn port_of(authority: &str) -> Option<&str> {
    let after_host = if authority.starts_with('[') {
        authority.split(']').nth(1)?
    } else if authority.matches(':').count() > 1 {
        // A bare IPv6 literal has no port.
        return None;
    } else {
        authority
    };
    after_host
        .rsplit_once(':')
        .map(|(_, port)| port)
        .filter(|port| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
}

/// Whether an address is one the forge will talk to.
///
/// Everything private, local, or otherwise special is refused. The list is
/// deliberately broad: a false refusal is an inconvenience, and a false
/// acceptance is a request made from inside the network on a stranger's behalf.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        // Carrier-grade NAT, 100.64.0.0/10.
        || (a == 100 && (64..128).contains(&b))
        // Benchmarking, 198.18.0.0/15.
        || (a == 198 && (18..20).contains(&b))
        // Reserved, 240.0.0.0/4 — including the cloud metadata neighbourhood's
        // less famous relatives.
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // Unique local, fc00::/7.
        || (segments[0] & 0xfe00) == 0xfc00
        // Link local, fe80::/10.
        || (segments[0] & 0xffc0) == 0xfe80
        // An IPv4 address in disguise is judged as what it is.
        || ip.to_ipv4_mapped().is_some_and(|v4| !is_public_v4(v4))
        || ip.to_ipv4().is_some_and(|v4| !is_public_v4(v4)))
}

/// Resolve a URL's host and refuse it if it lands anywhere private.
///
/// Checked here rather than only at save time because DNS can change: a name
/// that resolved publicly when the webhook was created can be repointed at
/// `127.0.0.1` afterwards.
pub async fn resolve_and_check(url: &str) -> Result<Vec<IpAddr>, TargetError> {
    check_url(url)?;

    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or(TargetError::BadScheme)?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();

    let host = host_of(host_port);
    let port = port_of(host_port).unwrap_or(if url.starts_with("https") {
        "443"
    } else {
        "80"
    });

    let addresses: Vec<IpAddr> = tokio::net::lookup_host((host, port.parse().unwrap_or(80)))
        .await
        .map_err(|_| TargetError::Unresolvable)?
        .map(|addr| addr.ip())
        .collect();

    if addresses.is_empty() {
        return Err(TargetError::Unresolvable);
    }
    // Every answer must be acceptable. A name that resolves to one public and
    // one private address would otherwise be usable by retrying.
    if addresses.iter().any(|ip| !is_public(*ip)) {
        return Err(TargetError::Blocked);
    }
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn ordinary_urls_are_accepted() {
        check!(check_url("https://example.com/hook").is_ok());
        check!(check_url("http://example.com:8080/hook?x=1").is_ok());
        check!(check_url("https://93.184.216.34/hook").is_ok());
    }

    #[test]
    fn other_schemes_are_refused() {
        // `file://` would read the forge's disk; `gopher://` was a classic
        // way to smuggle arbitrary bytes into an internal service.
        for url in [
            "file:///etc/passwd",
            "gopher://internal/x",
            "ftp://host/x",
            "//example.com",
            "example.com",
        ] {
            check!(
                check_url(url) == Err(TargetError::BadScheme),
                "accepted {url}"
            );
        }
    }

    #[test]
    fn loopback_and_private_literals_are_refused() {
        for url in [
            "http://127.0.0.1/hook",
            "http://localhost.localdomain@127.0.0.1/x",
            "http://10.0.0.5/hook",
            "http://192.168.1.1/hook",
            "http://172.16.0.1/hook",
            "http://0.0.0.0/hook",
            "http://[::1]/hook",
        ] {
            check!(
                check_url(url) == Err(TargetError::Blocked),
                "accepted {url}"
            );
        }
    }

    #[test]
    fn the_cloud_metadata_address_is_refused() {
        // The single most valuable target of a webhook SSRF: it hands out
        // credentials to anything that can reach it.
        check!(check_url("http://169.254.169.254/latest/meta-data/") == Err(TargetError::Blocked));
    }

    #[test]
    fn less_famous_reserved_ranges_are_refused_too() {
        for ip in [
            "100.64.0.1", // carrier-grade NAT
            "198.18.0.1", // benchmarking
            "240.0.0.1",  // reserved
            "224.0.0.1",  // multicast
            "255.255.255.255",
        ] {
            check!(
                check_url(&format!("http://{ip}/x")) == Err(TargetError::Blocked),
                "accepted {ip}"
            );
        }
    }

    #[test]
    fn a_host_is_separated_from_its_port_without_mangling_ipv6() {
        // Splitting on `:` here would turn `[::1]:8080` into `[`, which then
        // fails to parse as an address and gets waved through as a hostname —
        // disabling the guard for every IPv6 literal.
        check!(host_of("example.com") == "example.com");
        check!(host_of("example.com:8080") == "example.com");
        check!(host_of("[::1]") == "::1");
        check!(host_of("[::1]:8080") == "::1");
        check!(host_of("::1") == "::1");
        check!(host_of("127.0.0.1:80") == "127.0.0.1");

        check!(port_of("example.com:8080") == Some("8080"));
        check!(port_of("example.com").is_none());
        check!(port_of("[::1]:9092") == Some("9092"));
        check!(port_of("::1").is_none(), "a bare ipv6 literal has no port");
    }

    #[test]
    fn an_ipv4_address_disguised_as_ipv6_is_still_refused() {
        // `::ffff:127.0.0.1` is loopback wearing a hat.
        check!(check_url("http://[::ffff:127.0.0.1]/x") == Err(TargetError::Blocked));
        check!(!is_public("::ffff:10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn unique_local_and_link_local_ipv6_are_refused() {
        check!(!is_public("fc00::1".parse().unwrap()));
        check!(!is_public("fe80::1".parse().unwrap()));
        check!(is_public("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn credentials_in_the_url_do_not_hide_the_real_host() {
        // `http://example.com@127.0.0.1/` goes to 127.0.0.1, and a check that
        // reads up to the first `@` would miss it.
        check!(check_url("http://example.com@127.0.0.1/x") == Err(TargetError::Blocked));
    }

    #[tokio::test]
    async fn resolution_refuses_a_name_that_points_at_loopback() {
        // The attack the save-time check cannot catch: a name the attacker
        // controls, repointed after the webhook was created.
        check!(resolve_and_check("http://localhost/hook").await == Err(TargetError::Blocked));
    }

    #[tokio::test]
    async fn a_host_that_does_not_resolve_is_reported_as_such() {
        let result = resolve_and_check("https://no-such-host.invalid/hook").await;
        check!(result == Err(TargetError::Unresolvable));
    }
}
