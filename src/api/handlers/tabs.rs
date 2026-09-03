// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `tabs` resource — the bulk of the API surface: listing/usage, the share
//! viewer (`/view`) + live output poll (`/output`), input, file up/download,
//! inbox/outbox listing, and the per-tab mutation routes (resize/rename/lock/
//! net/net-allow/ssh-agent/schedule/bg-color/limits/env, create/delete).
//!
//! Bodies moved verbatim from `handle_connection`'s match arms
//! (behavior-preserving); each handler reaches the parent module's private
//! response writers + shared types via `super::super::…` (descendant access).

use std::fmt::Write as _;
use std::io::Write;
use std::sync::{Arc, Mutex};

use log::info;
use serde::Serialize;

use crate::tracking::USER_AGENT;

use super::super::{
    ApiResponse, BUILD_HASH, DOWNLOAD_GZIP_MAX, DnsEntryInfo, HostInfo, TabInfo, TabSnapshot, UPLOAD_MAX_BYTES,
    UPLOAD_MAX_BYTES_MIB, UPLOAD_MAX_INFLIGHT_PER_TOKEN, UploadSlot, VIEWER_HTML, collect_files_tree, error_json,
    is_safe_hex_color, parse_env_body, parse_tab_key, resolve_sandbox_path, resolve_tab_idx, respond_json,
    respond_with_etag, respond_with_etag_precomputed, sanitize_basename, strip_ansi, write_new_file_no_symlink,
    write_schedule_headers,
};

/// `GET /` | `/tabs` — the full tab list (cached snapshot projection).
pub(in crate::api) fn list<S: Write>(
    stream: &mut S,
    state: &Arc<Mutex<TabSnapshot>>,
    accept_gzip: bool,
    if_none_match: Option<&str>,
) {
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(body) = state.cached_response.clone() {
        drop(state);
        respond_with_etag(
            stream,
            200,
            "application/json",
            body.as_bytes(),
            accept_gzip,
            if_none_match,
            "",
        );
        return;
    }
    let tabs: Vec<TabInfo> = state
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| TabInfo {
            index: i,
            id: t.id.to_string(),
            name: t.name.to_string(),
            cwd: t.cwd.as_deref().map(str::to_string),
            active: i == state.active,
            preview: strip_ansi(t.output.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("")),
            // Dirtiness key for the out-of-process poller (brain): unchanged crc
            // ⇒ the tab's screen is byte-identical ⇒ brain skips its /output scan.
            output_crc: t.output_crc,
            uptime_secs: t.uptime_secs,
            #[cfg(feature = "energy")]
            cpu_percent: state.power.get(i).map_or(0.0, |p| p.cpu_percent),
            #[cfg(feature = "energy")]
            watts: state.power.get(i).and_then(|p| p.watts),
            agent_state: t.agent_state.as_ref().map(|s| match s.state {
                crate::AgentState::Thinking => "thinking",
                crate::AgentState::Waiting => "waiting",
                crate::AgentState::Error => "error",
            }),
            agent_kind: t.agent_kind.as_deref().map(str::to_string),
            led: t.agent_led.map(crate::TabLed::slug),
            last_used_at: t.last_used_at,
            agent_session_id: t.agent_session_id.as_deref().map(str::to_string),
            viewers: t.viewers,
            locked: crate::schedule::LockState::effective_locked(t),
            lock_reason: crate::schedule::LockState::lock_reason(t),
            schedule_rule: t.schedule.as_ref().map(|s| s.rule.clone()),
            schedule_tz: t.schedule.as_ref().map(|s| s.tz.clone()),
            context: t.context.as_deref().map(str::to_string),
            assignment: t.assignment.as_deref().map(str::to_string),
            parent_tab_id: t.parent_tab_id.as_deref().map(str::to_string),
            rehome_status: t.rehome_status.as_deref().map(str::to_string),
            net_disabled: t.net_disabled,
            connections: t.connections,
            tx_bytes: t.tx_bytes,
            tx_denied_bytes: t.tx_denied_bytes,
            net_allow_presets: t.net_allow.presets.iter().map(|p| p.id().to_string()).collect(),
            net_allow_domains: t.net_allow.domains.clone(),
            net_allow_cidrs: t.net_allow.cidrs.clone(),
            dns: t
                .dns_entries
                .iter()
                .map(|(domain, allowed, ips)| DnsEntryInfo {
                    domain: domain.clone(),
                    allowed: *allowed,
                    ips: ips.clone(),
                })
                .collect(),
            resident_memory_bytes: t.resident_memory_bytes,
            tokens: t.tokens,
            specialty: t.specialty.as_deref().map(str::to_string),
            orchestrator: t.orchestrator.as_deref().map(str::to_string),
            objective: t.objective.as_deref().map(str::to_string),
            current_task_log: t.current_task.clone(),
            conventions: t.conventions.clone(),
            evaluations: t.evaluations.clone(),
            rounds_active: t.rounds_active.clone(),
            usage_count: t.usage_count,
            // Inc9 cross-guard (#23): a hot-swap handoff is in progress → tell
            // external daemons (brain/clarify) to leave every tab alone. Carried
            // into the peeled handler from this fork's fabric+#23 context (the
            // upstream refactor never saw this field).
            in_handoff: crate::hotswap::frozen(),
            // Inc9 b2/b3 — context-% + recent-compaction (derived at read time).
            context_pct: t.context_pct,
            recently_compacted: crate::cli::clarify::recently_compacted(t.last_compaction_at, crate::unix_millis()),
        })
        .collect();
    #[cfg(feature = "energy")]
    let host = HostInfo {
        battery_percent: state.battery_percent,
        watts: {
            let total: f64 = state.power.iter().filter_map(|p| p.watts).sum();
            if total > 0.0 { Some(total) } else { None }
        },
    };
    #[cfg(not(feature = "energy"))]
    let host = HostInfo::default();
    let resp = ApiResponse {
        app: USER_AGENT,
        host,
        tabs,
    };
    let body: std::sync::Arc<str> = serde_json::to_string_pretty(&resp).unwrap_or_default().into();
    state.cached_response = Some(body.clone());
    drop(state);
    respond_with_etag(
        stream,
        200,
        "application/json",
        body.as_bytes(),
        accept_gzip,
        if_none_match,
        "",
    );
}

