// Self-check for the PURE logic of Increment 5 web slices (docs/dashboard-increment-5.md).
// Run: node assets/dashboard.inc5.test.mjs   (exits non-zero if a contract breaks)
// No framework — node:assert. Uses a NAMESPACE import so a not-yet-exported helper
// surfaces as a failing assertion (RED) rather than a module-link crash that would
// take the whole file down. The web builder makes these green by exporting:
//   legendModel()        (S1)  — the legend entries (swatch class + label)
//   activityModel(json)  (S4)  — the 5 figures + per-day series from /dashboard/activity
//   overviewLayout(state)(S6)  — band order (META first / UNASSIGNED last) + repo→orchestrators
// Builder: web (S1/S4/S6).
import assert from "node:assert/strict";
import * as dash from "./dashboard.js";

// ============================ S1 — legendModel ============================
// The legend explains the visual vocabulary: one swatch per led state, the
// orchestrator accent, and the delegation (lineage) arrow. Each entry carries the
// CSS class that actually paints the colour (so the GUI test can match swatch↔node).
assert.equal(typeof dash.legendModel, "function", "S1 RED: export legendModel() from dashboard.js");
{
  const model = dash.legendModel();
  assert.ok(Array.isArray(model) && model.length > 0, "legendModel returns a non-empty array");
  const classes = model.map((e) => e && e.cls);
  for (const s of ["led-working", "led-error", "led-idle", "led-unreviewed", "led-dead"]) {
    assert.ok(classes.includes(s), `legend has a swatch for ${s}`);
  }
  assert.ok(classes.includes("orchestrator"), "legend explains the orchestrator accent");
  assert.ok(classes.some((c) => typeof c === "string" && c.includes("lineage")), "legend explains the lineage arrow");
  for (const e of model) assert.ok(e && typeof e.label === "string" && e.label.length > 0, "every legend entry has a label");
}

// ============================ S4 — activityModel ============================
// Pulls the five figures + three per-day series out of an /dashboard/activity payload.
assert.equal(typeof dash.activityModel, "function", "S4 RED: export activityModel(json) from dashboard.js");
{
  const json = {
    window_hours: 24,
    summary_lines: ["3 human prompts", "2 features"],
    record: { label: "go-build", autonomy_minutes: 360 },
    totals: {
      features_implemented: 2,
      tokens_total: { input: 3500, output: 700, cache_creation: 150, cache_read: 35 },
      tokens_per_feature: 2100,
      minutes_since_last_human_prompt: 60,
      aligator_calls: 0,
      human_prompts: 3,
    },
    per_day: [
      { date: "2026-08-22", features: 1, tokens_per_feature: 900, autonomy_minutes_max: 120 },
      { date: "2026-08-23", features: 2, tokens_per_feature: 2100, autonomy_minutes_max: 270 },
    ],
  };
  const m = dash.activityModel(json);
  assert.equal(m.features, 2, "features from totals.features_implemented");
  assert.equal(m.tokensPerFeature, 2100, "tokens_per_feature spelled out");
  assert.equal(m.minutesSinceLastHumanPrompt, 60, "minutes_since_last_human_prompt spelled out");
  assert.equal(m.aligatorCalls, 0, "aligator_calls surfaced (0 today)");
  assert.equal(m.humanPrompts, 3, "human_prompts count");
  // Three per-day series, each with one point PER day (2 days -> length 2).
  assert.equal(m.series.features.length, 2, "features series has one point per day");
  assert.equal(m.series.tokensPerFeature.length, 2, "tokens/feature series has one point per day");
  assert.equal(m.series.autonomy.length, 2, "autonomy series has one point per day");
  assert.deepEqual(m.series.features, [1, 2], "features series in per_day order");
  assert.ok(Array.isArray(m.summaryLines) && m.summaryLines.length >= 1, "summary lines carried");
  assert.ok(/go-build/i.test(JSON.stringify(m.record)), "the PO benchmark record is carried");
  // Graceful empty: absent/empty payload -> zeros + empty series, never a throw.
  const empty = dash.activityModel({});
  assert.equal(empty.features, 0, "empty payload -> 0 features");
  assert.equal(empty.aligatorCalls, 0, "empty payload -> 0 aligator");
  assert.equal(empty.series.features.length, 0, "empty payload -> empty series");
  assert.doesNotThrow(() => dash.activityModel(null), "null payload must not throw");
}

// ============================ S6 — overviewLayout ============================
// The reorg: META band on top, repos in the middle (each naming its orchestrators),
// UNASSIGNED band at the bottom; a repo with >1 orchestrator renders as a tree.
assert.equal(typeof dash.overviewLayout, "function", "S6 RED: export overviewLayout(state) from dashboard.js");
{
  const state = {
    nodes: [], unmapped: [],
    projects: [
      { name: "kalpin-back", isMeta: false, hasOrchestrator: true, nodes: [], unmapped: [],
        orchestrators: [
          { id: "o1", name: "orch-a", item: "delegating build", childCount: 2 },
          { id: "o2", name: "orch-b", item: "delegating review", childCount: 1 },
        ] },
      { name: "kalpin-front", isMeta: false, hasOrchestrator: true, nodes: [], unmapped: [],
        orchestrators: [{ id: "o3", name: "orch-c", item: "one orch", childCount: 1 }] },
      { name: "méta", isMeta: true, hasOrchestrator: false, nodes: [], unmapped: [{ id: "m1", name: "planner", role: "planner" }],
        orchestrators: [] },
    ],
    unassigned: [{ id: "u1", name: "scratch-1" }, { id: "u2", name: "scratch-2" }],
  };
  const layout = dash.overviewLayout(state);
  // Band ORDER: META first, UNASSIGNED last (the whole point of the reorg).
  assert.equal(layout.order[0], "META", "META band is first");
  assert.equal(layout.order[layout.order.length - 1], "UNASSIGNED", "UNASSIGNED band is last");
  // META band carries the transverse meta project.
  assert.ok(layout.meta.some((p) => p.name === "méta"), "meta band holds the méta project");
  // Repos name their orchestrators; >1 orchestrator => tree.
  const kb = layout.repos.find((r) => r.name === "kalpin-back");
  const kf = layout.repos.find((r) => r.name === "kalpin-front");
  assert.equal(kb.orchestrators.length, 2, "kalpin-back names its 2 orchestrators");
  assert.equal(kb.tree, true, "a repo with >1 orchestrator renders as a tree");
  assert.equal(kf.tree, false, "a single-orchestrator repo is not a tree");
  // UNASSIGNED band holds the assignment-less tabs (legitimate, not errors — #90).
  assert.equal(layout.unassigned.length, 2, "unassigned band holds both loose tabs");
  // Graceful on malformed input.
  assert.doesNotThrow(() => dash.overviewLayout(null), "null state must not throw");
  const emptyLayout = dash.overviewLayout({});
  assert.equal(emptyLayout.order[0], "META");
  assert.equal(emptyLayout.order[emptyLayout.order.length - 1], "UNASSIGNED");
}

console.log("dashboard.inc5.test.mjs: OK");
