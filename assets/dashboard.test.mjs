// Self-check for the pure logic of the dashboard web app.
// Run: node assets/dashboard.test.mjs  (exits non-zero if the mapping breaks)
// No framework — just node:assert. Imports the real functions from dashboard.js
// so this stays a check of shipped code, not a copy.
import assert from "node:assert/strict";
import { ledClass, nodeMap, CANONICAL_PHASES } from "./dashboard.js";

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

console.log("dashboard.test.mjs: OK");
