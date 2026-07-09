// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebSocket transport for one attached tab — used by the xterm.js
//! viewer and (eventually) `tab-atelier remote attach`.
//!
//! ## Why
//!
//! The HTTP polling model (`GET /output?since=N&crc=H` every 80 ms,
//! `GET /stream?since=N` likewise) buys mid-frame freshness at the
//! cost of:
//!   - 12 fetches per second per tab per viewer at idle
//!   - header-only meta channel (lock state, schedule, agent state,
//!     outbox / inbox counts, build hash, cursor) — every poll
//!     reparses 12+ headers even when nothing changed
//!   - selection-clearing hacks on the client because every poll
//!     triggers a `term.write()` that nukes the user's selection
//!
//! A single per-tab WebSocket replaces all of that: server pushes
//! PTY bytes as soon as they arrive, state changes ride a typed
//! `meta` frame, and the client only writes into the terminal when
//! the server actually has something to say.
//!
//! ## Wire format — binary frames
//!
//! Every WS message is a binary frame. The FIRST byte is a tag, the
//! rest is the payload:
//!
//! | Tag  | Name     | Dir | Payload                                |
//! |------|----------|-----|----------------------------------------|
//! | 0x01 | in       | C→S | raw bytes typed by the user            |
//! | 0x02 | out      | S→C | raw bytes from the PTY (since last)    |
//! | 0x0A | out-gz   | S→C | gzip of an `out` payload (large frames) |
//! | 0x03 | meta     | S→C | JSON state delta (locked, schedule, …) |
//! | 0x04 | resize   | C→S | JSON `{"cols":N,"rows":M}`             |
//! | 0x07 | activate | C→S | empty — make this tab active           |
//! | 0x08 | rename   | C→S | JSON `{"name":"…"}`                    |
//! | 0x09 | close    | C→S | empty — close this tab                 |
//!
//! Ping/pong stay on tungstenite's built-in control frames; no
//! application-layer keepalive.
//!
//! ## Auth + RO
//!
//! Same model as the HTTP routes:
//!   - Master `api.token` → RW.
//!   - `share_token_rw` of the requested tab → RW.
//!   - `share_token_ro` of the requested tab → RO.
//!
//! RO connections accept `out` / `meta` from the server but the
//! server refuses every C→S frame except a connection close. We
//! close with code 1008 (policy violation) on the first violating
//! frame so a misbehaving client surfaces the problem fast.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::Full;
use hyper::{Request, Response};
use hyper_tungstenite::tungstenite::Message;
use hyper_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use hyper_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};

use crate::api::{SnapshotTab, TabSnapshot};
use crate::pty_ring::PtyRing;
use crate::schedule::LockState;

const TAG_IN: u8 = 0x01;
const TAG_OUT: u8 = 0x02;
const TAG_META: u8 = 0x03;
const TAG_RESIZE: u8 = 0x04;
const TAG_ACTIVATE: u8 = 0x07;
const TAG_RENAME: u8 = 0x08;
const TAG_CLOSE: u8 = 0x09;
/// Same semantics as [`TAG_OUT`] but the payload is gzip-compressed.
/// The client inflates, then advances its ring offset by the
/// *decompressed* length (so reconnect `since=` stays correct). Used
/// only for frames over [`COMPRESS_MIN_BYTES`] where it actually
/// shrinks them — keystroke echoes stay raw `TAG_OUT`.
const TAG_OUT_DEFLATE: u8 = 0x0A;

/// Don't bother gzipping `out` frames below this — gzip's ~18-byte
/// header + deflate framing would erase the win on tiny payloads, and
/// keystroke echoes must stay on the zero-CPU raw path. The biggest
/// beneficiary is the `since=0` scrollback replay (often MiB of highly
/// repetitive VT text → ~10-15× smaller). We also re-check the
/// compressed size and fall back to raw if it didn't actually shrink.
const COMPRESS_MIN_BYTES: usize = 256;

/// When `TAB_ATELIER_WS_DEBUG_INPUT` is set in the environment, every
/// inbound `TAG_IN` frame is logged to stderr (wall-clock ms, raw
/// bytes, printable repr, and what `ImeDedup` decided). Used to
/// diagnose the mobile-IME (`SwiftKey` / `GBoard`) duplicate-word
/// pattern (xterm.js#3600) from REAL device frames rather than guessing
/// — does `SwiftKey` send `wordswords` as one frame or two, with what gap?
/// Off by default; reading it once via `LazyLock` keeps the hot path
/// free when unset.
static WS_DEBUG_INPUT: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var_os("TAB_ATELIER_WS_DEBUG_INPUT").is_some());

/// Output is now event-driven: the pump parks on the `PtyRing`'s
/// `Notify` and flushes the moment a push lands (sub-millisecond echo),
/// so there is no output poll tick. This slower tick only drives the
/// non-latency-critical meta refresh (lock / agent / schedule / file
/// counts) and doubles as a belt-and-suspenders backstop. 100 ms is
/// plenty for state that changes on human/event timescales.
const META_POLL_MS: u64 = 100;

/// After a wake, wait this long before flushing so a burst of PTY
/// output (a `cat` flood) coalesces into one frame instead of hundreds
/// of tiny ones. 2 ms is imperceptible for a keystroke echo but caps
/// the flood frame rate at ~500/s. Inbound keystrokes only queue into
/// `pending_input` (drained on a separate 16 ms tick), so the brief
/// debounce never delays input meaningfully.
const OUTPUT_DEBOUNCE_MS: u64 = 2;

/// Per-message cap on inbound WS frames. Tungstenite's defaults
/// (16 MiB frame, 64 MiB message) are generous to a fault — an
/// authenticated RW client could send a 64 MiB `TAG_RENAME` and
/// stall every other tab while `serde_json::from_slice::<R>` parses
/// the giant payload under the snapshot lock.
///
/// 2 MiB max message + 1 MiB max frame is plenty for our use case:
/// `TAG_IN` keystrokes are a few bytes, `TAG_RESIZE` is ~16 bytes,
/// `TAG_RENAME` is a short string. A genuine paste of 2 MiB into a
/// PTY is already 1000× more text than any real-world workflow,
/// and tungstenite responds with a 1009 Message Too Big close so
/// the client surfaces the problem instead of silently truncating.
const WS_MAX_MESSAGE_BYTES: usize = 2 * 1024 * 1024;
const WS_MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Dedup window for mobile-IME `compositionupdate` + `compositionend`
/// duplicates. Android Gboard / iOS soft keyboards fire xterm.js's
/// `onData` cumulatively as the user types (`"h"`, `"he"`,
/// `"hel"`, …) then again on commit (`"hello"`) — every fire is a
/// separate WS frame, so the server receives a prefix chain and
/// the shell ends up with `"hhehelhellohello"` for one typed word.
///
/// WS itself orders frames correctly (TCP guarantee); the issue is
/// semantic, not ordering — the IME genuinely emits redundant
/// events that look identical-or-prefix to the previous send. We
/// recognise the pattern in a 200 ms window per WS connection.
const IME_DEDUP_WINDOW: Duration = Duration::from_millis(200);