/// `GET /tabs/usage` — the lean per-tab consumption projection (no scrollback).
pub(in crate::api) fn usage<S: Write>(
    stream: &mut S,
    state: &Arc<Mutex<TabSnapshot>>,
    accept_gzip: bool,
    if_none_match: Option<&str>,
) {
    #[derive(Serialize)]
    struct UsageTab {
        id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        resident_memory_bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<crate::TokenUsage>,
        cpu_percent: f64,
        connections: usize,
        tx_bytes: u64,
    }
    let state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    #[cfg(feature = "energy")]
    let cpu_of = |i: usize| state.power.get(i).map_or(0.0, |p| p.cpu_percent);
    #[cfg(not(feature = "energy"))]
    let cpu_of = |_i: usize| 0.0_f64;
    let usage: Vec<UsageTab> = state
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| UsageTab {
            id: t.id.to_string(),
            name: t.name.to_string(),
            resident_memory_bytes: t.resident_memory_bytes,
            tokens: t.tokens,
            cpu_percent: cpu_of(i),
            connections: t.connections,
            tx_bytes: t.tx_bytes,
        })
        .collect();
    let body = serde_json::to_string_pretty(&usage).unwrap_or_default();
    drop(state);
    respond_with_etag(
        stream,
        200,
        "application/json",
        body.as_bytes(),
        accept_gzip,
        if_none_match,
        "",
    );
}

