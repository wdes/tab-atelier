// Coverage lot — dashboard quality audit #357 (topic harness-quality).
// Run: node assets/dashboard.quality.test.mjs   (exits non-zero if a lock breaks)
// Namespace import so a not-yet-exported helper fails an assertion (RED) rather
// than crashing the module link. Builder: web (QUE assets/dashboard.*).
import assert from "node:assert/strict";
import * as dash from "./dashboard.js";

// ============================ Q7 — buildBandModel invariant ============================
// buildBandModel is the led+task SIGNATURE that feeds the S3 (Zoetrope) diff, but
// it is not exported -> the flicker-free invariant is untested. Export it and lock
// the CORE guarantee: a change to a tab's TASK (led unchanged, structure unchanged)
// produces exactly ONE targeted `update` op on that node — never add/remove, never a
// recreate. If the task ever drops out of the signature, the chart would silently go
// stale on task changes (or worse, rebuild and lose selection/scroll). This test is
// the tripwire. RED until buildBandModel is exported.
assert.equal(typeof dash.buildBandModel, "function", "Q7 RED: export buildBandModel(state) from dashboard.js");

// A stable two-tab team (orchestrator lead + one worker) + a tichef, so the band
// layout mounts a real chain. `extra` mutates only the worker's task signature.
const fleet = (extra) => ({
  nodes: [], unmapped: [], unassigned: [],
  projects: [
    { name: "kalpin-back", isMeta: false, orchestrators: [{ id: "o1", name: "lead", childCount: 1 }],
      nodes: [{ id: "build", rollupLed: "working", tabs: [
        { id: "o1", name: "lead", role: "orchestrator", assignment: "kalpin-back:build/orchestrator", led: "idle", currentTask: "orchestrating" },
        { id: "w1", name: "ta-w1", role: "implementer", parentTabId: "o1", assignment: "kalpin-back:build/implementer",
          led: "working", currentTask: extra.task, subAgents: extra.subs || [] },
      ] }], unmapped: [] },
    { name: "méta", isMeta: true, orchestrators: [], nodes: [], unmapped: [{ id: "tc", name: "tichef", role: "tichef", led: "idle" }] },
  ],
});

const s1 = fleet({ task: "wiring the parser" });
// Same tabs, same structure, w1's LED unchanged — ONLY its task text changed.
const s2 = fleet({ task: "wiring the CSS now" });
const opsTask = dash.diffRender(dash.buildBandModel(s1), dash.buildBandModel(s2));
assert.equal(opsTask.filter((o) => o.op === "add" || o.op === "remove").length, 0,
  "Q7: a task-only change adds/removes NO node (no recreate)");
assert.deepEqual(opsTask.filter((o) => o.op === "update").map((o) => o.id), ["w1"],
  "Q7: a task-only change => exactly ONE targeted update op on that node");

// The sub-agent list is part of the same signature (S4 chips): changing it also
// yields a single targeted update, not a rebuild.
const s3 = fleet({ task: "wiring the parser", subs: [{ name: "Explore", state: "running" }] });
const opsSubs = dash.diffRender(dash.buildBandModel(s1), dash.buildBandModel(s3));
assert.equal(opsSubs.filter((o) => o.op === "add" || o.op === "remove").length, 0,
  "Q7: a sub-agent change adds/removes NO node");
assert.deepEqual(opsSubs.filter((o) => o.op === "update").map((o) => o.id), ["w1"],
  "Q7: a sub-agent change => one targeted update (subAgents are in the signature)");

// Idempotent: an unchanged model produces ZERO ops — no spurious recreate on a tick.
assert.equal(dash.diffRender(dash.buildBandModel(s1), dash.buildBandModel(s1)).length, 0,
  "Q7: an unchanged tick produces no ops (idempotent, no flicker)");

// Fallback safety: buildBandModel must not throw on a malformed/empty state.
assert.doesNotThrow(() => dash.buildBandModel(null), "Q7: buildBandModel(null) must not throw");
assert.ok(Array.isArray(dash.buildBandModel({}).nodes), "Q7: buildBandModel({}) -> { nodes: [] }");

console.log("dashboard.quality.test.mjs: OK");
