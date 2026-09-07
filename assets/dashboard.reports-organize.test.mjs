// Self-check for the PURE logic of the Rapports "ranger" (a) + "à la une" (b), calqués sur le
// moteur du catalogue. Run: node assets/dashboard.reports-organize.test.mjs  (also `node --test`).
//   reportFacets(report)          -> derived {repo, task, day, mtime} from the flat {name,path,mtime};
//   reportGroups(readModel, mode) -> ordered [{label,count,reports}] for repo/task/date;
//   reportsFeatured(readModel, n) -> the top-N by mtime desc (the distinct "à la une").
import assert from "node:assert/strict";
import { reportFacets, reportGroups, reportsFeatured, REPORTS_SORT_MODES } from "./dashboard.js";

const D = 86400;
const at = (name, mtime) => ({ name, path: `outbox/${name}`, mtime });
// A varied outbox: distinct repos/tasks/dates + two runs of one task, one clearly-oldest report.
const REPORTS = [
  at("kalpin-back-review-01.md", 100 * D + 9), // repo kalpin / task kalpin-back-review
  at("kalpin-front-audit-02.md", 100 * D + 8), // repo kalpin / task kalpin-front-audit
  at("kalpin-back-review-03.md", 100 * D + 7), // repo kalpin / task kalpin-back-review (2nd run)
  at("titour-deploy-99.md", 100 * D + 6),      // repo titour / task titour-deploy
  at("atelier-notes.md", 100 * D + 5),         // repo atelier / task atelier-notes (no trailing digit)
  at("misc-oneoff-77.md", 2 * D),              // repo misc / task misc-oneoff — clearly OLDEST
];
const rm = { reports: REPORTS };

// ---- reportFacets: honest filename derivation (repo=leading token, task=stem minus digit-token) --
{
  const f = reportFacets(at("kalpin-back-review-01.md", 100 * D));
  assert.equal(f.repo, "kalpin", "repo = leading name token");
  assert.equal(f.task, "kalpin-back-review", "task = stem minus a trailing digit-bearing token (-01)");
  assert.equal(f.day, new Date(100 * D * 1000).toISOString().slice(0, 10), "day = mtime's ISO date");
  // a pure-alpha trailing token is KEPT (not mistaken for a version/nonce).
  assert.equal(reportFacets(at("atelier-notes.md", D)).task, "atelier-notes", "alpha trailing token kept");
  // a nonce (digit-bearing, 6+) is stripped.
  assert.equal(reportFacets(at("rapport-ab12cd.md", D)).task, "rapport", "digit-bearing nonce stripped");
  // a real path subdir wins as repo (upgrade path: outbox/<repo>/…).
  assert.equal(reportFacets({ name: "x.md", path: "outbox/kalpin-front/x.md", mtime: 1 }).repo, "kalpin-front", "dir segment wins as repo");
  // null-safe.
  assert.equal(reportFacets(null).repo, "divers", "null -> divers, no throw");
}

// ---- reportGroups repo: biggest cluster first, ties alpha; newest-first within a group ----------
{
  const g = reportGroups(rm, "repo");
  assert.deepEqual(g.map((x) => x.label), ["kalpin", "atelier", "misc", "titour"], "kalpin(3) first, then singletons alpha");
  assert.deepEqual(g.map((x) => x.count), [3, 1, 1, 1], "kalpin holds its 3 reports");
  assert.deepEqual(g[0].reports.map((r) => r.name),
    ["kalpin-back-review-01.md", "kalpin-front-audit-02.md", "kalpin-back-review-03.md"], "within group = newest mtime first");
}

// ---- reportGroups task: same reports, DIFFERENT partition (proves repo≠task axis) --------------
{
  const g = reportGroups(rm, "task");
  assert.deepEqual(g.map((x) => x.label),
    ["kalpin-back-review", "atelier-notes", "kalpin-front-audit", "misc-oneoff", "titour-deploy"],
    "kalpin-back-review(2) first, then singletons alpha");
  assert.deepEqual(g.map((x) => x.count), [2, 1, 1, 1, 1]);
  // repo gives 4 groups, task gives 5 -> the two axes genuinely differ.
  assert.notEqual(reportGroups(rm, "repo").length, g.length, "repo (4) and task (5) partition differently");
}

// ---- reportGroups date: one group per ISO day, NEWEST day first --------------------------------
{
  const g = reportGroups(rm, "date");
  const days = g.map((x) => x.label);
  assert.deepEqual(days, days.slice().sort((a, b) => b.localeCompare(a)), "date groups newest-day-first");
  assert.equal(g[g.length - 1].reports[0].name, "misc-oneoff-77.md", "the oldest report is in the last (oldest) day group");
}

// ---- reportsFeatured: the 5 most recent by mtime desc, DISTINCT from the grouping ---------------
{
  const feat = reportsFeatured(rm, 5);
  assert.equal(feat.length, 5, "capped at 5 even with 6 reports");
  assert.equal(feat[0].name, "kalpin-back-review-01.md", "most recent first");
  assert.ok(!feat.some((r) => r.name === "misc-oneoff-77.md"), "the oldest report is NOT à la une");
  // but it IS still present in the ranged list below (à la une doesn't archive it away).
  assert.ok(reportGroups(rm, "repo").some((g) => g.reports.some((r) => r.name === "misc-oneoff-77.md")),
    "the excluded report still shows in the grouped list");
  assert.deepEqual(reportsFeatured(null, 5), [], "null -> [], no throw");
}

// ---- the ranging modes are registered for the selector -----------------------------------------
assert.deepEqual(REPORTS_SORT_MODES.map((m) => m.value), ["date", "repo", "task"], "ranging modes wired for the selector");
assert.ok(REPORTS_SORT_MODES.some((m) => m.label === "tâche"), "task mode is labelled 'tâche'");

console.log("dashboard.reports-organize.test.mjs: OK");
