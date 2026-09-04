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
/// Auth-breaker back-off base/ceiling. On a 401/403 from the daemon API the
/// token is bad/expired — every call fails identically until it's fixed, so
/// brain stops nudging (and stops hammering the API) instead of re-nudging every
/// tick. Doubles per consecutive auth failure, resets on the next authorised
/// call. Distinct from the count-based storm breaker (which keys on fleet size).
const AUTH_BACKOFF_BASE_SECS: u64 = 60;
const AUTH_BACKOFF_MAX_SECS: u64 = 900;

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
    /// Inc9 hot-swap cross-guard: true while this tab is mid-adoption in a binary
    /// hot-swap handoff (from `/tabs` `inHandoff`). Brain must NOT nudge it — a
    /// `continue` racing the handoff could double-launch the agent.
    #[serde(default, rename = "inHandoff")]
    in_handoff: bool,
    /// crc32 of the tab's 200-line `/output` grid, carried on the `/tabs` list
    /// (the daemon's persist-tick dirtiness key). Brain's event-driven signal:
    /// unchanged since the last poll ⇒ the screen is byte-identical ⇒ skip the
    /// per-tab `/output` fetch+scan (S2). `#[serde(default)]` so an OLDER daemon
    /// that predates the field deserializes to 0 — S2 treats 0 as "unknown, scan
    /// it" (crc32 of a real non-empty screen is never 0), so a version-skew
    /// window degrades safely to the old poll-everything behaviour, never to
    /// silently skipping a frozen tab.
    #[serde(default)]
    output_crc: u32,
}

/// Is this tab a legitimate brain target? A live Claude session (kind + session)
/// that is NOT mid-hot-swap-handoff. Pure, so the "leave an adopted tab alone"
/// guard (Inc9) is unit-testable alongside the existing claude-only gate.
fn is_watchable(tab: &TabInfo) -> bool {
    !tab.id.is_empty()
        && tab.agent_kind.as_deref() == Some("claude")
        && !tab.agent_session_id.as_deref().unwrap_or("").is_empty()
        && !tab.in_handoff
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
    /// Stability fingerprint of the frozen screen that made this tab eligible
    /// (the wire crc, or a hash of `/output` in the crc==0 fallback). Recorded
    /// into the tab's [`TabWatch::nudged_fp`] on send so brain won't re-nudge the
    /// SAME frozen screen — only once the output changes (agent reacted, or
    /// re-stuck on something new) does the tab become eligible again.
    output_fp: u64,
}

/// Per-tab watch state, keyed by tab id. Tracks output stability so
/// brain can tell "frozen and stuck" from "actively working".
struct TabWatch {
    /// crc32 of the tab's `/output` grid as last carried on the `/tabs` list —
    /// the SKIP-FETCH key. Unchanged since our last poll (and non-zero) ⇒ the
    /// screen is byte-identical ⇒ brain does NOT re-fetch/re-scan this tab's
    /// `/output`. This is the event-driven signal: fetch cost ∝ output changes,
    /// not #tabs × every tick.
    last_crc: u32,
    /// Stability fingerprint driving the freeze clock: the wire crc when the
    /// daemon supplies one (`crc != 0`), else a hash of the fetched `/output`
    /// (the `crc == 0` fallback — an older daemon without the field, or a
    /// legitimately empty screen — so brain behaves exactly like the pre-event
    /// poll-and-hash path). `None` until the first fetch.
    last_fp: Option<u64>,
    /// When the fingerprint last changed — the tab's "last activity" instant.
    /// Brian's dead-man's-switch: `now - stable_since >= STABLE_SECS` ⇒ frozen
    /// (a frozen tab emits no more dirtiness events, so its ABSENCE of change is
    /// the freeze signal). Seeded to `now` on first sight (T4: no burst of
    /// false-gels at hot-swap boot).
    stable_since: Instant,
    /// T1 CACHE — the last needle [`scan_output`] found for this tab, captured on
    /// the last fetch (a crc-change tick). A frozen tab is NOT re-fetched, but its
    /// freeze-nudge eligibility needs the error that's stuck on the (byte-
    /// identical) frozen screen; the cache stands in for a fresh scan, which would
    /// return the same thing since the screen has not changed. `None` = no needle
    /// on the last-seen screen.
    cached_needle: Option<&'static Pattern>,
    /// Stability fingerprint at the moment we last sent `continue` to this tab,
    /// or `None` if we've never nudged it (or its output changed since).
    /// Guards against re-nudging an unchanged frozen screen.
    nudged_fp: Option<u64>,
    /// Consecutive nudges for the current unresolved error episode —
    /// drives the exponential backoff. Reset to 0 when the tab recovers
    /// (no error trigger) or hits a different error.
    nudge_streak: u32,
    /// Earliest time the next nudge is allowed (the backoff gate).
    next_nudge_at: Option<Instant>,
    /// Error label of the last nudge; a different label resets the streak.
    last_label: Option<&'static str>,
    /// When this session last showed an Anthropic-side API-storm error
    /// ([`is_api_storm_label`]). Feeds the level-(b) storm detector
    /// ([`api_error_sessions`]); `None` = never (or aged past the window).
    last_api_error_at: Option<Instant>,
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

/// One bounded `GET /tabs/by-id/<id>/output`. Returns the scrollback, or `None`
/// on a transient error (tab closed mid-tick, …) — the caller leaves that tab for
/// the next tick rather than aborting the whole sweep. Isolated so the tick loop's
/// only HTTP for a per-tab scan is injectable (see [`freeze_step`]).
fn fetch_output(ag: &ureq::Agent, ep: &Endpoint, auth: &str, tab_id: &str) -> Option<String> {
    match ag
        .get(format!("{}/tabs/by-id/{}/output", ep.url, tab_id))
        .header("Authorization", auth)
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
    {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("⛑ brain: GET output for {tab_id} failed: {e} — skipping this tab this tick");
            None
        }
    }
}

/// Result of one tab's [`freeze_step`]. All variants are cheap Copy payloads.
#[derive(Clone, Copy)]
enum FreezeOutcome {
    /// The `/output` fetch failed transiently — the caller leaves the tab's watch
    /// state untouched and moves on (it still counts as scanned).
    FetchFailed,
    /// Freeze assessed: how long the screen has been frozen, and the active error
    /// trigger this tick (`None` = nothing wrong / recovered).
    Assessed {
        stable_for: Duration,
        trigger: Option<Trigger>,
    },
}

/// Event-driven per-tab freeze bookkeeping — the core of Option C, extracted PURE
/// (HTTP injected via `fetch`) so the golden-set equivalence (T1), false-positive
/// (T2), storm (T3) and boot-seed (T4) checks drive it without a daemon.
///
/// `fetch` is invoked ONLY when the wire `crc` says the screen changed since the
/// last poll — `crc != last_crc`, or `crc == 0` = "unknown" (an older daemon
/// without the field, or a legitimately empty screen) → always fetch. That skip is
/// the whole CPU win: a frozen tab (crc stuck) is fetched ZERO times after it
/// froze. On a real change brain re-scans and CACHES the needle; on an unchanged
/// crc it reuses the cache — equivalent, because the screen is byte-identical.
///
/// Mutates `watch` exactly as the tick loop needs: `last_crc` (skip key),
/// `last_fp` (stability fingerprint: the crc, or a hash of `/output` in the crc==0
/// fallback so this path matches the pre-event poll-and-hash behaviour),
/// `stable_since` (freeze clock — reset when the fingerprint changes), and
/// `cached_needle` (T1). A first-seen tab (`last_fp == None`) always fetches.
fn freeze_step<F: FnOnce() -> Option<String>>(
    watch: &mut TabWatch,
    crc: u32,
    agent_error: bool,
    now: Instant,
    fetch: F,
) -> FreezeOutcome {
    // Skip only a tab we've fetched before whose real (non-zero) crc is unchanged.
    let skip_fetch = crc != 0 && watch.last_fp.is_some() && watch.last_crc == crc;
    if !skip_fetch {
        let Some(output) = fetch() else {
            return FreezeOutcome::FetchFailed;
        };
        watch.last_crc = crc;
        let fp = if crc != 0 { u64::from(crc) } else { hash_output(&output) };
        if watch.last_fp != Some(fp) {
            watch.last_fp = Some(fp);
            watch.stable_since = now;
        }
        watch.cached_needle = scan_output(&output);
    }
    let stable_for = now.duration_since(watch.stable_since);
    // Cached needle (== a fresh scan of the byte-identical screen) OR the
    // agent_state=="error" flag, which rides the /tabs list so it needs no fetch.
    // Pattern wins on tie (its label is more specific).
    let trigger = watch
        .cached_needle
        .map(Trigger::Pattern)
        .or_else(|| agent_error.then_some(Trigger::AgentError));
    FreezeOutcome::Assessed { stable_for, trigger }
}

