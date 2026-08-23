// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `clarify` — controlled context refresh via auto re-home.
//!
//! Instead of Claude's opaque auto-compaction, `clarify` refreshes a saturated
//! agent by RE-HOMING it in place: `rehome-tab.sh <uuid> <cwd> "<assignment>"
//! "<name>" --go --auto-close` writes a handoff synthesis, spawns a fresh agent
//! with the SAME cwd/assignment/name seeded from it, and closes the old tab once
//! the bidirectional proof completes. A controlled reset, not a black box.
//!
//! Two modes:
//! - `clarify <tab>`         — re-home one tab now (on demand).
//! - `clarify --watch [...]` — daemon poller (like `brain`): watches every
//!   Claude tab and fires a re-home when it crosses the context threshold.
//!
//! Guardrails (daemon): per-tab cooldown (no immediate re-fire on re-saturation),
//! SKIP meta/daemon tabs (brain, aligator, scribe, …, and any orchestrator), and
//! a count-based anti-storm circuit breaker (many tabs saturated at once = back
//! off, same discipline as `brain`'s breaker) so a fleet-wide spike doesn't
//! re-home everything simultaneously.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::cli::share_link::{Endpoint, agent, discover_endpoint, resolve};

/// Context-USED percentage above which a tab is refreshed (`> `, not `>=`).
const CONTEXT_THRESHOLD_PCT: u8 = 90;
/// Per-tab cooldown after a re-home: a fresh agent won't be re-homed again for
/// this long even if it re-saturates, so a pathological loop can't thrash.
const REHOME_COOLDOWN: Duration = Duration::from_mins(10);
/// Count-based anti-storm breaker: MORE than this many tabs saturated at once is
/// systemic — back off instead of re-homing them all (same idea as brain's).
const REFRESH_STORM_THRESHOLD: usize = 6;
const REFRESH_STORM_COOLDOWN: Duration = Duration::from_mins(1);
/// Default seconds between daemon scans.
const DEFAULT_INTERVAL_SECS: u64 = 30;

/// Tab names (case-insensitive substrings) that are meta/daemon tabs — never
/// auto-refreshed. These run tools/orchestration, not a re-homeable work session.
const SKIP_NAMES: [&str; 8] = [
    "brain", "aligator", "scribe", "ange", "ford", "sage", "tichef", "watcher",
];
/// Roles that are never auto-refreshed — an orchestrator / meta specialist owns
/// context that a blind re-home would drop.
const SKIP_ROLES: [&str; 4] = ["orchestrator", "tichef", "planner", "auditor"];

