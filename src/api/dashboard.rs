// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The harness dashboard: the aggregated, phase-grouped fleet view
//! (`GET /dashboard/state`), the activity passthrough (`GET
//! /dashboard/activity`), the read-only share-token mint (`GET
//! /dashboard/share-token`), and the dashboard app page (`GET /dashboard`).
//!
//! The aggregation reads ONLY the agent-card fields already on the snapshot
//! (`assignment` → phase/role/project, `specialty`/`orchestrator`/… + tokens).
//! It reuses the base's `parse_assignment` (via `super`) rather than a private
//! copy. Ponytail: the per-tab transcript-derived `activity` (current-task +
//! `Task()` sub-agents) is DEFERRED to its own PR — the base `catbus_agent`
//! transcript shapes lack the `id`/`tool_use_id` blocks the distiller needs, so
//! populating it means either an out-of-perimeter change there or a divergent
//! duplicate. The `TabActivity` shape stays (so `currentTask`/`subAgents` remain
//! in the JSON contract, empty) and lands populated when the scribe PR does.
//! Likewise `tasks`/`retired`/`skills` read-models (task/catalog primitives) are
//! omitted here until those branches land — the web's catalogue panel reads its
//! own cold `/catalog/list`, never `/dashboard/state`.

use std::io::Write;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use super::{TabSnapshot, parse_assignment, respond_json, respond_with_etag};

const DASHBOARD_HTML: &str = include_str!("../../assets/dashboard.html");

/// The seven canonical phase-node ids of the harness dashboard skeleton, in
/// flow order. A tab whose `assignment` phase is one of these maps to that node;
/// anything else falls to `unmapped`.
const DASHBOARD_PHASES: [&str; 7] = ["scope", "plan", "build", "review", "verify", "sweep", "done"];

/// Roles that mark an itinerant meta-specialist: with no repo cwd and no project
/// override, such a tab lands in the shared **`méta`** lane rather than `divers`.
const META_ROLES: [&str; 4] = ["planner", "auditor", "tichef", "orchestrator"];

/// Dev work-roots whose basename is NOT a project (a shell parked at the parent
/// of the repos). Ponytail: heuristic list, no git detection — a tab actually
/// inside `~/Dev/kalpin-back` still maps to `kalpin-back`; upgrade = walk to the
/// enclosing `.git`.
const WORK_ROOT_NAMES: [&str; 6] = ["dev", "src", "code", "projects", "repos", "workspace"];

const META_LANE: &str = "méta";
const DIVERS_LANE: &str = "divers";

// --------------------------------------------------------------------------
// Serializable view types (mirror the web contract in assets/dashboard.js).
// --------------------------------------------------------------------------

/// What a tab is doing now. `current_task` is the latest human-typed prompt;
/// `sub_agents` is every `Task(...)` it spawned. Flattened onto [`DashboardTab`]
/// as `currentTask` / `subAgents`. Empty until the activity-scribe PR populates
/// it (see the module note).
#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct TabActivity {
    pub(crate) current_task: Option<String>,
    pub(crate) sub_agents: Vec<SubAgent>,
}

/// One `Task()` sub-agent invocation: its `subagent_type` and lifecycle state.
#[derive(Serialize, Clone)]
pub struct SubAgent {
    pub(crate) name: String,
    /// `"running"` until a matching `tool_result` comes back, then `"completed"`.
    pub(crate) state: String,
}