/// Global minimum spacing between nudges — anti thundering-herd / anti-runaway.
///
/// Per-tab backoff + the round-robin already space *which* tab fires, but after
/// a restart / an API cap dozens of tabs go frozen at once and were nudged one
/// per fast tick — ~50 `continue`s in 1–2 min re-caps the Claude API, re-freezes
/// everything, and spirals (observed twice). This caps the FLEET-WIDE nudge rate
/// at one every `NUDGE_MIN_INTERVAL`, independent of the tick interval, so 50
/// frozen tabs get unstuck one at a time (2 s apart), never in a burst.
const NUDGE_MIN_INTERVAL: Duration = Duration::from_secs(2);

/// Global throttle gate: may a nudge fire `now`? Yes when none has fired within
/// `min_interval` — and then `now` is recorded as the last emission so the next
/// is spaced out. `None` (never nudged) is always allowed. `min_interval` is
/// [`NUDGE_MIN_INTERVAL`] normally and the longer [`nudge_interval`] heartbeat
/// under a systemic breaker. Pure + deterministic (injected `now`), so the
/// anti-storm spacing is unit-testable.
fn nudge_ready(last_nudge_at: &mut Option<Instant>, now: Instant, min_interval: Duration) -> bool {
    if let Some(t) = *last_nudge_at
        && now.duration_since(t) < min_interval
    {
        return false;
    }
    *last_nudge_at = Some(now);
    true
}

/// AXE B — fleet-wide nudge spacing for this tick.
///
/// Normal: [`NUDGE_MIN_INTERVAL`]. Under a **systemic** breaker (a likely API
/// cap): the SINGLE round-robin nudge is spaced to a slow heartbeat /
/// recovery-probe ([`CIRCUIT_BREAKER_COOLDOWN`] apart) — deliberately NOT silence
/// (total suppression made brain look dead and hid recovery) and never a storm
/// (still ≤1 nudge, just spaced). One nudge keeps brain visibly alive and
/// auto-detects the moment the cap clears. Pure, so the policy is unit-testable.
const fn nudge_interval(systemic: bool) -> Duration {
    if systemic {
        CIRCUIT_BREAKER_COOLDOWN
    } else {
        NUDGE_MIN_INTERVAL
    }
}

/// Circuit-breaker threshold: MORE than this many tabs frozen at the same time
/// is treated as SYSTEMIC (a likely Claude API cap, not N independent failures).
/// Count-based on purpose — no fragile screen-parsing / message-wording
/// heuristics ("network-blocked vs rate-limited" is deliberately out of scope).
const CIRCUIT_BREAKER_THRESHOLD: usize = 6;
/// How long Brain stops nudging once the breaker trips. Nudging a fleet that's
/// stuck on a cap only deepens the cap; backing off lets it clear.
const CIRCUIT_BREAKER_COOLDOWN: Duration = Duration::from_secs(30);

/// Count-based circuit breaker. Returns `true` (suppress all nudges this tick)
/// when Brain is either mid-cooldown OR the simultaneous-freeze count just
/// tripped the breaker — which arms the cooldown. `false` = normal operation.
///
/// This kills a cap's amplification at the root: instead of firing `continue`
/// at 50 tabs stuck on the same API limit (feeding the limit), Brain pauses for
/// [`CIRCUIT_BREAKER_COOLDOWN`]. Pure + deterministic (injected `now`) → testable.
fn circuit_breaker_open(breaker_until: &mut Option<Instant>, eligible_count: usize, now: Instant) -> bool {
    if let Some(until) = *breaker_until {
        if now < until {
            return true; // still cooling down
        }
        *breaker_until = None; // cooldown elapsed — re-evaluate below
    }
    if eligible_count > CIRCUIT_BREAKER_THRESHOLD {
        *breaker_until = Some(now + CIRCUIT_BREAKER_COOLDOWN);
        return true;
    }
    false
}

/// Level-(b) API-STORM threshold (rate-limit finding #5): this many DISTINCT
/// sessions showing an Anthropic-side API error within [`STORM_ERROR_WINDOW`] is a
/// systemic storm — the shared opus token-bucket is saturating.
const STORM_ERROR_THRESHOLD: usize = 5;
/// Sliding window for the storm count. Its own slide IS the "until calm" detector:
/// no separate cooldown — once the errors age out, the freeze lifts by itself.
const STORM_ERROR_WINDOW: Duration = Duration::from_mins(1);

/// Is `label` an Anthropic-side API-capacity error — the storm signature (the
/// shared opus bucket saturating: 429/overloaded/rate-limited/5xx, or an active
/// retry banner)? Distinct from LOCAL network faults (connection-refused /
/// econnreset / etimedout), which are per-box, not an org-wide storm. Pure.
fn is_api_storm_label(label: &str) -> bool {
    matches!(
        label,
        "anthropic-529" | "anthropic-rate-limited" | "anthropic-503" | "anthropic-5xx" | "api-retry-waiting"
    )
}

/// Count DISTINCT sessions whose last API-storm error was within
/// [`STORM_ERROR_WINDOW`] of `now`. Pure over the watch map + injected `now`, so
/// the sliding window is unit-testable.
fn api_error_sessions(watches: &HashMap<String, TabWatch>, now: Instant) -> usize {
    watches
        .values()
        .filter(|w| matches!(w.last_api_error_at, Some(t) if now.saturating_duration_since(t) < STORM_ERROR_WINDOW))
        .count()
}

