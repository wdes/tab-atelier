// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-tab network policy — the pure model shared by every enforcement
//! path. A tab is in one of three modes:
//!
//! * **On** — normal, unrestricted (the default).
//! * **Off** — full airgap. Enforced by bubblewrap `--unshare-net` (see
//!   [`crate::no_internet_command`]); the tab gets an empty net namespace
//!   with only loopback.
//! * **Allowlist** — selective egress: only the listed domains / CIDRs are
//!   reachable. Two enforcement mechanisms consume the same allow-set built
//!   here:
//!   - a local filtering CONNECT proxy (domains; works unprivileged, so it
//!     covers the desktop GUI), injected into the tab via `HTTPS_PROXY`;
//!   - on the privileged headless service, nftables CIDR rules in the tab's
//!     net namespace (hard enforcement, catches non-proxy-aware clients).
//!
//! This module is **pure**: presets, the resolved allow-set, and the
//! host/IP match decisions. No sockets, no process spawning — that lives in
//! the proxy and the spawn paths. Keeping it dependency-free (own tiny CIDR
//! parser, no `ipnet`) so it compiles in both the GUI and headless builds
//! and is trivially unit-testable.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// A named bundle of allow-rules the user can switch on by name.
///
/// Saves typing the individual hosts/ranges. Stored by id so a refreshed
/// preset (e.g. new Cloudflare ranges) applies on upgrade without rewriting
/// saved state.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    /// Just enough for Claude Code to reach the Anthropic API. Telemetry is
    /// already disabled via injected env vars, so the surface is small.
    ClaudeCode,
    /// Cloudflare's published edge ranges (v4 + v6). Useful when a tab only
    /// needs to reach sites fronted by Cloudflare.
    Cloudflare,
}

impl Preset {
    /// Parse the on-disk / CLI id.
    #[must_use]
    pub fn from_id(s: &str) -> Option<Self> {
        match s {
            "claude-code" => Some(Self::ClaudeCode),
            "cloudflare" => Some(Self::Cloudflare),
            _ => None,
        }
    }

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Cloudflare => "cloudflare",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code (Anthropic API)",
            Self::Cloudflare => "Cloudflare edge ranges",
        }
    }

    /// Domain suffixes this preset grants. See [`AllowSet::domains`] for the
    /// match semantics (suffix / subdomain).
    #[must_use]
    pub const fn domains(self) -> &'static [&'static str] {
        match self {
            // api.anthropic.com is the only endpoint Claude Code needs once
            // telemetry (statsig) is disabled by our injected envs. Kept
            // tight on purpose — widen with a custom domain if needed.
            Self::ClaudeCode => &["api.anthropic.com"],
            Self::Cloudflare => &[],
        }
    }

    /// CIDR strings this preset grants. Cloudflare publishes these at
    /// <https://www.cloudflare.com/ips/>; snapshotted here so enforcement
    /// doesn't depend on a network fetch at tab start.
    #[must_use]
    pub const fn cidrs(self) -> &'static [&'static str] {
        match self {
            Self::ClaudeCode => &[],
            Self::Cloudflare => CLOUDFLARE_CIDRS,
        }
    }
}

/// Cloudflare's published IPv4 + IPv6 edge ranges (snapshot). Source:
/// <https://www.cloudflare.com/ips-v4> and `/ips-v6`.
const CLOUDFLARE_CIDRS: &[&str] = &[
    // IPv4
    "173.245.48.0/20",
    "103.21.244.0/22",
    "103.22.200.0/22",
    "103.31.4.0/22",
    "141.101.64.0/18",
    "108.162.192.0/18",
    "190.93.240.0/20",
    "188.114.96.0/20",
    "197.234.240.0/22",
    "198.41.128.0/17",
    "162.158.0.0/15",
    "104.16.0.0/13",
    "104.24.0.0/14",
    "172.64.0.0/13",
    "131.0.72.0/22",
    // IPv6
    "2400:cb00::/32",
    "2606:4700::/32",
    "2803:f800::/32",
    "2405:b500::/32",
    "2405:8100::/32",
    "2a06:98c0::/29",
    "2c0f:f248::/32",
];

/// A parsed CIDR network (base address + prefix length), v4 or v6.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cidr {
    V4 { base: u32, prefix: u8 },
    V6 { base: u128, prefix: u8 },
}

impl Cidr {
    /// Parse `"10.0.0.0/8"` / `"2606:4700::/32"`. A bare address (no `/`) is
    /// treated as a host route (`/32` or `/128`). Returns `None` on garbage
    /// or an out-of-range prefix.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        match addr.parse::<IpAddr>().ok()? {
            IpAddr::V4(v4) => {
                let prefix = prefix.map_or(Some(32u8), |p| p.parse().ok())?;
                if prefix > 32 {
                    return None;
                }
                let base = u32::from(v4) & mask_v4(prefix);
                Some(Self::V4 { base, prefix })
            }
            IpAddr::V6(v6) => {
                let prefix = prefix.map_or(Some(128u8), |p| p.parse().ok())?;
                if prefix > 128 {
                    return None;
                }
                let base = u128::from(v6) & mask_v6(prefix);
                Some(Self::V6 { base, prefix })
            }
        }
    }

    /// Whether `ip` falls inside this network. A v4 CIDR never matches a v6
    /// address and vice-versa.
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self, ip) {
            (Self::V4 { base, prefix }, IpAddr::V4(a)) => u32::from(a) & mask_v4(*prefix) == *base,
            (Self::V6 { base, prefix }, IpAddr::V6(a)) => u128::from(a) & mask_v6(*prefix) == *base,
            _ => false,
        }
    }
}

