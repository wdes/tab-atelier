// SPDX-License-Identifier: MPL-2.0

//! The core tab collection: the `GET /` / `/tabs` list (cached, conditional),
//! tab creation (`POST /tabs`) and close (`DELETE /tabs/<id>`).

use std::sync::{Arc, Mutex};

use log::info;
use tab_atelier_api::{Conditional, Doc, Failure, Reply, Request, cached};

use super::{
    ApiResponse, DnsEntryInfo, HostInfo, NewTabCreated, PendingNewTab, TabInfo, TabSnapshot, etag_for, parse_tab_key,
    resolve_tab_idx, strip_ansi,
};

/// The projection every tab-list caller reads — built once, from the snapshot.
///
/// Extracted from the handler so the cached body and the JSON body in
/// `cached_response` are demonstrably the same function, and so the feature
/// gates (`energy`) live in one place rather than inside a closure that also
/// builds the envelope.
fn project(state: &TabSnapshot) -> Vec<TabInfo> {
    state
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
            // The dashboard groups by `phase`; a tab with no assignment reports `null` rather than
            // an empty string, so "nobody assigned this" is distinguishable from "assigned to a
            // phase whose name is empty" — the latter being a typo, not a state.
            assignment: t.assignment.clone(),
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
        .collect()
}

/// The host-wide half of the envelope, feature-gated in one place.
fn host_of(state: &TabSnapshot) -> HostInfo {
    #[cfg(feature = "energy")]
    {
        HostInfo {
            battery_percent: state.battery_percent,
            // Sum each tab's watts to give a host-wide draw figure;
            // tabs without a reading contribute zero, which is the
            // honest answer for any not-yet-sampled process.
            watts: {
                let total: f64 = state.power.iter().filter_map(|p| p.watts).sum();
                if total > 0.0 { Some(total) } else { None }
            },
        }
    }
    #[cfg(not(feature = "energy"))]
    {
        let _ = state;
        HostInfo::default()
    }
}

pub(super) fn list(state: &Arc<Mutex<TabSnapshot>>, client: Conditional<'_>) -> Reply {
    // The body is cached on the snapshot and cleared by `persist()` on any
    // mutation, so the expensive part (`strip_ansi` per tab, pretty-printed
    // JSON) happens once per generation instead of once per poll. The lock is
    // released before the reply is built, so a slow client cannot hold up the
    // owner loop.
    let body: bytes::Bytes = {
        let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cached_response.clone().unwrap_or_else(|| {
            let resp = ApiResponse {
                app: crate::tracking::USER_AGENT,
                host: host_of(&state),
                tabs: project(&state),
            };
            let bytes = bytes::Bytes::from(serde_json::to_string_pretty(&resp).unwrap_or_default());
            state.cached_response = Some(bytes.clone());
            bytes
        })
    };
    let etag = etag_for(&body);
    cached(200, "application/json", body, &etag, client)
}