/// Identify whether the path is a WS upgrade endpoint we handle.
/// Returns the tab KEY (UUID or index, as it appeared in the URL)
/// when so — the caller resolves it to an index against the live
/// snapshot the same way the HTTP routes do.
///
/// Accepts BOTH forms so the embedded Android `WebView`, which loads
/// `/tabs/<idx>/view` (index-based) and lets the share viewer's
/// `main.js` derive `/tabs/<idx>/ws` from `location.pathname`, can
/// upgrade without a server-side detour through `/tabs/by-id/...`.
#[must_use]
pub fn parse_ws_path(path: &str) -> Option<(&str, bool)> {
    let rest = path.strip_prefix("/tabs/")?;
    if let Some(rest) = rest.strip_prefix("by-id/") {
        let (uuid, suffix) = rest.split_once('/')?;
        let action = suffix.split('?').next().unwrap_or(suffix);
        if action == "ws" { Some((uuid, true)) } else { None }
    } else {
        let (idx_str, suffix) = rest.split_once('/')?;
        let action = suffix.split('?').next().unwrap_or(suffix);
        if action == "ws" && idx_str.parse::<usize>().is_ok() {
            Some((idx_str, false))
        } else {
            None
        }
    }
}

/// Outcome of the auth + lookup phase. RW gives full duplex; RO
/// makes C→S frames an instant close.
#[derive(Debug, Clone, Copy)]
enum Authz {
    Rw,
    Ro,
}

/// Per-WS-connection state that recognises the mobile-IME
/// compositionupdate + compositionend cumulative-prefix pattern.
/// Compares each new `TAG_IN` payload against the previous one,
/// within [`IME_DEDUP_WINDOW`]:
///
/// - single byte → always send (so fast desktop typing of `"aaa"`
///   is three deliberate `a`s, not one)
/// - exact match → drop (compositionend repeating what
///   compositionupdate just sent)
/// - proper prefix extension → send only the suffix (the IME
///   extended the in-progress composition; we already typed the
///   prefix into the PTY on the previous frame)
/// - anything else → send as-is
///
/// Outside the window every frame goes through unchanged. The
/// state lives on the `run_pump` stack so it's per-WS-connection,
/// matching the per-tab-per-viewer scope of IME composition.
struct ImeDedup {
    /// Printable composition accumulated since the last word boundary. The
    /// single-char frames a client emits as the user types are appended here,
    /// so a following whole-word commit frame can be recognised as a duplicate
    /// (the word-doubling bug). Also holds the last multi-byte frame, for the
    /// cumulative-prefix pattern.
    buf: Vec<u8>,
    /// True while `buf` was built by live single-char typing (an in-progress
    /// composition); false once a multi-byte frame set it. Gates the
    /// erase-preedit conversion path so a paste is never mistaken for one.
    composing: bool,
    last_at: Option<Instant>,
}

