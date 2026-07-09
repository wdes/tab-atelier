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

/// One deflate-compressed block of aged-out ring bytes. Terminal
/// output compresses 5-10×, so keeping the cold majority of the ring
/// compressed cuts a full tab's resident cost from `cap` (4 MiB) to
/// well under 1 MiB without giving up a byte of replayable history.
#[derive(Debug)]
struct ColdChunk {
    /// Uncompressed size — the chunk's extent in offset space.
    raw_len: u32,
    deflated: Vec<u8>,
}

impl ColdChunk {
    fn inflate(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.raw_len as usize);
        let mut dec = flate2::read::DeflateDecoder::new(self.deflated.as_slice());
        // Reading our own well-formed deflate stream into a Vec cannot
        // fail; a partial result would only follow memory corruption.
        let _ = std::io::Read::read_to_end(&mut dec, &mut out);
        out
    }
}

/// Bounded byte ring with monotonic [`u64`] addressing.
///
/// Cheap, all-purpose append + read-from-offset structure. We
/// deliberately avoid line awareness — the ring is opaque to the
/// terminal protocol so we don't have to keep up with escape-sequence
/// boundary detection. xterm.js's parser already does that on the
/// client side.
///
/// Storage is two-tier: the newest [`Self::HOT_MAX`] bytes stay plain
/// in `hot` (the WS pump reads the tail ~33×/s — that path must stay
/// O(result)), everything older is deflated into `cold` chunks. The
/// capacity bounds the UNCOMPRESSED total, so offset semantics and
/// retained history are identical to the uncompressed ring.
#[derive(Debug)]
pub struct PtyRing {
    /// Plain tail — the newest bytes, uncompressed.
    hot: VecDeque<u8>,
    /// Compressed older blocks, oldest first. Contiguous with `hot`:
    /// offsets run `base_offset` → cold chunks → hot.
    cold: VecDeque<ColdChunk>,
    /// Σ `raw_len` over `cold` — the cold section's extent.
    cold_raw: usize,
    /// Monotonic byte offset of the first retained byte since the ring
    /// was constructed. Grows whenever the ring drops bytes from the
    /// front to make room.
    base_offset: u64,
    cap: usize,
    /// Fired on every (non-empty) [`Self::push`] so a connected
    /// web-viewer pump wakes and flushes the new bytes immediately
    /// instead of waiting out a poll tick — see `api_ws::run_pump`.
    /// No-op when no pump is registered; lives outside the protocol
    /// model so it costs a single `notify_waiters` per PTY read.
    notify: std::sync::Arc<tokio::sync::Notify>,
    /// Number of WS viewers (browser share-link / `remote attach`)
    /// currently attached to this tab. Bumped by a guard in
    /// `api_ws::run_pump` for the life of each connection. An
    /// `Arc<AtomicUsize>` so the guard can inc/dec and the renderer can
    /// read it without taking the ring lock. Drives "is anyone watching
    /// this tab" — the GUI's dormant-LED suppressor and the `/tabs`
    /// `viewers` field.
    viewers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Lock-free mirror of [`Self::total_len`], updated on every push.
    /// The GUI's per-tab repaint pump reads it each tick to answer
    /// "did output arrive" — through this handle that's one atomic
    /// load instead of taking the ring mutex the PTY reader thread
    /// contends on.
    len_mirror: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// One-entry cache of the last encoded `out` wire frame, keyed by
    /// the byte range it covers. N viewers of one tab flush the same
    /// suffix in lockstep (they all stay caught up), so without this
    /// each connection copied the bytes out of the ring and gzipped
    /// them independently — the first pump to encode a range now
    /// shares the `Bytes` with the rest for free. A range mismatch
    /// (viewers at different offsets) just misses; correctness never
    /// depends on the cache.
    frame_cache: Option<(u64, u64, bytes::Bytes)>,
}

impl Default for PtyRing {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY_BYTES)
    }
}

