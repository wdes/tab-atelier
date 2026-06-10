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

/// How often the server-side task wakes to check for new PTY bytes
/// and state changes. v1 polls the snapshot — a follow-up will
/// switch to `tokio::sync::Notify` on the ring push site so we
/// emit bytes within microseconds instead of within this tick.
///
/// 30 ms is faster than the old 80 ms HTTP poll AND saves the round
/// trip + header overhead, so end-to-end latency drops even before
/// the Notify migration.
const TICK_MS: u64 = 30;

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
/// Returns the tab UUID if so.
#[must_use]
pub fn parse_ws_path(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/tabs/by-id/")?;
    let (uuid, suffix) = rest.split_once('/')?;
    // Strip query string from the suffix before comparing.
    let action = suffix.split('?').next().unwrap_or(suffix);
    if action == "ws" { Some(uuid) } else { None }
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
    last_bytes: Vec<u8>,
    last_at: Option<Instant>,
}

impl ImeDedup {
    const fn new() -> Self {
        Self {
            last_bytes: Vec::new(),
            last_at: None,
        }
    }

    /// Returns the bytes to actually inject into `pending_input`,
    /// or `None` to drop the frame entirely.
    fn classify(&mut self, bytes: &[u8]) -> Option<Vec<u8>> {
        let now = Instant::now();
        let fresh = self.last_at.is_none_or(|t| now.duration_since(t) >= IME_DEDUP_WINDOW);
        if bytes.len() <= 1 || fresh {
            self.last_bytes.clear();
            self.last_bytes.extend_from_slice(bytes);
            self.last_at = Some(now);
            return Some(bytes.to_vec());
        }
        if !self.last_bytes.is_empty() && self.last_bytes.as_slice() == bytes {
            self.last_at = Some(now);
            return None;
        }
        if !self.last_bytes.is_empty() && bytes.starts_with(&self.last_bytes) {
            let suffix = bytes[self.last_bytes.len()..].to_vec();
            self.last_bytes.clear();
            self.last_bytes.extend_from_slice(bytes);
            self.last_at = Some(now);
            return Some(suffix);
        }
        self.last_bytes.clear();
        self.last_bytes.extend_from_slice(bytes);
        self.last_at = Some(now);
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
    uuid: &str,
    provided: &[u8],
) -> Option<(Authz, Arc<Mutex<PtyRing>>)> {
    let (rw_tok, ro_tok, ring) = {
        let snap = state.lock().ok()?;
        let t = snap.tabs.iter().find(|t| t.id == uuid)?;
        let ring = t.pty_ring.clone()?;
        (t.share_token_rw.clone(), t.share_token_ro.clone(), ring)
    };
    let master_match = crate::api::constant_time_eq(provided, master.as_bytes());
    let rw_match = !rw_tok.is_empty() && crate::api::constant_time_eq(rw_tok.as_bytes(), provided);
    let ro_match = !ro_tok.is_empty() && crate::api::constant_time_eq(ro_tok.as_bytes(), provided);
    if master_match || rw_match {
        Some((Authz::Rw, ring))
    } else if ro_match {
        Some((Authz::Ro, ring))
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
/// clients, server-to-server, file://). "null" Origin → accept
/// (sandbox iframes, file://).
fn origin_ok<B>(req: &Request<B>) -> bool {
    let Some(origin) = req.headers().get("origin") else {
        return true; // no Origin = not a browser request
    };
    let Ok(origin_str) = origin.to_str() else {
        return false;
    };
    if origin_str == "null" {
        return true;
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

fn snapshot_meta(t: &SnapshotTab, authz: Authz) -> MetaSnapshot {
    let lock_reason = t.lock_reason();
    let dir_count = |dirname: &str| -> usize {
        t.cwd.as_deref().map_or(0, |cwd| {
            std::fs::read_dir(std::path::Path::new(cwd).join(dirname)).map_or(0, |rd| {
                rd.flatten().filter(|e| e.metadata().is_ok_and(|m| m.is_file())).count()
            })
        })
    };
    // RO viewers don't see `inbox_count`. The HTTP `/inbox` listing
    // endpoint is RW-only specifically so RO recipients can't
    // enumerate uploads (`src/api.rs` needs_rw includes "inbox"); a
    // count here would be a milder version of the same info leak.
    // `outbox_count` is fine for RO — downloads are allowed.
    let inbox_count = if matches!(authz, Authz::Ro) {
        0
    } else {
        dir_count("inbox")
    };
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
        outbox_count: dir_count("outbox"),
        inbox_count,
        build_hash: crate::api::BUILD_HASH,
    }
}

/// Entry point — called from `handle_hyper_request` in api.rs after
/// it has matched `/tabs/by-id/{uuid}/ws` via `parse_ws_path`. If
/// the upgrade is accepted, returns the 101 response immediately
/// and spawns the WS pump in the background; if auth fails, returns
/// a 401 / 404 without upgrading.
pub fn handle_upgrade(
    mut req: Request<hyper::body::Incoming>,
    state: Arc<Mutex<TabSnapshot>>,
    master_token: &str,
    read_only_process: bool,
    uuid: String,
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
    let Some((authz, ring)) = authorise_and_ring(&state, master_token, &uuid, &provided) else {
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
    let mut last_meta: Option<MetaSnapshot> = if let Some(meta) = current_meta(&state, &uuid, authz) {
        let bytes = encode_frame(TAG_META, serde_json::to_vec(&meta).unwrap_or_default());
        if sink.send(Message::Binary(bytes.into())).await.is_err() {
            return;
        }
        Some(meta)
    } else {
        None
    };

    let mut tick = tokio::time::interval(Duration::from_millis(TICK_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Periodic push: new PTY bytes + state changes.
            _ = tick.tick() => {
                // out frames — drop the ring lock immediately after
                // copying the new suffix into a Vec so the PTY-read
                // side (in pty_ring.rs) isn't blocked while we're
                // talking to the WS sink.
                let chunk = {
                    let Ok(r) = ring.lock() else { return; };
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
                    let frame = encode_frame(TAG_OUT, chunk);
                    if sink.send(Message::Binary(frame.into())).await.is_err() {
                        return;
                    }
                }
                // meta frame — only when something actually changed.
                // Structural compare against the last sent snapshot; we
                // only serialise (allocating a Vec) when it differs.
                if let Some(meta) = current_meta(&state, &uuid, authz)
                    && last_meta.as_ref() != Some(&meta)
                {
                    let bytes = encode_frame(TAG_META, serde_json::to_vec(&meta).unwrap_or_default());
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        return;
                    }
                    last_meta = Some(meta);
                }
            }
            // Inbound frames.
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
        }
    }
}

fn encode_frame(tag: u8, mut payload: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(tag);
    out.append(&mut payload);
    out
}

/// Dispatch a single C→S frame into the snapshot's pending queues.
/// Returns `Err(CloseFrame)` when the client violated the protocol
/// (RO trying to write, unknown tag, malformed JSON) and the
/// connection should close.
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
            if let Some(effective) = ime_dedup.classify(payload)
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

fn current_meta(state: &Arc<Mutex<TabSnapshot>>, uuid: &str, authz: Authz) -> Option<MetaSnapshot> {
    let snap = state.lock().ok()?;
    let meta = snap.tabs.iter().find(|t| t.id == uuid).map(|t| snapshot_meta(t, authz));
    drop(snap);
    meta
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
    fn parse_ws_path_accepts_canonical_form() {
        assert_eq!(parse_ws_path("/tabs/by-id/abc-123/ws"), Some("abc-123"));
        assert_eq!(parse_ws_path("/tabs/by-id/abc-123/ws?token=x"), Some("abc-123"));
    }

    #[test]
    fn parse_ws_path_rejects_other_endpoints() {
        assert_eq!(parse_ws_path("/tabs/by-id/abc/output"), None);
        assert_eq!(parse_ws_path("/tabs/by-id/abc"), None);
        assert_eq!(parse_ws_path("/tabs/0/ws"), None);
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
    fn origin_ok_null_origin_accepted() {
        // file:// pages and sandboxed iframes get Origin: null.
        assert!(origin_ok(&req_with_headers(&[
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
    fn ime_dedup_drops_exact_multibyte_repeat() {
        // compositionupdate("hello") + compositionend("hello") =
        // two identical multi-byte frames within the window.
        let mut d = ImeDedup::new();
        assert_eq!(d.classify(b"hello"), Some(b"hello".to_vec()));
        assert_eq!(d.classify(b"hello"), None);
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
}