pub(super) fn close(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Closed>, Failure> {
    // Accepts `/tabs/<idx>` and `/tabs/by-id/<uuid>` — the UUID is
    // the stable handle (index drifts as tabs open/close).
    let Some((key_raw, is_uuid)) = parse_tab_key(request.path, "") else {
        return Err(Failure::problem(404, "invalid tab key"));
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = resolve_tab_idx(&state, key_raw, is_uuid) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    info!("API: closing tab {idx}");
    state.pending_closes.push(idx);
    drop(state);
    Ok(Doc::ok(Closed { closed: idx }))
}

/// Which tab was queued for closing.
#[derive(Debug, serde::Serialize, tab_atelier_api::schemars::JsonSchema)]
pub(super) struct Closed {
    closed: usize,
}

/// A parsed `POST /tabs` body.
struct CreateRequest {
    cwd: Option<std::path::PathBuf>,
    meta: std::collections::BTreeMap<String, String>,
    sync: bool,
}

/// Parse a `POST /tabs` body.
///
/// An empty or unparseable body is NOT an error — it is the legacy "no hints,
/// inherit the cwd" request that `add`, `dispatch --new` and the kiosk all
/// send. Only a *present but invalid* `meta` is refused: the caller named
/// labels it believes exist, and storing something else — or dropping them —
/// is the silent lie this endpoint already had with `cwd`.
///
/// A `null` value inside `meta` is invalid, not a delete: deletion is not a
/// creation-time concept, so the only thing a null could mean here is a
/// mistake.
///
/// # Errors
/// A human-readable reason, for a 400.
fn parse_create_body(body_bytes: &[u8]) -> Result<CreateRequest, String> {
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(body_bytes) else {
        return Ok(CreateRequest {
            cwd: None,
            meta: std::collections::BTreeMap::new(),
            sync: false,
        });
    };
    let cwd = parsed
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(std::path::PathBuf::from);
    let sync = parsed.get("sync").and_then(serde_json::Value::as_bool).unwrap_or(false);
    let mut meta = std::collections::BTreeMap::new();
    if let Some(field) = parsed.get("meta").filter(|value| !value.is_null()) {
        let Some(object) = field.as_object() else {
            return Err("meta must be an object of key/value strings".to_string());
        };
        for (key, value) in object {
            let Some(value) = value.as_str() else {
                return Err(format!("meta value for {key:?} must be a string"));
            };
            let (key, value) = crate::sanitize_meta(key, value)?;
            meta.insert(key, value);
        }
        if meta.len() > crate::META_MAX_KEYS {
            return Err(format!("meta is full ({} keys)", crate::META_MAX_KEYS));
        }
    }
    Ok(CreateRequest { cwd, meta, sync })
}

/// What a creation answers: queued (with the cwd accepted), or synchronously
/// created with the tab's id.
///
/// One type for both because it is one route and the shape is the response's,
/// not the caller's: `queued` is present when the request was async, `id` when
/// it was `sync`. Exactly one of the two is ever serialized.
#[derive(Debug, serde::Serialize, tab_atelier_api::schemars::JsonSchema)]
pub(super) struct Created {
    #[serde(skip_serializing_if = "Option::is_none")]
    queued: Option<&'static str>,
    /// The cwd this creation landed on, always present and `null` when the tab
    /// inherited one. Not skipped: "which directory did I get" is the whole
    /// question a creation reply answers, and an absent key would read as
    /// "unchanged" rather than "none".
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    meta: Option<std::collections::BTreeMap<String, String>>,
}

pub(super) fn create(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Created>, Failure> {
    let CreateRequest { cwd, meta, sync } =
        parse_create_body(request.body).map_err(|reason| Failure::problem(400, reason))?;
    // What was accepted, captured before the hint is moved into the queue below.
    let accepted_cwd = cwd.as_ref().map(|p| p.display().to_string());
    info!(
        "API: queueing new tab creation (cwd: {}, meta: {} key(s), sync: {sync})",
        cwd.as_ref().map_or("inherit", |p| p.to_str().unwrap_or("?")),
        meta.len()
    );
    let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Result<NewTabCreated, String>>();
    {
        let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // One entry per creation, ALWAYS, carrying THIS request's hints. A bare
        // counter plus a queue of cwds could drift apart — an entry-less
        // creation shifted every later hint onto the wrong tab; a queue of
        // requests cannot.
        state.pending_new_tabs.push_back(PendingNewTab {
            cwd,
            meta: meta.clone(),
            reply: sync.then_some(reply_tx),
        });
        // Re-bump the activity counter AFTER the entry is queued. Auth already
        // bumped it before we got here, and the GUI's fast tick may have already
        // consumed that bump — which only makes it NOTICE the request, not act
        // on it: the new-tab drain runs in render() (it needs a Window), and
        // only a fresh bump makes that tick ask for the frame. Without it a
        // `sync` reply waits for whatever unrelated repaint comes next, which on
        // a quiet or hidden window can be a long time.
        state.touch();
    } // released before the wait below — the drains need this mutex to make the tab.

    if !sync {
        // The legacy reply, unchanged: what was accepted, not merely that
        // something was.
        return Ok(Doc::ok(Created {
            queued: Some("new"),
            cwd: accepted_cwd,
            id: None,
            meta: None,
        }));
    }

    // Blocking wait, no timeout: it ends on the owner's reply, on an error, or
    // when the sender is dropped (the entry was taken and its drain never
    // answered). The tab is created either way.
    match reply_rx.recv() {
        Ok(Ok(created)) => Ok(Doc::ok(Created {
            queued: None,
            // The tab's *own* cwd, resolved by the drain — not the one that was
            // asked for. They differ whenever the request omitted it or asked
            // for one that was refused, which is exactly when a caller needs to
            // know what it actually got.
            cwd: created.cwd.map(|p| p.display().to_string()),
            id: Some(created.id),
            meta: Some(meta),
        })),
        Ok(Err(reason)) => Err(Failure::problem(500, format!("tab creation failed: {reason}"))),
        Err(_) => Err(Failure::problem(500, "tab creation aborted (owner went away)")),
    }
}
