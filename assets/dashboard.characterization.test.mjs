// Characterization coverage — dashboard quality audit #357, PHASE A.
// Run: node assets/dashboard.characterization.test.mjs   (must stay GREEN)
// These lock the CURRENT behavior of the pure dashboard functions the web
// builder is about to refactor (Q3-Q10), so any behavior change is caught. They
// assert what the code ACTUALLY produces today — not an ideal. QUE assets/dashboard.*.
import assert from "node:assert/strict";
import {
  projectTabs, overviewLayout, resolveAltitude, serviceLayout, serviceGrouping,
  metaTopHtml, teamMemberHtml, unassignedTabHtml, popupHtml, tabEntryHtml,
} from "./dashboard.js";

// ===== projectTabs — the "all project tabs" flatten (nodes[].tabs then unmapped[]) =====
{
  const p = { nodes: [{ tabs: [{ id: "a" }] }, { tabs: [{ id: "b" }, { id: "c" }] }], unmapped: [{ id: "d" }] };
  assert.deepEqual(projectTabs(p).map((t) => t.id), ["a", "b", "c", "d"], "nodes tabs (in order) then unmapped");
  assert.deepEqual(projectTabs(null), [], "null project -> []");
  assert.deepEqual(projectTabs({}), [], "empty project -> []");
  assert.deepEqual(projectTabs({ nodes: [{}], unmapped: null }), [], "missing tabs/unmapped -> []");
}

// ===== overviewLayout — order / meta / repos(.orchestrators/.tree) / unassigned =====
{
  const state = {
    projects: [
      { name: "méta", isMeta: true },
      { name: "kb", isMeta: false, orchestrators: [{ id: "o1" }, { id: "o2" }] }, // tree (>1)
      { name: "kf", isMeta: false, orchestrators: [{ id: "o3" }] },               // not a tree
      { name: "solo", isMeta: false },                                            // no orchestrators
    ],
    unassigned: [{ id: "u1" }],
  };
  const o = overviewLayout(state);
  assert.deepEqual(o.order, ["META", "REPOS", "UNASSIGNED"], "fixed 3-slot order");
  assert.deepEqual(o.meta.map((p) => p.name), ["méta"], "meta = isMeta projects");
  assert.deepEqual(o.repos.map((r) => r.name), ["kb", "kf", "solo"], "repos = non-meta, server order");
  assert.equal(o.repos.find((r) => r.name === "kb").tree, true, ">1 orchestrator -> tree true");
  assert.equal(o.repos.find((r) => r.name === "kf").tree, false, "1 orchestrator -> tree false");
  assert.equal(o.repos.find((r) => r.name === "solo").tree, false, "0 orchestrator -> tree false");
  assert.deepEqual(o.repos.find((r) => r.name === "solo").orchestrators, [], "missing orchestrators -> [] (defaulted)");
  assert.deepEqual(o.unassigned.map((t) => t.id), ["u1"], "unassigned passthrough");
  // Malformed input never throws; defaults hold.
  assert.deepEqual(overviewLayout(null).order, ["META", "REPOS", "UNASSIGNED"]);
  assert.deepEqual(overviewLayout(null).repos, []);
  assert.deepEqual(overviewLayout({}).unassigned, []);
}

// ===== resolveAltitude — the 4 movements + tichef pin, and the `state` param is IGNORED =====
{
  // tichef pinned to Méta even while serving.
  assert.deepEqual(resolveAltitude({ role: "tichef", serving: "kb" }), { band: "meta" });
  // A meta specialist SERVING -> the team band, marked reinforcement (band is "worker" today).
  assert.deepEqual(resolveAltitude({ role: "planner", serving: "kb" }), { band: "worker", team: "kb", reinforcement: true });
  // A meta specialist with an assignment OVERRIDE (no serving) -> same, team from the override.
  assert.deepEqual(resolveAltitude({ role: "refiner", assignment: "kb:plan/refiner" }), { band: "worker", team: "kb", reinforcement: true });
  // A solo meta specialist -> Méta.
  assert.deepEqual(resolveAltitude({ role: "auditor" }), { band: "meta" });
  // Orchestrator -> Orchestrateurs; team = override or null.
  assert.deepEqual(resolveAltitude({ role: "orchestrator", assignment: "kb:build/orchestrator" }), { band: "orchestrator", team: "kb" });
  assert.deepEqual(resolveAltitude({ role: "orchestrator", assignment: "build/orchestrator" }), { band: "orchestrator", team: null });
  // Any assigned non-meta -> Workers, team from override.
  assert.deepEqual(resolveAltitude({ role: "implementer", assignment: "kb:build/implementer" }), { band: "worker", team: "kb" });
  assert.deepEqual(resolveAltitude({ role: "implementer", assignment: "build/implementer" }), { band: "worker", team: null });
  // Non-assigned -> Freelancers. Null tab -> freelancer.
  assert.deepEqual(resolveAltitude({ role: "worker" }), { band: "freelancer" });
  assert.deepEqual(resolveAltitude(null), { band: "freelancer" });
  // The 2nd param (state) is currently IGNORED: identical result with/without/garbage state.
  const tab = { role: "implementer", assignment: "kb:build/implementer" };
  assert.deepEqual(resolveAltitude(tab), resolveAltitude(tab, { projects: [{ name: "kb" }] }), "state ignored (with state)");
  assert.deepEqual(resolveAltitude(tab), resolveAltitude(tab, null), "state ignored (null)");
  assert.deepEqual(resolveAltitude(tab), resolveAltitude(tab, "garbage"), "state ignored (garbage)");
}

