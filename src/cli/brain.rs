// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ⛑ brain — a "rescue tab" that watches every running tab for
//! known agent-failure signatures and auto-sends remediation
//! input (typically `continue\n`) when it spots one.
//!
//! Designed to be run AS a tab itself: the user spawns a tab whose
//! command is `tab-atelier-headless brain`, and the brain's log
//! output becomes that tab's scrollback. The OSC 2 title escape
//! at startup names the tab "⛑ brain" so `tab-atelier-headless
//! tabs` shows it with the right label.
//!
//! v1 is pure pattern-matching. The pattern set covers the
//! Anthropic API connectivity errors that drop Claude Code's TUI
//! to its `❯ continue` prompt — those are the cases worth most of
//! the value with zero LLM calls. v2 can fall back to invoking
//! Claude / catbus-agent for shapes the pattern set doesn't catch.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::cli::share_link::{Endpoint, agent, discover_endpoint};

const DEFAULT_INTERVAL_SECS: u64 = 5;
const COOLDOWN_SECS: u64 = 60;
const SCOPE_TAIL_BYTES: usize = 4096;

/// Captive-portal-style connectivity probe.
///
/// Before sending `continue\n` we make sure the box can actually
/// reach the open internet — otherwise Claude / catbus-agent will
/// just re-fail on the next API call, the brain will see the same
/// error needle, hit cooldown, and we waste a tick every minute
/// for the duration of the outage.
///
/// Endpoints are the same ones Android / Chrome / GNOME use for
/// captive-portal detection:
/// - `connectivitycheck.gstatic.com/generate_204` — Google
/// - `1.1.1.1/cdn-cgi/trace` — Cloudflare, hits the IP directly so
///   the probe also works when DNS itself is broken
///
/// Plain HTTP on purpose — the probe answer is a static empty 204
/// (or a 1-line text response from CF). No TLS handshake to fail
/// independently of the connectivity it's supposed to measure.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a probe result stays cached. Reuse for multiple tabs
/// in a single tick + survive a quick subsequent tick. Shorter than
/// `COOLDOWN_SECS` so a brief outage releases quickly once the
/// network's back.
const PROBE_TTL: Duration = Duration::from_secs(10);
const PROBE_ENDPOINTS: &[&str] = &[
    "http://connectivitycheck.gstatic.com/generate_204",
    "http://1.1.1.1/cdn-cgi/trace",
];

/// Cached connectivity verdict. `is_online()` returns the cached
/// value if it's still fresh, otherwise re-probes.
#[derive(Debug, Default)]
struct ConnectivityProbe {
    last_check: Option<Instant>,
    last_online: bool,
}

impl ConnectivityProbe {
    fn is_online(&mut self) -> bool {
        let now = Instant::now();
        if let Some(at) = self.last_check
            && now.duration_since(at) < PROBE_TTL
        {
            return self.last_online;
        }
        let online = probe_once();
        self.last_check = Some(now);
        self.last_online = online;
        online
    }
}

/// One pass through the probe endpoints. Returns true on the first
/// 2xx (anything in `[200, 300)`) — Google's CPD returns 204, CF's
/// returns 200 with a tiny text body.
fn probe_once() -> bool {
    let ag = ureq::Agent::config_builder()
        .timeout_global(Some(PROBE_TIMEOUT))
        .build();
    let ag: ureq::Agent = ag.into();
    for url in PROBE_ENDPOINTS {
        if let Ok(resp) = ag.get(*url).call() {
            let code = resp.status().as_u16();
            if (200..300).contains(&code) {
                return true;
            }
        }
    }
    false
}

/// A single trigger → action mapping. Substring match by design —
/// regex would buy precision we don't need (Anthropic's error
/// strings are stable) at the cost of pulling in `regex`.
#[derive(Debug, Clone, Copy)]
pub struct Pattern {
    /// Literal substring searched for in the trailing scrollback.
    pub needle: &'static str,
    /// Short identifier used in logs + cooldown keys.
    pub label: &'static str,
    /// Bytes sent to `POST /tabs/by-id/<uuid>/input` when this
    /// pattern fires.
    pub action: &'static str,
}

/// Order matters only weakly — we return the first match in the
/// scope. All current entries map to `continue\n`, but the type
/// leaves room for per-pattern recovery actions.
pub const PATTERNS: &[Pattern] = &[
    Pattern {
        needle: "API Error: Unable to connect to API",
        label: "anthropic-unreachable",
        action: "continue\n",
    },
    Pattern {
        needle: "ConnectionRefused",
        label: "connection-refused",
        action: "continue\n",
    },
    Pattern {
        needle: "Connection refused",
        label: "connection-refused",
        action: "continue\n",
    },
    Pattern {
        needle: "ECONNRESET",
        label: "tcp-reset",
        action: "continue\n",
    },
    Pattern {
        needle: "ETIMEDOUT",
        label: "tcp-timeout",
        action: "continue\n",
    },
    Pattern {
        needle: "503 Service Unavailable",
        label: "anthropic-503",
        action: "continue\n",
    },
    Pattern {
        needle: "Internal server error",
        label: "anthropic-5xx",
        action: "continue\n",
    },
    Pattern {
        needle: "Overloaded (529)",
        label: "anthropic-529",
        action: "continue\n",
    },
];