/// `GET /tabs/…/view` — the share-link viewer HTML page.
pub(in crate::api) fn view<S: Write>(
    stream: &mut S,
    state: &Arc<Mutex<TabSnapshot>>,
    p: &str,
    accept_gzip: bool,
    if_none_match: Option<&str>,
) {
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/view") else {
        error_json(stream, 404, "invalid tab key");
        return;
    };
    let state_g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&state_g, key_raw, is_uuid) else {
        drop(state_g);
        error_json(stream, 404, "tab not found");
        return;
    };
    let t = &state_g.tabs[idx];
    let tab_name = t.name.clone();
    let tab_bg = if t.bg_color.is_empty() {
        crate::DEFAULT_TAB_BG_COLOR.to_string()
    } else {
        t.bg_color.to_string()
    };
    drop(state_g);
    let key_for_html = if is_uuid {
        format!("by-id/{key_raw}")
    } else {
        key_raw.to_string()
    };
    let asset_depth = 1 + key_for_html.split('/').filter(|s| !s.is_empty()).count();
    let asset_prefix = "../".repeat(asset_depth);
    let html_name = tab_name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    let js_name = serde_json::to_string(&tab_name)
        .unwrap_or_else(|_| "\"\"".into())
        .trim_matches('"')
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    let safe_bg: &str = if is_safe_hex_color(&tab_bg) {
        &tab_bg
    } else {
        crate::DEFAULT_TAB_BG_COLOR
    };
    let html = VIEWER_HTML
        .replace("__ASSET_PREFIX__", &asset_prefix)
        .replace("__TAB_KEY__", &key_for_html)
        .replace("__TAB_NAME_HTML__", &html_name)
        .replace("__TAB_NAME_JS__", &js_name)
        .replace("__TAB_BG__", safe_bg)
        .replace("__BUILD_HASH__", BUILD_HASH);
    respond_with_etag(
        stream,
        200,
        "text/html; charset=utf-8",
        html.as_bytes(),
        accept_gzip,
        if_none_match,
        "Cache-Control: no-store, no-cache, must-revalidate\r\n\
         Pragma: no-cache\r\n\
         X-Frame-Options: DENY\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'self' 'unsafe-inline'; \
         style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; \
         connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n\
         Referrer-Policy: no-referrer\r\n",
    );
}

/// `GET /tabs/…/output[?since=&crc=|?lines=]` — the live scrollback poll.
#[allow(clippy::too_many_arguments)]
pub(in crate::api) fn output<S: Write>(
    stream: &mut S,
    state: &Arc<Mutex<TabSnapshot>>,
    p: &str,
    accept_gzip: bool,
    query_since: Option<usize>,
    query_crc: Option<u32>,
    query_lines: Option<usize>,
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

    let (cursor, start_offset) = match (query_since, query_crc) {
        (Some(n), Some(client_crc)) if n <= total_len => {
            let prefix_crc = if n == total_len {
                total_crc
            } else {
                crate::crc32(&payload.as_bytes()[..n])
            };
            if prefix_crc == client_crc {
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
    if let Some((row, col)) = raw_cursor {
        let _ = write!(extra, "X-Raw-Cursor-Row: {row}\r\nX-Raw-Cursor-Col: {col}\r\n");
    }
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
    respond_with_etag_precomputed(
        stream,
        200,
        "text/plain; charset=utf-8",
        payload[start_offset..].as_bytes(),
        accept_gzip,
        None,
        &extra,
        (start_offset == 0).then(|| format!("{total_crc:08x}")),
    );
}

/// `DELETE /tabs/{idx|by-id/uuid}` — queue a tab close.
pub(in crate::api) fn delete<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str) {
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "") else {
        error_json(stream, 404, "invalid tab key");
        return;
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&state, key_raw, is_uuid) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    info!("API: closing tab {idx}");
    state.pending_closes.push(idx);
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({"closed": idx})).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /tabs` — queue a new tab (optional `{"cwd":…}`).
pub(in crate::api) fn create<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, body_bytes: &[u8]) {
    let cwd_hint: Option<std::path::PathBuf> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(body_bytes)
            .ok()
            .and_then(|v| {
                v.get("cwd")
                    .and_then(serde_json::Value::as_str)
                    .map(std::path::PathBuf::from)
            })
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    info!(
        "API: queueing new tab creation (cwd: {})",
        cwd_hint.as_ref().map_or("inherit", |p| p.to_str().unwrap_or("?"))
    );
    state.pending_new_tabs += 1;
    if let Some(cwd) = cwd_hint {
        state.pending_new_tab_cwds.push_back(cwd);
    }
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({"queued": "new"})).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /tabs/…/env` — per-tab env change.
pub(in crate::api) fn env<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/env") else {
        error_json(stream, 404, "missing tab id");
        return;
    };
    let parsed = parse_env_body(body_bytes);
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        drop(snap);
        error_json(stream, 404, "tab not found");
        return;
    };
    let id = snap.tabs[idx].id.to_string();
    match parsed {
        Ok(mut change) => {
            change.tab = Some(id);
            snap.pending_env_changes.push(change);
            drop(snap);
            respond_json(stream, 200, r#"{"queued":"env"}"#);
        }
        Err(e) => {
            drop(snap);
            error_json(stream, 400, &e);
        }
    }
}