/// RAII counter for "a viewer is attached to this tab". Increments the
/// ring's viewer count on construction and decrements on drop, so the
/// count is correct across every `run_pump` exit path (clean close,
/// send/recv error, task cancellation).
struct ViewerGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl ViewerGuard {
    fn new(counter: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl ImeDedup {
    const fn new() -> Self {
        Self {
            buf: Vec::new(),
            composing: false,
            last_at: None,
        }
    }

    fn set_baseline(&mut self, bytes: &[u8], now: Instant) {
        self.buf.clear();
        self.buf.extend_from_slice(bytes);
        self.composing = false;
        self.last_at = Some(now);
    }

    /// Returns the bytes to actually inject into `pending_input`,
    /// or `None` to drop the frame entirely.
    ///
    /// Handles two client shapes that both duplicate a typed word:
    /// - **cumulative-prefix** IMEs (`"h"`,`"he"`,`"hel"`,`"hello"`) — send only
    ///   each new suffix; a re-sent identical commit drops;
    /// - **char-by-char + commit** (`"w"`,`"r"`,…,`"g"`,`"writing"`, the reported
    ///   web-viewer bug) — the single chars are accumulated in `buf` so the
    ///   whole-word commit is recognised and dropped.
    ///
    /// Plus IME **conversion** (`"8"`,`"0"`,`"€"`; dead-key `"ù"`): the commit
    /// replaces the live composition, so erase the preedit then send it.
    fn classify(&mut self, bytes: &[u8]) -> Option<Vec<u8>> {
        // Control / cursor-key sequences (arrows `\x1b[D`, Home/End, F-keys,
        // Delete `\x1b[3~`, PageUp/Down, Alt+<key>) all begin with ESC. They are
        // deliberate, rapidly-repeatable keystrokes — never IME composition.
        // Pass them straight through WITHOUT touching the dedup state, so a fast
        // burst of Left presses isn't collapsed and the composition window
        // survives an arrow press mid-word.
        if bytes.first() == Some(&0x1b) {
            return Some(bytes.to_vec());
        }
        let now = Instant::now();
        let within = self.last_at.is_some_and(|t| now.duration_since(t) < IME_DEDUP_WINDOW);

        // A single-byte frame is one ASCII key. Word boundaries (space, Enter,
        // other C0 controls, DEL) end the composition; other printable chars
        // extend it. Single chars ALWAYS pass through unchanged — fast typing of
        // `"aaa"` is three deliberate `a`s, not a duplicate.
        if bytes.len() == 1 {
            let b = bytes[0];
            if b <= b' ' || b == 0x7f {
                self.buf.clear();
                self.composing = false;
            } else {
                self.buf.push(b);
                self.composing = true;
            }
            self.last_at = Some(now);
            return Some(bytes.to_vec());
        }

        // Multi-byte frame: a commit / cumulative-prefix re-send, or a paste.
        // Only correlate with the composition when it lands inside the window
        // (a commit fires within ms of the last char); otherwise it's new.
        if within && !self.buf.is_empty() {
            if self.buf.as_slice() == bytes {
                // Exact re-send of what we already typed → drop. Keep `buf` so a
                // 2nd/3rd identical commit in the window also drops (emoji `😁`).
                self.last_at = Some(now);
                return None;
            }
            if bytes.starts_with(&self.buf) {
                // Cumulative-prefix commit extends the composition → send only
                // the new suffix.
                let suffix = bytes[self.buf.len()..].to_vec();
                self.set_baseline(bytes, now);
                return Some(suffix);
            }
            if self.composing {
                // The commit REPLACES a live single-char composition (IME
                // conversion: `"80"`→`"€"`, dead-key `"ù"`). Erase the preedit we
                // already injected (one DEL per char), then send the commit.
                // Gated on `composing`, so a paste right after typing — `buf`
                // set from a prior multi-byte frame — is never erased.
                let preedit_chars = std::str::from_utf8(&self.buf).map_or(self.buf.len(), |s| s.chars().count());
                let mut out = vec![0x7f; preedit_chars];
                out.extend_from_slice(bytes);
                self.set_baseline(bytes, now);
                return Some(out);
            }
        }
        // Fresh / paste / two distinct commits → send as-is, becoming the new
        // baseline for any following cumulative-prefix extension.
        self.set_baseline(bytes, now);
        Some(bytes.to_vec())
    }
}

/// Resolve `(token, uuid)` to `(authorisation, ring)` in a SINGLE
/// snapshot lock. Master token wins (RW); per-tab share tokens are
/// matched in constant time, same as the HTTP gate.
///
/// Previously this function returned only `(idx, Authz)` and the
/// caller re-locked to clone the `pty_ring`. Between those two
/// locks a tab close on a different connection could shift the
/// vector and the second lookup would attach to the wrong tab's
/// ring — a S→C mis-routing race. By doing both lookups under one
/// guard we close that window. None on any failure (unknown tab,
/// bad token, missing token, no ring).
// The `?` early-returns inside the scoped block hold the snapshot
// guard until the borrowed `t` releases at the close brace; that's
// the tightest possible window. `significant_drop_tightening` flags
// it anyway because it can't model the dependency.
#[allow(clippy::significant_drop_tightening)]
fn authorise_and_ring(
    state: &Arc<Mutex<TabSnapshot>>,
    master: &str,
    key: &str,
    is_uuid: bool,
    provided: &[u8],
) -> Option<(Authz, Arc<Mutex<PtyRing>>, String)> {
    let (rw_tok, ro_tok, ring, uuid) = {
        let snap = state.lock().ok()?;
        let t = if is_uuid {
            snap.tabs.iter().find(|t| t.id == key)?
        } else {
            let idx: usize = key.parse().ok()?;
            snap.tabs.get(idx)?
        };
        let ring = t.pty_ring.clone()?;
        (t.share_token_rw.clone(), t.share_token_ro.clone(), ring, t.id.clone())
    };
    let master_match = crate::api::constant_time_eq(provided, master.as_bytes());
    let rw_match = !rw_tok.is_empty() && crate::api::constant_time_eq(rw_tok.as_bytes(), provided);
    let ro_match = !ro_tok.is_empty() && crate::api::constant_time_eq(ro_tok.as_bytes(), provided);
    if master_match || rw_match {
        Some((Authz::Rw, ring, uuid))
    } else if ro_match {
        Some((Authz::Ro, ring, uuid))
    } else {
        None
    }
}

/// Pull the token off the request — query `?token=...` first (the
/// browser-friendly form, since JS can't set Authorization on a WS
/// upgrade), then `Authorization: Bearer ...` as a fallback.
/// Returns the raw decoded bytes so the caller's constant-time
/// comparison runs on the exact wire input — see `percent_decode`'s
/// docstring for the reason this is `Vec<u8>` and not `String`.
fn extract_token<B>(req: &Request<B>) -> Option<Vec<u8>> {
    let q = req.uri().query()?;
    for pair in q.split('&') {
        if let Some(v) = pair.strip_prefix("token=") {
            return Some(percent_decode(v));
        }
    }
    let h = req.headers().get(hyper::header::AUTHORIZATION)?;
    let s = h.to_str().ok()?;
    s.strip_prefix("Bearer ").map(|t| t.as_bytes().to_vec())
}

/// Origin / CSRF defense-in-depth on the WS upgrade.
///
/// Browsers do NOT apply same-origin policy to WebSocket handshakes
/// the way they do to `fetch()` — any page can open
/// `new WebSocket("ws://victim/...")` from any origin and the
/// browser will send the upgrade. The token (128-bit, constant-time)
/// is the real auth gate, but tokens leak (referer headers, browser
/// history, screenshares, `ps` listings, …) and a leaked token
/// without an Origin check means any malicious tab in the user's
/// browser can attach to their terminal.
///
/// Check: Origin's host:port must match the Host header (or the
/// X-Forwarded-Host of a proxy). Missing Origin → accept (CLI
/// clients, server-to-server, file://).
///
/// A `null` Origin is REJECTED: it's what sandboxed iframes
/// (`sandbox="allow-scripts"`), `data:`/`file:` documents, and some
/// privacy proxies send, and accepting it would let such a context
/// (holding a leaked token) open the input WS. Non-browser clients
/// that legitimately need access simply omit Origin entirely, which is
/// still accepted above.
fn origin_ok<B>(req: &Request<B>) -> bool {
    let Some(origin) = req.headers().get("origin") else {
        return true; // no Origin = not a browser request
    };
    let Ok(origin_str) = origin.to_str() else {
        return false;
    };
    if origin_str == "null" {
        return false;
    }
    let origin_host = origin_str
        .strip_prefix("https://")
        .or_else(|| origin_str.strip_prefix("http://"))
        .unwrap_or(origin_str)
        .split('/')
        .next()
        .unwrap_or("");
    // Compare against X-Forwarded-Host first (proxy-aware), then Host.
    for hdr in ["x-forwarded-host", "host"] {
        if let Some(v) = req.headers().get(hdr)
            && let Ok(s) = v.to_str()
            && s == origin_host
        {
            return true;
        }
    }
    false
}

/// Pull `?since=N` off the URL — the viewer's bootstrap or resume
/// offset. Missing / unparseable → 0 (= replay the entire ring).
fn extract_since<B>(req: &Request<B>) -> u64 {
    let Some(q) = req.uri().query() else { return 0 };
    for pair in q.split('&') {
        if let Some(v) = pair.strip_prefix("since=")
            && let Ok(n) = v.parse::<u64>()
        {
            return n;
        }
    }
    0
}

/// Minimal percent-decode for the `?token=` query value.
///
/// Returns `Vec<u8>` (NOT `String`) because tokens are byte strings,
/// not text. The previous `String`-based version round-tripped each
/// byte through `u8 as char` + `String::push`, which UTF-8-encodes
/// any code point ≥ 0x80 into a 2-byte sequence and silently
/// corrupts the wire input:
///
/// ```text
/// %FF  →  '\u{00FF}'  →  String.push  →  "\xC3\xBF"  (2 bytes, WRONG)
/// ```
///
/// vs. what the wire actually sent:
///
/// ```text
/// %FF  →  byte 0xFF   →  Vec<u8>.push →  "\xFF"      (1 byte, correct)
/// ```
///
/// Tokens today are 32 hex chars (every byte ≤ 0x66) so this never
/// fires in practice, but it's a latent corruption waiting for the
/// first non-ASCII byte routed through the decoder.
fn percent_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    out
}

/// JSON snapshot of the per-tab state the client cares about.
/// Re-emitted as a `meta` frame whenever anything in here changes
/// (or on first connect).
///
/// Deliberately excludes anything that mutates every tick — the
/// frame goes out via the change-detection hash in `run_pump`, so
/// including a monotonic counter (uptime, sequence id) would force
/// a meta frame every 30 ms forever, defeating the
/// only-on-change push model. If a future viewer needs uptime,
/// expose it on a separate per-second tick channel.
#[derive(serde::Serialize, PartialEq)]
struct MetaSnapshot {
    name: String,
    cols: u16,
    rows: u16,
    locked: bool,
    lock_reason: Option<&'static str>,
    schedule_tz: Option<String>,
    schedule_rule: Option<String>,
    /// RFC 3339 UTC instant of the next schedule transition, if any.
    schedule_next: Option<String>,
    bg_color: Option<String>,
    agent_state: Option<&'static str>,
    agent_label: Option<String>,
    outbox_count: usize,
    inbox_count: usize,
    build_hash: &'static str,
}

/// Per-connection cache of the tab's inbox/outbox file counts. Each
/// count is a `read_dir` + a `stat` per entry on the tab's cwd —
/// blocking fs I/O that used to run inside [`snapshot_meta`] on every
/// 100 ms meta tick *while holding the global snapshot mutex*, on the
/// single-threaded runtime every connection shares. A big inbox (or a
/// slow/NFS cwd) stalled the whole API surface 10×/s per viewer. The
/// counts change on human timescales, so re-scan at most once per
/// second, always outside the lock.
struct DirCountCache {
    outbox: usize,
    inbox: usize,
    scanned_at: Option<Instant>,
}

impl DirCountCache {
    const TTL: Duration = Duration::from_secs(1);

