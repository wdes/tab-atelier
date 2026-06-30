// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-tab gating DNS resolver — the domain half of allowlist mode.
//!
//! A tiny blocking UDP forwarder bound on a loopback port. The tab's `:53`
//! is redirected to it by nftables ([`crate::net_nft::redirect_ruleset`]),
//! so every name the tab resolves passes through here. For each query:
//!
//! - **allowed** (the QNAME matches the tab's [`AllowSet`]): forward to the
//!   upstream resolver, return the answer, and add each resolved A/AAAA IP
//!   to the tab's nftables dynamic set with `timeout = TTL` and the domain
//!   as a comment ([`crate::net_nft::add_allow_ip`]) — so the tab can then
//!   reach exactly those IPs, and only while the TTL is live;
//! - **denied**: return `REFUSED` without forwarding, so a disallowed name
//!   never even resolves.
//!
//! It runs **in the daemon** (host loopback, no namespace), so its query log
//! (allowed + denied, for the DNS-entries view) is read directly — no
//! bind-mount. UDP only for now (the common path); TCP DNS (large answers)
//! would need a second listener. No external DNS library — the wire parsing
//! is small and unit-tested. Dependency-free, blocking `std::net`.

#![cfg(all(target_os = "linux", not(feature = "gui")))]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::net_policy::AllowSet;

/// A single observed DNS query, kept for the DNS-entries view.
#[derive(Clone, Debug)]
pub struct DnsEntry {
    pub domain: String,
    pub allowed: bool,
    /// Resolved IPs (allowed queries only).
    pub ips: Vec<IpAddr>,
}

/// Handle to a running per-tab resolver. Holds the loopback port (to feed the
/// nft `:53` redirect) and the query log. Dropping it stops the resolver.
pub struct ResolverHandle {
    port: u16,
    shutdown: Arc<AtomicBool>,
    log: Arc<Mutex<Vec<DnsEntry>>>,
}

impl ResolverHandle {
    /// The loopback port the resolver listens on — feed to
    /// [`crate::net_nft::redirect_ruleset`].
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Snapshot of the recent query log (allowed + denied).
    #[must_use]
    pub fn entries(&self) -> Vec<DnsEntry> {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    /// Stop the resolver (also runs on drop).
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl Drop for ResolverHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Cap on the kept query log so it can't grow without bound.
const LOG_CAP: usize = 256;

/// The upstream resolver to forward allowed queries to.
///
/// The first `nameserver` in `/etc/resolv.conf`, falling back to Cloudflare
/// (`1.1.1.1:53`). A loopback nameserver (systemd-resolved at 127.0.0.53) is
/// fine — the daemon itself isn't egress-restricted.
#[must_use]
pub fn upstream_resolver() -> SocketAddr {
    let fallback = SocketAddr::from(([1, 1, 1, 1], 53));
    let Ok(conf) = std::fs::read_to_string("/etc/resolv.conf") else {
        return fallback;
    };
    conf.lines()
        .filter_map(|l| l.strip_prefix("nameserver").map(str::trim))
        .find_map(|ns| ns.parse::<IpAddr>().ok())
        .map_or(fallback, |ip| SocketAddr::new(ip, 53))
}

/// Start a gating resolver for `tab_id` on a random loopback UDP port,
/// forwarding allowed queries to `upstream`.
///
/// # Errors
/// Returns the `io::Error` if the loopback socket can't be bound or the
/// worker thread can't be spawned.
pub fn spawn(tab_id: String, allow: AllowSet, upstream: SocketAddr) -> std::io::Result<ResolverHandle> {
    let sock = UdpSocket::bind(("127.0.0.1", 0))?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    let port = sock.local_addr()?.port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let log = Arc::new(Mutex::new(Vec::new()));
    let (sd, lg) = (shutdown.clone(), log.clone());
    std::thread::Builder::new()
        .name("net-resolver".to_string())
        .spawn(move || serve(&sock, &tab_id, &allow, upstream, &sd, &lg))?;
    Ok(ResolverHandle { port, shutdown, log })
}

fn serve(
    sock: &UdpSocket,
    tab_id: &str,
    allow: &AllowSet,
    upstream: SocketAddr,
    shutdown: &Arc<AtomicBool>,
    log: &Arc<Mutex<Vec<DnsEntry>>>,
) {
    let mut buf = [0u8; 1500];
    while !shutdown.load(Ordering::Relaxed) {
        let (n, client) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                continue;
            }
            Err(_) => break,
        };
        let query = &buf[..n];
        let Some(name) = extract_qname(query) else { continue };
        if allow.host_allowed(&name) {
            // Forward to upstream and relay the answer; program nft with the
            // resolved IPs so the tab can reach them.
            if let Some(resp) = forward(query, upstream) {
                let ips: Vec<(IpAddr, u32)> = extract_answer_ips(&resp);
                for &(ip, ttl) in &ips {
                    crate::net_nft::add_allow_ip(tab_id, ip, ttl.max(30), &name);
                }
                record(log, &name, true, ips.iter().map(|(ip, _)| *ip).collect());
                let _ = sock.send_to(&resp, client);
            }
        } else {
            record(log, &name, false, Vec::new());
            let _ = sock.send_to(&refused_response(query), client);
        }
    }
}