/// One tab as the dashboard renders it: identity, the derived
/// project/phase/role, its led + tokens, and the self-declared agent card.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DashboardTab {
    pub(crate) id: String,
    pub(crate) name: String,
    /// The volatile "5 words" (the current prompt); kept for the subtitle.
    pub(crate) context: Option<String>,
    /// Raw `"[<project>:]<phase>/<role>"` the agent set once. `None` ⇒ unassigned.
    pub(crate) assignment: Option<String>,
    /// The team this tab is SERVING = the assignment's `<project>:` override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) serving: Option<String>,
    /// Agent role, derived from `assignment` (never from the volatile context).
    pub(crate) role: String,
    /// The current unit of work — the volatile `context` (the prompt).
    pub(crate) item: String,
    /// UUID of the spawning tab, for the delegation lineage. `None` ⇒ root.
    pub(crate) parent_tab_id: Option<String>,
    /// Re-home progress on a predecessor tab. `None` ⇒ not rehoming.
    pub(crate) rehome_status: Option<String>,
    /// Static altitude band from the role class: 0 tichef, 1 orchestrator,
    /// 2 worker/specialist.
    pub(crate) altitude: u8,
    pub(crate) agent_state: Option<&'static str>,
    pub(crate) led: Option<&'static str>,
    pub(crate) tokens: Option<crate::TokenUsage>,
    pub(crate) viewer_url: String,
    /// Hard-wired specialty / prompt focus. Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) specialty: Option<String>,
    /// Orchestrator served (tab UUID or the literal `"free"`). Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) orchestrator: Option<String>,
    /// Current objective. Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) objective: Option<String>,
    /// The self-declared `current_task` PERMALOG, exposed as `currentTaskLog` —
    /// a DISTINCT key from the transcript-derived `currentTask`. Omitted empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) current_task_log: Vec<String>,
    /// Supervision-rounds status (`roundsActive`). Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) rounds_active: Option<crate::RoundsActive>,
    /// The bounded evaluations ring. Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) evaluations: Vec<crate::Evaluation>,
    /// Generic use counter (`usageCount`). Omitted when never used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) usage_count: Option<u64>,
    /// Unix-millis of last use (`lastUsedAt`). Omitted when never set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_used_at: Option<u64>,
    /// DECLARED conventions (`.md` list). Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) conventions: Vec<String>,
    /// Current task + `Task()` sub-agents, flattened → `currentTask` / `subAgents`.
    #[serde(flatten)]
    pub(crate) activity: TabActivity,
}

/// A phase node with its occupants and the worst-severity led among them.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DashboardNode {
    pub(crate) id: &'static str,
    pub(crate) rollup_led: Option<&'static str>,
    pub(crate) tabs: Vec<DashboardTab>,
}

/// An orchestrator working in a project: named, with its current `item` and a
/// GLOBAL `child_count` (tabs whose `parent_tab_id` is this orchestrator).
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratorRef {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) item: String,
    pub(crate) child_count: usize,
}

/// A project bucket: the 7-phase subtree scoped to one project, plus its rollup.
/// `méta` and `divers` are the two shared lanes.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardProject {
    pub(crate) name: String,
    pub(crate) tab_count: usize,
    pub(crate) rollup_led: Option<&'static str>,
    pub(crate) has_orchestrator: bool,
    pub(crate) is_meta: bool,
    pub(crate) orchestrators: Vec<OrchestratorRef>,
    pub(crate) nodes: Vec<DashboardNode>,
    pub(crate) unmapped: Vec<DashboardTab>,
}

/// A service = a family of repos: a shared prefix (≥2 repos) forms a named
/// service; a lone repo stays a "mono" service named after itself. Wraps the
/// flat `projects`, non-breaking.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardService {
    pub(crate) name: String,
    /// Worst led among the service's sub-repos.
    pub(crate) rollup_led: Option<&'static str>,
    /// The member repo names, sorted.
    pub(crate) projects: Vec<String>,
}

/// One delegation edge: `child` was spawned by `parent` (both tab UUIDs).
#[derive(Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LineageEdge {
    pub(crate) child: String,
    pub(crate) parent: String,
}

/// The aggregated fleet view served by `GET /dashboard/state`.
#[derive(Serialize)]
pub struct DashboardState {
    /// Global 7-node diagram.
    pub(crate) nodes: Vec<DashboardNode>,
    pub(crate) unmapped: Vec<DashboardTab>,
    /// Per-project buckets, sorted alpha with `méta`/`divers` last.
    pub(crate) projects: Vec<DashboardProject>,
    /// Services: the flat `projects` grouped into repo families. Non-breaking.
    pub(crate) services: Vec<DashboardService>,
    /// Delegation lineage: `parent_tab_id` edges whose parent is a known tab.
    pub(crate) lineage: Vec<LineageEdge>,
    /// Tabs with NO `assignment` at all — legitimately un-placed, sorted by id.
    pub(crate) unassigned: Vec<DashboardTab>,
}

