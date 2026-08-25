// Self-check for the PURE logic of Increment 7 web slices (docs/dashboard-increment-7.md).
// Run: node assets/dashboard.inc7.test.mjs   (exits non-zero if a contract breaks)
// Namespace import so a not-yet-exported helper fails an assertion (RED) rather
// than crashing the module link. The web builder makes these green by exporting:
//   bandLayout(state)        (S1) — the 4-band compact org-chart model
//   resolveAltitude(tab,st)  (S2) — dynamic altitude: the 4 movements + tichef pin
//   diffRender(prev,next)    (S3) — stable-id patch ops (add/update/remove), no rebuild
//   taskChips(tab)           (S4) — current task + invoked sub-agents render model
// Builder: web (S1/S2/S3/S4-web).
import assert from "node:assert/strict";
import * as dash from "./dashboard.js";

// ============================ S1 — bandLayout ============================
// 4 stacked bands Méta / Orchestrateurs / Workers / Freelancers, with the 3-tier
// chain orchestrator -> served repo(s) -> workers (parentTabId). A mono-repo
// orchestrator points its workers directly. Fallback: no services/assignment ->
// no crash, non-mapped tabs land in Freelancers.
assert.equal(typeof dash.bandLayout, "function", "S1 RED: export bandLayout(state) from dashboard.js");
{
  const t = (o) => ({ agentState: "idle", ...o });
  const tichef = t({ id: "tc", name: "ta-tichef", role: "tichef" });
  const planner = t({ id: "p", name: "ta-planner", role: "planner", serving: null });
  const o1 = t({ id: "o1", name: "ta-kalpin-lead", role: "orchestrator", assignment: "kalpin-back:build/orchestrator" });
  const w1 = t({ id: "w1", name: "ta-w1", role: "implementer", parentTabId: "o1", assignment: "kalpin-back:build/implementer" });
  const w2 = t({ id: "w2", name: "ta-w2", role: "implementer", parentTabId: "o1", assignment: "kalpin-front:build/implementer" });
  const o2 = t({ id: "o2", name: "ta-fx-lead", role: "orchestrator", assignment: "fx:build/orchestrator" });
  const w3 = t({ id: "w3", name: "ta-w3", role: "worker", parentTabId: "o2", assignment: "fx:build/worker" });
  const w4 = t({ id: "w4", name: "ta-w4", role: "worker", parentTabId: "o2", assignment: "fx:build/worker" });
  const u1 = t({ id: "u1", name: "ta-free-1", role: "worker" });
  const u2 = t({ id: "u2", name: "ta-free-2", role: "worker" });
  const state = {
    nodes: [], unmapped: [],
    projects: [
      { name: "kalpin-back", isMeta: false, orchestrators: [{ id: "o1", name: "ta-kalpin-lead", childCount: 2 }], nodes: [{ id: "build", tabs: [o1, w1] }], unmapped: [] },
      { name: "kalpin-front", isMeta: false, orchestrators: [], nodes: [{ id: "build", tabs: [w2] }], unmapped: [] },
      { name: "fx", isMeta: false, orchestrators: [{ id: "o2", name: "ta-fx-lead", childCount: 2 }], nodes: [{ id: "build", tabs: [o2, w3, w4] }], unmapped: [] },
      { name: "méta", isMeta: true, orchestrators: [], nodes: [], unmapped: [tichef, planner] },
    ],
    unassigned: [u1, u2],
  };
  const bl = dash.bandLayout(state);
  // Méta band: tichef + solo planner.
  assert.deepEqual(bl.meta.map((x) => x.id).sort(), ["p", "tc"], "Méta band = tichef + solo méta");
  // Orchestrateurs band: 2 leads, each with its served-repo sub-nodes + workers.
  assert.equal(bl.orchestrators.length, 2, "two orchestrators");
  const kal = bl.orchestrators.find((o) => o.lead.id === "o1");
  assert.deepEqual(kal.repos.map((r) => r.repo).sort(), ["kalpin-back", "kalpin-front"], "multi-repo orchestrator serves 2 repos");
  assert.equal(kal.repos.find((r) => r.repo === "kalpin-back").workers.length, 1, "each served repo carries its workers");
  const fx = bl.orchestrators.find((o) => o.lead.id === "o2");
  assert.equal(fx.repos.length, 1, "mono-repo orchestrator -> single repo node (direct workers)");
  assert.equal(fx.repos[0].workers.length, 2, "FX points its 2 workers directly");
  // Freelancers band: the unassigned tabs.
  assert.deepEqual(bl.freelancers.map((x) => x.id).sort(), ["u1", "u2"], "Freelancers band = unassigned");
  // Fallback: legacy/empty state must not crash.
  assert.doesNotThrow(() => dash.bandLayout(null), "null state must not throw");
  const legacy = dash.bandLayout({ nodes: [], unmapped: [{ id: "x", name: "n" }] });
  assert.ok(Array.isArray(legacy.freelancers), "fallback keeps a freelancers array");
}

