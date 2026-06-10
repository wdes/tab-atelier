// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-tab raw PTY byte ring + tap wrapper for alacritty's
//! [`tty::Pty`].
//!
//! ## Why
//!
//! alacritty's [`Term`] is the source of truth for the *visible*
//! grid + a 10 k-line scrollback, but two things conspire to make
//! that scrollback nearly empty in practice for share-link viewers:
//!
//! 1. Claude Code's TUI redraws in-place with cursor positioning
//!    and `\x1b[2J`. Rows are overwritten rather than scrolled, so
//!    they never enter alacritty's history.
//! 2. Some TUIs (less, htop, anything that re-mounts the alt-screen)
//!    emit `\x1b[3J` (Erase Saved Lines), which alacritty obediently
//!    handles by [calling
//!    `grid.clear_history()`][alacritty-clear-history], wiping every
//!    scroll-off row.
//!
//! [alacritty-clear-history]: https://github.com/alacritty/alacritty/blob/v0.26.0/alacritty_terminal/src/term/mod.rs#L1805
//!
//! Result: `raw_screen_text(Some(2000))` returns ~50 rows (visible
//! grid only) and the xterm.js viewer can never scroll past the
//! current screen. The desktop GUI doesn't suffer because it renders
//! directly from alacritty's grid + history in-process — but for a
//! remote viewer, the wire has nothing to offer.
//!
//! ## What
//!
//! [`PtyRing`] is a bounded byte ring that records every byte the
//! PTY emits, BEFORE alacritty parses it. [`PtyTap`] wraps an
//! [`alacritty_terminal::tty::Pty`] so the alacritty `EventLoop`
//! reads through us — every read is forwarded to the ring as a
//! side effect, then handed to alacritty as normal. Both `\x1b[3J`
//! and in-place redraws are captured at the wire level, so the
//! viewer can replay them and reconstruct any historical state
//! (modulo the ring's capacity).
//!
//! ## Wire model
//!
//! Bytes are addressed by monotonic [`u64`] offsets since the PTY
//! was first tapped. [`PtyRing::total_len`] gives the high-water
//! mark; [`PtyRing::since`] returns every byte from a given offset
//! onward (or as far back as the ring still holds when the requested
//! offset has already aged out). [`PtyRing::base_offset`] tells the
//! caller where our remembered window starts so it can detect a
//! truncation gap.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{ChildEvent, EventedPty, EventedReadWrite};
use polling::{Event, PollMode, Poller};

/// Default per-tab capacity. ~4 MB is enough to hold around 50 000
/// terminal lines of text (80 chars + a couple of SGR sequences).
/// Plenty for any reasonable interactive session; bounded so a
/// runaway tab can't grow without limit.
pub const DEFAULT_CAPACITY_BYTES: usize = 4 * 1024 * 1024;

/// Bounded byte ring with monotonic [`u64`] addressing.
///
/// Cheap, all-purpose append + read-from-offset structure. We
/// deliberately avoid line awareness — the ring is opaque to the
/// terminal protocol so we don't have to keep up with escape-sequence
/// boundary detection. xterm.js's parser already does that on the
/// client side.
#[derive(Debug)]
pub struct PtyRing {
    bytes: VecDeque<u8>,
    /// Monotonic byte offset of `bytes[0]` since the ring was
    /// constructed. Grows whenever the ring drops bytes from the
    /// front to make room.
    base_offset: u64,
    cap: usize,
}

impl Default for PtyRing {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY_BYTES)
    }
}