/// `POST /tabs/…/resize` — pin or clear a tab's fixed grid size.
pub(in crate::api) fn resize<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/resize") else {
        error_json(stream, 404, "missing tab id");
        return;
    };
    let parsed: serde_json::Value = match serde_json::from_slice(body_bytes) {
        Ok(v) => v,
        Err(e) => {
            error_json(stream, 400, &format!("invalid JSON body: {e}"));
            return;
        }
    };
    let clear = parsed
        .get("clear")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let dims = if clear {
        None
    } else {
        let cols = parsed
            .get("cols")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u16::try_from(n).ok());
        let rows = parsed
            .get("rows")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u16::try_from(n).ok());
        match (cols, rows) {
            (Some(c), Some(r)) if c >= 2 && r >= 1 => Some((c, r)),
            _ => {
                error_json(stream, 400, "provide cols (>=2) and rows (>=1), or clear:true");
                return;
            }
        }
    };
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        drop(snap);
        error_json(stream, 404, "tab not found");
        return;
    };
    let id = snap.tabs[idx].id.to_string();
    snap.pending_resizes.push((id, dims));
    drop(snap);
    respond_json(stream, 200, r#"{"queued":"resize"}"#);
}

/// `POST /tabs/…/limits` — set or clear per-tab resource limits.
pub(in crate::api) fn limits<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/limits") else {
        error_json(stream, 404, "missing tab id");
        return;
    };
    let parsed: serde_json::Value = match serde_json::from_slice(body_bytes) {
        Ok(v) => v,
        Err(e) => {
            error_json(stream, 400, &format!("invalid JSON body: {e}"));
            return;
        }
    };
    let clear = parsed
        .get("clear")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let over = crate::TabResourceLimits {
        memory_max: parsed.get("memory_max").and_then(|v| v.as_str()).map(str::to_owned),
        cpu_quota_percent: parsed
            .get("cpu_quota_percent")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        tasks_max: parsed.get("tasks_max").and_then(serde_json::Value::as_u64),
    };
    if !clear && over.is_empty() {
        error_json(
            stream,
            400,
            "provide memory_max / cpu_quota_percent / tasks_max, or clear:true",
        );
        return;
    }
    if !over.memory_max_valid() {
        error_json(
            stream,
            400,
            "memory_max must be a byte count or K/M/G/T value (e.g. \"8G\")",
        );
        return;
    }
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        drop(snap);
        error_json(stream, 404, "tab not found");
        return;
    };
    let id = snap.tabs[idx].id.to_string();
    snap.pending_limit_changes.push((id, over, clear));
    drop(snap);
    respond_json(stream, 200, r#"{"queued":"limits"}"#);
}

/// `POST /tabs/{idx}/rename` — rename a tab by index.
pub(in crate::api) fn rename<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let idx_str = &p["/tabs/".len()..p.len() - "/rename".len()];
    if let Ok(idx) = idx_str.parse::<usize>() {
        let body = body_bytes;
        let new_name = serde_json::from_slice::<serde_json::Value>(body).map_or_else(
            |_| String::from_utf8_lossy(body).trim().to_string(),
            |v| v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
        );
        if new_name.is_empty() {
            error_json(stream, 400, "missing or empty name");
            return;
        }
        let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if idx < state.tabs.len() {
            info!("API: renaming tab {idx} to {new_name}");
            state.pending_renames.push((idx, new_name.clone()));
            drop(state);
            let body =
                serde_json::to_string(&serde_json::json!({"renamed": idx, "name": new_name})).unwrap_or_default();
            respond_json(stream, 200, &body);
        } else {
            error_json(stream, 404, "tab index out of range");
        }
    } else {
        error_json(stream, 404, "invalid tab index");
    }
}