/// Searches the trailing window of `text` for a known failure pattern.
///
/// Trailing-window only — matches further back are stale signal (a
/// previous turn's error the user already resolved). Returns the
/// first match; `None` when nothing matches.
#[must_use]
pub fn scan_output(text: &str) -> Option<&'static Pattern> {
    let scope = if text.len() > SCOPE_TAIL_BYTES {
        &text[text.len() - SCOPE_TAIL_BYTES..]
    } else {
        text
    };
    PATTERNS.iter().find(|p| scope.contains(p.needle))
}

#[derive(Debug, Deserialize)]
struct TabInfo {
    id: String,
    name: String,
    /// "thinking" | "waiting" | "error" | None — the same flag the
    /// desktop's per-tab LED reflects. Sent by `set-status` from
    /// inside the agent's PTY. The brain treats `error` as an
    /// independent trigger (in case the agent's output didn't
    /// match any of our hard-coded patterns).
    #[serde(default)]
    agent_state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TabsResponse {
    tabs: Vec<TabInfo>,
}

/// Signal that fired for a tab: either a pattern needle in the
/// scrollback or just an `agent_state == "error"` flag. Both map to
/// the same default action (`continue\n`) today; the variant exists
/// so the log + cooldown key distinguishes them.
#[derive(Debug)]
enum Trigger {
    Pattern(&'static Pattern),
    AgentError,
}

impl Trigger {
    const fn label(&self) -> &'static str {
        match self {
            Self::Pattern(p) => p.label,
            Self::AgentError => "agent-state-error",
        }
    }

    const fn action(&self) -> &'static str {
        match self {
            Self::Pattern(p) => p.action,
            Self::AgentError => "continue\n",
        }
    }
}

