// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-tab allowlist pre-resolver — the domain half of allowlist mode.
//!
//! It does **not** sit in the tab's DNS path (that would need a mount
//! namespace to point the tab's `resolv.conf` at us, which the hardened
//! service unit forbids). Instead it runs beside the path: a daemon-side
//! thread that, on a timer, resolves each concrete allowlisted domain via the
//! upstream resolver and programs the answer IPs into the tab's nftables
//! dynamic set with `timeout = TTL` and the domain as a comment
//! ([`crate::net_nft::add_allow_ip`]). The tab resolves the same names through
//! the host resolver (allowed by a scoped `:53` hole) and connects to the IPs,
//! which the confine chain gates against `@allow_dyn`.
//!
//! Trade-off vs a true interceptor: enforcement is at the IP layer, so a
//! per-query CDN (short TTL, rotating IPs) can hand the tab an IP we didn't
//! pre-load — that connection drops even though the domain is allowed. A short
//! refresh interval + a TTL grace window narrow the gap. Wildcard (`*.`)
//! entries can't be enumerated and so can't be pre-resolved. And because we no
//! longer see the tab's queries, denied names aren't observed per-domain (only
//! the confine `drop` counter remains).
//!
//! No external DNS library — the wire format is small and unit-tested.
//! Dependency-free, blocking `std::net`.

#![cfg(all(target_os = "linux", not(feature = "gui")))]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::net_policy::AllowSet;

/// A pre-resolved allowlist domain, kept for the DNS-entries view.
#[derive(Clone, Debug)]
pub struct DnsEntry {
    pub domain: String,
    /// Always `true` in the pre-resolve model — the daemon only looks up names
    /// that are on the allowlist. Kept for API compatibility with the view.
    pub allowed: bool,
    /// The IPs the domain last resolved to (and were programmed into nft).
    pub ips: Vec<IpAddr>,
}

/// Handle to a running per-tab pre-resolver. Holds the resolved-domain log;
/// dropping it stops the refresh thread.
pub struct ResolverHandle {
    shutdown: Arc<AtomicBool>,
    log: Arc<Mutex<Vec<DnsEntry>>>,
}

impl ResolverHandle {
    /// Snapshot of the last resolved IPs per allowlist domain.
    #[must_use]
    pub fn entries(&self) -> Vec<DnsEntry> {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    /// Stop the refresh thread (also runs on drop).
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl Drop for ResolverHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Cap on the kept log so it can't grow without bound.
const LOG_CAP: usize = 256;
/// Refresh cadence bounds (seconds): re-resolve at least this often and at
/// most this rarely, regardless of the domains' TTLs.
const REFRESH_MIN_S: u32 = 20;
const REFRESH_MAX_S: u32 = 120;
/// Grace added to an element's nft timeout beyond the refresh interval, so an
/// IP survives until the next refresh reprograms it (avoids a blackout gap).
const TTL_GRACE_S: u32 = 60;

/// The upstream resolver the daemon forwards its own lookups to.
///
/// The first `nameserver` in `/etc/resolv.conf`, falling back to Cloudflare
/// (`1.1.1.1:53`). Matches what the tab's resolver uses, so the daemon and the
/// tab tend to see the same answer.
#[must_use]
pub fn upstream_resolver() -> SocketAddr {
    nameservers()
        .into_iter()
        .next()
        .map_or_else(|| SocketAddr::from(([1, 1, 1, 1], 53)), |ip| SocketAddr::new(ip, 53))
}

/// Every `nameserver` in `/etc/resolv.conf`.
///
/// These are the DNS servers the tab will use, so they scope the confine
/// chain's `:53` hole to just those hosts. Falls back to `1.1.1.1` when the
/// file is missing or lists none.
#[must_use]
pub fn nameservers() -> Vec<IpAddr> {
    let servers: Vec<IpAddr> = std::fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("nameserver").map(str::trim))
        // Strip a `%zone` scope id (link-local IPv6) before parsing.
        .filter_map(|ns| ns.split('%').next().unwrap_or(ns).parse::<IpAddr>().ok())
        .collect();
    if servers.is_empty() {
        vec![IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))]
    } else {
        servers
    }
}

/// Start the pre-resolve loop for `tab_id`, refreshing the tab's nft allow-set
/// from `allow`'s concrete domains, forwarding lookups to `upstream`.
///
/// # Errors
/// Returns the `io::Error` if the worker thread can't be spawned.
pub fn spawn(tab_id: String, allow: AllowSet, upstream: SocketAddr) -> std::io::Result<ResolverHandle> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let log = Arc::new(Mutex::new(Vec::new()));
    let (sd, lg) = (shutdown.clone(), log.clone());
    std::thread::Builder::new()
        .name("net-resolver".to_string())
        .spawn(move || refresh_loop(&tab_id, &allow, upstream, &sd, &lg))?;
    Ok(ResolverHandle { shutdown, log })
}

