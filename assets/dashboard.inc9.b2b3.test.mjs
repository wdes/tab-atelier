// Self-check for the PURE logic of Inc9 bricks b2 (context pill) + b3 (compaction
// badge). Run: node assets/dashboard.inc9.b2b3.test.mjs  (or node --test).
// TDD (coverage-first): proves the colour thresholds, the label, and the absence
// of a pill/badge when the fields are missing. Builder: web (fixtures mirror the
// rust field names: context_pct 0-100, recently_compacted bool).
import assert from "node:assert/strict";
import { contextPill, compactionBadge } from "./dashboard.js";

// ============================ b2 — contextPill thresholds ============================
{
  // green < 70
  assert.equal(contextPill({ context_pct: 0 }).cls, "ctx-ok", "0 -> green");
  assert.equal(contextPill({ context_pct: 50 }).cls, "ctx-ok", "50 -> green");
  assert.equal(contextPill({ context_pct: 69 }).cls, "ctx-ok", "69 -> green");
  // amber 70-90 (inclusive)
  assert.equal(contextPill({ context_pct: 70 }).cls, "ctx-warn", "70 -> amber");
  assert.equal(contextPill({ context_pct: 85 }).cls, "ctx-warn", "85 -> amber");
  assert.equal(contextPill({ context_pct: 90 }).cls, "ctx-warn", "90 -> amber");
  // red > 90
  assert.equal(contextPill({ context_pct: 91 }).cls, "ctx-crit", "91 -> red");
  assert.equal(contextPill({ context_pct: 100 }).cls, "ctx-crit", "100 -> red");
  // label + rounding
  assert.equal(contextPill({ context_pct: 42 }).label, "42% ctx", "label is 'N% ctx'");
  assert.equal(contextPill({ context_pct: 42.6 }).pct, 43, "pct is rounded");
  // absent / non-number -> null (the caller renders NO pill)
  assert.equal(contextPill({}), null, "absent context_pct -> null (no pill)");
  assert.equal(contextPill({ context_pct: null }), null, "null context_pct -> null");
  assert.equal(contextPill({ context_pct: "80" }), null, "string context_pct -> null (not a number)");
  assert.equal(contextPill(null), null, "null tab -> null, no throw");
}

// ============================ b3 — compactionBadge ============================
{
  assert.equal(compactionBadge({ recently_compacted: true }).show, true, "recently_compacted true -> shown");
  assert.equal(compactionBadge({ recently_compacted: false }).show, false, "false -> not shown");
  assert.equal(compactionBadge({}).show, false, "absent -> not shown");
  assert.equal(compactionBadge({ recently_compacted: 1 }).show, false, "truthy-but-not-true -> not shown (strict)");
  assert.equal(compactionBadge(null).show, false, "null tab -> not shown, no throw");
}

console.log("dashboard.inc9.b2b3.test.mjs: OK");
