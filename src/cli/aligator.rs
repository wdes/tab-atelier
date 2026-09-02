// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! 🐊 aligator — a deterministic input router (proof-of-concept for #35).
//!
//! Sibling of the ⛑ `brain` rescue tab. Where `brain` *reactively*
//! pattern-matches agent failures and injects `continue\r`, `aligator`
//! drains a typed **swamp** queue and types each entry's `input` into the
//! target tab via `POST /tabs/by-id/<uuid>/input`. A queue-driven,
//! cursor-based input router so any producer (a script, a peer agent, a cron)
//! can leave a message for tab X and have it delivered on the next round.
//!
//! Designed to run AS a tab, exactly like `brain`: `tab-atelier aligator`,
//! its log becomes the tab's scrollback (OSC-2 titled "🐊 aligator").
//!
//! **Swamp = a dedicated typed file** (`<state>/tab-atelier/swamp.jsonl`,
//! decided in #35 option B) — one JSON object per line, appended by the
//! `tab-atelier swamp <tab> "<text>"` producer. A dedicated typed file keeps
//! the routing key a real field (never a fragile `"<uuid> -> <text>"` split)
//! and decouples machine routing from the human-facing `note` blackboard.
//!
//! **Confused-deputy guard:** unlike `brain` (bounded to `continue\r`),
//! aligator types *arbitrary* text, so it only ever delivers to a **live
//! Claude agent tab** (`agent_kind == "claude"` + a non-empty
//! `agent_session_id`) — the same gate `brain` uses. It refuses to type into a
//! plain shell / human terminal. See #35 for the wider safety discussion.
//!
//! The I/O-free decision logic (arg parsing, per-round planning, line
//! encoding) is factored into pure functions so it's unit-tested without a
//! live daemon; `run`/`tick`/`run_swamp` are thin wrappers that add the HTTP
//! and filesystem effects.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cli::share_link::{Endpoint, agent, discover_endpoint};

const DEFAULT_INTERVAL_SECS: u64 = 5;
/// Bounded retry budget for a TRANSIENT skip — a target that IS (or should
/// become) an agent tab but the daemon hasn't detected live yet (startup /
/// resume / the restart-injection window). At the default 5s interval this is
/// ~30s of round-based backoff before the message is abandoned (and logged),
/// so a still-booting orchestrator's swamped input isn't lost on the first
/// round. A PERMANENT skip (daemon/shell) never retries — it's consumed at once.
const MAX_SKIP_ATTEMPTS: u32 = 6;
/// Delay before the submitting Enter, so the typed text is ingested as one
/// paste before `\r` lands (see the dispatch paste-submit fix, #31/#32). A
/// fixed floor here; a follow-up should reuse dispatch's settle poll.
const SUBMIT_DELAY: Duration = Duration::from_millis(400);

/// RS1 drain-priority class.
///
/// **Declaration order == drain order** (Block first): a round is stable-sorted
/// by this before the FIFO tie-break, so `block`/`verdict` land ahead of
/// `status`. `Directive` is the default so a legacy entry (no `priority` field)
/// keeps its FIFO slot among other legacy entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    /// An unblock/blocker nudge — drains first.
    Block,
    /// A supervisor verdict — ahead of routine traffic.
    Verdict,
    /// Routine directive — the neutral default (legacy FIFO position).
    #[default]
    Directive,
    /// Low-urgency status — drains last.
    Status,
}

impl Priority {
    /// Parse the `--priority` flag value (case-insensitive). `None` → caller errors.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "block" => Some(Self::Block),
            "verdict" => Some(Self::Verdict),
            "directive" => Some(Self::Directive),
            "status" => Some(Self::Status),
            _ => None,
        }
    }
}

/// One swamp entry — a request to type `input` into tab `tab`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwampEntry {
    /// Unix seconds when enqueued.
    pub ts: u64,
    /// Target tab UUID (routing key — a real field, not a parsed string).
    pub tab: String,
    /// Text to type into the tab's input.
    pub input: String,
    /// Whether to press Enter after the text (default true).
    #[serde(default = "default_true")]
    pub submit: bool,
    /// Who enqueued it, if given (audit / `--from`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Delivery attempts already spent on this entry (transient-skip retries).
    /// Absent/0 on a fresh enqueue; bumped each time it's re-queued for a target
    /// not yet live. Omitted from the JSONL when 0 to keep legacy lines clean.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub attempts: u32,
    /// RS1 drain priority. Omitted from the JSONL when default (`directive`) so
    /// legacy lines stay byte-clean and a round with no explicit priority drains
    /// FIFO.
    #[serde(default, skip_serializing_if = "is_default_priority")]
    pub priority: Priority,
    /// RS1 idempotency key. A later entry whose key was already delivered is
    /// dropped (dedup). `None` = unkeyed → always FIFO, never deduped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup_key: Option<String>,
}

const fn default_true() -> bool {
    true
}

// serde's `skip_serializing_if` mandates a `&T` predicate, hence the reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_default_priority(p: &Priority) -> bool {
    matches!(p, Priority::Directive)
}

// serde's `skip_serializing_if` mandates a `&T` predicate, hence the reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn state_file(name: &str) -> PathBuf {
    crate::platform::state_base_dir().join("tab-atelier").join(name)
}

/// The swamp producer file (`<state>/tab-atelier/swamp.jsonl`). `pub` so the
/// daemon's RA1 restart-wake can enqueue an orchestrator wake onto the same queue.
#[must_use]
pub fn swamp_path() -> PathBuf {
    state_file("swamp.jsonl")
}

fn cursor_path() -> PathBuf {
    state_file("aligator.cursor")
}

/// Parse a swamp body into entries, skipping blank / unparseable lines (a
/// half-written line from a racing appender is dropped, not fatal — same
/// tolerance as the `note` blackboard).
#[must_use]
pub fn parse_swamp(body: &str) -> Vec<SwampEntry> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<SwampEntry>(l).ok())
        .collect()
}

/// One swamp entry as a JSONL line (trailing newline included).
#[must_use]
pub fn encode_swamp_line(e: &SwampEntry) -> String {
    serde_json::to_string(e).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

/// The subset of a `/tabs` row the guard needs.
#[derive(Debug, Deserialize)]
struct TabInfo {
    id: String,
    /// Tab name — classifies meta/daemon targets (brain/aligator/scribe/…) as a
    /// PERMANENT skip so we never retry-spam them.
    #[serde(default)]
    name: String,
    #[serde(default)]
    agent_kind: Option<String>,
    #[serde(default)]
    agent_session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TabsResponse {
    tabs: Vec<TabInfo>,
}

/// A single tab is a **live Claude agent tab**: `agent_kind == "claude"` AND a
/// non-empty session id. The confused-deputy delivery gate — arbitrary text goes
/// only to a tab that expects programmatic input, never a shell/human terminal.
#[must_use]
fn is_live_claude(t: &TabInfo) -> bool {
    t.agent_kind.as_deref() == Some("claude") && !t.agent_session_id.as_deref().unwrap_or("").is_empty()
}

/// Why a swamp target was skipped this round — the fix for the restart-injection
/// drop (a message to a not-yet-live tab was consumed on the first miss).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipKind {
    /// The target IS (or should become) an agent tab but the daemon hasn't
    /// detected it live yet — startup / resume / the restart window. Retry
    /// bounded ([`MAX_SKIP_ATTEMPTS`]) before abandoning; do NOT drop on miss.
    Transient,
    /// The target is a meta/daemon tab (by name) or a plain persistent shell — it
    /// will never be a valid delivery target. Consume now (confused-deputy).
    Permanent,
}

/// The guard verdict for one target this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// A live Claude tab — deliver.
    Deliver,
    /// Not deliverable this round — skip, transiently (retry) or permanently.
    Skip(SkipKind),
}

/// Classify a swamp target: deliver, transient-skip (retry), or permanent-skip
/// (consume). The caller chose `uuid` deliberately, so a target that merely
/// isn't live YET (a Claude tab whose session hasn't re-attached after a
/// restart, or a tab the daemon hasn't listed yet) is TRANSIENT — retried, not
/// dropped. A meta/daemon name or a plain shell (no `agent_kind`) is PERMANENT.
/// Pure — the tab list is fetched in [`tick`].
#[must_use]
fn classify_target(tabs: &[TabInfo], uuid: &str) -> Guard {
    let Some(t) = tabs.iter().find(|t| t.id == uuid) else {
        // Ghost: a deliberately-targeted uuid the daemon doesn't list (yet) — a
        // spawn/restart gap. Bounded retry, then abandon; never a silent drop.
        return Guard::Skip(SkipKind::Transient);
    };
    if is_live_claude(t) {
        return Guard::Deliver;
    }
    // A meta/daemon tab must never be retry-spammed (confused-deputy), even if it
    // somehow carries an agent_kind. Name-only (an orchestrator ROLE is a legit
    // target and stays transient).
    if crate::cli::clarify::is_meta_daemon_name(&t.name) {
        return Guard::Skip(SkipKind::Permanent);
    }
    // TRANSIENT = an agent tab (durable `agent_kind`) whose session hasn't
    // attached YET — starting / resuming / the restart-injection window; it may
    // still become a live Claude target, so retry. Anything else — a live
    // NON-Claude agent (session up but wrong kind, never a Claude target) or a
    // plain shell (no agent_kind) — is PERMANENT: consume now.
    let starting_agent = t.agent_kind.is_some() && t.agent_session_id.as_deref().unwrap_or("").is_empty();
    if starting_agent {
        Guard::Skip(SkipKind::Transient)
    } else {
        Guard::Skip(SkipKind::Permanent)
    }
}