/// Forward a raw query to `upstream` and return the raw response (best-effort).
fn forward(query: &[u8], upstream: SocketAddr) -> Option<Vec<u8>> {
    let up = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    up.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    up.send_to(query, upstream).ok()?;
    let mut buf = [0u8; 1500];
    let n = up.recv(&mut buf).ok()?;
    Some(buf[..n].to_vec())
}

fn record(log: &Arc<Mutex<Vec<DnsEntry>>>, domain: &str, allowed: bool, ips: Vec<IpAddr>) {
    if let Ok(mut l) = log.lock() {
        // De-dup by (domain, allowed): refresh the existing entry's IPs.
        if let Some(e) = l.iter_mut().find(|e| e.domain == domain && e.allowed == allowed) {
            if !ips.is_empty() {
                e.ips = ips;
            }
        } else {
            if l.len() >= LOG_CAP {
                l.remove(0);
            }
            l.push(DnsEntry {
                domain: domain.to_string(),
                allowed,
                ips,
            });
        }
    }
}

/// Extract the queried name (QNAME) from a DNS query packet. Queries don't
/// use name compression, so this is a straight label walk. `None` on a
/// malformed/short packet.
#[must_use]
pub fn extract_qname(q: &[u8]) -> Option<String> {
    if q.len() < 12 {
        return None;
    }
    let mut i = 12;
    let mut labels: Vec<String> = Vec::new();
    loop {
        let len = *q.get(i)? as usize;
        if len == 0 {
            break;
        }
        if len & 0xC0 != 0 {
            return None; // compression pointer — not valid in a query QNAME
        }
        i += 1;
        let label = q.get(i..i + len)?;
        labels.push(std::str::from_utf8(label).ok()?.to_ascii_lowercase());
        i += len;
        if labels.len() > 127 {
            return None;
        }
    }
    if labels.is_empty() {
        return None;
    }
    Some(labels.join("."))
}

/// Advance past a DNS name at offset `i`, returning the index just after it.
/// Handles a compression pointer (2 bytes) and the label sequence.
fn skip_name(r: &[u8], mut i: usize) -> usize {
    loop {
        match r.get(i) {
            None => return i,
            Some(0) => return i + 1,
            Some(&len) if len & 0xC0 == 0xC0 => return i + 2, // pointer
            Some(&len) => i += 1 + len as usize,
        }
    }
}

