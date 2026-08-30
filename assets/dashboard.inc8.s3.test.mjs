// Self-check for the PURE logic of Increment 8 Slice 3 (docs/dashboard-increment-8.md).
// Run: node assets/dashboard.inc8.s3.test.mjs   (exits non-zero if a contract breaks)
// Namespace import so a not-yet-exported helper fails an assertion (RED) rather
// than crashing the module link. The web builder makes these green:
//   agentCardView(tab) — the full right-click "agent card" model (specialty /
//                        orchestrator / objective / bounded permalog / evaluations /
//                        evalCriteria); orchestrators get a card too.
//   roundsPill(tab)     — the orchestrator's supervision-rounds badge (green when
//                        roundsActive.active === true, grey otherwise).
//   bandLayout(state)   — (already exported) must put the META-TRIO (tichef + Brain
//                        + aligator, orchestrator="meta") in the Méta band.
// Reads the S1 fields exposed on DashboardTab: specialty / orchestrator / objective
// / currentTaskLog (the bounded permalog) / roundsActive.{active}. Builder: web.
import assert from "node:assert/strict";
import * as dash from "./dashboard.js";

// ============================ Rouge 1 — agentCardView ============================
assert.equal(typeof dash.agentCardView, "function", "Inc8-S3 RED: export agentCardView(tab) from dashboard.js");
{
  const log = ["read plan", "wire struct", "add test", "run tests", "fix lint", "review", "commit", "handoff"]; // 8 entries
  const tab = {
    id: "w1", name: "ta-w1", role: "implementer", assignment: "kalpin-back:build/implementer",
    specialty: "rust async internals", orchestrator: "o-uuid",
    objective: "land the parser refactor",
    currentTaskLog: log,
    evaluations: [{ evaluator: "olympe", verdict: "ok" }],
    evalCriteria: ["no panics", "tests green"],
  };
  const card = dash.agentCardView(tab);
  assert.equal(card.assignment, "kalpin-back:build/implementer", "Inc9-B: the card carries the assignment");
  assert.equal(card.specialty, "rust async internals");
  assert.equal(card.orchestrator, "o-uuid");
  assert.equal(card.free, false, "a uuid orchestrator is not free");
  assert.equal(card.objective, "land the parser refactor");
  // Permalog is BOUNDED on the card — only the LAST entries are shown (in order).
  assert.ok(card.recentTasks.length <= 5, "the card shows a bounded slice of the permalog");
  assert.deepEqual(card.recentTasks, ["add test", "run tests", "fix lint", "review", "commit", "handoff"].slice(-5),
    "recentTasks = the last 5 permalog entries, in order");
  assert.deepEqual(card.evaluations, [{ evaluator: "olympe", verdict: "ok" }], "evaluations shown when present");
  assert.deepEqual(card.evalCriteria, ["no panics", "tests green"], "evalCriteria shown");

  // An ORCHESTRATOR gets a card too (no exclusion), and a `free` agent flags it.
  const orch = dash.agentCardView({ id: "o1", name: "ta-lead", role: "orchestrator", orchestrator: "free", objective: "delegate" });
  assert.equal(orch.role, "orchestrator", "orchestrators get a card");
  assert.equal(orch.free, true, "orchestrator === 'free' -> free");

  // Graceful: absent fields -> empty; evaluations/evalCriteria default to [].
  const empty = dash.agentCardView({});
  assert.equal(empty.assignment, "", "no assignment -> empty string");
  assert.equal(empty.specialty, "");
  assert.equal(empty.objective, "");
  assert.deepEqual(empty.recentTasks, [], "no permalog -> no tasks");
  assert.deepEqual(empty.evaluations, []);
  assert.deepEqual(empty.evalCriteria, []);
  assert.doesNotThrow(() => dash.agentCardView(null), "null tab must not throw");
}

