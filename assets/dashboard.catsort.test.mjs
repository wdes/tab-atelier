// Self-check for the PURE logic of the catalogue category-sort (Jules, view-lane).
// Run: node assets/dashboard.catsort.test.mjs   (also picked up by `node --test`).
// catalogGroups(readModel, mode) -> ordered [{label, count, skills}]. Modes:
//   name (default) = flat header-less alpha list (label:null) = historical, non-breaking;
//   usage/status = mechanical buckets on the ACTIVE skills (Option A);
//   category (B3) = group by the `skill` a card was distilled from, over the COMBINED
//   active templates + `.retired` distilled-agent cards; skill==null -> "Divers", last.
import assert from "node:assert/strict";
import { catalogGroups, CATALOG_SORT_MODES } from "./dashboard.js";

const DIVERS = "Divers / non catégorisé";

// ---- name (default): non-breaking flat alpha, one header-less group -----------------
{
  const rm = { skills: [{ skill: "zebra" }, { skill: "alpha" }, { noSkill: 1 }, null], retired: [] };
  const g = catalogGroups(rm, "name");
  assert.equal(g.length, 1, "name -> single group");
  assert.equal(g[0].label, null, "name group is header-less");
  assert.deepEqual(g[0].skills.map((s) => s.skill), ["alpha", "zebra"], "alpha-sorted, null/nameless filtered");
  assert.equal(g[0].count, 2);
  assert.deepEqual(catalogGroups(null, "name")[0].skills, [], "null read-model -> [], no throw");
  assert.deepEqual(catalogGroups({ skills: [{ skill: "x" }] }), catalogGroups({ skills: [{ skill: "x" }] }, "name"), "default mode = name");
}

// ---- usage: buckets on usageCount, empty dropped, most-used first --------------------
{
  const rm = { skills: [
    { skill: "hot", usageCount: 9 }, { skill: "mid", usageCount: 3 },
    { skill: "cold", usageCount: 0 }, { skill: "nul" /* usageCount absent -> 0 */ },
  ] };
  const g = catalogGroups(rm, "usage");
  assert.deepEqual(g.map((x) => x.label), ["fréquent (5+)", "rare (1–4)", "jamais utilisé"], "bucket order");
  assert.deepEqual(g.map((x) => x.count), [1, 1, 2], "hot / mid / (cold+nul)");
  assert.deepEqual(g[0].skills.map((s) => s.skill), ["hot"]);
}

// ---- status: active vs deleted (SC3 tombstone ONLY, NOT retiredAt) -------------------
{
  const rm = { skills: [
    { skill: "a" },
    { skill: "b", retiredAt: 5 },      // stale retiredAt on an ACTIVE template -> still actif
    { skill: "c", deleted: true },     // SC3 soft-delete -> supprimé
    { skill: "d", tombstoned: true },  // tombstone -> supprimé
  ] };
  const g = catalogGroups(rm, "status");
  assert.deepEqual(g.map((x) => `${x.label}:${x.count}`), ["actifs:2", "supprimés:2"], "retiredAt≠deleted: a,b actifs ; c,d supprimés");
}

// ---- category (B3): group by distilled-from skill over skills+retired ----------------
{
  const rm = {
    skills: [
      { skill: "feature-pr-completer", usageCount: 1 }, // active template head
      { skill: "agent-role-auditor" },                  // active singleton head
    ],
    retired: [
      { skill: "feature-pr-completer", name: "Colette-2", retiredAt: 101 },
      { skill: "feature-pr-completer", name: "Colette-1", retiredAt: 100 },
      { skill: "agent-role-auditor", name: "Aud-1", retiredAt: 102 },
      { skill: null, name: "Ponytail", retiredAt: 103 },   // -> Divers
      { skill: null, name: "ta-perffix", retiredAt: 104 }, // -> Divers
      { noise: true },                                     // no skill, no name -> dropped
    ],
  };
  const g = catalogGroups(rm, "category");
  assert.deepEqual(g.map((x) => x.label), ["feature-pr-completer", "agent-role-auditor", DIVERS],
    "biggest cluster first, ties alpha, Divers always last");
  assert.deepEqual(g.map((x) => x.count), [3, 2, 2], "1 active + 2 retired ; 1 active + 1 retired ; 2 null-skill");
  // active template heads its cluster (retiredAt absent), then retired by name alpha.
  assert.equal(g[0].skills[0].retiredAt, undefined, "active template first in its cluster");
  assert.deepEqual(g[0].skills.slice(1).map((s) => s.name), ["Colette-1", "Colette-2"], "retired sorted by name");
  assert.deepEqual(g[2].skills.map((s) => s.name), ["Ponytail", "ta-perffix"], "Divers holds the skill==null named cards");
  // zero core: retired absent -> still works (just the active heads, singletons).
  const g2 = catalogGroups({ skills: [{ skill: "solo" }] }, "category");
  assert.deepEqual(g2.map((x) => `${x.label}:${x.count}`), ["solo:1"]);
}

// ---- the mode is registered for the selector ----------------------------------------
assert.ok(CATALOG_SORT_MODES.some((m) => m.value === "category" && m.label === "catégorie"), "category mode wired into the selector");

console.log("dashboard.catsort.test.mjs: OK");