/// Polled at every interval. Re-derives the endpoint each tick so
/// a daemon restart (different token, same URL) just resumes
/// silently on the next loop.
fn tick(cooldowns: &mut HashMap<(String, &'static str), Instant>, probe: &mut ConnectivityProbe) -> Result<(), String> {
    let ep: Endpoint = discover_endpoint()?;
    let ag = agent();
    let auth = format!("Bearer {}", ep.token);

    let tabs: TabsResponse = ag
        .get(format!("{}/tabs", ep.url))
        .header("Authorization", &auth)
        .call()
        .map_err(|e| format!("GET /tabs: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("parse /tabs: {e}"))?;

    let now = Instant::now();
    for tab in tabs.tabs {
        if tab.id.is_empty() {
            continue;
        }
        let output = ag
            .get(format!("{}/tabs/by-id/{}/output", ep.url, tab.id))
            .header("Authorization", &auth)
            .call()
            .map_err(|e| format!("GET output for {}: {e}", tab.id))?
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("read output for {}: {e}", tab.id))?;

        // Two parallel signals — a literal needle match in the
        // scrollback OR an `agent_state: "error"` flag set via
        // set-status from inside the agent. Pattern wins on tie
        // because its label is more specific than "agent-state-error".
        let trigger: Trigger = if let Some(p) = scan_output(&output) {
            Trigger::Pattern(p)
        } else if tab.agent_state.as_deref() == Some("error") {
            Trigger::AgentError
        } else {
            continue;
        };
        let key = (tab.id.clone(), trigger.label());
        if cooldowns
            .get(&key)
            .is_some_and(|t| now.duration_since(*t) < Duration::from_secs(COOLDOWN_SECS))
        {
            // Already fired recently for this tab+trigger — stay
            // quiet until the cooldown expires. Prevents tight
            // re-fire loops when the agent immediately re-errors
            // after our injected `continue`.
            continue;
        }

        // Connectivity gate. If the box can't reach the open
        // internet, sending `continue` would just trigger the same
        // error again and burn a cooldown for nothing. Skip the send
        // AND skip updating the cooldown so the next tick (~5s)
        // re-checks and fires as soon as the network's back.
        //
        // Cached for `PROBE_TTL` so multiple erroring tabs in a
        // single tick share one probe.
        if !probe.is_online() {
            println!(
                "⛑ brain: {name:<24} [{label}] → suppressed (no internet — probe failed)",
                name = tab.name,
                label = trigger.label(),
            );
            continue;
        }

        cooldowns.insert(key, now);

        let _ = ag
            .post(format!("{}/tabs/by-id/{}/input", ep.url, tab.id))
            .header("Authorization", &auth)
            .header("Content-Type", "application/octet-stream")
            .send(trigger.action().as_bytes())
            .map_err(|e| format!("POST input for {}: {e}", tab.id))?;

        println!(
            "⛑ brain: {name:<24} [{label}] → sent {action:?}",
            name = tab.name,
            label = trigger.label(),
            action = trigger.action()
        );
    }
    Ok(())
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let mut once = false;
    let mut interval = DEFAULT_INTERVAL_SECS;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--once" => once = true,
            "--interval" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    Some(n) if n >= 1 => interval = n,
                    _ => {
                        eprintln!("brain: --interval expects a number >= 1");
                        return 2;
                    }
                }
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: tab-atelier-headless brain [--once] [--interval SECS]\n\
                     Watches every tab for known agent-failure signatures and sends\n\
                     `continue\\n` to the matching tab. Cooldown {COOLDOWN_SECS}s per (tab,pattern)\n\
                     to prevent re-fire loops.\n\
                     Patterns: {n} known signatures (Anthropic API connectivity).\n\
                     Connectivity probe (Google generate_204 + Cloudflare 1.1.1.1) gates\n\
                     every send; offline → suppress, retry on next tick when back online.",
                    n = PATTERNS.len(),
                );
                return 0;
            }
            other => {
                eprintln!("brain: unknown argument: {other}");
                return 2;
            }
        }
        i += 1;
    }

    // Name the tab so `tab-atelier-headless tabs` and the share-link
    // viewer's <title> see the right label. OSC 2 = window title.
    print!("\x1b]2;\u{26d1} brain\x07");
    println!(
        "\u{26d1} brain — watching every {interval}s · {n} patterns · cooldown {COOLDOWN_SECS}s",
        n = PATTERNS.len()
    );

    let mut cooldowns: HashMap<(String, &'static str), Instant> = HashMap::new();
    let mut probe = ConnectivityProbe::default();
    loop {
        if let Err(e) = tick(&mut cooldowns, &mut probe) {
            // Log + keep going. The most likely error is a transient
            // daemon-restart window; the next tick will succeed.
            eprintln!("⛑ brain: tick failed: {e}");
        }
        if once {
            return 0;
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_the_canonical_anthropic_unreachable_string() {
        // The exact phrase Claude Code prints when Anthropic's API
        // refuses the connection — this is the case the user
        // reported.
        let log = "● Let me read the actual middleware config block:\n\
                   Read 1 file\n\
                   ⎿  API Error: Unable to connect to API (ConnectionRefused)\n\
                   ✻ Crunched for 5m 30s\n\
                   ❯ continue";
        let p = scan_output(log).expect("must match");
        assert_eq!(p.label, "anthropic-unreachable");
        assert_eq!(p.action, "continue\n");
    }

    #[test]
    fn scan_matches_connection_refused_substring() {
        // Looser match — any subprocess connection refused.
        let log = "[error] foo bar\nConnectionRefused\nbaz";
        assert!(scan_output(log).is_some());
    }

    #[test]
    fn scan_returns_none_on_clean_output() {
        let log = "$ ls\nfoo bar baz\n$ ";
        assert!(scan_output(log).is_none());
    }

    #[test]
    fn scan_only_looks_at_the_trailing_window() {
        // Pattern in the FAR past followed by lots of healthy
        // output → no match. Prevents re-firing on errors the user
        // has already moved past.
        let mut log = String::new();
        log.push_str("API Error: Unable to connect to API\n");
        log.push_str(&"healthy chatter ".repeat(SCOPE_TAIL_BYTES));
        assert!(scan_output(&log).is_none());
    }

    #[test]
    fn scan_matches_when_pattern_is_in_tail_within_window() {
        // Mirror image of the above — same long log, but with
        // the error AT THE END, in the window.
        let mut log = String::new();
        log.push_str(&"healthy chatter ".repeat(100));
        log.push_str("API Error: Unable to connect to API\n");
        assert!(scan_output(&log).is_some());
    }

    #[test]
    fn patterns_have_non_empty_labels_and_actions() {
        for p in PATTERNS {
            assert!(!p.needle.is_empty(), "needle empty for {p:?}");
            assert!(!p.label.is_empty(), "label empty for {p:?}");
            assert!(!p.action.is_empty(), "action empty for {p:?}");
        }
    }

    #[test]
    fn connectivity_probe_caches_within_ttl() {
        // First call populates the cache by hitting the real probe
        // endpoints — skip the network round-trip by pre-seeding the
        // cache to a known value and asserting the next call reuses
        // it without re-probing.
        let mut p = ConnectivityProbe {
            last_check: Some(Instant::now()),
            last_online: false,
        };
        // Fresh — must return cached false WITHOUT a real probe
        // call. If it re-probed, this test would flake on machines
        // with intermittent gstatic / cloudflare reachability.
        assert!(!p.is_online());
        // Pre-seed online: same logic, stays cached.
        p.last_online = true;
        assert!(p.is_online());
    }
}