#[derive(Debug, Deserialize)]
struct TabInfo {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    assignment: Option<String>,
    #[serde(default)]
    agent_kind: Option<String>,
    #[serde(default)]
    agent_session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TabsResponse {
    tabs: Vec<TabInfo>,
}

/// Every `<digits>%` number in `s`, clamped to 0..=100. Byte-safe: only indexes
/// ASCII digits + `%`, so a multibyte screen (emoji/accents) never panics.
fn percents(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    for i in 0..b.len() {
        if b[i] == b'%' {
            let mut j = i;
            while j > 0 && b[j - 1].is_ascii_digit() {
                j -= 1;
            }
            if j < i
                && let Ok(n) = s[j..i].parse::<u16>()
            {
                out.push(n.min(100) as u8);
            }
        }
    }
    out
}

/// Extract the agent's context-USED percentage from its screen.
///
/// Reads Claude Code's context marker in both wordings: "N% context used" (used
/// directly) and a "…context left…: N%" / "N% context left" auto-compact banner
/// (inverted, so used = 100 − N). Returns the highest USED% found; `None` when
/// no context marker is present. Pure + unit-tested.
///
/// `ponytail:` the exact Claude Code wording is confirmed end-to-end with tichef;
/// the "context" needle + the left/used flip cover the forms seen so far.
#[must_use]
pub fn parse_context_pct(screen: &str) -> Option<u8> {
    let mut best: Option<u8> = None;
    for line in screen.lines() {
        let lower = line.to_ascii_lowercase();
        if !lower.contains("context") {
            continue;
        }
        let inverted = lower.contains("left") || lower.contains("compact");
        for pct in percents(line) {
            let used = if inverted { 100u8.saturating_sub(pct) } else { pct };
            best = Some(best.map_or(used, |b| b.max(used)));
        }
    }
    best
}

/// Should this tab be skipped by the AUTO re-home poller? A meta/daemon name or
/// a meta/orchestrator role → yes (protect the tabs that run the show). Pure.
#[must_use]
pub fn should_skip_rehome(name: &str, assignment: Option<&str>) -> bool {
    let lname = name.to_ascii_lowercase();
    if SKIP_NAMES.iter().any(|d| lname.contains(d)) {
        return true;
    }
    SKIP_ROLES.contains(&crate::api::role_of(assignment).as_str())
}

/// Args passed to `rehome-tab.sh` for an in-place refresh: same cwd / assignment
/// / name, plus `--go --auto-close`. Pure, so the invocation is unit-testable.
#[must_use]
pub fn rehome_command(uuid: &str, cwd: &str, assignment: &str, name: &str) -> Vec<String> {
    vec![
        uuid.to_string(),
        cwd.to_string(),
        assignment.to_string(),
        name.to_string(),
        "--go".to_string(),
        "--auto-close".to_string(),
    ]
}

/// Count-based anti-storm circuit breaker (mirrors brain's discipline). Returns
/// `true` (suppress re-homes this tick) while mid-cooldown OR when the
/// saturated-count just tripped it (which arms the cooldown). Pure + testable.
fn refresh_storm_open(breaker_until: &mut Option<Instant>, saturated: usize, now: Instant) -> bool {
    if let Some(until) = *breaker_until {
        if now < until {
            return true;
        }
        *breaker_until = None;
    }
    if saturated > REFRESH_STORM_THRESHOLD {
        *breaker_until = Some(now + REFRESH_STORM_COOLDOWN);
        return true;
    }
    false
}

/// Path to `rehome-tab.sh`: `$TAB_ATELIER_REHOME_SCRIPT`, else
/// `~/Dev/Botmox/rehome-tab.sh`. ponytail: machine-specific default (the Kalpin
/// poste) — override via env on other hosts.
fn rehome_script() -> String {
    std::env::var("TAB_ATELIER_REHOME_SCRIPT").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/Dev/Botmox/rehome-tab.sh")
    })
}

/// Fire `rehome-tab.sh` detached (it runs for minutes — handoff, spawn, wait;
/// `--auto-close` closes the old tab). Fire-and-forget: we don't block the tick.
fn fire_rehome(script: &str, uuid: &str, cwd: &str, assignment: &str, name: &str) -> Result<(), String> {
    std::process::Command::new(script)
        .args(rehome_command(uuid, cwd, assignment, name))
        .spawn()
        .map(|_child| ())
        .map_err(|e| format!("spawn {script}: {e}"))
}

/// A saturated tab eligible for auto re-home this tick.
struct Eligible {
    id: String,
    cwd: String,
    assignment: String,
    name: String,
    pct: u8,
}