/// Should a transient skip / unconfirmed submission be retried, given the tries
/// its entry already spent?
///
/// Keep re-queuing while under [`MAX_SKIP_ATTEMPTS`], else abandon. Pure boundary
/// so the "N tries then give up" bound is unit-tested without a live daemon.
/// Shared budget: transient skips and ⏎-submit retries both spend `attempts`.
#[must_use]
pub const fn should_retry(attempts: u32) -> bool {
    attempts + 1 < MAX_SKIP_ATTEMPTS
}

/// The re-queue entry for an UNCONFIRMED ⏎ submission (gap 'a' fix).
///
/// The text already landed — only the newline needs retrying — so the copy has
/// EMPTY input (never re-types the body), `submit = true`, and `attempts + 1`.
/// Pure, so the "a ⏎ retry never re-types the text" invariant is unit-testable.
#[must_use]
fn submit_retry_entry(e: &SwampEntry) -> SwampEntry {
    SwampEntry {
        input: String::new(),
        submit: true,
        attempts: e.attempts + 1,
        ..e.clone()
    }
}

/// Clamp a persisted cursor to `[0, len]` — if the swamp was truncated or
/// rotated under us, a stale cursor past the end must not skip fresh entries
/// (reset to the new end) nor panic on the slice.
#[must_use]
pub fn clamp_cursor(cursor: usize, len: usize) -> usize {
    cursor.min(len)
}

/// The entries a compaction KEEPS: the undelivered tail (`entries[cursor..]`).
///
/// The swamp file is rewritten to just these and the cursor reset to 0, so the
/// file stays bounded AND a future cursor reset can't re-deliver already-
/// delivered entries — there are none left to re-deliver. Pure, so the "no
/// re-delivery / no loss / coherent cursor" invariants are unit-testable.
#[must_use]
pub fn compact(entries: &[SwampEntry], cursor: usize) -> Vec<SwampEntry> {
    let start = clamp_cursor(cursor, entries.len());
    entries[start..].to_vec()
}

/// Options parsed from `aligator [--once] [--interval SECS]`.
#[derive(Debug, PartialEq, Eq)]
pub struct RunOpts {
    pub once: bool,
    pub interval: u64,
}

/// Pure arg parser for `run`, so its branch logic is testable without looping.
/// Returns the options, or an exit code (`0` for `--help`, `2` for a bad arg)
/// with the message already printed.
///
/// # Errors
/// `Err(0)` on `-h`/`--help` (usage printed), `Err(2)` on an unknown argument
/// or a non-numeric / zero `--interval`.
pub fn parse_run_opts(args: &[String]) -> Result<RunOpts, i32> {
    let mut opts = RunOpts {
        once: false,
        interval: DEFAULT_INTERVAL_SECS,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--once" => opts.once = true,
            "--interval" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                    Some(n) if n >= 1 => opts.interval = n,
                    _ => {
                        eprintln!("aligator: --interval expects a number >= 1");
                        return Err(2);
                    }
                }
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: tab-atelier aligator [--once] [--interval SECS]\n\
                     Drains the swamp queue and types each entry's input into the target\n\
                     tab. Delivers ONLY to a live Claude agent tab (agent_kind == \"claude\"\n\
                     + a session) — never a plain shell. Cursor-based (exactly-once best\n\
                     effort), one round every {DEFAULT_INTERVAL_SECS}s by default.\n\
                     Enqueue with: tab-atelier swamp <tab-uuid> \"<text>\" [--no-submit]"
                );
                return Err(0);
            }
            other => {
                eprintln!("aligator: unknown argument: {other}");
                return Err(2);
            }
        }
        i += 1;
    }
    Ok(opts)
}

/// What to do with one swamp entry this round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Deliver `input` (+ Enter if `submit`) to a live Claude tab.
    Deliver {
        index: usize,
        tab: String,
        input: String,
        submit: bool,
    },
    /// Target isn't a live Claude tab — skip. `kind` decides whether `tick`
    /// retries (transient) or consumes it (permanent).
    Skip { index: usize, tab: String, kind: SkipKind },
    /// RS1 dedup — the entry's `dedup_key` was already delivered; consume WITHOUT
    /// re-delivering (idempotency). Distinct from `Skip`: never retried, no
    /// re-queue. Only ever downgraded from a `Deliver` (the gate still wins).
    Drop { index: usize, tab: String, key: String },
}