impl PtyRing {
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(cap.min(1024 * 1024)),
            base_offset: 0,
            cap: cap.max(1),
        }
    }

    /// Append `data` to the ring, dropping the oldest bytes if the
    /// capacity would be exceeded. `base_offset` advances by the
    /// number of dropped bytes so the public offset space stays
    /// monotonic.
    pub fn push(&mut self, data: &[u8]) {
        // Single push larger than capacity: the survivors are the
        // tail of `data`. Drop everything we had AND skip the head of
        // `data` so only the last `cap` bytes remain.
        if data.len() >= self.cap {
            let drop_existing = self.bytes.len();
            self.bytes.clear();
            self.base_offset += drop_existing as u64;
            let skip = data.len() - self.cap;
            self.base_offset += skip as u64;
            self.bytes.extend(&data[skip..]);
            return;
        }
        let overflow = (self.bytes.len() + data.len()).saturating_sub(self.cap);
        if overflow > 0 {
            self.bytes.drain(..overflow);
            self.base_offset += overflow as u64;
        }
        self.bytes.extend(data);
    }

    /// Monotonic high-water mark — total bytes ever written through
    /// this ring. The right offset to record after consuming a
    /// snapshot so subsequent [`Self::since`] calls return only new
    /// bytes.
    #[must_use]
    pub fn total_len(&self) -> u64 {
        self.base_offset + self.bytes.len() as u64
    }

    /// Lowest offset still retained. When a caller's `since` is below
    /// this, some bytes have aged out — caller should know to
    /// resync. Currently used only by tests; kept for future WS
    /// reconnect gap detection.
    #[allow(dead_code)]
    #[must_use]
    pub const fn base_offset(&self) -> u64 {
        self.base_offset
    }

    /// Capacity in bytes — does not change after construction.
    /// Same story as `base_offset` — currently unused outside tests.
    #[allow(dead_code)]
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.cap
    }

    /// Copy bytes from `offset` onward into a fresh `Vec`. Clamped to
    /// the available window: when `offset < base_offset`, callers
    /// receive everything we still have (and `base_offset()` lets
    /// them detect the gap).
    #[must_use]
    pub fn since(&self, offset: u64) -> Vec<u8> {
        if offset >= self.total_len() {
            return Vec::new();
        }
        let start = offset.max(self.base_offset);
        let skip = (start - self.base_offset) as usize;
        // Seek directly to `skip` via the deque's two contiguous
        // slices instead of `iter().skip(skip)`, which walks every
        // skipped byte one at a time. In steady state the WS pump polls
        // ~33x/s with `skip` near the end of a multi-MB ring, so the
        // old form re-scanned the whole buffer on every tick. This is
        // O(result) rather than O(buffer).
        let (head, tail) = self.bytes.as_slices();
        let mut out = Vec::with_capacity(self.bytes.len() - skip);
        if skip < head.len() {
            out.extend_from_slice(&head[skip..]);
            out.extend_from_slice(tail);
        } else {
            out.extend_from_slice(&tail[skip - head.len()..]);
        }
        out
    }
}

/// `EventedPty` wrapper that mirrors every read into the supplied
/// [`PtyRing`] before handing the bytes to alacritty's parser. The
/// inner Pty's writer and child-event side are passed through
/// untouched.
///
/// We declare `type Reader = Self` and implement [`io::Read`] on the
/// wrapper itself, so [`EventedReadWrite::reader`] returns `&mut
/// self`. This sidesteps the GAT gymnastics that would otherwise be
/// needed to return a struct holding `&mut self.inner.reader()`.
pub struct PtyTap<P: EventedPty> {
    inner: P,
    ring: Arc<Mutex<PtyRing>>,
}

impl<P: EventedPty> PtyTap<P> {
    pub const fn new(inner: P, ring: Arc<Mutex<PtyRing>>) -> Self {
        Self { inner, ring }
    }
}

impl<P: EventedPty> io::Read for PtyTap<P> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.reader().read(buf)?;
        if n > 0 {
            // T2 — instrumentation: bytes arrived from the PTY (about
            // to be parsed by alacritty). For typing-lag analysis,
            // this is "echo received" — the delta from T1 measures
            // shell round-trip latency, which is mostly kernel +
            // shell scheduling. Enable with
            // RUST_LOG=tab_atelier::input_lag=trace.
            log::trace!(
                target: "tab_atelier::input_lag",
                "T2 pty_read bytes={} preview={:?}",
                n,
                std::str::from_utf8(&buf[..n.min(32)]).unwrap_or("<non-utf8>"),
            );
            if let Ok(mut ring) = self.ring.lock() {
                ring.push(&buf[..n]);
            }
        }
        Ok(n)
    }
}