    const fn new() -> Self {
        Self {
            outbox: 0,
            inbox: 0,
            scanned_at: None,
        }
    }

    fn refresh(&mut self, cwd: Option<&str>, authz: Authz) {
        if self.scanned_at.is_some_and(|t| t.elapsed() < Self::TTL) {
            return;
        }
        self.scanned_at = Some(Instant::now());
        let dir_count = |dirname: &str| -> usize {
            cwd.map_or(0, |cwd| {
                std::fs::read_dir(std::path::Path::new(cwd).join(dirname)).map_or(0, |rd| {
                    rd.flatten().filter(|e| e.metadata().is_ok_and(|m| m.is_file())).count()
                })
            })
        };
        self.outbox = dir_count("outbox");
        // RO viewers don't see `inbox_count`. The HTTP `/inbox` listing
        // endpoint is RW-only specifically so RO recipients can't
        // enumerate uploads (`src/api.rs` needs_rw includes "inbox"); a
        // count here would be a milder version of the same info leak.
        // `outbox_count` is fine for RO — downloads are allowed.
        self.inbox = if matches!(authz, Authz::Ro) {
            0
        } else {
            dir_count("inbox")
        };
    }
}

/// Snapshot the lock-guarded meta fields. `outbox_count` / `inbox_count`
/// are left at 0 — [`current_meta`] fills them from the [`DirCountCache`]
/// after the snapshot lock is dropped (no fs I/O belongs under it).
fn snapshot_meta(t: &SnapshotTab) -> MetaSnapshot {
    let lock_reason = t.lock_reason();
    let (agent_state, agent_label) = t.agent_state.as_ref().map_or((None, None), |s| {
        let key = match s.state {
            crate::AgentState::Thinking => "thinking",
            crate::AgentState::Waiting => "waiting",
            crate::AgentState::Error => "error",
        };
        (Some(key), s.label.clone())
    });
    let (schedule_tz, schedule_rule, schedule_next) = t.schedule.as_ref().map_or((None, None, None), |s| {
        let next = s.next_change_from_now().map(|d| d.to_rfc3339());
        (Some(s.tz.clone()), Some(s.rule.clone()), next)
    });
    MetaSnapshot {
        name: t.name.clone(),
        cols: t.cols,
        rows: t.rows,
        locked: t.effective_locked(),
        lock_reason,
        schedule_tz,
        schedule_rule,
        schedule_next,
        bg_color: (!t.bg_color.is_empty()).then(|| t.bg_color.clone()),
        agent_state,
        agent_label,
        outbox_count: 0,
        inbox_count: 0,
        build_hash: crate::api::BUILD_HASH,
    }
}

/// Entry point — called from `handle_hyper_request` in api.rs after
/// it has matched `/tabs/<key>/ws` (UUID or index) via
/// `parse_ws_path`. If the upgrade is accepted, returns the 101
/// response immediately and spawns the WS pump in the background;
/// if auth fails, returns a 401 / 404 without upgrading.
#[allow(clippy::needless_pass_by_value)]
pub fn handle_upgrade(
    mut req: Request<hyper::body::Incoming>,
    state: Arc<Mutex<TabSnapshot>>,
    master_token: &str,
    read_only_process: bool,
    key: String,
    is_uuid: bool,
) -> Response<Full<Bytes>> {
    if !hyper_tungstenite::is_upgrade_request(&req) {
        return text_response(400, "expected websocket upgrade");
    }
    // Defense-in-depth Origin check before any auth lookup. See
    // `origin_ok` docstring — token is the real gate, this just
    // makes a malicious-page-with-leaked-token harder to land.
    if !origin_ok(&req) {
        return text_response(403, "origin mismatch");
    }
    let Some(provided) = extract_token(&req) else {
        return text_response(401, "missing token");
    };
    let Some((authz, ring, uuid)) = authorise_and_ring(&state, master_token, &key, is_uuid, &provided) else {
        // Same response for "no such tab" + "bad token" so a
        // probe can't distinguish; mirrors the HTTP gate.
        return text_response(401, "invalid or missing token");
    };
    // `?since=N` on the URL — start streaming from offset N. The
    // viewer passes 0 on initial connect to bootstrap full history;
    // on reconnect it passes the last byte it received. Missing /
    // junk → 0.
    let since = extract_since(&req);
    // Bound inbound message size so a single rogue frame can't
    // RAM-exhaust the daemon under the snapshot lock. See
    // WS_MAX_MESSAGE_BYTES / WS_MAX_FRAME_BYTES docstrings.
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(WS_MAX_MESSAGE_BYTES))
        .max_frame_size(Some(WS_MAX_FRAME_BYTES));
    let (response, ws_future) = match hyper_tungstenite::upgrade(&mut req, Some(ws_config)) {
        Ok(p) => p,
        Err(e) => return text_response(400, &format!("ws upgrade: {e}")),
    };
    tokio::spawn(async move {
        if let Ok(ws) = ws_future.await {
            run_pump(ws, state, uuid, authz, ring, read_only_process, since).await;
        }
    });
    response
}

