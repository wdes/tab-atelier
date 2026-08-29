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
  // The REAL tichef on the live daemon has role "manager" (assignment
  // "meta/manager"), living in a méta project's unmapped bucket. It still lands in
  // the Méta band (isTichefRole recognises "manager" as well as the "tichef" alias).
  const tichef = t({ id: "tc", name: "ta-tichef", role: "manager", assignment: "meta/manager" });
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
  // Inc9 (4): Méta band = the autonomous daemon (tichef) ONLY; the solo meta
  // specialist (planner) is now an on-demand SUPPORTER, in its own band.
  assert.deepEqual(bl.meta.map((x) => x.id).sort(), ["tc"], "Méta band = tichef (autonomous daemon)");
  assert.deepEqual(bl.supporters.map((x) => x.id).sort(), ["p"], "Supporters band = the solo meta specialist");
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
  // A live-shaped tichef (role "manager", in méta unmapped) lands in the Méta band.
  const liveMeta = dash.bandLayout({
    projects: [{ name: "méta", isMeta: true, nodes: [], unmapped: [{ id: "tc", role: "manager", assignment: "meta/manager" }] }],
    unmapped: [{ id: "tc", role: "manager", assignment: "meta/manager" }], // daemon also lists it top-level
  });
  assert.deepEqual(liveMeta.meta.map((m) => m.id), ["tc"], "manager tichef in Méta, deduped (not duplicated across sources)");
  assert.equal(liveMeta.freelancers.length, 0, "no duplicate tichef in Freelancers");
}

// ============ Inc8.2: nest-by-orchestrator-field fallback (declared living-card) ============
// A worker with NO spawn lineage nests under the lead named by its DECLARED card
// field `orchestrator` (set-orchestrator alone = single-source). When lineage IS
// present it WINS — the worker never lands under two leads.
{
  const t = (o) => ({ agentState: "idle", ...o });
  const lead = t({ id: "L", name: "ta-lead", role: "orchestrator", assignment: "repoA:build/orchestrator" });
  const other = t({ id: "L2", name: "ta-lead2", role: "orchestrator", assignment: "repoB:build/orchestrator" });
  const declaredOnly = t({ id: "d", name: "ta-declared", role: "worker", orchestrator: "L", assignment: "repoA:build/worker" }); // NO parentTabId
  const lineageWins = t({ id: "g", name: "ta-lineage", role: "worker", parentTabId: "L", orchestrator: "L2", assignment: "repoA:build/worker" });
  const bl = dash.bandLayout({ nodes: [], unmapped: [lead, other, declaredOnly, lineageWins], unassigned: [], projects: [] });
  const underL = bl.orchestrators.find((o) => o.lead.id === "L").repos.flatMap((r) => r.workers.map((w) => w.id)).sort();
  assert.deepEqual(underL, ["d", "g"], "Inc8.2: declared-only worker nests via the orchestrator field; the lineage worker stays under its parent");
  const underL2 = bl.orchestrators.find((o) => o.lead.id === "L2").repos.flatMap((r) => r.workers.map((w) => w.id));
  assert.equal(underL2.includes("g"), false, "Inc8.2: lineage WINS — a worker with parentTabId is NOT also placed under its declared-field lead");
  assert.equal(bl.freelancers.some((f) => f.id === "d"), false, "Inc8.2: the declared-only worker is placed (not orphaned to Freelancers)");
}

// ============================ hasTichef gate (root-cause fix) ============================
// The REAL tichef has role "manager" and lives in state.unmapped (not under a
// project) — the gate must recognise it there, or the 4-band view never triggers.
assert.equal(typeof dash.hasTichef, "function", "export hasTichef(state) for the gate");
assert.equal(dash.hasTichef({ unmapped: [{ id: "tc", role: "manager" }] }), true, "manager in TOP-LEVEL unmapped => gate on");
assert.equal(dash.hasTichef({ projects: [{ unmapped: [{ role: "manager" }] }] }), true, "manager in a project's unmapped => gate on");
assert.equal(dash.hasTichef({ projects: [{ nodes: [{ tabs: [{ role: "tichef" }] }] }] }), true, "'tichef' alias still recognised");
assert.equal(dash.hasTichef({ unassigned: [{ role: "manager" }] }), true, "manager in unassigned => gate on");
assert.equal(dash.hasTichef({ projects: [{ nodes: [{ tabs: [{ role: "orchestrator" }] }] }], unassigned: [{ role: "worker" }] }), false, "no tichef/manager => gate off");
assert.equal(dash.hasTichef(null), false, "null state => gate off, no throw");

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
  // Inc9 (4): a solo méta specialist (no serving) is now a SUPPORTER (its own band),
  // not Méta — Méta is reserved for the 3 autonomous daemons.
  assert.equal(dash.resolveAltitude({ role: "planner", serving: null }, state).band, "supporter", "solo méta -> Supporter band");
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
    id: "t1", name: "ta-x", currentTaskLog: ["wire the parser now"], // DECLARED permalog, not observed
    subAgents: [{ name: "Explore", state: "completed" }, { name: "code-reviewer", state: "running" }],
  };
  const chips = dash.taskChips(tab);
  assert.ok(Array.isArray(chips) && chips.length >= 3, "a task chip + one chip per sub-agent");
  assert.ok(chips.some((c) => /wire the parser/.test(c.label || "")), "the declared current task is shown");
  // Inc9 (1): a FREE agent shows NO task pill even with a declared permalog.
  assert.equal(dash.taskChips({ id: "f", orchestrator: "free", currentTaskLog: ["old task"] }).filter((c) => c.kind === "task").length, 0,
    "a free agent shows no task pill");
  // Inc9 (1): the OBSERVED transcript currentTask does NOT produce a pill (no bleed).
  assert.equal(dash.taskChips({ id: "b", currentTask: "[[ici MAS broadcast]]" }).filter((c) => c.kind === "task").length, 0,
    "the observed transcript currentTask does not bleed into a pill");
  const running = chips.find((c) => c.name === "code-reviewer");
  assert.ok(running && running.state === "running", "a running sub-agent chip carries its state");
  const done = chips.find((c) => c.name === "Explore");
  assert.ok(done && done.state === "completed", "a completed sub-agent chip carries its state");
  // Graceful: a tab with no transcript data -> empty chips, no throw.
  assert.deepEqual(dash.taskChips({ id: "e", name: "empty" }), [], "no task/sub-agents -> no chips");
  assert.doesNotThrow(() => dash.taskChips(null), "null tab must not throw");
}

console.log("dashboard.inc7.test.mjs: OK");
