// Self-check for the PURE logic of Catalogue #39 SC2 (web VUE read-only),
// reconciled to the LIVE SC1 contract. Run: node assets/dashboard.catalogue.test.mjs
// Contract: GET /catalog/list -> { skills: [ { skill, prompt, specialty,
// conventions[], tools[], patterns[], promptVersion, usageCount,
// metrics.byMode.{fresh,resume}{spawns,success,problem,tokensAvg,costAvg},
// freshVsResume{verdict, freshN, resumeN, deliveryDelta, tokensRatio} } ] }.
// The rust owns the G1 guard (MIN_SAMPLE=3) — the web renders the verdict VERBATIM.
import assert from "node:assert/strict";
import { catalogView, skillProfileModel, byModeMetricsModel } from "./dashboard.js";

// ============================ catalogView ============================
{
  const rm = { retired: [], skills: [{ skill: "zebra" }, { skill: "alpha" }, { noSkill: 1 }, null] };
  assert.deepEqual(catalogView(rm).map((s) => s.skill), ["alpha", "zebra"], "sorted by the `skill` fold key; nameless/null filtered");
  assert.deepEqual(catalogView({}), [], "no skills -> []");
  assert.deepEqual(catalogView(null), [], "null read-model -> [], no throw");
}

// ============================ skillProfileModel ============================
{
  const p = skillProfileModel({ skill: "x", prompt: "do", specialty: "rust", conventions: ["A.md"], tools: ["t"], patterns: ["p"], promptVersion: 3, usageCount: 9 });
  assert.equal(p.name, "x", "name comes from the `skill` field");
  assert.equal(p.prompt, "do");
  assert.equal(p.specialty, "rust");
  assert.deepEqual(p.conventions, ["A.md"]);
  assert.deepEqual(p.tools, ["t"]);
  assert.deepEqual(p.patterns, ["p"]);
  assert.equal(p.promptVersion, 3);
  assert.equal(p.usageCount, 9);
  const e = skillProfileModel({});
  assert.equal(e.name, "");
  assert.deepEqual(e.conventions, []);
  assert.equal(e.promptVersion, null);
  assert.equal(e.usageCount, null);
  assert.doesNotThrow(() => skillProfileModel(null), "null-safe");
}

// ============================ byModeMetricsModel — VERBATIM server verdict ============================
const VERDICTS = ["freshFavored", "resumeFavored", "inconclusive", "insufficientSample"];
{
  const skill = {
    metrics: { byMode: {
      fresh: { spawns: 15, success: 12, problem: 3, tokensAvg: 8000, costAvg: 5 },
      resume: { spawns: 15, success: 7, problem: 8, tokensAvg: 9000, costAvg: 6 },
    } },
    freshVsResume: { verdict: "freshFavored", freshN: 15, resumeN: 15, deliveryDelta: 0.33, tokensRatio: 0.89 },
  };
  const m = byModeMetricsModel(skill);
  assert.equal(m.verdict, "freshFavored", "server verdict rendered verbatim (camelCase)");
  assert.equal(m.insufficient, false);
  assert.equal(m.freshN, 15);
  assert.equal(m.resumeN, 15);
  assert.equal(m.n, 30);
  assert.equal(m.fresh.spawns, 15);
  assert.equal(m.fresh.success, 12);
  assert.equal(m.resume.problem, 8);
  assert.equal(m.fresh.tokensAvg, 8000);

  // insufficientSample from the server is surfaced verbatim (NO JS re-gate at 5).
  const ins = byModeMetricsModel({ metrics: { byMode: { fresh: { spawns: 2 }, resume: { spawns: 1 } } }, freshVsResume: { verdict: "insufficientSample", freshN: 2, resumeN: 1 } });
  assert.equal(ins.verdict, "insufficientSample");
  assert.equal(ins.insufficient, true);
  assert.equal(ins.freshN, 2);
  assert.equal(ins.resumeN, 1);

  // A server FreshFavored with n≥3 per arm is NOT overridden by any JS threshold —
  // this is the split-brain that MIN_SAMPLE=5-in-JS used to cause; gone now.
  const noSplit = byModeMetricsModel({ metrics: { byMode: { fresh: { spawns: 4 }, resume: { spawns: 4 } } }, freshVsResume: { verdict: "freshFavored", freshN: 4, resumeN: 4 } });
  assert.equal(noSplit.verdict, "freshFavored", "n=4/arm keeps the server verdict (no JS re-gate to 5)");
  assert.equal(noSplit.insufficient, false);

  // Missing tokensAvg/costAvg -> null (rendered as '—'); missing freshVsResume ->
  // defaults to insufficientSample; null-safe.
  const bare = byModeMetricsModel({ metrics: { byMode: { fresh: { spawns: 1 }, resume: {} } } });
  assert.equal(bare.fresh.tokensAvg, null);
  assert.equal(bare.verdict, "insufficientSample", "absent freshVsResume -> insufficientSample default");
  const nul = byModeMetricsModel(null);
  assert.equal(nul.verdict, "insufficientSample");
  assert.ok(VERDICTS.includes(m.verdict) && VERDICTS.includes(ins.verdict), "never pass/fail — one of the 4 verdicts");
}

console.log("dashboard.catalogue.test.mjs: OK");
