// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Serialization DTOs for the local API (the ~26 response/request structs +
//! enums the routes serialize/deserialize) + their derives. Relocated VERBATIM
//! from `api/mod.rs` (Slice 4, pure move); the only adaptation is a
//! `pub(crate)` visibility widening on the moved structs + their private fields
//! (same class as the `pub(in crate::api)` on the split fns) so cross-module
//! constructors (handlers, persist) keep building them. No logic changed.

// The `pub(crate)` widening is deliberate: it feeds the `pub(crate) use types::*`
// re-export in the parent so `api::X` paths resolve. clippy sees it as redundant
// (the module itself is private) but the re-export is what makes it meaningful.
#![allow(clippy::redundant_pub_crate)]
#[allow(clippy::wildcard_imports)]
use super::*;

#[derive(Serialize)]
pub struct TabInfo {
    pub(crate) index: usize,
    /// Stable per-tab UUID. Exposed so any client polling /tabs can
    /// correlate the row with `_TAB_ID` shells / set-status calls /
    /// auto-resume state.
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) cwd: Option<String>,
    pub(crate) active: bool,
    /// Effective lock state — true if either the user toggled the
    /// padlock OR the schedule's current window is closed. Mirrors
    /// `LockState::effective_locked`; CLI listers should source
    /// from this field, not from the raw `locked` bit which only
    /// reflects the manual toggle.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) locked: bool,
    /// "manual" / "schedule" / null. Only populated when locked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) lock_reason: Option<&'static str>,
    /// OSM `opening_hours` rule on the tab, if a schedule is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) schedule_rule: Option<String>,
    /// IANA timezone of the schedule rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) schedule_tz: Option<String>,
    /// Last non-empty line of the cached output buffer — used by remote clients
    /// to preview what's happening without fetching the full output.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) preview: String,
    /// Cumulative time the tab has spent in the "active" state on the
    /// desktop. Lets the mobile remote show the same per-tab counter
    /// without needing its own activity tracker.
    pub(crate) uptime_secs: f64,
    #[cfg(feature = "energy")]
    pub(crate) cpu_percent: f64,
    #[cfg(feature = "energy")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) watts: Option<f64>,
    /// Transient agent indicator state ("thinking" / "waiting" /
    /// "error"). Omitted when no agent is attached, so existing
    /// consumers don't see a new field unless they look.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent_state: Option<&'static str>,
    /// Durable agent kind ("catbus" / "claude" / …) when a session
    /// is attached, even if no transient state is current.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent_kind: Option<String>,
    /// Fully-derived per-tab LED, matching the desktop tab-strip dot:
    /// `"dead"` (dim red) / `"error"` / `"working"` (green) / `"unreviewed"`
    /// (blue) / `"idle"` (grey). Computed server-side by
    /// [`crate::compute_tab_led`] so the mobile remote and CLI viewer render
    /// the identical indicator. Omitted when no dot should show.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) led: Option<&'static str>,
    /// Unix-millis of the last time this tab was used (input / activate /
    /// viewer open). Clients sort the list by this descending to show the
    /// most-recently-used tabs first. Omitted for never-used tabs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_used_at: Option<u64>,
    /// Durable agent session UUID — set by `set-status --session
    /// <id>` from inside the agent's PTY. The brain uses this to
    /// confirm a Claude (or other agent) is actually mid-task before
    /// auto-injecting `continue`; a tab whose `agent_kind` happens to
    /// be `claude` but with no live session attached is not a brain
    /// target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent_session_id: Option<String>,
    /// Free-text context the in-tab agent set via `set-context` — the
    /// PR/task it's on. Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) context: Option<String>,
    /// Stable workflow assignment (`set-assignment`, `"[<project>:]<phase>/
    /// <role>"`). Persisted + hook-immune, unlike `context`. Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) assignment: Option<String>,
    /// UUID of the spawning tab (`parent_tab_id`) — the dashboard lineage edge.
    /// Omitted for a root (non-spawned) tab.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_tab_id: Option<String>,
    /// Re-home progress on a predecessor tab (`handoff-written` → `successor-ready`
    /// → `ack-sent` → `safe-to-close`). Omitted when not rehoming.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) rehome_status: Option<String>,
    /// Number of WS viewers (browser share-link / `remote attach`)
    /// currently watching this tab. Omitted when zero.
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) viewers: usize,
    /// Whether the tab has no internet (its shell runs inside a
    /// bubblewrap network-isolated sandbox). Omitted when false so
    /// existing consumers don't see a new field unless net is off.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) net_disabled: bool,
    /// Active outbound connections (metering). Omitted when zero.
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) connections: usize,
    /// Egress bytes a confined (allowlist) tab tried to send. Omitted when 0.
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub(crate) tx_bytes: u64,
    /// Of those, bytes the allowlist dropped. Omitted when 0.
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub(crate) tx_denied_bytes: u64,
    /// Current allowlist (when in allowlist mode). Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) net_allow_presets: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) net_allow_domains: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) net_allow_cidrs: Vec<String>,
    /// Per-tab resolver DNS log (domain-allowlist tabs). Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) dns: Vec<DnsEntryInfo>,
    /// Resident memory (bytes) of the tab's process subtree. Omitted until
    /// the first `/proc` sample lands (or when the walk fails).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) resident_memory_bytes: Option<u64>,
    /// Cumulative agent token usage (`{input, output}`). Omitted for
    /// non-agent tabs so existing consumers don't see a new field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tokens: Option<crate::TokenUsage>,
    // --- Inc9 brick 1: the agent CARD on `tabs --json` (same values + same
    //     camelCase keys as /dashboard/state) so an agent/tool can reread ITS OWN
    //     card without the aggregated state. Sourced from TabState via SnapshotTab.
    //     Omitted when empty/None (a card-less tab stays clean). `lastUsedAt` is
    //     already exposed above as `last_used_at`; `evalCriteria` doesn't exist yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) specialty: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) orchestrator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) objective: Option<String>,
    #[serde(rename = "currentTaskLog", skip_serializing_if = "Vec::is_empty")]
    pub(crate) current_task_log: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) conventions: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) evaluations: Vec<crate::Evaluation>,
    #[serde(rename = "roundsActive", skip_serializing_if = "Option::is_none")]
    pub(crate) rounds_active: Option<crate::RoundsActive>,
    #[serde(rename = "usageCount", skip_serializing_if = "Option::is_none")]
    pub(crate) usage_count: Option<u64>,
    /// Inc9 hot-swap cross-guard (#23): true while a binary hot-swap handoff is in
    /// progress ([`crate::hotswap::frozen`]). External daemons (brain nudges,
    /// clarify auto-rehome) MUST leave a tab alone while this is set — nudging /
    /// re-homing a tab whose PTY is mid-adoption would race the handoff and could
    /// double-launch its agent. Omitted (false) in the normal case. Carried into
    /// the moved DTO from this fork's fabric+#23 context.
    #[serde(rename = "inHandoff", skip_serializing_if = "std::ops::Not::not")]
    pub(crate) in_handoff: bool,
    /// Inc9 b2 — context-window % used (0-100). Omitted when no marker on screen.
    /// `SNAKE_CASE` on the wire (`context_pct`) — the b2/b3 web JS reads it snake,
    /// like `agent_kind`/`parent_tab_id`; a camelCase rename would read `undefined`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) context_pct: Option<u8>,
    /// Inc9 b3 — a compaction/rehome (brutal context-% drop) landed recently.
    /// `SNAKE_CASE` (`recently_compacted`). Omitted (false) in the common case.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) recently_compacted: bool,
}

