// Self-check for the PURE logic of Increment 6 web slices (docs/dashboard-increment-6.md).
// Run: node assets/dashboard.inc6.test.mjs   (exits non-zero if a contract breaks)
// Namespace import so a not-yet-exported helper surfaces as a failing assertion
// (RED) rather than a module-link crash. The web builder makes these green:
//   orgLayout(state)      (S2) — org-chart: méta-top / team lead / workers / serving-joins-team
//   serviceLayout(state)  (S4) — service -> sub-repos -> teams nesting (mono not over-nested)
//   activityModel(json)   (S6) — extend with self_tooling / fixes / issues / verdict
// Builder: web (S2/S4/S6).
import assert from "node:assert/strict";
import * as dash from "./dashboard.js";

// ============================ S2 — orgLayout ============================
// The org-chart: a solo méta stays on top; a repo shows its orchestrator as the
// team LEAD with workers below (parentTabId), and a méta that `serving`s the repo
// JOINS the team (marked indispo) instead of floating on top.
assert.equal(typeof dash.orgLayout, "function", "S2 RED: export orgLayout(state) from dashboard.js");
{
  const o1 = { id: "o1", name: "ta-lead", role: "orchestrator", led: "idle" };
  const w1 = { id: "w1", name: "ta-w1", role: "implementer", parentTabId: "o1", led: "working" };
  const w2 = { id: "w2", name: "ta-w2", role: "worker", parentTabId: "o1", led: "idle" };
  const sm = { id: "sm", name: "ta-serving-planner", role: "planner", serving: "kalpin-back", led: "idle" };
  const solo = { id: "p", name: "ta-solo-planner", role: "planner", serving: null, led: "idle" };
  const state = {
    nodes: [], unmapped: [],
    projects: [
      { name: "kalpin-back", isMeta: false, hasOrchestrator: true,
        orchestrators: [{ id: "o1", name: "ta-lead", item: "delegating", childCount: 2 }],
        nodes: [{ id: "build", rollupLed: "working", tabs: [o1, w1, w2, sm] }], unmapped: [] },
      { name: "méta", isMeta: true, hasOrchestrator: false, orchestrators: [],
        nodes: [], unmapped: [solo] },
    ],
  };
  const layout = dash.orgLayout(state);
  // Solo méta on top; the serving méta is NOT on top (it joined the team).
  assert.ok(layout.metaTop.some((t) => t.id === "p"), "solo méta (serving null) sits on top");
  assert.ok(!layout.metaTop.some((t) => t.id === "sm"), "a serving méta is NOT on top");
  const team = layout.teams.find((t) => t.repo === "kalpin-back");
  assert.ok(team, "a team exists for kalpin-back");
  assert.equal(team.lead.id, "o1", "the orchestrator is the team lead");
  const wids = team.workers.map((t) => t.id).sort();
  assert.deepEqual(wids, ["w1", "w2"], "workers hang under the lead (parentTabId)");
  assert.ok(team.serving.some((t) => t.id === "sm"), "the serving méta joined this team");
  assert.doesNotThrow(() => dash.orgLayout(null), "null state must not throw");
}

// ============================ S4 — serviceLayout ============================
// The service nesting: a family service wraps its sub-repos; a mono service is
// NOT over-nested.
assert.equal(typeof dash.serviceLayout, "function", "S4 RED: export serviceLayout(state) from dashboard.js");
{
  const state = {
    nodes: [], unmapped: [], projects: [],
    services: [
      { name: "kalpin", rollupLed: "error", projects: [{ name: "kalpin-back" }, { name: "kalpin-front" }] },
      { name: "tab-atelier", rollupLed: "idle", projects: [{ name: "tab-atelier" }] },
    ],
  };
  const layout = dash.serviceLayout(state);
  assert.equal(layout.length, 2, "one entry per service, in order");
  assert.equal(layout[0].service, "kalpin");
  assert.equal(layout[0].repos.length, 2, "kalpin wraps its 2 sub-repos");
  assert.equal(layout[0].mono, false, "a family service is not mono");
  assert.equal(layout[1].service, "tab-atelier");
  assert.equal(layout[1].mono, true, "a single-repo service is mono (not over-nested)");
  assert.equal(layout[1].repos.length, 1);
  assert.deepEqual(dash.serviceLayout({}), [], "no services -> empty layout");
  assert.doesNotThrow(() => dash.serviceLayout(null), "null state must not throw");
}

// ============================ S6 — activityModel (extended) ============================
// New SEPARATE counters + the maturity/growth verdict, alongside the inc5 figures.
{
  const json = {
    totals: {
      features_implemented: 2, fixes: 3, self_tooling: 1,
      issues_opened: 4, issues_closed: 5,
      tokens_per_feature: 2100, minutes_since_last_human_prompt: 60, aligator_calls: 0, human_prompts: 3,
      tokens_total: { input: 3500, output: 700, cache_creation: 150, cache_read: 35 },
    },
    self_improvement_verdict: { verdict: "croissance", autonomy_trend: "up", tooling_rate: 0.5, evidence: ["a new tool/day"] },
    per_day: [{ date: "2026-08-23", features: 2, fixes: 3, self_tooling: 1, tokens_per_feature: 2100, autonomy_minutes_max: 270 }],
  };
  const m = dash.activityModel(json);
  // features stays UNDIVIDED; the new counters are distinct.
  assert.equal(m.features, 2, "features unchanged (not divided)");
  assert.equal(m.fixes, 3, "fixes surfaced separately");
  assert.equal(m.selfTooling, 1, "self_tooling surfaced separately");
  assert.equal(m.issuesOpened, 4, "issues_opened surfaced");
  assert.equal(m.issuesClosed, 5, "issues_closed surfaced");
  assert.equal(m.verdict, "croissance", "the maturity/growth verdict text is carried");
  // Per-day series for the new mini-graphs.
  assert.equal(m.series.selfTooling.length, 1, "self_tooling per-day series");
  assert.equal(m.series.fixes.length, 1, "fixes per-day series");
  // Graceful empty.
  const empty = dash.activityModel({});
  assert.equal(empty.fixes, 0, "empty -> 0 fixes");
  assert.equal(empty.selfTooling, 0, "empty -> 0 self_tooling");
  assert.ok(!empty.verdict, "empty -> no verdict");
}

console.log("dashboard.inc6.test.mjs: OK");
