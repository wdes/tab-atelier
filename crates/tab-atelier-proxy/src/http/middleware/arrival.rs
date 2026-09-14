// SPDX-License-Identifier: MPL-2.0

//! Who is calling, and from where.
//!
//! Everything here reads the request as it arrived. None of it looks at the
//! account store, which is what keeps the audit trail independent of the
//! routing decision it records.

use std::net::{IpAddr, Ipv4Addr};

use hyper::HeaderMap;

/// The key presented by a caller, or an empty string.
///
/// Both headers are accepted because the two clients that exist disagree about
/// which is right: the Anthropic wire spells it `x-api-key`, the `OpenAI` wire
/// spells it `Authorization: Bearer`. An empty string rather than `None`
/// because every caller's next move is to look it up in the store, and a
/// missing key and a wrong key are the same event from here.
#[must_use]
pub(crate) fn presented(req: &crate::transport::InReq) -> String {
    let header = |name: &str| {
        req.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    header("x-api-key")
        .or_else(|| {
            req.headers
                .get(hyper::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_default()
}

/// The first value of a header, trimmed, non-empty.
#[must_use]
pub(crate) fn header_of(headers: &HeaderMap, name: &str) -> Option<String> {
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
pub(crate) fn client_ip(headers: &HeaderMap, peer: IpAddr) -> String {
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
pub(crate) const fn is_trusted_hop(peer: IpAddr) -> bool {
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
#[must_use]
pub(crate) const fn unknown_peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forwarded_header_is_ignored_from_an_untrusted_peer() {
        // Otherwise the audit trail records whatever the caller felt like
        // typing, and "last used from" becomes a lie a client writes itself.
        let mut h = HeaderMap::new();
        h.insert("x-real-ip", "203.0.113.9".parse().unwrap());
        let peer = "198.51.100.7".parse().unwrap();
        assert_eq!(client_ip(&h, peer), "198.51.100.7");
    }

    #[test]
    fn a_forwarded_header_is_believed_from_a_private_peer() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.9, 10.0.0.1".parse().unwrap());
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
}
