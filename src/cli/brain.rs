// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ⛑ brain — a "rescue tab" that watches every running tab for
//! known agent-failure signatures and auto-sends remediation
//! input (typically `continue\r`) when it spots one.
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
/// Before sending `continue\r` we make sure the box can actually
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
/// scope. All current entries map to `continue\r`, but the type
/// leaves room for per-pattern recovery actions.
pub const PATTERNS: &[Pattern] = &[
    Pattern {
        needle: "API Error: Unable to connect to API",
        label: "anthropic-unreachable",
        action: "continue\r",
    },
    Pattern {
        needle: "ConnectionRefused",
        label: "connection-refused",
        action: "continue\r",
    },
    Pattern {
        needle: "Connection refused",
        label: "connection-refused",
        action: "continue\r",
    },
    Pattern {
        needle: "ECONNRESET",
        label: "tcp-reset",
        action: "continue\r",
    },
    Pattern {
        needle: "ETIMEDOUT",
        label: "tcp-timeout",
        action: "continue\r",
    },
    Pattern {
        needle: "503 Service Unavailable",
        label: "anthropic-503",
        action: "continue\r",
    },
    Pattern {
        needle: "Internal server error",
        label: "anthropic-5xx",
        action: "continue\r",
    },
    Pattern {
        needle: "Overloaded (529)",
        label: "anthropic-529",
        action: "continue\r",
    },
    // Anthropic-side rate limit ("not your usage limit" — server
    // capacity throttling). Same shape as 529: retryable, the
    // 60 s cooldown gives Anthropic time to recover before the
    // next attempt.
    Pattern {
        needle: "Server is temporarily limiting requests",
        label: "anthropic-rate-limited",
        action: "continue\r",
    },
    // Network-layer abort mid-request. Claude Code prints this
    // when fetch()'s underlying TLS socket dies before the response
    // is fully received (mobile network handoff, ISP NAT timeout,
    // a transient Cloudflare 525, …). Same recovery as the other
    // network patterns: wait the cooldown, then `continue` on a
    // fresh connection.
    Pattern {
        needle: "The socket connection was closed unexpectedly",
        label: "socket-closed-unexpectedly",
        action: "continue\r",
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
        // `&text[text.len() - SCOPE_TAIL_BYTES..]` panics when the
        // byte offset lands mid-character (multi-byte UTF-8 — em
        // dashes, accents, emoji). Walk back to the nearest valid
        // char boundary; UTF-8 chars are at most 4 bytes so at most
        // 3 iterations.
        let mut start = text.len() - SCOPE_TAIL_BYTES;
        while start > 0 && !text.is_char_boundary(start) {
            start -= 1;
        }
        &text[start..]
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
    /// Durable agent CLI kind ("claude" / "catbus" / …). None when
    /// no agent has ever attached to this tab; the brain only
    /// monitors tabs whose kind is `"claude"`.
    #[serde(default)]
    agent_kind: Option<String>,
    /// Durable agent session UUID set by the Claude Code hook. The
    /// brain requires this in addition to `agent_kind == "claude"`
    /// so a tab that briefly ran Claude in the past but isn't
    /// currently in a session doesn't get auto-`continue`ed.
    #[serde(default)]
    agent_session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TabsResponse {
    tabs: Vec<TabInfo>,
}

/// Signal that fired for a tab: either a pattern needle in the
/// scrollback or just an `agent_state == "error"` flag. Both map to
/// the same default action (`continue\r`) today; the variant exists
/// so the log + cooldown key distinguishes them.
#[derive(Debug, Clone, Copy)]
enum Trigger {
    Pattern(&'static Pattern),
    AgentError,
}

impl Trigger {
    const fn label(self) -> &'static str {
        match self {
            Self::Pattern(p) => p.label,
            Self::AgentError => "agent-state-error",
        }
    }

    const fn action(self) -> &'static str {
        match self {
            Self::Pattern(p) => p.action,
            Self::AgentError => "continue\r",
        }
    }
}

/// A tab that's flagged and past its cooldown — a candidate for the
/// round-robin picker.
#[derive(Debug, Clone)]
struct Eligible {
    tab_id: String,
    tab_name: String,
    trigger: Trigger,
}

/// Round-robin pick from a slice. Advances `cursor` mod `len()` and
/// returns the chosen element (a reference into the slice, since the
/// caller still owns the Vec). `None` on empty input — caller treats
/// that as "nothing to do this tick" without advancing the cursor.
///
/// Extracted as a pure fn so tests can exercise the wrap-around +
/// monotonic-advance behaviour without mocking HTTP.
fn pick_round_robin<'a, T>(items: &'a [T], cursor: &mut usize) -> Option<&'a T> {
    if items.is_empty() {
        return None;
    }
    let idx = *cursor % items.len();
    *cursor = cursor.wrapping_add(1);
    items.get(idx)
}

