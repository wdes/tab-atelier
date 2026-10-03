// SPDX-License-Identifier: MPL-2.0

//! Anthropic API relay — the LOCAL hop.
//!
//! A local tab-atelier forwards its Claude tabs' Anthropic API calls to a
//! remote (see [`crate::RELAY_MODE`]). This module is what that hop needs: an
//! HTTP agent tuned for streaming, and the header constants the far end will
//! expect.
//!
//! **The egress side is not here any more.** It used to be — this module read
//! `~/.claude/.credentials.json`, refreshed the OAuth token and injected the
//! Claude Code headers. That role moved to the `tab-atelier-proxy` package,
//! which authenticates each person with a key of their own instead of sharing
//! one token across a fleet. The credential half left behind was dead code for
//! several releases: nothing called it, and it had quietly fallen behind the
//! proxy's copy (no 0600-from-creation write, no `sync_all`, no OAuth error
//! decoding). Deleted rather than left to rot.
//!
//! What remains of the wire protocol lives in [`claude_api`], shared with the
//! proxy and `catbus-agent`.

use std::time::Duration;

/// Beta flags and API version, re-exported so callers have one import for the
/// relay rather than two. The definitions are [`claude_api`]'s.
pub use claude_api::{ANTHROPIC_BETA, ANTHROPIC_VERSION, BASE_API_URL as ANTHROPIC_BASE, merge_beta};

/// A ureq agent for relay calls.
///
/// **No global timeout** (LLM streams run for minutes) and **default `WebPKI`
/// verification** (unlike the LAN self-signed remote agent). A connect timeout
/// still bounds a dead upstream.
///
/// `http_status_as_error(false)`: a relay must be transparent to the upstream's
/// status. ureq's default turns any non-2xx into `Err`, which would collapse a
/// real upstream 429/500/529 (with its explanatory body) into an opaque
/// synthetic 502 — the caller (Claude Code) then can't see the real reason or
/// honour Retry-After. With this off, non-2xx comes back as `Ok(resp)` and we
/// stream the true status + body through.
///
/// The User-Agent identifies THIS hop, not Claude Code: the next hop is our own
/// remote, which is entitled to know what is talking to it. The client's own
/// `claude-cli/…` travels in the forwarded headers, and it is that one Anthropic
/// eventually sees.
#[must_use]
pub fn relay_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .user_agent(concat!("tab-atelier/", env!("CARGO_PKG_VERSION"), " (relay)"))
        .build()
        .new_agent()
}

#[cfg(test)]
mod tests {
    use super::{ANTHROPIC_BETA, ANTHROPIC_VERSION, merge_beta};

    #[test]
    fn the_clients_beta_flags_survive_the_local_hop() {
        // The local hop is a pipe: the egress needs the client's flags to
        // merge, so dropping them here breaks a request two machines away.
        let merged = merge_beta(Some("context-management-2025-06-27"), ANTHROPIC_BETA);
        assert!(merged.starts_with("context-management-2025-06-27,"), "{merged}");
        for required in ANTHROPIC_BETA.split(',') {
            assert!(merged.contains(required), "{required} missing from {merged}");
        }
    }

    #[test]
    fn relay_header_constants_match_claude_code() {
        assert_eq!(ANTHROPIC_VERSION, "2023-06-01");
        assert!(ANTHROPIC_BETA.contains("oauth-2025-04-20"));
        assert!(ANTHROPIC_BETA.contains("claude-code-20250219"));
    }
}