/// The minimal per-tab projection the builder consumes, so the mapping/rollup
/// logic is unit-testable without a full `SnapshotTab`. `assignment` drives
/// phase/role/project; `cwd` drives project; `context` is the volatile subtitle.
pub struct DashboardTabInput {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) cwd: Option<String>,
    pub(crate) assignment: Option<String>,
    pub(crate) context: Option<String>,
    pub(crate) parent_tab_id: Option<String>,
    pub(crate) rehome_status: Option<String>,
    pub(crate) agent_state: Option<&'static str>,
    pub(crate) led: Option<&'static str>,
    pub(crate) tokens: Option<crate::TokenUsage>,
    pub(crate) specialty: Option<String>,
    pub(crate) orchestrator: Option<String>,
    pub(crate) objective: Option<String>,
    pub(crate) current_task: Vec<String>,
    pub(crate) rounds_active: Option<crate::RoundsActive>,
    pub(crate) evaluations: Vec<crate::Evaluation>,
    pub(crate) usage_count: Option<u64>,
    pub(crate) last_used_at: Option<u64>,
    pub(crate) conventions: Vec<String>,
}

// --------------------------------------------------------------------------
// Pure mapping/rollup helpers (unit-testable, FS-free).
// --------------------------------------------------------------------------

/// Rollup severity of a `led` slug — higher is worse. Mirrors [`crate::TabLed`]
/// precedence: dead > error > working > unreviewed > idle. Unknown ⇒ 0.
fn led_severity(led: &str) -> u8 {
    match led {
        "dead" => 5,
        "error" => 4,
        "working" => 3,
        "unreviewed" => 2,
        "idle" => 1,
        _ => 0,
    }
}

/// Static altitude band from an agent role: 0 = tichef (top), 1 = orchestrator,
/// 2 = worker/specialist (bottom).
fn role_altitude(role: &str) -> u8 {
    match role {
        "tichef" => 0,
        "orchestrator" => 1,
        _ => 2,
    }
}

/// Resolve a tab's project, in order: (1) `<project>:` override; (2) basename of
/// a repo cwd; (3) `méta` lane for a meta-role itinerant; (4) `divers`.
fn project_of(cwd: Option<&str>, assignment: Option<&str>) -> String {
    let (over, _phase, role) = assignment.map_or((None, String::new(), String::new()), parse_assignment);
    if let Some(p) = over {
        return p;
    }
    if let Some(c) = cwd {
        let base = c.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        if !base.is_empty() && !WORK_ROOT_NAMES.contains(&base.to_ascii_lowercase().as_str()) {
            return base.to_string();
        }
    }
    if META_ROLES.contains(&role.as_str()) {
        META_LANE.to_string()
    } else {
        DIVERS_LANE.to_string()
    }
}

/// Resolve a repo to its service key: the prefix before the first `-`, else the
/// repo's own name (mono, no `-`). Pure. Ponytail: no explicit `repo_families`
/// map (the daemon never threaded one — the builder always passed defaults); a
/// prefix heuristic covers the real repo naming (`kalpin-*`).
fn service_of(project: &str) -> String {
    match project.split_once('-') {
        Some((prefix, _)) if !prefix.is_empty() => prefix.to_string(),
        _ => project.to_string(),
    }
}

/// Group projects into services. A prefix shared by ≥2 repos (or a repo named
/// after its own key) forms a named service; a lone prefix-heuristic repo
/// collapses back to a "mono" service under its full name. Rollup = worst led of
/// the members; services sorted by name. Pure.
fn group_services(projects: &[DashboardProject]) -> Vec<DashboardService> {
    let mut by_key: std::collections::BTreeMap<String, Vec<&DashboardProject>> = std::collections::BTreeMap::new();
    for p in projects {
        by_key.entry(service_of(&p.name)).or_default().push(p);
    }
    let mut services: Vec<DashboardService> = by_key
        .into_iter()
        .map(|(key, members)| {
            let keep_named = members.len() >= 2 || members.iter().any(|p| p.name == key);
            let name = if keep_named { key } else { members[0].name.clone() };
            let rollup_led = members
                .iter()
                .filter_map(|p| p.rollup_led)
                .max_by_key(|led| led_severity(led));
            let mut names: Vec<String> = members.iter().map(|p| p.name.clone()).collect();
            names.sort();
            DashboardService {
                name,
                rollup_led,
                projects: names,
            }
        })
        .collect();
    services.sort_by(|a, b| a.name.cmp(&b.name));
    services
}

