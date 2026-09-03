// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Deterministic per-section duration instrumentation, gated behind the
//! `KALPIN_PERF_INSTRUMENT=1` env var (default OFF → fully inert).
//!
//! ## Why
//!
//! The daemon's per-streaming-tab CPU cost can't be attributed with a
//! sampling profiler (the shipped binary is LTO-stripped — zero symbols).
//! This is a maison alternative: wrap each suspected section in a cumulative
//! `Instant::now()` delta, and log the ms/window every 10 s so the DOMINANT
//! section is read off deterministically under real streaming load.
//!
//! ## Zero cost when off
//!
//! [`time`] returns `Timer(None)` when the flag is unset — no `Instant::now`,
//! its `Drop` is a no-op, and the reporter thread is never spawned. The only
//! residual is one cached-bool branch per call site (call sites are ≤ ~10 Hz
//! per tab, so immeasurable).
//!
//! ponytail: fixed 7-section set + `Relaxed` atomics (a diagnostic, not a
//! metrics backbone — no per-tab cardinality, no histograms). The reporter
//! logs to stderr with a `[PERF]` prefix so capture doesn't depend on
//! `RUST_LOG`. Upgrade path if a section needs finer split: add a `Section`
//! variant + a `NAMES` entry.

use std::fmt::Write as _;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The instrumented sections. `#[repr(usize)]` so the variant doubles as its
/// index into the counter arrays.
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Section {
    /// `HeadlessTab::cached_grid` — the crc-scan / ANSI-ify of one tab's grid.
    GridScan = 0,
    /// `agent_probe::sample_tree_cached` — the per-tab `/proc` RSS subtree walk.
    RssProbe = 1,
    /// The whole `refresh_snapshot` call (`grid_scan` + `rss_probe` are subsets;
    /// the remainder ≈ the `SnapshotTab` build + snapshot writeback).
    RefreshTotal = 2,
    /// The whole 2 s `persist` call (tabs.json serialize + crc + submits).
    Persist = 3,
    /// `GET /tabs` cache-miss rebuild (strip-ansi per tab + pretty JSON).
    TabsJson = 4,
    /// `GET /dashboard/state` aggregation (per-tab roll-up + pretty JSON).
    DashboardAgg = 5,
    /// `api_ws::encode_out_frame` — encode/gzip one forwarded PTY chunk.
    WsForward = 6,
}

const N: usize = 7;
const NAMES: [&str; N] = [
    "grid_scan",
    "rss_probe",
    "refresh_total",
    "persist",
    "tabs_json",
    "dashboard_agg",
    "ws_forward",
];

static NANOS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static HITS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];

/// `true` iff `KALPIN_PERF_INSTRUMENT=1` in the environment. Read once,
/// cached — after the first call it's a plain atomic load.
#[inline]
#[must_use]
pub fn enabled() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var("KALPIN_PERF_INSTRUMENT").is_ok_and(|v| v == "1"));
    *ON
}

/// RAII stopwatch. Records the elapsed nanos into its section on `Drop`.
/// `Timer(None)` when instrumentation is off — a zero-cost no-op.
pub struct Timer(Option<(usize, Instant)>);

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some((i, start)) = self.0 {
            // `as u64` saturates a >584-year delta; irrelevant here.
            NANOS[i].fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
            HITS[i].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Start timing `section`. Bind the result to a `_timer` local; it records on
/// scope exit. `Instant::now()` is only paid when the flag is on.
#[inline]
#[must_use]
pub fn time(section: Section) -> Timer {
    if enabled() {
        Timer(Some((section as usize, Instant::now())))
    } else {
        Timer(None)
    }
}

/// Spawn the 10 s reporter thread — a no-op when instrumentation is off.
/// Each window it drains (swaps to 0) every counter and logs the cumulative
/// ms + hit count per section to stderr, so successive lines are per-window.
pub fn spawn_reporter() {
    if !enabled() {
        return;
    }
    let spawned = std::thread::Builder::new().name("perf-instr".into()).spawn(|| {
        eprintln!("[PERF] instrumentation ON — sections: {}", NAMES.join(" "));
        loop {
            std::thread::sleep(Duration::from_secs(10));
            let mut line = String::from("[PERF] window=10s");
            for i in 0..N {
                let ns = NANOS[i].swap(0, Ordering::Relaxed);
                let hits = HITS[i].swap(0, Ordering::Relaxed);
                let ms = ns as f64 / 1_000_000.0;
                let _ = write!(line, " {}={ms:.1}ms/{hits}", NAMES[i]);
            }
            eprintln!("{line}");
        }
    });
    if let Err(e) = spawned {
        eprintln!("[PERF] reporter thread failed to spawn: {e}");
    }
}
