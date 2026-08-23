// Self-check for the pure logic of the dashboard web app.
// Run: node assets/dashboard.test.mjs  (exits non-zero if the mapping breaks)
// No framework — just node:assert. Imports the real functions from dashboard.js
// so this stays a check of shipped code, not a copy.
import assert from "node:assert/strict";
import {
  ledClass, nodeMap, CANONICAL_PHASES, resolveView, renderProjectCard,
  readProjectParam, shortContext, nodeSubtitle,
  isOrchestrator, roleAltitude, projectAltitude, lineageEdges,
  viewerUrlWithToken,
  REHOME_STATES, rehomeStep, rehomePairs, rehomePairHtml,
} from "./dashboard.js";

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

// --- resolveView: level-0 grid vs scoped/legacy diagram (S2/S3) ---
// No projects[] (pre-S1 / legacy) -> the global diagram, whatever currentProject.
{
  const legacy = { nodes: [{ id: "build", rollupLed: "working", tabs: [] }], unmapped: [] };
  const v = resolveView(legacy, null);
  assert.equal(v.mode, "diagram");
  assert.equal(v.scoped, false);
  assert.equal(v.nodes.length, 1);
  // A stray currentProject can't conjure a grid when the server sends no projects.
  assert.equal(resolveView(legacy, "kalpin-back").mode, "diagram");
}
// projects[] present, none drilled -> grid, in server order (no re-sort).
{
  const state = {
    projects: [
      { name: "kalpin-back", tabCount: 2, rollupLed: "working", nodes: [{ id: "build", rollupLed: "working", tabs: [] }] },
      { name: "méta", tabCount: 1, rollupLed: "idle", isMeta: true, nodes: [] },
    ],
    nodes: [], unmapped: [],
  };
  const grid = resolveView(state, null);
  assert.equal(grid.mode, "grid");
  assert.deepEqual(grid.projects.map((p) => p.name), ["kalpin-back", "méta"]);
  // Drill into a known project -> scoped diagram with that project's nodes.
  const drill = resolveView(state, "kalpin-back");
  assert.equal(drill.mode, "diagram");
  assert.equal(drill.scoped, true);
  assert.equal(drill.nodes[0].id, "build");
  // Unknown selected project -> empty scoped diagram, not an error/throw.
  const unknown = resolveView(state, "nope");
  assert.equal(unknown.mode, "diagram");
  assert.equal(unknown.scoped, true);
  assert.deepEqual(unknown.nodes, []);
}

// --- renderProjectCard: name, led class, count, meta + orchestrator markers ---
{
  const id = (s) => s; // identity escaper for the test
  const card = renderProjectCard(
    { name: "kalpin-back", tabCount: 3, rollupLed: "error", hasOrchestrator: true },
    id
  );
  assert.match(card, /project-card led-error/, "card carries its led class");
  assert.match(card, /data-project="kalpin-back"/, "card carries its project name for drill-in");
  assert.match(card, /3 tabs/, "card shows the tab count (plural)");
  assert.match(card, /orch-badge/, "card shows the orchestrator badge");
  // meta lane + neutral led + singular count.
  const metaCard = renderProjectCard({ name: "méta", tabCount: 1, rollupLed: null, isMeta: true }, id);
  assert.match(metaCard, /project-card led-neutral meta/, "meta card is neutral + meta");
  assert.match(metaCard, /1 tab</, "singular count has no plural s");
  assert.doesNotMatch(metaCard, /orch-badge/, "no orchestrator badge when absent");
}

// --- readProjectParam: deep-link ?project= -> drilled project (S3) ---
assert.equal(readProjectParam("?project=kalpin-back"), "kalpin-back");
assert.equal(readProjectParam(""), null, "no query -> level 0");
assert.equal(readProjectParam("?token=abc"), null, "other params -> level 0");
assert.equal(readProjectParam("?project="), null, "empty value -> level 0");

// --- shortContext / nodeSubtitle: node subtitles = context ~5 words (S4) ---
assert.equal(shortContext("refactor the auth token flow now", 5), "refactor the auth token flow…", "clips to 5 words with ellipsis");
assert.equal(shortContext("just three words here"), "just three words here", "under the cap -> verbatim");
assert.equal(shortContext(""), "", "empty context -> empty");
assert.equal(shortContext(null), "", "null context -> empty");
// Empty node -> no subtitle.
assert.equal(nodeSubtitle({ tabs: [] }), "");
// One tab -> name · context.
assert.equal(nodeSubtitle({ tabs: [{ name: "ta", context: "do a thing" }] }), "ta · do a thing");
// Multiple tabs -> first + "+N".
assert.match(nodeSubtitle({ tabs: [{ name: "a", context: "x" }, {}, {}] }), /\+2$/, "multi-tab node shows +N");

// --- S5 orchestrator tint + card marker ---
assert.equal(isOrchestrator("orchestrator"), true);
assert.equal(isOrchestrator("Orchestrator"), true, "case-insensitive");
assert.equal(isOrchestrator("worker"), false);
assert.equal(isOrchestrator(null), false);
{
  const id = (s) => s;
  const card = renderProjectCard({ name: "kalpin-back", tabCount: 2, rollupLed: "idle", hasOrchestrator: true }, id);
  assert.match(card, /project-card led-idle orchestrator/, "orchestrator project card carries the tint class");
  const plain = renderProjectCard({ name: "x", tabCount: 1, rollupLed: "idle" }, id);
  assert.doesNotMatch(plain, /orchestrator/, "no tint without an orchestrator");
}