/// Build the delegation lineage from `(child_id, parent_id)` pairs. An edge is
/// kept only when the parent is a known tab and isn't the child itself, so an
/// unknown parent degrades to a root and no self-cycle survives. Deduped.
fn build_lineage(tabs: &[(String, Option<String>)]) -> Vec<LineageEdge> {
    let ids: std::collections::HashSet<&str> = tabs.iter().map(|(id, _)| id.as_str()).collect();
    let mut seen = std::collections::HashSet::new();
    let mut edges = vec![];
    for (child, parent) in tabs {
        if let Some(p) = parent
            && p != child
            && ids.contains(p.as_str())
            && seen.insert((child.clone(), p.clone()))
        {
            edges.push(LineageEdge {
                child: child.clone(),
                parent: p.clone(),
            });
        }
    }
    edges
}

/// Group `(phase, tab)` pairs into the 7 canonical nodes + an unmapped bucket,
/// rolling up each node's led to its worst occupant. Shared by the global
/// diagram and each project subtree.
fn group_into_nodes<I: Iterator<Item = (String, DashboardTab)>>(items: I) -> (Vec<DashboardNode>, Vec<DashboardTab>) {
    let mut nodes: Vec<DashboardNode> = DASHBOARD_PHASES
        .iter()
        .map(|id| DashboardNode {
            id,
            rollup_led: None,
            tabs: vec![],
        })
        .collect();
    let mut unmapped: Vec<DashboardTab> = vec![];
    for (phase, tab) in items {
        match nodes.iter_mut().find(|n| n.id == phase) {
            Some(n) => n.tabs.push(tab),
            None => unmapped.push(tab),
        }
    }
    for n in &mut nodes {
        n.rollup_led = n.tabs.iter().filter_map(|t| t.led).max_by_key(|led| led_severity(led));
    }
    (nodes, unmapped)
}