/// One DNS-entries-view row for the `/tabs` response.
#[derive(Serialize)]
pub struct DnsEntryInfo {
    pub(crate) domain: String,
    pub(crate) allowed: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) ips: Vec<String>,
}

/// Host-wide stats reported alongside the per-tab list. Keeps the
/// mobile remote from having to guess these values (it used to read
/// the *phone's* own battery, which made no sense — the user wants
/// the workstation's stats).
#[derive(Serialize, Default)]
pub struct HostInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) battery_percent: Option<u8>,
    /// Total instantaneous power draw across every tab's tracked
    /// processes, in watts. Omitted when RAPL is unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) watts: Option<f64>,
}

#[derive(Serialize)]
pub struct ApiResponse {
    pub(crate) app: &'static str,
    pub(crate) host: HostInfo,
    pub(crate) tabs: Vec<TabInfo>,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub(crate) error: String,
}

/// What a tab is doing now, distilled from its transcript for the dashboard.
///
/// `current_task` is the latest human-typed prompt; `sub_agents` is every
/// `Task(...)` it spawned. Flattened onto [`DashboardTab`] as `currentTask` /
/// `subAgents` for the web `taskChips`.
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

// Raw transcript shapes — mirror the scribe's private copies; same gate as
// `parse_tab_activity`, their only consumer.
#[cfg(any(feature = "catbus", test))]
#[derive(serde::Deserialize)]
pub(crate) struct RawLine {
    pub(crate) r#type: String,
    pub(crate) message: Option<RawMessage>,
    /// `"typed"` on a user line the human typed (vs a `tool_result` / reminder).
    #[serde(rename = "promptSource")]
    pub(crate) prompt_source: Option<String>,
}