// --- S6 altitude bands: derived from ROLE, not phase/server altitude (finding a) ---
assert.equal(roleAltitude("tichef"), 0);
assert.equal(roleAltitude("orchestrator"), 1);
assert.equal(roleAltitude(" Orchestrator "), 1, "trims + case-insensitive");
assert.equal(roleAltitude("implementer"), 2, "workers/specialists at the bottom band");
// A meta-lane orchestrator lands in the orchestrator band, NEVER the tichef band.
assert.equal(
  projectAltitude({ isMeta: true, nodes: [{ tabs: [{ role: "orchestrator" }, { role: "planner" }] }] }),
  1,
  "meta-lane orchestrator -> band 1 (not tichef band 0)"
);
// A server-provided altitude is IGNORED — role is authoritative.
assert.equal(
  projectAltitude({ altitude: 0, nodes: [{ tabs: [{ role: "orchestrator" }] }] }),
  1,
  "phase/server altitude ignored; role wins"
);
// The most senior occupant sets the band.
assert.equal(
  projectAltitude({ nodes: [{ tabs: [{ role: "worker" }, { role: "orchestrator" }] }] }),
  1,
  "an orchestrator in the project lifts it to band 1"
);
assert.equal(projectAltitude({ nodes: [] }), 2, "empty project -> worker band");

// --- S6 lineage edges (cross-project delegation only, deduped) ---
{
  const projects = [
    { name: "méta", nodes: [{ tabs: [{ id: "orch1", role: "orchestrator" }] }], unmapped: [] },
    { name: "kalpin-back", nodes: [{ tabs: [
      { id: "w1", role: "worker", parentTabId: "orch1" },
      { id: "w2", role: "worker", parentTabId: "orch1" }, // same parent+project -> deduped
      { id: "w3", role: "worker", parentTabId: "w1" },    // intra-project -> not an inter-card edge
    ] }], unmapped: [] },
  ];
  const edges = lineageEdges(projects);
  assert.deepEqual(edges, [{ from: "méta", to: "kalpin-back" }], "one deduped cross-project delegation edge");
  assert.deepEqual(lineageEdges([]), [], "no projects -> no edges");
  assert.deepEqual(lineageEdges(null), [], "malformed input -> no edges");
}

// --- viewer open carries the page share-token (fix-viewer-token) ---
// The right-click open must append the page token so the viewer routes (now
// authorised by the read-only dashboard token) don't 401. Host stays relative.
assert.equal(
  viewerUrlWithToken("/tabs/by-id/abc/view", "dash-obs"),
  "/tabs/by-id/abc/view?token=dash-obs",
  "appends ?token= to a token-less viewer url"
);
assert.equal(
  viewerUrlWithToken("/tabs/by-id/abc/view?ro=1", "d&x"),
  "/tabs/by-id/abc/view?ro=1&token=d%26x",
  "uses & when a query already exists, and encodes the token"
);
assert.equal(viewerUrlWithToken("/x/view", ""), "/x/view", "no token -> url unchanged");
assert.equal(viewerUrlWithToken("", "t"), "", "no url -> empty");

// --- Slice C: re-home predecessor -> successor pairs + progress ---
assert.deepEqual(REHOME_STATES, ["handoff-written", "successor-ready", "ack-sent", "safe-to-close"]);
assert.equal(rehomeStep("handoff-written"), 0);
assert.equal(rehomeStep("safe-to-close"), 3);
assert.equal(rehomeStep("bogus"), -1, "unknown -> -1");
assert.equal(rehomeStep(null), -1, "none -> -1");
{
  const tabs = [
    { id: "old1", name: "team titour (old)", rehomeStatus: "ack-sent" },
    { id: "new1", name: "team titour", parentTabId: "old1" }, // successor
    { id: "solo", name: "just-delegated", parentTabId: "old1" === "x" ? "x" : "unrelated" }, // not a rehome parent
    { id: "old2", name: "predecessor-no-succ", rehomeStatus: "handoff-written" }, // no successor yet
    { id: "plain", name: "no rehome" },
  ];
  const pairs = rehomePairs(tabs);
  assert.equal(pairs.length, 2, "only tabs with a rehomeStatus are predecessors");
  const ack = pairs.find((p) => p.predecessor.id === "old1");
  assert.equal(ack.successor.id, "new1", "successor found via parentTabId");
  assert.equal(ack.step, 2, "ack-sent is step 2");
  const pending = pairs.find((p) => p.predecessor.id === "old2");
  assert.equal(pending.successor, null, "no successor linked yet -> null");
  assert.deepEqual(rehomePairs([]), [], "no tabs -> no pairs");
  assert.deepEqual(rehomePairs(null), [], "malformed -> no pairs");
  // HTML: names, arrow, status badge, safe class, 4 progress dots (filled to step).
  const id = (s) => s;
  const html = rehomePairHtml(pairs.find((p) => p.predecessor.id === "old1"), id);
  assert.match(html, /team titour \(old\)/);
  assert.match(html, /→/);
  assert.match(html, /data-status="ack-sent"/);
  assert.equal((html.match(/rehome-dot/g) || []).length, 4, "always four progress dots");
  assert.equal((html.match(/rehome-dot on/g) || []).length, 3, "filled up to and including the current step");
  const safeHtml = rehomePairHtml({ predecessor: { name: "o" }, successor: { name: "n" }, status: "safe-to-close" }, id);
  assert.match(safeHtml, /rehome-pair safe/, "safe-to-close marks the pair safe");
  const pendingHtml = rehomePairHtml(pending, id);
  assert.match(pendingHtml, /\(successor pending\)/, "no successor -> pending label");
}

console.log("dashboard.test.mjs: OK");
