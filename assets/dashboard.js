// Harness control panel. Slice S3. See docs/dashboard.md.
// Polls GET /dashboard/state every ~1.5s and renders the phase diagram.
// ES module: the pure functions are exported so assets/dashboard.test.mjs can
// import them under Node. The DOM bootstrap at the bottom is guarded so the
// import stays side-effect-free off-browser.
"use strict";

// Canonical phase node ids, in flow order (docs/dashboard.md "Phase nodes").
export const CANONICAL_PHASES = ["scope", "plan", "build", "review", "verify", "sweep", "done"];

// The five synthesized led states a tab/node can carry (docs "led rollup").
const LED_STATES = ["dead", "error", "working", "unreviewed", "idle"];

// Pure: a node's rollupLed -> its CSS highlight class.
// The five known leds -> `led-<state>`; null / unknown (empty node) -> neutral.
// Never throws — a garbage value degrades to neutral rather than a broken class.
export function ledClass(led) {
  return LED_STATES.includes(led) ? `led-${led}` : "led-neutral";
}

// Pure: the legend entries (S1) — one swatch per led state, the orchestrator
// accent, and the delegation (lineage) arrow. `cls` is the CSS class that paints
// the swatch, so a GUI test can match swatch colour against the node using it.
export function legendModel() {
  return [
    { cls: "led-working", label: "working" },
    { cls: "led-error", label: "error" },
    { cls: "led-unreviewed", label: "unreviewed" },
    { cls: "led-idle", label: "idle" },
    { cls: "led-dead", label: "dead" },
    { cls: "led-neutral", label: "empty" },
    { cls: "orchestrator", label: "orchestrator" },
    { cls: "lineage-edge", label: "delegation" },
  ];
}

// Pure: /dashboard/activity payload -> the panel model (S4). Five headline
// figures + three per-day series (one point per day) + summary lines + the PO
// benchmark record. Absent/empty/null payload -> zeros + empty series, no throw.
export function activityModel(json) {
  const j = json || {};
  const totals = j.totals || {};
  const perDay = Array.isArray(j.per_day) ? j.per_day : [];
  const num = (v) => Number(v || 0);
  // Inc6: the maturity/growth verdict (falsy when absent).
  const verdictDetail = j.self_improvement_verdict || null;
  return {
    windowHours: num(j.window_hours),
    features: num(totals.features_implemented),
    // Inc6: separate counters — features stays UNDIVIDED; these are distinct.
    fixes: num(totals.fixes),
    selfTooling: num(totals.self_tooling),
    issuesOpened: num(totals.issues_opened),
    issuesClosed: num(totals.issues_closed),
    tokensPerFeature: num(totals.tokens_per_feature),
    minutesSinceLastHumanPrompt: num(totals.minutes_since_last_human_prompt),
    aligatorCalls: num(totals.aligator_calls),
    humanPrompts: num(totals.human_prompts),
    tokensTotal: totals.tokens_total || {},
    days: perDay.map((d) => (d && d.date) || ""),
    series: {
      features: perDay.map((d) => num(d && d.features)),
      fixes: perDay.map((d) => num(d && d.fixes)),
      selfTooling: perDay.map((d) => num(d && d.self_tooling)),
      tokensPerFeature: perDay.map((d) => num(d && d.tokens_per_feature)),
      autonomy: perDay.map((d) => num(d && d.autonomy_minutes_max)),
    },
    summaryLines: Array.isArray(j.summary_lines) ? j.summary_lines : [],
    record: j.record || null,
    verdict: (verdictDetail && verdictDetail.verdict) || "",
    verdictDetail,
  };
}

// Pure: /dashboard/state -> Map<phaseId, node>. Defensive against a missing or
// malformed `nodes` array so a bad poll never wipes the diagram with a throw.
export function nodeMap(state) {
  const map = new Map();
  const nodes = state && Array.isArray(state.nodes) ? state.nodes : [];
  for (const node of nodes) {
    if (node && typeof node.id === "string") map.set(node.id, node);
  }
  return map;
}

// Pure: decide what to render from (state, currentProject). Three modes:
//   - "grid"    : level 0, the project cards (state.projects present, none drilled)
//   - "diagram" : the 7-phase flow — scoped to a project when one is selected,
//                 or the legacy GLOBAL diagram when the server predates the
//                 project dimension (no state.projects). An unknown selected
//                 project yields an empty scoped diagram, never an error.
// Keeping this pure means the view choice is unit-testable without a DOM.
export function resolveView(state, currentProject) {
  const projects = state && Array.isArray(state.projects) ? state.projects : [];
  if (!projects.length) {
    // Pre-S1 / legacy contract: no project dimension -> the global diagram.
    return {
      mode: "diagram",
      scoped: false,
      nodes: (state && state.nodes) || [],
      unmapped: (state && state.unmapped) || [],
    };
  }
  if (currentProject == null) {
    return { mode: "grid", projects };
  }
  const project = projects.find((p) => p && p.name === currentProject) || null;
  return {
    mode: "diagram",
    scoped: true,
    project,
    nodes: project ? project.nodes || [] : [],
    unmapped: project ? project.unmapped || [] : [],
  };
}

// Pure: a project -> its level-0 card HTML. Rendered in server order (no re-sort)
// so positions stay put across reloads. `esc` is injected so this stays free of
// DOM globals and importable under Node (the self-check passes the same escaper).
export function renderProjectCard(project, esc) {
  const name = project && project.name != null ? String(project.name) : "?";
  const led = ledClass(project && project.rollupLed);
  const meta = project && project.isMeta ? " meta" : "";
  const hasOrch = project && project.hasOrchestrator;
  const orchCls = hasOrch ? " orchestrator" : "";
  const orch = hasOrch
    ? ` <span class="orch-badge" title="has an orchestrator">◆</span>`
    : "";
  const count = Number((project && project.tabCount) || 0);
  // S6: name each orchestrator under the repo; a repo with >1 renders a tree.
  const orchestrators = Array.isArray(project && project.orchestrators) ? project.orchestrators : [];
  const orchList = orchestrators.length
    ? `<span class="orch-list${orchestrators.length > 1 ? " orch-tree" : ""}">${orchestrators
        .map((o) => `<span class="orch-name" title="${esc((o && o.item) || "")}">${esc((o && o.name) || "orchestrator")}</span>`)
        .join("")}</span>`
    : "";
  return `<button class="project-card ${led}${meta}${orchCls}" data-project="${esc(name)}">
    <span class="card-name">${esc(name)}${orch}</span>
    <span class="card-count">${count} tab${count === 1 ? "" : "s"}</span>
    ${orchList}
  </button>`;
}

// Pure: the drilled project from a URL query string (`?project=<name>`), or null
// for the level-0 grid. Deep-links open straight into a project.
export function readProjectParam(search) {
  const value = new URLSearchParams(search || "").get("project");
  return value ? value : null;
}

