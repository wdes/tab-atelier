// Self-check for the pure logic of the dashboard web app.
// Run: node assets/dashboard.test.mjs  (exits non-zero if the mapping breaks)
// No framework — just node:assert. Imports the real functions from dashboard.js
// so this stays a check of shipped code, not a copy.
import assert from "node:assert/strict";
import { ledClass, nodeMap, CANONICAL_PHASES, resolveView, renderProjectCard, readProjectParam } from "./dashboard.js";

// The five led states each map to their own distinct class.
const cases = {
  dead: "led-dead",
  error: "led-error",
  working: "led-working",
  unreviewed: "led-unreviewed",
  idle: "led-idle",
};
for (const [led, cls] of Object.entries(cases)) {
  assert.equal(ledClass(led), cls, `led "${led}" should map to ${cls}`);
}

// The five classes are distinct (no two leds collide).
assert.equal(new Set(Object.values(cases)).size, 5, "five leds -> five distinct classes");

// An empty node (rollupLed null) renders neutral, not a broken/blank class.
assert.equal(ledClass(null), "led-neutral", "null -> neutral");
// Unknown/garbage degrades to neutral rather than led-undefined.
assert.equal(ledClass(undefined), "led-neutral", "undefined -> neutral");
assert.equal(ledClass("bogus"), "led-neutral", "unknown -> neutral");

// nodeMap indexes nodes by id and survives malformed input.
const m = nodeMap({ nodes: [{ id: "build", rollupLed: "working" }, { id: "done", rollupLed: null }] });
assert.equal(m.get("build").rollupLed, "working");
assert.equal(m.get("done").rollupLed, null);
assert.equal(nodeMap(null).size, 0, "null state -> empty map");
assert.equal(nodeMap({}).size, 0, "no nodes -> empty map");

// The canonical skeleton is exactly the seven documented phases, in order.
assert.deepEqual(CANONICAL_PHASES, ["scope", "plan", "build", "review", "verify", "sweep", "done"]);

// --- resolveView: level-0 grid vs scoped/legacy diagram (S2/S3) ---
// No projects[] (pre-S1 / legacy) -> the global diagram, whatever currentProject.
{
  const legacy = { nodes: [{ id: "build", rollupLed: "working", tabs: [] }], unmapped: [] };
  const v = resolveView(legacy, null);
  assert.equal(v.mode, "diagram");
  assert.equal(v.scoped, false);
  assert.equal(v.nodes.length, 1);
  // A stray currentProject can't conjure a grid when the server sends no projects.
  assert.equal(resolveView(legacy, "kalpin-back").mode, "diagram");
}
// projects[] present, none drilled -> grid, in server order (no re-sort).
{
  const state = {
    projects: [
      { name: "kalpin-back", tabCount: 2, rollupLed: "working", nodes: [{ id: "build", rollupLed: "working", tabs: [] }] },
      { name: "méta", tabCount: 1, rollupLed: "idle", isMeta: true, nodes: [] },
    ],
    nodes: [], unmapped: [],
  };
  const grid = resolveView(state, null);
  assert.equal(grid.mode, "grid");
  assert.deepEqual(grid.projects.map((p) => p.name), ["kalpin-back", "méta"]);
  // Drill into a known project -> scoped diagram with that project's nodes.
  const drill = resolveView(state, "kalpin-back");
  assert.equal(drill.mode, "diagram");
  assert.equal(drill.scoped, true);
  assert.equal(drill.nodes[0].id, "build");
  // Unknown selected project -> empty scoped diagram, not an error/throw.
  const unknown = resolveView(state, "nope");
  assert.equal(unknown.mode, "diagram");
  assert.equal(unknown.scoped, true);
  assert.deepEqual(unknown.nodes, []);
}

// --- renderProjectCard: name, led class, count, meta + orchestrator markers ---
{
  const id = (s) => s; // identity escaper for the test
  const card = renderProjectCard(
    { name: "kalpin-back", tabCount: 3, rollupLed: "error", hasOrchestrator: true },
    id
  );
  assert.match(card, /project-card led-error/, "card carries its led class");
  assert.match(card, /data-project="kalpin-back"/, "card carries its project name for drill-in");
  assert.match(card, /3 tabs/, "card shows the tab count (plural)");
  assert.match(card, /orch-badge/, "card shows the orchestrator badge");
  // meta lane + neutral led + singular count.
  const metaCard = renderProjectCard({ name: "méta", tabCount: 1, rollupLed: null, isMeta: true }, id);
  assert.match(metaCard, /project-card led-neutral meta/, "meta card is neutral + meta");
  assert.match(metaCard, /1 tab</, "singular count has no plural s");
  assert.doesNotMatch(metaCard, /orch-badge/, "no orchestrator badge when absent");
}

// --- readProjectParam: deep-link ?project= -> drilled project (S3) ---
assert.equal(readProjectParam("?project=kalpin-back"), "kalpin-back");
assert.equal(readProjectParam(""), null, "no query -> level 0");
assert.equal(readProjectParam("?token=abc"), null, "other params -> level 0");
assert.equal(readProjectParam("?project="), null, "empty value -> level 0");

console.log("dashboard.test.mjs: OK");
