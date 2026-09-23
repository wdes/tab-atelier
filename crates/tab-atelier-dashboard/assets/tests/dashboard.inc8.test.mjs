// Self-check for the PURE logic of Increment 8 Slice 1 (docs/dashboard-increment-8.md).
// Run: node assets/dashboard.inc8.test.mjs   (exits non-zero if a contract breaks)
// Namespace import so a not-yet-exported helper fails an assertion (RED) rather
// than crashing the module link. The web builder makes this green by exporting:
//   agentCard(tab)  (S1) — the self-declared "agent card" render model:
//                          specialty / orchestrator / objective / currentTask +
//                          a `free` flag (orchestrator === "free") for the 'libre' badge.
// Builder: web (card render).
import assert from "node:assert/strict";
import * as dash from "./dashboard.js";

assert.equal(typeof dash.agentCard, "function", "Inc8-S1 RED: export agentCard(tab) from dashboard.js");
{
  // A FREE agent (orchestrator "free") with a DECLARED permalog (`currentTaskLog`,
  // array of appended phrases): the card shows its objective + the LATEST phrase,
  // and the 'libre' badge fires. currentTask sources from the DECLARED permalog,
  // NOT the observed transcript field (Inc9 (1) fix).
  const card = dash.agentCard({
    id: "u1", name: "ta-x",
    specialty: "rust async internals",
    orchestrator: "free",
    objective: "land the parser refactor",
    currentTaskLog: ["read the plan", "wire the struct", "run the tests"],
  });
  assert.equal(card.specialty, "rust async internals");
  assert.equal(card.objective, "land the parser refactor");
  assert.equal(card.orchestrator, "free");
  assert.equal(card.free, true, "orchestrator === 'free' -> the 'libre' badge fires");
  assert.equal(card.currentTask, "run the tests", "currentTask = the latest DECLARED permalog phrase");

  // The OBSERVED transcript field `currentTask` is IGNORED by the card (it bled
  // broadcasts) — only the declared `currentTaskLog` counts.
  const observedOnly = dash.agentCard({ orchestrator: "orch-uuid-123", currentTask: "[[ici MAS broadcast]]" });
  assert.equal(observedOnly.currentTask, "", "the observed transcript currentTask is NOT surfaced (no bleed)");

  // An OWNED agent (orchestrator = a uuid) is NOT free; its declared permalog shows.
  const owned = dash.agentCard({ orchestrator: "orch-uuid-123", objective: "impl S1", currentTaskLog: ["wiring now"] });
  assert.equal(owned.free, false, "a uuid orchestrator is not free");
  assert.equal(owned.orchestrator, "orch-uuid-123");
  assert.equal(owned.currentTask, "wiring now", "declared permalog latest phrase");

  // Graceful: absent fields -> empty strings / null / false, no throw.
  const empty = dash.agentCard({});
  assert.equal(empty.specialty, "");
  assert.equal(empty.objective, "");
  assert.equal(empty.currentTask, "");
  assert.equal(empty.orchestrator, null, "no orchestrator -> null");
  assert.equal(empty.free, false, "no orchestrator -> not free");
  assert.doesNotThrow(() => dash.agentCard(null), "null tab must not throw");
  // An empty permalog array -> empty currentTask.
  assert.equal(dash.agentCard({ currentTaskLog: [] }).currentTask, "", "empty permalog -> empty currentTask");
}

console.log("dashboard.inc8.test.mjs: OK");