/// Plan one round: classify every entry past `cursor` into deliver / skip.
///
/// Pure — the HTTP calls (fetch `/tabs`, POST input) live in [`tick`]; this is
/// the routing logic, unit-tested with a fake classifier. `classify` returns the
/// [`Guard`] verdict for a target uuid; `index` is the entry's absolute position.
#[must_use]
pub fn plan_round(entries: &[SwampEntry], cursor: usize, classify: impl Fn(&str) -> Guard) -> Vec<Decision> {
    let start = clamp_cursor(cursor, entries.len());
    entries[start..]
        .iter()
        .enumerate()
        .map(|(offset, e)| {
            let index = start + offset;
            match classify(&e.tab) {
                Guard::Deliver => Decision::Deliver {
                    index,
                    tab: e.tab.clone(),
                    input: e.input.clone(),
                    submit: e.submit,
                },
                Guard::Skip(kind) => Decision::Skip {
                    index,
                    tab: e.tab.clone(),
                    kind,
                },
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// RS1 — drain heuristics (fabric v2). All PURE + unit-tested; each active only
// when its signal is present, else a no-op that leaves the drain byte-identical
// to the legacy FIFO (invariant a) and fails open to it (invariant b).
// ---------------------------------------------------------------------------

/// Max deliveries to a single target within one round before the rest wait for
/// the next round (rate-limit window). ponytail: the window is one round; a
/// cross-round token-bucket + jitter (cf. api-ratelimit-analysis) is the upgrade.
const RATE_LIMIT_PER_ROUND: usize = 3;

/// Does the round carry ANY RS1 signal (a non-default priority or a dedup key)?
///
/// A round of purely-legacy entries returns false → the heuristic layer is
/// skipped entirely → byte-identical FIFO (invariant a).
#[must_use]
pub fn round_has_rs1_signal(candidates: &[SwampEntry]) -> bool {
    candidates
        .iter()
        .any(|e| !matches!(e.priority, Priority::Directive) || e.dedup_key.is_some())
}

/// RS1 dedup predicate: is `key` one already delivered? An unkeyed entry
/// (`None`) is never a duplicate — it always drains FIFO.
#[must_use]
#[allow(clippy::implicit_hasher)] // callers use the default hasher
pub fn is_duplicate(key: Option<&str>, delivered: &std::collections::HashSet<String>) -> bool {
    key.is_some_and(|k| delivered.contains(k))
}

/// RS1 priority order: stable-sort candidate INDICES so higher-urgency classes
/// drain first, FIFO within a class. All-default input → identity `[0,1,2,…]`
/// (byte-identical FIFO — invariant a). Pure.
#[must_use]
pub fn priority_order(candidates: &[SwampEntry]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..candidates.len()).collect();
    // `sort_by_key` is stable → equal priority keeps the FIFO (index) order.
    idx.sort_by_key(|&i| candidates[i].priority);
    idx
}

/// RS1 rate-limit: has `target` already hit its per-round cap? Pure — the caller
/// tracks the running count.
#[must_use]
pub const fn rate_limited(delivered_to_target: usize, limit: usize) -> bool {
    delivered_to_target >= limit
}

/// RS1 anti-ping-pong: would delivering to `target` continue an A→B→A oscillation?
///
/// True when the last two deliveries were `(target, other)` — i.e. `target` is
/// bouncing back right after a hop to a different tab. `recent` = delivery
/// targets in order, most-recent last. Pure.
#[must_use]
pub fn is_ping_pong(target: &str, recent: &[String]) -> bool {
    let n = recent.len();
    n >= 2 && recent[n - 2] == target && recent[n - 1] != target
}

/// RS1 delivery-heuristics pass: post-process a FIFO plan into the drained plan.
///
/// Priority ordering is applied UPSTREAM (the candidates are pre-sorted by
/// [`priority_order`] before planning), so this only *downgrades* `Deliver`s:
/// dedup → [`Decision::Drop`]; rate-limit / ping-pong → `Skip(Transient)` (defer
/// & re-queue via the existing path). A `Skip` is never promoted — the
/// confused-deputy gate always wins (invariant c). With no dedup ledger and no
/// rate/ping-pong trip the plan passes through unchanged (invariant a/b).
#[must_use]
#[allow(clippy::implicit_hasher)] // callers use the default hasher
pub fn apply_delivery_heuristics(
    plan: Vec<Decision>,
    entries: &[SwampEntry],
    delivered: &std::collections::HashSet<String>,
    rate_limit: usize,
) -> Vec<Decision> {
    // Gate: a round with no RS1 signal is left byte-identical (invariant a) — the
    // target-based heuristics (rate-limit/ping-pong) are opt-in, so a pure legacy
    // flood still drains FIFO. ponytail: to regulate legacy floods, a producer
    // tags the entries (priority/dedup) → the round opts into the layer.
    if !round_has_rs1_signal(entries) {
        return plan;
    }
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut recent: Vec<String> = Vec::new();
    plan.into_iter()
        .map(|d| match d {
            Decision::Deliver {
                index,
                tab,
                input,
                submit,
            } => {
                let key = entries.get(index).and_then(|e| e.dedup_key.as_deref());
                if is_duplicate(key, delivered) {
                    return Decision::Drop {
                        index,
                        tab,
                        key: key.unwrap_or_default().to_string(),
                    };
                }
                let already = counts.get(&tab).copied().unwrap_or(0);
                if rate_limited(already, rate_limit) || is_ping_pong(&tab, &recent) {
                    // Defer to a later round — reuse the transient re-queue path.
                    return Decision::Skip {
                        index,
                        tab,
                        kind: SkipKind::Transient,
                    };
                }
                *counts.entry(tab.clone()).or_insert(0) += 1;
                recent.push(tab.clone());
                Decision::Deliver {
                    index,
                    tab,
                    input,
                    submit,
                }
            }
            // A Skip/Drop is never upgraded — the gate wins (invariant c).
            other => other,
        })
        .collect()
}

/// Physically reorder the undelivered swamp tail by [`priority_order`] (stable)
/// so the cursor still advances monotonically while higher-priority entries drain
/// first. Rewrites the file to `prefix ++ reordered-tail` (reusing the atomic
/// tmp+rename of [`compact_swamp`]) ONLY when the order changes; an already-sorted
/// tail is left untouched. Best-effort: a write failure returns the ORIGINAL
/// order (fail-open — invariant b). Returns the (possibly reordered) entry vec.
fn prioritize_tail(path: &Path, entries: Vec<SwampEntry>, cursor: usize) -> Vec<SwampEntry> {
    let start = clamp_cursor(cursor, entries.len());
    let order = priority_order(&entries[start..]);
    if order.iter().enumerate().all(|(i, &j)| i == j) {
        return entries; // already in priority order → no rewrite
    }
    let mut reordered = entries[..start].to_vec();
    reordered.extend(order.iter().map(|&j| entries[start + j].clone()));
    let body: String = reordered.iter().map(encode_swamp_line).collect();
    let tmp = path.with_extension("jsonl.tmp");
    if std::fs::write(&tmp, &body).is_ok() && std::fs::rename(&tmp, path).is_ok() {
        reordered
    } else {
        let _ = std::fs::remove_file(&tmp);
        entries // fail-open: keep the original FIFO order
    }
}

/// The dedup ledger path (delivered keys, one per line — like the cursor file).
fn dedup_path() -> PathBuf {
    state_file("aligator.dedup")
}

/// Load the delivered-dedup-key ledger (missing/unreadable → empty; fail-open).
fn load_delivered(path: &Path) -> std::collections::HashSet<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// Record a delivered dedup key (append; best-effort — a failure just risks a
/// future re-delivery, never a crash). ponytail: append-only → grows unbounded;
/// upgrade = prune alongside the swamp compaction.
fn record_delivered(path: &Path, key: &str) {
    if key.is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{key}");
    }
}

/// The three state files a drain round reads/writes, bundled so the core
/// [`drain`] is injectable for the integration test (real HTTP delivery against a
/// loopback server, hermetic temp files — no global-env fiddling). Production
/// uses [`Paths::production`]; the test points them at a `tempdir`.
struct Paths {
    swamp: PathBuf,
    dedup: PathBuf,
    cursor: PathBuf,
}

impl Paths {
    fn production() -> Self {
        Self {
            swamp: swamp_path(),
            dedup: dedup_path(),
            cursor: cursor_path(),
        }
    }
}

fn read_cursor(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_cursor(path: &Path, n: usize) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, n.to_string());
}

/// Compact the swamp file after a fully-drained round: re-read it (to keep any
/// entry a producer appended DURING the round), rewrite it atomically to just
/// the undelivered tail past `*cursor`, then reset the cursor to 0. Bounds the
/// file and removes the "cursor reset → re-deliver stale" hazard. `*cursor` is
/// left at 0 to match the shortened file.
///
/// `ponytail:` a producer append landing in the tiny window between the re-read
/// and the rename is lost — inherent to any read-modify-write on the append log
/// without a lock; the window is a few syscalls wide. Upgrade = an flock.
fn compact_swamp(swamp: &Path, cursor_file: &Path, cursor: &mut usize) {
    let fresh = parse_swamp(&std::fs::read_to_string(swamp).unwrap_or_default());
    let kept = compact(&fresh, *cursor);
    let body: String = kept.iter().map(encode_swamp_line).collect();
    let tmp = swamp.with_extension("jsonl.tmp");
    if std::fs::write(&tmp, &body).is_ok() && std::fs::rename(&tmp, swamp).is_ok() {
        *cursor = 0;
        write_cursor(cursor_file, 0);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Aligator's startup self-announce decision + payload. A programmatically
/// launched aligator (the restart-watcher types `tab-atelier aligator` into a
/// shell) lands on a tab with `agent_kind = None`, so the daemon never
/// resurrects it on restart — unlike a menu-launched brain, which the menu
/// stamps `agent_kind = "brain"`. Posting `agentKind:"aligator"` on our own tab
/// closes that gap.
///
/// Given the `_TAB_ID` env value, returns `Some((tab_id, body))` inside a tab or
/// `None` outside one (silent no-op — a shell rc running aligator out of a tab
/// mustn't error). Session-less on purpose: aligator holds no agent session, so
/// the body omits `sessionId` and never clobbers one. Pure — the HTTP is the
/// caller's, kept best-effort.
#[must_use]
fn self_announce(tab_id: Option<&str>) -> Option<(String, String)> {
    let tab_id = tab_id.map(str::trim).filter(|s| !s.is_empty())?;
    let body = serde_json::json!({ "state": "thinking", "agentKind": "aligator" }).to_string();
    Some((tab_id.to_string(), body))
}

fn send_input(ep: &Endpoint, uuid: &str, bytes: &[u8]) -> Result<(), String> {
    agent()
        .post(format!("{}/tabs/by-id/{uuid}/input", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/octet-stream")
        .send(bytes)
        .map_err(|e| format!("POST input for {uuid}: {e}"))?;
    Ok(())
}

/// One round: read new swamp entries past the cursor, plan deliver/skip
/// (guarded), execute deliveries over HTTP, advance the cursor after each
/// (exactly-once best effort — a crash mid-round re-reads from the persisted
/// cursor).
///
/// Thin wrapper: cheap idle short-circuit (no endpoint / no HTTP when the queue
/// has nothing new), then delegate the regulated drain to [`drain`] against the
/// production state files.
fn tick(cursor: &mut usize) -> Result<(), String> {
    let paths = Paths::production();
    let entries = parse_swamp(&std::fs::read_to_string(&paths.swamp).unwrap_or_default());
    *cursor = clamp_cursor(*cursor, entries.len());
    if *cursor >= entries.len() {
        return Ok(()); // nothing new this round → don't even resolve the endpoint
    }
    let ep = discover_endpoint()?;
    drain(&ep, &paths, cursor)
}

/// The regulated drain core — injectable for the integration test (real HTTP
/// delivery against a loopback server + hermetic temp `paths`, no global env).
/// Reads `paths.swamp`, plans the round (RS1 dedup / rate-limit / anti-ping-pong
/// when the round carries a signal, else byte-identical FIFO), executes the
/// deliveries over HTTP against `ep`, advancing the cursor after each, and
/// compacts the tail once fully drained.
fn drain(ep: &Endpoint, paths: &Paths, cursor: &mut usize) -> Result<(), String> {
    let body = std::fs::read_to_string(&paths.swamp).unwrap_or_default();
    let entries = parse_swamp(&body);
    *cursor = clamp_cursor(*cursor, entries.len());
    if *cursor >= entries.len() {
        return Ok(()); // nothing new this round
    }

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

    // RS1 fabric v2: when the round carries any priority/dedup signal, drain
    // through the heuristic layer — priority-reorder the tail (cursor stays
    // monotonic), then dedup / rate-limit / anti-ping-pong the plan. A purely-
    // legacy round skips the whole layer → byte-identical FIFO (invariant a); any
    // failure in the layer falls back to the bare FIFO plan (invariant b).
    let has_signal = round_has_rs1_signal(&entries[clamp_cursor(*cursor, entries.len())..]);
    let entries = if has_signal {
        prioritize_tail(&paths.swamp, entries, *cursor)
    } else {
        entries
    };
    let fifo = plan_round(&entries, *cursor, |uuid| classify_target(&tabs.tabs, uuid));
    let plan = if has_signal {
        apply_delivery_heuristics(fifo, &entries, &load_delivered(&paths.dedup), RATE_LIMIT_PER_ROUND)
    } else {
        fifo
    };
    for decision in plan {
        match decision {
            // RS1 dedup: the key was already delivered — consume WITHOUT re-typing
            // (idempotency). Record the key (idempotent) so a later duplicate is
            // dropped too. Never re-queued (distinct from a transient skip).
            Decision::Drop { index, tab, key } => {
                println!("🐊 aligator: DEDUP {tab} — key '{key}' already delivered, dropped");
                record_delivered(&paths.dedup, &key);
                *cursor = index + 1;
                write_cursor(&paths.cursor, *cursor);
            }
            Decision::Deliver {
                index,
                tab,
                input,
                submit,
            } => {
                // 1. Type the text. Empty input = a re-queued submit-only ⏎ nudge
                //    (see the gap-'a' fix below), so skip the POST entirely.
                if !input.is_empty()
                    && let Err(e) = send_input(ep, &tab, input.as_bytes())
                {
                    eprintln!("🐊 aligator: deliver failed for {tab}: {e}");
                    // Nothing typed yet → leave the cursor before this entry;
                    // retry the whole entry next round (no double-type).
                    return Ok(());
                }
                // 2. Submit the ⏎ ATOMICALLY w.r.t. the cursor (gap 'a' fix): the
                //    entry is consumed only once the newline POST is CONFIRMED.
                //    "Confirmed" = the input POST returned Ok — the daemon accepted
                //    the `\r` byte into the tab's PTY. ponytail: best-effort — there
                //    is no agent-side ack that the line was actually executed; this
                //    is the most reliable signal the input API exposes, but it's no
                //    longer fire-and-forget.
                if submit {
                    std::thread::sleep(SUBMIT_DELAY);
                    if let Err(e) = send_input(ep, &tab, b"\r") {
                        // ⏎ unconfirmed: DON'T drop the nudge. Re-queue a submit-only
                        // copy (empty text so the body isn't re-typed) with
                        // attempts+1, bounded by should_retry, then abandon+log.
                        // Advancing here mirrors the transient-skip re-queue: the
                        // pending submission survives as a tail entry (kept by the
                        // end-of-round compaction), later live tabs still deliver.
                        let spent = entries[index].attempts;
                        *cursor = index + 1;
                        write_cursor(&paths.cursor, *cursor);
                        if should_retry(spent) {
                            match append_swamp_line(&paths.swamp, &submit_retry_entry(&entries[index])) {
                                Ok(()) => eprintln!(
                                    "🐊 aligator: ⏎ not confirmed for {tab}: {e} — re-queued (attempt {n}/{MAX_SKIP_ATTEMPTS})",
                                    n = spent + 1,
                                ),
                                Err(w) => eprintln!("🐊 aligator: re-queue ⏎ for {tab} failed: {w} — dropping"),
                            }
                        } else {
                            eprintln!(
                                "🐊 aligator: ABANDON ⏎ for {tab} — submission unconfirmed after {MAX_SKIP_ATTEMPTS} attempts; dropping"
                            );
                        }
                        continue;
                    }
                }
                // 3. Delivered (text ok + ⏎ confirmed, or no submit asked): consume.
                //    Inc8 S4: bump the target's usage (observability — who's being
                //    fed from the swamp). Best-effort; never blocks the consume.
                crate::cli::share_link::bump_usage(ep, &tab);
                // RS1: record this entry's dedup_key so a later duplicate is
                // dropped (idempotency). No-op for an unkeyed (legacy) entry.
                if let Some(k) = entries[index].dedup_key.as_deref() {
                    record_delivered(&paths.dedup, k);
                }
                println!(
                    "🐊 aligator: {tab:<36} ← {n} byte(s){s}",
                    n = input.len(),
                    s = if submit { " + ⏎" } else { "" },
                );
                *cursor = index + 1;
                write_cursor(&paths.cursor, *cursor);
            }
            // Permanent: a daemon/shell target — consume it (confused-deputy).
            Decision::Skip {
                index,
                tab,
                kind: SkipKind::Permanent,
            } => {
                println!("🐊 aligator: SKIP {tab} — meta/daemon or plain shell (confused-deputy guard), consumed");
                *cursor = index + 1;
                write_cursor(&paths.cursor, *cursor);
            }
            // Transient: an agent tab not live YET. Consume THIS position but
            // re-queue a copy (attempts+1) so it's retried a later round instead
            // of dropped — the restart-injection fix. Advancing the cursor keeps
            // the queue non-blocking (later live tabs still get delivered now);
            // the re-appended copy is preserved by the end-of-round compaction.
            Decision::Skip {
                index,
                tab,
                kind: SkipKind::Transient,
            } => {
                let spent = entries[index].attempts;
                *cursor = index + 1;
                write_cursor(&paths.cursor, *cursor);
                if should_retry(spent) {
                    let mut retry = entries[index].clone();
                    retry.attempts = spent + 1;
                    match append_swamp_line(&paths.swamp, &retry) {
                        Ok(()) => println!(
                            "🐊 aligator: retry {tab} — not live yet (attempt {n}/{MAX_SKIP_ATTEMPTS})",
                            n = spent + 1,
                        ),
                        // Re-queue write failed: better a dropped retry than a loop.
                        Err(e) => eprintln!("🐊 aligator: re-queue {tab} failed: {e} — dropping"),
                    }
                } else {
                    eprintln!(
                        "🐊 aligator: ABANDON {tab} — still not a live Claude tab after {MAX_SKIP_ATTEMPTS} attempts; dropping message"
                    );
                }
            }
        }
    }
    // Round fully drained (a deliver failure returns early, before this): compact
    // the swamp to its undelivered tail so it stays bounded and a future cursor
    // reset can't re-deliver already-delivered entries.
    compact_swamp(&paths.swamp, &paths.cursor, cursor);
    Ok(())
}

/// Append `brain-crash.log`-style trace when a tick panics.
fn crash_log(msg: &str) {
    let path = state_file("aligator-crash.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{msg}");
    }
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let opts = match parse_run_opts(args) {
        Ok(o) => o,
        Err(code) => return code,
    };

    print!("\x1b]2;\u{1f40a} aligator\x07");
    println!(
        "\u{1f40a} aligator — draining {swamp} every {interval}s (Claude-tab guard on)",
        swamp = swamp_path().display(),
        interval = opts.interval,
    );

    // Durability: announce our own agent_kind on our tab so the daemon's
    // restart-watcher resurrects us on restart (a programmatic launch leaves
    // agent_kind=None otherwise; the GUI menu stamps brain, never aligator).
    // Best-effort — a missing endpoint or a network failure must NEVER stop the
    // drain (same discipline as brain). Runs once at startup, before the loop.
    if let Some((tab_id, body)) = self_announce(std::env::var("_TAB_ID").ok().as_deref())
        && let Ok(ep) = discover_endpoint()
    {
        let _ = agent()
            .post(format!("{}/tabs/by-id/{tab_id}/status", ep.url))
            .header("Authorization", format!("Bearer {}", ep.token))
            .header("Content-Type", "application/json")
            .send(body.as_str());
    }

    let mut cursor = read_cursor(&cursor_path());
    loop {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tick(&mut cursor)));
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("🐊 aligator: round failed: {e}"),
            Err(panic) => {
                let msg = panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("(non-string panic payload)");
                crash_log(&format!("tick panicked (recovered): {msg}"));
                let _ = std::io::Write::write_all(
                    &mut std::io::stderr(),
                    format!("🐊 aligator: tick PANICKED, recovered: {msg}\n").as_bytes(),
                );
            }
        }
        if opts.once {
            return 0;
        }
        std::thread::sleep(Duration::from_secs(opts.interval));
    }
}

/// A parsed `swamp` producer request (minus the timestamp, stamped at write).
#[derive(Debug, PartialEq, Eq)]
pub struct SwampArgs {
    pub tab: String,
    pub input: String,
    pub submit: bool,
    pub from: Option<String>,
    /// RS1 drain priority (`--priority`), default `directive`.
    pub priority: Priority,
    /// RS1 idempotency key (`--dedup-key`), `None` when unset.
    pub dedup_key: Option<String>,
}

/// Pure arg parser for
/// `swamp <tab> "<text>" [--no-submit] [--from NAME] [--priority P] [--dedup-key K]`.
///
/// # Errors
/// `Err(0)` on `-h`/`--help` (usage printed), `Err(2)` on a missing tab/text, an
/// unexpected extra argument, or an unknown `--priority` value.
pub fn parse_swamp_args(args: &[String]) -> Result<SwampArgs, i32> {
    let mut tab: Option<String> = None;
    let mut input: Option<String> = None;
    let mut submit = true;
    let mut from: Option<String> = None;
    let mut priority = Priority::default();
    let mut dedup_key: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-submit" => submit = false,
            "--from" => from = it.next().cloned(),
            "--priority" => {
                let Some(p) = it.next().and_then(|v| Priority::parse(v)) else {
                    eprintln!("swamp: --priority expects block|verdict|directive|status");
                    return Err(2);
                };
                priority = p;
            }
            "--dedup-key" => dedup_key = it.next().cloned(),
            "-h" | "--help" => {
                eprintln!(
                    "usage: tab-atelier swamp <tab-uuid> \"<text>\" [--no-submit] [--from NAME] [--priority block|verdict|directive|status] [--dedup-key KEY]"
                );
                return Err(0);
            }
            other if tab.is_none() => tab = Some(other.to_string()),
            other if input.is_none() => input = Some(other.to_string()),
            other => {
                eprintln!("swamp: unexpected argument: {other}");
                return Err(2);
            }
        }
    }
    if let (Some(tab), Some(input)) = (tab, input) {
        Ok(SwampArgs {
            tab,
            input,
            submit,
            from,
            priority,
            dedup_key,
        })
    } else {
        eprintln!("swamp: usage: tab-atelier swamp <tab-uuid> \"<text>\" [--no-submit]");
        Err(2)
    }
}