/// `POST /tabs/…/files?name=<basename>` — upload into the tab's `cwd/inbox/`.
pub(in crate::api) fn files_post<S: Write>(
    stream: &mut S,
    state: &Arc<Mutex<TabSnapshot>>,
    p: &str,
    body_bytes: &[u8],
    provided_token: Option<&str>,
    query_name: Option<&str>,
) {
    let upload_token = provided_token.unwrap_or("");
    let _slot = match UploadSlot::try_acquire(upload_token) {
        Ok(s) => s,
        Err(n) => {
            error_json(
                stream,
                429,
                &format!(
                    "too many concurrent uploads from this token ({n} already in flight; cap {UPLOAD_MAX_INFLIGHT_PER_TOKEN})"
                ),
            );
            return;
        }
    };
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/files") else {
        error_json(stream, 404, "invalid tab key");
        return;
    };
    let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        drop(snap);
        error_json(stream, 404, "tab not found");
        return;
    };
    let Some(t) = snap.tabs.get(idx) else {
        drop(snap);
        error_json(stream, 404, "tab index out of range");
        return;
    };
    if crate::schedule::LockState::effective_locked(t) {
        drop(snap);
        error_json(stream, 423, "tab is locked");
        return;
    }
    let cwd = t.cwd.clone();
    drop(snap);
    let Some(cwd) = cwd else {
        error_json(stream, 400, "tab has no known cwd");
        return;
    };
    let Some(name) = query_name.and_then(sanitize_basename) else {
        error_json(stream, 400, "missing or invalid ?name=<basename>");
        return;
    };
    if body_bytes.len() > UPLOAD_MAX_BYTES {
        error_json(stream, 413, &format!("upload exceeds {UPLOAD_MAX_BYTES_MIB} MiB limit"));
        return;
    }
    let inbox = std::path::Path::new(&*cwd).join("inbox");
    if let Err(e) = std::fs::create_dir_all(&inbox) {
        error_json(stream, 500, &format!("mkdir inbox: {e}"));
        return;
    }
    let resolved = std::path::Path::new(&*cwd)
        .canonicalize()
        .ok()
        .zip(inbox.canonicalize().ok());
    let Some((cwd_canon, inbox_canon)) = resolved else {
        error_json(stream, 404, "inbox path unreadable");
        return;
    };
    if !inbox_canon.starts_with(&cwd_canon) || inbox_canon.file_name() != Some(std::ffi::OsStr::new("inbox")) {
        error_json(stream, 403, "inbox escapes the tab's cwd");
        return;
    }
    let dest = inbox_canon.join(&name);
    let staging = inbox_canon.join(format!(".{name}.tmp"));
    if let Err(e) = write_new_file_no_symlink(&staging, body_bytes) {
        error_json(stream, 500, &format!("write inbox/.{name}.tmp: {e}"));
        return;
    }
    if let Err(e) = std::fs::rename(&staging, &dest) {
        let _ = std::fs::remove_file(&staging);
        error_json(stream, 500, &format!("rename into inbox/{name}: {e}"));
        return;
    }
    info!("API: stored {} bytes in {}", body_bytes.len(), dest.display());
    let body = serde_json::to_string(&serde_json::json!({
        "path": dest.to_string_lossy(),
        "relpath": format!("inbox/{name}"),
        "bytes": body_bytes.len(),
    }))
    .unwrap_or_default();
    respond_json(stream, 201, &body);
}

