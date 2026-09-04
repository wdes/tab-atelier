// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-memory ring of the most recent log records.
//!
//! Reading what a *running* instance is doing meant one of: journald access
//! (service installs only), a pre-configured file sink (`tab-atelier log
//! <filter>`, which applies on the NEXT start), or a terminal you launched it
//! from. None of those help when something is misbehaving right now — hence a
//! small always-on ring that `GET /logs` and `tab-atelier logs` read.
//!
//! It is deliberately bounded and lossy: [`CAPACITY`] lines, oldest dropped
//! first. This is a live tail for diagnosis, not an audit log — anything that
//! must survive belongs in the file sink.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Lines kept. A few hundred covers "what just happened" while staying a
/// rounding error next to a tab's scrollback.
pub const CAPACITY: usize = 500;

/// Longest message kept; a runaway `Debug` dump can't blow the ring's memory.
pub const MAX_MSG: usize = 2000;

/// Records at this level or more severe enter the ring, whatever the console
/// filter is — the ring is meant to be useful without configuring anything.
pub const RING_LEVEL: log::LevelFilter = log::LevelFilter::Info;

/// One captured record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Line {
    /// Unix milliseconds — the client formats; the daemon stays timezone-free.
    pub ts_ms: u64,
    /// `ERROR` / `WARN` / `INFO` / …
    pub level: &'static str,
    /// Emitting module path.
    pub target: String,
    pub msg: String,
}

static RING: Mutex<VecDeque<Line>> = Mutex::new(VecDeque::new());

/// Append one line, dropping the oldest once [`CAPACITY`] is reached.
pub fn push(line: Line) {
    let mut ring = RING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if ring.len() == CAPACITY {
        ring.pop_front();
    }
    ring.push_back(line);
}

/// The last `n` lines, oldest first. `n` larger than what's held returns
/// everything; 0 returns nothing.
#[must_use]
pub fn tail(n: usize) -> Vec<Line> {
    let ring = RING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let skip = ring.len().saturating_sub(n);
    ring.iter().skip(skip).cloned().collect()
}

/// Serializes the tests that assert on the ring — it is process-global, so
/// one test's `clear` would otherwise empty another's fixture mid-assert.
#[cfg(test)]
pub static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Drop everything. Tests only — the ring is process-global.
#[cfg(test)]
pub fn clear() {
    RING.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
}

/// Truncate a message to [`MAX_MSG`] chars, marking that it was cut.
#[must_use]
pub fn clamp_msg(msg: &str) -> String {
    if msg.chars().count() <= MAX_MSG {
        return msg.to_string();
    }
    let kept: String = msg.chars().take(MAX_MSG).collect();
    format!("{kept}… (truncated)")
}

/// A `log::Log` that keeps every record in the ring *and* forwards the ones
/// the wrapped logger wants to its own sink.
///
/// Wrapping rather than replacing keeps the existing behaviour intact: the
/// console/file output still honours the configured filter, while the ring
/// captures [`RING_LEVEL`] and above regardless — so `logs` is useful on a
/// daemon started with no filter at all.
pub struct RingLogger<L: log::Log> {
    inner: L,
}

impl<L: log::Log> RingLogger<L> {
    pub const fn new(inner: L) -> Self {
        Self { inner }
    }
}

impl<L: log::Log> log::Log for RingLogger<L> {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= RING_LEVEL || self.inner.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if self.inner.enabled(record.metadata()) {
            self.inner.log(record);
        }
        if record.level() <= RING_LEVEL {
            push(Line {
                ts_ms: crate::unix_millis(),
                level: record.level().as_str(),
                target: record.target().to_string(),
                msg: clamp_msg(&record.args().to_string()),
            });
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// Install `inner` wrapped in the ring, raising the global max level so
/// [`RING_LEVEL`] records reach us even when the console filter is stricter.
///
/// No-op if a logger is already installed (`log` allows exactly one).
pub fn install<L: log::Log + 'static>(inner: L, inner_level: log::LevelFilter) {
    if log::set_boxed_logger(Box::new(RingLogger::new(inner))).is_ok() {
        log::set_max_level(inner_level.max(RING_LEVEL));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(msg: &str) -> Line {
        Line {
            ts_ms: 1,
            level: "INFO",
            target: "t".into(),
            msg: msg.into(),
        }
    }

    #[test]
    fn the_ring_keeps_the_newest_lines_and_drops_the_oldest() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        clear();
        for i in 0..CAPACITY + 50 {
            push(line(&format!("line-{i}")));
        }
        let all = tail(CAPACITY * 2);
        assert_eq!(all.len(), CAPACITY, "bounded — a chatty daemon can't grow it");
        assert_eq!(all[0].msg, format!("line-{}", 50), "oldest dropped first");
        assert_eq!(all[CAPACITY - 1].msg, format!("line-{}", CAPACITY + 49));
        // A tail shorter than the ring returns the NEWEST n, in order.
        let last3 = tail(3);
        assert_eq!(last3.len(), 3);
        assert_eq!(last3[2].msg, format!("line-{}", CAPACITY + 49));
        assert_eq!(last3[0].msg, format!("line-{}", CAPACITY + 47));
        assert!(tail(0).is_empty());
        clear();
        assert!(tail(10).is_empty(), "cleared");
    }

    #[test]
    fn a_runaway_message_is_clamped() {
        assert_eq!(clamp_msg("short"), "short");
        let long = "x".repeat(MAX_MSG + 100);
        let cut = clamp_msg(&long);
        assert!(cut.ends_with("… (truncated)"));
        assert_eq!(cut.chars().count(), MAX_MSG + "… (truncated)".chars().count());
        // Multi-byte input must be cut on a char boundary, not a byte one.
        let accented = "é".repeat(MAX_MSG + 10);
        assert!(clamp_msg(&accented).starts_with('é'));
    }
}