// ============================ Rouge 2 — roundsPill ============================
assert.equal(typeof dash.roundsPill, "function", "Inc8-S3 RED: export roundsPill(tab) from dashboard.js");
{
  const on = dash.roundsPill({ roundsActive: { active: true, lastRoundAt: 123 } });
  assert.equal(on.active, true, "roundsActive.active true -> active");
  assert.match(on.cls, /rounds-on/, "active -> the GREEN pill class");
  const off = dash.roundsPill({ roundsActive: { active: false } });
  assert.equal(off.active, false);
  assert.match(off.cls, /rounds-off/, "inactive -> the GREY pill class");
  // Absent roundsActive -> grey (idle), null-safe.
  assert.equal(dash.roundsPill({}).active, false, "no roundsActive -> not active");
  assert.match(dash.roundsPill({}).cls, /rounds-off/, "no roundsActive -> grey");
  assert.doesNotThrow(() => dash.roundsPill(null), "null tab must not throw");
  assert.match(dash.roundsPill(null).cls, /rounds-off/);
}

// ============================ Inc9 (2) — clipWords (detailed popup 'voir plus') ============================
assert.equal(typeof dash.clipWords, "function", "Inc9-(2): export clipWords(text, max) from dashboard.js");
{
  const short = dash.clipWords("a b c", 50);
  assert.equal(short.clipped, false, "≤ max words -> not clipped");
  assert.equal(short.text, "a b c", "short text passes through verbatim");
  const words = Array.from({ length: 60 }, (_, i) => `w${i}`).join(" ");
  const long = dash.clipWords(words, 50);
  assert.equal(long.clipped, true, "> max words -> clipped");
  assert.match(long.text, /…$/, "clipped text ends with an ellipsis");
  assert.equal(long.text.replace(/…$/, "").trim().split(/\s+/).length, 50, "clipped to exactly 50 words (ellipsis glued)");
  assert.equal(long.full, words, "the FULL text is preserved for 'voir plus'");
  assert.equal(dash.clipWords(null).clipped, false, "null -> not clipped, no throw");
  assert.equal(dash.clipWords("").text, "", "empty -> empty");
}

// ============================ Rouge 3 — méta-trio in the Méta band ============================
// The Méta band now holds the meta-TRIO: tichef (role "manager") + Brain + aligator
// (daemons, orchestrator="meta", cards filled by the tichef). Live shape: daemons
// listed in the méta project's unmapped bucket.
{
  const card = { specialty: "s", objective: "o", currentTaskLog: ["t"] };
  const tichef = { id: "tc", name: "ta-tichef", role: "manager", orchestrator: "meta", ...card };
  const brain = { id: "brain", name: "⛑ brain", agent_kind: "brain", role: "", orchestrator: "meta", ...card };
  const aligator = { id: "alig", name: "🐊 aligator", agent_kind: "aligator", role: "", orchestrator: "meta", ...card };
  const state = {
    nodes: [], unmapped: [], unassigned: [],
    projects: [{ name: "méta", isMeta: true, orchestrators: [], nodes: [], unmapped: [tichef, brain, aligator] }],
  };
  const bl = dash.bandLayout(state);
  assert.deepEqual(bl.meta.map((t) => t.id).sort(), ["alig", "brain", "tc"], "Méta band = the meta-trio (tichef + Brain + aligator)");
  // The daemons must NOT leak into Freelancers/Workers.
  assert.equal(bl.freelancers.filter((t) => t.id === "brain" || t.id === "alig").length, 0, "daemons are not Freelancers");
}

// Inc8.1 (2): the LIVE shape — a trio daemon carries NEITHER orchestrator="meta"
// NOR agent_kind: its ONLY meta signal is the `meta/…` assignment (aligator =
// meta/router, kind=None). Regression guard: such a tab must land in the Méta band,
// not leak into Workers (the bug the PO saw: aligator absent from Méta).
{
  const aligLive = { id: "alig2", name: "🐊 aligator", role: "", assignment: "meta/router" };
  const scribeLive = { id: "scr", name: "ta-scribe", role: "", assignment: "meta/scribe" };
  const bl = dash.bandLayout({ nodes: [], unmapped: [aligLive, scribeLive], unassigned: [], projects: [] });
  // Inc9 (4): meta/router = one of the 3 autonomous daemons -> Méta; meta/scribe is
  // an on-demand supporter -> Supporters band (NOT Méta, NOT Workers).
  assert.deepEqual(bl.meta.map((t) => t.id).sort(), ["alig2"], "Inc8.1/9: meta/router (trio) -> Méta band");
  assert.deepEqual(bl.supporters.map((t) => t.id).sort(), ["scr"], "Inc9 (4): meta/scribe -> Supporters band");
  assert.equal(bl.workers.filter((t) => t.id === "alig2").length, 0, "Inc8.1: a meta/router tab does NOT leak into Workers");
  // A meta specialist REINFORCING a team (project:role assignment) stays a worker.
  assert.equal(dash.resolveAltitude({ role: "refiner", assignment: "kalpin-back:build/refiner" }).band, "worker",
    "Inc8.1: a reinforcing specialist (project:role) is NOT pulled into Méta");
}

