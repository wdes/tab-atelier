// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-tab network resources: full-airgap (`net-off` / `net-on`, a bubblewrap
//! net-namespace jail) and the nftables allowlist (`net-allow`). Both queue
//! to the owner (the shell respawns to apply). The allowlist is headless-only
//! (needs `CAP_NET_ADMIN`); the GUI edition 501s.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, error_json, respond_json};

pub(super) fn set<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Turn the tab's internet off / on (bubblewrap net-namespace
    // jail). Master token only (share-token gate above does not
    // allow `/net`). Body `{"disabled": true|false}`; absent →
    // toggle. The shell respawns to apply, so the change isn't
    // instantaneous — the runtime tab picks it up next tick.
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
    // Refuse turning net OFF when bubblewrap isn't installed —
    // there's no way to build the netns jail, and silently
    // leaving the net on would be a lie. Turning net back ON is
    // always allowed (no bwrap needed to un-jail).
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

// The params are unused in the GUI edition, which only 501s.
#[cfg_attr(feature = "gui", allow(unused_variables))]
pub(super) fn allow<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Put the tab into allowlist mode (or clear it). Master token
    // only. Body: `{"presets":[...],"domains":[...],"cidrs":[...]}`;
    // an empty/absent set clears allowlist mode (back to On). A
    // non-empty set also clears net-off (mutually exclusive). The
    // shell respawns to apply, so it's not instantaneous.
    //
    // Per-tab allowlisting is enforced by nftables + a DNS pre-resolver
    // that need CAP_NET_ADMIN — a headless-daemon capability. The
    // unprivileged desktop GUI can't install them and doesn't drain
    // `pending_net_allow_changes`, so accepting the request would
    // enforce NOTHING while reporting success (a security-relevant
    // false positive). Refuse with 501 on the GUI instead. Full airgap
    // (net-off/net-on) is unprivileged and works on both editions.
    #[cfg(feature = "gui")]
    error_json(
        stream,
        501,
        "per-tab allowlist (net-allow) requires the headless daemon (nftables / CAP_NET_ADMIN); \
         the desktop GUI supports only full airgap via net-off / net-on",
    );
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
        // Validate presets + CIDRs up front so a typo is a clear 400
        // rather than a silently-dropped rule.
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
        // A non-empty allowlist clears full-airgap (mutually exclusive).
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