fn text_response(status: u16, msg: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .header("x-robots-tag", "noindex, nofollow, noarchive")
        .body(Full::new(Bytes::from(msg.to_string())))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

/// The main loop. Holds the WS stream open, pushes `out` + `meta`
/// frames on each tick when something changed, and dispatches C→S
/// frames into the snapshot's pending queues.
async fn run_pump(
    ws: hyper_tungstenite::HyperWebsocketStream,
    state: Arc<Mutex<TabSnapshot>>,
    uuid: String,
    authz: Authz,
    ring: Arc<Mutex<PtyRing>>,
    read_only_process: bool,
    since: u64,
) {
    let (mut sink, mut stream) = ws.split();
    // Start streaming from `since`. The first tick will read this
    // offset to total_len, ship the bytes in one `out` frame, and
    // advance. since=0 ⇒ replay the entire ring on connect (viewer
    // bootstrap). since=N ⇒ resume from N (reconnect after a drop).
    let mut ring_offset: u64 = since;
    let mut ime_dedup = ImeDedup::new();

    // Send the initial meta on connect and keep the last one so the
    // first tick after this doesn't re-emit the same payload. We hold
    // the whole `MetaSnapshot` and compare structurally rather than
    // re-serialising + hashing it on every 30 ms tick — the meta only
    // changes on a lock/agent/schedule/file-count event, so the common
    // case is an allocation-free `==` against the cached value.
    let mut dir_counts = DirCountCache::new();
    let mut last_meta: Option<MetaSnapshot> = if let Some(meta) = current_meta(&state, &uuid, authz, &mut dir_counts) {
        let bytes = encode_frame(TAG_META, serde_json::to_vec(&meta).unwrap_or_default());
        if sink.send(Message::Binary(bytes.into())).await.is_err() {
            return;
        }
        Some(meta)
    } else {
        None
    };

    // Event-driven output: wake on a `PtyRing` push and flush
    // immediately. `notify` is cloned from the ring once up front, along
    // with the viewer-count handle.
    let (notify, viewers) = {
        let Ok(r) = ring.lock() else {
            return;
        };
        (r.notifier(), r.viewers_handle())
    };
    // Count this connection as a viewer for its whole lifetime. The
    // guard's Drop decrements on every exit path (clean close, error,
    // task cancel), so a crashed viewer can't leak a phantom count.
    let _viewer = ViewerGuard::new(viewers);
    let mut meta_tick = tokio::time::interval(Duration::from_millis(META_POLL_MS));
    meta_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // Arm the wake BEFORE flushing: `enable()` registers the waiter
        // now, so a push that lands during the flush below is captured
        // (the next loop iteration's flush sends it) — no lost wakeup.
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        // Flush any new ring bytes. Idempotent — only the suffix since
        // `ring_offset` — so a spurious wake is a cheap no-op. The lock
        // is dropped before we await the sink so the PTY-read side
        // (pty_ring) is never blocked while we talk to the socket.
        let chunk = {
            let Ok(r) = ring.lock() else {
                return;
            };
            let new_total = r.total_len();
            if new_total == ring_offset {
                Vec::new()
            } else {
                let bytes = r.since(ring_offset);
                ring_offset = new_total;
                bytes
            }
        };
        if !chunk.is_empty() {
            let frame = encode_out_frame(chunk);
            if sink.send(Message::Binary(frame.into())).await.is_err() {
                return;
            }
        }

        tokio::select! {
            biased;
            // Inbound first — keystrokes are latency-critical (they
            // just queue into pending_input, so this is quick).
            msg = stream.next() => {
                let Some(Ok(msg)) = msg else { return; };
                match msg {
                    Message::Binary(b) => {
                        if let Err(close) = handle_inbound(&b, authz, read_only_process, &state, &uuid, &mut ime_dedup) {
                            let _ = sink.send(Message::Close(Some(close))).await;
                            return;
                        }
                    }
                    Message::Ping(p) => {
                        let _ = sink.send(Message::Pong(p)).await;
                    }
                    Message::Close(_) => return,
                    _ => {} // Text / Pong / Frame — ignore.
                }
            }
            // New PTY output: brief debounce to coalesce a burst, then
            // loop back to flush.
            () = &mut notified => {
                tokio::time::sleep(Duration::from_millis(OUTPUT_DEBOUNCE_MS)).await;
            }
            // Meta frame — only when something actually changed.
            // Structural compare; serialise only on a real diff.
            _ = meta_tick.tick() => {
                if let Some(meta) = current_meta(&state, &uuid, authz, &mut dir_counts)
                    && last_meta.as_ref() != Some(&meta)
                {
                    let bytes = encode_frame(TAG_META, serde_json::to_vec(&meta).unwrap_or_default());
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        return;
                    }
                    last_meta = Some(meta);
                }
            }
        }
    }
}

fn encode_frame(tag: u8, mut payload: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(tag);
    out.append(&mut payload);
    out
}

/// Build an `out` frame, gzipping the payload when it's large enough to
/// benefit ([`COMPRESS_MIN_BYTES`]) AND the gzip actually shrinks it.
/// Otherwise send raw [`TAG_OUT`]. Keystroke echoes (tiny) always take
/// the raw, zero-CPU path; the big win is the scrollback replay.
fn encode_out_frame(chunk: Vec<u8>) -> Vec<u8> {
    if chunk.len() >= COMPRESS_MIN_BYTES
        && let Some(gz) = gzip(&chunk)
        && gz.len() < chunk.len()
    {
        return encode_frame(TAG_OUT_DEFLATE, gz);
    }
    encode_frame(TAG_OUT, chunk)
}