#[cfg(any(feature = "catbus", test))]
#[derive(serde::Deserialize)]
pub(crate) struct RawMessage {
    pub(crate) content: RawContent,
}

#[cfg(any(feature = "catbus", test))]
#[derive(serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum RawContent {
    String(String),
    Blocks(Vec<RawBlock>),
}

#[cfg(any(feature = "catbus", test))]
#[derive(serde::Deserialize)]
pub(crate) struct RawBlock {
    pub(crate) r#type: String,
    pub(crate) name: Option<String>,
    pub(crate) input: Option<serde_json::Value>,
    /// `tool_use` block id ↔ `tool_result` back-reference — paired to flip a
    /// `Task()` sub-agent from "running" to "completed".
    pub(crate) id: Option<String>,
    pub(crate) tool_use_id: Option<String>,
}

/// One tab projected into the dashboard state: the same per-tab data as
/// `/tabs/usage`, plus `role` (from `assignment`), the `context` subtitle, the
/// raw `assignment`, and a ready-made `viewerUrl`. camelCase.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DashboardTab {
    pub(crate) id: String,
    pub(crate) name: String,
    /// The volatile "5 words" (the current prompt); kept for the S4 subtitle.
    pub(crate) context: Option<String>,
    /// Raw `"[<project>:]<phase>/<role>"` the agent set once. `None` ⇒ unassigned.
    pub(crate) assignment: Option<String>,
    /// The team this tab is SERVING = the assignment's `<project>:` override
    /// (S1). A méta with an override is busy serving that team (not available);
    /// `None` ⇒ no override (a plain méta / a repo-cwd tab). Skipped when None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) serving: Option<String>,
    /// Agent role, derived from `assignment` (never from the volatile context).
    pub(crate) role: String,
    /// The current unit of work — now the volatile `context` (the prompt).
    pub(crate) item: String,
    /// UUID of the spawning tab, for the delegation lineage. `None` ⇒ root.
    pub(crate) parent_tab_id: Option<String>,
    /// Re-home progress on a predecessor tab (annotates the old→new drill-in
    /// link with readiness/ACK). `None` ⇒ not rehoming.
    pub(crate) rehome_status: Option<String>,
    /// Static altitude band from the role class: 0 tichef, 1 orchestrator,
    /// 2 worker/specialist. A socle available without lineage data.
    pub(crate) altitude: u8,
    pub(crate) agent_state: Option<&'static str>,
    pub(crate) led: Option<&'static str>,
    pub(crate) tokens: Option<crate::TokenUsage>,
    pub(crate) viewer_url: String,
    // --- Inc8 S1: the self-declared agent card (observable by peers + the web).
    /// Hard-wired specialty / prompt focus. Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) specialty: Option<String>,
    /// Orchestrator served (tab UUID or the literal `"free"`). Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) orchestrator: Option<String>,
    /// Current objective. Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) objective: Option<String>,
    /// The self-declared `current_task` PERMALOG (bounded ring). Exposed as
    /// `currentTaskLog` — a DISTINCT key from Inc7 S4's transcript-derived
    /// `currentTask` (observed) so the two don't collide; the declared↔observed
    /// reconciliation into one field is a later PO decision. Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) current_task_log: Vec<String>,
    /// Supervision-rounds status (`roundsActive`). Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) rounds_active: Option<crate::RoundsActive>,
    // --- Inc8 S4: evaluations ring + generic usage observability.
    /// The bounded evaluations ring (camelCase records, `taskRef` inside). Omitted
    /// when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) evaluations: Vec<crate::Evaluation>,
    /// Generic use counter (`usageCount`). Omitted when never used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) usage_count: Option<u64>,
    /// Unix-millis of last use (`lastUsedAt`). Omitted when never set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_used_at: Option<u64>,
    /// Inc8 fold — DECLARED conventions (`.md` list). Omitted when empty (the web
    /// flags that emptiness; the daemon just omits it).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) conventions: Vec<String>,
    /// Inc9 b2 — context-window % used (0-100). Forced `SNAKE_CASE` (`context_pct`)
    /// via an explicit rename (this struct is `rename_all = "camelCase"`), because
    /// the b2/b3 web JS reads it snake — a camelCase key would read `undefined`.
    /// Omitted when no marker on screen.
    #[serde(rename = "context_pct", skip_serializing_if = "Option::is_none")]
    pub(crate) context_pct: Option<u8>,
    /// Inc9 b3 — a compaction/rehome (brutal context-% drop) landed recently.
    /// Forced `SNAKE_CASE` (`recently_compacted`) for the same reason. Omitted false.
    #[serde(rename = "recently_compacted", skip_serializing_if = "std::ops::Not::not")]
    pub(crate) recently_compacted: bool,
    /// S4: current task + `Task()` sub-agents, read from the tab's transcript.
    /// Flattened → `currentTask` / `subAgents` sit on the tab for `taskChips`.
    /// Empty (`currentTask:null`, `subAgents:[]`) when the tab has no transcript.
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