/// Level-(b) decision: FULL FREEZE — send NOTHING this tick, not even the
/// level-(a) heartbeat — when the distinct API-error session count reaches the
/// storm threshold. A `continue` during a 429 plateau only re-feeds the herd
/// that's backing off. Pure; the window slide (see [`api_error_sessions`]) lifts
/// the freeze once the agents recover, so no cooldown is needed.
const fn storm_freeze(api_error_sessions: usize) -> bool {
    api_error_sessions >= STORM_ERROR_THRESHOLD
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

/// Seconds brain suppresses nudges after `streak` consecutive auth failures:
/// `AUTH_BACKOFF_BASE_SECS * 2^(streak-1)`, capped. So 60s, 120s, 240s, 480s,
/// 900s, 900s… Mirrors [`backoff_secs`] but keyed on the auth episode.
fn auth_backoff_secs(streak: u32) -> u64 {
    let shift = streak.saturating_sub(1).min(6);
    AUTH_BACKOFF_BASE_SECS
        .saturating_mul(1u64 << shift)
        .min(AUTH_BACKOFF_MAX_SECS)
}

/// A hard auth rejection (401/403): the daemon token is bad/expired, so retrying
/// won't help until it's restored — unlike a transient network error (5xx/429),
/// which is worth another tick. Keeps the [`AuthBreaker`] from tripping on those.
fn is_auth_error(e: &ureq::Error) -> bool {
    matches!(e, ureq::Error::StatusCode(401 | 403))
}

/// Auth circuit-breaker, threaded across ticks like `breaker_until`. While open,
/// brain skips the whole tick — no fetch, no nudge — because a rejected token
/// makes every call futile (the "brain kept nudging through a 401" bug). Trips
/// on an auth error, backs off exponentially, and clears the moment an
/// authorised call succeeds.
#[derive(Default)]
struct AuthBreaker {
    /// While `Some(t)` and `now < t`, the breaker is open.
    until: Option<Instant>,
    /// Consecutive auth failures — drives the exponential back-off.
    streak: u32,
}

impl AuthBreaker {
    /// Record an auth failure `now`: grow the streak, arm the back-off, log once.
    fn trip(&mut self, now: Instant) {
        self.streak = self.streak.saturating_add(1);
        let wait = auth_backoff_secs(self.streak);
        let streak = self.streak;
        self.until = Some(now + Duration::from_secs(wait));
        eprintln!(
            "⛑ brain: daemon auth rejected (401/403) — token bad/expired; \
             suppressing all nudges for {wait}s (streak {streak}). No nudge can \
             land until auth is restored."
        );
    }

    /// Called after any authorised call succeeds: clear the breaker (recovered).
    fn clear(&mut self) {
        if self.until.is_some() || self.streak > 0 {
            eprintln!("⛑ brain: daemon auth restored — resuming nudges.");
        }
        self.until = None;
        self.streak = 0;
    }

    /// True while the back-off window is open (caller skips the tick).
    fn is_open(&self, now: Instant) -> bool {
        self.until.is_some_and(|t| now < t)
    }
}

/// AXE A — wall-clock budget for one tick's per-tab `/output` scan.
///
/// The scan does one bounded (`share_link` 3 s timeout) GET per Claude tab,
/// sequentially. Under a storm the daemon crawls, so N tabs times up-to-3 s is a
/// multi-minute tick the PO perceived as a freeze. Past this budget the scan
/// stops and resumes across the fleet next tick (see [`scan_order`] plus
/// `scan_cursor`), so a tick is bounded INDEPENDENTLY of the tab count: even 50
/// tabs on a slow daemon still return in about the budget. Lowering the per-GET
/// timeout was rejected (tichef): it only moves the threshold. Worst-case block
/// is roughly the budget plus one already-in-flight GET (≤ the 3 s timeout).
const TICK_BUDGET: Duration = Duration::from_secs(6);

/// The order to scan `n` tabs this tick: start at `cursor`, wrap around. So when
/// the [`TICK_BUDGET`] truncates a scan, the next tick resumes on the tabs it
/// didn't reach instead of always re-scanning the head — fair coverage across
/// the fleet. Pure, so the wrap/rotation is unit-testable. Empty when `n == 0`.
fn scan_order(n: usize, cursor: usize) -> impl Iterator<Item = usize> {
    // Reduce the cursor to a start index ONCE (`cursor % n`), then rotate — so
    // `(start + off) % n` is a true permutation of `0..n` for any cursor,
    // including one near usize::MAX (a per-request `wrapping_add(off) % n` would
    // wrap mid-sequence and repeat/skip indices).
    let start = if n == 0 { 0 } else { cursor % n };
    (0..n).map(move |off| (start + off) % n.max(1))
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
    watches: &mut HashMap<String, TabWatch>,
    probe: &mut ConnectivityProbe,
    cursor: &mut usize,
    last_nudge_at: &mut Option<Instant>,
    breaker_until: &mut Option<Instant>,
    scan_cursor: &mut usize,
    auth_breaker: &mut AuthBreaker,
) -> Result<(), String> {
    // Auth breaker: while a 401/403 stands, skip the tick entirely — a rejected
    // token makes the fetch and every nudge futile until it's restored.
    if auth_breaker.is_open(Instant::now()) {
        return Ok(());
    }
    let ep: Endpoint = discover_endpoint()?;
    let ag = agent();
    let auth = format!("Bearer {}", ep.token);

    let mut resp = match ag
        .get(format!("{}/tabs", ep.url))
        .header("Authorization", &auth)
        .call()
    {
        Ok(r) => r,
        Err(e) if is_auth_error(&e) => {
            auth_breaker.trip(Instant::now());
            return Ok(());
        }
        Err(e) => return Err(format!("GET /tabs: {e}")),
    };
    let tabs: TabsResponse = resp
        .body_mut()
        .read_json()
        .map_err(|e| format!("parse /tabs: {e}"))?;
    // The GET was authorised → any prior auth episode is over.
    auth_breaker.clear();

    let now = Instant::now();
    let mut eligible: Vec<Eligible> = Vec::new();

    // Gate the whole per-tab scan on "Claude is mid-session here". Without it,
    // brain polled /output on EVERY tab — shell tabs, log tailers, vim sessions —
    // and anything whose scrollback happened to contain a needle (e.g. `git log`
    // showing "ECONNRESET" in a commit message) would get an injected `continue`.
    // Only tabs whose hook has reported a Claude session are legitimate targets.
    // Filtered up front (cheap, no HTTP) so the budget below governs only the
    // expensive /output GETs.
    let claude_tabs: Vec<TabInfo> = tabs.tabs.into_iter().filter(is_watchable).collect();
    // `seen_ids` = ALL claude tabs (not just the ones scanned this tick) so a
    // budget-deferred tab keeps its watch state instead of having its freeze
    // clock reset every time the scan can't reach it.
    let seen_ids: Vec<String> = claude_tabs.iter().map(|t| t.id.clone()).collect();

    // AXE A — bounded, resumable scan. Walk the claude tabs from `scan_cursor`
    // (round-robin order), STOP once TICK_BUDGET is spent, and resume next tick
    // where we left off. This caps a tick's wall-clock independently of the tab
    // count, killing the "brain frozen for minutes under a storm" symptom.
    let n = claude_tabs.len();
    let tick_start = Instant::now();
    let mut scanned = 0usize;
    for i in scan_order(n, *scan_cursor) {
        if tick_start.elapsed() >= TICK_BUDGET {
            println!(
                "⛑ brain: tick budget {b}s reached after {scanned}/{n} tab(s) — deferring {rest} to next tick",
                b = TICK_BUDGET.as_secs(),
                rest = n - scanned,
            );
            break;
        }
        let tab = &claude_tabs[i];
        scanned += 1;
        let crc = tab.output_crc;
        let agent_error = tab.agent_state.as_deref() == Some("error");

        let watch = watches.entry(tab.id.clone()).or_insert_with(|| TabWatch {
            last_crc: crc,
            last_fp: None,
            // T4 boot-seed: a first-seen tab is treated as "just active" (stable_since
            // = now), so a tab ALREADY frozen when brain starts / hot-swaps in still
            // gets a full STABLE_SECS grace instead of an instant false-gel.
            stable_since: now,
            cached_needle: None,
            nudged_fp: None,
            nudge_streak: 0,
            next_nudge_at: None,
            last_label: None,
            last_api_error_at: None,
        });

        // Event-driven freeze bookkeeping (Option C core). `freeze_step` fetches
        // /output ONLY when the wire crc says the screen changed — that skip is the
        // whole CPU win — and reuses the cached needle otherwise. A transient fetch
        // error leaves the tab untouched for next tick; it still counts as scanned
        // so the cursor advances past it (a daemon-wide outage already failed the
        // /tabs GET above, before we got here).
        let (stable_for, trigger) =
            match freeze_step(watch, crc, agent_error, now, || fetch_output(&ag, &ep, &auth, &tab.id)) {
                FreezeOutcome::FetchFailed => continue,
                FreezeOutcome::Assessed { stable_for, trigger } => (stable_for, trigger),
            };
        let fp = watch.last_fp.unwrap_or(0);

        let Some(trigger) = trigger else {
            // No error on screen → recovered (or never errored). Clear
            // the backoff so the next episode starts fresh.
            watch.nudge_streak = 0;
            watch.next_nudge_at = None;
            watch.last_label = None;
            continue;
        };
        let label = trigger.label();

        // Level-(b) storm signal: stamp this session's last API-storm error time
        // for EVERY api-erroring tab (frozen or not — a live 429/retry screen isn't
        // frozen yet but still counts toward the storm). Runs off `trigger` (cache-
        // backed) each tick, so a tab frozen on a 529 keeps refreshing its storm
        // timestamp exactly as the pre-event poll did — no storm-detector drift.
        if matches!(trigger, Trigger::Pattern(_)) && is_api_storm_label(label) {
            watch.last_api_error_at = Some(now);
        }

        // Frozen long enough AND not already nudged at this exact screen?
        // (An active auto-retry countdown keeps the screen moving, so it
        // never freezes STABLE_SECS and never reaches here.)
        if !should_nudge(stable_for, watch.nudged_fp, fp) {
            continue;
        }

        // Exponential backoff: while the SAME error keeps recurring, wait
        // progressively longer between nudges so brain doesn't hammer a
        // transient outage (repeated `529 Overloaded`, rate limits). A
        // different error label resets the streak (applied on send).
        if watch.last_label == Some(label)
            && let Some(at) = watch.next_nudge_at
            && now < at
        {
            let wait = at.duration_since(now).as_secs();
            println!(
                "⛑ brain: {name:<24} [{label}] backing off — next nudge in ~{wait}s (streak {streak})",
                name = tab.name,
                streak = watch.nudge_streak,
            );
            continue;
        }

        eligible.push(Eligible {
            tab_id: tab.id.clone(),
            tab_name: tab.name.clone(),
            trigger,
            output_fp: fp,
        });
    }
    // Resume next tick right after the last tab we reached, so a budget-truncated
    // scan rotates over the whole fleet instead of always re-scanning the head.
    *scan_cursor = scan_cursor.wrapping_add(scanned);

    // Drop watch state for tabs that vanished (closed / no longer a
    // Claude session) so the map stays bounded.
    watches.retain(|id, _| seen_ids.iter().any(|s| s == id));

    // Level (b) — API-STORM FULL FREEZE (rate-limit finding #5). When >=
    // STORM_ERROR_THRESHOLD distinct sessions hit an Anthropic API error within
    // the last minute, the opus bucket is storming: send NOTHING this tick — not
    // even the level-(a) heartbeat — so we don't re-poke agents that are backing
    // off (a `continue` on a 429 plateau only deepens it). The sliding window
    // lifts the freeze on its own once the errors age out (the agents recover).
    let storm_sessions = api_error_sessions(watches, now);
    if storm_freeze(storm_sessions) {
        println!(
            "⛑ brain: {storm_sessions} sessions in API error within {w}s (>= {thr}) — SYSTEMIC STORM; \
             FULL FREEZE this tick (0 nudge, not even the heartbeat) until it clears",
            w = STORM_ERROR_WINDOW.as_secs(),
            thr = STORM_ERROR_THRESHOLD,
        );
        return Ok(());
    }

    if eligible.is_empty() {
        return Ok(());
    }

    // AXE B — count-based circuit breaker, POLICY UPDATED. MORE than
    // CIRCUIT_BREAKER_THRESHOLD tabs frozen at once is systemic (a likely API
    // cap), not N independent failures. We NO LONGER go fully silent for the
    // cooldown (that made brain look dead and hid the recovery moment). Instead
    // we drop into HEARTBEAT mode: the SINGLE round-robin nudge still fires, just
    // spaced to `nudge_interval(true)` — one gentle recovery-probe that can't
    // storm the cap (still ≤1 nudge, and slower), keeps brain visibly alive, and
    // auto-resumes normal cadence the instant the cap clears.
    let systemic = circuit_breaker_open(breaker_until, eligible.len(), now);
    if systemic {
        println!(
            "⛑ brain: {n} tabs frozen at once (> {thr}) — SYSTEMIC (likely API cap); \
             heartbeat mode: one ~{cd}s round-robin probe nudge (not silence)",
            n = eligible.len(),
            thr = CIRCUIT_BREAKER_THRESHOLD,
            cd = CIRCUIT_BREAKER_COOLDOWN.as_secs(),
        );
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

    // Global anti-storm throttle: at most one nudge every `interval` across ALL
    // tabs — NUDGE_MIN_INTERVAL normally, the longer heartbeat under a systemic
    // breaker (AXE B). If a nudge fired within the window, skip this tick's send
    // WITHOUT advancing the cursor or touching any watch — every eligible tab
    // stays a candidate and fires at the next slot. This turns "50 frozen tabs →
    // burst of 50 continues" into a spaced single nudge, so a restart / API cap
    // can't spiral into a self-inflicted rate-limit.
    let interval = nudge_interval(systemic);
    if !nudge_ready(last_nudge_at, now, interval) {
        println!(
            "⛑ brain: {n} eligible, throttled (≤1 nudge / {s}s{mode}) — next slot soon",
            n = eligible.len(),
            s = interval.as_secs(),
            mode = if systemic { " heartbeat" } else { " fleet-wide" },
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
    // Record the frozen-output hash so this exact screen won't be
    // nudged again until the agent's output changes (work resumed,
    // or it re-stuck on something new). Replaces the old time-based
    // cooldown — a state guard, not a clock.
    if let Some(w) = watches.get_mut(&pick.tab_id) {
        w.nudged_fp = Some(pick.output_fp);
        // Advance the exponential backoff for this error episode: same
        // label → grow the streak (longer wait next time); new label →
        // restart at 1. The gate above suppresses nudges until then.
        let label = pick.trigger.label();
        w.nudge_streak = if w.last_label == Some(label) {
            w.nudge_streak + 1
        } else {
            1
        };
        w.last_label = Some(label);
        w.next_nudge_at = Some(now + Duration::from_secs(backoff_secs(w.nudge_streak)));
    }

    match ag
        .post(format!("{}/tabs/by-id/{}/input", ep.url, pick.tab_id))
        .header("Authorization", &auth)
        .header("Content-Type", "application/octet-stream")
        .send(pick.trigger.action().as_bytes())
    {
        Ok(_) => {}
        Err(e) if is_auth_error(&e) => {
            auth_breaker.trip(now);
            return Ok(());
        }
        Err(e) => return Err(format!("POST input for {}: {e}", pick.tab_id)),
    }

    // Inc8 S4: the `continue` landed → bump the tab's usage (observability of who
    // brain is nudging). Best-effort; a failed bump never fails the nudge.
    crate::cli::share_link::bump_usage(&ep, &pick.tab_id);

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
                     Anti-storm: fleet-wide, at most one nudge every {throttle}s, so a\n\
                     restart / API cap can't nudge dozens of frozen tabs in a burst.\n\
                     Circuit breaker: > {thr} tabs frozen at once is treated as systemic\n\
                     (likely an API cap) — Brain backs off ~{cd}s instead of nudging.",
                    n = PATTERNS.len(),
                    throttle = NUDGE_MIN_INTERVAL.as_secs(),
                    thr = CIRCUIT_BREAKER_THRESHOLD,
                    cd = CIRCUIT_BREAKER_COOLDOWN.as_secs(),
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

    let mut watches: HashMap<String, TabWatch> = HashMap::new();
    let mut probe = ConnectivityProbe::default();
    let mut rr_cursor: usize = 0;
    // Fleet-wide last-nudge clock for the anti-storm throttle (see nudge_ready).
    let mut last_nudge_at: Option<Instant> = None;
    // Circuit-breaker cooldown deadline: `Some(t)` while backing off (see
    // circuit_breaker_open).
    let mut breaker_until: Option<Instant> = None;
    // AXE A — persistent scan cursor: where the next tick's budget-bounded
    // /output scan resumes (see scan_order / TICK_BUDGET).
    let mut scan_cursor: usize = 0;
    let mut auth_breaker = AuthBreaker::default();
    loop {
        // Run the tick under catch_unwind so a panic anywhere in it (a
        // dependency edge case, a broken-pipe `println!`, …) is caught
        // and logged instead of silently killing brain. The &mut state
        // is AssertUnwindSafe: a panic mid-tick may leave the watch map
        // slightly stale, which the next tick re-syncs from the daemon.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tick(
                &mut watches,
                &mut probe,
                &mut rr_cursor,
                &mut last_nudge_at,
                &mut breaker_until,
                &mut scan_cursor,
                &mut auth_breaker,
            )
        }));
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

    #[test]
    fn circuit_breaker_throttles_to_a_heartbeat_not_silence() {
        // AXE B — POLICY: a systemic freeze no longer silences brain. The
        // detector (circuit_breaker_open) still trips + sticks for the cooldown,
        // but "systemic" now means HEARTBEAT MODE: the single round-robin nudge
        // still fires, just spaced to `nudge_interval(true)`. Locked here:
        // breaker-open ⇒ still ≤1 nudge, NEVER 0-all-suppressed silence.
        let t0 = Instant::now();
        // Detection (unchanged): a few frozen tabs = independent failures.
        let mut b: Option<Instant> = None;
        assert!(!circuit_breaker_open(&mut b, 2, t0), "2 frozen → not systemic");
        assert!(
            !circuit_breaker_open(&mut b, CIRCUIT_BREAKER_THRESHOLD, t0),
            "AT the threshold → not systemic"
        );
        assert_eq!(b, None, "no cooldown armed below/at threshold");
        // MORE than threshold = systemic → enters heartbeat mode + arms the
        // sticky cooldown (which now bounds how long we stay in heartbeat).
        let over = CIRCUIT_BREAKER_THRESHOLD + 1;
        assert!(circuit_breaker_open(&mut b, over, t0), "> threshold → systemic");
        assert!(b.is_some(), "cooldown armed (heartbeat-mode duration)");
        // Sticky: stays systemic through the cooldown even if the count drops.
        assert!(
            circuit_breaker_open(&mut b, 1, t0 + Duration::from_secs(5)),
            "still systemic during cooldown, regardless of count"
        );
        // Clears once the cooldown elapses and the spike is gone.
        assert!(
            !circuit_breaker_open(&mut b, 2, t0 + CIRCUIT_BREAKER_COOLDOWN),
            "after cooldown + settled → back to normal cadence"
        );
        assert_eq!(b, None, "cooldown cleared");
        // Still systemic after the cooldown → re-arms heartbeat (keeps probing).
        assert!(
            circuit_breaker_open(&mut b, over, t0 + CIRCUIT_BREAKER_COOLDOWN),
            "still systemic after cooldown → re-arms"
        );

        // POLICY LOCK: systemic ⇒ a SPACED heartbeat, never silence.
        assert_eq!(nudge_interval(false), NUDGE_MIN_INTERVAL, "normal spacing");
        assert_eq!(
            nudge_interval(true),
            CIRCUIT_BREAKER_COOLDOWN,
            "systemic → slower heartbeat spacing"
        );
        assert!(
            nudge_interval(true) > nudge_interval(false),
            "heartbeat is spaced MORE than normal (gentler probe)"
        );
        // The heartbeat still lets ONE nudge through — breaker-open ≠ 0 nudges.
        let mut last: Option<Instant> = None;
        assert!(
            nudge_ready(&mut last, t0, nudge_interval(true)),
            "systemic still fires ONE nudge (heartbeat/probe) — never total silence"
        );
        // …but spaced: a second within the heartbeat window is throttled (≤1, no storm).
        assert!(
            !nudge_ready(&mut last, t0 + Duration::from_secs(5), nudge_interval(true)),
            "second within the heartbeat window is throttled — still ≤1 nudge"
        );
        assert!(
            nudge_ready(&mut last, t0 + CIRCUIT_BREAKER_COOLDOWN, nudge_interval(true)),
            "the next heartbeat fires after the interval — brain stays alive + auto-probes"
        );
    }

    #[test]
    fn two_level_breaker_full_freezes_on_api_storm_but_heartbeats_when_mild() {
        // Rate-limit finding #5 — the breaker now has TWO levels:
        //  (a) MILD (few sessions stuck): unchanged — the spaced 1-nudge heartbeat.
        //  (b) SYSTEMIC (>= 5 DISTINCT sessions in API error within 60s): FULL
        //      FREEZE — 0 nudge, NOT EVEN the heartbeat — so a `continue` doesn't
        //      re-feed a herd that's backing off. The sliding window is the "calm"
        //      detector: once the errors age out, the freeze lifts on its own.
        let t0 = Instant::now();
        let errored_at = |at: Instant| TabWatch {
            last_crc: 0,
            last_fp: None,
            stable_since: at,
            cached_needle: None,
            nudged_fp: None,
            nudge_streak: 0,
            next_nudge_at: None,
            last_label: None,
            last_api_error_at: Some(at),
        };

        // (b) >= threshold distinct sessions in error within the window -> FREEZE.
        let mut storm: HashMap<String, TabWatch> = HashMap::new();
        for i in 0..STORM_ERROR_THRESHOLD {
            storm.insert(format!("t{i}"), errored_at(t0));
        }
        assert_eq!(api_error_sessions(&storm, t0), STORM_ERROR_THRESHOLD);
        assert!(
            storm_freeze(api_error_sessions(&storm, t0)),
            "storm -> FULL FREEZE (0 nudge)"
        );

        // Return to calm: errors age past the 60s window -> count drops -> resume.
        let calm = t0 + STORM_ERROR_WINDOW + Duration::from_secs(1);
        assert_eq!(
            api_error_sessions(&storm, calm),
            0,
            "errors older than the window don't count"
        );
        assert!(
            !storm_freeze(api_error_sessions(&storm, calm)),
            "window cleared -> no longer frozen"
        );

        // (a) below the threshold -> NOT a full freeze; the mild heartbeat still
        //     governs and fires exactly ONE spaced nudge (never total silence).
        let mut mild: HashMap<String, TabWatch> = HashMap::new();
        for i in 0..(STORM_ERROR_THRESHOLD - 1) {
            mild.insert(format!("t{i}"), errored_at(t0));
        }
        assert!(
            !storm_freeze(api_error_sessions(&mild, t0)),
            "4 < 5 -> mild, no full freeze"
        );
        let mut last: Option<Instant> = None;
        assert!(
            nudge_ready(&mut last, t0, nudge_interval(true)),
            "mild level still fires the level-(a) heartbeat nudge"
        );

        // The storm signal is API-capacity errors only — LOCAL network faults and
        // the generic agent-error flag don't inflate the storm count.
        assert!(is_api_storm_label("anthropic-529"), "429/overloaded counts");
        assert!(is_api_storm_label("anthropic-rate-limited"), "rate-limited counts");
        assert!(is_api_storm_label("api-retry-waiting"), "active retry banner counts");
        assert!(
            !is_api_storm_label("connection-refused"),
            "local network fault does NOT"
        );
        assert!(!is_api_storm_label("agent-state-error"), "generic error flag does NOT");
    }

    #[test]
    fn brain_leaves_a_tab_in_hotswap_handoff_alone() {
        // Inc9 cross-guard (hot-swap × brain): a tab mid-adoption in a binary
        // handoff carries `inHandoff:true` on /tabs. Brain must NOT nudge it — a
        // `continue` racing the handoff could double-launch the still-live agent.
        // A live Claude tab NOT in handoff is watchable; the same tab in handoff
        // is not. The `inHandoff` wire field (camelCase) deserializes correctly.
        let live: TabInfo =
            serde_json::from_str(r#"{"id":"t1","name":"worker","agent_kind":"claude","agent_session_id":"s1"}"#)
                .unwrap();
        assert!(
            is_watchable(&live),
            "a live Claude tab (not in handoff) is a brain target"
        );
        let adopting: TabInfo = serde_json::from_str(
            r#"{"id":"t1","name":"worker","agent_kind":"claude","agent_session_id":"s1","inHandoff":true}"#,
        )
        .unwrap();
        assert!(adopting.in_handoff, "inHandoff wire field parsed");
        assert!(
            !is_watchable(&adopting),
            "a tab mid-hot-swap-handoff is left ALONE by brain (no nudge)"
        );
    }

    #[test]
    fn output_crc_deserializes_from_tabs_and_defaults_for_old_daemon() {
        // S1 — the event-driven dirtiness signal rides the /tabs list. A row from
        // a NEW daemon carries `output_crc`; brain reads it to tell "screen changed
        // since my last poll" without a per-tab /output fetch.
        let fresh: TabInfo = serde_json::from_str(
            r#"{"id":"t1","name":"w","agent_kind":"claude","agent_session_id":"s1","output_crc":3735928559}"#,
        )
        .unwrap();
        assert_eq!(fresh.output_crc, 3_735_928_559, "crc read straight off the /tabs row");
        // A row from an OLDER daemon (pre-field) must default to 0 — the version-skew
        // window S2 treats as "unknown, scan it", never as a silently-skipped freeze.
        let old: TabInfo =
            serde_json::from_str(r#"{"id":"t1","name":"w","agent_kind":"claude","agent_session_id":"s1"}"#).unwrap();
        assert_eq!(old.output_crc, 0, "absent field defaults to 0 (old-daemon fallback)");
    }

    /// A fresh watch as the tick loop seeds one (T4 boot-seed: `stable_since` = now,
    /// `last_fp` None so the first `freeze_step` always fetches).
    fn fresh_watch(now: Instant) -> TabWatch {
        TabWatch {
            last_crc: 0,
            last_fp: None,
            stable_since: now,
            cached_needle: None,
            nudged_fp: None,
            nudge_streak: 0,
            next_nudge_at: None,
            last_label: None,
            last_api_error_at: None,
        }
    }

    fn assess(o: FreezeOutcome) -> (Duration, Option<Trigger>) {
        match o {
            FreezeOutcome::Assessed { stable_for, trigger } => (stable_for, trigger),
            FreezeOutcome::FetchFailed => panic!("expected Assessed, got FetchFailed"),
        }
    }

    const UNREACHABLE: &str = "⎿  API Error: Unable to connect to API (ConnectionRefused)\n❯ continue";

    #[test]
    fn freeze_step_skips_unchanged_crc_and_reuses_cached_needle() {
        // S2 core (the CPU win + T1 cache): an unchanged wire crc ⇒ NO /output
        // fetch, yet the tab stays freeze-assessed off the CACHED needle, and its
        // freeze clock keeps accruing. This is exactly the frozen-tab path: it
        // emits no new event, so brain must nudge it WITHOUT re-polling it.
        let t0 = Instant::now();
        let mut w = fresh_watch(t0);
        let fetches = std::cell::Cell::new(0);
        // First sight (last_fp None) → fetch, scan, cache the unreachable needle.
        let (stable0, trig0) = assess(freeze_step(&mut w, 0x1234, false, t0, || {
            fetches.set(fetches.get() + 1);
            Some(UNREACHABLE.to_string())
        }));
        assert_eq!(fetches.get(), 1, "first sight fetches");
        assert_eq!(stable0, Duration::ZERO, "freeze clock starts at 0 on first fetch");
        assert!(
            matches!(trig0, Some(Trigger::Pattern(p)) if p.label == "anthropic-unreachable"),
            "needle scanned + returned on the fetch tick"
        );
        // Next tick, SAME crc, 25 s later → MUST NOT fetch; cached needle still
        // drives the trigger; stable_for has grown past STABLE_SECS (frozen).
        let t1 = t0 + Duration::from_secs(25);
        let (stable1, trig1) = assess(freeze_step(&mut w, 0x1234, false, t1, || {
            fetches.set(fetches.get() + 1);
            panic!("must not fetch an unchanged tab");
        }));
        assert_eq!(fetches.get(), 1, "unchanged crc → ZERO extra fetches (the whole point)");
        assert!(
            stable1 >= Duration::from_secs(STABLE_SECS),
            "freeze clock accrued across the skip"
        );
        assert!(
            matches!(trig1, Some(Trigger::Pattern(p)) if p.label == "anthropic-unreachable"),
            "T1: the cached needle stands in for a fresh scan of the byte-identical screen"
        );
    }

    #[test]
    fn freeze_step_refetches_on_crc_change_and_resets_freeze_clock() {
        // A changed crc = the screen moved = the agent is alive → re-fetch, re-scan,
        // and reset the freeze clock (never nudge a working tab).
        let t0 = Instant::now();
        let mut w = fresh_watch(t0);
        let _ = assess(freeze_step(&mut w, 0x1111, false, t0, || Some(UNREACHABLE.to_string())));
        // crc changed → fetch a now-clean screen; freeze clock resets, needle clears.
        let t1 = t0 + Duration::from_secs(25);
        let (stable1, trig1) = assess(freeze_step(&mut w, 0x2222, false, t1, || {
            Some("all good\n$ ".to_string())
        }));
        assert_eq!(stable1, Duration::ZERO, "crc change reset the freeze clock");
        assert!(trig1.is_none(), "re-scan of the new clean screen clears the trigger");
    }

    #[test]
    fn freeze_step_crc_zero_falls_back_to_output_hash() {
        // crc == 0 (old daemon w/o the field, or an empty screen) = "unknown": brain
        // must always fetch and drive the freeze clock off a HASH of the output —
        // exactly the pre-event behaviour — never silently skip a possibly-frozen tab.
        let t0 = Instant::now();
        let mut w = fresh_watch(t0);
        let fetches = std::cell::Cell::new(0);
        let mut step = |now: Instant, text: &'static str| {
            assess(freeze_step(&mut w, 0, false, now, || {
                fetches.set(fetches.get() + 1);
                Some(text.to_string())
            }))
        };
        let (_, _) = step(t0, UNREACHABLE);
        // Same text, crc still 0 → STILL fetches (can't skip on 0), and since the
        // hash is unchanged the freeze clock accrues → frozen after STABLE_SECS.
        let (stable1, trig1) = step(t0 + Duration::from_secs(25), UNREACHABLE);
        assert_eq!(fetches.get(), 2, "crc==0 always fetches (no skip in the fallback)");
        assert!(
            stable1 >= Duration::from_secs(STABLE_SECS),
            "identical output-hash accrues freeze"
        );
        assert!(
            matches!(trig1, Some(Trigger::Pattern(_))),
            "needle still detected in the fallback"
        );
    }

    #[test]
    fn freeze_step_fetch_failure_leaves_state_untouched() {
        // A transient /output error must NOT corrupt the watch (no false freeze-reset,
        // no cache wipe) — the tab is simply reassessed next tick.
        let t0 = Instant::now();
        let mut w = fresh_watch(t0);
        let _ = assess(freeze_step(&mut w, 0x9, false, t0, || Some(UNREACHABLE.to_string())));
        let fp_before = w.last_fp;
        let since_before = w.stable_since;
        // crc changed (would normally fetch) but the fetch fails → FetchFailed, state intact.
        let out = freeze_step(&mut w, 0xA, false, t0 + Duration::from_secs(5), || None);
        assert!(matches!(out, FreezeOutcome::FetchFailed), "fetch error → FetchFailed");
        assert_eq!(w.last_fp, fp_before, "fingerprint untouched by a failed fetch");
        assert_eq!(w.stable_since, since_before, "freeze clock untouched by a failed fetch");
    }

    #[test]
    fn t1_golden_set_event_driven_matches_polling_exactly() {
        // T1 (PORTEUR). Because output_crc = crc32(output), the wire crc changes IFF
        // the screen changes — so the event-driven model (skip-fetch + cached needle
        // + crc-driven freeze clock) reaches the SAME (frozen? , which-needle?)
        // decision, TICK FOR TICK, as the old poll-every-tick model (hash + fresh
        // scan). Not "a gel is nudged" but "EXACTLY the same tabs, same ticks".
        //
        // Each row is (elapsed_secs, screen_text). The crc stand-in bumps whenever
        // the text changes — the real crc32 invariant, forced non-zero.
        let scenario: &[(u64, &str)] = &[
            (0, "working: building...\n"),   // screen moving → alive
            (2, "working: compiling foo\n"), // changed → alive
            (4, UNREACHABLE),                // error appears, screen now stuck here
            (9, UNREACHABLE),                // frozen 5 s (< STABLE) — not yet
            (24, UNREACHABLE),               // frozen 20 s (>= STABLE) — FROZEN + needle
            (40, UNREACHABLE),               // still frozen on the same screen
            (42, "recovered, all good\n$ "), // changed → clean → freeze clock resets
            (62, "recovered, all good\n$ "), // frozen again but NO needle → never eligible
        ];
        let t0 = Instant::now();
        let mut wn = fresh_watch(t0); // NEW event-driven model
        let mut old_last_hash: Option<u64> = None; // OLD poll-every-tick reference
        let mut old_stable = t0;
        for &(secs, text) in scenario {
            let now = t0 + Duration::from_secs(secs);
            // NEW: crc changes iff text changes (forced non-zero so it's never the
            // "unknown" fallback). The fetch closure only runs when NEW decides to.
            let crc = (hash_output(text) as u32) | 1;
            let (sf_new, trig_new) = assess(freeze_step(&mut wn, crc, false, now, || Some(text.to_string())));
            let elig_new = sf_new >= Duration::from_secs(STABLE_SECS) && trig_new.is_some();
            // OLD: fetch every tick, hash for stability, scan fresh for the needle.
            let h = hash_output(text);
            if old_last_hash != Some(h) {
                old_last_hash = Some(h);
                old_stable = now;
            }
            let sf_old = now.duration_since(old_stable);
            let trig_old = scan_output(text).map(Trigger::Pattern);
            let elig_old = sf_old >= Duration::from_secs(STABLE_SECS) && trig_old.is_some();
            assert_eq!(elig_new, elig_old, "eligibility diverged at t+{secs}s");
            assert_eq!(
                trig_new.map(Trigger::label),
                trig_old.map(Trigger::label),
                "needle diverged at t+{secs}s"
            );
        }
    }

    #[test]
    fn t2_a_clean_frozen_tab_is_never_nudged() {
        // T2 false-positive. A tab frozen on a screen with NO error needle and no
        // agent-error flag is legitimately quiet (idle at a prompt, a silent compile)
        // — it must NEVER be nudged, however long it's frozen. (inHandoff tabs are
        // excluded upstream by is_watchable, before freeze_step — see
        // brain_leaves_a_tab_in_hotswap_handoff_alone.)
        let t0 = Instant::now();
        let mut w = fresh_watch(t0);
        let (_, trig0) = assess(freeze_step(&mut w, 0x55, false, t0, || {
            Some("$ cargo build\n   Compiling tab-atelier\n".to_string())
        }));
        assert!(trig0.is_none(), "a clean screen yields no trigger");
        // 10 minutes later, same crc (frozen), still no needle, agent_error = false.
        let (sf, trig) = assess(freeze_step(&mut w, 0x55, false, t0 + Duration::from_mins(10), || {
            panic!("frozen clean tab must not be re-fetched")
        }));
        assert!(sf >= Duration::from_secs(STABLE_SECS), "it IS frozen a long time");
        assert!(trig.is_none(), "…but with no needle/error it is never eligible");
    }

    #[test]
    fn t3_storm_coalesces_to_the_latest_delta_and_freeze_stays_reliable() {
        // T3. Under a storm a tab's screen churns fast. Option A coalesces
        // structurally: the wire carries only the LATEST crc per tab, so brain always
        // assesses the most RECENT delta, never a stale earlier one. Here a tab churns
        // (retry banner) then FREEZES on a 529 — brain must nudge on the 529 (latest),
        // not the earlier retry banner.
        let t0 = Instant::now();
        let mut w = fresh_watch(t0);
        // Delta 1: a live retry banner (its own crc).
        let (_, trig_a) = assess(freeze_step(&mut w, 0xA1, false, t0, || {
            Some("✻ Waiting for API response · will retry in 1m 57s".to_string())
        }));
        assert!(
            matches!(trig_a, Some(Trigger::Pattern(p)) if p.label == "api-retry-waiting"),
            "first delta = retry banner"
        );
        // Delta 2 (latest): the screen moves to a 529 and STICKS there.
        let (_, trig_b) = assess(freeze_step(&mut w, 0xB2, false, t0 + Duration::from_secs(1), || {
            Some("● API Error: 529 Overloaded. This is a server-side issue, usually temporary".to_string())
        }));
        assert!(
            matches!(trig_b, Some(Trigger::Pattern(p)) if p.label == "anthropic-529"),
            "coalesced to the LATEST delta (529), not the stale retry banner"
        );
        // Frozen on the 529 for STABLE_SECS → eligible, still on the latest needle.
        let (sf, trig_c) = assess(freeze_step(
            &mut w,
            0xB2,
            false,
            t0 + Duration::from_secs(STABLE_SECS + 2),
            || panic!("frozen 529: must not re-fetch"),
        ));
        assert!(
            sf >= Duration::from_secs(STABLE_SECS),
            "freeze reliable off the latest delta"
        );
        assert!(
            matches!(trig_c, Some(Trigger::Pattern(p)) if p.label == "anthropic-529"),
            "nudge fires on the 529 that's actually stuck, not an earlier delta"
        );
    }

    #[test]
    fn t4_boot_seed_prevents_a_false_gel_burst_at_hotswap() {
        // T4. At brain start / hot-swap a whole fleet may already be sitting frozen on
        // an error. Seeding stable_since = now on first sight gives each a full
        // STABLE_SECS grace, so NONE is instantly eligible — no burst of `continue`s
        // the instant brain comes up. Only after real STABLE_SECS of continued freeze
        // do they qualify (and the fleet-wide throttle then spaces them, one at a time).
        let t0 = Instant::now();
        let mut fleet: Vec<TabWatch> = (0..10).map(|_| fresh_watch(t0)).collect();
        let mut eligible_at_boot = 0;
        for (i, w) in fleet.iter_mut().enumerate() {
            let crc = 0x1000 + i as u32;
            let (sf, trig) = assess(freeze_step(w, crc, false, t0, || Some(UNREACHABLE.to_string())));
            assert!(trig.is_some(), "the error IS detected at boot");
            if should_nudge(sf, w.nudged_fp, w.last_fp.unwrap_or(0)) {
                eligible_at_boot += 1;
            }
        }
        assert_eq!(
            eligible_at_boot, 0,
            "T4: boot-seed → ZERO instant nudges (no false-gel burst)"
        );
        // After a real STABLE_SECS of the SAME frozen screen, they all qualify.
        let later = t0 + Duration::from_secs(STABLE_SECS + 1);
        let mut eligible_later = 0;
        for (i, w) in fleet.iter_mut().enumerate() {
            let crc = 0x1000 + i as u32;
            let (sf, _) = assess(freeze_step(w, crc, false, later, || {
                panic!("frozen: must not re-fetch")
            }));
            if should_nudge(sf, w.nudged_fp, w.last_fp.unwrap_or(0)) {
                eligible_later += 1;
            }
        }
        assert_eq!(
            eligible_later, 10,
            "after real STABLE_SECS the frozen fleet qualifies (throttle then spaces them)"
        );
    }

    #[test]
    fn s3_measure_event_driven_cuts_the_busy_scan_load() {
        // S3 MEASUREMENT (built≠wired). Prove the event-driven path actually REMOVES
        // per-tab work on the busy fleet — not just that it compiles. Model a busy
        // fleet of FLEET tabs where DIRTY_PER_TICK stream (crc changes) each 5 s tick,
        // over TICKS ticks. The old model fetch+scans EVERY tab EVERY tick (that N
        // per-tab /output GETs is the ~47% busy driver the PO feels); the event-driven
        // model, via freeze_step, fetches ONLY the tabs whose crc moved. We drive the
        // REAL freeze_step and count the REAL fetch-closure invocations (= /output
        // round-trips), plus time the real per-scan CPU (hash + scan on a ~4 KB
        // screen) so the count converts to actual saved busy-time.
        //
        // Run `cargo test s3_measure -- --nocapture` to see the numbers.
        const FLEET: usize = 30;
        const DIRTY_PER_TICK: usize = 9; // ~30 % streaming at any instant = a busy fleet
        const TICKS: usize = 200;

        // A realistic ~4 KB agent screen with an error needle in the trailing window.
        let mut screen = "● Running the build and watching for the connectivity flake\n".repeat(90);
        screen.push_str(UNREACHABLE);
        assert!(screen.len() > SCOPE_TAIL_BYTES, "screen spans the full scan window");

        // Measured cost of ONE round-trip's Brian-side CPU (what a fetch+scan does):
        // hash the output + scan it for a needle. This is the per-GET work the skip
        // eliminates (on top of the HTTP round-trip + the daemon's snapshot clone/crc,
        // which this unit test can't spin a daemon to time).
        let cpu_probe_iters = 2_000u32;
        let t = Instant::now();
        let mut sink = 0u64;
        for _ in 0..cpu_probe_iters {
            sink ^= hash_output(&screen);
            sink ^= scan_output(&screen).map_or(0, |p| p.needle.len() as u64);
        }
        let per_scan = t.elapsed() / cpu_probe_iters;
        assert_ne!(sink, 1, "keep the optimiser honest"); // sink is observed

        // Event-driven: drive freeze_step, count real fetch invocations.
        let t0 = Instant::now();
        let mut watches: Vec<TabWatch> = (0..FLEET).map(|_| fresh_watch(t0)).collect();
        let mut crcs: Vec<u32> = (0..FLEET as u32).map(|i| (i | 1) << 1).collect();
        let mut evt_fetches = 0usize;
        for tick in 0..TICKS {
            let now = t0 + Duration::from_secs((tick as u64) * 5);
            // Rotate which tabs stream this tick (a moving hot-set).
            for d in 0..DIRTY_PER_TICK {
                let idx = (tick * DIRTY_PER_TICK + d) % FLEET;
                crcs[idx] = crcs[idx].wrapping_add(2).max(2); // new, non-zero crc
            }
            for i in 0..FLEET {
                let _ = freeze_step(&mut watches[i], crcs[i], false, now, || {
                    evt_fetches += 1;
                    Some(screen.clone())
                });
            }
        }

        // Poll-all baseline: every tab, every tick — what brain did before.
        let poll_fetches = FLEET * TICKS;
        let evt_busy = per_scan * evt_fetches as u32;
        let poll_busy = per_scan * poll_fetches as u32;
        let pct_cut = 100 - (evt_fetches * 100 / poll_fetches);
        println!(
            "S3 busy-scan measurement (FLEET={FLEET}, {DIRTY_PER_TICK}/tick dirty, {TICKS} ticks):\n  \
             per-scan CPU : {per_scan:?}\n  \
             poll-all     : {poll_fetches} /output round-trips  (~{poll_busy:?} scan-CPU)\n  \
             event-driven : {evt_fetches} /output round-trips  (~{evt_busy:?} scan-CPU)\n  \
             REDUCTION    : {pct_cut}% fewer round-trips on the busy fleet"
        );

        // The idle/unchanged tabs (FLEET - DIRTY_PER_TICK each tick) no longer cost a
        // round-trip + scan. With ~30 % streaming that's a ~70 % cut — real busy-time
        // off the daemon, deterministically. (first-sight adds FLEET fetches once.)
        assert!(
            evt_fetches <= DIRTY_PER_TICK * TICKS + FLEET,
            "event-driven fetches only the dirty tabs (+ first-sight seed)"
        );
        assert!(
            evt_fetches * 2 < poll_fetches,
            "busy fleet: event-driven does <50% of poll-all's round-trips ({evt_fetches} vs {poll_fetches})"
        );
    }

    #[test]
    fn scan_order_rotates_and_wraps_for_resumable_scans() {
        // AXE A — a budget-truncated scan resumes across the fleet: starting at
        // the cursor and wrapping, so later ticks reach the tail instead of
        // always re-scanning the head.
        assert_eq!(scan_order(3, 0).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(
            scan_order(3, 4).collect::<Vec<_>>(),
            vec![1, 2, 0],
            "start at 4 % 3 = 1, wrap"
        );
        assert_eq!(
            scan_order(0, 5).collect::<Vec<_>>(),
            Vec::<usize>::new(),
            "no tabs → nothing to scan"
        );
        // usize::MAX cursor must not panic (wrapping), and still covers all indices.
        let mut got = scan_order(3, usize::MAX).collect::<Vec<_>>();
        got.sort_unstable();
        assert_eq!(got, vec![0, 1, 2], "wrap-around still covers every tab exactly once");
    }

    #[test]
    fn nudge_throttle_spaces_bursts_and_never_simultaneous() {
        // Two (or fifty) tabs freeze at once: the throttle lets exactly ONE
        // nudge through per NUDGE_MIN_INTERVAL, so they're unstuck 2 s apart,
        // never in a burst that re-caps the API.
        let mut last: Option<Instant> = None;
        let t0 = Instant::now();
        let iv = NUDGE_MIN_INTERVAL; // normal (non-systemic) spacing
        // First frozen tab fires immediately (nothing nudged yet).
        assert!(nudge_ready(&mut last, t0, iv), "first nudge always allowed");
        // A SECOND frozen tab in the same instant is throttled — not simultaneous.
        assert!(
            !nudge_ready(&mut last, t0, iv),
            "a second nudge in the same window is skipped"
        );
        // Still throttled just under the interval.
        assert!(
            !nudge_ready(&mut last, t0 + Duration::from_millis(1999), iv),
            "still throttled just under {}s",
            NUDGE_MIN_INTERVAL.as_secs()
        );
        // Once the interval has elapsed since the last EMITTED nudge, the next fires.
        assert!(
            nudge_ready(&mut last, t0 + NUDGE_MIN_INTERVAL, iv),
            "next nudge allowed ≥{}s after the previous one",
            NUDGE_MIN_INTERVAL.as_secs()
        );
        // …and firing re-arms the throttle, so the third waits another interval.
        assert!(
            !nudge_ready(&mut last, t0 + NUDGE_MIN_INTERVAL, iv),
            "throttle re-arms on each emission"
        );
        assert!(
            nudge_ready(&mut last, t0 + NUDGE_MIN_INTERVAL * 2, iv),
            "and the one after, a slot later"
        );
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
            output_fp: 0,
        };
        let a = Eligible {
            tab_id: "tab-1".into(),
            tab_name: "shell".into(),
            trigger: Trigger::AgentError,
            output_fp: 0,
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
    fn auth_backoff_grows_and_caps() {
        // Mirrors the nudge backoff: 60s, 120s, 240s, 480s, then capped 900s.
        assert_eq!(auth_backoff_secs(1), 60);
        assert_eq!(auth_backoff_secs(2), 120);
        assert_eq!(auth_backoff_secs(3), 240);
        assert_eq!(auth_backoff_secs(4), 480);
        assert_eq!(auth_backoff_secs(5), 900); // 960 → capped
        assert_eq!(auth_backoff_secs(20), 900); // stays capped, no overflow
        assert_eq!(auth_backoff_secs(0), 60); // saturating, never panics
    }

    #[test]
    fn auth_breaker_trips_backs_off_and_recovers() {
        // The "brain kept nudging through a 401" fix: one auth failure opens the
        // breaker (brain skips ticks) and it stays open for the backoff window;
        // a second failure grows it; an authorised call clears it at once.
        let mut b = AuthBreaker::default();
        let now = Instant::now();
        assert!(!b.is_open(now)); // closed by default

        b.trip(now); // first 401 → open 60s
        assert_eq!(b.streak, 1);
        assert!(b.is_open(now));
        assert!(b.is_open(now + Duration::from_secs(59)));
        assert!(!b.is_open(now + Duration::from_secs(61))); // window elapsed

        b.trip(now); // consecutive failure grows the back-off to 120s
        assert_eq!(b.streak, 2);
        assert!(b.is_open(now + Duration::from_secs(61)));
        assert!(!b.is_open(now + Duration::from_secs(121)));

        b.clear(); // authorised call → resume nudging immediately
        assert_eq!(b.streak, 0);
        assert!(!b.is_open(now));
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
    fn hash_output_is_stable_and_sensitive() {
        // Same input → same hash (stability across ticks).
        assert_eq!(hash_output("retrying in 38s"), hash_output("retrying in 38s"));
        // A one-second countdown tick changes the hash → resets the
        // stability clock → an auto-retrying agent is never nudged.
        assert_ne!(hash_output("retrying in 38s"), hash_output("retrying in 37s"));
    }
}