// Pure: first `maxWords` words of a context string, with an ellipsis if clipped.
// context is the volatile prompt ("what this tab is on right now") — the "five
// words" of docs/dashboard.md.
export function shortContext(text, maxWords = 5) {
  const words = String(text == null ? "" : text).trim().split(/\s+/).filter(Boolean);
  if (!words.length) return "";
  const head = words.slice(0, maxWords).join(" ");
  return words.length > maxWords ? head + "…" : head;
}

// Pure: a node's on-diagram subtitle = first occupant's name + short context,
// with a "+N" tail when the node holds more than one tab. Capped so it fits the
// node box. Empty when the node has no tabs.
export function nodeSubtitle(node) {
  const tabs = (node && node.tabs) || [];
  if (!tabs.length) return "";
  const first = tabs[0] || {};
  const name = first.name || "";
  const ctx = shortContext(first.context || first.item);
  let label = ctx ? `${name} · ${ctx}` : name;
  if (label.length > 24) label = label.slice(0, 23) + "…";
  return tabs.length > 1 ? `${label} +${tabs.length - 1}` : label;
}

// --- S5/S6: orchestrator tint + altitude bands + delegation lineage ---
// These consume role / parentTabId / (optional) altitude fields exposed by the
// Rust builder. ponytail: the altitude/lineage contract is provisional until the
// Rust S6 slice lands — everything below degrades cleanly when the fields are
// absent (one band, no edges), so today's fixtures render fine.

export function isOrchestrator(role) {
  return String(role || "").toLowerCase() === "orchestrator";
}

// Pure: an agent ROLE -> its altitude band (0 = highest). Three bands per the
// plan: tichef atop, orchestrators below, workers/specialists at the bottom.
// Keyed strictly on the role, NOT the phase (tichef finding): a meta-lane
// orchestrator (role "orchestrator", any phase) must land in the orchestrator
// band, never the tichef band.
export function roleAltitude(role) {
  const r = String(role || "").trim().toLowerCase();
  if (r === "tichef") return 0;
  if (r === "orchestrator") return 1;
  return 2;
}

// Every tab of a project, across its phase nodes and its unmapped bucket.
function projectTabs(project) {
  const out = [];
  for (const n of (project && project.nodes) || []) for (const t of (n && n.tabs) || []) out.push(t);
  for (const t of (project && project.unmapped) || []) out.push(t);
  return out;
}

// Pure: a project's altitude band = the most senior agent working in it (lowest
// roleAltitude). Derived from ROLE only — a server-provided `altitude` (which
// may be phase-influenced) is deliberately NOT trusted here (tichef finding).
// Empty project -> worker band (2).
export function projectAltitude(project) {
  const tabs = projectTabs(project);
  if (!tabs.length) return 2;
  return Math.min(...tabs.map((t) => roleAltitude(t && t.role)));
}

// Pure: cross-project delegation edges from parentTabId links. An edge
// {from: parentProject, to: childProject} is emitted when a tab's parent lives in
// a DIFFERENT project (intra-project links aren't drawn between cards). Deduped.
export function lineageEdges(projects) {
  const list = Array.isArray(projects) ? projects : [];
  const owner = new Map();
  for (const p of list) for (const t of projectTabs(p)) if (t && t.id) owner.set(t.id, p.name);
  const edges = [];
  const seen = new Set();
  for (const p of list) {
    for (const t of projectTabs(p)) {
      if (!t || !t.parentTabId) continue;
      const from = owner.get(t.parentTabId);
      if (!from || from === p.name) continue;
      const key = `${from} ${p.name}`;
      if (seen.has(key)) continue;
      seen.add(key);
      edges.push({ from, to: p.name });
    }
  }
  return edges;
}

// Pure: the overview reorg (S6). META band first, repos in the middle (each
// naming its orchestrators; >1 orchestrator => a tree), UNASSIGNED band last.
// Consumes S5's project.orchestrators[] and state.unassigned[]. Never throws.
export function overviewLayout(state) {
  const s = state || {};
  const projects = Array.isArray(s.projects) ? s.projects : [];
  const meta = projects.filter((p) => p && p.isMeta);
  const repos = projects
    .filter((p) => p && !p.isMeta)
    .map((p) => {
      const orchestrators = Array.isArray(p.orchestrators) ? p.orchestrators : [];
      return { ...p, orchestrators, tree: orchestrators.length > 1 };
    });
  const unassigned = Array.isArray(s.unassigned) ? s.unassigned : [];
  return { order: ["META", "REPOS", "UNASSIGNED"], meta, repos, unassigned };
}

// Pure: the project a tab is assigned to. "project:phase/role" -> project;
// a bare "phase/role" (no override) -> null.
function assignmentProject(assignment) {
  if (!assignment || typeof assignment !== "string") return null;
  const colon = assignment.indexOf(":");
  return colon > 0 ? assignment.slice(0, colon) : null;
}

// Meta-class roles: itinerant specialists that live in the Méta band unless they
// join a team. tichef is meta too but pinned (handled first in resolveAltitude).
function isMetaRole(role) {
  const r = String(role || "").toLowerCase();
  return r === "tichef" || r === "planner" || r === "refiner" || r === "auditor" || r === "scoper";
}

// Pure: a tab's dynamic altitude band (Inc7 S2). Encodes the 4 movements + the
// tichef pin (docs/dashboard-increment-7.md "altitude dynamique"):
//   - tichef -> always Méta (pinned, even while serving);
//   - a meta specialist SERVING a team -> that team's band, marked reinforcement;
//   - a solo meta specialist -> Méta;
//   - an orchestrator -> Orchestrateurs;
//   - any tab with an assignment -> Workers, under its team;
//   - otherwise -> Freelancers.
export function resolveAltitude(tab, state) {
  const t = tab || {};
  const role = String(t.role || "").toLowerCase();
  if (role === "tichef") return { band: "meta" };
  if (isMetaRole(role)) {
    if (t.serving) return { band: "worker", team: t.serving, reinforcement: true };
    const override = assignmentProject(t.assignment);
    if (override) return { band: "worker", team: override, reinforcement: true };
    return { band: "meta" };
  }
  if (role === "orchestrator") return { band: "orchestrator", team: assignmentProject(t.assignment) };
  if (t.assignment) return { band: "worker", team: assignmentProject(t.assignment) };
  return { band: "freelancer" };
}