/// An orchestrator working in a project (S5): named, with its current `item`
/// (the volatile context) and a GLOBAL `child_count` — the number of tabs whose
/// `parent_tab_id` is this orchestrator, wherever they live. Feeds the "name the
/// orchestrators under their repo + multi-orch tree" view.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratorRef {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) item: String,
    pub(crate) child_count: usize,
}

/// A project bucket (level 0): the 7-phase subtree scoped to one project, plus
/// its rollup. `méta` and `divers` are the two shared lanes.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardProject {
    pub(crate) name: String,
    pub(crate) tab_count: usize,
    pub(crate) rollup_led: Option<&'static str>,
    pub(crate) has_orchestrator: bool,
    pub(crate) is_meta: bool,
    /// The orchestrators working in this repo, sorted by id (S5).
    pub(crate) orchestrators: Vec<OrchestratorRef>,
    pub(crate) nodes: Vec<DashboardNode>,
    pub(crate) unmapped: Vec<DashboardTab>,
}

/// A service = a family of repos (Increment 6 S3): a shared prefix (≥2 repos) or
/// an explicit `repo_families` map forms a named service; a lone repo stays a
/// "mono" service named after itself. Wraps the flat `projects`, non-breaking.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardService {
    pub(crate) name: String,
    /// Worst led among the service's sub-repos.
    pub(crate) rollup_led: Option<&'static str>,
    /// The member repo names (== `DashboardProject.name`s), sorted.
    pub(crate) projects: Vec<String>,
}

/// One delegation edge: `child` was spawned by `parent` (both tab UUIDs).
#[derive(Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LineageEdge {
    pub(crate) child: String,
    pub(crate) parent: String,
}

#[derive(Serialize)]
pub struct DashboardState {
    /// Global 7-node diagram (Increment 1 contract — preserved).
    pub(crate) nodes: Vec<DashboardNode>,
    pub(crate) unmapped: Vec<DashboardTab>,
    /// Per-project buckets (Increment 2), sorted alpha with `méta`/`divers` last.
    pub(crate) projects: Vec<DashboardProject>,
    /// Services (Increment 6 S3): the flat `projects` grouped into repo families.
    /// Kept ALONGSIDE `projects` (non-breaking) — the web can use either level.
    pub(crate) services: Vec<DashboardService>,
    /// Delegation lineage (S6): `parent_tab_id` edges whose parent is a known
    /// tab. A tab with no/unknown parent is a root (no edge). Self-edges dropped.
    pub(crate) lineage: Vec<LineageEdge>,
    /// Tabs with NO `assignment` at all (S5, #90) — legitimately un-placed,
    /// sorted by id. Distinct from `unmapped` (assigned but an unknown phase).
    pub(crate) unassigned: Vec<DashboardTab>,
    /// task primitive (#11 S4) read-model: every task queue's current state,
    /// derived READ-ONLY at read time (queued / claimed@peer / done). Filled at
    /// the handler (FS read there); empty from the pure builder / in tests.
    pub(crate) tasks: Vec<crate::cli::task::TaskQueueView>,
    /// agent-lifecycle (RB2) read-model: retired agent cards, a SEPARATE SOURCE
    /// read from catalog.jsonl at serve-time (a retired agent is absent from the
    /// live snapshot, so this is NOT a filter of it), folded latest-per-slug.
    /// READ-ONLY / INERT. Filled at the handler; empty from the pure builder.
    pub(crate) retired: Vec<crate::cli::catalog::CatalogCard>,
    /// agent-lifecycle v2 (SV3) read-model: retired records folded BY SKILL NAME into
    /// one mode-agnostic profile + per-mode metrics + a DERIVED fresh-vs-resume
    /// compare. v2 records only (v1 quarantined). Filled at the handler; empty from
    /// the pure builder. READ-ONLY.
    pub(crate) skills: Vec<crate::cli::catalog::SkillProfile>,
}