// ===== serviceGrouping — the ternary wrapper delegating to serviceLayout =====
{
  const state = { services: [
    { name: "kalpin", rollupLed: "error", projects: ["kb", "kf"] },
    { name: "solo", projects: ["solo"] },
  ] };
  assert.deepEqual(serviceGrouping(state), serviceLayout(state), "serviceGrouping === serviceLayout (fn present)");
  const g = serviceGrouping(state);
  assert.equal(g[0].service, "kalpin");
  assert.equal(g[0].rollupLed, "error");
  assert.deepEqual(g[0].repos.map((r) => r.name), ["kb", "kf"], "string projects mapped to {name}");
  assert.equal(g[0].mono, false, ">1 repo -> not mono");
  assert.equal(g[1].mono, true, "1 repo -> mono (repos.length <= 1)");
  assert.equal(serviceGrouping({ services: [{ name: "empty", projects: [] }] })[0].mono, true, "0 repos -> mono too");
  assert.deepEqual(serviceGrouping({}), [], "no services -> []");
  assert.deepEqual(serviceGrouping(null), [], "null -> []");
}

// ===== HTML helpers — metaTopHtml / teamMemberHtml / unassignedTabHtml =====
{
  const m = metaTopHtml({ name: "ta-planner", viewerUrl: "/v" });
  assert.match(m, /class="meta-top-tab"/, "meta-top-tab class");
  assert.match(m, /data-viewer="\/v"/, "carries the viewer url");
  assert.match(m, />ta-planner</, "shows the name");
  assert.match(metaTopHtml({}), />méta</, "fallback name 'méta'");
  assert.match(metaTopHtml({ name: "<x>" }), /&lt;x&gt;/, "name is HTML-escaped");

  const tm = teamMemberHtml({ name: "w1", viewerUrl: "/w" }, "worker");
  assert.match(tm, /class="worker"/, "uses the passed class");
  assert.match(tm, /data-viewer="\/w"/);
  assert.match(tm, />w1</);
  assert.match(teamMemberHtml({}, "team-lead"), /class="team-lead"/, "class passthrough");
  assert.match(teamMemberHtml({}, "worker"), />tab</, "fallback name 'tab'");

  const u = unassignedTabHtml({ name: "free", viewerUrl: "/u" });
  assert.match(u, /class="unassigned-tab"/);
  assert.match(u, /data-viewer="\/u"/);
  assert.match(u, />free</);
  assert.match(unassignedTabHtml({}), />tab</, "fallback name 'tab'");
  assert.match(unassignedTabHtml(null), /class="unassigned-tab"/, "null-safe");
}

// ===== popup logic — popupHtml + tabEntryHtml =====
{
  const tab = { id: "t1", name: "ta-x", role: "implementer", item: "wiring", agentState: "thinking", tokens: { input: 5, output: 2 }, viewerUrl: "/v1" };
  const html = popupHtml("build", "working", [tab]);
  assert.match(html, /class="popup-title"/);
  assert.match(html, /build · working/, "title · led");
  assert.match(html, /class="popup-tabs"/);
  assert.match(html, /class="popup-tab"/);
  assert.match(html, /data-viewer="\/v1"/);
  assert.match(html, /tab-name">ta-x</);
  assert.match(html, /tab-role">implementer</);
  assert.match(html, /tab-item">wiring</);
  assert.match(html, /tab-state">thinking</);
  assert.match(html, /class="tab-tokens"/);
  assert.match(html, /right-click a tab to open its viewer/, "the hint line");
  // led fallback + empty list.
  assert.match(popupHtml("x", null, []), /x · —/, "null led -> em dash");
  // An orchestrator occupant gets the tint class on its entry.
  assert.match(popupHtml("x", "idle", [{ id: "o", name: "o", role: "orchestrator", viewerUrl: "" }]), /class="popup-tab orchestrator"/);
  // tabEntryHtml directly (the popup building block), usage=null is tolerated.
  const entry = tabEntryHtml({ id: "t", name: "n", role: "worker", item: "i", agentState: "waiting", tokens: { input: 1, output: 1 }, viewerUrl: "/e" }, null);
  assert.match(entry, /class="popup-tab"/);
  assert.match(entry, /tab-name">n</);
  assert.match(entry, /data-viewer="\/e"/);
}

console.log("dashboard.characterization.test.mjs: OK");
