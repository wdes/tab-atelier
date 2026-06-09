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
use std::time::Duration;

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
    provided: &str,
) -> Option<(Authz, Arc<Mutex<PtyRing>>)> {
    let (rw_tok, ro_tok, ring) = {
        let snap = state.lock().ok()?;
        let t = snap.tabs.iter().find(|t| t.id == uuid)?;
        let ring = t.pty_ring.clone()?;
        (t.share_token_rw.clone(), t.share_token_ro.clone(), ring)
    };
    let master_match = crate::api::constant_time_eq(provided.as_bytes(), master.as_bytes());
    let rw_match = !rw_tok.is_empty() && crate::api::constant_time_eq(rw_tok.as_bytes(), provided.as_bytes());
    let ro_match = !ro_tok.is_empty() && crate::api::constant_time_eq(ro_tok.as_bytes(), provided.as_bytes());
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
fn extract_token<B>(req: &Request<B>) -> Option<String> {
    let q = req.uri().query()?;
    for pair in q.split('&') {
        if let Some(v) = pair.strip_prefix("token=") {
            return Some(percent_decode(v));
        }
    }
    let h = req.headers().get(hyper::header::AUTHORIZATION)?;
    let s = h.to_str().ok()?;
    s.strip_prefix("Bearer ").map(str::to_string)
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

/// Minimal percent-decode for the `?token=` query value. Tokens
/// today are hex (no encoding) but bearer-token rotators may
/// emit `+` / `%2B` etc.
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h * 16 + l) as u8) as char);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(' ');
        } else {
            out.push(bytes[i] as char);
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
#[derive(serde::Serialize)]
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

    // Send the initial meta on connect and seed the hash so the
    // first tick after this doesn't re-emit the same payload.
    let mut last_meta_hash: u64 = if let Some(meta) = current_meta(&state, &uuid, authz) {
        let bytes = encode_frame(TAG_META, serde_json::to_vec(&meta).unwrap_or_default());
        if sink.send(Message::Binary(bytes.into())).await.is_err() {
            return;
        }
        meta_hash(&meta)
    } else {
        0
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
                if let Some(meta) = current_meta(&state, &uuid, authz) {
                    let h = meta_hash(&meta);
                    if h != last_meta_hash {
                        let bytes = encode_frame(TAG_META, serde_json::to_vec(&meta).unwrap_or_default());
                        if sink.send(Message::Binary(bytes.into())).await.is_err() {
                            return;
                        }
                        last_meta_hash = h;
                    }
                }
            }
            // Inbound frames.
            msg = stream.next() => {
                let Some(Ok(msg)) = msg else { return; };
                match msg {
                    Message::Binary(b) => {
                        if let Err(close) = handle_inbound(&b, authz, read_only_process, &state, &uuid) {
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
            snap.pending_input.push((idx, payload.to_vec()));
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

/// Cheap fingerprint so we only emit a `meta` frame when something
/// actually changed. Serialise + hash; not perfect (re-orders aren't
/// detected, but `serde_json` is order-stable on a struct) and the
/// cost is bounded by the meta payload size.
fn meta_hash(m: &MetaSnapshot) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let bytes = serde_json::to_vec(m).unwrap_or_default();
    h.write(&bytes);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("a%2Bb"), "a+b");
        assert_eq!(percent_decode("plain"), "plain");
        // Malformed escape is passed through.
        assert_eq!(percent_decode("a%ZZ"), "a%ZZ");
    }

    #[test]
    fn encode_frame_prepends_tag_byte() {
        let f = encode_frame(0x02, b"hello".to_vec());
        assert_eq!(f, b"\x02hello");
    }
}