/// `GET /tabs/…/files?path=…` — download a file from the tab's sandbox.
pub(in crate::api) fn files_get<S: Write>(
    stream: &mut S,
    state: &Arc<Mutex<TabSnapshot>>,
    p: &str,
    accept_gzip: bool,
    if_none_match: Option<&str>,
    query_path: Option<&str>,
) {
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/files") else {
        error_json(stream, 404, "invalid tab key");
        return;
    };
    let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        drop(snap);
        error_json(stream, 404, "tab not found");
        return;
    };
    let Some(t) = snap.tabs.get(idx) else {
        drop(snap);
        error_json(stream, 404, "tab index out of range");
        return;
    };
    let cwd = t.cwd.clone();
    drop(snap);
    let Some(cwd) = cwd else {
        error_json(stream, 400, "tab has no known cwd");
        return;
    };
    let Some(raw_path) = query_path else {
        error_json(stream, 400, "missing ?path=<relative-path>");
        return;
    };
    let canonical = match resolve_sandbox_path(&cwd, raw_path) {
        Ok(p) => p,
        Err((status, msg)) => {
            error_json(stream, status, &msg);
            return;
        }
    };
    let Ok(meta) = std::fs::symlink_metadata(&canonical) else {
        error_json(stream, 404, "file not found");
        return;
    };
    if !meta.file_type().is_file() {
        error_json(stream, 403, "not a regular file");
        return;
    }
    let Ok(bytes) = std::fs::read(&canonical) else {
        error_json(stream, 404, "file not found");
        return;
    };
    let display_name = canonical.file_name().and_then(|s| s.to_str()).unwrap_or("download");
    info!("API: served {} bytes from {}", bytes.len(), canonical.display());
    let accept_gzip = accept_gzip && bytes.len() <= DOWNLOAD_GZIP_MAX;
    let mut percent: String = String::with_capacity(display_name.len());
    for byte in display_name.bytes() {
        if matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~') {
            percent.push(byte as char);
        } else {
            let _ = write!(&mut percent, "%{byte:02X}");
        }
    }
    let ascii_fallback: String = display_name
        .chars()
        .filter(|c| c.is_ascii() && *c != '"' && *c != '\\')
        .collect();
    let disposition = format!(
        "Content-Disposition: attachment; filename=\"{ascii_fallback}\"; filename*=UTF-8''{percent}\r\nX-Content-Type-Options: nosniff\r\n"
    );
    respond_with_etag(
        stream,
        200,
        "application/octet-stream",
        &bytes,
        accept_gzip,
        if_none_match,
        &disposition,
    );
}