/// Minimal per-tab projection the dashboard builder consumes, so the
/// mapping/rollup logic is unit-testable without constructing a full
/// `TabState`. `assignment` drives phase/role/project; `cwd` drives project;
/// `context` is the volatile subtitle.
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
    /// S4 per-tab activity, read from the transcript at the call site (the pure
    /// builder stays FS-free / unit-testable). Empty in tests and headless.
    pub(crate) activity: TabActivity,
    // Inc8 S1 agent-card fields (self-declared), threaded through to DashboardTab.
    pub(crate) specialty: Option<String>,
    pub(crate) orchestrator: Option<String>,
    pub(crate) objective: Option<String>,
    pub(crate) current_task: Vec<String>,
    pub(crate) rounds_active: Option<crate::RoundsActive>,
    // Inc8 S4: evaluations ring + generic usage observability.
    pub(crate) evaluations: Vec<crate::Evaluation>,
    pub(crate) usage_count: Option<u64>,
    pub(crate) last_used_at: Option<u64>,
    // Inc8 fold: declared conventions (.md list).
    pub(crate) conventions: Vec<String>,
    // Inc9 b2/b3: context-% + whether a compaction landed recently.
    pub(crate) context_pct: Option<u8>,
    pub(crate) recently_compacted: bool,
}

/// One re-home lifecycle step: the wire slug + its French progress-badge label.
pub struct RehomeStep {
    pub slug: &'static str,
    /// Read only by `rehome_badge` (a GUI-only consumer, app.rs); `REHOME_STEPS`
    /// still sets it in both editions, so it's dead — not absent — in headless.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub label: &'static str,
}

/// THE single source of truth for the 4 re-home states, in progress order
/// (audit Q3). Validation (`POST …/rehome`), the safe-to-close gate, and the
/// badge all derive from this — adding a 5th state means editing only here. The
/// last step is the terminal `safe-to-close`, posted by the old agent on its
/// ACK, which gates the "close the predecessor" action. (`set_rehome.rs`'s
/// `--help` / 400 text is kept in sync by `rehome_help_lists_every_state`.)
pub const REHOME_STEPS: [RehomeStep; 4] = [
    RehomeStep {
        slug: "handoff-written",
        label: "handoff écrit",
    },
    RehomeStep {
        slug: "successor-ready",
        label: "successeur prêt",
    },
    RehomeStep {
        slug: "ack-sent",
        label: "ACK envoyé",
    },
    RehomeStep {
        slug: "safe-to-close",
        label: "SAFE À FERMER",
    },
];