/// Polled at every interval. Re-derives the endpoint each tick so
/// a daemon restart (different token, same URL) just resumes
/// silently on the next loop.
///
/// Round-robin send model — at most ONE `continue` per tick. If
/// five tabs are all stuck on the same connectivity error, sending
/// to all five simultaneously dogpiles whatever was wrong (rate
/// limit, transient 5xx) and we'd just collect five fresh failures.
/// Instead: collect all eligible tabs, pick one via the cursor,
/// fire only that one. The next tick (~5 s later) picks the next
/// one, and so on. Cooldown per (tab, pattern) still applies; the
/// round-robin just spaces out which one fires when.
fn tick(
    cooldowns: &mut HashMap<(String, &'static str), Instant>,
    probe: &mut ConnectivityProbe,
    cursor: &mut usize,
) -> Result<(), String> {
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
    let mut eligible: Vec<Eligible> = Vec::new();
    for tab in tabs.tabs {
        if tab.id.is_empty() {
            continue;
        }
        // Gate the entire per-tab scan on "Claude is mid-session
        // here". Without it, brain was polling /output on every tab
        // — including shell tabs, log tailers, vim sessions — and
        // anything whose scrollback happened to contain a needle
        // (e.g. `git log` showing "ECONNRESET" in a commit message)
        // would get an injected `continue\r`. Only tabs whose hook
        // has reported a Claude session are legitimate targets.
        if tab.agent_kind.as_deref() != Some("claude") || tab.agent_session_id.as_deref().unwrap_or("").is_empty() {
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
        eligible.push(Eligible {
            tab_id: tab.id,
            tab_name: tab.name,
            trigger,
        });
    }

    if eligible.is_empty() {
        return Ok(());
    }

    // Connectivity gate. If the box can't reach the open internet,
    // sending `continue` would just trigger the same error again and
    // burn a cooldown for nothing. Skip the send AND skip updating
    // the cooldown / round-robin cursor so the next tick (~5 s)
    // re-probes and fires as soon as the network's back. One probe
    // covers the whole eligible set; the result is cached for
    // `PROBE_TTL` so tabs share it.
    if !probe.is_online() {
        println!(
            "⛑ brain: {n} tab(s) flagged but suppressed (no internet — probe failed)",
            n = eligible.len(),
        );
        return Ok(());
    }

    // Round-robin: pick one from the eligible set. Cursor advances
    // on every successful tick (online + at least one eligible), so
    // the next tick walks past this tab to its neighbours. Single
    // stuck tab → it always wins; multiple → rotation.
    let Some(pick) = pick_round_robin(&eligible, cursor) else {
        return Ok(());
    };
    let deferred = eligible.len() - 1;
    let key = (pick.tab_id.clone(), pick.trigger.label());
    cooldowns.insert(key, now);

    let _ = ag
        .post(format!("{}/tabs/by-id/{}/input", ep.url, pick.tab_id))
        .header("Authorization", &auth)
        .header("Content-Type", "application/octet-stream")
        .send(pick.trigger.action().as_bytes())
        .map_err(|e| format!("POST input for {}: {e}", pick.tab_id))?;

    if deferred > 0 {
        println!(
            "⛑ brain: {name:<24} [{label}] → sent {action:?} ({deferred} other tab(s) deferred — round-robin)",
            name = pick.tab_name,
            label = pick.trigger.label(),
            action = pick.trigger.action(),
        );
    } else {
        println!(
            "⛑ brain: {name:<24} [{label}] → sent {action:?}",
            name = pick.tab_name,
            label = pick.trigger.label(),
            action = pick.trigger.action(),
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
                     `continue\\r` to the matching tab. Cooldown {COOLDOWN_SECS}s per (tab,pattern)\n\
                     to prevent re-fire loops.\n\
                     Patterns: {n} known signatures (Anthropic API connectivity).\n\
                     Connectivity probe (Google generate_204 + Cloudflare 1.1.1.1) gates\n\
                     every send; offline → suppress, retry on next tick when back online.\n\
                     Round-robin: at most one send per tick across all eligible tabs.\n\
                     Five stuck tabs ⇒ they fire 5s apart, not all at once.",
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

    // Name the tab so the share-link viewer's <title> and any /tabs
    // consumer see the right label. OSC 2 = window title.
    print!("\x1b]2;\u{26d1} brain\x07");
    println!(
        "\u{26d1} brain — watching every {interval}s · {n} patterns · cooldown {COOLDOWN_SECS}s",
        n = PATTERNS.len()
    );

    let mut cooldowns: HashMap<(String, &'static str), Instant> = HashMap::new();
    let mut probe = ConnectivityProbe::default();
    let mut rr_cursor: usize = 0;
    loop {
        if let Err(e) = tick(&mut cooldowns, &mut probe, &mut rr_cursor) {
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
        assert_eq!(p.action, "continue\r");
    }

    #[test]
    fn scan_matches_connection_refused_substring() {
        // Looser match — any subprocess connection refused.
        let log = "[error] foo bar\nConnectionRefused\nbaz";
        assert!(scan_output(log).is_some());
    }

    #[test]
    fn scan_handles_multibyte_char_at_window_boundary() {
        // Regression: panicked when the trailing-window cutoff fell
        // mid-UTF-8. Repro pattern: an em dash (3-byte E2 80 94)
        // straddles the SCOPE_TAIL_BYTES boundary from the tail end,
        // and the slice operation panics on the start of the next
        // byte instead of finding a char boundary.
        let mut log = String::new();
        // Pad to push the em dash so part of it falls right on the
        // cutoff. With SCOPE_TAIL_BYTES = 4096, putting "—" at
        // position (total - 4097) puts byte 4096 (= cut) inside it.
        log.push_str(&"x".repeat(SCOPE_TAIL_BYTES - 1));
        log.push('—');
        log.push_str(&"y".repeat(SCOPE_TAIL_BYTES));
        // Must NOT panic.
        let _ = scan_output(&log);
    }

    #[test]
    fn scan_matches_anthropic_rate_limited() {
        // Canonical Claude Code output for Anthropic-side capacity
        // throttling — distinct from per-user usage limits, which
        // the user must fix themselves, hence the "(not your usage
        // limit)" parenthetical the brain SHOULDN'T retry around.
        // The needle matches only the server-side phrasing.
        let log = "● API Error: Server is temporarily limiting requests \
                   (not your usage limit) · Rate limited\n\
                   ❯ continue";
        let p = scan_output(log).expect("must match");
        assert_eq!(p.label, "anthropic-rate-limited");
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

    #[test]
    fn round_robin_empty_returns_none_without_advancing_cursor() {
        // No work this tick → cursor must NOT advance, otherwise a
        // long quiet period would slide the start index past every
        // possible "first" of the next non-empty eligible set and
        // we'd skip tabs unfairly.
        let items: [&str; 0] = [];
        let mut cursor = 7;
        assert!(pick_round_robin(&items, &mut cursor).is_none());
        assert_eq!(cursor, 7);
    }

    #[test]
    fn round_robin_single_item_always_picks_it() {
        let items = ["only-tab"];
        let mut cursor = 0;
        assert_eq!(pick_round_robin(&items, &mut cursor), Some(&"only-tab"));
        assert_eq!(pick_round_robin(&items, &mut cursor), Some(&"only-tab"));
        assert_eq!(pick_round_robin(&items, &mut cursor), Some(&"only-tab"));
    }

    #[test]
    fn round_robin_rotates_through_set() {
        // The shape of the actual behaviour the user asked for: 3
        // stuck tabs fire in order A, B, C, A, B, C, …
        let items = ["A", "B", "C"];
        let mut cursor = 0;
        let picks: Vec<&str> = (0..7)
            .map(|_| *pick_round_robin(&items, &mut cursor).unwrap())
            .collect();
        assert_eq!(picks, vec!["A", "B", "C", "A", "B", "C", "A"]);
    }

    #[test]
    fn round_robin_starting_cursor_offsets_the_first_pick() {
        // Cursor 4 in a 3-item set hits idx 4 % 3 = 1 = "B".
        let items = ["A", "B", "C"];
        let mut cursor = 4;
        assert_eq!(pick_round_robin(&items, &mut cursor), Some(&"B"));
        assert_eq!(cursor, 5);
        assert_eq!(pick_round_robin(&items, &mut cursor), Some(&"C"));
    }

    #[test]
    fn round_robin_survives_wrap_around() {
        // wrapping_add at usize::MAX shouldn't panic. The cursor
        // wraps to 0 and the next pick goes to idx 0.
        let items = ["A", "B", "C"];
        let mut cursor = usize::MAX;
        // (usize::MAX) % 3 = 0 → "A". Then cursor wraps to 0.
        assert_eq!(pick_round_robin(&items, &mut cursor), Some(&"A"));
        assert_eq!(cursor, 0);
    }

    #[test]
    fn round_robin_set_shrinks_between_ticks() {
        // Realistic shape: this tick sees 3 eligible tabs, next tick
        // only 1 (the other 2 are now in cooldown). Cursor advanced
        // to 1 last tick; the new set's len is 1 so idx = 1 % 1 = 0
        // — we pick the lone remaining tab without panic.
        let mut cursor = 1;
        let three = ["A", "B", "C"];
        assert_eq!(pick_round_robin(&three, &mut cursor), Some(&"B"));
        // Now only one eligible left.
        let one = ["Z"];
        assert_eq!(pick_round_robin(&one, &mut cursor), Some(&"Z"));
        assert_eq!(cursor, 3);
    }

    #[test]
    fn eligible_label_distinguishes_pattern_from_agent_error() {
        // Cooldown key uses (tab_id, label) — pattern hits and
        // agent_state-driven hits MUST have distinct labels or one
        // would cooldown-suppress the other.
        let pattern = &PATTERNS[0];
        let p = Eligible {
            tab_id: "tab-1".into(),
            tab_name: "shell".into(),
            trigger: Trigger::Pattern(pattern),
        };
        let a = Eligible {
            tab_id: "tab-1".into(),
            tab_name: "shell".into(),
            trigger: Trigger::AgentError,
        };
        assert_ne!(p.trigger.label(), a.trigger.label());
    }
}