// Inc9 (4): the SUPPORTER band — the 3 autonomous daemons stay Méta; every other
// meta-class agent (guardian/foreman/sage/scribe/coverage/scout/auditor) is an
// on-demand SUPPORTER in its own band. Live assignments from the real fleet.
{
  const trio = [
    { id: "tc", name: "tichef", assignment: "meta/manager" },
    { id: "brn", name: "Brian", agent_kind: "brain", assignment: "meta/brain" },
    { id: "alg", name: "aligator", assignment: "meta/router" },
  ];
  const supporters = [
    { id: "jo", name: "Joséphine", assignment: "meta/guardian" },
    { id: "hf", name: "Henri Ford", assignment: "meta/foreman" },
    { id: "sg", name: "le sage", assignment: "meta/sage" },
    { id: "sc", name: "ta-scribe", assignment: "meta/scribe" },
    { id: "cv", name: "Bot Coverage", assignment: "meta/coverage" },
    { id: "st", name: "Bot Scout", assignment: "meta/scout" },
    { id: "au", name: "ta-convention-auditor", assignment: "scope/auditor" }, // NOT meta/*, still a supporter
  ];
  const bl = dash.bandLayout({ nodes: [], unmapped: [...trio, ...supporters], unassigned: [], projects: [] });
  assert.deepEqual(bl.meta.map((t) => t.id).sort(), ["alg", "brn", "tc"], "Inc9 (4): Méta band = the 3 autonomous daemons ONLY");
  assert.deepEqual(bl.supporters.map((t) => t.id).sort(), ["au", "cv", "hf", "jo", "sc", "sg", "st"],
    "Inc9 (4): the 7 on-demand supporters land in the Supporters band");
  // Supporters are NOT under tichef (no orchestrator chain) and NOT freelancers.
  assert.equal(bl.freelancers.filter((t) => supporters.some((s) => s.id === t.id)).length, 0, "Inc9 (4): supporters are not Freelancers");
  assert.equal(bl.workers.filter((t) => supporters.some((s) => s.id === t.id)).length, 0, "Inc9 (4): idle supporters are not Workers");
}

// Inc9 (précédence supporter > parentTabId) : un supporter fleet-wide qui a une ARÊTE
// parent (dispatch-dérivée) vers un orchestrateur doit RESTER en bande Supporters, PAS
// nicher comme son worker. + Olympe (review/evaluator) est un supporter.
{
  assert.equal(dash.resolveAltitude({ assignment: "review/evaluator" }).band, "supporter",
    "Inc9: review/evaluator (Olympe) -> Supporters band");
  const lead = { id: "L", name: "ta-lead", role: "orchestrator", assignment: "repoA:build/orchestrator" };
  // supporters AVEC une arête parent (parentTabId=L) et/ou orchestrator=L vers le lead :
  const supWithParent = { id: "jo", name: "Joséphine", assignment: "meta/guardian", parentTabId: "L" };
  const supWithOrch = { id: "ol", name: "Olympe", assignment: "review/evaluator", orchestrator: "L" };
  const realWorker = { id: "w", name: "ta-w", role: "implementer", assignment: "repoA:build/impl", parentTabId: "L" };
  const bl = dash.bandLayout({ nodes: [], unmapped: [lead, supWithParent, supWithOrch, realWorker], unassigned: [], projects: [] });
  assert.deepEqual(bl.supporters.map((t) => t.id).sort(), ["jo", "ol"],
    "Inc9: les supporters à arête parent restent en Supporters band");
  const underL = bl.orchestrators.find((o) => o.lead.id === "L").repos.flatMap((r) => r.workers.map((x) => x.id));
  assert.deepEqual(underL, ["w"], "Inc9: seul le VRAI worker niche sous le lead (les supporters ont précédence)");
}

console.log("dashboard.inc8.s3.test.mjs: OK");
