// Self-check for the PURE logic of Catalogue #39 SC2 (web VUE read-only).
// Run: node assets/dashboard.catalogue.test.mjs  (or node --test).
// Proves: catalogView (sorted, null-safe), skillProfileModel (normalised), and
// byModeMetricsModel (borne 6: directional fresh_vs_resume + InsufficientSample,
// NEVER pass/fail). Fixtures use the camelCase contract to confirm with SC1/ta-rust.
import assert from "node:assert/strict";
import { catalogView, skillProfileModel, byModeMetricsModel, MIN_SAMPLE } from "./dashboard.js";

// ============================ catalogView ============================
{
  const rm = { skills: [{ name: "zebra" }, { name: "alpha" }, { notname: 1 }, null] };
  assert.deepEqual(catalogView(rm).map((s) => s.name), ["alpha", "zebra"], "sorted by proper name; nameless/null filtered");
  assert.deepEqual(catalogView({}), [], "no skills -> []");
  assert.deepEqual(catalogView(null), [], "null read-model -> [], no throw");
}

// ============================ skillProfileModel ============================
{
  const p = skillProfileModel({ name: "x", prompt: "do", specialty: "rust", conventions: ["A.md"], tools: ["t"], patterns: ["p"], promptVersion: 3 });
  assert.equal(p.name, "x");
  assert.equal(p.prompt, "do");
  assert.equal(p.specialty, "rust");
  assert.deepEqual(p.conventions, ["A.md"]);
  assert.deepEqual(p.tools, ["t"]);
  assert.deepEqual(p.patterns, ["p"]);
  assert.equal(p.promptVersion, 3);
  const e = skillProfileModel({});
  assert.equal(e.name, "");
  assert.equal(e.prompt, "");
  assert.deepEqual(e.conventions, []);
  assert.deepEqual(e.tools, []);
  assert.deepEqual(e.patterns, []);
  assert.equal(e.promptVersion, null);
  assert.doesNotThrow(() => skillProfileModel(null), "null-safe");
}

// ============================ byModeMetricsModel — borne 6 ============================
const VERDICTS = ["FreshFavored", "ResumeFavored", "Inconclusive", "InsufficientSample"];
{
  // Enough samples both sides + server verdict -> carried, not insufficient.
  const m = byModeMetricsModel({
    metrics: { byMode: { fresh: { success: 8, problem: 1, tokensAvg: 100, costAvg: 2, n: 9 }, resume: { success: 5, problem: 4, tokensAvg: 120, costAvg: 3, n: 9 } } },
    fresh_vs_resume: "FreshFavored",
  });
  assert.equal(m.verdict, "FreshFavored");
  assert.equal(m.insufficient, false);
  assert.equal(m.n, 18);
  assert.equal(m.fresh.success, 8);
  assert.equal(m.resume.problem, 4);

  // One mode below MIN_SAMPLE -> InsufficientSample OVERRIDES any server verdict.
  const ins = byModeMetricsModel({ metrics: { byMode: { fresh: { n: 2 }, resume: { n: 20 } } }, fresh_vs_resume: "FreshFavored" });
  assert.equal(ins.verdict, "InsufficientSample", "a thin mode => no verdict (borne 6)");
  assert.equal(ins.insufficient, true);

  // Derive a DIRECTION from success rates when the server omits fresh_vs_resume.
  assert.equal(byModeMetricsModel({ metrics: { byMode: { fresh: { success: 9, problem: 0, n: 9 }, resume: { success: 3, problem: 6, n: 9 } } } }).verdict, "FreshFavored", "fresh higher rate");
  assert.equal(byModeMetricsModel({ metrics: { byMode: { fresh: { success: 3, problem: 6, n: 9 }, resume: { success: 9, problem: 0, n: 9 } } } }).verdict, "ResumeFavored", "resume higher rate");
  assert.equal(byModeMetricsModel({ metrics: { byMode: { fresh: { success: 5, problem: 5, n: 10 }, resume: { success: 5, problem: 5, n: 10 } } } }).verdict, "Inconclusive", "equal rate -> inconclusive");

  // Normalise snake/spaced server variants.
  assert.equal(byModeMetricsModel({ metrics: { byMode: { fresh: { n: 9 }, resume: { n: 9 } } }, fresh_vs_resume: "resume_favored" }).verdict, "ResumeFavored");

  // NEVER pass/fail: the verdict is always one of the four directional/insufficient values.
  assert.ok(VERDICTS.includes(m.verdict) && VERDICTS.includes(ins.verdict));

  // Null-safe -> InsufficientSample, no throw.
  const nul = byModeMetricsModel(null);
  assert.equal(nul.insufficient, true);
  assert.equal(nul.verdict, "InsufficientSample");
}
assert.ok(MIN_SAMPLE >= 1, "MIN_SAMPLE is a positive threshold");

console.log("dashboard.catalogue.test.mjs: OK");