/// gzip `data` at a fast level (terminal text compresses well even at
/// level 1, and this can sit on the output hot path). `None` on the
/// practically-impossible encoder error so the caller falls back to raw.
fn gzip(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(data.len() / 3 + 32), flate2::Compression::fast());
    enc.write_all(data).ok()?;
    enc.finish().ok()
}

/// Dispatch a single C→S frame into the snapshot's pending queues.
/// Returns `Err(CloseFrame)` when the client violated the protocol
/// (RO trying to write, unknown tag, malformed JSON) and the
/// connection should close.
///
/// Debug dump of one inbound `TAG_IN` frame (gated by [`WS_DEBUG_INPUT`]).
/// Prints a wall-clock millisecond timestamp so consecutive frames'
/// inter-arrival gap is visible — the key question for the `SwiftKey`
/// duplicate-word bug (one doubled frame vs two frames within the
/// dedup window).
fn debug_log_input(idx: usize, raw: &[u8], decision: Option<&[u8]>) {
    use std::fmt::Write as _;
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let mut hex = String::with_capacity(raw.len() * 3);
    for b in raw {
        let _ = write!(hex, "{b:02x} ");
    }
    let repr: String = raw
        .iter()
        .map(|&b| if (0x20..=0x7e).contains(&b) { b as char } else { '.' })
        .collect();
    let act = match decision {
        None => "DROP".to_owned(),
        Some(e) if e == raw => "send-as-is".to_owned(),
        Some(e) => format!("send-suffix({}/{})", e.len(), raw.len()),
    };
    eprintln!(
        "[ws-in tab={idx} t={t}] len={} hex=[{}] repr=\"{repr}\" -> {act}",
        raw.len(),
        hex.trim_end(),
    );
}

fn handle_inbound(
    bytes: &[u8],
    authz: Authz,
    read_only_process: bool,
    state: &Arc<Mutex<TabSnapshot>>,
    uuid: &str,
    ime_dedup: &mut ImeDedup,
) -> Result<(), CloseFrame> {
    let Some((&tag, payload)) = bytes.split_first() else {
        return Err(CloseFrame {
            code: CloseCode::Protocol,
            reason: "empty frame".into(),
        });
    };
    // Every C→S tag is a mutation. RO tokens + process-level
    // --read-only both refuse with policy violation.
    if matches!(authz, Authz::Ro) || read_only_process {
        return Err(CloseFrame {
            code: CloseCode::Policy,
            reason: "read-only".into(),
        });
    }
    let Ok(mut snap) = state.lock() else {
        return Err(CloseFrame {
            code: CloseCode::Error,
            reason: "snapshot poisoned".into(),
        });
    };
    let Some(idx) = snap.tabs.iter().position(|t| t.id == uuid) else {
        return Err(CloseFrame {
            code: CloseCode::Away,
            reason: "tab vanished".into(),
        });
    };
    // Any C→S frame counts as activity — keeps the input-drain loops
    // on their fast tick while a viewer is interacting.
    snap.touch();
    match tag {
        TAG_IN => {
            // Refuse input if the tab is effective-locked (manual or
            // schedule). Mirrors the HTTP /input gate so a stale
            // viewer can't race past a fresh lock.
            if snap.tabs[idx].effective_locked() {
                return Err(CloseFrame {
                    code: CloseCode::Policy,
                    reason: "tab locked".into(),
                });
            }
            // Recognise mobile-IME compositionupdate + commit
            // duplicates BEFORE queueing into pending_input. None
            // ⇒ the IME re-sent a payload we already injected;
            // drop silently so the shell doesn't see the
            // accumulated `"hhehelhellohello"` mess.
            let decision = ime_dedup.classify(payload);
            if *WS_DEBUG_INPUT {
                debug_log_input(idx, payload, decision.as_deref());
            }
            if let Some(effective) = decision
                && !effective.is_empty()
            {
                snap.pending_input.push((idx, effective));
            }
        }
        TAG_RESIZE => {
            // {"cols":N,"rows":M} — the snapshot itself doesn't carry
            // a resize queue today (the desktop GUI / headless drives
            // dims), so v1 logs the request and drops it. Future:
            // wire a pending_resizes queue analogous to
            // pending_lock_changes.
            #[derive(serde::Deserialize)]
            struct R {
                cols: u16,
                rows: u16,
            }
            if let Ok(r) = serde_json::from_slice::<R>(payload) {
                log::debug!("ws resize for tab {idx}: {}x{} (no-op v1)", r.cols, r.rows);
            }
        }
        TAG_ACTIVATE => {
            snap.pending_activate = Some(idx);
        }
        TAG_RENAME => {
            #[derive(serde::Deserialize)]
            struct R {
                name: String,
            }
            if let Ok(r) = serde_json::from_slice::<R>(payload) {
                snap.pending_renames.push((idx, r.name));
            } else {
                return Err(CloseFrame {
                    code: CloseCode::Protocol,
                    reason: "bad rename payload".into(),
                });
            }
        }
        TAG_CLOSE => {
            snap.pending_closes.push(idx);
        }
        other => {
            return Err(CloseFrame {
                code: CloseCode::Protocol,
                reason: format!("unknown tag {other:#04x}").into(),
            });
        }
    }
    Ok(())
}