/// `/prefix` network mask for IPv4. `prefix == 0` ⇒ all-zero mask (matches
/// everything); the shift is guarded because `u32 << 32` is UB-ish (panics
/// in debug).
const fn mask_v4(prefix: u8) -> u32 {
    if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) }
}

const fn mask_v6(prefix: u8) -> u128 {
    if prefix == 0 { 0 } else { u128::MAX << (128 - prefix) }
}

/// The resolved set of destinations a tab may reach, flattened from presets
/// plus the user's custom entries. Built once when a tab's policy is set and
/// handed to whichever enforcement path is active.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowSet {
    /// Domain suffixes. A connection to `host` is allowed when `host` equals
    /// an entry or is a subdomain of it (`api.anthropic.com` allows
    /// `api.anthropic.com` but not `evil-api.anthropic.com.attacker.test`).
    /// A leading `*.` (e.g. `*.example.com`) is accepted and normalised to
    /// the bare suffix (subdomains only, not the apex).
    pub domains: Vec<String>,
    /// Domains that must match the APEX as well (from a plain entry, not a
    /// `*.` wildcard). Parallel to `domains`; `true` ⇒ the bare host matches.
    apex_ok: Vec<bool>,
    /// Allowed networks.
    pub cidrs: Vec<Cidr>,
}

impl AllowSet {
    /// Build from preset ids + custom domains + custom CIDR strings. Unknown
    /// presets and unparseable CIDRs are skipped (the caller validates and
    /// reports those separately); duplicate domains are de-duplicated.
    #[must_use]
    pub fn build(presets: &[Preset], custom_domains: &[String], custom_cidrs: &[String]) -> Self {
        let mut set = Self::default();
        for p in presets {
            for d in p.domains() {
                set.push_domain(d);
            }
            for c in p.cidrs() {
                if let Some(cidr) = Cidr::parse(c) {
                    set.cidrs.push(cidr);
                }
            }
        }
        for d in custom_domains {
            set.push_domain(d);
        }
        for c in custom_cidrs {
            if let Some(cidr) = Cidr::parse(c) {
                set.cidrs.push(cidr);
            }
        }
        set
    }

    fn push_domain(&mut self, raw: &str) {
        let raw = raw.trim().trim_end_matches('.').to_ascii_lowercase();
        if raw.is_empty() {
            return;
        }
        let (suffix, apex) = raw
            .strip_prefix("*.")
            .map_or((raw.as_str(), true), |bare| (bare, false));
        if suffix.is_empty() {
            return;
        }
        // De-dup; if the same suffix arrives both apex-ok and wildcard-only,
        // keep the more permissive (apex-ok) flag.
        if let Some(i) = self.domains.iter().position(|d| d == suffix) {
            self.apex_ok[i] = self.apex_ok[i] || apex;
            return;
        }
        self.domains.push(suffix.to_string());
        self.apex_ok.push(apex);
    }

    /// Whether a hostname (from a proxy CONNECT target or an SNI) is allowed.
    /// Case-insensitive; a trailing dot and a `:port` suffix are ignored.
    #[must_use]
    pub fn host_allowed(&self, host: &str) -> bool {
        let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return false;
        }
        // A literal IP target is matched against the CIDRs.
        if let Ok(ip) = host.parse::<IpAddr>() {
            return self.ip_allowed(ip);
        }
        for (i, suffix) in self.domains.iter().enumerate() {
            if host == *suffix {
                if self.apex_ok[i] {
                    return true;
                }
            } else if host.ends_with(suffix.as_str()) && host.as_bytes()[host.len() - suffix.len() - 1] == b'.' {
                return true;
            }
        }
        false
    }

    /// Whether a resolved IP is in one of the allowed CIDRs.
    #[must_use]
    pub fn ip_allowed(&self, ip: IpAddr) -> bool {
        self.cidrs.iter().any(|c| c.contains(ip))
    }

    /// Is the allow-set empty (no presets matched, no custom entries)? An
    /// empty allowlist would block everything, so the UI/CLI warns.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.domains.is_empty() && self.cidrs.is_empty()
    }
}

/// The three-state network mode persisted per tab.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetMode {
    #[default]
    On,
    Off,
    Allowlist,
}

/// The raw allowlist inputs for a tab (presets + custom entries).
///
/// Carried together through the spawn paths so the param lists don't
/// balloon. Flatten to the resolved [`AllowSet`] with [`Self::to_allow_set`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowConfig {
    pub presets: Vec<Preset>,
    pub domains: Vec<String>,
    pub cidrs: Vec<String>,
}

