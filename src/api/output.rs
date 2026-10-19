// SPDX-License-Identifier: MPL-2.0

//! The `/tabs/<id>/output` polling endpoint: CRC-delta / line-tail / full
//! scrollback, with the live tab state (lock, schedule, agent) in headers.
//!
//! The one route whose *body is not the whole answer*. A viewer polling every
//! few hundred milliseconds needs the cursor, the lock state, the schedule and
//! the agent indicator on the same request that carries the bytes, and a fetch
//! cannot read a second response — so the state rides in `X-…` headers. That is
//! why this builds a [`Reply`] by hand instead of returning a `Doc<T>`: the
//! resource is the pair (suffix bytes, headers), and the headers are part of the
//! contract the viewer's JS already reads.

use std::sync::{Arc, Mutex};

use tab_atelier_api::{Conditional, Failure, Reply, Request, cached};

use super::{TabSnapshot, is_safe_hex_color, parse_tab_key, resolve_tab_idx};

/// The headers a poll answers with, built together so the safety rules live in
/// one place.
#[derive(Default)]
pub(super) struct Headers(pub(super) Vec<(&'static str, String)>);

impl Headers {
    fn put(&mut self, name: &'static str, value: impl Into<String>) {
        self.0.push((name, value.into()));
    }

    /// A value that came from outside this process: percent-encode everything
    /// that is not plainly printable, so a CRLF in a tab name or an agent label
    /// cannot end the header and start a new one.
    fn put_encoded(&mut self, name: &'static str, value: &str, cap: usize) {
        let mut encoded = String::with_capacity(value.len());
        for byte in value.chars().take(cap).collect::<String>().into_bytes() {
            if matches!(byte, 0x20..=0x7e) && !matches!(byte, b'%' | b'\r' | b'\n') {
                encoded.push(char::from(byte));
            } else {
                use std::fmt::Write as _;
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
        if !encoded.is_empty() {
            self.put(name, encoded);
        }
    }
}

/// The off-hours schedule, so the viewer can say when the tab reopens.
///
/// Every value here is checked for CRLF before it is emitted: `tz` is restricted
/// to the chrono-tz table's alphabet, the next change is RFC 3339, and the rule
/// is percent-encoded because OSM grammar accepts non-ASCII in some comment
/// forms.
fn schedule_of(headers: &mut Headers, schedule: &crate::schedule::TabSchedule) {
    let tz_safe = schedule
        .tz
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'));
    if tz_safe {
        headers.put("X-Tab-Schedule-Tz", schedule.tz.clone());
    }
    if let Some(next_utc) = schedule.next_change_from_now() {
        headers.put("X-Tab-Schedule-Next", next_utc.to_rfc3339());
    }
    headers.put_encoded("X-Tab-Schedule-Rule", &schedule.rule, usize::MAX);
}

/// The schedule headers on their own, for the injection test in `api::tests`.
///
/// Kept public to the parent module rather than duplicated in the test: the
/// point of the test is that *this* function cannot emit a header line that
/// ends early, and a copy would prove nothing.
pub(super) fn schedule_headers(schedule: &crate::schedule::TabSchedule) -> Vec<(&'static str, String)> {
    let mut headers = Headers::default();
    schedule_of(&mut headers, schedule);
    headers.0
}

pub(super) fn run(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Reply, Failure> {
    let Some((key_raw, is_uuid)) = parse_tab_key(request.path, "/output") else {
        return Err(Failure::problem(404, "invalid tab key"));
    };
    let state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&state, key_raw, is_uuid) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let Some(t) = state.tabs.get(idx) else {
        return Err(Failure::problem(404, "tab index out of range"));
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
    //
    // Use raw_output (row-by-row, no WRAPLINE join) so xterm.js
    // can reproduce the server's layout exactly when it's
    // resized to the same cols/rows. The mobile remote keeps
    // talking to /tabs (which returns the joined `output`).
    // Clone the Arc handle (refcount bump) + the small fields,
    // then drop the global snapshot lock BEFORE the CRC passes
    // and suffix search below — they walk up to hundreds of KB
    // per poll and used to run entirely under the mutex every
    // other API user (and every WS keystroke) needs.
    let (payload, total_crc): (Arc<str>, u32) = if t.raw_output.is_empty() {
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
    // (🧠 Thinking / ⌛ Waiting / ❗ Error). Strictly additive:
    // omitted when no agent is attached.
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
    let (cursor, start_offset) = match (
        request.query_value("since").and_then(parse_usize),
        request.query_value("crc").and_then(parse_crc),
    ) {
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
        _ => match request.query_value("lines").and_then(parse_usize) {
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

    let mut headers = Headers::default();
    if let Some((row, col)) = cursor {
        headers.put("X-Cursor-Row", row.to_string());
        headers.put("X-Cursor-Col", col.to_string());
    }
    headers.put("X-Output-Length", total_len.to_string());
    headers.put("X-Output-Crc", format!("{total_crc:08x}"));
    headers.put("X-Output-Start", start_offset.to_string());
    headers.put("X-Output-Cols", pty_cols.to_string());
    headers.put("X-Output-Rows", pty_rows.to_string());
    // Cursor position in raw-output coords — the viewer
    // reapplies it after each write so xterm.js puts its
    // blink at the server's real cursor (otherwise the
    // cursor sits at the end of the last written byte =
    // bottom-right corner of the dump, never where the user
    // is actually typing).
    if let Some((row, col)) = raw_cursor {
        headers.put("X-Raw-Cursor-Row", row.to_string());
        headers.put("X-Raw-Cursor-Col", col.to_string());
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
        headers.put("X-Tab-Bg", bg_color.to_string());
    }
    if locked {
        headers.put("X-Tab-Locked", "1");
        if let Some(r) = lock_reason {
            headers.put("X-Tab-Locked-Reason", r);
        }
    }
    if let Some(s) = schedule.as_ref() {
        // Through the helper rather than inline, so the injection test exercises
        // the same code the response runs.
        headers.0.extend(schedule_headers(s));
    }
    if let Some(state_str) = agent_state_str {
        headers.put("X-Agent-State", state_str);
        // Label can be any UTF-8 reported via `set-status --label`. Cap at 256
        // characters before encoding, and percent-encode as `put_encoded` does.
        if let Some(label) = agent_label.as_deref() {
            headers.put_encoded("X-Agent-Label", label, 256);
        }
    }

    // Never 304. This is a live polling endpoint whose state lives in the
    // headers (X-Tab-Locked, X-Agent-State, …): a 304 would have to carry them,
    // and browsers disagree about whether `fetch` exposes a 304's headers —
    // Chrome and Safari sometimes serve the cached 200's set instead, so a
    // mid-session unlock or agent-state flip would not reach the JS until a
    // reload. `if_none_match: None` is how the shared helper is told exactly
    // that; every poll is a fresh 200 with fresh headers.
    let body = bytes::Bytes::from(payload[start_offset..].to_owned());
    // Full-body response ⇒ the cached total CRC IS the tag (and costs no hash
    // pass); a delta ships a small suffix, hashed cheaply as usual.
    let tag = if start_offset == 0 {
        format!("{total_crc:08x}")
    } else {
        format!("{:08x}", crate::crc32(&body))
    };
    let mut reply = cached(
        200,
        "text/plain; charset=utf-8",
        body,
        &tag,
        Conditional {
            accept_gzip: request.conditional.accept_gzip,
            if_none_match: None,
        },
    );
    for (name, value) in headers.0 {
        reply = reply.with_header(name, value);
    }
    Ok(reply)
}

fn parse_usize(raw: &str) -> Option<usize> {
    raw.parse().ok()
}

fn parse_crc(raw: &str) -> Option<u32> {
    u32::from_str_radix(raw, 16).ok()
}
