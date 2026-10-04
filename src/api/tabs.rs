// SPDX-License-Identifier: MPL-2.0

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
            assignment: t.assignment.as_deref().map(str::to_string),
            parent_tab_id: t.parent_tab_id.as_deref().map(str::to_string),
            rehome_status: t.rehome_status.as_deref().map(str::to_string),
            specialty: t.specialty.as_deref().map(str::to_string),
            orchestrator: t.orchestrator.as_deref().map(str::to_string),
            objective: t.objective.as_deref().map(str::to_string),
            current_task_log: t.current_task.clone(),
            conventions: t.conventions.clone(),
            evaluations: t.evaluations.clone(),
            rounds_active: t.rounds_active.clone(),
            usage_count: t.usage_count,
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

/// Parse the optional `POST /tabs` body into a [`crate::api::NewTabSpec`].
///
/// Separate from [`create`] so the rules below are testable without a socket.
/// Every field is independent and optional, and anything unparseable yields the
/// default spec — the caller then gets exactly the behaviour the endpoint had
/// before `name` and `cmd` existed, which is the safe failure for an HTTP
/// handler that cannot report a body error mid-request.
fn parse_new_tab_spec(body_bytes: &[u8]) -> crate::api::NewTabSpec {
    if body_bytes.is_empty() {
        return crate::api::NewTabSpec::default();
    }
    serde_json::from_slice::<serde_json::Value>(body_bytes)
        .ok()
        .map_or_else(crate::api::NewTabSpec::default, |v| {
            crate::api::NewTabSpec {
                cwd: v
                    .get("cwd")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(std::path::PathBuf::from),
                name: v
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                // Trailing newline, because what receives this is a shell at a
                // prompt: a command without its Enter is a command nobody runs.
                cmd: v
                    .get("cmd")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        if s.ends_with('\n') {
                            s.to_string()
                        } else {
                            format!("{s}\n")
                        }
                    }),
            }
        })
}

pub(super) fn create<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, body_bytes: &[u8]) {
    let spec = parse_new_tab_spec(body_bytes);
    let explicit = spec.cwd.is_some() || spec.name.is_some() || spec.cmd.is_some();
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    info!(
        "API: queueing new tab creation (cwd: {}, name: {}, cmd: {})",
        spec.cwd.as_ref().map_or("inherit", |p| p.to_str().unwrap_or("?")),
        spec.name.as_deref().unwrap_or("default"),
        if spec.cmd.is_some() { "yes" } else { "no" }
    );
    state.pending_new_tabs += 1;
    // A spec that names nothing is not queued: the drain side pops one per
    // creation and treats its absence as "inherit", which is the same outcome
    // an empty spec would produce.
    if explicit {
        state.pending_new_tab_cwds.push_back(spec);
    }
    drop(state);
    let body = serde_json::to_string(&serde_json::json!({"queued": "new"})).unwrap_or_default();
    respond_json(stream, 200, &body);
}

#[cfg(test)]
mod tests {
    use super::parse_new_tab_spec;

    /// Each field of the body is independent: a caller naming only a command
    /// gets a tab whose cwd is inherited and whose name is the default, which
    /// is the whole point of the change — one request instead of four.
    #[test]
    fn a_body_carries_any_subset_of_cwd_name_and_cmd() {
        let full = parse_new_tab_spec(br#"{"cwd":"/tmp","name":"Bot","cmd":"echo hi"}"#);
        assert_eq!(full.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
        assert_eq!(full.name.as_deref(), Some("Bot"));
        assert_eq!(full.cmd.as_deref(), Some("echo hi\n"));

        let only_cmd = parse_new_tab_spec(br#"{"cmd":"catbus-agent"}"#);
        assert!(only_cmd.cwd.is_none(), "cwd must stay None so it is inherited");
        assert!(only_cmd.name.is_none(), "name must stay None for the default");
        assert_eq!(only_cmd.cmd.as_deref(), Some("catbus-agent\n"));

        let only_name = parse_new_tab_spec(br#"{"name":"Planner"}"#);
        assert!(only_name.cwd.is_none() && only_name.cmd.is_none());
        assert_eq!(only_name.name.as_deref(), Some("Planner"));
    }

    /// The command gets exactly one newline, added or kept, never doubled.
    ///
    /// It is typed into a shell, so a missing Enter is a command nobody runs
    /// and two would run an empty line after it.
    #[test]
    fn the_command_gains_one_newline_and_no_more() {
        assert_eq!(parse_new_tab_spec(br#"{"cmd":"ls"}"#).cmd.as_deref(), Some("ls\n"));
        assert_eq!(parse_new_tab_spec(br#"{"cmd":"ls\n"}"#).cmd.as_deref(), Some("ls\n"));
    }

    /// A body that says nothing, or says nothing usable, falls back to the
    /// legacy inherit-everything behaviour rather than failing the request —
    /// an HTTP handler has no way to report a body error mid-response.
    #[test]
    fn an_empty_or_unusable_body_yields_the_default_spec() {
        for body in [
            &b""[..],
            b"not json",
            b"{}",
            br#"{"cwd":""}"#,
            br#"{"name":""}"#,
            br#"{"cmd":""}"#,
            br#"{"cwd":42,"name":null,"cmd":[]}"#,
        ] {
            let spec = parse_new_tab_spec(body);
            assert!(
                spec.cwd.is_none() && spec.name.is_none() && spec.cmd.is_none(),
                "body {body:?} should name nothing"
            );
        }
    }
}
