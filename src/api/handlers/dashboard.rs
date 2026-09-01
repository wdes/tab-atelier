// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/dashboard[/state|/activity|/share-token]` route handlers. Bodies moved
//! verbatim from `handle_connection`'s match arms (behavior-preserving).

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::super::{
    DASHBOARD_HTML, DashboardTabInput, TabSnapshot, build_dashboard_state, read_activity_json, respond_json,
    respond_with_etag, tab_activity,
};

/// `GET /dashboard` — the dashboard app HTML page (behind the auth gate; its
/// static assets stay public).
pub(in crate::api) fn page<S: Write>(stream: &mut S, accept_gzip: bool, if_none_match: Option<&str>) {
    respond_with_etag(
        stream,
        200,
        "text/html; charset=utf-8",
        DASHBOARD_HTML.as_bytes(),
        accept_gzip,
        if_none_match,
        "Cache-Control: no-cache\r\n",
    );
}

/// `GET /dashboard/share-token` — return (minting on first use) the global
/// dashboard share-token. Master only (not in the dashboard-token allowlist).
pub(in crate::api) fn share_token<S: Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>) {
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if snap.dashboard_share_token.is_empty() {
        snap.dashboard_share_token = crate::mint_share_token().into();
    }
    let token = snap.dashboard_share_token.to_string();
    drop(snap);
    respond_json(stream, 200, &format!(r#"{{"token":"{token}"}}"#));
}

/// `GET /dashboard/activity` — thin passthrough of the scribe's `activity.json`
/// (verbatim when present, gracefully empty when absent/malformed).
pub(in crate::api) fn activity<S: Write>(stream: &mut S) {
    let body = read_activity_json(&crate::platform::state_base_dir());
    respond_json(stream, 200, &body);
}

/// `GET /dashboard/state` — the aggregated, phase-grouped fleet view.
pub(in crate::api) fn state<S: Write>(
    stream: &mut S,
    state: &Arc<Mutex<TabSnapshot>>,
    accept_gzip: bool,
    if_none_match: Option<&str>,
) {
    let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let inputs: Vec<DashboardTabInput> = snap
        .tabs
        .iter()
        .map(|t| DashboardTabInput {
            id: t.id.to_string(),
            name: t.name.to_string(),
            cwd: t.cwd.as_deref().map(str::to_string),
            assignment: t.assignment.as_deref().map(str::to_string),
            context: t.context.as_deref().map(str::to_string),
            parent_tab_id: t.parent_tab_id.as_deref().map(str::to_string),
            rehome_status: t.rehome_status.as_deref().map(str::to_string),
            agent_state: t.agent_state.as_ref().map(|s| match s.state {
                crate::AgentState::Thinking => "thinking",
                crate::AgentState::Waiting => "waiting",
                crate::AgentState::Error => "error",
            }),
            led: t.agent_led.map(crate::TabLed::slug),
            tokens: t.tokens,
            activity: tab_activity(t.shell_pid),
            specialty: t.specialty.as_deref().map(str::to_string),
            orchestrator: t.orchestrator.as_deref().map(str::to_string),
            objective: t.objective.as_deref().map(str::to_string),
            current_task: t.current_task.clone(),
            rounds_active: t.rounds_active.clone(),
            evaluations: t.evaluations.clone(),
            usage_count: t.usage_count,
            last_used_at: t.last_used_at,
            conventions: t.conventions.clone(),
            context_pct: t.context_pct,
            recently_compacted: crate::cli::clarify::recently_compacted(t.last_compaction_at, crate::unix_millis()),
        })
        .collect();
    drop(snap);
    let mut dashboard = build_dashboard_state(inputs);
    // S4 task read-model: derive every queue's current state READ-ONLY (FS read
    // here so the pure builder stays FS-free). Nothing is mutated / compacted.
    dashboard.tasks = crate::cli::task::read_all_queue_views(crate::unix_millis());
    // RB2 retired read-model: a SEPARATE SOURCE read from catalog.jsonl (a retired
    // agent is absent from the live snapshot), folded latest-per-slug. READ-ONLY.
    dashboard.retired = crate::cli::catalog::read_retired();
    // SV3 v2 skill read-model: same catalogue, folded BY SKILL NAME (v1 quarantined),
    // metrics partitioned by mode + fresh-vs-resume DERIVED at read. READ-ONLY.
    dashboard.skills = crate::cli::catalog::read_skill_profiles();
    let body = serde_json::to_string_pretty(&dashboard).unwrap_or_default();
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