/// `GET /tabs/…/outbox` | `…/inbox` — list a sandbox dir's file tree.
pub(in crate::api) fn list_dir<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str) {
    let dirname = if p.ends_with("/outbox") { "outbox" } else { "inbox" };
    let suffix = if dirname == "outbox" { "/outbox" } else { "/inbox" };
    let Some((key_raw, is_uuid)) = parse_tab_key(p, suffix) else {
        error_json(stream, 404, "invalid tab key");
        return;
    };
    let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
        drop(snap);
        error_json(stream, 404, "tab not found");
        return;
    };
    let Some(t) = snap.tabs.get(idx) else {
        drop(snap);
        error_json(stream, 404, "tab index out of range");
        return;
    };
    let cwd = t.cwd.clone();
    drop(snap);
    let Some(cwd) = cwd else {
        respond_json(stream, 200, r#"{"files":[],"dir":""}"#);
        return;
    };
    let dir_path = std::path::Path::new(&*cwd).join(dirname);
    let mut files: Vec<serde_json::Value> = Vec::new();
    collect_files_tree(&dir_path, "", 0, &mut files);
    files.sort_by(|a, b| a["path"].as_str().unwrap_or("").cmp(b["path"].as_str().unwrap_or("")));
    let body = serde_json::to_string(&serde_json::json!({
        "files": files,
        "dir": dir_path.to_string_lossy(),
    }))
    .unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /tabs/by-id/{uuid}/lock` — flip the per-tab lock.
pub(in crate::api) fn lock<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let inner = &p["/tabs/by-id/".len()..p.len() - "/lock".len()];
    let on_body: Option<bool> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(body_bytes)
            .ok()
            .and_then(|v| v.get("on").and_then(serde_json::Value::as_bool))
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    let current_locked = state.tabs[idx].locked;
    let new_val = on_body.unwrap_or(!current_locked);
    if !new_val && crate::schedule::lock_reason(false, state.tabs[idx].schedule.as_ref()) == Some("schedule") {
        drop(state);
        error_json(stream, 423, "schedule is closed");
        return;
    }
    state.tabs[idx].locked = new_val;
    state.pending_lock_changes.push((tab_id, new_val));
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({"locked": new_val})).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /tabs/by-id/{uuid}/net` — turn the tab's internet off/on.
pub(in crate::api) fn net<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let inner = &p["/tabs/by-id/".len()..p.len() - "/net".len()];
    let disabled_body: Option<bool> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(body_bytes)
            .ok()
            .and_then(|v| v.get("disabled").and_then(serde_json::Value::as_bool))
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    let new_val = disabled_body.unwrap_or(!state.tabs[idx].net_disabled);
    if new_val && !crate::bwrap_available() {
        drop(state);
        error_json(stream, 412, "bubblewrap (bwrap) is not installed");
        return;
    }
    state.tabs[idx].net_disabled = new_val;
    state.pending_net_changes.push((tab_id, new_val));
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({"net_disabled": new_val})).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /tabs/by-id/{uuid}/net-allow` — allowlist mode (headless only).
pub(in crate::api) fn net_allow<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    #[cfg(feature = "gui")]
    {
        let _ = (state, p, body_bytes);
        error_json(
            stream,
            501,
            "per-tab allowlist (net-allow) requires the headless daemon (nftables / CAP_NET_ADMIN); \
             the desktop GUI supports only full airgap via net-off / net-on",
        );
    }
    #[cfg(not(feature = "gui"))]
    {
        let inner = &p["/tabs/by-id/".len()..p.len() - "/net-allow".len()];
        let val: serde_json::Value = if body_bytes.is_empty() {
            serde_json::json!({})
        } else {
            let Ok(v) = serde_json::from_slice(body_bytes) else {
                error_json(stream, 400, "invalid JSON body");
                return;
            };
            v
        };
        let str_array = |key: &str| -> Vec<String> {
            val.get(key)
                .and_then(serde_json::Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        let mut presets = Vec::new();
        for id in str_array("presets") {
            let Some(p) = crate::net_policy::Preset::from_id(&id) else {
                error_json(stream, 400, &format!("unknown preset: {id}"));
                return;
            };
            presets.push(p);
        }
        let domains = str_array("domains");
        let cidrs = str_array("cidrs");
        for c in &cidrs {
            if crate::net_policy::Cidr::parse(c).is_none() {
                error_json(stream, 400, &format!("invalid CIDR: {c}"));
                return;
            }
        }
        let config = crate::net_policy::AllowConfig {
            presets,
            domains,
            cidrs,
        };
        let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
            drop(state);
            error_json(stream, 404, "tab not found");
            return;
        };
        let tab_id = state.tabs[idx].id.to_string();
        if !config.is_empty() {
            state.tabs[idx].net_disabled = false;
        }
        let active = !config.is_empty();
        state.pending_net_allow_changes.push((tab_id, config));
        drop(state);
        let body = serde_json::to_string(&serde_json::json!({"allowlist_active": active})).unwrap_or_default();
        respond_json(stream, 200, &body);
    }
}

/// `POST /tabs/by-id/{uuid}/ssh-agent` — per-tab ssh-agent (headless only).
pub(in crate::api) fn ssh_agent<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    #[cfg(feature = "gui")]
    {
        let _ = (state, p, body_bytes);
        error_json(
            stream,
            501,
            "per-tab ssh-agent requires the headless daemon; the desktop GUI does not manage per-tab agents",
        );
    }
    #[cfg(not(feature = "gui"))]
    {
        let inner = &p["/tabs/by-id/".len()..p.len() - "/ssh-agent".len()];
        let val: serde_json::Value = if body_bytes.is_empty() {
            serde_json::json!({})
        } else {
            let Ok(v) = serde_json::from_slice(body_bytes) else {
                error_json(stream, 400, "invalid JSON body");
                return;
            };
            v
        };
        let enabled = val.get("enabled").and_then(serde_json::Value::as_bool).unwrap_or(true);
        let key = val.get("key").and_then(serde_json::Value::as_str).map(str::to_string);
        let config = enabled.then_some(crate::SshAgentConfig { key });
        let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
            drop(state);
            error_json(stream, 404, "tab not found");
            return;
        };
        let tab_id = state.tabs[idx].id.to_string();
        state.pending_ssh_agent_changes.push((tab_id, config));
        drop(state);
        let body = serde_json::to_string(&serde_json::json!({"ssh_agent": enabled})).unwrap_or_default();
        respond_json(stream, 200, &body);
    }
}

/// `POST /tabs/by-id/{uuid}/schedule` — set/clear the off-hours auto-lock schedule.
pub(in crate::api) fn schedule<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    #[derive(serde::Deserialize)]
    struct Body {
        rule: Option<String>,
        tz: Option<String>,
    }
    let inner = &p["/tabs/by-id/".len()..p.len() - "/schedule".len()];
    let parsed: Option<Body> = if body_bytes.is_empty() {
        Some(Body { rule: None, tz: None })
    } else {
        serde_json::from_slice::<Body>(body_bytes).ok()
    };
    let Some(body) = parsed else {
        error_json(stream, 400, "invalid JSON body");
        return;
    };
    let schedule_opt: Option<crate::schedule::TabSchedule> = match (body.rule.as_deref(), body.tz.as_deref()) {
        (None | Some(""), _) => None,
        (Some(rule), Some(tz)) => match crate::schedule::TabSchedule::new(rule, tz) {
            Ok(s) => Some(s),
            Err(e) => {
                error_json(stream, 400, &format!("{e}"));
                return;
            }
        },
        (Some(_), None) => {
            error_json(stream, 400, "tz is required when rule is set");
            return;
        }
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    state.tabs[idx].schedule.clone_from(&schedule_opt);
    state.pending_schedule_changes.push((tab_id, schedule_opt.clone()));
    drop(state);
    let body = schedule_opt.as_ref().map_or_else(
        || serde_json::json!({"rule": serde_json::Value::Null}),
        |s| serde_json::json!({"rule": s.rule, "tz": s.tz}),
    );
    respond_json(stream, 200, &body.to_string());
}

/// `POST /tabs/by-id/{uuid}/bg-color` — set/clear the per-tab bg override.
pub(in crate::api) fn bg_color<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    let inner = &p["/tabs/by-id/".len()..p.len() - "/bg-color".len()];
    let parsed: Option<Option<String>> = if body_bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(body_bytes)
            .ok()
            .and_then(|v| {
                let c = v.get("color")?;
                if c.is_null() {
                    Some(None)
                } else {
                    c.as_str().map(|s| Some(s.to_string()))
                }
            })
    };
    let Some(color_opt) = parsed else {
        error_json(stream, 400, "missing {\"color\": \"#RRGGBB\"} or {\"color\": null}");
        return;
    };
    if let Some(ref c) = color_opt
        && (c.len() != 7 || !c.starts_with('#') || !c[1..].chars().all(|x| x.is_ascii_hexdigit()))
    {
        error_json(stream, 400, "color must be #RRGGBB");
        return;
    }
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    state.tabs[idx].bg_color = color_opt.as_deref().unwrap_or_default().into();
    state.pending_bg_color_changes.push((tab_id, color_opt.clone()));
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({ "color": color_opt })).unwrap_or_default();
    respond_json(stream, 200, &body);
}

/// `POST /tabs/…/input` — send input bytes to the tab (refused when locked).
pub(in crate::api) fn input<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: Vec<u8>) {
    let Some((key_raw, is_uuid)) = parse_tab_key(p, "/input") else {
        error_json(stream, 404, "invalid tab key");
        return;
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(idx) = resolve_tab_idx(&state, key_raw, is_uuid) {
        if crate::schedule::LockState::effective_locked(&state.tabs[idx]) {
            drop(state);
            error_json(stream, 403, "tab is locked");
            return;
        }
        info!("API: sending {} bytes of input to tab {idx}", body_bytes.len());
        let n = body_bytes.len();
        state.pending_input.push((idx, body_bytes));
        drop(state);
        let resp = serde_json::to_string(&serde_json::json!({"sent": n})).unwrap_or_default();
        respond_json(stream, 200, &resp);
    } else {
        drop(state);
        error_json(stream, 404, "tab not found");
    }
}
