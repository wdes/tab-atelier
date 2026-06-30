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
//! The pure ruleset generators ([`ruleset`] / [`meter_ruleset`]) are
//! unit-tested; [`apply`] / [`apply_meter`] / [`teardown`] shell out to
//! `nft`. CIDR-only enforcement — domain allowlists are the DNS resolver's
//! job (nftables can't match a hostname).
//!
//! ## Metering
//!
//! Every non-net-off tab gets a per-tab table (allowlist tabs the confining
//! [`ruleset`], plain tabs the count-only [`meter_ruleset`]), each with a
//! `counter` on the cgroup match — so [`read_counters`] yields per-tab
//! egress bytes (total, and denied for allowlist tabs) for *all* tabs.

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
    // `counter drop`: bytes/packets DENIED (tried to leave the allowlist).
    s.push_str("    counter drop comment \"tab-atelier: off-allowlist egress denied\"\n");
    s.push_str("  }\n");
    s.push_str("  chain out {\n");
    s.push_str("    type filter hook output priority 0; policy accept;\n");
    // `counter` on the jump: TOTAL egress from this tab's cgroup (allowed +
    // denied). Allowed = total − denied, read back by `read_counters`.
    let _ = writeln!(
        s,
        "    socket cgroupv2 level {level} \"{cgroup_rel}\" counter jump confine comment \"tab-atelier egress allowlist\""
    );
    s.push_str("  }\n");
    s.push_str("}\n");
    s
}

/// Build a **count-only** ruleset for a tab that is NOT in allowlist mode.
///
/// One table whose OUTPUT-hook rule matches the tab's cgroup, bumps an
/// (anonymous) counter and accepts. No drop — the tab reaches anywhere; we
/// just meter its egress so *every* tab gets byte counts, not only confined
/// ones. Same `socket cgroupv2` match as [`ruleset`].
#[must_use]
pub fn meter_ruleset(table: &str, cgroup_rel: &str) -> String {
    let cgroup_rel = cgroup_rel.trim_matches('/');
    let level = cgroup_rel.split('/').filter(|s| !s.is_empty()).count();
    let mut s = String::new();
    let _ = writeln!(s, "table inet {table} {{");
    s.push_str("  chain out {\n");
    s.push_str("    type filter hook output priority 0; policy accept;\n");
    let _ = writeln!(
        s,
        "    socket cgroupv2 level {level} \"{cgroup_rel}\" counter accept comment \"tab-atelier metering\""
    );
    s.push_str("  }\n");
    s.push_str("}\n");
    s
}