/// Map tabs onto phase nodes **via `assignment`**, group them under projects
/// (via cwd/override), roll up leds, and derive lineage + services. The pure
/// core of `GET /dashboard/state`.
#[must_use]
pub fn build_dashboard_state(inputs: Vec<DashboardTabInput>) -> DashboardState {
    struct Projected {
        project: String,
        phase: String,
        tab: DashboardTab,
    }
    let projected: Vec<Projected> = inputs
        .into_iter()
        .map(|t| {
            let (serving, phase, role) = t
                .assignment
                .as_deref()
                .map_or((None, String::new(), String::new()), parse_assignment);
            let project = project_of(t.cwd.as_deref(), t.assignment.as_deref());
            let viewer_url = format!("/tabs/by-id/{}/view", t.id);
            let altitude = role_altitude(&role);
            let tab = DashboardTab {
                id: t.id,
                name: t.name,
                item: t.context.clone().unwrap_or_default(),
                context: t.context,
                assignment: t.assignment,
                serving,
                role,
                parent_tab_id: t.parent_tab_id,
                rehome_status: t.rehome_status,
                altitude,
                agent_state: t.agent_state,
                led: t.led,
                tokens: t.tokens,
                viewer_url,
                specialty: t.specialty,
                orchestrator: t.orchestrator,
                objective: t.objective,
                current_task_log: t.current_task,
                rounds_active: t.rounds_active,
                evaluations: t.evaluations,
                usage_count: t.usage_count,
                last_used_at: t.last_used_at,
                conventions: t.conventions,
                // Deferred: populated by the activity-scribe PR (see module note).
                activity: TabActivity::default(),
            };
            Projected { project, phase, tab }
        })
        .collect();

    // Global diagram.
    let (nodes, unmapped) = group_into_nodes(projected.iter().map(|p| (p.phase.clone(), p.tab.clone())));

    // GLOBAL child count per tab id (how many tabs it spawned, anywhere).
    let mut child_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for p in &projected {
        if let Some(parent) = &p.tab.parent_tab_id {
            *child_counts.entry(parent.clone()).or_default() += 1;
        }
    }

    // Top-level `unassigned`: tabs with no assignment at all, sorted by id.
    let mut unassigned: Vec<DashboardTab> = projected
        .iter()
        .filter(|p| p.tab.assignment.is_none())
        .map(|p| p.tab.clone())
        .collect();
    unassigned.sort_by(|a, b| a.id.cmp(&b.id));

    // Distinct projects, sorted alpha with méta/divers pinned last.
    let mut names: Vec<String> = projected.iter().map(|p| p.project.clone()).collect();
    names.sort();
    names.dedup();
    let rank = |n: &str| match n {
        META_LANE => 1,
        DIVERS_LANE => 2,
        _ => 0,
    };
    names.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));

    let projects: Vec<DashboardProject> = names
        .into_iter()
        .map(|name| {
            let mine: Vec<&Projected> = projected.iter().filter(|p| p.project == name).collect();
            let tab_count = mine.len();
            let has_orchestrator = mine.iter().any(|p| p.tab.role == "orchestrator");
            let rollup_led = mine
                .iter()
                .filter_map(|p| p.tab.led)
                .max_by_key(|led| led_severity(led));
            let mut orchestrators: Vec<OrchestratorRef> = mine
                .iter()
                .filter(|p| p.tab.role == "orchestrator")
                .map(|p| OrchestratorRef {
                    id: p.tab.id.clone(),
                    name: p.tab.name.clone(),
                    item: p.tab.item.clone(),
                    child_count: child_counts.get(&p.tab.id).copied().unwrap_or(0),
                })
                .collect();
            orchestrators.sort_by(|a, b| a.id.cmp(&b.id));
            let (nodes, unmapped) = group_into_nodes(mine.iter().map(|p| (p.phase.clone(), p.tab.clone())));
            let is_meta = name == META_LANE;
            DashboardProject {
                name,
                tab_count,
                rollup_led,
                has_orchestrator,
                is_meta,
                orchestrators,
                nodes,
                unmapped,
            }
        })
        .collect();

    let lineage = build_lineage(
        &projected
            .iter()
            .map(|p| (p.tab.id.clone(), p.tab.parent_tab_id.clone()))
            .collect::<Vec<_>>(),
    );

    let services = group_services(&projects);

    DashboardState {
        nodes,
        unmapped,
        projects,
        services,
        lineage,
        unassigned,
    }
}

/// Thin passthrough of the activity scribe's `activity.json` (under the state
/// dir). Returns the file VERBATIM when present + parseable; degrades to a
/// graceful empty JSON object `{}` when absent or malformed — the panel reads
/// valid JSON either way, never a 404/500. `base` is the state-base dir, so it's
/// testable on a tempdir without touching XDG.
#[must_use]
pub fn read_activity_json(base: &std::path::Path) -> String {
    let path = crate::state_dir(base).join("activity.json");
    match std::fs::read_to_string(&path) {
        Ok(s) if serde_json::from_str::<serde_json::Value>(&s).is_ok() => s,
        _ => "{}".to_string(),
    }
}

// --------------------------------------------------------------------------
// Route handlers.
// --------------------------------------------------------------------------

/// `GET /dashboard` — the dashboard app HTML page (behind the auth gate; its
/// static assets `/assets/dashboard.{js,css}` stay public).
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
        snap.dashboard_share_token = crate::mint_share_token();
    }
    let token = snap.dashboard_share_token.clone();
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
            specialty: t.specialty.as_deref().map(str::to_string),
            orchestrator: t.orchestrator.as_deref().map(str::to_string),
            objective: t.objective.as_deref().map(str::to_string),
            current_task: t.current_task.clone(),
            rounds_active: t.rounds_active.clone(),
            evaluations: t.evaluations.clone(),
            usage_count: t.usage_count,
            last_used_at: t.last_used_at,
            conventions: t.conventions.clone(),
        })
        .collect();
    drop(snap);
    let dashboard = build_dashboard_state(inputs);
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