fn refresh_loop(
    tab_id: &str,
    allow: &AllowSet,
    upstream: SocketAddr,
    shutdown: &Arc<AtomicBool>,
    log: &Arc<Mutex<Vec<DnsEntry>>>,
) {
    let domains = allow.resolvable_domains();
    while !shutdown.load(Ordering::Relaxed) {
        let mut min_ttl = REFRESH_MAX_S;
        let mut resolved: Vec<(String, Vec<(IpAddr, u32)>)> = Vec::with_capacity(domains.len());
        for domain in &domains {
            let ips = resolve(domain, upstream);
            for &(_, ttl) in &ips {
                min_ttl = min_ttl.min(ttl.max(1));
            }
            resolved.push((domain.clone(), ips));
        }
        let sleep_s = min_ttl.clamp(REFRESH_MIN_S, REFRESH_MAX_S);
        let elem_timeout = sleep_s + TTL_GRACE_S;
        for (domain, ips) in &resolved {
            for &(ip, ttl) in ips {
                // Live at least until the next refresh reprograms it.
                crate::net_nft::add_allow_ip(tab_id, ip, ttl.max(elem_timeout), domain);
            }
            record(log, domain, ips.iter().map(|(ip, _)| *ip).collect());
        }
        sleep_with_shutdown(sleep_s, shutdown);
    }
}

/// Sleep `secs` seconds in 1-second steps, waking early if shutdown is set.
fn sleep_with_shutdown(secs: u32, shutdown: &Arc<AtomicBool>) {
    for _ in 0..secs {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Resolve `domain` to its A and AAAA IPs (with TTLs) via `upstream`.
fn resolve(domain: &str, upstream: SocketAddr) -> Vec<(IpAddr, u32)> {
    let mut ips = Vec::new();
    for qtype in [1u16 /* A */, 28u16 /* AAAA */] {
        if let Some(q) = build_query(domain, qtype)
            && let Some(resp) = forward(&q, upstream)
        {
            ips.extend(extract_answer_ips(&resp));
        }
    }
    ips
}

/// Build a minimal DNS query packet (RD=1, one question) for `name`/`qtype`.
/// `None` if a label is empty or over 63 bytes.
#[must_use]
pub fn build_query(name: &str, qtype: u16) -> Option<Vec<u8>> {
    // id=1, flags RD=1, qd=1, an/ns/ar=0.
    let mut q = vec![0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0); // root label
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&[0x00, 0x01]); // class IN
    Some(q)
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

fn record(log: &Arc<Mutex<Vec<DnsEntry>>>, domain: &str, ips: Vec<IpAddr>) {
    if let Ok(mut l) = log.lock() {
        if let Some(e) = l.iter_mut().find(|e| e.domain == domain) {
            if !ips.is_empty() {
                e.ips = ips;
            }
        } else {
            if l.len() >= LOG_CAP {
                l.remove(0);
            }
            l.push(DnsEntry {
                domain: domain.to_string(),
                allowed: true,
                ips,
            });
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_query_encodes_name_and_type() {
        let q = build_query("api.anthropic.com", 1).unwrap();
        // header: id=1, RD, qd=1
        assert_eq!(&q[0..6], &[0x00, 0x01, 0x01, 0x00, 0x00, 0x01]);
        // labels: 3"api" …
        assert_eq!(&q[12..16], &[0x03, b'a', b'p', b'i']);
        // trailing: root(0) + qtype A(1) + class IN(1)
        assert_eq!(&q[q.len() - 5..], &[0x00, 0x00, 0x01, 0x00, 0x01]);
        // AAAA type (28 = 0x1C) is encoded in the trailing qtype.
        let q6 = build_query("a", 28).unwrap();
        assert_eq!(&q6[q6.len() - 4..], &[0x00, 0x1C, 0x00, 0x01]);
    }

    #[test]
    fn build_query_rejects_bad_labels() {
        assert!(build_query("a..b", 1).is_none()); // empty label
        assert!(build_query(&"x".repeat(64), 1).is_none()); // label too long
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
    fn record_dedups_by_domain_and_refreshes_ips() {
        let log = Arc::new(Mutex::new(Vec::new()));
        record(&log, "api.x.com", vec![IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))]);
        record(&log, "api.x.com", vec![IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8))]);
        let e = log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        assert_eq!(e.len(), 1);
        assert!(e[0].allowed);
        assert_eq!(e[0].ips, vec![IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8))]);
    }
}