/// Extract `(ip, ttl)` for every A / AAAA answer in a DNS response.
#[must_use]
pub fn extract_answer_ips(r: &[u8]) -> Vec<(IpAddr, u32)> {
    let mut out = Vec::new();
    if r.len() < 12 {
        return out;
    }
    let qd = u16::from_be_bytes([r[4], r[5]]) as usize;
    let an = u16::from_be_bytes([r[6], r[7]]) as usize;
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(r, i) + 4; // qname + qtype + qclass
    }
    for _ in 0..an {
        i = skip_name(r, i);
        if i + 10 > r.len() {
            break;
        }
        let typ = u16::from_be_bytes([r[i], r[i + 1]]);
        let ttl = u32::from_be_bytes([r[i + 4], r[i + 5], r[i + 6], r[i + 7]]);
        let rdlen = u16::from_be_bytes([r[i + 8], r[i + 9]]) as usize;
        i += 10;
        if i + rdlen > r.len() {
            break;
        }
        if typ == 1 && rdlen == 4 {
            out.push((IpAddr::V4(Ipv4Addr::new(r[i], r[i + 1], r[i + 2], r[i + 3])), ttl));
        } else if typ == 28 && rdlen == 16 {
            let mut b = [0u8; 16];
            b.copy_from_slice(&r[i..i + 16]);
            out.push((IpAddr::V6(Ipv6Addr::from(b)), ttl));
        }
        i += rdlen;
    }
    out
}

/// Craft a `REFUSED` reply to `query`: same header/question, QR=1, RCODE=5,
/// no answers.
#[must_use]
pub fn refused_response(query: &[u8]) -> Vec<u8> {
    let mut r = query.to_vec();
    if r.len() >= 12 {
        r[2] = (r[2] & 0x01) | 0x80; // QR=1, preserve RD, opcode 0
        r[3] = 0x05; // RCODE=REFUSED, RA=0
        // Zero AN/NS/AR counts (keep QDCOUNT).
        r[6..12].fill(0);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    // A DNS query for "api.anthropic.com" A: header (id, flags, qd=1) + qname.
    fn query_for(name: &str) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0); // root
        q.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // A IN
        q
    }

    #[test]
    fn extract_qname_reads_the_name() {
        assert_eq!(
            extract_qname(&query_for("api.anthropic.com")).as_deref(),
            Some("api.anthropic.com")
        );
        assert_eq!(extract_qname(&query_for("EXAMPLE.com")).as_deref(), Some("example.com")); // lowercased
        assert_eq!(extract_qname(b"\x00\x01"), None); // too short
    }

    #[test]
    fn extract_answer_ips_reads_a_and_aaaa() {
        // header: id, flags(resp), qd=1, an=2
        let mut r = vec![0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x02, 0, 0, 0, 0];
        // question: "x" A IN
        r.extend_from_slice(&[0x01, b'x', 0x00, 0x00, 0x01, 0x00, 0x01]);
        // answer 1: name ptr 0xC00C, A, IN, ttl=300, rdlen=4, 1.1.1.1
        r.extend_from_slice(&[
            0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x01, 0x2C, 0x00, 0x04, 1, 1, 1, 1,
        ]);
        // answer 2: name ptr, AAAA(28), IN, ttl=60, rdlen=16, ::1
        r.extend_from_slice(&[0xC0, 0x0C, 0x00, 0x1C, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3C, 0x00, 0x10]);
        r.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let ips = extract_answer_ips(&r);
        assert_eq!(ips.len(), 2);
        assert_eq!(ips[0], (IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 300));
        assert_eq!(
            ips[1].0,
            IpAddr::V6(Ipv6Addr::from([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]))
        );
        assert_eq!(ips[1].1, 60);
    }

    #[test]
    fn refused_sets_qr_and_rcode() {
        let q = query_for("blocked.test");
        let r = refused_response(&q);
        assert_eq!(r[2] & 0x80, 0x80, "QR set");
        assert_eq!(r[3] & 0x0F, 0x05, "RCODE = REFUSED");
        assert_eq!(&r[6..12], &[0, 0, 0, 0, 0, 0], "no answer/auth/additional");
        // header id preserved
        assert_eq!(&r[0..2], &q[0..2]);
    }

    #[test]
    fn gate_allows_subdomain_refuses_other() {
        let allow = AllowSet::build(&[], &["example.com".to_string()], &[]);
        assert!(allow.host_allowed(&extract_qname(&query_for("a.example.com")).unwrap()));
        assert!(!allow.host_allowed(&extract_qname(&query_for("evil.test")).unwrap()));
    }
}