// `EventedReadWrite::register` is `unsafe fn` in the upstream trait
// (the implementor must guarantee the source outlives its
// registration). Implementing it requires `unsafe fn`; the crate
// otherwise denies `unsafe_code`. Allow it on this impl block only.
#[allow(unsafe_code)]
impl<P: EventedPty> EventedReadWrite for PtyTap<P> {
    type Reader = Self;
    type Writer = P::Writer;

    unsafe fn register(&mut self, poller: &Arc<Poller>, event: Event, mode: PollMode) -> io::Result<()> {
        // Safety: contract is unchanged from the inner Pty — same fd,
        // same lifetime. Wrapper holds the Pty until Drop, mirroring
        // the inner's invariant.
        unsafe { self.inner.register(poller, event, mode) }
    }

    fn reregister(&mut self, poller: &Arc<Poller>, event: Event, mode: PollMode) -> io::Result<()> {
        self.inner.reregister(poller, event, mode)
    }

    fn deregister(&mut self, poller: &Arc<Poller>) -> io::Result<()> {
        self.inner.deregister(poller)
    }

    fn reader(&mut self) -> &mut Self::Reader {
        self
    }

    fn writer(&mut self) -> &mut Self::Writer {
        self.inner.writer()
    }
}

impl<P: EventedPty> EventedPty for PtyTap<P> {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.inner.next_child_event()
    }
}

impl<P: EventedPty + OnResize> OnResize for PtyTap<P> {
    fn on_resize(&mut self, size: WindowSize) {
        self.inner.on_resize(size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_total_len_matches_input_size() {
        let mut r = PtyRing::with_capacity(1024);
        assert_eq!(r.total_len(), 0);
        r.push(b"hello");
        assert_eq!(r.total_len(), 5);
        r.push(b" world");
        assert_eq!(r.total_len(), 11);
        assert_eq!(r.base_offset(), 0);
    }

    #[test]
    fn since_from_zero_returns_everything() {
        let mut r = PtyRing::with_capacity(1024);
        r.push(b"abc");
        r.push(b"def");
        assert_eq!(r.since(0), b"abcdef");
    }

    #[test]
    fn since_from_midpoint_returns_suffix() {
        let mut r = PtyRing::with_capacity(1024);
        r.push(b"abcdef");
        assert_eq!(r.since(3), b"def");
        assert_eq!(r.since(6), b"");
        assert_eq!(r.since(99), b"");
    }

    #[test]
    fn overflow_drops_front_and_advances_base_offset() {
        let mut r = PtyRing::with_capacity(4);
        r.push(b"abcd"); // [a b c d]
        assert_eq!(r.base_offset(), 0);
        assert_eq!(r.since(0), b"abcd");

        r.push(b"ef"); // dropped a,b → [c d e f], base_offset = 2
        assert_eq!(r.base_offset(), 2);
        assert_eq!(r.total_len(), 6);
        assert_eq!(r.since(2), b"cdef");
        // Asking for bytes that have aged out: caller gets the
        // surviving window. base_offset() flags the gap.
        assert_eq!(r.since(0), b"cdef");
    }

    #[test]
    fn since_aged_out_offset_returns_surviving_window() {
        let mut r = PtyRing::with_capacity(3);
        r.push(b"abcdef"); // → [d e f], base_offset = 3
        assert_eq!(r.base_offset(), 3);
        assert_eq!(r.since(0), b"def");
        assert_eq!(r.since(2), b"def");
        assert_eq!(r.since(3), b"def");
        assert_eq!(r.since(5), b"f");
    }

    #[test]
    fn push_chunk_larger_than_capacity_truncates_to_tail() {
        let mut r = PtyRing::with_capacity(3);
        r.push(b"abcdefghij"); // only "hij" survives
        assert_eq!(r.since(0), b"hij");
        assert_eq!(r.base_offset(), 7);
        assert_eq!(r.total_len(), 10);
    }

    #[test]
    fn empty_ring_since_returns_empty() {
        let r = PtyRing::with_capacity(1024);
        assert!(r.since(0).is_empty());
        assert_eq!(r.total_len(), 0);
    }
}
