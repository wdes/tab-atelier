// SPDX-License-Identifier: MPL-2.0

//! Who is calling, and from where.
//!
//! Everything here reads the request as it arrived. None of it looks at the
//! account store, which is what keeps the audit trail independent of the
//! routing decision it records.
//!
//! The header map is `hyper`'s rather than Rocket's because these functions are
//! also used by the relay, which forwards to upstream over `hyper` and so works
//! in that type all the way through. [`from_rocket`] is the one conversion.

use std::net::IpAddr;

use http::HeaderMap;

/// The key presented by a caller, or an empty string.
///
/// Both headers are accepted because the two clients that exist disagree about
/// which is right: the Anthropic wire spells it `x-api-key`, the `OpenAI` wire
/// spells it `Authorization: Bearer`. An empty string rather than `None`
/// because every caller's next move is to look it up in the store, and a
/// missing key and a wrong key are the same event from here.
#[must_use]
pub fn presented(headers: &HeaderMap) -> String {
    let bearer = || {
        headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                let trimmed = v.trim();
                trimmed
                    .strip_prefix("Bearer ")
                    .or_else(|| trimmed.strip_prefix("bearer "))
            })
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    header_of(headers, "x-api-key").or_else(bearer).unwrap_or_default()
}

/// The first value of a header, trimmed, non-empty.
#[must_use]
pub fn header_of(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// The caller's address, as far as it can be trusted.
///
/// A forwarded header is believed only from a private peer, because anything
/// else can be set by whoever is connecting. This is the rule that makes the
/// audit trail worth reading: without it, a client could write someone else's
/// address into their own row.
#[must_use]
pub fn client_ip(headers: &HeaderMap, peer: IpAddr) -> String {
    if !is_trusted_hop(peer) {
        return peer.to_string();
    }
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    header("x-real-ip")
        .or_else(|| header("x-forwarded-for"))
        .unwrap_or_else(|| peer.to_string())
}

/// Whether a peer is close enough that a forwarding header on its request is
/// evidence rather than a claim.
///
/// Loopback, private and link-local all count: the proxy sits behind a TLS
/// terminator that is one of the three.
#[must_use]
pub const fn is_trusted_hop(peer: IpAddr) -> bool {
    match peer {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return mapped.is_loopback() || mapped.is_private() || mapped.is_link_local();
            }
            let seg = v6.segments()[0];
            // fc00::/7 unique-local, fe80::/10 link-local.
            v6.is_loopback() || (seg & 0xfe00) == 0xfc00 || (seg & 0xffc0) == 0xfe80
        }
    }
}

/// A loopback address, for when a transport cannot report a peer.
///
/// A connection over a unix socket, or a request driven through Rocket's local
/// client, has no peer. Loopback is the honest answer for both: the caller is
/// on this machine, so it is trusted for forwarding headers, which is the only
/// decision that reads this.
#[must_use]
pub const fn unknown_peer() -> IpAddr {
    IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
}

/// The headers of a Rocket request, in the type the relay forwards with.
///
/// This exists because the two halves of the proxy look at headers through
/// different crates: Rocket's HTTP types for what it routed, and `hyper`'s for
/// what it forwards. Converting in one place keeps the trusted-hop rule and the
/// header list from being reimplemented on either side.
#[must_use]
pub fn from_rocket(headers: &rocket::http::HeaderMap<'_>) -> HeaderMap {
    let mut out = HeaderMap::new();
    for header in headers.iter() {
        let Ok(name) = http::header::HeaderName::from_bytes(header.name.as_str().as_bytes()) else {
            continue;
        };
        let Ok(value) = http::header::HeaderValue::from_str(&header.value) else {
            continue;
        };
        out.append(name, value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn a_forwarded_header_is_ignored_from_an_untrusted_peer() {
        // Otherwise the audit trail records whatever the caller felt like
        // typing, and "last used from" becomes a lie a client writes itself.
        let h = map(&[("x-real-ip", "203.0.113.9")]);
        let peer = "198.51.100.7".parse().unwrap();
        assert_eq!(client_ip(&h, peer), "198.51.100.7");
    }

    #[test]
    fn a_forwarded_header_is_believed_from_a_private_peer() {
        let h = map(&[("x-forwarded-for", "203.0.113.9, 10.0.0.1")]);
        let peer = "10.0.0.1".parse().unwrap();
        // The first entry is the client; the rest are the hops it came through.
        assert_eq!(client_ip(&h, peer), "203.0.113.9");
    }

    #[test]
    fn the_forwarded_header_falls_back_to_the_peer_when_absent() {
        let h = HeaderMap::new();
        let peer = "10.0.0.5".parse().unwrap();
        assert_eq!(client_ip(&h, peer), "10.0.0.5");
    }

    #[test]
    fn a_mapped_ipv6_loopback_is_still_loopback() {
        // A v4 connection seen on a dual-stack socket arrives as ::ffff:127.0.0.1,
        // and treating that as untrusted would drop the header from the local
        // TLS terminator.
        let peer: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert!(is_trusted_hop(peer));
    }

    #[test]
    fn a_public_ipv6_is_not_a_trusted_hop() {
        let peer: IpAddr = "2001:db8::1".parse().unwrap();
        assert!(!is_trusted_hop(peer));
    }

    #[test]
    fn the_anthropic_header_is_read_when_there_is_no_authorization() {
        let h = map(&[("x-api-key", "sk-ant-1")]);
        assert_eq!(presented(&h), "sk-ant-1");
    }

    #[test]
    fn the_bearer_prefix_is_stripped_whatever_its_case() {
        // The SDKs disagree about capitalisation, and a token compared with the
        // word "Bearer" still attached matches nothing.
        let h = map(&[("authorization", "bearer sk-2")]);
        assert_eq!(presented(&h), "sk-2");
    }

    #[test]
    fn a_caller_presenting_nothing_presents_an_empty_key() {
        assert_eq!(presented(&HeaderMap::new()), "");
    }

    #[test]
    fn x_api_key_wins_over_authorization() {
        // The Anthropic header is the more specific of the two, so a client
        // sending both means the one it went out of its way to set.
        let h = map(&[("x-api-key", "from-x"), ("authorization", "Bearer from-auth")]);
        assert_eq!(presented(&h), "from-x");
    }

    #[test]
    fn a_header_of_nothing_but_spaces_is_not_a_value() {
        let h = map(&[("x-empty", "   ")]);
        assert_eq!(header_of(&h, "x-empty"), None);
    }

    #[test]
    fn rocket_headers_arrive_in_hyper_unchanged() {
        let mut rocket = rocket::http::HeaderMap::new();
        rocket.add(rocket::http::Header::new("X-Api-Key", "sk-3"));
        let converted = from_rocket(&rocket);
        assert_eq!(presented(&converted), "sk-3");
    }
}
