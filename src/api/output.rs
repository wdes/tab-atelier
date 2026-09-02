// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `/tabs/<id>/output` polling endpoint: CRC-delta / line-tail / full
//! scrollback, with the live tab state (lock, schedule, agent) in headers.

use std::fmt::Write as _;
use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{
    TabSnapshot, error_json, is_safe_hex_color, parse_tab_key, resolve_tab_idx, respond_with_etag_precomputed,
    write_schedule_headers,
};

pub(super) fn run<W: Write>(
    stream: &mut W,
    state: &Arc<Mutex<TabSnapshot>>,
    p: &str,
    query_since: Option<usize>,
    query_crc: Option<u32>,
    query_lines: Option<usize>,
    accept_gzip: bool,
) {
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/output") else {
        error_json(stream, 404, "invalid tab key");
        return;
    };
    let state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&state, key_raw, is_uuid) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let Some(t) = state.tabs.get(idx) else {
        drop(state);
        error_json(stream, 404, "tab index out of range");
        return;
    };

    // Three response modes, picked in this order:
    //   1. ?since=N&crc=HHHHHHHH  — append-only patching. Server
    //      checks CRC32 of its own first N bytes; on match we
    //      ship only [N..]. Mismatch (cleared screen, alt-screen
    //      swap, scrollback ring-shifted) falls through to a
    //      full body.
    //   2. ?lines=N  — tail by line count (the existing behaviour).
    //   3. neither   — full scrollback.
    //
    // Mode 1 is what turns a noisy LAN poll into a few-byte delta
    // for the steady-state append case (>99% of the time, a tab
    // is just appending output).
    // Use raw_output (row-by-row, no WRAPLINE join) so xterm.js
    // can reproduce the server's layout exactly when it's
    // resized to the same cols/rows. The mobile remote keeps
    // talking to /tabs (which returns the joined `output`).
    // Clone the Arc handle (refcount bump) + the small fields,
    // then drop the global snapshot lock BEFORE the CRC passes
    // and suffix search below — they walk up to hundreds of KB
    // per poll and used to run entirely under the mutex every
    // other API user (and every WS keystroke) needs.
    let (payload, total_crc): (std::sync::Arc<str>, u32) = if t.raw_output.is_empty() {
        (t.output.clone(), t.output_crc)
    } else {
        (t.raw_output.clone(), t.raw_output_crc)
    };
    let full_cursor = t.cursor;
    let pty_cols = t.cols;
    let pty_rows = t.rows;
    let raw_cursor = t.raw_cursor;
    let bg_color = t.bg_color.clone();
    let schedule = t.schedule.clone();
    let lock_reason = crate::schedule::LockState::lock_reason(t);
    let locked = crate::schedule::LockState::effective_locked(t);
    // Agent indicator surfaced to the share-link viewer so the
    // browser tab title can mirror what the desktop GUI shows
    // (\u{1f9e0} Thinking / ⌛ Waiting / ❗ Error). Strictly
    // additive: omitted when no agent is attached.
    let (agent_state_str, agent_label) = t.agent_state.as_ref().map_or((None, None), |s| {
        let key = match s.state {
            crate::AgentState::Thinking => "thinking",
            crate::AgentState::Waiting => "waiting",
            crate::AgentState::Error => "error",
        };
        (Some(key), s.label.clone())
    });
    drop(state);

    let total_len = payload.len();

    // Every response mode ships a suffix of `payload`, so track
    // just the start offset — the body is sliced out of the
    // shared Arc at respond time, no per-request copy.
    let (cursor, start_offset) = match (query_since, query_crc) {
        (Some(n), Some(client_crc)) if n <= total_len => {
            // Steady state (>99% of polls): the client is fully
            // caught up, so its prefix IS the whole payload and
            // the cached total CRC answers without a hash pass.
            let prefix_crc = if n == total_len {
                total_crc
            } else {
                crate::crc32(&payload.as_bytes()[..n])
            };
            if prefix_crc == client_crc {
                // The client's history is still a real prefix of
                // ours. Ship the suffix only — cursor row is
                // relative to the full buffer, the client knows
                // how to add its own line count.
                (full_cursor, n)
            } else {
                (full_cursor, 0)
            }
        }
        _ => match query_lines {
            Some(n) if n > 0 => {
                let total_lines = payload.lines().count();
                let drop_count = total_lines.saturating_sub(n);
                if drop_count == 0 {
                    (full_cursor, 0)
                } else {
                    let mut offset = 0;
                    for _ in 0..drop_count {
                        if let Some(nl) = payload[offset..].find('\n') {
                            offset += nl + 1;
                        } else {
                            offset = payload.len();
                            break;
                        }
                    }
                    let cur = full_cursor.and_then(|(r, c)| {
                        if r >= drop_count {
                            Some((r - drop_count, c))
                        } else {
                            None
                        }
                    });
                    (cur, offset)
                }
            }
            _ => (full_cursor, 0),
        },
    };

    let mut extra = String::new();
    if let Some((row, col)) = cursor {
        let _ = write!(extra, "X-Cursor-Row: {row}\r\nX-Cursor-Col: {col}\r\n");
    }
    let _ = write!(
        extra,
        "X-Output-Length: {total_len}\r\nX-Output-Crc: {total_crc:08x}\r\nX-Output-Start: {start_offset}\r\nX-Output-Cols: {pty_cols}\r\nX-Output-Rows: {pty_rows}\r\n"
    );
    // Cursor position in raw-output coords — the viewer
    // reapplies it after each write so xterm.js puts its
    // blink at the server's real cursor (otherwise the
    // cursor sits at the end of the last written byte =
    // bottom-right corner of the dump, never where the user
    // is actually typing).
    if let Some((row, col)) = raw_cursor {
        let _ = write!(extra, "X-Raw-Cursor-Row: {row}\r\nX-Raw-Cursor-Col: {col}\r\n");
    }
    // Effective background color (per-tab override OR global
    // default, resolved server-side). The JS reads this on
    // every poll and updates theme.background mid-session.
    // Re-validate before echoing into a header line — input
    // validation should already have rejected anything weird,
    // but the round-trip through TabSnapshot is enough of a
    // surface that we don't want a hypothetical bypass to
    // turn into a header-injection vector.
    if is_safe_hex_color(&bg_color) {
        let _ = write!(extra, "X-Tab-Bg: {bg_color}\r\n");
    }
    if locked {
        let _ = write!(extra, "X-Tab-Locked: 1\r\n");
        if let Some(r) = lock_reason {
            let _ = write!(extra, "X-Tab-Locked-Reason: {r}\r\n");
        }
    }
    if let Some(s) = schedule.as_ref() {
        write_schedule_headers(&mut extra, s);
    }
    if let Some(state_str) = agent_state_str {
        let _ = write!(extra, "X-Agent-State: {state_str}\r\n");
        // Label can be any UTF-8 reported via `set-status
        // --label`. Percent-encode every non-ASCII byte +
        // CRLF / `%` so the wire stays strict-ASCII and the
        // viewer can `decodeURIComponent` it back. Cap at
        // 256 chars before encoding.
        if let Some(label) = agent_label {
            let truncated: String = label.chars().take(256).collect();
            let mut encoded = String::with_capacity(truncated.len());
            for byte in truncated.bytes() {
                if matches!(byte, 0x20..=0x7e) && byte != b'%' && byte != b'\r' && byte != b'\n' {
                    encoded.push(byte as char);
                } else {
                    let _ = write!(encoded, "%{byte:02X}");
                }
            }
            if !encoded.is_empty() {
                let _ = write!(extra, "X-Agent-Label: {encoded}\r\n");
            }
        }
    }
    // Pass `None` for if_none_match — /output is a live
    // polling endpoint whose live state lives in headers
    // (X-Tab-Locked, X-Agent-State, X-Outbox-Count, …).
    // Returning 304 on an idle poll (when the body's CRC
    // hasn't changed) ships those headers via the 304's
    // header block, but browsers vary on whether fetch()
    // exposes 304 headers — Chrome / Safari sometimes serve
    // the cached 200's header set instead, which means a
    // mid-session unlock / agent-state flip wouldn't reach
    // the JS until a full page reload. Force 200 so every
    // poll carries fresh headers in a fresh response.
    respond_with_etag_precomputed(
        stream,
        200,
        "text/plain; charset=utf-8",
        payload[start_offset..].as_bytes(),
        accept_gzip,
        None,
        &extra,
        // Full-body response ⇒ the cached total CRC IS the etag;
        // a delta ships a small suffix, hashed cheaply as usual.
        (start_offset == 0).then(|| format!("{total_crc:08x}")),
    );
}