/// Append one entry to a swamp file (create + append, line-atomic like the
/// `note` blackboard). Path-injectable so it's testable against a temp file.
///
/// # Errors
/// Propagates any create / write I/O error.
pub fn append_swamp_line(path: &Path, entry: &SwampEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(encode_swamp_line(entry).as_bytes())
}

/// `tab-atelier swamp <tab-uuid> "<text>" [--no-submit] [--from NAME]` — the
/// producer: append one entry to the swamp for aligator to deliver.
#[must_use]
pub fn run_swamp(args: &[String]) -> i32 {
    let parsed = match parse_swamp_args(args) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let entry = SwampEntry {
        ts,
        tab: parsed.tab.clone(),
        input: parsed.input.clone(),
        submit: parsed.submit,
        from: parsed.from,
        attempts: 0,
        priority: parsed.priority,
        dedup_key: parsed.dedup_key,
    };
    match append_swamp_line(&swamp_path(), &entry) {
        Ok(()) => {
            println!(
                "🐊 swamped → {tab} ({n} byte(s){s})",
                tab = parsed.tab,
                n = parsed.input.len(),
                s = if parsed.submit { " + ⏎" } else { "" },
            );
            0
        }
        Err(e) => {
            eprintln!("swamp: write {}: {e}", swamp_path().display());
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: &str, kind: Option<&str>, session: Option<&str>) -> TabInfo {
        // name defaults to the id — daemon-name tests pass id == "aligator" etc.
        TabInfo {
            id: id.into(),
            name: id.into(),
            agent_kind: kind.map(Into::into),
            agent_session_id: session.map(Into::into),
        }
    }

    fn entry(tab: &str, input: &str, submit: bool) -> SwampEntry {
        SwampEntry {
            ts: 0,
            tab: tab.into(),
            input: input.into(),
            submit,
            from: None,
            attempts: 0,
            priority: Priority::default(),
            dedup_key: None,
        }
    }

    /// Like [`entry`] but with an explicit priority + dedup key (RS1 tests).
    fn tagged(tab: &str, priority: Priority, dedup_key: Option<&str>) -> SwampEntry {
        SwampEntry {
            priority,
            dedup_key: dedup_key.map(Into::into),
            ..entry(tab, "x", true)
        }
    }

    #[test]
    fn compact_keeps_undelivered_drops_delivered_and_is_cursor_coherent() {
        let entries = vec![
            entry("t0", "a", true), // 0 delivered
            entry("t1", "b", true), // 1 delivered
            entry("t2", "c", true), // 2 UNdelivered
            entry("t3", "d", true), // 3 UNdelivered
        ];
        let cursor = 2; // delivered [0,1], undelivered [2,3]
        let kept = compact(&entries, cursor);
        // (b) no loss of undelivered — exactly the tail survives, in order.
        assert_eq!(kept, entries[2..].to_vec());
        // (a) no re-delivery of delivered — a re-plan from the reset cursor never
        //     touches t0/t1 (they're gone from the compacted file).
        let plan = plan_round(&kept, 0, |_| Guard::Deliver);
        let replanned: Vec<&str> = plan
            .iter()
            .filter_map(|d| match d {
                Decision::Deliver { tab, .. } => Some(tab.as_str()),
                Decision::Skip { .. } | Decision::Drop { .. } => None,
            })
            .collect();
        assert_eq!(replanned, vec!["t2", "t3"], "only the undelivered are re-planned");
        assert!(
            !replanned.contains(&"t0") && !replanned.contains(&"t1"),
            "delivered entries are never re-delivered after compaction"
        );
        // (c) coherent cursor — cursor 0 on the shortened file addresses the
        //     first undelivered entry (t2); no off-by-one, nothing skipped.
        assert_eq!(clamp_cursor(0, kept.len()), 0);
        assert_eq!(kept.first().map(|e| e.tab.as_str()), Some("t2"));
    }

    #[test]
    fn compact_of_fully_drained_round_is_empty_and_clamps() {
        let entries = vec![entry("t0", "a", true), entry("t1", "b", true)];
        // Everything delivered (cursor == len) → the compacted file is empty.
        assert!(compact(&entries, 2).is_empty());
        // A stale over-the-end cursor clamps instead of panicking on the slice.
        assert!(compact(&entries, 99).is_empty());
    }

    #[test]
    fn guard_delivers_only_to_live_claude_tabs() {
        // The confused-deputy gate, expressed through the production classifier:
        // ONLY a live Claude tab (claude + session) is a Deliver — everything else
        // is a Skip. (The transient-vs-permanent split is asserted separately in
        // `classify_target_splits_transient_from_permanent`.)
        let tabs = vec![
            tab("claude-live", Some("claude"), Some("sess-1")),
            tab("claude-nosession", Some("claude"), None),
            tab("shell", None, None),
            tab("catbus", Some("catbus"), Some("s2")),
        ];
        assert_eq!(classify_target(&tabs, "claude-live"), Guard::Deliver);
        assert!(matches!(classify_target(&tabs, "claude-nosession"), Guard::Skip(_)));
        assert!(matches!(classify_target(&tabs, "shell"), Guard::Skip(_)));
        assert!(matches!(classify_target(&tabs, "catbus"), Guard::Skip(_)));
        assert!(matches!(classify_target(&tabs, "ghost"), Guard::Skip(_)));
    }

    #[test]
    fn classify_target_splits_transient_from_permanent() {
        let tabs = vec![
            tab("claude-live", Some("claude"), Some("sess-1")),
            // A Claude tab whose session hasn't re-attached — the restart-injection
            // case: still becoming live → TRANSIENT (retry, don't drop).
            tab("claude-booting", Some("claude"), None),
            // A live NON-Claude agent (session up, wrong kind) — never a Claude
            // target → PERMANENT.
            tab("catbus-live", Some("catbus"), Some("s2")),
            // A plain persistent shell (no agent_kind) → PERMANENT (confused-deputy).
            tab("shell", None, None),
            // A daemon by NAME, even mid-boot → PERMANENT (never retry-spam it).
            tab("aligator", Some("claude"), None),
        ];
        assert_eq!(classify_target(&tabs, "claude-live"), Guard::Deliver);
        assert_eq!(
            classify_target(&tabs, "claude-booting"),
            Guard::Skip(SkipKind::Transient),
            "a resuming Claude tab is retried, not dropped (restart-injection fix)"
        );
        assert_eq!(classify_target(&tabs, "catbus-live"), Guard::Skip(SkipKind::Permanent));
        assert_eq!(classify_target(&tabs, "shell"), Guard::Skip(SkipKind::Permanent));
        assert_eq!(
            classify_target(&tabs, "aligator"),
            Guard::Skip(SkipKind::Permanent),
            "a meta/daemon name is permanent even with agent_kind=claude"
        );
        // A deliberately-targeted uuid the daemon doesn't list yet (spawn/restart
        // gap) is TRANSIENT — bounded retry, never a silent first-round drop.
        assert_eq!(classify_target(&tabs, "ghost"), Guard::Skip(SkipKind::Transient));
    }

    #[test]
    fn parse_swamp_skips_blank_and_garbage_lines() {
        let body = "\
{\"ts\":1,\"tab\":\"a\",\"input\":\"hi\",\"submit\":true}

not json at all
{\"ts\":2,\"tab\":\"b\",\"input\":\"yo\"}
";
        let e = parse_swamp(body);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].tab, "a");
        assert_eq!(e[0].input, "hi");
        assert!(e[1].submit); // defaults true when omitted
        assert_eq!(e[1].tab, "b");
    }

    #[test]
    fn encode_roundtrips_through_parse() {
        let e = SwampEntry {
            ts: 42,
            tab: "uuid-x".into(),
            input: "run the tests".into(),
            submit: false,
            from: Some("bot-orc".into()),
            attempts: 3, // a re-queued entry roundtrips its retry counter
            priority: Priority::Block,
            dedup_key: Some("verdict-42".into()),
        };
        let line = encode_swamp_line(&e);
        assert!(line.ends_with('\n'));
        assert_eq!(parse_swamp(&line), vec![e]);
    }

    #[test]
    fn cursor_clamps_when_swamp_shrinks() {
        assert_eq!(clamp_cursor(9, 3), 3);
        assert_eq!(clamp_cursor(2, 3), 2);
        assert_eq!(clamp_cursor(0, 0), 0);
    }

    #[test]
    fn parse_run_opts_covers_every_branch() {
        // Defaults.
        assert_eq!(
            parse_run_opts(&[]).unwrap(),
            RunOpts {
                once: false,
                interval: DEFAULT_INTERVAL_SECS
            }
        );
        // --once + --interval.
        assert_eq!(
            parse_run_opts(&["--once".into(), "--interval".into(), "9".into()]).unwrap(),
            RunOpts {
                once: true,
                interval: 9
            }
        );
        // Bad interval (zero / non-numeric / missing) → exit 2.
        assert_eq!(parse_run_opts(&["--interval".into(), "0".into()]), Err(2));
        assert_eq!(parse_run_opts(&["--interval".into(), "x".into()]), Err(2));
        assert_eq!(parse_run_opts(&["--interval".into()]), Err(2));
        // Help → exit 0; unknown → exit 2.
        assert_eq!(parse_run_opts(&["--help".into()]), Err(0));
        assert_eq!(parse_run_opts(&["--nope".into()]), Err(2));
    }

    #[test]
    fn parse_swamp_args_covers_every_branch() {
        // Minimal: tab + text, submit defaults true.
        assert_eq!(
            parse_swamp_args(&["uuid".into(), "hello".into()]).unwrap(),
            SwampArgs {
                tab: "uuid".into(),
                input: "hello".into(),
                submit: true,
                from: None,
                priority: Priority::default(),
                dedup_key: None,
            }
        );
        // Flags in any position, incl. RS1 --priority / --dedup-key.
        assert_eq!(
            parse_swamp_args(&[
                "--no-submit".into(),
                "uuid".into(),
                "hi".into(),
                "--from".into(),
                "bot".into(),
                "--priority".into(),
                "block".into(),
                "--dedup-key".into(),
                "k1".into()
            ])
            .unwrap(),
            SwampArgs {
                tab: "uuid".into(),
                input: "hi".into(),
                submit: false,
                from: Some("bot".into()),
                priority: Priority::Block,
                dedup_key: Some("k1".into()),
            }
        );
        // Missing text → 2; extra positional → 2; help → 0.
        assert_eq!(parse_swamp_args(&["only-tab".into()]), Err(2));
        assert_eq!(parse_swamp_args(&["a".into(), "b".into(), "c".into()]), Err(2));
        assert_eq!(parse_swamp_args(&["-h".into()]), Err(0));
        // An unknown --priority value → exit 2 (no silent default).
        assert_eq!(
            parse_swamp_args(&["u".into(), "t".into(), "--priority".into(), "bogus".into()]),
            Err(2)
        );
        // RS1 flags default cleanly (case-insensitive priority parse).
        assert_eq!(Priority::parse("VERDICT"), Some(Priority::Verdict));
        assert_eq!(Priority::parse("nope"), None);
    }

    #[test]
    fn plan_round_delivers_from_cursor_and_guards_targets() {
        let entries = vec![
            entry("old", "done", true),       // 0: before the cursor
            entry("claude", "go", true),      // 1: deliverable
            entry("shell", "rm -rf /", true), // 2: guard trips
            entry("claude", "again", false),  // 3: deliverable, no submit
        ];
        // Classifier: "claude" delivers; anything else is a permanent skip here.
        let ok = |uuid: &str| {
            if uuid == "claude" {
                Guard::Deliver
            } else {
                Guard::Skip(SkipKind::Permanent)
            }
        };
        let plan = plan_round(&entries, 1, ok);
        assert_eq!(
            plan,
            vec![
                Decision::Deliver {
                    index: 1,
                    tab: "claude".into(),
                    input: "go".into(),
                    submit: true
                },
                Decision::Skip {
                    index: 2,
                    tab: "shell".into(),
                    kind: SkipKind::Permanent
                },
                Decision::Deliver {
                    index: 3,
                    tab: "claude".into(),
                    input: "again".into(),
                    submit: false
                },
            ]
        );
        // Cursor past the end → empty plan, no panic.
        assert!(plan_round(&entries, 99, ok).is_empty());
    }

    #[test]
    fn append_swamp_line_appends_parseable_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("swamp.jsonl");
        append_swamp_line(&path, &entry("t1", "one", true)).unwrap();
        append_swamp_line(&path, &entry("t2", "two", false)).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_swamp(&body);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].tab, "t1");
        assert_eq!(parsed[1].tab, "t2");
        assert!(!parsed[1].submit);
    }

    // ===================================================================
    // LOT 2 — characterization of KNOWN GAPS (documented via a test, NOT fixed).
    // ===================================================================

    #[test]
    fn compact_rmw_window_drops_a_concurrent_append() {
        // GAP (b) — DOCUMENTED, NOT FIXED. compact_swamp() is a read-modify-write:
        // it re-reads the swamp into a `snapshot`, keeps compact(snapshot, cursor),
        // and renames the rewrite over the file. An entry a producer appends in the
        // tiny window BETWEEN that re-read and the rename is absent from `snapshot`,
        // hence absent from the kept tail, and the rename OVERWRITES it — lost. This
        // pins that reality: compact() only ever sees the snapshot handed to it.
        // (Upgrade = an flock around the RMW — see compact_swamp's ponytail.)
        let snapshot = vec![entry("t0", "a", true), entry("t1", "b", true)]; // read at compaction start
        let kept = compact(&snapshot, 2); // all delivered -> tail empty
        assert!(kept.is_empty());
        let concurrent = entry("t2", "c", true); // appended AFTER the snapshot read
        assert!(
            !kept.contains(&concurrent),
            "a concurrently-appended entry is lost by the RMW window (gap b)"
        );
    }

    #[test]
    fn submit_enter_is_atomic_unconfirmed_retries_bounded() {
        // GAP (a) — FIXED. The ⏎ submission is now ATOMIC w.r.t. the cursor. tick
        // types the text, then presses Enter as a SEPARATE POST after SUBMIT_DELAY,
        // and advances the cursor ONLY once that POST is CONFIRMED (returns Ok — the
        // daemon accepted the byte). If ⏎ isn't confirmed the entry is NOT dropped:
        // a submit-only nudge is re-queued (bounded by should_retry /
        // MAX_SKIP_ATTEMPTS) and abandoned+logged only once the budget is spent.
        //
        // Pinned facts:
        //  1. the delivered `input` is EXACTLY the entry text — ⏎ is a distinct send,
        //     never baked into `input`;
        //  2. the ⏎-retry re-queue NEVER re-types the body (empty input), keeps
        //     `submit`, and bumps `attempts` — so a retry is a pure newline;
        //  3. the retry is bounded (shared budget with the skip-retry path);
        //  4. SUBMIT_DELAY is a real (non-zero) settle gap between text and ⏎.
        let e = entry("claude", "run the tests", true);
        let plan = plan_round(std::slice::from_ref(&e), 0, |_| Guard::Deliver);
        match &plan[0] {
            Decision::Drop { .. } => panic!("expected Deliver, got a Drop"),
            Decision::Deliver { input, submit, .. } => {
                assert_eq!(input, "run the tests", "input is the raw text — ⏎ is NOT appended");
                assert!(!input.ends_with('\r'), "⏎ is a separate send, never baked into input");
                assert!(*submit, "submit is carried as its own flag");
            }
            Decision::Skip { .. } => panic!("expected Deliver, got a Skip"),
        }
        // (2) an unconfirmed ⏎ re-queues a pure newline, not the body again.
        let retry = submit_retry_entry(&e);
        assert_eq!(retry.input, "", "⏎ retry never re-types the body");
        assert!(retry.submit, "⏎ retry still asks to submit");
        assert_eq!(retry.attempts, 1, "⏎ retry bumps the attempt counter");
        assert_eq!(retry.tab, "claude", "⏎ retry keeps the routing key");
        assert_eq!(
            submit_retry_entry(&retry).attempts,
            2,
            "chained ⏎ retries climb toward the cap"
        );
        // (3) bounded: retried while under the cap, abandoned at it.
        assert!(should_retry(0), "first unconfirmed ⏎ is retried");
        assert!(
            !should_retry(MAX_SKIP_ATTEMPTS - 1),
            "the last attempt exhausts the budget → abandon (logged)"
        );
        // (4) a real settle delay still separates text from ⏎.
        assert!(
            SUBMIT_DELAY > Duration::from_millis(0),
            "a real settle delay separates text from ⏎"
        );
    }

    #[test]
    fn transient_skip_is_retried_bounded_permanent_skip_is_consumed() {
        // FIXED (#35, restart-injection). A guard-tripped target is no longer
        // uniformly dropped: `plan_round` now carries the skip's KIND, and `tick`
        // treats the two kinds differently.
        //
        // TRANSIENT (a target still becoming a live Claude tab — startup / resume /
        // the restart window): NOT dropped. `tick` re-queues a copy with
        // attempts+1, so it's retried a later round, up to MAX_SKIP_ATTEMPTS, then
        // abandoned (and logged). The `should_retry` boundary pins that bound.
        let e = entry("not-live-yet", "please run tests", true);
        let plan = plan_round(std::slice::from_ref(&e), 0, |_| Guard::Skip(SkipKind::Transient));
        assert_eq!(
            plan,
            vec![Decision::Skip {
                index: 0,
                tab: "not-live-yet".into(),
                kind: SkipKind::Transient,
            }],
            "a not-yet-live target is a TRANSIENT skip (tick re-queues it, not a drop)"
        );
        // Bounded retry: retried while under the cap, abandoned at it. With
        // MAX_SKIP_ATTEMPTS = 6 the entry is tried 6 times (attempts 0..=5) then
        // given up — no infinite loop, no permanent silent loss on a real tab.
        for spent in 0..MAX_SKIP_ATTEMPTS - 1 {
            assert!(should_retry(spent), "attempt {spent} is under the cap → retry");
        }
        assert!(
            !should_retry(MAX_SKIP_ATTEMPTS - 1),
            "the last attempt exhausts the budget → abandon (logged)"
        );

        // PERMANENT (a daemon/shell target): consumed at once, exactly as before —
        // the confused-deputy safety property is unchanged.
        let p = entry("some-shell", "rm -rf /", true);
        let plan = plan_round(std::slice::from_ref(&p), 0, |_| Guard::Skip(SkipKind::Permanent));
        assert_eq!(
            plan,
            vec![Decision::Skip {
                index: 0,
                tab: "some-shell".into(),
                kind: SkipKind::Permanent,
            }],
            "a daemon/shell target is a PERMANENT skip (tick consumes it — safety)"
        );
    }

    #[test]
    fn self_announce_tags_agent_kind_aligator_inside_a_tab_only() {
        // Inside a tab (_TAB_ID set): announce, body carries state + the agent_kind
        // the daemon's restart-watcher keys on to resurrect us on restart.
        let (tab, body) = self_announce(Some("uuid-123")).expect("inside a tab → announce");
        assert_eq!(tab, "uuid-123");
        assert!(body.contains("\"agentKind\":\"aligator\""), "tags agent_kind: {body}");
        assert!(body.contains("\"state\":\"thinking\""), "sets a live state: {body}");
        // Session-less: aligator never claims an agent session (don't clobber one).
        assert!(!body.contains("sessionId"), "session-less: {body}");
        // Outside a tab (_TAB_ID unset / empty / blank) → silent no-op (None).
        assert!(self_announce(None).is_none(), "no _TAB_ID → no announce");
        assert!(self_announce(Some("")).is_none(), "empty _TAB_ID → no announce");
        assert!(self_announce(Some("   ")).is_none(), "blank _TAB_ID → no announce");
    }

    // ---- RS1 fabric v2 — drain heuristics + invariants -------------------

    /// Build a deliverable FIFO plan (every target delivers) over `entries`.
    fn deliver_plan(entries: &[SwampEntry]) -> Vec<Decision> {
        plan_round(entries, 0, |_| Guard::Deliver)
    }

    /// The target uuids of the `Deliver` decisions, in plan order.
    fn delivered_tabs(plan: &[Decision]) -> Vec<String> {
        plan.iter()
            .filter_map(|d| match d {
                Decision::Deliver { tab, .. } => Some(tab.clone()),
                _ => None,
            })
            .collect()
    }

    fn empty_ledger() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    #[test]
    fn invariant_a_legacy_round_is_byte_identical_fifo() {
        // A round with NO priority/dedup signal must drain EXACTLY as today: the
        // heuristic layer is skipped (round_has_rs1_signal=false), priority_order
        // is the identity, and apply_delivery_heuristics is a pass-through.
        let entries = vec![entry("a", "1", true), entry("b", "2", true), entry("a", "3", true)];
        assert!(!round_has_rs1_signal(&entries), "no fields → no RS1 signal");
        assert_eq!(priority_order(&entries), vec![0, 1, 2], "all-default → identity order");
        let fifo = deliver_plan(&entries);
        let after = apply_delivery_heuristics(fifo.clone(), &entries, &empty_ledger(), RATE_LIMIT_PER_ROUND);
        assert_eq!(after, fifo, "legacy round passes through unchanged (FIFO identical)");
    }

    #[test]
    fn invariant_b_fail_open_empty_state_keeps_the_fifo_plan() {
        // Fail-open: with an empty ledger and no rate/ping-pong trip, every FIFO
        // Deliver survives — the layer never blocks a legit delivery.
        let entries = vec![
            tagged("a", Priority::Block, Some("k1")),
            tagged("b", Priority::Status, None),
        ];
        let fifo = deliver_plan(&entries);
        let after = apply_delivery_heuristics(fifo, &entries, &empty_ledger(), RATE_LIMIT_PER_ROUND);
        assert_eq!(
            delivered_tabs(&after),
            vec!["a", "b"],
            "nothing dropped/deferred when state is clean"
        );
    }

    #[test]
    fn invariant_c_gate_wins_over_heuristics() {
        // A gated target (not a live Claude tab) stays a Skip even with a high
        // priority + a dedup key — heuristics only DOWNGRADE Delivers, never
        // promote a Skip. Aligator's confused-deputy gate is intact.
        let entries = vec![tagged("shell", Priority::Block, Some("k1"))];
        let gated = plan_round(&entries, 0, |_| Guard::Skip(SkipKind::Permanent));
        let after = apply_delivery_heuristics(gated, &entries, &empty_ledger(), RATE_LIMIT_PER_ROUND);
        assert!(
            matches!(
                after.as_slice(),
                [Decision::Skip {
                    kind: SkipKind::Permanent,
                    ..
                }]
            ),
            "gated target stays a permanent skip regardless of priority/dedup: {after:?}"
        );
    }

    #[test]
    fn dedup_drops_an_already_delivered_key() {
        assert!(!is_duplicate(None, &empty_ledger()), "unkeyed is never a duplicate");
        let mut ledger = empty_ledger();
        ledger.insert("k1".to_string());
        assert!(is_duplicate(Some("k1"), &ledger));
        assert!(!is_duplicate(Some("k2"), &ledger));
        // In a plan: the entry whose key is already delivered becomes a Drop; the
        // fresh-keyed one still delivers.
        let entries = vec![
            tagged("a", Priority::Directive, Some("k1")), // already delivered
            tagged("b", Priority::Directive, Some("k2")), // fresh
        ];
        let after = apply_delivery_heuristics(deliver_plan(&entries), &entries, &ledger, RATE_LIMIT_PER_ROUND);
        assert!(matches!(after[0], Decision::Drop { .. }), "dup key → Drop: {after:?}");
        assert_eq!(delivered_tabs(&after), vec!["b"], "only the fresh-keyed entry delivers");
    }

    #[test]
    fn rate_limit_bounds_a_burst_to_one_target() {
        assert!(!rate_limited(2, 3));
        assert!(rate_limited(3, 3), "at the cap → limited");
        // 5 entries to the same tab, cap 3 → first 3 deliver, the rest defer
        // (Skip Transient, re-queued next round). Tagged Block so the round opts
        // into the RS1 layer (target-based heuristics are opt-in, invariant a).
        let entries: Vec<SwampEntry> = (0..5).map(|_| tagged("hot", Priority::Block, None)).collect();
        let after = apply_delivery_heuristics(deliver_plan(&entries), &entries, &empty_ledger(), 3);
        assert_eq!(delivered_tabs(&after).len(), 3, "burst capped at the rate limit");
        let deferred = after
            .iter()
            .filter(|d| {
                matches!(
                    d,
                    Decision::Skip {
                        kind: SkipKind::Transient,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(deferred, 2, "the surplus is deferred, not dropped");
    }

    #[test]
    fn ping_pong_oscillation_is_broken() {
        assert!(
            is_ping_pong("A", &["A".into(), "B".into()]),
            "A after (A,B) = oscillation"
        );
        assert!(!is_ping_pong("A", &["A".into()]), "one hop is not a ping-pong");
        assert!(
            !is_ping_pong("C", &["A".into(), "B".into()]),
            "a new target is not bouncing back"
        );
        // A → B → A → B: the 3rd (A) and 4th (B) are the returns that get deferred.
        // Tagged Block (same class → stable identity order) so the round opts into
        // the RS1 layer without perturbing the A,B,A,B sequence.
        let entries = vec![
            tagged("A", Priority::Block, None),
            tagged("B", Priority::Block, None),
            tagged("A", Priority::Block, None),
            tagged("B", Priority::Block, None),
        ];
        let after = apply_delivery_heuristics(deliver_plan(&entries), &entries, &empty_ledger(), 99);
        // The bounce-back to A (index 2) is deferred → the A↔B loop is broken; A is
        // delivered exactly once (its return blocked).
        assert!(
            matches!(
                after[2],
                Decision::Skip {
                    kind: SkipKind::Transient,
                    ..
                }
            ),
            "the return to A is deferred (loop broken): {after:?}"
        );
        let a_delivered = delivered_tabs(&after).iter().filter(|t| *t == "A").count();
        assert_eq!(a_delivered, 1, "A delivered once — its bounce-back is broken");
    }

    #[test]
    fn priority_orders_block_verdict_before_status_stable_within_class() {
        // block/verdict drain before directive/status; FIFO within a class.
        let entries = vec![
            tagged("s1", Priority::Status, None),
            tagged("b1", Priority::Block, None),
            tagged("d1", Priority::Directive, None),
            tagged("b2", Priority::Block, None),
            tagged("v1", Priority::Verdict, None),
        ];
        let order = priority_order(&entries);
        let ordered: Vec<&str> = order.iter().map(|&i| entries[i].tab.as_str()).collect();
        // Block (b1,b2 in FIFO) → Verdict (v1) → Directive (d1) → Status (s1).
        assert_eq!(ordered, vec!["b1", "b2", "v1", "d1", "s1"]);
    }

    // ===================================================================
    // INTEGRATION — the regulation FIRES over real HTTP (anti built≠wired).
    // ===================================================================

    /// Shared record of `(tab-uuid, delivered-bytes)` observed by the mock daemon.
    type Deliveries = std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>;

    /// A throwaway loopback HTTP server that answers `GET /tabs` with a fixed tab
    /// list and records the body of every `POST …/input`. Real sockets + real
    /// `ureq` client (aligator's own), so a delivery that fires is observable and a
    /// delivery the regulation suppressed is observably absent. Returns the base
    /// URL and the shared record of `(tab-uuid, bytes)` deliveries.
    fn spawn_mock_daemon(tabs_json: String) -> (String, Deliveries) {
        use std::io::{Read, Write};
        let recorded: Deliveries = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let rec = recorded.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 2048];
                // Read up to the end of the request headers.
                let hend = loop {
                    match stream.read(&mut tmp) {
                        Ok(0) | Err(_) => break None,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(p);
                    }
                    if buf.len() > 65_536 {
                        break None;
                    }
                };
                let Some(hend) = hend else { continue };
                let head = String::from_utf8_lossy(&buf[..hend]).into_owned();
                let mut req = head.lines().next().unwrap_or("").split_whitespace();
                let method = req.next().unwrap_or("").to_string();
                let path = req.next().unwrap_or("").to_string();
                let clen: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let mut body = buf[hend + 4..].to_vec();
                while body.len() < clen {
                    match stream.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => body.extend_from_slice(&tmp[..n]),
                    }
                }
                let resp_body = if method == "GET" && path == "/tabs" {
                    tabs_json.clone()
                } else {
                    if method == "POST" && path.starts_with("/tabs/by-id/") && path.ends_with("/input") {
                        let uuid = path
                            .trim_start_matches("/tabs/by-id/")
                            .trim_end_matches("/input")
                            .to_string();
                        rec.lock().unwrap().push((uuid, body.clone()));
                    }
                    "{}".to_string()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                    resp_body.len(),
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        (url, recorded)
    }

    #[test]
    fn regulation_fires_over_real_http_throttle_dedup_and_defer() {
        // A single `drain` round against a REAL loopback daemon, proving the
        // regulation is WIRED (not just compiled): the anti-herd throttle caps a
        // burst, the dedup ledger DROPS an already-delivered key (zero re-send),
        // and the throttled surplus is DEFERRED to the tail — never lost.
        let tabs_json = r#"{"tabs":[
            {"id":"hot","name":"worker-hot","agent_kind":"claude","agent_session_id":"s1"},
            {"id":"cool","name":"worker-cool","agent_kind":"claude","agent_session_id":"s2"},
            {"id":"dup","name":"worker-dup","agent_kind":"claude","agent_session_id":"s3"}
        ]}"#
        .to_string();
        let (url, recorded) = spawn_mock_daemon(tabs_json);
        let ep = Endpoint {
            url,
            token: "test".into(),
        };

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            swamp: dir.path().join("swamp.jsonl"),
            dedup: dir.path().join("aligator.dedup"),
            cursor: dir.path().join("aligator.cursor"),
        };
        // The dedup ledger already holds "seen" — a key delivered a PRIOR round.
        std::fs::write(&paths.dedup, "seen\n").unwrap();

        // All Block so the round opts into the RS1 regulation layer; submit=false
        // so each delivery is a single /input POST (no ⏎ settle delay → fast).
        let mk = |tab: &str, body: &str, key: Option<&str>| SwampEntry {
            ts: 0,
            tab: tab.into(),
            input: body.into(),
            submit: false,
            from: None,
            attempts: 0,
            priority: Priority::Block,
            dedup_key: key.map(Into::into),
        };
        for e in [
            mk("hot", "m1", None),
            mk("hot", "m2", None),
            mk("hot", "m3", None),
            mk("hot", "m4", None),              // 4th to hot → over the per-round cap
            mk("dup", "already", Some("seen")), // key already delivered → dropped
            mk("cool", "c1", None),
        ] {
            append_swamp_line(&paths.swamp, &e).unwrap();
        }

        let mut cursor = 0usize;
        drain(&ep, &paths, &mut cursor).expect("drain round");

        let rec = recorded.lock().unwrap();
        let count = |tab: &str| rec.iter().filter(|(t, _)| t == tab).count();
        // THROTTLE (anti-herd): a burst of 4 to one target is capped at the
        // per-round rate limit — the observable one-send-regulation.
        assert_eq!(
            count("hot"),
            RATE_LIMIT_PER_ROUND,
            "burst throttled to the per-round cap (anti-herd)"
        );
        // DEDUP: an entry whose key was already delivered is DROPPED — zero HTTP.
        assert_eq!(count("dup"), 0, "already-delivered key is dropped, never re-sent");
        // A fresh target still delivers exactly once (regulation ≠ starvation).
        assert_eq!(count("cool"), 1, "a fresh target delivers exactly once");
        // Real payload bytes landed (not a bare 200 / ping): the actual message.
        assert!(
            rec.iter().any(|(t, b)| t == "hot" && b == b"m1"),
            "the real message bytes were delivered over HTTP"
        );
        drop(rec);

        // DEFER-not-drop: the throttled 4th message survives in the compacted tail
        // to drain a later round — anti-herd defers, it never loses.
        let tail = parse_swamp(&std::fs::read_to_string(&paths.swamp).unwrap());
        assert!(
            tail.iter().any(|e| e.tab == "hot" && e.input == "m4"),
            "the throttled surplus is deferred to the tail, not dropped"
        );
    }
}