// Pure: the 4-band compact org-chart model (Inc7 S1). Méta / Orchestrateurs (each
// with its served repo(s) -> workers chain) / Workers (assigned but orphan) /
// Freelancers (unassigned). Degrades cleanly (no services/assignment -> non-mapped
// tabs land in Freelancers). Never throws.
export function bandLayout(state) {
  const s = state || {};
  const projects = Array.isArray(s.projects) ? s.projects : [];
  const unassigned = Array.isArray(s.unassigned) ? s.unassigned : [];
  const allTabs = [];
  for (const p of projects) for (const t of projectTabs(p)) allTabs.push(t);

  const meta = allTabs.filter((t) => resolveAltitude(t, s).band === "meta");

  const orchestrators = allTabs
    .filter((t) => String(t && t.role || "").toLowerCase() === "orchestrator")
    .map((lead) => {
      const workers = allTabs.filter((t) => t && t.parentTabId === lead.id);
      const leadProj = assignmentProject(lead.assignment);
      const byRepo = new Map();
      for (const w of workers) {
        const repo = assignmentProject(w.assignment) || leadProj || lead.id;
        if (!byRepo.has(repo)) byRepo.set(repo, []);
        byRepo.get(repo).push(w);
      }
      if (!byRepo.size && leadProj) byRepo.set(leadProj, []);
      const repos = [...byRepo.entries()].map(([repo, ws]) => ({ repo, workers: ws }));
      return { lead, repos };
    });

  // Workers band = assigned workers NOT already shown under a chain (orphans).
  const shown = new Set();
  for (const o of orchestrators) for (const r of o.repos) for (const w of r.workers) shown.add(w.id);
  const workers = allTabs.filter((t) => resolveAltitude(t, s).band === "worker" && !shown.has(t.id));

  const freelancers = [...unassigned];
  for (const t of allTabs) if (resolveAltitude(t, s).band === "freelancer" && !freelancers.includes(t)) freelancers.push(t);
  for (const t of (Array.isArray(s.unmapped) ? s.unmapped : [])) if (!freelancers.includes(t)) freelancers.push(t);

  return { meta, orchestrators, workers, freelancers };
}

// Pure: the service nesting (Inc6 S4). One entry per service, in order, wrapping
// its sub-repos; a single-repo service is `mono` (not over-nested). Repo entries
// are normalised to {name} whether the server sends strings or objects. Null-safe.
export function serviceLayout(state) {
  const s = state || {};
  const services = Array.isArray(s.services) ? s.services : [];
  return services.map((svc) => {
    const repos = Array.isArray(svc && svc.projects)
      ? svc.projects.map((p) => (typeof p === "string" ? { name: p } : p))
      : [];
    return { service: svc && svc.name, rollupLed: svc && svc.rollupLed, repos, mono: repos.length <= 1 };
  });
}

// Pure: the org-chart (Inc6 S2). A solo méta (serving null) stays on top; each
// repo is a team whose LEAD is its orchestrator, with workers hanging under the
// lead (parentTabId) and any méta `serving` this repo JOINING the team (indispo).
// Never throws.
export function orgLayout(state) {
  const s = state || {};
  const projects = Array.isArray(s.projects) ? s.projects : [];
  const metaProjects = projects.filter((p) => p && p.isMeta);
  const repos = projects.filter((p) => p && !p.isMeta);
  const allTabs = [];
  for (const p of projects) for (const t of projectTabs(p)) allTabs.push(t);
  // Solo méta (not serving anyone) floats on top.
  const metaTop = [];
  for (const p of metaProjects) for (const t of projectTabs(p)) if (t && !t.serving) metaTop.push(t);
  const teams = repos.map((p) => {
    const tabs = projectTabs(p);
    const lead = tabs.find((t) => t && isOrchestrator(t.role)) || null;
    const workers = lead ? tabs.filter((t) => t && t.parentTabId === lead.id) : [];
    // A serving méta from ANYWHERE joins the team of the repo it serves.
    const serving = allTabs.filter((t) => t && t.serving === p.name);
    return { repo: p.name, lead, workers, serving };
  });
  return { metaTop, teams };
}

// --- Slice C: predecessor -> successor re-home link (drill-in) ---
// A re-homed tab (predecessor) carries a rehomeStatus through its bidirectional
// proof loop; the successor's parentTabId points back at the predecessor
// (docs/dashboard.md "Re-home status"). At drill-in we surface that pair with its
// readiness/ACK progress.
export const REHOME_STATES = ["handoff-written", "successor-ready", "ack-sent", "safe-to-close"];

// Pure: rehomeStatus -> its step index (0..3), or -1 for none/unknown.
export function rehomeStep(status) {
  return REHOME_STATES.indexOf(status);
}

// Pure: from a flat tab list, the re-home pairs. A predecessor is any tab with a
// (known) rehomeStatus; its successor is the tab whose parentTabId points back at
// it (null while none is linked yet, e.g. at handoff-written). Deterministic order.
export function rehomePairs(tabs) {
  const list = Array.isArray(tabs) ? tabs : [];
  const byParent = new Map();
  for (const t of list) if (t && t.parentTabId) byParent.set(t.parentTabId, t);
  const pairs = [];
  for (const pred of list) {
    if (!pred || rehomeStep(pred.rehomeStatus) < 0) continue;
    pairs.push({
      predecessor: pred,
      successor: (pred.id && byParent.get(pred.id)) || null,
      status: pred.rehomeStatus,
      step: rehomeStep(pred.rehomeStatus),
    });
  }
  return pairs;
}

// Pure: one re-home pair -> its list-item HTML. `esc` injected (Node-importable).
export function rehomePairHtml(pair, esc) {
  const pred = (pair && pair.predecessor) || {};
  const succ = pair && pair.successor;
  const status = (pair && pair.status) || "";
  const step = rehomeStep(status);
  const succName = succ ? esc(succ.name || "successor") : "(successor pending)";
  const dots = REHOME_STATES.map((s, i) =>
    `<span class="rehome-dot${i <= step ? " on" : ""}${i === step ? " current" : ""}" title="${esc(s)}"></span>`
  ).join("");
  const safe = status === "safe-to-close";
  return `<li class="rehome-pair${safe ? " safe" : ""}">
    <span class="rehome-old">${esc(pred.name || "predecessor")}</span>
    <span class="rehome-arrow" aria-hidden="true">→</span>
    <span class="rehome-new">${succName}</span>
    <span class="rehome-status" data-status="${esc(status)}">${esc(status || "—")}</span>
    <span class="rehome-progress" aria-label="re-home step ${step + 1} of 4">${dots}</span>
  </li>`;
}

// Pure: append the current page's share-token to a viewer URL so a right-click
// "open viewer" carries it. The viewer routes require a token, and the dashboard
// token is now a read-only observability credential for the whole fleet, so the
// page token is exactly what authorises the viewer. Host stays RELATIVE (works
// loopback AND behind a public host like amaury.wdes.eu). No url/token → passthrough.
export function viewerUrlWithToken(url, token) {
  if (!url || !token) return url || "";
  return url + (url.includes("?") ? "&" : "?") + "token=" + encodeURIComponent(token);
}

const POLL_MS = 1500;
const STATE_URL = "/dashboard/state";
// Per-tab RAM/CPU live in /tabs/usage (not /dashboard/state) — polled alongside
// and merged by id for the hover tooltip (tichef finding b).
const USAGE_URL = "/tabs/usage";
// The "Dernières heures" figures live in /dashboard/activity — a 3rd poll leg (S4).
const ACTIVITY_URL = "/dashboard/activity";

// Pure: /tabs/usage array -> Map<id, {ram, cpu}>. Tolerant of a missing array.
export function usageMap(list) {
  const map = new Map();
  for (const u of Array.isArray(list) ? list : []) {
    if (u && u.id) map.set(u.id, { ram: u.resident_memory_bytes, cpu: u.cpu_percent });
  }
  return map;
}

