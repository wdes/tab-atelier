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
/// A tab's `/output` must be byte-for-byte UNCHANGED for at least this
/// long before brain will nudge it. This is the "is the agent actually
/// working?" gate: any output activity — a spinner frame, a rate-limit
/// "retrying in 38s" countdown, real tool output scrolling — resets the
/// stability clock, so brain only ever nudges a tab whose screen has
/// been genuinely frozen. At the default 5 s poll that's ~4 ticks of
/// no movement.
const STABLE_SECS: u64 = 20;
const SCOPE_TAIL_BYTES: usize = 4096;
/// Exponential-backoff base. After the first `continue` for an error
/// episode, brain waits this long before the next nudge, doubling each
/// time the SAME error recurs — so it stops hammering a transient outage
/// (repeated `529 Overloaded`, rate limits). A different error resets it.
const NUDGE_BACKOFF_BASE_SECS: u64 = 60;
/// Backoff ceiling — a long outage still gets a retry roughly every 15 min.
const NUDGE_BACKOFF_MAX_SECS: u64 = 900;
/// Wall-clock ceiling on the per-tab scan of one tick. Kept below
/// [`DEFAULT_INTERVAL_SECS`] so a large fleet can't make a cycle outrun the
/// poll interval; the scan resumes at `scan_cursor` on the next tick. Worst
/// case is the budget plus one already-in-flight `/output` GET (3 s, the
/// `share_link` agent's global timeout).
const TICK_BUDGET: Duration = Duration::from_secs(4);
/// More than this many eligible tabs stuck on an Anthropic-capacity error
/// means the fleet is capped upstream, not individually wedged.
const CIRCUIT_BREAKER_THRESHOLD: usize = 5;
/// Nudge spacing while the fleet is capped, and how long that verdict stays
/// sticky so a spike doesn't flap the cadence.
const CIRCUIT_BREAKER_COOLDOWN: Duration = Duration::from_secs(30);

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
    // Current Claude Code wording for the same 529: "API Error: 529
    // Overloaded. This is a server-side issue, usually temporary …".
    // The parenthesised form above doesn't match it, so cover both.
    Pattern {
        needle: "529 Overloaded",
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
    // Streaming response cut off mid-flight — Claude Code prints
    //   "API Error: Connection closed mid-response. The response
    //    above may be incomplete."
    // when the SSE/response stream drops before completion (same
    // transient network causes as the socket-closed case). The turn
    // is left half-finished, so recover the same way: cooldown, then
    // `continue` to re-request on a fresh connection.
    Pattern {
        needle: "Connection closed mid-response",
        label: "connection-closed-mid-response",
        action: "continue\r",
    },
    // Auto-mode model-routing classifier briefly unavailable
    // (Anthropic-side dependency that decides which model to use
    // for the next turn). Shape Claude Code prints:
    //   "<model> is temporarily unavailable, so auto mode cannot
    //    determine the safety of Bash right now. Wait briefly …"
    // Recovery is identical to the other transient outages.
    Pattern {
        needle: "auto mode cannot determine the safety",
        label: "auto-mode-classifier-down",
        action: "continue\r",
    },
    // Claude Code's own auto-retry banner while an API call is failing:
    //   "✻ Waiting for API response · will retry in 1m 57s · check your
    //    network"
    // The countdown ticks every second, so a LIVE retry keeps the screen
    // moving and never trips the STABLE_SECS freeze gate — brain stays out
    // of its way and lets Claude retry itself (the intended behaviour). This
    // pattern only bites when that banner is FROZEN, i.e. the TUI hung on it
    // without actually counting down: after STABLE_SECS of a byte-identical
    // screen, `continue` unsticks it the same as the other transient errors.
    // Needle is the stable prefix before the countdown ("will retry in"),
    // which never appears in healthy output nor collides with the rate-limit
    // wording ("retrying in").
    Pattern {
        needle: "will retry in",
        label: "api-retry-waiting",
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
    /// Hash of the frozen `/output` that made this tab eligible.
    /// Recorded into the tab's [`TabWatch::nudged_hash`] on send so
    /// brain won't re-nudge the SAME frozen screen — only once the
    /// output changes (agent reacted, or re-stuck on something new)
    /// does the tab become eligible again.
    output_hash: u64,
}

/// Per-tab watch state, keyed by tab id. Tracks output stability so
/// brain can tell "frozen and stuck" from "actively working".
struct TabWatch {
    /// Hash of the last `/output` we saw for this tab.
    last_hash: u64,
    /// When the output first reached `last_hash`. `now - stable_since`
    /// is how long the screen has been frozen.
    stable_since: Instant,
    /// Output hash at the moment we last sent `continue` to this tab,
    /// or `None` if we've never nudged it (or its output changed since).
    /// Guards against re-nudging an unchanged frozen screen.
    nudged_hash: Option<u64>,
    /// Consecutive nudges for the current unresolved error episode —
    /// drives the exponential backoff. Reset to 0 when the tab recovers
    /// (no error trigger) or hits a different error.
    nudge_streak: u32,
    /// Earliest time the next nudge is allowed (the backoff gate).
    next_nudge_at: Option<Instant>,
    /// Error label of the last nudge; a different label resets the streak.
    last_label: Option<&'static str>,
}

impl TabWatch {
    /// `last_hash` starts at 0 so the first [`evaluate_tab`] call always takes
    /// the "output changed" branch — which re-stamps `stable_since` to the same
    /// `now` it was built with, so the freeze clock still starts here.
    const fn new(now: Instant) -> Self {
        Self {
            last_hash: 0,
            stable_since: now,
            nudged_hash: None,
            nudge_streak: 0,
            next_nudge_at: None,
            last_label: None,
        }
    }
}

/// FNV-1a hash of a tab's `/output`. Process-local only (never
/// persisted) so any stable hash function works; FNV is allocation-
/// free and fast enough for a few-KB string per tick.
fn hash_output(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Pure eligibility decision for a single tab, given its frozen-output
/// duration and nudge history. Extracted so the "don't nudge a working
/// or already-nudged tab" rule is unit-testable without HTTP.
///
/// Returns true only when the screen has been frozen ≥ [`STABLE_SECS`]
/// AND we haven't already nudged this exact frozen screen.
fn should_nudge(stable_for: Duration, nudged_hash: Option<u64>, current_hash: u64) -> bool {
    stable_for >= Duration::from_secs(STABLE_SECS) && nudged_hash != Some(current_hash)
}

/// Seconds to wait before the next nudge after `streak` consecutive
/// nudges for the same error episode: `BASE * 2^(streak-1)`, capped at
/// [`NUDGE_BACKOFF_MAX_SECS`]. So 60s, 120s, 240s, 480s, 900s, 900s…
fn backoff_secs(streak: u32) -> u64 {
    let shift = streak.saturating_sub(1).min(6);
    NUDGE_BACKOFF_BASE_SECS
        .saturating_mul(1u64 << shift)
        .min(NUDGE_BACKOFF_MAX_SECS)
}

/// Pure per-tab decision for one tick: folds this tick's `output` into
/// `watch` and returns the trigger to nudge on, or `None`. All HTTP stays in
/// [`Brain::tick`], so the whole gate chain — stability clock, trigger
/// detection, recovery reset, [`should_nudge`], backoff — is testable over
/// many simulated ticks with an injected `now`.
fn evaluate_tab(watch: &mut TabWatch, output: &str, agent_state: Option<&str>, now: Instant) -> Option<Trigger> {
    // Output-stability tracking — the core "is the agent working?" gate. A
    // changed screen means the agent is producing output → it's alive → reset
    // the clock. An unchanged one accumulates frozen time.
    let h = hash_output(output);
    if watch.last_hash != h {
        watch.last_hash = h;
        watch.stable_since = now;
    }

    // Identify the trigger for EVERY tab (not just frozen ones) so a recovered
    // tab is the reset point for the backoff. Two parallel signals — a literal
    // needle in the scrollback OR an `agent_state: "error"` flag set via
    // set-status. Pattern wins on tie (its label is more specific).
    let trigger = scan_output(output)
        .map(Trigger::Pattern)
        .or_else(|| (agent_state == Some("error")).then_some(Trigger::AgentError));
    let Some(trigger) = trigger else {
        // No error on screen → recovered (or never errored). Clear the backoff
        // so the next episode starts fresh.
        watch.nudge_streak = 0;
        watch.next_nudge_at = None;
        watch.last_label = None;
        return None;
    };

    // Frozen long enough AND not already nudged at this exact screen? (An
    // active auto-retry countdown keeps the screen moving, so it never freezes
    // STABLE_SECS and never reaches here.)
    if !should_nudge(now.duration_since(watch.stable_since), watch.nudged_hash, h) {
        return None;
    }

    // Exponential backoff while the SAME error keeps recurring, so brain
    // doesn't hammer a transient outage. A different label resets the streak
    // (applied on send).
    if watch.last_label == Some(trigger.label())
        && let Some(at) = watch.next_nudge_at
        && now < at
    {
        return None;
    }

    Some(trigger)
}

/// True when the fleet looks capped upstream rather than individually wedged:
/// more than [`CIRCUIT_BREAKER_THRESHOLD`] of THIS TICK's eligible tabs are
/// stuck on an Anthropic-capacity error. Sticky for [`CIRCUIT_BREAKER_COOLDOWN`]
/// so a spike doesn't flap the cadence.
///
/// Deliberately stateless per tick apart from that stickiness: deriving it from
/// `eligible` — which already means "frozen, unnudged at this screen, past
/// backoff" — makes recovery automatic. Recovered tabs leave `eligible`, the
/// count drops, normal cadence resumes. A time-stamped sliding window cannot do
/// that, because a frozen screen re-stamps the window faster than it can age
/// out and brain goes silent forever.
fn systemic_api_freeze(eligible: &[Eligible], breaker_until: &mut Option<Instant>, now: Instant) -> bool {
    let capped = eligible
        .iter()
        .filter(|e| is_api_storm_label(e.trigger.label()))
        .count();
    if capped > CIRCUIT_BREAKER_THRESHOLD {
        *breaker_until = Some(now + CIRCUIT_BREAKER_COOLDOWN);
        return true;
    }
    match *breaker_until {
        Some(until) if now < until => true,
        Some(_) => {
            *breaker_until = None;
            false
        }
        None => false,
    }
}

/// Anthropic-side capacity errors — the ones that hit the whole fleet at once.
/// Local per-box faults (connection refused, TCP resets) are excluded: those
/// are independent failures, not a shared cap.
///
/// `api-retry-waiting` is excluded too. "will retry in" is Claude Code's
/// healthy self-retry banner (see [`PATTERNS`]); counting it would let five
/// recovering tabs throttle the fleet.
fn is_api_storm_label(label: &str) -> bool {
    matches!(
        label,
        "anthropic-529" | "anthropic-rate-limited" | "anthropic-503" | "anthropic-5xx"
    )
}

/// One-per-`min_interval` gate: returns true and stamps `last_at` when the
/// interval has elapsed (or nothing has been sent yet), false otherwise. Used
/// only for the systemic heartbeat — normal cadence is already spaced by the
/// round-robin's one-send-per-tick.
fn nudge_ready(last_at: &mut Option<Instant>, now: Instant, min_interval: Duration) -> bool {
    if let Some(at) = *last_at
        && now.duration_since(at) < min_interval
    {
        return false;
    }
    *last_at = Some(now);
    true
}

/// Indices `0..n` starting at `cursor % n` and wrapping — the scan order for a
/// budget-truncated tick that resumes where the previous one stopped.
fn scan_order(n: usize, cursor: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let start = cursor % n;
    (0..n).map(|i| (start + i) % n).collect()
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

/// Everything that survives between ticks.
#[derive(Default)]
struct Brain {
    watches: HashMap<String, TabWatch>,
    probe: ConnectivityProbe,
    /// Which eligible tab gets this tick's single nudge.
    rr_cursor: usize,
    /// Where the last [`TICK_BUDGET`]-truncated scan stopped.
    scan_cursor: usize,
    breaker_until: Option<Instant>,
    last_nudge_at: Option<Instant>,
    /// Last systemic verdict, so entering/leaving is logged once instead of
    /// every tick.
    systemic: bool,
}

impl Brain {
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
    /// one, and so on. Backoff per (tab, label) still applies; the
    /// round-robin just spaces out which one fires when.
    fn tick(&mut self) -> Result<(), String> {
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

        // Gate the entire per-tab scan on "Claude is mid-session here".
        // Without it, brain polls /output on every tab — shell tabs, log
        // tailers, vim sessions — and anything whose scrollback happens to
        // contain a needle (e.g. `git log` showing "ECONNRESET" in a commit
        // message) would get an injected `continue\r`. Only tabs whose hook
        // has reported a Claude session are legitimate targets. Filtering up
        // front is free (no HTTP) and gives the scan its index space.
        let claude: Vec<TabInfo> = tabs
            .tabs
            .into_iter()
            .filter(|t| {
                !t.id.is_empty()
                    && t.agent_kind.as_deref() == Some("claude")
                    && !t.agent_session_id.as_deref().unwrap_or("").is_empty()
            })
            .collect();

        // Drop watch state for tabs that vanished (closed / no longer a Claude
        // session) so the map stays bounded. Built from ALL Claude tabs, not
        // just the ones this tick had budget for — a budget-deferred tab must
        // keep its watch, or its freeze clock restarts every time it's skipped
        // and it can never reach STABLE_SECS.
        self.watches.retain(|id, _| claude.iter().any(|t| &t.id == id));

        let started = Instant::now();
        let now = started;
        let mut eligible: Vec<Eligible> = Vec::new();
        let mut scanned = 0usize;
        for idx in scan_order(claude.len(), self.scan_cursor) {
            let tab = &claude[idx];
            scanned += 1;
            let output = match ag
                .get(format!("{}/tabs/by-id/{}/output", ep.url, tab.id))
                .header("Authorization", &auth)
                .call()
                .map_err(|e| format!("GET output for {}: {e}", tab.id))
                .and_then(|mut r| {
                    r.body_mut()
                        .read_to_string()
                        .map_err(|e| format!("read output for {}: {e}", tab.id))
                }) {
                Ok(o) => o,
                // One tab closing mid-tick must not strand every tab after it.
                Err(e) => {
                    eprintln!("⛑ brain: {e}");
                    continue;
                }
            };

            let watch = self.watches.entry(tab.id.clone()).or_insert_with(|| TabWatch::new(now));
            if let Some(trigger) = evaluate_tab(watch, &output, tab.agent_state.as_deref(), now) {
                eligible.push(Eligible {
                    tab_id: tab.id.clone(),
                    tab_name: tab.name.clone(),
                    trigger,
                    output_hash: watch.last_hash,
                });
            }

            if started.elapsed() >= TICK_BUDGET {
                break;
            }
        }
        self.scan_cursor = self.scan_cursor.wrapping_add(scanned);

        // Fleet capped upstream → drop to a heartbeat. Spaced, never silent:
        // total suppression makes brain look dead and hides the moment the
        // capacity comes back. Evaluated before the empty-set return so a quiet
        // fleet — the actual recovery — is what releases the breaker.
        let systemic = systemic_api_freeze(&eligible, &mut self.breaker_until, now);
        if systemic != self.systemic {
            self.systemic = systemic;
            if systemic {
                println!(
                    "⛑ brain: fleet capped upstream ({n} tab(s) on Anthropic capacity errors) — one nudge per {s}s",
                    n = eligible.len(),
                    s = CIRCUIT_BREAKER_COOLDOWN.as_secs(),
                );
            } else {
                println!("⛑ brain: upstream capacity recovered — normal cadence");
            }
        }

        if eligible.is_empty() {
            return Ok(());
        }

        // Connectivity gate. If the box can't reach the open internet,
        // sending `continue` would just trigger the same error again and
        // burn a backoff step for nothing. Skip the send AND skip updating
        // the backoff / round-robin cursor so the next tick (~5 s)
        // re-probes and fires as soon as the network's back. One probe
        // covers the whole eligible set; the result is cached for
        // `PROBE_TTL` so tabs share it.
        if !self.probe.is_online() {
            println!(
                "⛑ brain: {n} tab(s) flagged but suppressed (no internet — probe failed)",
                n = eligible.len(),
            );
            return Ok(());
        }

        if systemic && !nudge_ready(&mut self.last_nudge_at, now, CIRCUIT_BREAKER_COOLDOWN) {
            return Ok(());
        }

        // Round-robin: pick one from the eligible set. Cursor advances
        // on every successful tick (online + at least one eligible), so
        // the next tick walks past this tab to its neighbours. Single
        // stuck tab → it always wins; multiple → rotation.
        let Some(pick) = pick_round_robin(&eligible, &mut self.rr_cursor) else {
            return Ok(());
        };
        let deferred = eligible.len() - 1;
        // Record the frozen-output hash so this exact screen won't be
        // nudged again until the agent's output changes (work resumed,
        // or it re-stuck on something new). A state guard, not a clock.
        if let Some(w) = self.watches.get_mut(&pick.tab_id) {
            w.nudged_hash = Some(pick.output_hash);
            // Advance the exponential backoff for this error episode: same
            // label → grow the streak (longer wait next time); new label →
            // restart at 1.
            let label = pick.trigger.label();
            w.nudge_streak = if w.last_label == Some(label) {
                w.nudge_streak + 1
            } else {
                1
            };
            w.last_label = Some(label);
            w.next_nudge_at = Some(now + Duration::from_secs(backoff_secs(w.nudge_streak)));
        }

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
}

/// Append a line to `brain-crash.log` in the state dir. Used when a
/// tick panics, so a brain whose terminal/PTY has gone away still
/// leaves a trace — the "crashed for no visible reason" case. All
/// errors swallowed: logging must never be the thing that kills brain.
fn crash_log(msg: &str) {
    use std::io::Write as _;
    let path = crate::platform::state_base_dir()
        .join("tab-atelier")
        .join("brain-crash.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{msg}");
    }
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
                     Watches every Claude tab for known agent-failure signatures and\n\
                     sends `continue\\r` to the matching tab. A tab is nudged only when\n\
                     its screen has been FROZEN for {STABLE_SECS}s (so an actively-working\n\
                     or auto-retrying agent — whose output is still moving — is never\n\
                     interrupted), and only ONCE per frozen screen (re-nudges wait for\n\
                     the output to change first).\n\
                     Patterns: {n} known signatures (Anthropic API connectivity).\n\
                     Connectivity probe (Google generate_204 + Cloudflare 1.1.1.1) gates\n\
                     every send; offline → suppress, retry on next tick when back online.\n\
                     Round-robin: at most one send per tick across all eligible tabs.\n\
                     Repeat nudges for the SAME error back off exponentially, {b}s → {m}s.\n\
                     When more than {t} eligible tabs are stuck on an Anthropic capacity\n\
                     error (529 / 503 / 5xx / rate-limited) the fleet is capped upstream,\n\
                     so sends drop to one every {c}s until it recovers — spaced, never\n\
                     silent. Each tick scans for at most {budget}s and resumes where it\n\
                     stopped, so a large fleet can't outrun the poll interval.",
                    n = PATTERNS.len(),
                    b = NUDGE_BACKOFF_BASE_SECS,
                    m = NUDGE_BACKOFF_MAX_SECS,
                    t = CIRCUIT_BREAKER_THRESHOLD,
                    c = CIRCUIT_BREAKER_COOLDOWN.as_secs(),
                    budget = TICK_BUDGET.as_secs(),
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
        "\u{26d1} brain — watching every {interval}s · {n} patterns · nudge after {STABLE_SECS}s frozen",
        n = PATTERNS.len()
    );

    let mut brain = Brain::default();
    loop {
        // Run the tick under catch_unwind so a panic anywhere in it (a
        // dependency edge case, a broken-pipe `println!`, …) is caught
        // and logged instead of silently killing brain. The &mut state
        // is AssertUnwindSafe: a panic mid-tick may leave the watch map
        // slightly stale, which the next tick re-syncs from the daemon.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| brain.tick()));
        match outcome {
            Ok(Ok(())) => {}
            // Most likely a transient daemon-restart window; next tick succeeds.
            Ok(Err(e)) => eprintln!("⛑ brain: tick failed: {e}"),
            Err(panic) => {
                let msg = panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("(non-string panic payload)");
                // Record to a file (terminal may be gone) + best-effort
                // stderr, then keep watching rather than die.
                crash_log(&format!("tick panicked (recovered): {msg}"));
                let _ = std::io::Write::write_all(
                    &mut std::io::stderr(),
                    format!("⛑ brain: tick PANICKED, recovered: {msg}\n").as_bytes(),
                );
            }
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

    fn pattern_for(label: &str) -> &'static Pattern {
        PATTERNS.iter().find(|p| p.label == label).expect("known label")
    }

    fn eligible_with(i: usize, label: &str) -> Eligible {
        Eligible {
            tab_id: format!("tab-{i}"),
            tab_name: format!("claude-{i}"),
            trigger: Trigger::Pattern(pattern_for(label)),
            output_hash: hash_output(&i.to_string()),
        }
    }

    fn eligible_529(i: usize) -> Eligible {
        eligible_with(i, "anthropic-529")
    }

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
        // Pattern hits and agent_state-driven hits MUST have distinct
        // labels so logs + any future per-label bookkeeping don't
        // conflate them.
        let pattern = &PATTERNS[0];
        let p = Eligible {
            tab_id: "tab-1".into(),
            tab_name: "shell".into(),
            trigger: Trigger::Pattern(pattern),
            output_hash: 0,
        };
        let a = Eligible {
            tab_id: "tab-1".into(),
            tab_name: "shell".into(),
            trigger: Trigger::AgentError,
            output_hash: 0,
        };
        assert_ne!(p.trigger.label(), a.trigger.label());
    }

    #[test]
    fn should_nudge_requires_frozen_long_enough() {
        // Output frozen for less than STABLE_SECS — the agent might
        // still be producing; never nudge.
        assert!(!should_nudge(Duration::from_secs(STABLE_SECS - 1), None, 42));
        // Frozen long enough, never nudged → eligible.
        assert!(should_nudge(Duration::from_secs(STABLE_SECS), None, 42));
        assert!(should_nudge(Duration::from_secs(STABLE_SECS + 100), None, 42));
    }

    #[test]
    fn scan_matches_current_529_wording() {
        // The exact line Claude Code prints today — the parenthesised
        // "Overloaded (529)" needle wouldn't catch it.
        let p = scan_output("● API Error: 529 Overloaded. This is a server-side issue, usually temporary")
            .expect("must match");
        assert_eq!(p.label, "anthropic-529");
    }

    #[test]
    fn scan_matches_connection_closed_mid_response() {
        // Streaming response cut off mid-flight — recover like the other
        // network aborts (cooldown + `continue`).
        let p = scan_output("● API Error: Connection closed mid-response. The response above may be incomplete.")
            .expect("must match");
        assert_eq!(p.label, "connection-closed-mid-response");
        assert_eq!(p.action, "continue\r");
    }

    #[test]
    fn scan_matches_api_retry_waiting_banner() {
        // Claude Code's auto-retry banner. Matches on the stable prefix so
        // the ticking countdown / "check your network" tail don't matter.
        // (A live countdown never freezes STABLE_SECS, so brain only acts on
        // a hung one — the pattern just makes that case recoverable.)
        let p =
            scan_output("✻ Waiting for API response · will retry in 1m 57s · check your network").expect("must match");
        assert_eq!(p.label, "api-retry-waiting");
        assert_eq!(p.action, "continue\r");
    }

    #[test]
    fn scan_retry_banner_does_not_collide_with_rate_limit_wording() {
        // The rate-limit banner says "retrying in", NOT "will retry in", so
        // the new needle must not swallow it (it has its own label/handling).
        let p = scan_output("● Rate limited · retrying in 38s\n❯ continue");
        // No "will retry in" here → the api-retry-waiting needle must miss.
        assert!(p.is_none_or(|p| p.label != "api-retry-waiting"));
    }

    #[test]
    fn backoff_grows_exponentially_then_caps() {
        // 60s, 120s, 240s, 480s, then capped at 900s — so brain stops
        // hammering a stuck agent (e.g. repeated 529 Overloaded) and
        // retries less and less often.
        assert_eq!(backoff_secs(1), 60);
        assert_eq!(backoff_secs(2), 120);
        assert_eq!(backoff_secs(3), 240);
        assert_eq!(backoff_secs(4), 480);
        assert_eq!(backoff_secs(5), 900); // 960 → capped
        assert_eq!(backoff_secs(20), 900); // stays capped, no overflow
        assert_eq!(backoff_secs(0), 60); // saturating, never panics
    }

    #[test]
    fn should_nudge_suppresses_same_frozen_screen() {
        // Already nudged at this exact frozen output → don't spam,
        // even though it's been frozen well past the threshold. This
        // is the fix for the "continue\r sent every 60s into a stuck
        // rate-limit" report.
        assert!(!should_nudge(Duration::from_secs(STABLE_SECS + 600), Some(42), 42));
        // Output changed since the nudge (different hash) → the agent
        // reacted / re-stuck on something new → eligible again.
        assert!(should_nudge(Duration::from_secs(STABLE_SECS), Some(42), 99));
    }

    #[test]
    fn frozen_api_fleet_still_gets_heartbeat_nudges() {
        // 8 tabs frozen on `529 Overloaded`, screens byte-identical every tick
        // — the population the breaker exists for. A time-windowed storm
        // detector deadlocks here: the unchanged screen re-stamps the window
        // every tick, so it can never age out and brain goes silent forever.
        // Systemic mode must throttle to a heartbeat, never to zero.
        let t0 = Instant::now();
        let eligible: Vec<Eligible> = (0..8).map(eligible_529).collect();
        let (mut breaker, mut last_nudge, mut cursor) = (None, None, 0usize);
        let mut sent = 0;
        // 5 simulated minutes at the default 5s tick.
        for tick in 0..60 {
            let now = t0 + Duration::from_secs(tick * DEFAULT_INTERVAL_SECS);
            let systemic = systemic_api_freeze(&eligible, &mut breaker, now);
            assert!(systemic, "8 > threshold stays systemic while nothing recovers");
            if nudge_ready(&mut last_nudge, now, CIRCUIT_BREAKER_COOLDOWN) {
                assert!(pick_round_robin(&eligible, &mut cursor).is_some());
                sent += 1;
            }
        }
        assert!(sent > 0, "SILENT FOREVER — the deadlock this design avoids");
        // ~1 per cooldown over 300s, and never a burst.
        assert!((9..=11).contains(&sent), "heartbeat cadence, got {sent}");
    }

    #[test]
    fn local_mass_freeze_drains_one_per_tick_instead_of_a_burst() {
        // 50 tabs wedge on the same LOCAL fault in the same tick — the herd.
        // There is no explicit fleet-wide simultaneous cap, and the breaker
        // deliberately ignores local faults (they're independent failures, not a
        // shared upstream cap). What bounds the herd is `pick_round_robin` — one
        // send per tick whatever the eligible count — plus `nudged_hash`, which
        // retires each frozen screen after its single nudge. Neither TICK_BUDGET
        // (it bounds the SCAN, not the sends) nor `backoff_secs` (it only bites
        // from a tab's SECOND nudge, which a frozen screen never reaches) is in
        // this path. Offline is covered separately by the connectivity probe.
        // The guarantee is SPACING, not volume: 50 nudges over 50 ticks. #40.
        let t0 = Instant::now();
        let frozen = "⎿  API Error: Unable to connect to API (ConnectionRefused)\n❯ continue";
        let fleet = 50usize;
        let mut watches: Vec<TabWatch> = (0..fleet).map(|_| TabWatch::new(t0)).collect();
        let (mut breaker, mut cursor) = (None, 0usize);
        let mut nudges = vec![0u32; fleet];
        let mut nudge_ticks: Vec<u64> = Vec::new();
        let mut peak_eligible = 0usize;

        for tick in 0..120 {
            let now = t0 + Duration::from_secs(tick * DEFAULT_INTERVAL_SECS);
            let eligible: Vec<Eligible> = watches
                .iter_mut()
                .enumerate()
                .filter_map(|(i, w)| {
                    evaluate_tab(w, frozen, None, now).map(|trigger| Eligible {
                        tab_id: format!("tab-{i}"),
                        tab_name: format!("claude-{i}"),
                        trigger,
                        output_hash: w.last_hash,
                    })
                })
                .collect();
            peak_eligible = peak_eligible.max(eligible.len());
            assert!(
                !systemic_api_freeze(&eligible, &mut breaker, now),
                "a local mass freeze must never read as an upstream cap"
            );

            let Some(pick) = pick_round_robin(&eligible, &mut cursor) else {
                continue;
            };
            let i: usize = pick.tab_id.strip_prefix("tab-").unwrap().parse().unwrap();
            nudges[i] += 1;
            nudge_ticks.push(tick);
            // Exactly what tick() records on send — including the backoff state,
            // to show it isn't what bounds the drain.
            let w = &mut watches[i];
            w.nudged_hash = Some(w.last_hash);
            w.nudge_streak = 1;
            w.last_label = Some(pick.trigger.label());
            w.next_nudge_at = Some(now + Duration::from_secs(backoff_secs(1)));
        }

        assert_eq!(peak_eligible, fleet, "the whole fleet really did freeze at once");
        assert!(
            nudges.iter().all(|n| *n == 1),
            "every tab nudged exactly once — nudged_hash retires a frozen screen"
        );
        // One per tick, no gaps: the herd is spread across `fleet` ticks
        // (~250s at the default interval) rather than fired in one burst.
        assert_eq!(nudge_ticks.len(), fleet);
        let (first, last) = (nudge_ticks[0], nudge_ticks[fleet - 1]);
        assert_eq!(last - first, (fleet - 1) as u64, "one nudge per tick, never two");
        // Nothing before the freeze gate opens at STABLE_SECS.
        assert_eq!(first * DEFAULT_INTERVAL_SECS, STABLE_SECS);
    }

    #[test]
    fn systemic_freeze_needs_more_than_threshold_and_is_sticky() {
        let t0 = Instant::now();
        let mut breaker = None;
        // Below the threshold → not systemic, nothing armed.
        let few: Vec<Eligible> = (0..2).map(eligible_529).collect();
        assert!(!systemic_api_freeze(&few, &mut breaker, t0));
        assert!(breaker.is_none());
        // AT the threshold → still not systemic (strictly more is required).
        let at: Vec<Eligible> = (0..CIRCUIT_BREAKER_THRESHOLD).map(eligible_529).collect();
        assert!(!systemic_api_freeze(&at, &mut breaker, t0));
        assert!(breaker.is_none());
        // Above → systemic, cooldown armed.
        let many: Vec<Eligible> = (0..=CIRCUIT_BREAKER_THRESHOLD).map(eligible_529).collect();
        assert!(systemic_api_freeze(&many, &mut breaker, t0));
        assert!(breaker.is_some());
        // Sticky through the cooldown even as the count collapses, so a spike
        // doesn't flap the cadence.
        assert!(systemic_api_freeze(&few, &mut breaker, t0 + Duration::from_secs(5)));
        assert!(systemic_api_freeze(
            &[],
            &mut breaker,
            (t0 + CIRCUIT_BREAKER_COOLDOWN)
                .checked_sub(Duration::from_secs(1))
                .unwrap()
        ));
        // Elapsed → clears itself.
        assert!(!systemic_api_freeze(&few, &mut breaker, t0 + CIRCUIT_BREAKER_COOLDOWN));
        assert!(breaker.is_none());
        // Still over after clearing → re-arms.
        assert!(systemic_api_freeze(&many, &mut breaker, t0 + CIRCUIT_BREAKER_COOLDOWN));
    }

    #[test]
    fn only_anthropic_capacity_errors_trip_the_breaker() {
        let t0 = Instant::now();
        let mut breaker = None;
        // 8 tabs all on a LOCAL fault — independent failures, not a fleet-wide
        // cap. Throttling them would be counting eligible tabs, not storms.
        let local: Vec<Eligible> = (0..8).map(|i| eligible_with(i, "connection-refused")).collect();
        assert!(!systemic_api_freeze(&local, &mut breaker, t0));
        // Mixed 3 API + 5 local → only 3 count, still below threshold.
        let mixed: Vec<Eligible> = (0..3)
            .map(eligible_529)
            .chain((3..8).map(|i| eligible_with(i, "tcp-reset")))
            .collect();
        assert!(!systemic_api_freeze(&mixed, &mut breaker, t0));
        assert!(breaker.is_none());
    }

    #[test]
    fn storm_labels_cover_capacity_errors_but_not_the_healthy_retry_banner() {
        assert!(is_api_storm_label("anthropic-529"));
        assert!(is_api_storm_label("anthropic-rate-limited"));
        assert!(is_api_storm_label("anthropic-503"));
        assert!(is_api_storm_label("anthropic-5xx"));
        // "will retry in" is Claude Code's own healthy self-retry banner (see
        // the api-retry-waiting Pattern above): a live countdown means the tab
        // is RECOVERING. Counting it would let five recovering tabs throttle
        // the whole fleet.
        assert!(!is_api_storm_label("api-retry-waiting"));
        assert!(!is_api_storm_label("connection-refused"));
        assert!(!is_api_storm_label("agent-state-error"));
    }

    #[test]
    fn recovery_resumes_normal_cadence() {
        // The self-clearing property: once the tabs recover they leave
        // `eligible`, so the count drops on its own and the heartbeat gate is
        // out of the send path entirely.
        let t0 = Instant::now();
        let mut breaker = None;
        let many: Vec<Eligible> = (0..8).map(eligible_529).collect();
        assert!(systemic_api_freeze(&many, &mut breaker, t0));
        assert!(!systemic_api_freeze(&[], &mut breaker, t0 + CIRCUIT_BREAKER_COOLDOWN));
        assert!(breaker.is_none());
    }

    #[test]
    fn heartbeat_gate_spaces_nudges_without_silencing_them() {
        let t0 = Instant::now();
        let mut last = None;
        // First nudge of an episode fires immediately.
        assert!(nudge_ready(&mut last, t0, CIRCUIT_BREAKER_COOLDOWN));
        // Next tick, well inside the window → refused.
        assert!(!nudge_ready(
            &mut last,
            t0 + Duration::from_secs(5),
            CIRCUIT_BREAKER_COOLDOWN
        ));
        // Window elapsed → fires again. Exactly one per cooldown, never zero.
        assert!(nudge_ready(
            &mut last,
            t0 + CIRCUIT_BREAKER_COOLDOWN,
            CIRCUIT_BREAKER_COOLDOWN
        ));
    }

    #[test]
    fn evaluate_tab_nudges_a_frozen_screen_once_then_suppresses_it() {
        let t0 = Instant::now();
        let mut watch = TabWatch::new(t0);
        let frozen = "● API Error: 529 Overloaded. This is a server-side issue\n❯ continue";
        let mut nudges = 0;
        for tick in 0..60 {
            let now = t0 + Duration::from_secs(tick * DEFAULT_INTERVAL_SECS);
            if let Some(t) = evaluate_tab(&mut watch, frozen, None, now) {
                assert_eq!(t.label(), "anthropic-529");
                nudges += 1;
                // What tick() records on send.
                watch.nudged_hash = Some(watch.last_hash);
            }
        }
        // Recovery needs the OUTPUT to change — which is why a fleet-wide time
        // window can't be the release condition.
        assert_eq!(nudges, 1, "one nudge per frozen screen, then silence");
    }

    #[test]
    fn evaluate_tab_never_nudges_moving_output() {
        // A LIVE auto-retry countdown: matches a pattern every tick, but each
        // change resets the freeze clock, so it's never eligible.
        let t0 = Instant::now();
        let mut watch = TabWatch::new(t0);
        for tick in 0..60 {
            let now = t0 + Duration::from_secs(tick * DEFAULT_INTERVAL_SECS);
            let out = format!(
                "✻ Waiting for API response · will retry in 1m {}s · check your network",
                59 - tick
            );
            assert!(
                evaluate_tab(&mut watch, &out, None, now).is_none(),
                "moving screen at tick {tick}"
            );
        }
    }

    #[test]
    fn evaluate_tab_recovery_clears_backoff_state() {
        let t0 = Instant::now();
        let mut watch = TabWatch::new(t0);
        watch.nudge_streak = 3;
        watch.last_label = Some("anthropic-529");
        watch.next_nudge_at = Some(t0 + Duration::from_mins(4));
        assert!(evaluate_tab(&mut watch, "$ ls\nfoo bar\n$ ", None, t0).is_none());
        assert_eq!(watch.nudge_streak, 0);
        assert_eq!(watch.last_label, None);
        assert_eq!(watch.next_nudge_at, None);
    }

    #[test]
    fn budget_deferred_ticks_do_not_reset_the_freeze_clock() {
        // TICK_BUDGET truncation means a tab can be skipped for several ticks.
        // Its watch must survive untouched, or the freeze clock restarts every
        // time and the tab can never reach STABLE_SECS.
        let t0 = Instant::now();
        let mut watch = TabWatch::new(t0);
        let frozen = "⎿  API Error: Unable to connect to API (ConnectionRefused)\n❯ continue";
        assert!(evaluate_tab(&mut watch, frozen, None, t0).is_none());
        let started_at = watch.stable_since;
        // Skipped for 6 ticks, then scanned again with the same screen.
        let later = t0 + Duration::from_secs(30);
        assert!(evaluate_tab(&mut watch, frozen, None, later).is_some());
        assert_eq!(watch.stable_since, started_at, "freeze clock untouched by deferral");
    }

    #[test]
    fn scan_order_rotates_and_wraps_for_resumable_scans() {
        assert!(scan_order(0, 3).is_empty());
        assert_eq!(scan_order(3, 0), vec![0, 1, 2]);
        assert_eq!(scan_order(3, 1), vec![1, 2, 0]);
        assert_eq!(scan_order(3, 4), vec![1, 2, 0]);
        // usize::MAX % 3 == 0 — must not panic on the wrapped cursor.
        assert_eq!(scan_order(3, usize::MAX), vec![0, 1, 2]);
    }

    #[test]
    fn truncated_scans_still_cover_the_whole_fleet() {
        // 10 tabs, budget only allows 3 per tick → every tab must be visited
        // within a few ticks. This is the fairness claim the cursor makes.
        let mut cursor = 0usize;
        let mut visited = [false; 10];
        for _ in 0..4 {
            let scanned = 3;
            for idx in scan_order(10, cursor).into_iter().take(scanned) {
                visited[idx] = true;
            }
            cursor = cursor.wrapping_add(scanned);
        }
        assert!(visited.iter().all(|v| *v), "every tab scanned within 4 ticks");
    }

    #[test]
    fn a_budget_deferred_tab_keeps_its_watch() {
        // seen_ids covers ALL Claude tabs, not just the scanned ones, so the
        // retain that drops closed tabs doesn't wipe deferred ones.
        let t0 = Instant::now();
        let all: Vec<String> = (0..10).map(|i| format!("tab-{i}")).collect();
        let mut watches: HashMap<String, TabWatch> = all.iter().map(|id| (id.clone(), TabWatch::new(t0))).collect();
        // Only the first 3 were scanned this tick; the retain is built from all 10.
        watches.retain(|id, _| all.iter().any(|s| s == id));
        assert_eq!(watches.len(), 10);
        // A tab that really went away is still dropped.
        let survivors: Vec<String> = all[..9].to_vec();
        watches.retain(|id, _| survivors.iter().any(|s| s == id));
        assert_eq!(watches.len(), 9);
    }

    #[test]
    fn hash_output_is_stable_and_sensitive() {
        // Same input → same hash (stability across ticks).
        assert_eq!(hash_output("retrying in 38s"), hash_output("retrying in 38s"));
        // A one-second countdown tick changes the hash → resets the
        // stability clock → an auto-retrying agent is never nudged.
        assert_ne!(hash_output("retrying in 38s"), hash_output("retrying in 37s"));
    }
}