fn current_meta(
    state: &Arc<Mutex<TabSnapshot>>,
    uuid: &str,
    authz: Authz,
    counts: &mut DirCountCache,
) -> Option<MetaSnapshot> {
    let snap = state.lock().ok()?;
    let t = snap.tabs.iter().find(|t| t.id == uuid)?;
    let mut meta = snapshot_meta(t);
    let cwd = t.cwd.clone();
    drop(snap);
    // fs work strictly after the snapshot lock is dropped.
    counts.refresh(cwd.as_deref(), authz);
    meta.outbox_count = counts.outbox;
    meta.inbox_count = counts.inbox;
    Some(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_snapshot_structural_eq_detects_change() {
        // The WS pump emits a meta frame only when the snapshot differs
        // from the last one sent. This replaced a serialize+hash; verify
        // the derived `PartialEq` distinguishes the fields that matter.
        let mk = |name: &str, locked: bool| MetaSnapshot {
            name: name.into(),
            cols: 80,
            rows: 24,
            locked,
            lock_reason: if locked { Some("manual") } else { None },
            schedule_tz: None,
            schedule_rule: None,
            schedule_next: None,
            bg_color: None,
            agent_state: None,
            agent_label: None,
            outbox_count: 0,
            inbox_count: 0,
            build_hash: crate::api::BUILD_HASH,
        };
        assert!(
            mk("shell", false) == mk("shell", false),
            "identical snapshots must compare equal"
        );
        assert!(
            mk("shell", false) != mk("shell", true),
            "a lock change must be detected"
        );
        assert!(mk("shell", false) != mk("build", false), "a rename must be detected");
    }

    #[test]
    fn parse_ws_path_accepts_uuid_and_index_forms() {
        assert_eq!(parse_ws_path("/tabs/by-id/abc-123/ws"), Some(("abc-123", true)));
        assert_eq!(parse_ws_path("/tabs/by-id/abc-123/ws?token=x"), Some(("abc-123", true)));
        // Index form — used by the Android WebView whose share-viewer
        // URL is `/tabs/<idx>/view` and whose main.js derives the WS
        // path from `location.pathname`.
        assert_eq!(parse_ws_path("/tabs/0/ws"), Some(("0", false)));
        assert_eq!(parse_ws_path("/tabs/12/ws?token=x"), Some(("12", false)));
    }

    #[test]
    fn parse_ws_path_rejects_other_endpoints() {
        assert_eq!(parse_ws_path("/tabs/by-id/abc/output"), None);
        assert_eq!(parse_ws_path("/tabs/by-id/abc"), None);
        // Non-numeric index isn't a tab.
        assert_eq!(parse_ws_path("/tabs/foo/ws"), None);
        assert_eq!(parse_ws_path("/"), None);
    }

    #[test]
    fn percent_decode_handles_plus_and_hex() {
        assert_eq!(percent_decode("a+b"), b"a b");
        assert_eq!(percent_decode("a%2Bb"), b"a+b");
        assert_eq!(percent_decode("plain"), b"plain");
        // Malformed escape is passed through.
        assert_eq!(percent_decode("a%ZZ"), b"a%ZZ");
    }

    fn req_with_headers(headers: &[(&str, &str)]) -> hyper::Request<()> {
        let mut b = hyper::Request::builder().uri("/").method("GET");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(()).unwrap()
    }

    #[test]
    fn origin_ok_no_origin_accepted() {
        // CLI / server-to-server requests: no Origin → allow.
        assert!(origin_ok(&req_with_headers(&[])));
    }

    #[test]
    fn origin_ok_null_origin_rejected() {
        // `null` Origin (sandboxed iframes, file://, data:) must be
        // refused — a leaked token in such a context shouldn't be able
        // to open the input WS. Legit non-browser clients omit Origin
        // entirely (covered by origin_ok_no_origin_accepted).
        assert!(!origin_ok(&req_with_headers(&[
            ("origin", "null"),
            ("host", "x.example")
        ])));
    }

    #[test]
    fn origin_ok_matching_host_accepted() {
        assert!(origin_ok(&req_with_headers(&[
            ("origin", "https://amaury.wdes.eu"),
            ("host", "amaury.wdes.eu"),
        ])));
        assert!(origin_ok(&req_with_headers(&[
            ("origin", "http://192.168.27.77:7890"),
            ("host", "192.168.27.77:7890"),
        ])));
    }

    #[test]
    fn origin_ok_mismatched_host_rejected() {
        assert!(!origin_ok(&req_with_headers(&[
            ("origin", "https://attacker.evil"),
            ("host", "amaury.wdes.eu"),
        ])));
    }

    #[test]
    fn origin_ok_falls_back_to_forwarded_host_for_proxies() {
        assert!(origin_ok(&req_with_headers(&[
            ("origin", "https://amaury.wdes.eu"),
            ("host", "127.0.0.1:7890"),
            ("x-forwarded-host", "amaury.wdes.eu"),
        ])));
    }

    #[test]
    fn percent_decode_preserves_high_bytes_one_for_one() {
        // The regression: %FF was being re-encoded as 0xC3 0xBF
        // (UTF-8 of U+00FF) via the now-removed u8-as-char round-trip.
        assert_eq!(percent_decode("%FF"), vec![0xFF]);
        assert_eq!(percent_decode("%80"), vec![0x80]);
        assert_eq!(percent_decode("%C3%BF"), vec![0xC3, 0xBF]);
    }

    #[test]
    fn encode_frame_prepends_tag_byte() {
        let f = encode_frame(0x02, b"hello".to_vec());
        assert_eq!(f, b"\x02hello");
    }

    #[test]
    fn ime_dedup_passes_single_byte_keystrokes() {
        // Desktop fast typing of "aaa" must NOT get deduped — even
        // though the bytes match, single-byte input is whitelisted.
        let mut d = ImeDedup::new();
        assert_eq!(d.classify(b"a"), Some(b"a".to_vec()));
        assert_eq!(d.classify(b"a"), Some(b"a".to_vec()));
        assert_eq!(d.classify(b"a"), Some(b"a".to_vec()));
    }

    #[test]
    fn ime_dedup_keeps_doubled_letters_and_repeated_backspace() {
        // Regression: a per-character mobile IME fires a separate
        // composition for every letter, so typing "William" arrives as
        // single bytes W i l l i a m — the two 'l's are a real double,
        // not an echo, and MUST both reach the PTY. Likewise holding
        // Backspace sends 0x7f repeatedly. A client-side "drop the
        // identical key within 300 ms" guard broke both; single-byte
        // whitelisting here is what keeps them working.
        let mut d = ImeDedup::new();
        for b in b"William" {
            assert_eq!(d.classify(&[*b]), Some(vec![*b]), "byte {b:#x} dropped");
        }
        // Repeated Backspace (DEL, 0x7f) — every press must pass.
        assert_eq!(d.classify(b"\x7f"), Some(b"\x7f".to_vec()));
        assert_eq!(d.classify(b"\x7f"), Some(b"\x7f".to_vec()));
        assert_eq!(d.classify(b"\x7f"), Some(b"\x7f".to_vec()));
    }

    #[test]
    fn ime_dedup_drops_exact_multibyte_repeat() {
        // compositionupdate("hello") + compositionend("hello") =
        // two identical multi-byte frames within the window.
        let mut d = ImeDedup::new();
        assert_eq!(d.classify(b"hello"), Some(b"hello".to_vec()));
        assert_eq!(d.classify(b"hello"), None);
    }

    #[test]
    fn viewer_guard_counts_connections() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let n = Arc::new(AtomicUsize::new(0));
        {
            let _a = ViewerGuard::new(n.clone());
            assert_eq!(n.load(Ordering::Relaxed), 1);
            let b = ViewerGuard::new(n.clone());
            assert_eq!(n.load(Ordering::Relaxed), 2, "two viewers");
            drop(b);
            assert_eq!(n.load(Ordering::Relaxed), 1, "one disconnected");
        }
        // Both guards dropped (incl. error/cancel paths) → back to zero.
        assert_eq!(n.load(Ordering::Relaxed), 0, "no phantom viewers leak");
    }

    #[test]
    fn ime_dedup_passes_rapid_escape_sequences() {
        // Cursor / control keys all start with ESC and are deliberate,
        // rapidly-repeatable keystrokes — they must NEVER be deduped.
        // Regression: holding/spamming Left (`\x1b[D`) in the xterm.js
        // viewer dropped every repeat after the first; only a >window
        // pause let the next one through.
        let mut d = ImeDedup::new();
        assert_eq!(d.classify(b"\x1b[D"), Some(b"\x1b[D".to_vec()));
        assert_eq!(d.classify(b"\x1b[D"), Some(b"\x1b[D".to_vec()));
        assert_eq!(d.classify(b"\x1b[D"), Some(b"\x1b[D".to_vec()));
        // Home/End and other CSI keys too.
        assert_eq!(d.classify(b"\x1b[H"), Some(b"\x1b[H".to_vec()));
        assert_eq!(d.classify(b"\x1b[H"), Some(b"\x1b[H".to_vec()));
        // …and an escape key in the middle must not corrupt a real IME
        // prefix cascade that resumes after it.
        let mut d2 = ImeDedup::new();
        assert_eq!(d2.classify(b"he"), Some(b"he".to_vec()));
        assert_eq!(d2.classify(b"\x1b[D"), Some(b"\x1b[D".to_vec()));
        assert_eq!(d2.classify(b"hel"), Some(b"l".to_vec()));
    }

    #[test]
    fn ime_dedup_extracts_suffix_on_prefix_extension() {
        // The Android compose cascade — each frame is the cumulative
        // composition. We already typed "he"; the next frame "hel"
        // should only inject "l", not "hel".
        let mut d = ImeDedup::new();
        assert_eq!(d.classify(b"he"), Some(b"he".to_vec()));
        assert_eq!(d.classify(b"hel"), Some(b"l".to_vec()));
        assert_eq!(d.classify(b"hello"), Some(b"lo".to_vec()));
    }

    #[test]
    fn ime_dedup_window_expires() {
        // After the dedup window, even an exact match flows through —
        // the user genuinely typed the same word twice.
        let mut d = ImeDedup::new();
        assert_eq!(d.classify(b"hello"), Some(b"hello".to_vec()));
        // Force the timestamp to look stale.
        d.last_at = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(500))
                .expect("monotonic clock just stepped backwards 500 ms during a test"),
        );
        assert_eq!(d.classify(b"hello"), Some(b"hello".to_vec()));
    }

    #[test]
    fn ime_dedup_unrelated_multibyte_flows_through() {
        // Two different words within the window — both must be sent.
        let mut d = ImeDedup::new();
        assert_eq!(d.classify(b"hello"), Some(b"hello".to_vec()));
        assert_eq!(d.classify(b"world"), Some(b"world".to_vec()));
    }

    #[test]
    fn ime_dedup_drops_char_by_char_word_commit() {
        // The reported web-viewer bug: the client sends each character as its
        // own frame AS TYPED, then re-sends the whole word on `compositionend`.
        // The single chars are accumulated so the commit is recognised + dropped
        // (otherwise the shell gets "writingwriting"). Captured live sequence.
        let mut d = ImeDedup::new();
        for c in b"writing" {
            assert_eq!(d.classify(&[*c]), Some(vec![*c]), "char {c:#x} passes through");
        }
        assert_eq!(d.classify(b"writing"), None, "whole-word commit dropped");
    }

    #[test]
    fn ime_dedup_word_boundary_resets_composition() {
        // The commit fires right before the space; the space then resets the
        // composition so the next word starts clean. Captured "am _ to" shape.
        let mut d = ImeDedup::new();
        assert_eq!(d.classify(b"a"), Some(b"a".to_vec()));
        assert_eq!(d.classify(b"m"), Some(b"m".to_vec()));
        assert_eq!(d.classify(b"am"), None, "commit of first word dropped");
        assert_eq!(d.classify(b" "), Some(b" ".to_vec()), "space passes + resets");
        assert_eq!(d.classify(b"t"), Some(b"t".to_vec()));
        assert_eq!(d.classify(b"o"), Some(b"o".to_vec()));
        assert_eq!(d.classify(b"to"), None, "second word commit dropped, not doubled");
    }

    #[test]
    fn ime_dedup_erases_preedit_on_ime_conversion() {
        // IME conversion (captured "80"→"€", and dead-key "ù"): the commit
        // REPLACES the live single-char composition, so the already-injected
        // preedit is erased (one DEL per char) and the commit sent.
        let mut d = ImeDedup::new();
        assert_eq!(d.classify(b"8"), Some(b"8".to_vec()));
        assert_eq!(d.classify(b"0"), Some(b"0".to_vec()));
        let euro = "€".as_bytes();
        let mut expected = vec![0x7f, 0x7f]; // erase the "80" preedit
        expected.extend_from_slice(euro);
        assert_eq!(d.classify(euro), Some(expected), "erase preedit then send €");
    }

    #[test]
    fn ime_dedup_paste_after_word_not_erased() {
        // Safety: a multi-byte frame that follows a COMMITTED word (not a live
        // single-char composition) must never be treated as a conversion +
        // erase the previous word. `composing` is false here.
        let mut d = ImeDedup::new();
        assert_eq!(d.classify(b"hello"), Some(b"hello".to_vec())); // commit → composing=false
        assert_eq!(d.classify(b"pasted"), Some(b"pasted".to_vec()), "sent as-is, no erase");
    }

    #[test]
    fn ime_dedup_collapses_repeated_emoji_commit() {
        // Emoji picker double/triple-fires the same multi-byte commit ("😁😁😁"
        // for one pick). Within the window the repeats drop to a single emoji.
        let mut d = ImeDedup::new();
        let e = "😁".as_bytes();
        assert_eq!(d.classify(e), Some(e.to_vec()), "first emoji sent");
        assert_eq!(d.classify(e), None, "2nd identical commit dropped");
        assert_eq!(d.classify(e), None, "3rd identical commit dropped");
    }
}