/// GET /tabs, then per Claude tab: skip meta/daemon + cooldown'd, read its
/// screen, parse context%, collect those over the threshold. One re-home per
/// tick (behind the anti-storm breaker).
fn watch_tick(
    last_rehome: &mut HashMap<String, Instant>,
    breaker_until: &mut Option<Instant>,
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
    let mut seen: Vec<String> = Vec::new();
    for tab in tabs.tabs {
        // Only live Claude sessions.
        if tab.agent_kind.as_deref() != Some("claude") || tab.agent_session_id.as_deref().unwrap_or("").is_empty() {
            continue;
        }
        seen.push(tab.id.clone());
        // Guardrail: never auto-refresh a meta/daemon/orchestrator tab.
        if should_skip_rehome(&tab.name, tab.assignment.as_deref()) {
            continue;
        }
        // In-place re-home needs both a repo cwd and an assignment to re-seed.
        let (Some(cwd), Some(assignment)) = (tab.cwd.clone(), tab.assignment.clone()) else {
            continue;
        };
        // Guardrail: per-tab cooldown.
        if let Some(last) = last_rehome.get(&tab.id)
            && now.duration_since(*last) < REHOME_COOLDOWN
        {
            continue;
        }
        // Read the screen + parse the context marker.
        let output = ag
            .get(format!("{}/tabs/by-id/{}/output", ep.url, tab.id))
            .header("Authorization", &auth)
            .call()
            .map_err(|e| format!("GET output for {}: {e}", tab.id))?
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("read output for {}: {e}", tab.id))?;
        let Some(pct) = parse_context_pct(&output) else {
            continue;
        };
        if pct <= CONTEXT_THRESHOLD_PCT {
            continue;
        }
        eligible.push(Eligible {
            id: tab.id,
            cwd,
            assignment,
            name: tab.name,
            pct,
        });
    }
    last_rehome.retain(|id, _| seen.iter().any(|s| s == id));

    if eligible.is_empty() {
        return Ok(());
    }
    // Anti-storm: many saturated at once = systemic → back off.
    if refresh_storm_open(breaker_until, eligible.len(), now) {
        println!(
            "clarify: {n} tabs over {thr}% context at once — SYSTEMIC; backing off ~{cd}s (no refresh)",
            n = eligible.len(),
            thr = CONTEXT_THRESHOLD_PCT,
            cd = REFRESH_STORM_COOLDOWN.as_secs(),
        );
        return Ok(());
    }
    // One re-home per tick, round-robin across saturated tabs.
    let idx = *cursor % eligible.len();
    *cursor = cursor.wrapping_add(1);
    let pick = &eligible[idx];
    let script = rehome_script();
    println!(
        "clarify: {name} at {pct}% context (> {thr}) → re-homing in place",
        name = pick.name,
        pct = pick.pct,
        thr = CONTEXT_THRESHOLD_PCT,
    );
    match fire_rehome(&script, &pick.id, &pick.cwd, &pick.assignment, &pick.name) {
        Ok(()) => {
            last_rehome.insert(pick.id.clone(), now);
        }
        Err(e) => eprintln!("clarify: {e}"),
    }
    Ok(())
}