/// Install the count-only metering table for a non-allowlist tab. Like
/// [`apply`] but no enforcement — best-effort, idempotent (teardown first).
#[must_use]
pub fn apply_meter(tab_id: &str, cgroup_rel: &str) -> bool {
    teardown(tab_id);
    matches!(run_nft_stdin(&meter_ruleset(&table_name(tab_id), cgroup_rel)), Ok(true))
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

/// Read a tab's egress byte counters: `(total_bytes, denied_bytes)`.
///
/// `total` is everything the tab's cgroup tried to send (the counter on the
/// jump rule); `denied` is what the allowlist dropped (the counter on the
/// `drop`). Allowed = total − denied. `None` when the table doesn't exist
/// (tab not confined) or `nft` can't be read.
#[must_use]
pub fn read_counters(tab_id: &str) -> Option<(u64, u64)> {
    let table = table_name(tab_id);
    let out = std::process::Command::new(nft_bin())
        .args(["-j", "list", "table", "inet", &table])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(parse_counters(&json))
}

/// Pull `(total, denied)` byte counts out of `nft -j list table` JSON. Pure
/// so it's unit-testable. `total` = the counter on the chain-`out` jump
/// rule; `denied` = the counter on the chain-`confine` `drop` rule.
#[must_use]
fn parse_counters(json: &serde_json::Value) -> (u64, u64) {
    let mut total = 0;
    let mut denied = 0;
    let Some(items) = json.get("nftables").and_then(|n| n.as_array()) else {
        return (0, 0);
    };
    for item in items {
        let Some(rule) = item.get("rule") else { continue };
        let chain = rule
            .get("chain")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let Some(exprs) = rule.get("expr").and_then(|e| e.as_array()) else {
            continue;
        };
        let counter_bytes = exprs.iter().find_map(|e| {
            e.get("counter")
                .and_then(|c| c.get("bytes"))
                .and_then(serde_json::Value::as_u64)
        });
        let Some(bytes) = counter_bytes else { continue };
        // The single rule in chain `out` carries the TOTAL counter, whether
        // it jumps into a confine chain (allowlist) or just accepts
        // (meter-only). The confine chain's drop rule carries DENIED.
        if chain == "out" {
            total = bytes;
        } else if chain == "confine" && exprs.iter().any(|e| e.get("drop").is_some()) {
            denied = bytes;
        }
    }
    (total, denied)
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
            rs.contains("socket cgroupv2 level 3 \"system.slice/svc/tab-x\" counter jump confine"),
            "{rs}"
        );
        // Output hook must stay accept so the daemon isn't policed.
        assert!(rs.contains("hook output priority 0; policy accept;"));
        // The allowlisted v4 net is accepted, then everything else dropped.
        assert!(rs.contains("ip daddr { 104.16.0.0/13 } accept"));
        assert!(rs.contains("    counter drop comment "));
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
        assert!(empty.contains("    counter drop comment "));
        assert!(!empty.contains("ip daddr"));
        assert!(!empty.contains("ip6 daddr"));
    }

    #[test]
    fn parse_counters_extracts_total_and_denied() {
        // Shape of `nft -j list table inet …`: a jump rule (chain out) with
        // the total counter, and a drop rule (chain confine) with denied.
        let json: serde_json::Value = serde_json::from_str(
            r#"{"nftables":[
                {"metainfo":{"version":"1.1.3"}},
                {"table":{"family":"inet","name":"tabatelier_x"}},
                {"rule":{"chain":"confine","expr":[
                    {"counter":{"packets":3,"bytes":240}},{"drop":null}]}},
                {"rule":{"chain":"out","expr":[
                    {"match":{"left":{"socket":{"key":"cgroupv2"}}}},
                    {"counter":{"packets":12,"bytes":4096}},
                    {"jump":{"target":"confine"}}]}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(parse_counters(&json), (4096, 240)); // (total, denied)
    }

    #[test]
    fn parse_counters_empty_is_zero() {
        let json: serde_json::Value = serde_json::from_str(r#"{"nftables":[]}"#).unwrap();
        assert_eq!(parse_counters(&json), (0, 0));
    }

    #[test]
    fn meter_ruleset_counts_and_accepts_no_drop() {
        let rs = meter_ruleset("t", "/a/b/c/");
        assert!(
            rs.contains("socket cgroupv2 level 3 \"a/b/c\" counter accept comment \"tab-atelier metering\""),
            "{rs}"
        );
        assert!(!rs.contains("drop"), "meter-only never drops");
        assert!(!rs.contains("confine"), "no policing chain");
    }

    #[test]
    fn parse_counters_meter_only_has_total_no_denied() {
        // Meter-only out rule: counter + accept, no jump, no confine chain.
        let json: serde_json::Value = serde_json::from_str(
            r#"{"nftables":[{"rule":{"chain":"out","expr":[
                {"match":{"left":{"socket":{"key":"cgroupv2"}}}},
                {"counter":{"packets":5,"bytes":2048}},{"accept":null}]}}]}"#,
        )
        .unwrap();
        assert_eq!(parse_counters(&json), (2048, 0));
    }

    #[test]
    fn level_counts_components() {
        // Leading/trailing slashes don't inflate the level.
        let rs = ruleset("t", "/one/two/", &[]);
        assert!(rs.contains("level 2 \"one/two\""), "{rs}");
    }
}