impl AllowConfig {
    /// No presets and no custom entries ⇒ the tab is not in allowlist mode.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.presets.is_empty() && self.domains.is_empty() && self.cidrs.is_empty()
    }

    /// Resolve to the match-set the proxy / nftables consume.
    #[must_use]
    pub fn to_allow_set(&self) -> AllowSet {
        AllowSet::build(&self.presets, &self.domains, &self.cidrs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_parse_and_contains_v4() {
        let net = Cidr::parse("104.16.0.0/13").unwrap();
        assert!(net.contains("104.16.1.1".parse().unwrap()));
        assert!(net.contains("104.23.255.255".parse().unwrap()));
        assert!(!net.contains("104.24.0.1".parse().unwrap()));
        assert!(!net.contains("8.8.8.8".parse().unwrap()));
        // A v4 net never matches a v6 address.
        assert!(!net.contains("2606:4700::1".parse().unwrap()));
    }

    #[test]
    fn cidr_parse_and_contains_v6() {
        let net = Cidr::parse("2606:4700::/32").unwrap();
        assert!(net.contains("2606:4700:4700::1111".parse().unwrap()));
        assert!(!net.contains("2400:cb00::1".parse().unwrap()));
    }

    #[test]
    fn cidr_bare_address_is_host_route() {
        let net = Cidr::parse("1.2.3.4").unwrap();
        assert!(net.contains("1.2.3.4".parse().unwrap()));
        assert!(!net.contains("1.2.3.5".parse().unwrap()));
    }

    #[test]
    fn cidr_rejects_garbage() {
        assert!(Cidr::parse("not-an-ip").is_none());
        assert!(Cidr::parse("10.0.0.0/40").is_none());
        assert!(Cidr::parse("2606::/200").is_none());
    }

    #[test]
    fn cloudflare_preset_matches_published_ip() {
        let set = AllowSet::build(&[Preset::Cloudflare], &[], &[]);
        // 1.1.1.1 is in 1.0.0.0/?? — not in the published edge list, so use
        // a known edge range member instead.
        assert!(set.ip_allowed("104.16.0.5".parse().unwrap()));
        assert!(set.ip_allowed("2606:4700::1".parse().unwrap()));
        assert!(!set.ip_allowed("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn claude_preset_allows_api_host_and_subdomains() {
        let set = AllowSet::build(&[Preset::ClaudeCode], &[], &[]);
        assert!(set.host_allowed("api.anthropic.com"));
        assert!(set.host_allowed("api.anthropic.com:443"));
        assert!(set.host_allowed("API.Anthropic.Com")); // case-insensitive
        // A subdomain of the allowed host is allowed…
        assert!(set.host_allowed("edge.api.anthropic.com"));
        // …but a lookalike that merely ends with the string is NOT.
        assert!(!set.host_allowed("evil-api.anthropic.com.attacker.test"));
        assert!(!set.host_allowed("notanthropic.com"));
        assert!(!set.host_allowed("console.anthropic.com"));
    }

    #[test]
    fn wildcard_domain_excludes_apex() {
        let set = AllowSet::build(&[], &["*.example.com".to_string()], &[]);
        assert!(set.host_allowed("a.example.com"));
        assert!(set.host_allowed("deep.a.example.com"));
        assert!(!set.host_allowed("example.com"), "wildcard should not match the apex");
    }

    #[test]
    fn plain_domain_includes_apex_and_subdomains() {
        let set = AllowSet::build(&[], &["example.com".to_string()], &[]);
        assert!(set.host_allowed("example.com"));
        assert!(set.host_allowed("www.example.com"));
    }

    #[test]
    fn custom_cidr_and_literal_ip_host() {
        let set = AllowSet::build(&[], &[], &["10.0.0.0/8".to_string()]);
        // A CONNECT to a literal IP is checked against CIDRs.
        assert!(set.host_allowed("10.1.2.3"));
        assert!(set.host_allowed("10.1.2.3:8080"));
        assert!(!set.host_allowed("11.0.0.1"));
    }

    #[test]
    fn empty_allowset_blocks_everything() {
        let set = AllowSet::build(&[], &[], &[]);
        assert!(set.is_empty());
        assert!(!set.host_allowed("example.com"));
        assert!(!set.ip_allowed("104.16.0.5".parse().unwrap()));
    }

    #[test]
    fn preset_id_roundtrip() {
        for p in [Preset::ClaudeCode, Preset::Cloudflare] {
            assert_eq!(Preset::from_id(p.id()), Some(p));
            assert!(!p.label().is_empty());
        }
        assert_eq!(Preset::from_id("nope"), None);
    }

    #[test]
    fn dedup_keeps_apex_permissive() {
        // Same suffix from a wildcard and a plain entry → apex allowed.
        let set = AllowSet::build(&[], &["*.example.com".to_string(), "example.com".to_string()], &[]);
        assert!(set.host_allowed("example.com"));
        assert!(set.host_allowed("x.example.com"));
        assert_eq!(set.domains.len(), 1, "suffix de-duplicated");
    }
}
