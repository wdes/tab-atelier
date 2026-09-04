// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `/dashboard[/state|/activity|/share-token]` route handlers. Bodies moved
//! verbatim from `handle_connection`'s match arms (behavior-preserving).

use std::io::Write;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use super::super::{
    DASHBOARD_HTML, DashboardTabInput, TabSnapshot, build_dashboard_state, etag_for, read_activity_json,
    respond_json, respond_with_etag, respond_with_etag_precomputed, tab_activity,
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

/// S1.5 response-cache entry: the serialized `/dashboard/state` body + its etag
/// + when it was built.
struct CachedState {
    body: String,
    etag: String,
    at: Instant,
}

/// Coalescing response-cache for `/dashboard/state` (see [`state`]).
static STATE_CACHE: LazyLock<Mutex<Option<CachedState>>> = LazyLock::new(|| Mutex::new(None));

/// TTL of the response-cache. `KALPIN_STATE_CACHE_TTL_MS` overrides it (read once);
/// `0` disables the cache (always rebuild). Default 1 s, kept ≤ the client poll
/// period so a viewer never sees more than about one poll of staleness.
static STATE_CACHE_TTL: LazyLock<Duration> = LazyLock::new(|| {
    // Tests spin several daemons in ONE process but share this process-global
    // cache → a stale body would leak across daemons. Disable it under test so
    // each rebuild reflects that daemon's own state (isolation); production (one
    // daemon) keeps the cache. The cache's perf behaviour is covered by the busy
    // multi-viewer smoke, not these content unit tests.
    if cfg!(test) {
        return Duration::ZERO;
    }
    std::env::var("KALPIN_STATE_CACHE_TTL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::from_secs(1), Duration::from_millis)
});

/// Rebuild the full `/dashboard/state` body: snapshot clone + derived read-models
/// (org-chart/bands/projects) + the three READ-ONLY FS read-models (tasks, retired,
/// skills) + pretty JSON. This is the expensive path the response-cache coalesces.
fn build_state_body(state: &Arc<Mutex<TabSnapshot>>) -> String {
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
    serde_json::to_string_pretty(&dashboard).unwrap_or_default()
}

/// `GET /dashboard/state` — the aggregated, phase-grouped fleet view. Served from
/// a short-TTL response-cache: within [`STATE_CACHE_TTL`] every viewer shares ONE
/// rebuild (coalescing), so a busy fleet + N viewers no longer amplifies the
/// rebuild N× (the crisis's root). The recompute runs under the cache lock — a
/// burst of viewers at expiry coalesces onto one rebuild, no thundering herd — and
/// lock order is always cache → snapshot. Invalidation is purely temporal, so
/// there is no version to track and nothing stale beyond the TTL. The client still
/// gets a `304` on an unchanged body via its `If-None-Match` (S1a).
pub(in crate::api) fn state<S: Write>(
    stream: &mut S,
    state: &Arc<Mutex<TabSnapshot>>,
    accept_gzip: bool,
    if_none_match: Option<&str>,
) {
    let (body, etag) = {
        let mut cache = STATE_CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        // Clone out of a fresh entry (borrow ends here), else rebuild + store.
        let hit = cache
            .as_ref()
            .filter(|c| now.duration_since(c.at) < *STATE_CACHE_TTL)
            .map(|c| (c.body.clone(), c.etag.clone()));
        if let Some(pair) = hit {
            pair
        } else {
            let body = build_state_body(state);
            let etag = etag_for(body.as_bytes());
            *cache = Some(CachedState { body: body.clone(), etag: etag.clone(), at: now });
            (body, etag)
        }
    };
    respond_with_etag_precomputed(
        stream,
        200,
        "application/json",
        body.as_bytes(),
        accept_gzip,
        if_none_match,
        "",
        Some(etag),
    );
}
