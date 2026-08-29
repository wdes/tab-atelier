// Self-check for the PURE logic of the Inc8 `conventions` fold (docs/dashboard-increment-8.md).
// Run: node assets/dashboard.inc8.conventions.test.mjs   (exits non-zero on a break)
// Namespace import so a not-yet-exported helper fails an assertion (RED) rather
// than crashing the module link. The web builder makes this green by exporting:
//   conventionsCheck(tab) — the card's declared-conventions section: the list +
//                           a `missing` FLAG when the agent declared none (the
//                           free-bot-style "no conventions" check). Reads the
//                           /dashboard/state field `conventions` (a string[]).
// The declared-vs-existing SEMANTIC check is ta-convention-auditor's job, not here.
// Builder: web (card conventions render).
import assert from "node:assert/strict";
import * as dash from "./dashboard.js";

assert.equal(typeof dash.conventionsCheck, "function", "Inc8-conventions RED: export conventionsCheck(tab) from dashboard.js");
{
  // Declared: the list rides through, not flagged.
  const has = dash.conventionsCheck({ conventions: ["AGENTS.md", "docs/dashboard.md"] });
  assert.deepEqual(has.conventions, ["AGENTS.md", "docs/dashboard.md"], "the declared list is carried");
  assert.equal(has.declared, true);
  assert.equal(has.missing, false, "a declared agent is not flagged");

  // Empty list -> FLAG (agent declared no conventions).
  const none = dash.conventionsCheck({ conventions: [] });
  assert.deepEqual(none.conventions, []);
  assert.equal(none.declared, false);
  assert.equal(none.missing, true, "empty conventions -> missing flag");

  // Absent field (old tab / never declared) -> same flag, null-safe.
  const absent = dash.conventionsCheck({});
  assert.deepEqual(absent.conventions, []);
  assert.equal(absent.missing, true, "absent conventions -> missing flag");
  assert.doesNotThrow(() => dash.conventionsCheck(null), "null tab must not throw");
  assert.equal(dash.conventionsCheck(null).missing, true, "null tab -> missing flag, no throw");
}

console.log("dashboard.inc8.conventions.test.mjs: OK");
