// Self-check for the PURE logic of Increment 8 Slice 4 (docs/dashboard-increment-8.md).
// Run: node assets/dashboard.inc8.s4.test.mjs   (exits non-zero if a contract breaks)
// Namespace import so a not-yet-exported helper fails an assertion (RED) rather
// than crashing the module link. The web builder makes this green by exporting:
//   evalSummary(tab) — the right-click card's evaluations section: the last evals,
//                      the latest verdict, and a `triggerArmed` flag mirroring the
//                      rust avg/burst triggers (errors over the 1/1M budget, OR
//                      >=3 errors in the last 1M tokens). Reads the /dashboard/state
//                      records: evaluations[].{tokens:{in,out}, scores:{errors}, verdict}.
// Builder: web (card evaluations render).
import assert from "node:assert/strict";
import * as dash from "./dashboard.js";

// eval record as exposed on the wire (camelCase; tokens keyed "in"/"out").
const ev = (errors, tin, tout, verdict = "ok") =>
  ({ evaluator: "olympe", at: 0, taskRef: "t", tokens: { in: tin, out: tout }, scores: { relevance: 8, errors, omissions: 0 }, verdict });

assert.equal(typeof dash.evalSummary, "function", "Inc8-S4 RED: export evalSummary(tab) from dashboard.js");
{
  // Clean: 1 error over 2M tokens -> under the 1/1M budget AND < 3 -> NOT armed.
  const clean = dash.evalSummary({ evaluations: [ev(1, 1_000_000, 1_000_000, "ok")] });
  assert.equal(clean.triggerArmed, false, "1 error / 2M tokens, single record -> not armed");
  assert.equal(clean.verdict, "ok", "verdict = the latest eval's verdict");
  assert.equal(clean.recent.length, 1);

  // avg trigger: 2 errors over 1M tokens -> over the 1/1M budget -> armed.
  const avg = dash.evalSummary({ evaluations: [ev(2, 500_000, 500_000)] });
  assert.equal(avg.triggerArmed, true, "2 errors / 1M -> avg trigger armed");

  // burst trigger: 3 errors within the last 1M tokens -> armed (even if avg is fine).
  const burst = dash.evalSummary({ evaluations: [ev(1, 50_000, 50_000), ev(1, 50_000, 50_000), ev(1, 50_000, 50_000)] });
  assert.equal(burst.triggerArmed, true, "3 errors in the 1M window -> burst trigger armed");

  // recent = the LAST 5 evals, verdict = the newest.
  const many = { evaluations: Array.from({ length: 7 }, (_, i) => ev(0, 10, 10, `v${i}`)) };
  const s = dash.evalSummary(many);
  assert.equal(s.recent.length, 5, "the card shows a bounded slice of the evals");
  assert.equal(s.verdict, "v6", "verdict = the latest record");
  assert.equal(s.recent[s.recent.length - 1].verdict, "v6", "recent is newest-last, bounded to 5");

  // Graceful: no evaluations -> empty, no verdict, not armed; null-safe.
  const empty = dash.evalSummary({});
  assert.deepEqual(empty.recent, []);
  assert.equal(empty.verdict, "");
  assert.equal(empty.triggerArmed, false);
  assert.doesNotThrow(() => dash.evalSummary(null), "null tab must not throw");
}

console.log("dashboard.inc8.s4.test.mjs: OK");