// ============================ S2 — resolveAltitude ============================
// The dynamic-altitude rules encoded in placement.
assert.equal(typeof dash.resolveAltitude, "function", "S2 RED: export resolveAltitude(tab,state) from dashboard.js");
{
  const state = { projects: [], unassigned: [] };
  // PIN: tichef is always Méta, even while serving.
  assert.equal(dash.resolveAltitude({ role: "tichef", serving: "kalpin-back" }, state).band, "meta", "tichef pinned to Méta");
  // Movement 1: a serving méta specialist descends into the served team (renfort), NOT Méta.
  const r = dash.resolveAltitude({ role: "planner", serving: "kalpin-back", assignment: "kalpin-back:plan/planner" }, state);
  assert.notEqual(r.band, "meta", "a serving méta leaves Méta");
  assert.equal(r.team, "kalpin-back", "…and joins the served team");
  assert.equal(r.reinforcement, true, "…marked as reinforcement (renfort)");
  // A solo méta (no serving) stays in Méta.
  assert.equal(dash.resolveAltitude({ role: "planner", serving: null }, state).band, "meta", "solo méta stays Méta");
  // Movement 2: a freelancer that RECEIVES an assignment climbs to worker under its team.
  const w = dash.resolveAltitude({ role: "implementer", assignment: "kalpin-back:build/implementer" }, state);
  assert.equal(w.band, "worker", "assigned worker climbs to Workers");
  assert.equal(w.team, "kalpin-back", "…under its team");
  // Movement 3 (orchestrator) + Movement 4 (non-assigned -> Freelancers).
  assert.equal(dash.resolveAltitude({ role: "orchestrator", assignment: "kalpin-back:build/orchestrator" }, state).band, "orchestrator");
  assert.equal(dash.resolveAltitude({ role: "worker", assignment: null }, state).band, "freelancer", "non-assigned -> Freelancers");
}

// ============================ S3 — diffRender ============================
// Flicker-free refresh (Zoetrope borrow): stable node ids, in-place patch ops.
assert.equal(typeof dash.diffRender, "function", "S3 RED: export diffRender(prev,next) from dashboard.js");
{
  const m1 = { nodes: [{ id: "a", led: "idle" }, { id: "b", led: "working" }] };
  // (a) a tick with NO structural change recreates nothing: only updates, no add/remove.
  const m1b = { nodes: [{ id: "a", led: "working" }, { id: "b", led: "working" }] };
  const ops1 = dash.diffRender(m1, m1b);
  assert.ok(ops1.every((o) => o.op !== "add" && o.op !== "remove"), "no node added/removed on a plain tick");
  assert.ok(ops1.some((o) => o.op === "update" && o.id === "a"), "the changed node is updated in place");
  // Identical model -> still no add/remove (idempotent).
  assert.ok(dash.diffRender(m1, m1).every((o) => o.op !== "add" && o.op !== "remove"), "identical model -> no structural op");
  // (c) adding a tab -> a single targeted add; nothing removed.
  const m2 = { nodes: [{ id: "a" }, { id: "b" }, { id: "c" }] };
  const opsAdd = dash.diffRender(m1, m2);
  assert.deepEqual(opsAdd.filter((o) => o.op === "add").map((o) => o.id), ["c"], "add tab -> one add op for its id");
  assert.equal(opsAdd.filter((o) => o.op === "remove").length, 0, "…and no removes");
  // Removing a tab -> a single targeted remove; nothing added.
  const m3 = { nodes: [{ id: "a" }] };
  const opsRem = dash.diffRender(m1, m3);
  assert.deepEqual(opsRem.filter((o) => o.op === "remove").map((o) => o.id), ["b"], "remove tab -> one remove op for its id");
  assert.equal(opsRem.filter((o) => o.op === "add").length, 0, "…and no adds");
  assert.doesNotThrow(() => dash.diffRender(null, m1), "malformed prev must not throw");
}

// ============================ S4 — taskChips (web render) ============================
// The per-card current task + invoked sub-agents (from the rust S4 fields).
assert.equal(typeof dash.taskChips, "function", "S4 RED: export taskChips(tab) from dashboard.js");
{
  const tab = {
    id: "t1", name: "ta-x", currentTask: "wire the parser now",
    subAgents: [{ name: "Explore", state: "completed" }, { name: "code-reviewer", state: "running" }],
  };
  const chips = dash.taskChips(tab);
  assert.ok(Array.isArray(chips) && chips.length >= 3, "a task chip + one chip per sub-agent");
  assert.ok(chips.some((c) => /wire the parser/.test(c.label || "")), "the current task is shown");
  const running = chips.find((c) => c.name === "code-reviewer");
  assert.ok(running && running.state === "running", "a running sub-agent chip carries its state");
  const done = chips.find((c) => c.name === "Explore");
  assert.ok(done && done.state === "completed", "a completed sub-agent chip carries its state");
  // Graceful: a tab with no transcript data -> empty chips, no throw.
  assert.deepEqual(dash.taskChips({ id: "e", name: "empty" }), [], "no task/sub-agents -> no chips");
  assert.doesNotThrow(() => dash.taskChips(null), "null tab must not throw");
}

console.log("dashboard.inc7.test.mjs: OK");
