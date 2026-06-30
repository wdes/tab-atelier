// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Privileged, kernel-enforced egress allowlist for allowlist-mode tabs.
//!
//! Installs per-tab nftables rules keyed on the tab's cgroup v2 path so that
//! traffic from that cgroup may only reach the allowlisted CIDRs (plus
//! loopback for the local API + DNS so names still resolve); everything else
//! is dropped — including from software that would ignore a proxy. The
//! daemon holds `CAP_NET_ADMIN` to program this; the tabs do not (their caps
//! are stripped on spawn), so an agent can't `nft flush` its way out.
//!
//! ## Requirements & degradation
//!
//! - Linux headless service with `CAP_NET_ADMIN`, the `nft` binary, AND the
//!   per-tab cgroup ([`crate::cgroup`]). Missing any ⇒ [`apply`] is a
//!   best-effort no-op (logs at debug) and the tab is **not** confined.
//! - The OUTPUT hook stays `policy accept` and only *jumps* the tab's own
//!   cgroup into the policing chain, so it can never affect the host/daemon.
//!
//! ## Scope
//!
//! The pure ruleset generator ([`ruleset`]) is unit-tested; [`apply`] /
//! [`teardown`] shell out to `nft`. CIDR-only — domain allowlists are the
//! DNS resolver's job (nftables can't match a hostname).

#![cfg(all(target_os = "linux", not(feature = "gui")))]

use std::fmt::Write as _;

use crate::net_policy::Cidr;

/// nftables table name for a tab. Sanitised so the id is a safe nft
/// identifier (alnum + `_`); collisions across tabs are impossible because
/// tab ids are UUIDs.
#[must_use]
pub fn table_name(tab_id: &str) -> String {
    let safe: String = tab_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("tabatelier_{safe}")
}

/// Build the `nft -f` ruleset that confines `cgroup_rel` (the tab's cgroup
/// path relative to the v2 mount, e.g.
/// `system.slice/tab-atelier-headless.service/tab-<id>`) to `cidrs`.
///
/// The OUTPUT hook stays `policy accept` (so the daemon and every other
/// process are untouched) and only *jumps* sockets belonging to this tab's
/// cgroup into the policing chain, which accepts loopback / DNS / the
/// allowlisted networks and drops the rest. An empty CIDR list still emits
/// a valid ruleset (loopback + DNS only — everything outbound denied).
#[must_use]
pub fn ruleset(table: &str, cgroup_rel: &str, cidrs: &[Cidr]) -> String {
    let cgroup_rel = cgroup_rel.trim_matches('/');
    // `socket cgroupv2 level N "path"` matches the socket's cgroup at depth
    // N; N is the number of path components.
    let level = cgroup_rel.split('/').filter(|s| !s.is_empty()).count();

    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for c in cidrs {
        match c {
            Cidr::V4 { base, prefix } => {
                let o = base.to_be_bytes();
                v4.push(format!("{}.{}.{}.{}/{}", o[0], o[1], o[2], o[3], prefix));
            }
            Cidr::V6 { base, prefix } => {
                v6.push(format!("{}/{}", fmt_v6(*base), prefix));
            }
        }
    }

    // The policing chain is named `confine`, NOT `policy` — `policy` is a
    // reserved nftables keyword (used in `policy accept;`), so a chain by
    // that name is a syntax error.
    let mut s = String::new();
    let _ = writeln!(s, "table inet {table} {{");
    s.push_str("  chain confine {\n");
    s.push_str("    oifname \"lo\" accept comment \"loopback (local API, resolver)\"\n");
    s.push_str("    ct state established,related accept comment \"replies\"\n");
    s.push_str("    udp dport 53 accept comment \"dns\"\n");
    s.push_str("    tcp dport 53 accept comment \"dns\"\n");
    if !v4.is_empty() {
        let _ = writeln!(
            s,
            "    ip daddr {{ {} }} accept comment \"allowlist v4\"",
            v4.join(", ")
        );
    }
    if !v6.is_empty() {
        let _ = writeln!(
            s,
            "    ip6 daddr {{ {} }} accept comment \"allowlist v6\"",
            v6.join(", ")
        );
    }
    s.push_str("    drop comment \"tab-atelier: off-allowlist egress denied\"\n");
    s.push_str("  }\n");
    s.push_str("  chain out {\n");
    s.push_str("    type filter hook output priority 0; policy accept;\n");
    let _ = writeln!(
        s,
        "    socket cgroupv2 level {level} \"{cgroup_rel}\" jump confine comment \"tab-atelier egress allowlist\""
    );
    s.push_str("  }\n");
    s.push_str("}\n");
    s
}