#[derive(Clone)]
pub struct SnapshotTab {
    /// Stable per-tab UUID, mirrored from `TabState.id`. Used to route
    /// `POST /tabs/by-id/{id}/status` to the right tab independent of
    /// its position in the list (renames don't change it).
    /// The small per-tab strings are `Arc<str>` for the same reason the
    /// two big dumps below are: the snapshot is rebuilt per tab on
    /// every refresh, and under attended streaming that's ~10x/s — as
    /// owned `String`s each rebuild re-allocated every tab's id, name,
    /// tokens, cwd, … for content that almost never changes. The
    /// headless tab state holds the same `Arc`s, so a rebuild clone is
    /// a refcount bump. (Consumers that need owned text — the /tabs
    /// JSON, WS meta — pay one copy on their own, rarer, cadence.)
    pub id: std::sync::Arc<str>,
    pub name: std::sync::Arc<str>,
    pub cwd: Option<std::sync::Arc<str>>,
    /// `Arc<str>` (shared with the per-tab `GridSnapshotCache`): the
    /// snapshot is rebuilt per tab on every refresh tick, and these two
    /// dumps are by far its heaviest fields — sharing makes the rebuild
    /// a refcount bump instead of a multi-hundred-KB copy per tab.
    pub output: std::sync::Arc<str>,
    /// Row-by-row dump for the xterm.js viewer — server grid rows
    /// emitted as separate `\n`-terminated lines (NO WRAPLINE join),
    /// so the browser-side terminal at the same cols reproduces the
    /// server's layout cell-for-cell. The mobile remote and CLI
    /// viewer keep using `output` (logical lines, easier to word-wrap
    /// on a phone).
    pub raw_output: std::sync::Arc<str>,
    /// CRC32 of `output` / `raw_output`, stamped when the grid dump was
    /// (re)built (`GridSnapshotCache::new`) so `GET /output` doesn't
    /// re-hash the whole payload on every poll.
    pub output_crc: u32,
    pub raw_output_crc: u32,
    /// Cursor (`row_in_raw_output`, col) — coordinates inside
    /// `raw_output` so the xterm.js viewer can issue a
    /// cursor-position escape after each write and the blinking
    /// cursor lands where the user is actually typing. Distinct
    /// from `cursor` which is in `output` (joined-line) coords.
    pub raw_cursor: Option<(usize, usize)>,
    pub uptime_secs: f64,
    /// Cursor (logical-row, logical-column) within `output` — after
    /// alacritty's WRAPLINE rows have been joined into single lines.
    /// None when the cursor is outside the emitted lines (e.g. in
    /// scrollback beyond the cached window).
    pub cursor: Option<(usize, usize)>,
    /// Current PTY dimensions (cols, rows). Surfaced on /output as
    /// `X-Output-Cols` / `X-Output-Rows` so the xterm.js viewer can
    /// resize its grid to match the server, avoiding wrap mismatch.
    pub cols: u16,
    pub rows: u16,
    /// Per-tab share secrets. The "read-write" one authorises every
    /// `/tabs/by-id/{uuid}/...` route on this tab; the "read-only"
    /// one is rejected on `/input` with 403, so the URL itself is
    /// the permission scope (stripping `&ro=1` does nothing because
    /// the *token* is what's checked). Both default to empty until
    /// the GUI menu mints them on first share.
    pub share_token_rw: std::sync::Arc<str>,
    pub share_token_ro: std::sync::Arc<str>,
    /// Manual lock — user-toggled via right-click / `POST /lock`.
    ///
    /// **Gate authors:** read [`crate::schedule::LockState::effective_locked`]
    /// instead of this raw field. The effective state factors in the
    /// off-hours [`Self::schedule`] auto-lock so a new gate can't
    /// accidentally honour only the manual flag.
    pub locked: bool,
    /// Off-hours auto-lock. Mirrored from `TabState.schedule`. When
    /// the rule's current state is closed,
    /// [`crate::schedule::LockState::effective_locked`] reports
    /// `true` even if [`Self::locked`] is false. Carries the tz so
    /// the viewer can show "locked until Mo 09:00 Europe/Paris" in
    /// headers (`X-Tab-Schedule-Tz`, `X-Tab-Schedule-Next`) without
    /// parsing the rule.
    pub schedule: Option<crate::schedule::TabSchedule>,
    /// Effective background color for this tab's viewer (per-tab
    /// override or global default; never `None`). Shipped to the
    /// viewer via `X-Tab-Bg` on /output + `__TAB_BG__` template
    /// substitution on /view.
    pub bg_color: std::sync::Arc<str>,
    /// Free-text context an in-tab agent set for itself via
    /// `tab-atelier set-context "…"` — e.g. the PR/issue it's working
    /// on. Surfaced on `/tabs` and as a hover tooltip on the GUI tab
    /// name. `None` ⇒ no context set.
    pub context: Option<std::sync::Arc<str>>,
    /// The agent's stable workflow assignment (`"[<project>:]<phase>/<role>"`),
    /// mirrored from the runtime tab. Unlike `context`, it's persisted and
    /// hook-immune (see [`crate::TabState::assignment`]); the dashboard maps a
    /// tab onto a phase node + project from this. `None` ⇒ unassigned.
    pub assignment: Option<std::sync::Arc<str>>,
    /// UUID of the spawning tab (`parent_tab_id`), mirrored from the runtime
    /// tab. Drives the dashboard delegation lineage. `None` ⇒ a root tab.
    pub parent_tab_id: Option<std::sync::Arc<str>>,
    /// Re-home progress on a predecessor tab, mirrored from the runtime tab.
    /// See [`crate::TabState::rehome_status`]. `None` ⇒ not rehoming.
    pub rehome_status: Option<std::sync::Arc<str>>,
    /// PID of the tab's shell. The /catbus endpoints walk its
    /// descendant processes to find a catbus-agent (or fallback
    /// `claude` TUI) and resolve the session's transcript file.
    #[cfg_attr(not(feature = "catbus"), allow(dead_code))]
    pub shell_pid: u32,
    /// Transient agent state, mirrored from the in-RAM Tab. Surfaced
    /// in the `/tabs` response (so the CLI viewer can render the LED
    /// without a per-tab probe) and as the `X-Agent-State` header
    /// on `/stream` for the share-link viewer's title badge.
    pub agent_state: Option<crate::AgentStateSnapshot>,
    /// Durable agent session UUID, mirrored from the in-RAM Tab.
    /// Populated by `set-status --session …`; today no API consumer
    /// reads it, but the field is persisted into tabs.json so
    /// auto-resume after a daemon restart can reconstruct the
    /// agent's session.
    #[allow(dead_code)]
    pub agent_session_id: Option<std::sync::Arc<str>>,
    /// Durable agent CLI kind (`catbus` / `claude` / …). Same
    /// "session attached" semantic the desktop LED uses to render a
    /// steady grey dot when there's no transient state.
    pub agent_kind: Option<std::sync::Arc<str>>,
    /// The fully-derived per-tab agent LED, computed once at snapshot-build
    /// time (GUI and headless) by [`crate::compute_tab_led`] so the `/tabs`
    /// `led` field, the CLI viewer and the mobile remote all render the exact
    /// dot the desktop draws — without each consumer re-deriving it (they lack
    /// the raw liveness / last-output / unreviewed signals). `None` ⇒ no dot.
    pub agent_led: Option<crate::TabLed>,
    /// Unix-millis of the last time this tab was used (input / activate /
    /// viewer open). Mirrored from the runtime tab and serialized on `/tabs`
    /// so clients order the list most-recently-used-first server-side.
    pub last_used_at: Option<u64>,
    /// How many WS viewers (browser share-link / `remote attach`) are
    /// currently watching this tab. Surfaced on `/tabs` so `tabs`-list
    /// consumers can see who's being watched; also the GUI's "tab is
    /// being tended" signal that suppresses the dormant LED.
    pub viewers: usize,
    /// Per-tab raw PTY byte ring captured BEFORE alacritty's parser.
    /// `GET /tabs/by-id/{id}/stream[?since=N]` reads from this; the
    /// xterm.js share-link viewer uses it to populate scrollback,
    /// because alacritty's grid history is wiped by `\x1b[3J` and
    /// doesn't grow when TUIs (Claude, htop, less) redraw in-place.
    /// `None` for tabs that pre-date PTY-tap wiring — endpoint
    /// responds 404 in that case.
    pub pty_ring: Option<std::sync::Arc<std::sync::Mutex<crate::pty_ring::PtyRing>>>,
    /// Whether the tab's shell runs with no internet (bubblewrap
    /// network-isolated). Mirrored from the runtime tab so `/tabs` and
    /// the net toggle endpoint can report it. Desktop GUI toggles it via
    /// the right-click menu; headless via `net-off`/`net-on`.
    pub net_disabled: bool,
    /// Active outbound connection count (metering), refreshed on a timer
    /// from `/proc` (see `net_meter`). 0 when not yet sampled / none.
    pub connections: usize,
    /// Egress bytes (allowlist tabs only, from nftables counters): total the
    /// tab tried to send, and bytes the allowlist dropped. 0 otherwise.
    pub tx_bytes: u64,
    pub tx_denied_bytes: u64,
    /// The tab's current allowlist config, mirrored from the runtime tab so
    /// `/tabs` reports it and the `net-allow --add/--remove` CLI can merge
    /// against it. Empty ⇒ not in allowlist mode.
    pub net_allow: crate::net_policy::AllowConfig,
    /// DNS-entries view for a domain-allowlist tab: `(domain, allowed, ips)`
    /// from the per-tab resolver — including DENIED queries (what the tab
    /// tried to reach and couldn't). Empty when no resolver.
    pub dns_entries: Vec<(String, bool, Vec<String>)>,
    /// Resident set size (bytes) of the tab's shell-process subtree,
    /// sampled from `/proc` at the 2 s snapshot cadence (`agent_probe::sample_tree`).
    /// `None` until the first sample, or when the subtree walk fails.
    /// Surfaced on `/tabs` + `/tabs/usage` as `resident_memory_bytes`.
    pub resident_memory_bytes: Option<u64>,
    /// Cumulative agent token usage, mirrored from the runtime tab's
    /// catbus-agent `tokens.json` sidecar. `None` for non-agent tabs (or
    /// builds without `catbus`). Surfaced as `tokens: {input, output}`.
    pub tokens: Option<crate::TokenUsage>,
    // --- Inc8 S1 agent card, mirrored from the persisted TabState (hook-immune).
    pub specialty: Option<std::sync::Arc<str>>,
    pub orchestrator: Option<std::sync::Arc<str>>,
    pub objective: Option<std::sync::Arc<str>>,
    /// The bounded `current_task` permalog (see [`crate::append_current_task`]).
    pub current_task: Vec<String>,
    pub rounds_active: Option<crate::RoundsActive>,
    // Inc8 S4 (last_used_at already lives above as the MRU stamp).
    pub evaluations: Vec<crate::Evaluation>,
    pub usage_count: Option<u64>,
    /// Inc8 fold — declared conventions (`.md` list).
    pub conventions: Vec<String>,
    /// Inc9 b2 — context-window % used (0-100) parsed from the tab's screen
    /// (`clarify::parse_context_pct`); `None` when no context marker is on screen.
    pub context_pct: Option<u8>,
    /// Inc9 b3 — unix-millis of the last detected compaction/rehome (a brutal
    /// context-% drop). `None` = never; drives `recently_compacted` on the wire.
    pub last_compaction_at: Option<u64>,
}