impl PtyRing {
    /// Plain-tail budget: bytes newer than this stay uncompressed so
    /// the steady-state `since` reads (WS pump tail polls) never touch
    /// a decompressor. Compaction only engages when the capacity
    /// meaningfully exceeds it, so small test rings behave exactly
    /// like the historical uncompressed ring.
    const HOT_MAX: usize = 256 * 1024;
    /// Uncompressed size of one cold chunk.
    const CHUNK_RAW: usize = 128 * 1024;

    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            // Start small and let the deque grow toward `cap` on demand:
            // the old 1 MiB pre-allocation charged every tab ~1 MiB of
            // RSS at spawn even if it never printed more than a prompt
            // (60 restored tabs ⇒ ~60 MiB of mostly-untouched buffers).
            hot: VecDeque::with_capacity(cap.min(64 * 1024)),
            cold: VecDeque::new(),
            cold_raw: 0,
            base_offset: 0,
            cap: cap.max(1),
            notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            viewers: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            len_mirror: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            frame_cache: None,
        }
    }

    /// Clone the lock-free `total_len` mirror. See the `len_mirror` field.
    /// The production caller is the GUI's repaint pump; headless builds
    /// compile the method for the shared unit tests only.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    #[must_use]
    pub fn total_len_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        self.len_mirror.clone()
    }

    /// Ceiling on a cached wire frame. Anything larger (a `since=0`
    /// scrollback bootstrap can be megabytes) is a per-connection one-off —
    /// caching it would just pin the memory until the next steady-state
    /// frame overwrites it.
    const FRAME_CACHE_MAX: usize = 512 * 1024;

    /// The encoded wire frame covering exactly `[from, to)`, if a pump
    /// already built one. See the `frame_cache` field.
    #[must_use]
    pub fn cached_frame(&self, from: u64, to: u64) -> Option<bytes::Bytes> {
        self.frame_cache
            .as_ref()
            .filter(|(f, t, _)| *f == from && *t == to)
            .map(|(_, _, b)| b.clone())
    }

    /// Publish the encoded wire frame for `[from, to)` so sibling
    /// viewers of this tab can reuse it. Oversized frames are skipped
    /// (see [`Self::FRAME_CACHE_MAX`]).
    pub fn store_frame(&mut self, from: u64, to: u64, frame: bytes::Bytes) {
        if frame.len() <= Self::FRAME_CACHE_MAX {
            self.frame_cache = Some((from, to, frame));
        }
    }

    /// Clone the wake handle for a consumer (the WS pump) to await.
    /// Each `notified()` waiter is woken on the next non-empty push.
    #[must_use]
    pub fn notifier(&self) -> std::sync::Arc<tokio::sync::Notify> {
        self.notify.clone()
    }

    /// Clone the viewer-count handle so a connection guard can inc/dec
    /// it without holding the ring lock. See [`Self::viewer_count`].
    #[must_use]
    pub fn viewers_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.viewers.clone()
    }

    /// How many WS viewers are currently attached to this tab.
    #[must_use]
    pub fn viewer_count(&self) -> usize {
        self.viewers.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Append `data` to the ring, dropping the oldest bytes if the
    /// capacity would be exceeded. `base_offset` advances by the
    /// number of dropped bytes so the public offset space stays
    /// monotonic.
    pub fn push(&mut self, data: &[u8]) {
        self.push_inner(data);
        self.len_mirror
            .store(self.total_len(), std::sync::atomic::Ordering::Relaxed);
        // Wake the viewer pump so the new bytes flush within
        // microseconds. Skip on an empty push (nothing to flush) and
        // when nobody is attached — `notify_waiters` takes an internal
        // waiter-list mutex even with zero waiters, and this runs once
        // per PTY read on every tab (N unwatched flooding tabs would
        // pay it thousands of times a second for nothing). A viewer
        // increments the count before it first awaits the notifier, and
        // the pump's 100 ms meta tick backstops any connect race.
        if !data.is_empty() && self.viewer_count() > 0 {
            self.notify.notify_waiters();
        } else if self.frame_cache.is_some() {
            // No viewer left to reuse it — drop the retained wire frame
            // so an unwatched tab doesn't pin up to 512 KiB of encoded
            // output indefinitely.
            self.frame_cache = None;
        }
    }

    fn push_inner(&mut self, data: &[u8]) {
        // Single push larger than capacity: the survivors are the
        // tail of `data`. Drop everything we had AND skip the head of
        // `data` so only the last `cap` bytes remain.
        if data.len() >= self.cap {
            let drop_existing = self.cold_raw + self.hot.len();
            self.cold.clear();
            self.cold_raw = 0;
            self.hot.clear();
            self.base_offset += drop_existing as u64;
            let skip = data.len() - self.cap;
            self.base_offset += skip as u64;
            self.hot.extend(&data[skip..]);
            self.compact();
            return;
        }
        self.hot.extend(data);
        self.compact();
        self.evict_over_cap();
    }

    /// Deflate the oldest hot bytes into cold chunks until the plain
    /// tail is back under [`Self::HOT_MAX`]. No-op for small rings
    /// (tests, custom caps) — they keep the historical plain behaviour.
    fn compact(&mut self) {
        if self.cap <= Self::HOT_MAX * 2 {
            return;
        }
        while self.hot.len() > Self::HOT_MAX {
            let raw: Vec<u8> = self.hot.drain(..Self::CHUNK_RAW).collect();
            let mut enc =
                flate2::write::DeflateEncoder::new(Vec::with_capacity(raw.len() / 4), flate2::Compression::fast());
            // Writing into a Vec sink cannot fail.
            let _ = std::io::Write::write_all(&mut enc, &raw);
            let deflated = enc.finish().unwrap_or_default();
            self.cold_raw += raw.len();
            self.cold.push_back(ColdChunk {
                raw_len: raw.len() as u32,
                deflated,
            });
        }
    }

    /// Drop the oldest bytes until the UNCOMPRESSED total fits `cap`.
    /// Cold chunks go whole (recompressing a partial chunk isn't worth
    /// it — at most one chunk of slack); with no cold section this is
    /// the historical front-drain.
    fn evict_over_cap(&mut self) {
        let mut overflow = (self.cold_raw + self.hot.len()).saturating_sub(self.cap);
        while overflow > 0 {
            if let Some(front) = self.cold.front() {
                let n = front.raw_len as usize;
                self.cold.pop_front();
                self.cold_raw -= n;
                self.base_offset += n as u64;
                overflow = overflow.saturating_sub(n);
            } else {
                self.hot.drain(..overflow);
                self.base_offset += overflow as u64;
                break;
            }
        }
    }

    /// Monotonic high-water mark — total bytes ever written through
    /// this ring. The right offset to record after consuming a
    /// snapshot so subsequent [`Self::since`] calls return only new
    /// bytes.
    #[must_use]
    pub fn total_len(&self) -> u64 {
        self.base_offset + (self.cold_raw + self.hot.len()) as u64
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
    ///
    /// Steady-state callers (the WS pump tailing new output) land in
    /// the plain hot section and never pay for decompression; only a
    /// read reaching back into cold history (a viewer's scrollback
    /// bootstrap — once per connection) inflates chunks.
    #[must_use]
    pub fn since(&self, offset: u64) -> Vec<u8> {
        if offset >= self.total_len() {
            return Vec::new();
        }
        let start = offset.max(self.base_offset);
        let hot_start = self.base_offset + self.cold_raw as u64;
        if start >= hot_start {
            let skip = (start - hot_start) as usize;
            // Seek directly to `skip` via the deque's two contiguous
            // slices instead of `iter().skip(skip)`, which walks every
            // skipped byte one at a time. In steady state the WS pump
            // polls ~33x/s with `skip` near the end of the tail, so
            // this stays O(result) rather than O(buffer).
            let (head, tail) = self.hot.as_slices();
            let mut out = Vec::with_capacity(self.hot.len() - skip);
            if skip < head.len() {
                out.extend_from_slice(&head[skip..]);
                out.extend_from_slice(tail);
            } else {
                out.extend_from_slice(&tail[skip - head.len()..]);
            }
            return out;
        }
        let mut out = Vec::with_capacity((self.total_len() - start) as usize);
        let mut pos = self.base_offset;
        for chunk in &self.cold {
            let end = pos + u64::from(chunk.raw_len);
            if end > start {
                let raw = chunk.inflate();
                let skip = start.saturating_sub(pos) as usize;
                out.extend_from_slice(&raw[skip..]);
            }
            pos = end;
        }
        let (head, tail) = self.hot.as_slices();
        out.extend_from_slice(head);
        out.extend_from_slice(tail);
        out
    }

    /// Actual resident bytes (compressed cold + plain hot) — what the
    /// two-tier storage costs, as opposed to `total_len - base_offset`
    /// which is what it *represents*. Telemetry/test hook.
    #[allow(dead_code)]
    #[must_use]
    pub fn stored_bytes(&self) -> usize {
        self.cold.iter().map(|c| c.deflated.len()).sum::<usize>() + self.hot.len()
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
                target: crate::INPUT_TRACE_TARGET,
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

    #[test]
    fn frame_cache_hits_only_on_exact_range() {
        let mut r = PtyRing::with_capacity(1024);
        assert!(r.cached_frame(0, 10).is_none(), "empty cache misses");
        r.store_frame(0, 10, bytes::Bytes::from_static(b"\x02frame"));
        assert_eq!(r.cached_frame(0, 10).as_deref(), Some(&b"\x02frame"[..]));
        // A different range (viewer at another offset) must miss.
        assert!(r.cached_frame(5, 10).is_none());
        assert!(r.cached_frame(0, 11).is_none());
        // A newer store replaces the entry.
        r.store_frame(10, 12, bytes::Bytes::from_static(b"\x02hi"));
        assert!(r.cached_frame(0, 10).is_none());
        assert_eq!(r.cached_frame(10, 12).as_deref(), Some(&b"\x02hi"[..]));
    }

    #[test]
    fn frame_cache_skips_oversized_frames() {
        let mut r = PtyRing::with_capacity(1024);
        let big = bytes::Bytes::from(vec![0u8; PtyRing::FRAME_CACHE_MAX + 1]);
        r.store_frame(0, 1, big);
        assert!(r.cached_frame(0, 1).is_none(), "oversized frame not cached");
    }

    /// Pseudo-terminal-ish payload: repetitive enough to compress like
    /// real output, varied enough that offsets are distinguishable.
    fn payload(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| b"$ cargo build --release\r\n"[i % 25] ^ (i / 1024) as u8)
            .collect()
    }

    #[test]
    fn cold_compression_roundtrips_byte_exact() {
        let mut r = PtyRing::with_capacity(DEFAULT_CAPACITY_BYTES);
        let data = payload(PtyRing::HOT_MAX * 3);
        // Push in PTY-read-sized pieces so compaction runs mid-stream.
        for piece in data.chunks(4096) {
            r.push(piece);
        }
        assert_eq!(r.total_len(), data.len() as u64);
        assert_eq!(r.base_offset(), 0, "under cap — nothing dropped");
        assert_eq!(r.since(0), data, "cold + hot reassemble byte-exact");
        // A mid-cold-chunk offset (spans chunk boundary + hot).
        let mid = PtyRing::CHUNK_RAW as u64 / 2;
        assert_eq!(r.since(mid), data[mid as usize..]);
        // The point of the exercise: resident < represented.
        assert!(
            r.stored_bytes() < data.len() / 2,
            "stored {} for {} raw",
            r.stored_bytes(),
            data.len()
        );
    }

    #[test]
    fn hot_tail_reads_stay_plain() {
        let mut r = PtyRing::with_capacity(DEFAULT_CAPACITY_BYTES);
        let data = payload(PtyRing::HOT_MAX * 2);
        r.push(&data);
        // A tail read (the WS pump's steady state) must round-trip.
        let tail_from = r.total_len() - 100;
        assert_eq!(r.since(tail_from), data[data.len() - 100..]);
    }

    #[test]
    fn cold_eviction_advances_base_offset_by_whole_chunks() {
        // Cap big enough to compact, small enough to overflow quickly.
        let cap = PtyRing::HOT_MAX * 2 + PtyRing::CHUNK_RAW;
        let mut r = PtyRing::with_capacity(cap);
        let data = payload(cap * 2);
        for piece in data.chunks(4096) {
            r.push(piece);
        }
        assert_eq!(r.total_len(), data.len() as u64);
        let base = r.base_offset();
        assert!(base > 0, "over cap — oldest bytes dropped");
        assert!(
            (r.total_len() - base) as usize <= cap,
            "retained window fits the (uncompressed) cap"
        );
        // Whole-chunk eviction: the boundary sits on a chunk edge.
        assert_eq!(base % PtyRing::CHUNK_RAW as u64, 0);
        // Everything still retained reassembles byte-exact.
        assert_eq!(r.since(base), data[base as usize..]);
        // Aged-out offsets fall back to the surviving window.
        assert_eq!(r.since(0), data[base as usize..]);
    }

    #[test]
    fn len_mirror_tracks_total_len() {
        let mut r = PtyRing::with_capacity(1024);
        let mirror = r.total_len_handle();
        assert_eq!(mirror.load(std::sync::atomic::Ordering::Relaxed), 0);
        r.push(b"hello");
        assert_eq!(mirror.load(std::sync::atomic::Ordering::Relaxed), 5);
        r.push(&[0u8; 2048]); // oversized push still lands on total_len
        assert_eq!(mirror.load(std::sync::atomic::Ordering::Relaxed), r.total_len());
    }

    #[test]
    fn frame_cache_dropped_when_no_viewer_remains() {
        let mut r = PtyRing::with_capacity(1024);
        let viewers = r.viewers_handle();
        viewers.store(1, std::sync::atomic::Ordering::Relaxed);
        r.store_frame(0, 3, bytes::Bytes::from_static(b"\x02abc"));
        // Watched: a push keeps the cache (range no longer matches new
        // data, but sibling pumps may still be mid-flush on it).
        r.push(b"abc");
        assert!(r.cached_frame(0, 3).is_some());
        // Last viewer left: the next push frees the retained frame.
        viewers.store(0, std::sync::atomic::Ordering::Relaxed);
        r.push(b"def");
        assert!(r.cached_frame(0, 3).is_none());
    }
}