// Pure: bytes -> a compact human string (B/KB/MB/GB).
export function fmtBytes(n) {
  let v = Number(n || 0);
  if (v <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${i === 0 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
}

// Pure: cpu percent -> a short string.
export function fmtCpu(n) {
  return `${Math.round(Number(n || 0))}%`;
}

// The share-token the daemon gated this page on (master or the global dashboard
// token), carried in the page URL's `?token=` exactly like the tab viewer
// (main.js). Sent as `Authorization: Bearer` on the state poll so a remote,
// token-only load authorises. Guarded so importing this module under Node (the
// self-check) — where `location` is undefined — stays side-effect-free.
const TOKEN = typeof location === "undefined" ? "" : new URLSearchParams(location.search).get("token") || "";
const AUTH_HEADERS = TOKEN ? { Authorization: "Bearer " + TOKEN } : {};

// Live snapshot the popup reads from, refreshed each poll.
let currentNodes = new Map();
let currentUnmapped = [];
// Per-tab RAM/CPU from /tabs/usage, keyed by id (tooltip enrichment).
let usageById = new Map();
// Last band-chart model, for the flicker-free in-place patch (Inc7 S3).
let prevBandModel = null;
// The drilled-in project (null = level 0 / grid). Read from ?project= at boot.
let currentProject = null;
// Last state received, so a view switch (drill-in / back) can re-render without
// waiting for the next poll.
let currentState = null;

function escapeHtml(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, (c) => (
    { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]
  ));
}

function fmtTokens(tokens) {
  const t = tokens || {};
  const inp = Number(t.input || 0).toLocaleString();
  const out = Number(t.output || 0).toLocaleString();
  return `▲${inp} ▼${out}`;
}

// Pure: the tooltip detail chips for a tab (tichef finding b) — assignment, cwd,
// rehome_status, RAM, CPU. Each chip is rendered only when its datum exists, so
// the tooltip stays clean when a field is absent. `usage` = {ram, cpu} or null.
export function tabDetailChips(tab, usage, esc) {
  const t = tab || {};
  const u = usage || {};
  const chips = [];
  if (t.assignment) chips.push(`<span class="td">assign ${esc(t.assignment)}</span>`);
  if (t.cwd) chips.push(`<span class="td">cwd ${esc(t.cwd)}</span>`);
  if (t.rehomeStatus) chips.push(`<span class="td td-rehome" data-status="${esc(t.rehomeStatus)}">rehome ${esc(t.rehomeStatus)}</span>`);
  if (u.ram != null) chips.push(`<span class="td">RAM ${esc(fmtBytes(u.ram))}</span>`);
  if (u.cpu != null) chips.push(`<span class="td">CPU ${esc(fmtCpu(u.cpu))}</span>`);
  return chips.join("");
}

// One tab entry inside the popup (or the unmapped list). data-viewer carries the
// viewerUrl for the right-click handler; an orchestrator occupant gets the S5
// tint; `usage` (from /tabs/usage) feeds the RAM/CPU tooltip chips.
function tabEntryHtml(tab, usage) {
  const orch = isOrchestrator(tab.role) ? " orchestrator" : "";
  const chips = tabDetailChips(tab, usage, escapeHtml);
  return `<li class="popup-tab${orch}" data-viewer="${escapeHtml(tab.viewerUrl || "")}">
    <span class="tab-name">${escapeHtml(tab.name)}</span>
    <span class="tab-role">${escapeHtml(tab.role || "—")}</span>
    <span class="tab-item">${escapeHtml(tab.item || "—")}</span>
    <span class="tab-state">${escapeHtml(tab.agentState || "—")}</span>
    <span class="tab-tokens">${escapeHtml(fmtTokens(tab.tokens))}</span>
    ${chips ? `<span class="tab-details">${chips}</span>` : ""}
    ${taskChipsHtml(tab)}
  </li>`;
}

// --- DOM wiring (defined always, executed only in a browser) ---

function applyState(state) {
  currentState = state;
  render();
}

// Render the current (state, currentProject) — called on every poll and on any
// view switch (drill-in / back).
function render() {
  const view = resolveView(currentState, currentProject);
  // The org-chart reserves a minimum working area so it stays pannable (and its
  // scroll position survives a refresh) — a no-op once a real fleet overflows.
  if (typeof document !== "undefined") {
    document.body.classList.toggle("band-view", view.mode === "grid" && hasTichef(currentState));
  }
  if (view.mode === "grid") renderGrid();
  else renderDiagram(view);
  setViewChrome(view);
}

const BAND_LABELS = { 0: "tichef", 1: "orchestrators", 2: "workers" };

function bandHtml(label, inner) {
  return `<div class="altitude-band" data-band-label="${escapeHtml(label)}">${inner}</div>`;
}

// An assignment-less tab in the UNASSIGNED band — legitimate, NOT an error (#90).
function unassignedTabHtml(tab) {
  const t = tab || {};
  return `<div class="unassigned-tab" data-viewer="${escapeHtml(t.viewerUrl || "")}">${escapeHtml(t.name || "tab")}</div>`;
}

// Pure: the per-tab task/sub-agents render model (Inc7 S4-web). Reads the rust
// S4 fields (camelCase on DashboardTab): currentTask (string) + subAgents[]
// ({name, state}). Returns a chip list: one "task" chip (label = the task) then
// one "subagent" chip per invoked Task() (carrying its name + state). A tab with
// no transcript data -> [] (degrades cleanly). Null-safe.
export function taskChips(tab) {
  const t = tab || {};
  const chips = [];
  if (t.currentTask && String(t.currentTask).trim()) {
    chips.push({ kind: "task", label: String(t.currentTask) });
  }
  for (const s of Array.isArray(t.subAgents) ? t.subAgents : []) {
    if (!s) continue;
    chips.push({ kind: "subagent", name: s.name, state: s.state, label: s.name });
  }
  return chips;
}

// Pure: the S4 chips of a tab -> compact HTML (empty when there are none).
function taskChipsHtml(tab) {
  const chips = taskChips(tab);
  if (!chips.length) return "";
  const items = chips
    .map((c) =>
      c.kind === "task"
        ? `<span class="task-chip" title="current task">${escapeHtml(c.label)}</span>`
        : `<span class="subagent-chip state-${escapeHtml(c.state || "")}" title="sub-agent ${escapeHtml(c.name || "")} — ${escapeHtml(c.state || "")}">${escapeHtml(c.name || "")}</span>`
    )
    .join("");
  return `<span class="task-chips">${items}</span>`;
}

// Pure: the flicker-free refresh diff (Inc7 S3, Zoetrope borrow). Compares two
// models by STABLE node id and returns the minimal op list: `add` for a new id,
// `remove` for a gone id, `update` for a changed node, nothing for an unchanged
// one. Callers patch the DOM in place from these ops (no clear-and-rebuild), so
// node identity + selection + scroll + hover survive a poll. Malformed-safe.
export function diffRender(prev, next) {
  const prevNodes = prev && Array.isArray(prev.nodes) ? prev.nodes : [];
  const nextNodes = next && Array.isArray(next.nodes) ? next.nodes : [];
  const prevById = new Map(prevNodes.map((n) => [n && n.id, n]));
  const nextById = new Map(nextNodes.map((n) => [n && n.id, n]));
  const ops = [];
  for (const n of nextNodes) {
    if (!n) continue;
    const p = prevById.get(n.id);
    if (!p) ops.push({ op: "add", id: n.id, node: n });
    else if (JSON.stringify(p) !== JSON.stringify(n)) ops.push({ op: "update", id: n.id, node: n });
  }
  for (const p of prevNodes) if (p && !nextById.has(p.id)) ops.push({ op: "remove", id: p.id });
  return ops;
}

// --- Inc7 S1: compact 4-band org-chart ---

// Activation gate: a coordinated fleet (a tichef is present) gets the Inc7 4-band
// org-chart; fixtures/states without a tichef keep the Inc5/Inc6 views.
function hasTichef(state) {
  const projects = Array.isArray(state && state.projects) ? state.projects : [];
  for (const p of projects) for (const t of projectTabs(p)) {
    if (String(t && t.role || "").toLowerCase() === "tichef") return true;
  }
  return false;
}

// Inner content of a band node: name + renfort badge (S2) + task/sub-agent chips
// (S4). Factored so the S3 in-place patch can refresh it without recreating the
// element (identity + scroll survive).
function bandNodeInner(tab) {
  const t = tab || {};
  const alt = resolveAltitude(t, currentState);
  const badge = alt.reinforcement ? ` <span class="renfort-badge" title="en renfort dans ${escapeHtml(alt.team || "")}">renfort</span>` : "";
  return `${escapeHtml(t.name || "tab")}${badge}${taskChipsHtml(t)}`;
}

function bandNodeHtml(tab, cls) {
  const t = tab || {};
  const led = ledClass(t.led != null ? t.led : t.rollupLed);
  const reinf = resolveAltitude(t, currentState).reinforcement ? " reinforcement" : "";
  return `<div class="band-node ${cls} ${led}${reinf}" data-tab-id="${escapeHtml(t.id || "")}" data-viewer="${escapeHtml(t.viewerUrl || "")}" title="${escapeHtml(t.name || "")}">${bandNodeInner(t)}</div>`;
}

// Orchestrator chain: lead -> served repo sub-nodes -> workers (parentTabId).
function orchChainHtml(orch) {
  const lead = orch.lead || {};
  const repos = (orch.repos || [])
    .map((r) => `<div class="band-repo" data-repo="${escapeHtml(r.repo)}"><div class="repo-name">${escapeHtml(r.repo)}</div><div class="repo-workers">${(r.workers || []).map((w) => bandNodeHtml(w, "worker")).join("")}</div></div>`)
    .join("");
  return `<div class="band-orch" data-orch="${escapeHtml(lead.id || "")}">${bandNodeHtml(lead, "lead")}<div class="orch-repos">${repos}</div></div>`;
}

function bandHtml7(id, label, inner) {
  return `<div class="band" data-band="${id}"><div class="band-label">${escapeHtml(label)}</div><div class="band-row">${inner}</div></div>`;
}

// Live tab objects keyed by id, refreshed each build so the in-place patch can
// re-render a node's content (chips) from fresh data.
let bandTabById = new Map();

// The flat, stable-id model the S3 diff runs on: every band node with the fields
// that affect its rendering — its led AND a task signature (S4 chips), so a task
// change also produces an `update` op. Order-independent (keyed by id).
function buildBandModel(state) {
  const bl = bandLayout(state);
  const nodes = [];
  bandTabById = new Map();
  const push = (t) => {
    if (!t || !t.id) return;
    bandTabById.set(t.id, t);
    const led = t.led != null ? t.led : (t.rollupLed != null ? t.rollupLed : null);
    const subs = Array.isArray(t.subAgents) ? t.subAgents.map((s) => s && `${s.name}:${s.state}`).join(",") : "";
    nodes.push({ id: t.id, led, task: `${t.currentTask || ""}|${subs}` });
  };
  bl.meta.forEach(push);
  for (const o of bl.orchestrators) { push(o.lead); for (const r of o.repos) r.workers.forEach(push); }
  bl.workers.forEach(push);
  bl.freelancers.forEach(push);
  return { nodes };
}

// Patch one band node in place (no rebuild) — refresh its led class AND its inner
// content (name + chips) from fresh data, keeping the SAME element so its identity,
// selection and scroll survive (Inc7 S3).
function patchBandNode(id, node) {
  const el = document.querySelector(`.band-node[data-tab-id="${(typeof CSS !== "undefined" && CSS.escape) ? CSS.escape(id) : id}"]`);
  if (!el) return;
  for (const c of [...el.classList]) if (c.indexOf("led-") === 0) el.classList.remove(c);
  el.classList.add(ledClass(node.led));
  const tab = bandTabById.get(id);
  if (tab) el.innerHTML = bandNodeInner(tab);
}

// Refresh the band chart flicker-free: rebuild only on a structural change
// (add/remove) or when the chart isn't currently mounted; otherwise patch the
// changed nodes in place (Inc7 S3).
function renderBandOrPatch() {
  const grid = document.getElementById("project-grid");
  if (!grid) return;
  const next = buildBandModel(currentState);
  const mounted = !!grid.querySelector("[data-band]");
  const ops = diffRender(prevBandModel, next);
  const structural = ops.some((o) => o.op === "add" || o.op === "remove");
  if (!prevBandModel || !mounted || structural) {
    renderBandChart();
  } else {
    for (const op of ops) if (op.op === "update") patchBandNode(op.id, op.node);
  }
  prevBandModel = next;
}

// Full build of the 4-band chart (S1). S3 patches it in place between structural
// changes; this rebuild runs on first render and on any add/remove.
function renderBandChart() {
  const grid = document.getElementById("project-grid");
  if (!grid) return;
  const bl = bandLayout(currentState);
  const meta = bl.meta.map((t) => bandNodeHtml(t, "meta")).join("");
  const orchs = bl.orchestrators.map(orchChainHtml).join("");
  const workers = bl.workers.map((t) => bandNodeHtml(t, "worker")).join("");
  const free = bl.freelancers.map((t) => bandNodeHtml(t, "freelancer")).join("");
  grid.innerHTML =
    bandHtml7("meta", "Méta", meta) +
    bandHtml7("orchestrators", "Orchestrateurs", orchs) +
    bandHtml7("workers", "Workers", workers) +
    bandHtml7("freelancers", "Freelancers", free);
  const layer = document.getElementById("lineage-layer");
  if (layer) layer.innerHTML = "";
  currentNodes = new Map();
  currentUnmapped = [];
  renderRehome([]);
  renderUnmapped();
}

// --- Inc6 S2/S4: org-chart (méta on top / team lead + workers / serving joins) ---

// Service grouping for the org-chart. S2 renders teams flat; S4 wires this to the
// exported serviceLayout so a family service wraps its sub-repo teams.
function serviceGrouping(state) {
  return typeof serviceLayout === "function" ? serviceLayout(state) : [];
}

function metaTopHtml(tab) {
  const t = tab || {};
  return `<div class="meta-top-tab" data-viewer="${escapeHtml(t.viewerUrl || "")}">${escapeHtml(t.name || "méta")}</div>`;
}

function teamMemberHtml(tab, cls) {
  const t = tab || {};
  return `<div class="${cls}" data-viewer="${escapeHtml(t.viewerUrl || "")}">${escapeHtml(t.name || "tab")}</div>`;
}

// One team = a repo's org sub-tree: orchestrator lead, workers below, and any
// méta serving this repo joined in (marked indispo).
function teamHtml(team) {
  const t = team || { repo: "", lead: null, workers: [], serving: [] };
  const lead = t.lead ? teamMemberHtml(t.lead, "team-lead") : `<div class="team-lead team-lead-none">${escapeHtml(t.repo)}</div>`;
  const workers = (t.workers || []).map((w) => teamMemberHtml(w, "worker")).join("");
  const serving = (t.serving || [])
    .map((sv) => `<div class="serving" data-viewer="${escapeHtml((sv && sv.viewerUrl) || "")}" title="serving ${escapeHtml(t.repo)} — indispo">${escapeHtml((sv && sv.name) || "méta")} <span class="serving-badge">indispo</span></div>`)
    .join("");
  return `<div class="team project-card" data-repo="${escapeHtml(t.repo)}" data-project="${escapeHtml(t.repo)}"><div class="team-name">${escapeHtml(t.repo)}</div>${lead}<div class="team-members">${workers}${serving}</div></div>`;
}

// The Inc6 org-chart view, used when the server exposes the service dimension.
function renderOrgChart() {
  const grid = document.getElementById("project-grid");
  if (!grid) return;
  const org = orgLayout(currentState);
  const teamByRepo = new Map(org.teams.map((t) => [t.repo, t]));
  const parts = [];
  if (org.metaTop.length) {
    parts.push(`<div class="meta-top"><div class="meta-top-label">méta</div>${org.metaTop.map(metaTopHtml).join("")}</div>`);
  }
  const services = serviceGrouping(currentState);
  if (services.length) {
    // S4: group teams under their service (family wrapper; mono not over-nested).
    const covered = new Set();
    for (const svc of services) {
      const teams = svc.repos
        .map((r) => { covered.add(r.name); return teamHtml(teamByRepo.get(r.name) || { repo: r.name, lead: null, workers: [], serving: [] }); })
        .join("");
      parts.push(`<div class="service ${svc.mono ? "service-mono" : "service-family"}" data-service="${escapeHtml(svc.service)}"${svc.mono ? ' data-mono="true"' : ""}><div class="service-name">${escapeHtml(svc.service)}</div>${teams}</div>`);
    }
    // Safety: never drop a repo the services list forgot — render it flat.
    for (const t of org.teams) if (!covered.has(t.repo)) parts.push(teamHtml(t));
  } else {
    // No service grouping yet -> flat teams.
    for (const t of org.teams) parts.push(teamHtml(t));
  }
  grid.innerHTML = parts.join("");
  const layer = document.getElementById("lineage-layer");
  if (layer) layer.innerHTML = "";
  currentNodes = new Map();
  currentUnmapped = [];
  renderRehome([]);
  renderUnmapped();
}

// Level-0 overview (S6 reorg): META band on top, repos in the middle grouped by
// altitude (orchestrators / workers — server order within a band, stable across
// reloads), UNASSIGNED band at the bottom. Empty META/UNASSIGNED bands are
// skipped, but META always renders first and UNASSIGNED always last.
function renderGrid() {
  const grid = document.getElementById("project-grid");
  if (!grid) return;
  // Inc7: a coordinated fleet (tichef present) gets the 4-band compact org-chart,
  // refreshed flicker-free (in-place patch between structural changes).
  if (hasTichef(currentState)) {
    renderBandOrPatch();
    return;
  }
  // Inc6: when the server exposes the service dimension, show the org-chart.
  if (Array.isArray(currentState && currentState.services) && currentState.services.length) {
    renderOrgChart();
    return;
  }
  const layout = overviewLayout(currentState);
  const parts = [];
  if (layout.meta.length) {
    parts.push(bandHtml("META", layout.meta.map((p) => renderProjectCard(p, escapeHtml)).join("")));
  }
  const bands = new Map();
  for (const p of layout.repos) {
    const a = projectAltitude(p);
    if (!bands.has(a)) bands.set(a, []);
    bands.get(a).push(p);
  }
  for (const a of [...bands.keys()].sort((x, y) => x - y)) {
    const label = BAND_LABELS[a] || `altitude ${a}`;
    parts.push(bandHtml(label, bands.get(a).map((p) => renderProjectCard(p, escapeHtml)).join("")));
  }
  if (layout.unassigned.length) {
    parts.push(bandHtml("UNASSIGNED", layout.unassigned.map(unassignedTabHtml).join("")));
  }
  grid.innerHTML = parts.join("");
  drawLineage(layout.meta.concat(layout.repos));
  currentNodes = new Map();
  currentUnmapped = [];
  renderRehome([]); // re-home links are a drill-in concern; hide at level 0
  renderUnmapped();
}

// Draw cross-project delegation edges over the cards. The edge LIST is pure
// (lineageEdges); only the card-to-coordinate mapping needs the laid-out DOM.
function drawLineage(projects) {
  const layer = document.getElementById("lineage-layer");
  const wrap = document.getElementById("grid-wrap");
  if (!layer || !wrap) return;
  const edges = lineageEdges(projects);
  const wrapRect = wrap.getBoundingClientRect();
  layer.setAttribute("viewBox", `0 0 ${Math.round(wrapRect.width)} ${Math.round(wrapRect.height)}`);
  const escSel = (s) => (typeof CSS !== "undefined" && CSS.escape ? CSS.escape(s) : String(s).replace(/"/g, '\\"'));
  const card = (name) => wrap.querySelector(`.project-card[data-project="${escSel(name)}"]`);
  const lines = [];
  for (const e of edges) {
    const a = card(e.from);
    const b = card(e.to);
    if (!a || !b) continue;
    const ar = a.getBoundingClientRect();
    const br = b.getBoundingClientRect();
    const x1 = ar.left + ar.width / 2 - wrapRect.left;
    const y1 = ar.bottom - wrapRect.top;
    const x2 = br.left + br.width / 2 - wrapRect.left;
    const y2 = br.top - wrapRect.top;
    lines.push(`<line class="lineage-edge" x1="${x1}" y1="${y1}" x2="${x2}" y2="${y2}" marker-end="url(#lineage-arrow)"/>`);
  }
  layer.innerHTML =
    `<defs><marker id="lineage-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" class="lineage-arrow-head"/></marker></defs>` +
    lines.join("");
}

function renderDiagram(view) {
  currentNodes = nodeMap({ nodes: view.nodes });
  currentUnmapped = Array.isArray(view.unmapped) ? view.unmapped : [];

  for (const phase of CANONICAL_PHASES) {
    const el = document.getElementById(`node-${phase}`);
    if (!el) continue;
    const node = currentNodes.get(phase);
    const led = node ? node.rollupLed : null;
    el.setAttribute("class", `node ${ledClass(led)}`);
    const count = node && node.tabs ? node.tabs.length : 0;
    const countEl = el.querySelector(".node-count");
    if (countEl) countEl.textContent = count ? String(count) : "";
    const subEl = el.querySelector(".node-subtitle");
    if (subEl) subEl.textContent = node ? nodeSubtitle(node) : "";
  }

  // Slice C: re-home links among the tabs currently in view.
  const tabs = [...view.nodes.flatMap((n) => (n && n.tabs) || []), ...currentUnmapped];
  renderRehome(tabs);
  renderUnmapped();
}

function renderRehome(tabs) {
  const section = document.getElementById("rehome");
  const list = document.getElementById("rehome-list");
  if (!section || !list) return;
  const pairs = rehomePairs(tabs);
  if (!pairs.length) {
    section.hidden = true;
    list.innerHTML = "";
    return;
  }
  section.hidden = false;
  list.innerHTML = pairs.map((p) => rehomePairHtml(p, escapeHtml)).join("");
}

// Show/hide the level-0 grid vs the level-1 diagram (+ back button). The lineage
// overlay lives inside the grid wrapper, so hiding the wrapper also hides it in
// L1 (no stray edges over the diagram).
function setViewChrome(view) {
  const wrap = document.getElementById("grid-wrap");
  const flow = document.getElementById("flow");
  const back = document.getElementById("back-btn");
  const isGrid = view.mode === "grid";
  if (wrap) wrap.hidden = !isGrid;
  // #flow is an <svg> (SVGElement): the `hidden` IDL prop is HTMLElement-only, so
  // `flow.hidden = …` is a no-op and the empty diagram bleeds under the grid.
  // toggleAttribute writes the content attribute on any element -> [hidden] hides it.
  if (flow) flow.toggleAttribute("hidden", isGrid);
  if (back) back.hidden = !(view.mode === "diagram" && view.scoped);
}

// Drill into a project (or back out with null), keeping the URL's ?project= in
// sync so the view is deep-linkable and the browser back button works. Re-renders
// from the last state immediately — no wait for the next poll.
function navigateTo(project, push) {
  currentProject = project || null;
  if (typeof history !== "undefined" && typeof location !== "undefined") {
    const url = new URL(location.href);
    if (currentProject) url.searchParams.set("project", currentProject);
    else url.searchParams.delete("project");
    if (push) history.pushState({ project: currentProject }, "", url);
    else history.replaceState({ project: currentProject }, "", url);
  }
  render();
}

function renderUnmapped() {
  const section = document.getElementById("unmapped");
  const list = document.getElementById("unmapped-list");
  if (!section || !list) return;
  if (!currentUnmapped.length) {
    section.hidden = true;
    list.innerHTML = "";
    return;
  }
  section.hidden = false;
  list.innerHTML = currentUnmapped.map((t) => tabEntryHtml(t, usageById.get(t.id))).join("");
}

let hideTimer = null;

function positionPopup(popup, anchorRect) {
  popup.style.left = `${Math.round(anchorRect.left + window.scrollX)}px`;
  popup.style.top = `${Math.round(anchorRect.bottom + window.scrollY + 8)}px`;
}

function popupHtml(title, led, tabs) {
  return (
    `<div class="popup-title">${escapeHtml(title)} · ${escapeHtml(led || "—")}</div>
     <ul class="popup-tabs">${tabs.map((t) => tabEntryHtml(t, usageById.get(t.id))).join("")}</ul>
     <div class="popup-hint">right-click a tab to open its viewer</div>`
  );
}

function showPopupFor(phase, anchorEl) {
  const popup = document.getElementById("popup");
  const node = currentNodes.get(phase);
  if (!popup || !node || !node.tabs || !node.tabs.length) return;
  clearTimeout(hideTimer);
  popup.innerHTML = popupHtml(phase, node.rollupLed, node.tabs);
  popup.hidden = false;
  positionPopup(popup, anchorEl.getBoundingClientRect());
}

// Hover a project CARD (level 0) -> tooltip listing its occupants with details.
function showPopupForProject(name, anchorEl) {
  const popup = document.getElementById("popup");
  if (!popup || !currentState) return;
  const projects = Array.isArray(currentState.projects) ? currentState.projects : [];
  const project = projects.find((p) => p && p.name === name);
  if (!project) return;
  const tabs = projectTabs(project);
  if (!tabs.length) return;
  clearTimeout(hideTimer);
  popup.innerHTML = popupHtml(name, project.rollupLed, tabs);
  popup.hidden = false;
  positionPopup(popup, anchorEl.getBoundingClientRect());
}

function scheduleHide() {
  const popup = document.getElementById("popup");
  if (!popup) return;
  hideTimer = setTimeout(() => { popup.hidden = true; }, 200);
}

function openViewerFrom(target) {
  const entry = target.closest && target.closest(".popup-tab");
  if (!entry) return false;
  const url = entry.getAttribute("data-viewer");
  if (url) window.open(viewerUrlWithToken(url, TOKEN), "_blank", "noopener");
  return true;
}

async function poll() {
  const status = document.getElementById("status");
  const headers = { accept: "application/json", ...AUTH_HEADERS };
  try {
    // /tabs/usage (RAM/CPU tooltip) and /dashboard/activity (S4 panel) are
    // best-effort side legs; their failure never breaks the dashboard — only the
    // state poll gates 'live'/'offline'.
    const [res, usageRes, actRes] = await Promise.all([
      fetch(STATE_URL, { headers }),
      fetch(USAGE_URL, { headers }).catch(() => null),
      fetch(ACTIVITY_URL, { headers }).catch(() => null),
    ]);
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    if (usageRes && usageRes.ok) {
      try { usageById = usageMap(await usageRes.json()); } catch { /* keep last */ }
    }
    if (actRes && actRes.ok) {
      try { renderActivity(activityModel(await actRes.json())); } catch { /* keep last */ }
    }
    applyState(await res.json());
    if (status) status.textContent = "live";
    if (status) status.className = "status ok";
  } catch (err) {
    if (status) status.textContent = `offline (${err.message})`;
    if (status) status.className = "status err";
  }
}

// --- S4: "Dernières heures" activity panel ---
const ACTIVITY_FIGURES = [
  // Inc6: features stays undivided; fixes / self-tooling / issues are SEPARATE.
  { key: "features_implemented", label: "features", get: (m) => m.features },
  { key: "fixes", label: "fixes", get: (m) => m.fixes },
  { key: "self_tooling", label: "self-tooling", get: (m) => m.selfTooling },
  { key: "issues_opened", label: "issues opened", get: (m) => m.issuesOpened },
  { key: "issues_closed", label: "issues closed", get: (m) => m.issuesClosed },
  { key: "tokens_per_feature", label: "tokens/feature", get: (m) => m.tokensPerFeature },
  { key: "minutes_since_last_human_prompt", label: "min since human", get: (m) => m.minutesSinceLastHumanPrompt },
  { key: "aligator_calls", label: "aligator", get: (m) => m.aligatorCalls },
  { key: "human_prompts", label: "human prompts", get: (m) => m.humanPrompts },
];
const ACTIVITY_SERIES = [
  { key: "features", label: "features/day", get: (m) => m.series.features },
  { key: "fixes", label: "fixes/day", get: (m) => m.series.fixes },
  { key: "self_tooling", label: "self-tooling/day", get: (m) => m.series.selfTooling },
  { key: "tokens_per_feature", label: "tokens/feature", get: (m) => m.series.tokensPerFeature },
  { key: "autonomy", label: "autonomy (min)", get: (m) => m.series.autonomy },
];

// One bar per day, height scaled to the series max (min 2px so a nonzero day is
// always visible). Empty series -> no bars (the empty-panel state).
function drawActivityBars(values) {
  const nums = (values || []).map((v) => Number(v || 0));
  const max = Math.max(1, ...nums);
  return nums
    .map((v) => `<span class="activity-bar" style="height:${Math.max(2, Math.round((v / max) * 40))}px" title="${escapeHtml(String(v))}"></span>`)
    .join("");
}

function renderActivity(model) {
  const body = document.getElementById("activity-body");
  if (!body) return;
  const figures = ACTIVITY_FIGURES
    .map((f) => `<div class="activity-figure"><span class="fig-value" data-figure="${f.key}">${escapeHtml(String(f.get(model)))}</span><span class="fig-label">${escapeHtml(f.label)}</span></div>`)
    .join("");
  const series = ACTIVITY_SERIES
    .map((s) => `<div class="activity-serie"><span class="serie-label">${escapeHtml(s.label)}</span><div class="activity-bars" data-series="${s.key}">${drawActivityBars(s.get(model))}</div></div>`)
    .join("");
  const summary = model.summaryLines.length
    ? `<ul class="activity-summary">${model.summaryLines.map((l) => `<li>${escapeHtml(l)}</li>`).join("")}</ul>`
    : "";
  const record = model.record
    ? `<div class="activity-record">record: ${escapeHtml(model.record.label || "")} · ~${escapeHtml(String(Math.round((Number(model.record.autonomy_minutes) || 0) / 60)))}h autonomy</div>`
    : "";
  // Inc6: maturity/growth verdict badge (self-improvement), when present.
  const trend = model.verdictDetail && model.verdictDetail.autonomy_trend ? ` (autonomy ${escapeHtml(model.verdictDetail.autonomy_trend)})` : "";
  const verdict = model.verdict
    ? `<div class="verdict-badge" data-verdict="${escapeHtml(model.verdict)}">${escapeHtml(model.verdict)}${trend}</div>`
    : "";
  body.innerHTML = `${verdict}<div class="activity-figures">${figures}</div><div class="activity-series">${series}</div>${summary}${record}`;
}

// --- S1: legend rendering + persistent on/off toggle ---
const LEGEND_KEY = "ta-dash.legend-hidden";

function renderLegend() {
  const el = document.getElementById("legend");
  if (!el) return;
  el.innerHTML = legendModel()
    .map((e) => `<span class="legend-item"><span class="legend-swatch ${e.cls}"></span><span class="legend-label">${escapeHtml(e.label)}</span></span>`)
    .join("");
}

function readLegendHidden() {
  try { return localStorage.getItem(LEGEND_KEY) === "1"; } catch { return false; }
}

function applyLegendVisibility() {
  const el = document.getElementById("legend");
  if (el) el.toggleAttribute("hidden", readLegendHidden());
}

function wireLegendToggle() {
  const toggle = document.getElementById("legend-toggle");
  const el = document.getElementById("legend");
  if (!toggle || !el) return;
  toggle.addEventListener("click", () => {
    const nowHidden = !el.hasAttribute("hidden");
    el.toggleAttribute("hidden", nowHidden);
    try { localStorage.setItem(LEGEND_KEY, nowHidden ? "1" : "0"); } catch { /* ignore */ }
  });
}

function bootstrap() {
  renderLegend();
  applyLegendVisibility();
  wireLegendToggle();
  // Hover popups on each phase node; a short hide delay lets the pointer travel
  // into the popup (where right-click lives) without it vanishing first.
  for (const el of document.querySelectorAll(".node")) {
    el.addEventListener("mouseenter", () => showPopupFor(el.dataset.phase, el));
    el.addEventListener("focus", () => showPopupFor(el.dataset.phase, el));
    el.addEventListener("mouseleave", scheduleHide);
    el.addEventListener("blur", scheduleHide);
  }
  const popup = document.getElementById("popup");
  if (popup) {
    popup.addEventListener("mouseenter", () => clearTimeout(hideTimer));
    popup.addEventListener("mouseleave", scheduleHide);
  }
  // Right-click a tab entry (in the popup or the unmapped list) -> its viewer.
  document.addEventListener("contextmenu", (e) => {
    if (openViewerFrom(e.target)) e.preventDefault();
  });

  // Drill into a project card (delegated — the grid is re-rendered each poll).
  const grid = document.getElementById("project-grid");
  if (grid) {
    grid.addEventListener("click", (e) => {
      const card = e.target.closest && e.target.closest(".project-card");
      if (card) navigateTo(card.dataset.project, true);
    });
    // Hover a card -> occupant tooltip (delegated; short hide delay like nodes).
    grid.addEventListener("mouseover", (e) => {
      const card = e.target.closest && e.target.closest(".project-card");
      if (card) showPopupForProject(card.dataset.project, card);
    });
    grid.addEventListener("mouseout", (e) => {
      const card = e.target.closest && e.target.closest(".project-card");
      if (card) scheduleHide();
    });
  }
  // Back to the grid.
  const back = document.getElementById("back-btn");
  if (back) back.addEventListener("click", () => navigateTo(null, true));
  // Browser back/forward moves between grid and drilled project.
  window.addEventListener("popstate", () => {
    currentProject = readProjectParam(location.search);
    render();
  });
  // Lineage edge coordinates are layout-derived -> redraw on resize (grid only).
  window.addEventListener("resize", () => {
    if (currentProject == null) render();
  });

  // Deep-link: open straight into ?project= if present.
  currentProject = readProjectParam(location.search);

  poll();
  setInterval(poll, POLL_MS);
}

if (typeof document !== "undefined") {
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", bootstrap);
  } else {
    bootstrap();
  }
}

// --- Coverage seam (audit #357 Phase A) — export internal pure helpers so the
// characterization suite can lock their CURRENT behavior BEFORE the Q3-Q10
// refactor. Additive only (function declarations, hoisted); the refactor keeps
// these names or updates the tests. See assets/dashboard.characterization.test.mjs.
export { projectTabs, serviceGrouping, metaTopHtml, teamMemberHtml, unassignedTabHtml, popupHtml, tabEntryHtml };