/// Manual one-shot: re-home `key` (index/UUID) now, regardless of context% —
/// the human asked. Still requires a cwd + assignment to re-seed in place.
fn clarify_one(key: &str) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("clarify: {e}");
            return 1;
        }
    };
    let uuid = match resolve(&ep, key) {
        Ok((_, id)) => id,
        Err(e) => {
            eprintln!("clarify: {e}");
            return 1;
        }
    };
    let ag = agent();
    let auth = format!("Bearer {}", ep.token);
    let tabs: TabsResponse = match ag
        .get(format!("{}/tabs", ep.url))
        .header("Authorization", &auth)
        .call()
        .map_err(|e| format!("GET /tabs: {e}"))
        .and_then(|mut r| r.body_mut().read_json().map_err(|e| format!("parse /tabs: {e}")))
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("clarify: {e}");
            return 1;
        }
    };
    let Some(tab) = tabs.tabs.into_iter().find(|t| t.id == uuid) else {
        eprintln!("clarify: tab {uuid} not found");
        return 1;
    };
    let (Some(cwd), Some(assignment)) = (tab.cwd, tab.assignment) else {
        eprintln!("clarify: tab {uuid} has no cwd/assignment — nothing to re-seed (assign it first)");
        return 1;
    };
    let script = rehome_script();
    match fire_rehome(&script, &uuid, &cwd, &assignment, &tab.name) {
        Ok(()) => {
            println!("clarify: re-homing {name} ({uuid}) in place", name = tab.name);
            0
        }
        Err(e) => {
            eprintln!("clarify: {e}");
            1
        }
    }
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let mut watch = false;
    let mut once = false;
    let mut interval = DEFAULT_INTERVAL_SECS;
    let mut key: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--watch" => watch = true,
            "--once" => once = true,
            "--interval" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    Some(n) if n >= 1 => interval = n,
                    _ => {
                        eprintln!("clarify: --interval expects a number >= 1");
                        return 2;
                    }
                }
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: tab-atelier-headless clarify <tab>            # re-home one tab now\n       \
                            tab-atelier-headless clarify --watch [--interval SECS] [--once]\n\
                     Controlled context refresh: re-home a saturated Claude tab in place\n\
                     (same cwd/assignment/name) instead of opaque auto-compaction.\n\
                     Daemon fires at > {CONTEXT_THRESHOLD_PCT}% context. Guardrails: per-tab\n\
                     cooldown, skips meta/daemon tabs + orchestrators, anti-storm breaker."
                );
                return 0;
            }
            other if !other.starts_with('-') && key.is_none() => key = Some(other.to_string()),
            other => {
                eprintln!("clarify: unexpected argument: {other}");
                return 2;
            }
        }
        i += 1;
    }

    if let Some(key) = key {
        if watch {
            eprintln!("clarify: pass a tab OR --watch, not both");
            return 2;
        }
        return clarify_one(&key);
    }
    if !watch {
        eprintln!("clarify: pass a tab to re-home now, or --watch to run the poller (see --help)");
        return 2;
    }

    println!("clarify — watching every {interval}s · auto re-home at > {CONTEXT_THRESHOLD_PCT}% context (in place)");
    let mut last_rehome: HashMap<String, Instant> = HashMap::new();
    let mut breaker_until: Option<Instant> = None;
    let mut cursor: usize = 0;
    loop {
        if let Err(e) = watch_tick(&mut last_rehome, &mut breaker_until, &mut cursor) {
            eprintln!("clarify: tick failed: {e}");
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
    fn parse_context_pct_used_left_and_none() {
        // "N% context used" → used directly.
        assert_eq!(parse_context_pct("foo\n 92% context used \nbar"), Some(92));
        // Auto-compact "left" banner → inverted (used = 100 − left).
        assert_eq!(parse_context_pct("Context left until auto-compact: 8%"), Some(92));
        assert_eq!(parse_context_pct("3% context left"), Some(97));
        // Bare "N% context" assumed used.
        assert_eq!(parse_context_pct("55% context"), Some(55));
        // No context marker → None (a stray % elsewhere is ignored).
        assert_eq!(parse_context_pct("CPU 40% MEM 20%"), None);
        assert_eq!(parse_context_pct(""), None);
    }

    #[test]
    fn skip_meta_daemon_and_orchestrator_tabs() {
        // Named daemons / meta tabs.
        for n in ["⛑ brain", "aligator", "ta-scribe", "tichef", "the watcher"] {
            assert!(should_skip_rehome(n, Some("build/implementer")), "{n} must be skipped");
        }
        // Orchestrator / meta role, whatever the name.
        assert!(should_skip_rehome("team back", Some("kalpin-back:build/orchestrator")));
        assert!(should_skip_rehome("m-planner", Some("plan/planner")));
        // A plain worker with a mundane name → NOT skipped.
        assert!(!should_skip_rehome("team-back-worker", Some("build/implementer")));
        assert!(!should_skip_rehome("m-invoice", Some("review/reviewer")));
    }

    #[test]
    fn rehome_command_is_in_place_go_autoclose() {
        assert_eq!(
            rehome_command(
                "uuid-1",
                "/home/u/Dev/kalpin-back",
                "kalpin-back:build/implementer",
                "team back"
            ),
            vec![
                "uuid-1",
                "/home/u/Dev/kalpin-back",
                "kalpin-back:build/implementer",
                "team back",
                "--go",
                "--auto-close",
            ]
        );
    }

    #[test]
    fn refresh_storm_breaker_backs_off_on_systemic_spike() {
        let t0 = Instant::now();
        let mut b: Option<Instant> = None;
        // A few saturated tabs → refresh normally (no trip).
        assert!(!refresh_storm_open(&mut b, 2, t0));
        assert!(
            !refresh_storm_open(&mut b, REFRESH_STORM_THRESHOLD, t0),
            "at threshold → normal"
        );
        assert_eq!(b, None);
        // More than the threshold at once → systemic → trip + cooldown.
        assert!(refresh_storm_open(&mut b, REFRESH_STORM_THRESHOLD + 1, t0));
        assert!(b.is_some());
        // Mid-cooldown: suppressed even if the count drops.
        assert!(refresh_storm_open(&mut b, 1, t0 + Duration::from_secs(5)));
        // After the cooldown + spike gone → back to normal.
        assert!(!refresh_storm_open(&mut b, 2, t0 + REFRESH_STORM_COOLDOWN));
        assert_eq!(b, None);
    }
}
