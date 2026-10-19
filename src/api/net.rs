// SPDX-License-Identifier: MPL-2.0

//! Per-tab network resources: full-airgap (`net-off` / `net-on`, a bubblewrap
//! net-namespace jail) and the nftables allowlist (`net-allow`). Both queue
//! to the owner (the shell respawns to apply). The allowlist is headless-only
//! (needs `CAP_NET_ADMIN`); the GUI edition 501s.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tab_atelier_api::schemars;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};

use super::TabSnapshot;

/// The resulting airgap state.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct NetState {
    net_disabled: bool,
}

/// Whether allowlist mode is now on.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Allowlist {
    allowlist_active: bool,
}

/// The tab a `/tabs/by-id/<uuid>/<suffix>` path names.
fn tab_of<'a>(path: &'a str, suffix: &str) -> Option<&'a str> {
    path.strip_prefix("/tabs/by-id/")?.strip_suffix(suffix)
}

pub(super) fn set(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<NetState>, Failure> {
    // Turn the tab's internet off / on (bubblewrap net-namespace
    // jail). Master token only (share-token gate above does not
    // allow `/net`). Body `{"disabled": true|false}`; absent →
    // toggle. The shell respawns to apply, so the change isn't
    // instantaneous — the runtime tab picks it up next tick.
    let Some(inner) = tab_of(request.path, "/net") else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let disabled_body: Option<bool> = if request.body.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(request.body)
            .ok()
            .and_then(|v| v.get("disabled").and_then(serde_json::Value::as_bool))
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        return Err(Failure::problem(404, "tab not found"));
    };
    let tab_id = state.tabs[idx].id.to_string();
    let new_val = disabled_body.unwrap_or(!state.tabs[idx].net_disabled);
    // Refuse turning net OFF when bubblewrap isn't installed —
    // there's no way to build the netns jail, and silently
    // leaving the net on would be a lie. Turning net back ON is
    // always allowed (no bwrap needed to un-jail).
    if new_val && !crate::bwrap_available() {
        return Err(Failure::problem(412, "bubblewrap (bwrap) is not installed"));
    }
    state.tabs[idx].net_disabled = new_val;
    state.pending_net_changes.push((tab_id, new_val));
    drop(state);
    Ok(Doc::ok(NetState { net_disabled: new_val }))
}

/// Put the tab into allowlist mode (or clear it). Master token only.
///
/// On the desktop GUI this refuses: the enforcement is nftables plus a DNS
/// pre-resolver, both needing `CAP_NET_ADMIN`. An unprivileged GUI cannot
/// install them and does not drain `pending_net_allow_changes`, so accepting the
/// request would enforce nothing while reporting success — a security-relevant
/// false positive. It is a 501 either way the edition is built; only the
/// *reason* differs, so the refusal is one `Failure` rather than two returns.
pub(super) fn allow(request: &Request<'_>, state: &Arc<Mutex<TabSnapshot>>) -> Result<Doc<Allowlist>, Failure> {
    #[cfg(feature = "gui")]
    {
        let _ = (request, state);
        Err(Failure::problem(
            501,
            "per-tab allowlist (net-allow) requires the headless daemon (nftables / CAP_NET_ADMIN); \
             the desktop GUI supports only full airgap via net-off / net-on",
        ))
    }
    #[cfg(not(feature = "gui"))]
    {
        // Body: `{"presets":[...],"domains":[...],"cidrs":[...]}`;
        // an empty/absent set clears allowlist mode (back to On). A
        // non-empty set also clears net-off (mutually exclusive). The
        // shell respawns to apply, so it's not instantaneous.
        let Some(inner) = tab_of(request.path, "/net-allow") else {
            return Err(Failure::problem(404, "tab not found"));
        };
        let val: serde_json::Value = if request.body.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_slice(request.body).map_err(|_| Failure::problem(400, "invalid JSON body"))?
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
            let preset = crate::net_policy::Preset::from_id(&id)
                .ok_or_else(|| Failure::problem(400, format!("unknown preset: {id}")))?;
            presets.push(preset);
        }
        let domains = str_array("domains");
        let cidrs = str_array("cidrs");
        for c in &cidrs {
            if crate::net_policy::Cidr::parse(c).is_none() {
                return Err(Failure::problem(400, format!("invalid CIDR: {c}")));
            }
        }
        let config = crate::net_policy::AllowConfig {
            presets,
            domains,
            cidrs,
        };
        let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
            return Err(Failure::problem(404, "tab not found"));
        };
        let tab_id = state.tabs[idx].id.to_string();
        // A non-empty allowlist clears full-airgap (mutually exclusive).
        if !config.is_empty() {
            state.tabs[idx].net_disabled = false;
        }
        let active = !config.is_empty();
        state.pending_net_allow_changes.push((tab_id, config));
        drop(state);
        Ok(Doc::ok(Allowlist {
            allowlist_active: active,
        }))
    }
}