/// Format a u128 as a fully-expanded IPv6 address (no `::` compression —
/// nft accepts the long form and it keeps the generator trivial).
fn fmt_v6(v: u128) -> String {
    let g = v.to_be_bytes();
    (0..8)
        .map(|i| format!("{:x}", u16::from_be_bytes([g[i * 2], g[i * 2 + 1]])))
        .collect::<Vec<_>>()
        .join(":")
}

/// Install the ruleset for a tab. Best-effort.
///
/// Returns `false` (and logs at debug) when `nft` is missing or the command
/// fails — a tab is never killed over a firewall-programming failure, but
/// note that a `false` here means the tab is **not** egress-confined.
///
/// Idempotent: drops any existing table of the same name first, so a
/// respawn re-applies cleanly.
#[must_use]
pub fn apply(tab_id: &str, cgroup_rel: &str, cidrs: &[Cidr]) -> bool {
    let table = table_name(tab_id);
    teardown(tab_id);
    let script = ruleset(&table, cgroup_rel, cidrs);
    match run_nft_stdin(&script) {
        Ok(true) => {
            log::debug!(
                "net_nft: applied egress allowlist for tab {tab_id} ({} cidrs)",
                cidrs.len()
            );
            true
        }
        Ok(false) => {
            log::debug!("net_nft: nft rejected ruleset for tab {tab_id}; tab unconfined at kernel level");
            false
        }
        Err(e) => {
            log::debug!("net_nft: could not run nft for tab {tab_id}: {e}; kernel enforcement skipped");
            false
        }
    }
}

/// Resolve the `nft` binary. It lives in `/usr/sbin` (or `/sbin`), which a
/// hardened service's `PATH` may not include — so prefer the absolute paths
/// and fall back to a bare `nft` (PATH lookup) only if none exist.
fn nft_bin() -> &'static str {
    for p in ["/usr/sbin/nft", "/sbin/nft", "/usr/bin/nft"] {
        if std::path::Path::new(p).is_file() {
            return p;
        }
    }
    "nft"
}

/// Remove a tab's table. Best-effort and silent — a missing table (never
/// applied, or already gone) is not an error.
pub fn teardown(tab_id: &str) {
    let table = table_name(tab_id);
    let _ = std::process::Command::new(nft_bin())
        .args(["delete", "table", "inet", &table])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Pipe an nft script to `nft -f -`. `Ok(true)` on success, `Ok(false)` on
/// a non-zero exit (bad ruleset / no permission), `Err` if `nft` can't be
/// spawned at all.
fn run_nft_stdin(script: &str) -> std::io::Result<bool> {
    use std::io::Write;
    let mut child = std::process::Command::new(nft_bin())
        .args(["-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes())?;
    }
    Ok(child.wait()?.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_name_sanitises() {
        assert_eq!(table_name("16eb00d6-17e7-48c2"), "tabatelier_16eb00d6_17e7_48c2");
        assert_eq!(table_name("a/b c"), "tabatelier_a_b_c");
    }

    #[test]
    fn ruleset_has_cgroup_jump_and_drop() {
        let cidrs = vec![Cidr::parse("104.16.0.0/13").unwrap()];
        let rs = ruleset("t", "system.slice/svc/tab-x", &cidrs);
        // Three path components ⇒ level 3.
        assert!(
            rs.contains("socket cgroupv2 level 3 \"system.slice/svc/tab-x\" jump confine"),
            "{rs}"
        );
        // Output hook must stay accept so the daemon isn't policed.
        assert!(rs.contains("hook output priority 0; policy accept;"));
        // The allowlisted v4 net is accepted, then everything else dropped.
        assert!(rs.contains("ip daddr { 104.16.0.0/13 } accept"));
        assert!(rs.contains("    drop comment "));
        // Loopback + DNS always allowed (local API, proxy, resolution).
        assert!(rs.contains("oifname \"lo\" accept"));
        assert!(rs.contains("udp dport 53 accept"));
    }

    #[test]
    fn ruleset_handles_v6_and_empty() {
        let cidrs = vec![Cidr::parse("2606:4700::/32").unwrap()];
        let rs = ruleset("t", "a/b", &cidrs);
        assert!(rs.contains("ip6 daddr { 2606:4700:0:0:0:0:0:0/32 } accept"), "{rs}");
        // No v4 line when there are no v4 CIDRs.
        assert!(!rs.contains("ip daddr {"));

        // Empty allowlist → still valid (loopback + DNS, everything else drop).
        let empty = ruleset("t", "a/b", &[]);
        assert!(empty.contains("    drop comment "));
        assert!(!empty.contains("ip daddr"));
        assert!(!empty.contains("ip6 daddr"));
    }

    #[test]
    fn level_counts_components() {
        // Leading/trailing slashes don't inflate the level.
        let rs = ruleset("t", "/one/two/", &[]);
        assert!(rs.contains("level 2 \"one/two\""), "{rs}");
    }
}
