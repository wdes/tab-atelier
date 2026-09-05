// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The core tab collection: the `GET /` / `/tabs` list (ETag-cached), tab
//! creation (`POST /tabs`) and close (`DELETE /tabs/<id>`).

use std::io::Write;
use std::sync::{Arc, Mutex};

use log::info;

use super::{
    ApiResponse, DnsEntryInfo, HostInfo, TabInfo, TabSnapshot, error_json, parse_tab_key, resolve_tab_idx,
    respond_json, respond_with_etag, strip_ansi,
};

pub(super) fn list<W: Write>(
    stream: &mut W,
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
            // The cached output now ships ANSI SGR escapes for
            // remote-side colouring, but the tab-list preview is
            // rendered as plain Text — strip them first so the
            // ESC byte and `[…m` payload don't show up as junk.
            preview: strip_ansi(t.output.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("")),
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
            meta: t.meta.clone(),
            badge: t.badge.as_deref().map(str::to_string),
            // Mirror what /output would serve: raw_output when present (what
            // the viewer and brain read), else the joined form.
            output_crc: if t.raw_output.is_empty() {
                t.output_crc
            } else {
                t.raw_output_crc
            },
            output_len: if t.raw_output.is_empty() {
                t.output.len() as u64
            } else {
                t.raw_output.len() as u64
            },
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
        })
        .collect();
    #[cfg(feature = "energy")]
    let host = HostInfo {
        battery_percent: state.battery_percent,
        // Sum each tab's watts to give a host-wide draw figure;
        // tabs without a reading contribute zero, which is the
        // honest answer for any not-yet-sampled process.
        watts: {
            let total: f64 = state.power.iter().filter_map(|p| p.watts).sum();
            if total > 0.0 { Some(total) } else { None }
        },
    };
    #[cfg(not(feature = "energy"))]
    let host = HostInfo::default();
    let resp = ApiResponse {
        app: crate::tracking::USER_AGENT,
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

pub(super) fn close<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str) {
    // Accepts `/tabs/<idx>` and `/tabs/by-id/<uuid>` — the UUID is
    // the stable handle (index drifts as tabs open/close).
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

pub(super) fn create<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, body_bytes: &[u8]) {
    // Optional JSON body: `{"cwd": "<path>"}` opens the tab
    // rooted at that path instead of inheriting from the
    // active tab. Missing or invalid body → falls back to the
    // legacy inherit-cwd behaviour.
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