impl crate::schedule::LockState for SnapshotTab {
    fn manual_locked(&self) -> bool {
        self.locked
    }
    fn schedule(&self) -> Option<&crate::schedule::TabSchedule> {
        self.schedule.as_ref()
    }
}

/// A status update queued by `POST /tabs/by-id/{id}/status` — drained
/// by the main loop, which writes both the transient `agent_state`
/// snapshot and the durable `agent_session_id` / `agent_kind` /
/// `agent_plan_mode` fields onto the matching tab.
#[derive(Clone, Debug)]
pub struct PendingStatusUpdate {
    pub tab_id: String,
    pub state: crate::AgentState,
    pub label: Option<String>,
    pub session_id: Option<String>,
    pub agent_kind: Option<String>,
    pub plan_mode: Option<bool>,
}

/// A queued relay-config change (the CLI `relay via <ep>` / `relay egress`).
/// `endpoint: Some("box")` sets `relay_endpoint_id` (resolved against
/// `remote_endpoints`); `Some("")` clears it. `egress: Some(bool)` sets the
/// egress role. Applied + persisted + re-installed live by the drain.
#[derive(Clone, Debug, Default)]
pub struct RelayConfigChange {
    pub endpoint: Option<String>,
    pub egress: Option<bool>,
}

/// A queued env-var change (the CLI `env set/unset`). `tab: None` targets the
/// global map; `Some(uuid)` a single tab. `set` upserts, `unset` removes. Takes
/// effect on the tab's next (re)spawn.
#[derive(Clone, Debug)]
pub struct EnvChange {
    pub tab: Option<String>,
    pub set: std::collections::BTreeMap<String, String>,
    pub unset: Vec<String>,
}

/// Inc8 S1 — one queued agent-card mutation for the owner loop to apply + persist.
///
/// Overwrite variants carry `Option<String>` (`None` = clear); `CurrentTaskAppend`
/// appends one phrase to the bounded permalog ([`crate::append_current_task`]);
/// `RoundsActive` sets the supervision-rounds status. One enum keeps the owner
/// drain a single pass (vs a queue per field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardChange {
    Specialty(Option<String>),
    Orchestrator(Option<String>),
    Objective(Option<String>),
    CurrentTaskAppend(String),
    RoundsActive(crate::RoundsActive),
    // Inc8 S4: append an evaluation record (bounded ring); bump usage (count+stamp).
    EvaluationAppend(crate::Evaluation),
    Usage(u64, u64),
    // Inc8 fold: OVERWRITE the declared conventions (.md list).
    Conventions(Vec<String>),
}
