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
  // Inc9-A: the subtitle shows the DECLARED currentTask (currentTaskLog latest) —
  // NOT the observed `context`/`item` (last prompt), which bled broadcasts onto the
  // node. No declared task -> name only (consistent with the 'libre' pill: no
  // observed fallback, so a dispatched/broadcast prompt never surfaces here).
  const log = Array.isArray(first.currentTaskLog) ? first.currentTaskLog : [];
  const ctx = shortContext(log.length ? String(log[log.length - 1]) : "");
  let label = ctx ? `${name} · ${ctx}` : name;
  if (label.length > 24) label = label.slice(0, 23) + "…";
  return tabs.length > 1 ? `${label} +${tabs.length - 1}` : label;
}

// Pure: the self-declared agent card (Inc8 S1). specialty / orchestrator /
// objective / currentTask (the latest permalog phrase if it's an array, else the
// string verbatim) + a `free` flag (orchestrator === "free" -> the 'libre' badge).
// Absent fields -> "" / null / false, null-safe.
export function agentCard(tab) {
  const t = tab || {};
  // currentTask = the latest DECLARED permalog phrase (`currentTaskLog`, written by
  // append_current_task / set-current-task) — NOT the OBSERVED transcript field
  // `currentTask` ("latest human-typed prompt"), which bleeds every dispatched /
  // broadcast message onto its recipients (Inc9 (1) bug). Empty permalog -> "".
  const log = Array.isArray(t.currentTaskLog) ? t.currentTaskLog : [];
  const currentTask = log.length ? String(log[log.length - 1]) : "";
  const orchestrator = t.orchestrator != null ? t.orchestrator : null;
  return {
    specialty: t.specialty ? String(t.specialty) : "",
    objective: t.objective ? String(t.objective) : "",
    orchestrator,
    free: orchestrator === "free",
    currentTask,
  };
}

// Pure: the full right-click agent-card view (Inc8 S3). Like agentCard but with a
// BOUNDED permalog (recentTasks = the last 5 entries of currentTaskLog, in order),
// evaluations + evalCriteria (default []). Orchestrators get a card too (no
// exclusion). `free` = orchestrator === "free". Null-safe.
export function agentCardView(tab) {
  const t = tab || {};
  const log = Array.isArray(t.currentTaskLog) ? t.currentTaskLog.map(String) : [];
  return {
    role: t.role || "",
    // Inc9-B: assignment (projet:phase/role) = l'ancre d'identité stable (§17).
    assignment: t.assignment ? String(t.assignment) : "",
    specialty: t.specialty ? String(t.specialty) : "",
    orchestrator: t.orchestrator != null ? t.orchestrator : "",
    free: t.orchestrator === "free",
    objective: t.objective ? String(t.objective) : "",
    recentTasks: log.slice(-5),
    evaluations: Array.isArray(t.evaluations) ? t.evaluations : [],
    evalCriteria: Array.isArray(t.evalCriteria) ? t.evalCriteria : [],
  };
}

// Pure: clip a text to `max` words (Inc9 (2) detailed popup). Returns the clipped
// text, whether it overflowed, and the FULL text — so the popup renders a 'voir
// plus' toggle only past the threshold. Null-safe; collapses runs of whitespace.
export function clipWords(text, max = 50) {
  const s = String(text == null ? "" : text);
  const words = s.trim().split(/\s+/).filter(Boolean);
  if (words.length <= max) return { text: s, clipped: false, full: s };
  return { text: words.slice(0, max).join(" ") + "…", clipped: true, full: s };
}

// Pure: an orchestrator's supervision-rounds pill (Inc8 S3). GREEN when
// roundsActive.active === true, GREY otherwise (absent/null-safe).
export function roundsPill(tab) {
  const ra = tab && tab.roundsActive;
  const active = !!(ra && ra.active === true);
  return { active, cls: active ? "rounds-on" : "rounds-off" };
}

// Pure: the context-usage pill (Inc9 b2). Reads `context_pct` (Option 0-100).
// Colour by threshold: green <70, amber 70-90, red >90. Returns null when the
// field is absent/not a number -> the caller renders no pill. Null-safe.
export function contextPill(tab) {
  const v = tab && tab.context_pct;
  if (typeof v !== "number" || Number.isNaN(v)) return null;
  const pct = Math.round(v);
  const cls = pct > 90 ? "ctx-crit" : pct >= 70 ? "ctx-warn" : "ctx-ok";
  return { pct, cls, label: `${pct}% ctx` };
}

// Pure: the recent-compaction badge (Inc9 b3). Shows when `recently_compacted`
// is true. Null-safe; absent/false -> not shown.
export function compactionBadge(tab) {
  return { show: !!(tab && tab.recently_compacted === true) };
}

// Pure: the card's evaluations section (Inc8 S4). recent = the LAST 5 eval records
// (newest-last), verdict = the newest record's verdict, and triggerArmed mirrors
// the rust auto-improvement triggers — ARMED when either:
//   - avg: total errors exceed the 1-error-per-1M-tokens budget, OR
//   - burst: >= 3 errors within the last 1M tokens of evaluation.
// Signal only (S5 acts on it). Reads evaluations[].{tokens:{in,out}, scores:{errors}}.
// Null-safe; missing tokens/scores default to 0.
export function evalSummary(tab) {
  const evals = tab && Array.isArray(tab.evaluations) ? tab.evaluations : [];
  const errsOf = (e) => Number((e && e.scores && e.scores.errors) || 0);
  const toksOf = (e) => Number((e && e.tokens && e.tokens.in) || 0) + Number((e && e.tokens && e.tokens.out) || 0);
  const totalErrors = evals.reduce((a, e) => a + errsOf(e), 0);
  const totalTokens = evals.reduce((a, e) => a + toksOf(e), 0);
  const avgArmed = totalTokens > 0 ? totalErrors * 1_000_000 > totalTokens : totalErrors > 0;
  // Burst window: walk newest -> oldest, sum errors until the 1M-token window fills.
  let cum = 0, burstErrors = 0;
  for (let i = evals.length - 1; i >= 0; i--) {
    burstErrors += errsOf(evals[i]);
    cum += toksOf(evals[i]);
    if (cum >= 1_000_000) break;
  }
  const last = evals.length ? evals[evals.length - 1] : null;
  return {
    recent: evals.slice(-5),
    verdict: last && last.verdict != null ? String(last.verdict) : "",
    triggerArmed: avgArmed || burstErrors >= 3,
  };
}

// Pure: the card's declared-conventions section (Inc8 fold). Reads the wire field
// `conventions` (a string[] of declared .md files). `missing` FLAGS an agent that
// declared none (the free-bot-style "no conventions" check). The declared-vs-
// existing SEMANTIC check is ta-convention-auditor's job, not the dashboard's.
// Null-safe.
export function conventionsCheck(tab) {
  const conventions = tab && Array.isArray(tab.conventions) ? tab.conventions : [];
  const declared = conventions.length > 0;
  return { conventions, declared, missing: !declared };
}

// --- Catalogue #39 SC2 (reconciled to the live SC1 contract) ---
// GET /catalog/list -> { retired, skills }. A skill's fold key is `skill`; metrics
// are `metrics.byMode.{fresh,resume}{spawns,success,problem,tokensAvg,costAvg}`; the
// A/B compare is an OBJECT `freshVsResume{verdict, freshN, resumeN, deliveryDelta,
// tokensRatio}`. The RUST is the single source of the G1 guard (MIN_SAMPLE=3) — the
// web renders the server verdict VERBATIM, never re-gates (no MIN_SAMPLE in JS).
// The catalogue is a COLD source, fetched on-demand, never in the 1.5s poll (RB2).

// Pure: the read-model's skills -> a deterministic list (sorted by the fold key
// `skill`). Tombstoned skills are already filtered server-side. Null-safe.
export function catalogView(readModel) {
  const skills = readModel && Array.isArray(readModel.skills) ? readModel.skills : [];
  return skills
    .filter((s) => s && s.skill != null)
    .slice()
    .sort((a, b) => String(a.skill).localeCompare(String(b.skill)));
}

// Pure: a skill's profile fold -> normalised render fields. Absent -> ""/[]/null.
export function skillProfileModel(skill) {
  const s = skill || {};
  const arr = (x) => (Array.isArray(x) ? x.map(String) : []);
  return {
    name: s.skill != null ? String(s.skill) : "",
    prompt: s.prompt != null ? String(s.prompt) : "",
    specialty: s.specialty != null ? String(s.specialty) : "",
    conventions: arr(s.conventions),
    tools: arr(s.tools),
    patterns: arr(s.patterns),
    promptVersion: s.promptVersion != null ? s.promptVersion : null,
    usageCount: s.usageCount != null ? s.usageCount : null,
  };
}

// Pure: the byMode metrics table model. The fresh_vs_resume VERDICT is the server's
// (`freshVsResume.verdict`, camelCase: insufficientSample | inconclusive |
// freshFavored | resumeFavored) rendered VERBATIM — the rust applies G1 (MIN_SAMPLE=3)
// as the single source of truth, so there is NO JS re-gate. Per-arm sample sizes
// (freshN/resumeN) are surfaced (G3). Never a per-task pass/fail. Null-safe.
export function byModeMetricsModel(skill) {
  const bm = (skill && skill.metrics && skill.metrics.byMode) || {};
  const norm = (m) => ({
    spawns: Number((m && m.spawns) || 0),
    success: Number((m && m.success) || 0),
    problem: Number((m && m.problem) || 0),
    tokensAvg: m && m.tokensAvg != null ? Number(m.tokensAvg) : null,
    costAvg: m && m.costAvg != null ? Number(m.costAvg) : null,
  });
  const fvr = (skill && skill.freshVsResume) || {};
  const verdict = fvr.verdict != null ? String(fvr.verdict) : "insufficientSample";
  const freshN = Number(fvr.freshN || 0);
  const resumeN = Number(fvr.resumeN || 0);
  return {
    fresh: norm(bm.fresh),
    resume: norm(bm.resume),
    verdict,
    freshN,
    resumeN,
    n: freshN + resumeN,
    insufficient: verdict === "insufficientSample",
    deliveryDelta: fvr.deliveryDelta != null ? Number(fvr.deliveryDelta) : null,
    tokensRatio: fvr.tokensRatio != null ? Number(fvr.tokensRatio) : null,
  };
}

// Pure: the SC3 edit form -> {ok, body}|{ok:false, error}. CLIENT CF1 guard (double
// garde with the server 409): the prompt must stay non-empty. conventions accept an
// array or a newline-separated string (blank lines dropped). promptVersion rides as
// the optimistic-concurrency token when present.
export function editBody(form) {
  const f = form || {};
  const prompt = f.prompt != null ? String(f.prompt) : "";
  if (!prompt.trim()) return { ok: false, error: "le prompt ne peut pas être vide (CF1)" };
  const body = { prompt };
  if (f.specialty != null) body.specialty = String(f.specialty);
  if (f.conventions != null) {
    body.conventions = Array.isArray(f.conventions)
      ? f.conventions.map(String).map((x) => x.trim()).filter(Boolean)
      : String(f.conventions).split(/\r?\n/).map((x) => x.trim()).filter(Boolean);
  }
  if (f.promptVersion != null && f.promptVersion !== "") {
    const pv = Number(f.promptVersion);
    if (!Number.isNaN(pv)) body.promptVersion = pv;
  }
  return { ok: true, body };
}

// --- S5/S6: orchestrator tint + altitude bands + delegation lineage ---
// These consume role / parentTabId / (optional) altitude fields exposed by the
// Rust builder. ponytail: the altitude/lineage contract is provisional until the
// Rust S6 slice lands — everything below degrades cleanly when the fields are
// absent (one band, no edges), so today's fixtures render fine.

export function isOrchestrator(role) {
  return String(role || "").toLowerCase() === "orchestrator";
}

// The fleet's top-level coordinator. Its REAL role on the live daemon is
// `manager` (assignment "meta/manager", a unique tab); `tichef` is kept as an
// alias for fixtures. Recognised in all three places that pin the coordinator to
// the Méta band: roleAltitude (band 0), resolveAltitude (pin), hasTichef (gate).
function isTichefRole(role) {
  const r = String(role || "").trim().toLowerCase();
  return r === "tichef" || r === "manager";
}

// Pure: an agent ROLE -> its altitude band (0 = highest). Three bands per the
// plan: tichef atop, orchestrators below, workers/specialists at the bottom.
// Keyed strictly on the role, NOT the phase (tichef finding): a meta-lane
// orchestrator (role "orchestrator", any phase) must land in the orchestrator
// band, never the tichef band.
export function roleAltitude(role) {
  const r = String(role || "").trim().toLowerCase();
  if (isTichefRole(r)) return 0;
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

// Every tab of every project in the state, flattened. (lineageEdges keeps its own
// loop because it needs each tab's owning project name.)
function allProjectTabs(state) {
  const projects = Array.isArray(state && state.projects) ? state.projects : [];
  const out = [];
  for (const p of projects) for (const t of projectTabs(p)) out.push(t);
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

// Meta daemons of the trio (Brain + aligator) report to orchestrator "meta" and
// have no role — pin them to the Méta band alongside the tichef (Inc8 S3).
function isMetaDaemon(t) {
  if (!t) return false;
  const kind = String(t.agent_kind || "").toLowerCase();
  return t.orchestrator === "meta" || kind === "brain" || kind === "aligator";
}

// Inc9 (4): the 3 AUTONOMOUS daemons that supervise EVERY agent — the TRUE Méta band:
// tichef (meta/manager), Brain (meta/brain / kind brain), aligator (meta/router /
// kind aligator). Everything else meta-class is an on-demand SUPPORTER, not a daemon.
const CORE_META_ASSIGN = ["meta/manager", "meta/brain", "meta/router"];
function isCoreMeta(t) {
  if (!t) return false;
  if (isTichefRole(String(t.role || "").toLowerCase())) return true;
  const kind = String(t.agent_kind || "").toLowerCase();
  if (kind === "brain" || kind === "aligator") return true;
  return CORE_META_ASSIGN.includes(String(t.assignment || ""));
}

// Inc9 (4): a meta-class SUPPORTER — an on-demand specialist (guardian=Joséphine,
// foreman=Henri Ford, sage, scribe, coverage, scout, auditor…) called by an
// orchestrator/tichef for a precise task. Closer to a freelancer than a daemon, and
// NOT one of the 3 autonomous daemons -> its OWN band, NOT under tichef (future:
// multiple tichef). Signalled by a "meta/…" or "scope/…" lane assignment or a meta
// role. A supporter REINFORCING a team (project:role / serving) still joins that team.
function isSupporter(t) {
  if (!t || isCoreMeta(t)) return false;
  const a = String(t.assignment || "");
  // Lanes de supporters fleet-wide (assignment "lane/role" SANS préfixe projet). "review/"
  // capte le reviewer dédié (Olympe = review/evaluator) ; un "projet:review/auditor" (worker
  // de projet, ex. ta-quality-auditor) commence par "projet:", donc n'est PAS capté ici.
  return a.startsWith("meta/") || a.startsWith("scope/") || a.startsWith("review/")
    || isMetaRole(String(t.role || "").toLowerCase()) || isMetaDaemon(t);
}

// Pure: a tab's dynamic altitude band (Inc7 S2). Encodes the 4 movements + the
// tichef pin (docs/dashboard-increment-7.md "altitude dynamique"):
//   - tichef -> always Méta (pinned, even while serving);
//   - a meta specialist SERVING a team -> that team's band, marked reinforcement;
//   - a solo meta specialist -> Méta;
//   - an orchestrator -> Orchestrateurs;
//   - any tab with an assignment -> Workers, under its team;
//   - otherwise -> Freelancers.
// Depends only on the tab (no fleet state); callers may pass a 2nd arg, ignored.
export function resolveAltitude(tab) {
  const t = tab || {};
  const role = String(t.role || "").toLowerCase();
  // The 3 autonomous daemons (tichef/Brain/aligator) -> the TRUE Méta band (Inc9 (4)).
  if (isCoreMeta(t)) return { band: "meta" };
  // On-demand meta-class SUPPORTERS -> their own band, BETWEEN Méta and Orchestrateurs
  // — UNLESS currently reinforcing a team (serving / a "project:role" assignment),
  // which joins that team's Workers band, marked reinforcement (as before).
  if (isSupporter(t)) {
    if (t.serving) return { band: "worker", team: t.serving, reinforcement: true };
    const override = assignmentProject(t.assignment);
    if (override) return { band: "worker", team: override, reinforcement: true };
    return { band: "supporter" };
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
  // Dedup every tab by id across ALL sources: the live daemon lists an unmapped
  // tab both at the top level AND inside a synthetic "divers" project, so without
  // a dedup a tab would show in two bands at once. First occurrence wins.
  const byId = new Map();
  const collect = (arr) => { for (const t of arr || []) if (t && t.id && !byId.has(t.id)) byId.set(t.id, t); };
  collect(allProjectTabs(s));
  collect(Array.isArray(s.unmapped) ? s.unmapped : []);
  collect(Array.isArray(s.unassigned) ? s.unassigned : []);
  const allTabs = [...byId.values()];

  const meta = allTabs.filter((t) => resolveAltitude(t).band === "meta");
  // Inc9 (4): the on-demand SUPPORTERS band (between Méta and Orchestrateurs).
  const supporters = allTabs.filter((t) => resolveAltitude(t).band === "supporter");

  // An orchestrator is a LEAD (its own team), never a chain-worker under a parent,
  // even when it was itself spawned by another orchestrator (parentTabId set).
  const leadIds = new Set(allTabs.filter((t) => String(t && t.role || "").toLowerCase() === "orchestrator").map((t) => t.id));
  const orchestrators = allTabs
    .filter((t) => String(t && t.role || "").toLowerCase() === "orchestrator")
    .map((lead) => {
      // Nesting source (Inc8.2): the spawn lineage `parentTabId` (persistent, wins
      // when set) OR, as a fallback when a tab has NO lineage, the DECLARED card
      // field `orchestrator` (set-orchestrator = single-source living-card). The
      // fallback only fires when parentTabId is absent, so a tab never lands under
      // two leads. Sentinel values ("free"/"meta") never equal a lead UUID.
      const workers = allTabs.filter((t) => {
        if (!t || t.id === lead.id || leadIds.has(t.id)) return false;
        // Inc9 : le statut SUPPORTER/META a PRÉCÉDENCE sur le nesting parentTabId/orchestrator.
        // Un supporter fleet-wide (meta/scope/review non-daemon) reste en bande Supporters MÊME
        // s'il a une arête parent dispatch-dérivée -> il ne nest jamais comme worker d'un lead.
        const b = resolveAltitude(t).band;
        if (b === "supporter" || b === "meta") return false;
        return t.parentTabId ? t.parentTabId === lead.id : t.orchestrator === lead.id;
      });
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

  // Assign each unique tab to exactly ONE band. Priority: meta -> chain (lead +
  // its workers) -> orphan Workers -> everything else Freelancers.
  const placed = new Set();
  meta.forEach((t) => placed.add(t.id));
  supporters.forEach((t) => placed.add(t.id));
  for (const o of orchestrators) { placed.add(o.lead.id); for (const r of o.repos) for (const w of r.workers) placed.add(w.id); }
  const workers = allTabs.filter((t) => !placed.has(t.id) && resolveAltitude(t).band === "worker");
  workers.forEach((t) => placed.add(t.id));
  const freelancers = allTabs.filter((t) => !placed.has(t.id));

  return { meta, supporters, orchestrators, workers, freelancers };
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
  const allTabs = allProjectTabs(s);
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

// Band labels — single source. Two schemes coexist by design (see renderGrid's
// three layers): ALTITUDE_LABELS drives the Inc5/I2 fallback altitude bands (EN,
// matched by the I2 accept as `data-band-label`); INC7_BANDS drives the Inc7
// 4-band org-chart (the `id` is load-bearing — matched via `data-band` — and the
// FR `label` is display-only).
const ALTITUDE_LABELS = { 0: "tichef", 1: "orchestrators", 2: "workers" };
const INC7_BANDS = [
  { id: "meta", label: "Méta" },
  { id: "supporters", label: "Supporters" },
  { id: "orchestrators", label: "Orchestrateurs" },
  { id: "workers", label: "Workers" },
  { id: "freelancers", label: "Freelancers" },
];

function bandHtml(label, inner) {
  return `<div class="altitude-band" data-band-label="${escapeHtml(label)}">${inner}</div>`;
}

// An assignment-less tab in the UNASSIGNED band — legitimate, NOT an error (#90).
function unassignedTabHtml(tab) {
  return tabDiv("unassigned-tab", tab, "tab");
}

// Pure: the per-tab task/sub-agents render model (Inc7 S4-web). Reads the rust
// S4 fields (camelCase on DashboardTab): currentTask (string) + subAgents[]
// ({name, state}). Returns a chip list: one "task" chip (label = the task) then
// one "subagent" chip per invoked Task() (carrying its name + state). A tab with
// no transcript data -> [] (degrades cleanly). Null-safe.
export function taskChips(tab) {
  const t = tab || {};
  const chips = [];
  // The task pill = the LATEST DECLARED permalog phrase (`currentTaskLog`, via
  // append_current_task) — NOT the OBSERVED transcript `currentTask` ("latest
  // human-typed prompt"), which bleeds dispatched/broadcast messages onto every
  // recipient (Inc9 (1) bug: many agents showed the same "[[ici MAS…]]" pill).
  // A FREE agent shows NO task pill (the 'libre' badge speaks for it). Ponytail:
  // the permalog is append-only, so a non-free agent that went idle without a new
  // append keeps its last declared phrase — acceptable; the fix targets free + bleed.
  const log = Array.isArray(t.currentTaskLog) ? t.currentTaskLog : [];
  const latest = log.length ? String(log[log.length - 1]) : "";
  if (t.orchestrator !== "free" && latest.trim()) {
    chips.push({ kind: "task", label: latest });
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
        ? `<span class="task-chip" title="current task : ${escapeHtml(c.label)}">${escapeHtml(c.label)}</span>`
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

// Activation gate: a coordinated fleet (the tichef/manager is present) gets the
// Inc7 4-band org-chart; states without it keep the Inc5/Inc6 views. The tichef
// lives in state.unmapped (a méta tab, not under a project), so scan EVERY tab —
// projects + top-level unmapped + unassigned — not just the project tabs.
export function hasTichef(state) {
  const s = state || {};
  const loose = [
    ...(Array.isArray(s.unmapped) ? s.unmapped : []),
    ...(Array.isArray(s.unassigned) ? s.unassigned : []),
  ];
  return [...allProjectTabs(s), ...loose].some((t) => isTichefRole(t && t.role));
}

// Inner content of a band node: name + renfort badge (S2) + task/sub-agent chips
// (S4). Factored so the S3 in-place patch can refresh it without recreating the
// element (identity + scroll survive).
function bandNodeInner(tab) {
  const t = tab || {};
  const alt = resolveAltitude(t);
  const badge = alt.reinforcement ? ` <span class="renfort-badge" title="en renfort dans ${escapeHtml(alt.team || "")}">renfort</span>` : "";
  // Inc8 S3: the supervision-rounds pill on an orchestrator node (green/grey).
  const pill = isOrchestrator(t.role)
    ? ` <span class="rounds-pill ${roundsPill(t).cls}" title="supervision rounds ${roundsPill(t).active ? "active" : "idle"}"></span>`
    : "";
  // Inc8 S1: the self-declared agent card rendered inline — objective + latest
  // currentTask phrase + a 'libre' badge when the agent is free.
  const ac = agentCard(t);
  const free = ac.free ? ` <span class="free-badge">libre</span>` : "";
  // Compact band: the objective is a short declared line (truncated 1-line via CSS,
  // full text on hover). The currentTask is NOT dumped here — it lives only in the
  // grey truncated `.task-chip` pill (hover-full) + the right-click card (full permalog).
  const obj = ac.objective ? `<span class="agent-objective" title="${escapeHtml(ac.objective)}">${escapeHtml(ac.objective)}</span>` : "";
  const inline = obj ? `<span class="agent-card-inline">${obj}</span>` : "";
  // Inc9 b2: context-usage pill (green/amber/red), nothing when context_pct absent.
  const cp = contextPill(t);
  const ctx = cp ? ` <span class="ctx-pill ${cp.cls}" title="context usage ${cp.pct}%">${escapeHtml(cp.label)}</span>` : "";
  // Inc9 b3: discreet compaction badge when the tab was recently compacted.
  const comp = compactionBadge(t).show ? ` <span class="compact-badge" title="contexte compacté récemment">⟳</span>` : "";
  return `${escapeHtml(t.name || "tab")}${badge}${pill}${free}${ctx}${comp}${inline}${taskChipsHtml(t)}`;
}

function bandNodeHtml(tab, cls) {
  const t = tab || {};
  const led = ledClass(t.led != null ? t.led : t.rollupLed);
  const reinf = resolveAltitude(t).reinforcement ? " reinforcement" : "";
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
// change also produces an `update` op. Order-independent (keyed by id). Exported so
// the Zoetrope invariant (task change => 1 update op) is directly testable (Q7).
export function buildBandModel(state) {
  const bl = bandLayout(state);
  const nodes = [];
  bandTabById = new Map();
  const push = (t) => {
    if (!t || !t.id) return;
    bandTabById.set(t.id, t);
    const led = t.led != null ? t.led : (t.rollupLed != null ? t.rollupLed : null);
    const subs = Array.isArray(t.subAgents) ? t.subAgents.map((s) => s && `${s.name}:${s.state}`).join(",") : "";
    // The rounds pill (S3) + inline card fields (S1) join the signature so they
    // patch live without a rebuild.
    const rounds = roundsPill(t).active ? "1" : "0";
    const ac = agentCard(t);
    const card = `${ac.objective}|${t.orchestrator || ""}`;
    // Inc9 b2/b3: the context pill + compaction badge join the signature so they
    // patch live (flicker-free) when context_pct / recently_compacted change.
    const cp = contextPill(t);
    const ctx = `${cp ? cp.pct + cp.cls : ""}|${compactionBadge(t).show ? "1" : "0"}`;
    // Signature keys off the DECLARED task (what the pill shows), not the observed
    // transcript prompt — so a broadcast no longer churns every recipient's node.
    nodes.push({ id: t.id, led, task: `${ac.currentTask}|${subs}|r${rounds}|${card}|${ctx}` });
  };
  bl.meta.forEach(push);
  bl.supporters.forEach(push);
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
  const inner = {
    meta: bl.meta.map((t) => bandNodeHtml(t, "meta")).join(""),
    supporters: bl.supporters.map((t) => bandNodeHtml(t, "supporter")).join(""),
    orchestrators: bl.orchestrators.map(orchChainHtml).join(""),
    workers: bl.workers.map((t) => bandNodeHtml(t, "worker")).join(""),
    freelancers: bl.freelancers.map((t) => bandNodeHtml(t, "freelancer")).join(""),
  };
  grid.innerHTML = INC7_BANDS.map((b) => bandHtml7(b.id, b.label, inner[b.id])).join("");
  const layer = document.getElementById("lineage-layer");
  if (layer) layer.innerHTML = "";
  currentNodes = new Map();
  currentUnmapped = [];
  renderRehome([]);
  renderUnmapped();
}

// --- Inc6 S2/S4: org-chart (méta on top / team lead + workers / serving joins) ---

// Service grouping for the org-chart: a family service wraps its sub-repo teams.
// A thin alias over serviceLayout (kept as its own name for the call sites/tests).
function serviceGrouping(state) {
  return serviceLayout(state);
}

// Shared shape for the compact org-chart tab boxes: a div carrying a class + the
// viewer url + the (escaped) name with a fallback. metaTop / team-member /
// unassigned only differ by their class and fallback name.
function tabDiv(cls, tab, fallbackName) {
  const t = tab || {};
  return `<div class="${cls}" data-viewer="${escapeHtml(t.viewerUrl || "")}">${escapeHtml(t.name || fallbackName)}</div>`;
}

function metaTopHtml(tab) {
  return tabDiv("meta-top-tab", tab, "méta");
}

function teamMemberHtml(tab, cls) {
  return tabDiv(cls, tab, "tab");
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

// Level-0 overview. THREE layers, tried in order (audit #357 Q2 — the fallbacks
// are KEPT on purpose):
//   1. tichef present   -> Inc7 4-band compact org-chart (renderBandChart).
//   2. else + services   -> Inc6 org-chart (renderOrgChart).
//   3. else (no tichef)  -> Inc6-S6 altitude overview (this function's body).
// The no-tichef layers are the safety net for the transient window when the tichef
// is being re-homed (its UUID changes, so `hasTichef` briefly reads false): the
// dashboard degrades gracefully instead of blanking.
function renderGrid() {
  const grid = document.getElementById("project-grid");
  if (!grid) return;
  // Layer 1 — Inc7: a coordinated fleet (tichef present) gets the 4-band compact
  // org-chart, refreshed flicker-free (in-place patch between structural changes).
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
    const label = ALTITUDE_LABELS[a] || `altitude ${a}`;
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

// Shared popup open: cancel the hide timer, fill, show, anchor. No-op if there's
// nothing to show (keeps the empty-popup guard both callers had).
function openPopup(anchorEl, title, led, tabs) {
  const popup = document.getElementById("popup");
  if (!popup || !tabs || !tabs.length) return;
  clearTimeout(hideTimer);
  popup.innerHTML = popupHtml(title, led, tabs);
  popup.hidden = false;
  positionPopup(popup, anchorEl.getBoundingClientRect());
}

function showPopupFor(phase, anchorEl) {
  const node = currentNodes.get(phase);
  if (!node) return;
  openPopup(anchorEl, phase, node.rollupLed, node.tabs);
}

// Hover a project CARD (level 0) -> tooltip listing its occupants with details.
function showPopupForProject(name, anchorEl) {
  if (!currentState) return;
  const projects = Array.isArray(currentState.projects) ? currentState.projects : [];
  const project = projects.find((p) => p && p.name === name);
  if (!project) return;
  openPopup(anchorEl, name, project.rollupLed, projectTabs(project));
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

// Inc8 S3: build the agent-card HTML from the pure agentCardView model. Only the
// present sections render (evaluations/evalCriteria are optional).
// A possibly-long value rendered with a 'voir plus' toggle past 50 words (Inc9 (2)).
// The FULL text rides in data-full so the delegated click handler swaps it in.
function clippedHtml(value) {
  const cw = clipWords(value);
  if (!cw.clipped) return escapeHtml(cw.text);
  // The clipped prefix AND the toggle live in one .ac-clip container carrying the
  // full text, so expanding replaces the whole container — no duplicated prefix.
  return `<span class="ac-clip" data-full="${escapeHtml(cw.full)}">${escapeHtml(cw.text)} <span class="ac-more" role="button" tabindex="0">voir plus</span></span>`;
}

function agentCardHtml(tab) {
  const c = agentCardView(tab);
  const rows = [];
  // Inc9-B: FULL fiche d'identité — les champs DÉCLARÉS sont TOUJOURS affichés
  // (« — » quand vides) pour une carte complète et prévisible. `val` rend un champ
  // texte (clippé 50 mots) ou un « — » discret.
  const val = (s) => (s && String(s).trim() ? clippedHtml(s) : `<span class="ac-empty">—</span>`);
  rows.push(`<div class="ac-row"><span class="ac-key">assignment</span> ${val(c.assignment)}</div>`);
  rows.push(`<div class="ac-row"><span class="ac-key">specialty</span> ${val(c.specialty)}</div>`);
  rows.push(`<div class="ac-row"><span class="ac-key">orchestrator</span> ${escapeHtml(String(c.orchestrator || "—"))}${c.free ? ` <span class="ac-free">libre</span>` : ""}</div>`);
  rows.push(`<div class="ac-row"${c.objective ? ` title="objective : ${escapeHtml(c.objective)}"` : ""}><span class="ac-key">objective</span> ${val(c.objective)}</div>`);
  // currentTask permalog (déclaré) — les dernières entrées, ou « — (libre) ».
  if (c.recentTasks.length) {
    rows.push(`<div class="ac-key">currentTask (permalog)</div><ul class="ac-log">${c.recentTasks.map((t) => `<li title="Current task : ${escapeHtml(t)}">${clippedHtml(t)}</li>`).join("")}</ul>`);
  } else {
    rows.push(`<div class="ac-row"><span class="ac-key">currentTask</span> <span class="ac-empty">— (libre)</span></div>`);
  }
  // Inc8 S4: evaluations (verdict + trigger + records) — TOUJOURS, « — » si aucune.
  const es = evalSummary(tab);
  if (es.verdict || es.recent.length) {
    const trigger = es.triggerArmed
      ? ` <span class="eval-trigger armed" data-trigger="armed" title="auto-improvement trigger armed (over the 1/1M error budget or a 3-error burst)">⚠ trigger armé</span>`
      : ` <span class="eval-trigger" title="within budget">within budget</span>`;
    if (es.verdict) rows.push(`<div class="ac-row"><span class="ac-key">verdict</span> ${escapeHtml(es.verdict)}${trigger}</div>`);
    if (es.recent.length) {
      rows.push(`<div class="ac-key">evaluations</div><ul class="ac-evals">${es.recent.map((e) => {
        const errors = e && e.scores && e.scores.errors != null ? ` <span class="ac-scores">(err ${escapeHtml(String(e.scores.errors))})</span>` : "";
        return `<li>${escapeHtml((e && e.evaluator) || "?")}: ${escapeHtml(String((e && e.verdict) != null ? e.verdict : ""))}${errors}</li>`;
      }).join("")}</ul>`);
    }
  } else {
    rows.push(`<div class="ac-row"><span class="ac-key">evaluations</span> <span class="ac-empty">—</span></div>`);
  }
  // evalCriteria — TOUJOURS, « — » si aucun.
  if (c.evalCriteria.length) {
    rows.push(`<div class="ac-key">evalCriteria</div><ul class="ac-crit">${c.evalCriteria.map((x) => `<li>${clippedHtml(String(x))}</li>`).join("")}</ul>`);
  } else {
    rows.push(`<div class="ac-row"><span class="ac-key">evalCriteria</span> <span class="ac-empty">—</span></div>`);
  }
  // Inc8 conventions fold: the declared .md files, or a FLAG when none are declared.
  const cv = conventionsCheck(tab);
  if (cv.declared) {
    rows.push(`<div class="ac-key">conventions</div><ul class="ac-conv">${cv.conventions.map((x) => `<li title="${escapeHtml(String(x))}">${escapeHtml(String(x))}</li>`).join("")}</ul>`);
  } else {
    rows.push(`<div class="ac-row conventions-missing" data-conventions="missing"><span class="ac-key">conventions</span> <span class="conv-flag">⚠ aucune convention déclarée</span></div>`);
  }
  // usage (usageCount / lastUsedAt) — TOUJOURS, « — » si non exposé.
  const usageParts = [];
  if (tab && tab.usageCount != null) usageParts.push(`used ${escapeHtml(String(tab.usageCount))}×`);
  if (tab && tab.lastUsedAt != null) usageParts.push(`last ${escapeHtml(String(tab.lastUsedAt))}`);
  rows.push(`<div class="ac-row"><span class="ac-key">usage</span> ${usageParts.length ? usageParts.join(" · ") : `<span class="ac-empty">—</span>`}</div>`);
  // roundsActive — TOUJOURS (active/idle pour un orchestrateur, « — » sinon).
  rows.push(`<div class="ac-row"><span class="ac-key">roundsActive</span> ${tab && tab.roundsActive != null ? (roundsPill(tab).active ? "active" : "idle") : `<span class="ac-empty">—</span>`}</div>`);
  const name = tab && tab.name ? tab.name : "agent";
  // Inc9 (3): restore "open the agent's tab in the browser". The viewerUrl is
  // relative, so window.open resolves it against the CURRENT origin — when the
  // dashboard is viewed over the LAN/remote share URL, that IS the remote link.
  const viewer = tab && tab.viewerUrl ? tab.viewerUrl : "";
  const openLink = viewer
    ? ` <button class="ac-open" data-viewer="${escapeHtml(viewer)}" title="ouvrir l'onglet dans le navigateur (lien distant)" aria-label="ouvrir l'onglet dans le navigateur">↗</button>`
    : "";
  return `<button class="ac-close" title="close" aria-label="close">×</button><div class="ac-name">${escapeHtml(name)}${openLink}</div>${rows.join("")}`;
}

function openAgentCard(id) {
  const el = document.getElementById("agent-card");
  const tab = bandTabById.get(id);
  if (!el || !tab) return;
  el.innerHTML = agentCardHtml(tab);
  // Inc9 (3): carry the tab's viewer URL on the popup root so a right-click on a
  // FREE ZONE of the card opens the remote tab (via the contextmenu → openViewerFrom
  // path); the visible ↗ button is the discoverable equivalent.
  if (tab.viewerUrl) el.dataset.viewer = tab.viewerUrl; else delete el.dataset.viewer;
  el.hidden = false;
}

function closeAgentCard() {
  const el = document.getElementById("agent-card");
  if (el) el.hidden = true;
}

// --- Catalogue #39 SC2: on-demand overlay over the catalog read-model ---
// The read-only catalog read-model (camelCase, same page token as the dashboard).
const CATALOG_URL = "/catalog/list";

// The server verdict (camelCase, VERBATIM) -> {cls, label}. Never pass/fail: the
// class encodes the DIRECTION (or the explicit insufficient-sample case). The rust
// owns the G1 guard — no JS threshold here.
function verdictBadge(m) {
  const map = {
    freshFavored: { cls: "fvr-fresh", label: "fresh favorisé" },
    resumeFavored: { cls: "fvr-resume", label: "resume favorisé" },
    inconclusive: { cls: "fvr-inconclusive", label: "non concluant" },
    insufficientSample: { cls: "fvr-insufficient", label: "échantillon trop petit, pas de verdict" },
  };
  const v = map[m.verdict] || map.inconclusive;
  const n = `fresh n=${m.freshN} · resume n=${m.resumeN}`;
  return `<span class="fvr-verdict ${v.cls}" data-verdict="${escapeHtml(m.verdict)}">${escapeHtml(v.label)} · ${escapeHtml(n)}</span>`;
}

// The fresh-vs-resume metrics table for a skill (byMode ledger).
function metricsTableHtml(skill) {
  const m = byModeMetricsModel(skill);
  const num = (x) => (x == null ? "—" : x);
  const row = (label, mode) =>
    `<tr><th scope="row">${label}</th><td>${mode.spawns}</td><td>${mode.success}</td><td>${mode.problem}</td><td>${num(mode.tokensAvg)}</td><td>${num(mode.costAvg)}</td></tr>`;
  return `<div class="cat-metrics">
    <table class="metrics-table"><thead><tr><th></th><th>spawns</th><th>success</th><th>problem</th><th>tokensAvg</th><th>costAvg</th></tr></thead>
    <tbody>${row("fresh", m.fresh)}${row("resume", m.resume)}</tbody></table>
    <div class="fvr-line">fresh_vs_resume : ${verdictBadge(m)}</div>
  </div>`;
}

// One skill row: a header (proper name + version) that toggles a collapsible body
// (profile + metrics). The long prompt reuses the 'voir plus' fold (clippedHtml).
function catalogSkillHtml(skill) {
  const p = skillProfileModel(skill);
  const deleted = !!(skill && (skill.deleted === true || skill.tombstoned === true));
  const list = (label, xs) => (xs.length ? `<div class="cat-field"><span class="cat-key">${label}</span> ${xs.map((x) => `<span class="cat-tag">${escapeHtml(x)}</span>`).join(" ")}</div>` : "");
  const ver = p.promptVersion != null ? ` <span class="cat-ver">v${escapeHtml(String(p.promptVersion))}</span>` : "";
  const pvAttr = p.promptVersion != null ? escapeHtml(String(p.promptVersion)) : "";
  // SC3: the edit form (specialty / prompt / conventions) + delete / restore.
  const editForm = `<form class="cat-edit" data-skill="${escapeHtml(p.name)}" data-prompt-version="${pvAttr}">
      <div class="cat-edit-row"><label>specialty</label><input class="cat-edit-specialty" type="text" value="${escapeHtml(p.specialty)}"></div>
      <div class="cat-edit-row"><label>prompt</label><textarea class="cat-edit-prompt" rows="4">${escapeHtml(p.prompt)}</textarea></div>
      <div class="cat-edit-row"><label>conventions<br><small>(un .md par ligne)</small></label><textarea class="cat-edit-conventions" rows="2">${escapeHtml(p.conventions.join("\n"))}</textarea></div>
      <div class="cat-edit-actions">
        <button type="button" class="cat-save">Enregistrer</button>
        ${deleted
          ? `<button type="button" class="cat-restore" data-skill="${escapeHtml(p.name)}">Restaurer</button>`
          : `<button type="button" class="cat-delete" data-skill="${escapeHtml(p.name)}">Supprimer</button>`}
        <span class="cat-edit-msg" role="status"></span>
      </div>
    </form>`;
  return `<div class="cat-skill${deleted ? " cat-deleted" : ""}" data-skill="${escapeHtml(p.name)}">
    <button class="cat-skill-head" aria-expanded="false"><span class="cat-caret">▸</span> <span class="cat-name">${escapeHtml(p.name)}</span>${ver}${deleted ? ` <span class="cat-tombstone">supprimé</span>` : ""}</button>
    <div class="cat-skill-body" hidden>
      ${p.specialty ? `<div class="cat-field"><span class="cat-key">specialty</span> ${escapeHtml(p.specialty)}</div>` : ""}
      ${p.prompt ? `<div class="cat-field"><span class="cat-key">prompt</span> <span class="cat-prompt">${clippedHtml(p.prompt)}</span></div>` : ""}
      ${list("conventions", p.conventions)}
      ${list("tools", p.tools)}
      ${list("patterns", p.patterns)}
      ${metricsTableHtml(skill)}
      ${editForm}
    </div>
  </div>`;
}

function catalogHtml(readModel) {
  const skills = catalogView(readModel);
  const body = skills.length
    ? skills.map(catalogSkillHtml).join("")
    : `<div class="cat-empty">Aucun skill au catalogue.</div>`;
  // SC3-toggle: "afficher les supprimés" -> re-fetch with ?includeDeleted so the
  // tombstoned cards (deleted:true) show, making the Restore button reachable.
  return `<div class="cat-header">
      <span class="cat-title">Catalogue des skills</span>
      <span class="cat-count">${skills.length} skill${skills.length === 1 ? "" : "s"}</span>
      <label class="cat-deleted-toggle"><input type="checkbox" class="cat-show-deleted"${catalogIncludeDeleted ? " checked" : ""}> afficher les supprimés</label>
      <button class="cat-refresh" title="rafraîchir">↻</button>
      <button class="cat-close" title="fermer" aria-label="fermer">×</button>
    </div>
    <div class="cat-list">${body}</div>`;
}

let catalogOpen = false;
// SC3-toggle: whether the current fetch asks the server for tombstoned skills.
let catalogIncludeDeleted = false;

async function openCatalog() {
  const el = document.getElementById("catalog-panel");
  if (!el) return;
  catalogOpen = true;
  el.innerHTML = `<div class="cat-header"><span class="cat-title">Catalogue des skills</span></div><div class="cat-loading">chargement…</div>`;
  el.hidden = false;
  try {
    const url = catalogIncludeDeleted ? `${CATALOG_URL}?includeDeleted=true` : CATALOG_URL;
    const res = await fetch(url, { headers: { accept: "application/json", ...AUTH_HEADERS } });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    el.innerHTML = catalogHtml(await res.json());
  } catch (err) {
    el.innerHTML = `<div class="cat-header"><span class="cat-title">Catalogue des skills</span><button class="cat-close" title="fermer" aria-label="fermer">×</button></div><div class="cat-error">catalogue indisponible (${escapeHtml(err.message)})</div>`;
  }
}

function closeCatalog() {
  const el = document.getElementById("catalog-panel");
  if (el) { el.hidden = true; catalogOpen = false; }
}

// SC3: a catalog mutation (edit/delete/restore) with the page token. Returns the
// Response so the caller can read 2xx (refresh) vs 409 (show the server error).
function catalogPost(skill, verb, body) {
  return fetch(`/catalog/${encodeURIComponent(skill)}/${verb}`, {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json", ...AUTH_HEADERS },
    body: JSON.stringify(body || {}),
  });
}

// Handle a click on a SC3 edit-form control (save/delete/restore). Returns true if
// it handled the target. Async: refreshes the read-model after a 2xx (server = the
// source of truth, no optimistic mutation).
async function handleCatalogEdit(target) {
  const save = target.closest && target.closest(".cat-save");
  if (save) {
    const form = save.closest(".cat-edit");
    const msg = form.querySelector(".cat-edit-msg");
    const built = editBody({
      prompt: form.querySelector(".cat-edit-prompt").value,
      specialty: form.querySelector(".cat-edit-specialty").value,
      conventions: form.querySelector(".cat-edit-conventions").value,
      promptVersion: form.dataset.promptVersion,
    });
    if (!built.ok) { msg.textContent = built.error; msg.className = "cat-edit-msg err"; return true; }
    try {
      const res = await catalogPost(form.dataset.skill, "edit", built.body);
      if (res.ok) { openCatalog(); }
      else { const t = (await res.text().catch(() => "")) || `HTTP ${res.status}`; msg.textContent = `erreur ${res.status} : ${t}`; msg.className = "cat-edit-msg err"; }
    } catch (err) { msg.textContent = `erreur réseau : ${err.message}`; msg.className = "cat-edit-msg err"; }
    return true;
  }
  const del = target.closest && target.closest(".cat-delete");
  if (del) {
    const skill = del.dataset.skill;
    // STICKY deletion — restaurable only via an explicit Restore (strong confirm).
    if (typeof confirm === "function" && !confirm(`Supprimer « ${skill} » du catalogue ?\n\nSuppression STICKY — restaurable UNIQUEMENT via l'action Restore explicite.`)) return true;
    try { const res = await catalogPost(skill, "delete", {}); if (res.ok) openCatalog(); } catch { /* ignore */ }
    return true;
  }
  const restore = target.closest && target.closest(".cat-restore");
  if (restore) {
    try { const res = await catalogPost(restore.dataset.skill, "restore", {}); if (res.ok) openCatalog(); } catch { /* ignore */ }
    return true;
  }
  return false;
}

// ===== KIOSK #kiosk (PD2): the cross-project pending-decisions panel =====
// Same ossature as the catalogue: a topbar button opens a cold overlay fetched
// on-demand (NOT in the 1.5s poll — a separate cold source). The server read-model is
// rendered VERBATIM: state / verdict / visibility are the fold's call, no JS re-gate.
const DECISIONS_URL = "/decisions";
let kioskOpen = false;
let kioskIncludeArchived = false;

// The server read-model -> the decisions array (tolerate {decisions:[…]} or a bare array).
export function kioskView(readModel) {
  if (readModel && Array.isArray(readModel.decisions)) return readModel.decisions;
  return Array.isArray(readModel) ? readModel : [];
}

// state -> a short human label (visual vocabulary distinct from the living-cards).
const DECISION_STATE_LABEL = { open: "à trancher", read: "lu", tranched: "tranché", archived: "archivé" };
// open first, then the read->tranched->archived progression.
const DECISION_STATE_ORDER = { open: 0, read: 1, tranched: 2, archived: 3 };

// Render a decision's long-form `detail` as SAFE simple-markdown for the toggle body:
// escape EVERYTHING first (XSS), then re-introduce only <strong> (**bold**) and <br>
// (line breaks). No raw HTML from the payload ever reaches the DOM.
export function renderDetail(text) {
  return escapeHtml(String(text == null ? "" : text))
    .replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\r?\n/g, "<br>");
}

// Kiosk deploy seam: base for turning a bare code-source ref into a clickable repo blob
// link. The dashboard can't know the running checkout's remote/branch, so it reads a
// <meta name="repo-blob-base"> (dashboard.html) with this default (the mx fork tip). An
// empty value disables construction → the code ref degrades to honest copyable text.
const REPO_BLOB_BASE = (function () {
  const dflt = "https://github.com/a-biskoazh/tab-atelier-mx/blob/mx/live";
  if (typeof document === "undefined") return dflt; // node (unit tests) → the default
  const m = document.querySelector('meta[name="repo-blob-base"]');
  const v = m && m.getAttribute("content");
  return v == null ? dflt : v.trim(); // present-but-empty meta explicitly disables
})();

// Build a repo blob URL from a bare code-source ref (`path:line` / `path:start-end`):
// <REPO_BLOB_BASE>/<path>#L<start>[-L<end>]. Encodes each path segment (keeps slashes),
// drops a leading ./ or /. No base configured → "" so the caller falls back to text.
export function codeRefBlobUrl(raw, base = REPO_BLOB_BASE) {
  if (!base) return "";
  const s = String(raw == null ? "" : raw).trim();
  const m = /^(.*?):(\d+)(?:-(\d+))?$/.exec(s);
  const path = (m ? m[1] : s).replace(/^\.?\//, "");
  if (!path) return "";
  const enc = path.split("/").map(encodeURIComponent).join("/");
  const anchor = m ? `#L${m[2]}${m[3] ? `-L${m[3]}` : ""}` : "";
  return `${base.replace(/\/$/, "")}/${enc}${anchor}`;
}

// FU2 (#kiosk) + follow-up fix: a decision's files[] mixes reference kinds that must NOT
// render the same way — and NONE of them may render as dead text (the FU2 regression):
//  - a SERVABLE DOC (a real .md under the served outbox zone, e.g. ~/Dev/outbox/x.md
//    or an _archive copy) → the sandboxed /decisions/file viewer legitimately serves
//    it (200). Keep the file-viewer link.
//  - a CODE-SOURCE REF (auth.rs:76-78, src/cli/decision.rs:520, a bare source path,
//    anything carrying a :line) → the viewer is anti-traversal-sandboxed and does NOT
//    serve repo sources → it 404s. So NEVER a /decisions/file link. Instead build a real
//    repo blob link (remote+branch from REPO_BLOB_BASE + path + #L anchor) so the reader
//    CLICKS through to the source. Pointing at the repo beats a dead span: even a drifted
//    line / bare path lands the reader in the right tree (GitHub's own file-finder). Only
//    when no base is configured does it fall back to honest copyable text — never a 404.
//  ponytail 🟡: a per-project server-injected remote+branch map would resolve bare
//  `auth.rs` to its full path + pin the exact commit (no line drift) — deferred.
export function classifyDecisionFile(f) {
  const raw = String(f == null ? "" : f).trim();
  // Already a full web URL (e.g. a github/blob link stored in files[]) → link as-is.
  if (/^https?:\/\//i.test(raw)) return { kind: "url", href: raw, label: raw };
  // Servable doc = under the served outbox zone AND a doc extension (ignore any :line).
  const bare = raw.replace(/:\d+(?:-\d+)?$/, "");
  const inOutbox = raw.startsWith("~/Dev/outbox/") || /(?:^|\/)Dev\/outbox\//.test(raw);
  const isDoc = /\.(?:md|markdown)$/i.test(bare);
  if (inOutbox && isDoc) return { kind: "doc", path: raw, label: raw };
  // Code-source ref → a clickable repo blob link when a base is configured (the default),
  // else honest copyable text. Never the 404 viewer link.
  const href = codeRefBlobUrl(raw);
  return href ? { kind: "code", href, label: raw } : { kind: "code", label: raw };
}

// Render one files[] entry per its kind. `canRule` gates the viewer token, as before.
// XSS-safe: every value is escaped before it reaches the DOM.
export function decisionFileHtml(f, canRule) {
  const c = classifyDecisionFile(f);
  if (c.kind === "doc") {
    const href = `/decisions/file?path=${encodeURIComponent(c.path)}${canRule ? `&token=${encodeURIComponent(TOKEN)}` : ""}`;
    return `<a class="kk-file" href="${escapeHtml(href)}" target="_blank" rel="noopener">${escapeHtml(c.label)}</a>`;
  }
  if (c.kind === "url") {
    return `<a class="kk-file kk-file-repo" href="${escapeHtml(c.href)}" target="_blank" rel="noopener">${escapeHtml(c.label)}</a>`;
  }
  // Code ref: a clickable repo blob link (NO /decisions/file, NO 404). Falls back to
  // copyable text only when no repo base is configured (c.href absent).
  if (c.href) {
    return `<a class="kk-file kk-file-repo" href="${escapeHtml(c.href)}" target="_blank" rel="noopener" title="ouvrir la source sur le repo">${escapeHtml(c.label)}</a>`;
  }
  return `<span class="kk-file-ref" role="button" tabindex="0" title="référence de code — clic pour copier" data-copy="${escapeHtml(c.label)}">${escapeHtml(c.label)}</span>`;
}

// One decision card: the 2-notch checkbox (Lu -> Tranché) in the head, the digest lines
// (title / why-gated / reco / effort), the file links, and a short verdict field. The
// checkboxes reflect the SERVER state (no optimistic UI); once reached, a notch is
// checked+disabled (state only progresses; there is no un-read / un-tranch route here).
// Kiosk detail-toggle: when the server ships a non-empty `detail`, a small (+)/(-) toggle
// reveals the long-form body (collapsed by default). No `detail` → no toggle (feature-detect).
export function decisionCardHtml(d) {
  const state = String(d.state || "open");
  const isRead = state === "read" || state === "tranched" || state === "archived";
  const isTranched = state === "tranched" || state === "archived";
  // Bug2 UX guard: no page token -> the daemon rejects Lu/Tranché (read-only dashboard
  // without the ruling scope). Disable the controls with a hint rather than fail silently.
  const canRule = typeof TOKEN === "string" && TOKEN.length > 0;
  const id = escapeHtml(String(d.id || ""));
  const line = (label, val) => (val ? `<div class="kk-field"><span class="kk-key">${label}</span> ${escapeHtml(String(val))}</div>` : "");
  const files = Array.isArray(d.files) ? d.files : [];
  // FU2: render each entry per its kind — a servable doc keeps the SANDBOXED viewer link
  // (Bug1: the raw outbox path 401s; the server confines it to the outbox + _archive
  // subtree), a code-source ref points at the repo / falls back to copyable text (never a
  // 404 /decisions/file link). See `classifyDecisionFile`.
  const links = files.length
    ? `<div class="kk-files">${files.map((f) => decisionFileHtml(f, canRule)).join("")}</div>`
    : "";
  // Lu is the only actionable notch; Tranché is a STATE INDICATOR (always disabled) — the
  // ruling action is the explicit "Trancher" button (Bug3), so submission is evident.
  const luDisabled = isRead || !canRule;
  const ruleDisabled = isTranched || !canRule;
  // Feature-detect: the toggle exists ONLY when a non-empty `detail` was served (a
  // detail-less decision degrades gracefully to no toggle, no empty body).
  const hasDetail = typeof d.detail === "string" && d.detail.trim().length > 0;
  const detailToggle = hasDetail
    ? ` <button type="button" class="kk-detail-toggle" aria-expanded="false" title="déplier le détail">(+)</button>`
    : "";
  const detailBody = hasDetail ? `<div class="kk-detail" hidden>${renderDetail(d.detail)}</div>` : "";
  return `<div class="kk-card kk-state-${escapeHtml(state)}" data-id="${id}" data-state="${escapeHtml(state)}">
    <div class="kk-head">
      <label class="kk-check"><input type="checkbox" class="kk-lu"${isRead ? " checked" : ""}${luDisabled ? " disabled" : ""}> Lu</label>
      <label class="kk-check"><input type="checkbox" class="kk-tranche" disabled${isTranched ? " checked" : ""}> Tranché</label>
      <span class="kk-title">${escapeHtml(String(d.title || d.id || ""))}</span>
      <span class="kk-state-tag">${escapeHtml(DECISION_STATE_LABEL[state] || state)}</span>${detailToggle}
    </div>
    ${line("pourquoi gaté", d.whyGated)}
    ${line("reco", d.reco)}
    ${line("effort", d.effort)}
    ${detailBody}
    ${links}
    <div class="kk-rule">
      <input type="text" class="kk-verdict-input" placeholder="verdict court…" value="${escapeHtml(String(d.verdict || ""))}"${ruleDisabled ? " disabled" : ""}>
      <button type="button" class="kk-send"${ruleDisabled ? " disabled" : ""}>Trancher</button>
      ${d.verdict ? `<span class="kk-verdict">verdict : ${escapeHtml(String(d.verdict))}</span>` : ""}
      ${canRule ? "" : `<span class="kk-hint">lecture seule — ouvrez le dashboard avec un token pour trancher</span>`}
      <span class="kk-msg" role="status"></span>
    </div>
  </div>`;
}

// FU2: copy a code-source ref's text to the clipboard (best-effort). Flashes a brief
// "copié" affordance on the element's title; a browser without clipboard access no-ops.
function copyDecisionFileRef(el) {
  const text = (el && el.dataset && el.dataset.copy) || (el && el.textContent) || "";
  if (!text) return;
  try {
    if (navigator && navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(() => {
        const prev = el.getAttribute("title");
        el.setAttribute("title", "copié ✓");
        setTimeout(() => el.setAttribute("title", prev || ""), 1200);
      }).catch(() => {});
    }
  } catch { /* no clipboard (older/embedded webview) — silent no-op */ }
}

// Flip one card's detail toggle: (+) collapsed <-> (-) expanded. Purely local (no fetch).
function toggleDetail(btn) {
  const card = btn.closest && btn.closest(".kk-card");
  const body = card && card.querySelector(".kk-detail");
  const expanded = btn.getAttribute("aria-expanded") === "true";
  btn.setAttribute("aria-expanded", String(!expanded));
  btn.textContent = expanded ? "(+)" : "(-)";
  if (body) body.hidden = expanded;
}

export function kioskHtml(readModel) {
  const decisions = kioskView(readModel);
  // Group by project (transverse); within a group, open first.
  const byProject = new Map();
  for (const d of decisions) {
    const p = d.project || "—";
    if (!byProject.has(p)) byProject.set(p, []);
    byProject.get(p).push(d);
  }
  const groups = [...byProject.keys()].sort().map((p) => {
    const cards = byProject.get(p).slice()
      .sort((a, b) => (DECISION_STATE_ORDER[a.state] ?? 9) - (DECISION_STATE_ORDER[b.state] ?? 9))
      .map(decisionCardHtml).join("");
    return `<div class="kk-group"><h3 class="kk-project">${escapeHtml(String(p))}</h3>${cards}</div>`;
  }).join("");
  const openCount = decisions.filter((d) => d.state === "open").length;
  const body = decisions.length ? groups : `<div class="kk-empty">Aucune décision en attente.</div>`;
  return `<div class="kk-header">
      <span class="kk-panel-title">Décisions en attente</span>
      <span class="kk-count">${openCount} à trancher</span>
      <label class="kk-archived-toggle"><input type="checkbox" class="kk-show-archived"${kioskIncludeArchived ? " checked" : ""}> afficher les archivées</label>
      <button class="kk-refresh" title="rafraîchir">↻</button>
      <button class="kk-close" title="fermer" aria-label="fermer">×</button>
    </div>
    <div class="kk-list">${body}</div>`;
}

// The badge = nb of OPEN decisions, from any decisions fetch (never the 1.5s poll —
// cold source). Hidden at zero. The open count is toggle-invariant (open !== archived).
function renderKioskBadge(decisions) {
  const badge = document.getElementById("kiosk-badge");
  if (!badge) return;
  const n = decisions.filter((d) => d && d.state === "open").length;
  badge.textContent = String(n);
  badge.hidden = n === 0;
}

function fetchDecisions() {
  const url = kioskIncludeArchived ? `${DECISIONS_URL}?includeArchived=true` : DECISIONS_URL;
  return fetch(url, { headers: { accept: "application/json", ...AUTH_HEADERS } }).then((res) => {
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    return res.json();
  });
}

async function refreshKioskBadge() {
  try { renderKioskBadge(kioskView(await fetchDecisions())); } catch { /* keep last */ }
}

async function openKiosk() {
  const el = document.getElementById("kiosk-panel");
  if (!el) return;
  kioskOpen = true;
  el.innerHTML = `<div class="kk-header"><span class="kk-panel-title">Décisions en attente</span></div><div class="kk-loading">chargement…</div>`;
  el.hidden = false;
  try {
    const model = await fetchDecisions();
    el.innerHTML = kioskHtml(model);
    renderKioskBadge(kioskView(model));
  } catch (err) {
    el.innerHTML = `<div class="kk-header"><span class="kk-panel-title">Décisions en attente</span><button class="kk-close" title="fermer" aria-label="fermer">×</button></div><div class="kk-error">décisions indisponibles (${escapeHtml(err.message)})</div>`;
  }
}

function closeKiosk() {
  const el = document.getElementById("kiosk-panel");
  if (el) { el.hidden = true; kioskOpen = false; }
}

// A KIOSK mutation (read/tranch) with the page token. Returns the Response.
function decisionPost(id, verb, body) {
  return fetch(`/decisions/${encodeURIComponent(id)}/${verb}`, {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json", ...AUTH_HEADERS },
    body: JSON.stringify(body || {}),
  });
}

// Submit the ruling for a card: POST /tranch with the typed verdict (non-empty required —
// the server 400s an empty one too). Driven by the explicit "Trancher" button + Enter.
async function submitTranch(card) {
  const msg = card.querySelector(".kk-msg");
  const fail = (t) => { if (msg) { msg.textContent = t; msg.className = "kk-msg err"; } };
  const verdict = (card.querySelector(".kk-verdict-input")?.value || "").trim();
  if (!verdict) { fail("un verdict est requis pour trancher"); return; }
  try { const res = await decisionPost(card.dataset.id, "tranch", { verdict }); if (res.ok) openKiosk(); else fail(`erreur ${res.status}`); }
  catch (err) { fail(`réseau : ${err.message}`); }
}

// Handle a click on the Lu checkbox or the "Trancher" button. The SERVER decides the new
// state; we re-fetch after a 2xx (no optimistic mutation). Returns true if handled.
async function handleKioskAction(target) {
  const card = target.closest && target.closest(".kk-card");
  if (!card) return false;
  if (target.closest(".kk-lu")) {
    const msg = card.querySelector(".kk-msg");
    const fail = (t) => { if (msg) { msg.textContent = t; msg.className = "kk-msg err"; } };
    try { const res = await decisionPost(card.dataset.id, "read", {}); if (res.ok) openKiosk(); else fail(`erreur ${res.status}`); }
    catch (err) { fail(`réseau : ${err.message}`); }
    return true;
  }
  if (target.closest(".kk-send")) { submitTranch(card); return true; }
  return false;
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
  // Right-click: on a band node (Inc7/Inc8) open its agent-card (Inc8 S3); on a
  // popup tab entry open its viewer.
  document.addEventListener("contextmenu", (e) => {
    const node = e.target.closest && e.target.closest(".band-node[data-tab-id]");
    if (node && node.dataset.tabId) { openAgentCard(node.dataset.tabId); e.preventDefault(); return; }
    // Inc9 (3): right-click on a FREE ZONE of the open agent-card (not its buttons)
    // opens the agent's tab in the browser (remote viewer).
    const card = e.target.closest && e.target.closest("#agent-card[data-viewer]");
    if (card && !(e.target.closest && e.target.closest(".ac-close, .ac-more, .ac-open"))) {
      const u = card.getAttribute("data-viewer");
      if (u) { window.open(viewerUrlWithToken(u, TOKEN), "_blank", "noopener"); e.preventDefault(); return; }
    }
    if (openViewerFrom(e.target)) e.preventDefault();
  });
  // Close the agent-card: its × button, Escape, or a click outside it.
  document.addEventListener("click", (e) => {
    const el = document.getElementById("agent-card");
    if (!el || el.hidden) return;
    // Inc9 (3): the ↗ button opens the agent's tab in a new browser (remote viewer).
    const open = e.target.closest && e.target.closest(".ac-open");
    if (open) { const u = open.getAttribute("data-viewer"); if (u) window.open(viewerUrlWithToken(u, TOKEN), "_blank", "noopener"); return; }
    // Inc9 (2): 'voir plus' expands the clipped field in place (safe text swap), and
    // must NOT close the card — handle it before the close logic.
    const more = e.target.closest && e.target.closest(".ac-more");
    if (more) { const clip = more.closest(".ac-clip"); if (clip) clip.textContent = clip.dataset.full || ""; return; }
    if (e.target.closest(".ac-close") || !e.target.closest("#agent-card")) closeAgentCard();
  });
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeAgentCard(); });

  // Catalogue #39 SC2: open the cold catalog overlay on-demand (NOT in the poll).
  const catToggle = document.getElementById("catalog-toggle");
  if (catToggle) catToggle.addEventListener("click", () => { catalogIncludeDeleted = false; openCatalog(); });
  const catPanel = document.getElementById("catalog-panel");
  if (catPanel) {
    catPanel.addEventListener("click", (e) => {
      // Handled in-panel: never let the document outside-click handler see it (a
      // refresh replaces innerHTML synchronously, which would detach e.target and
      // fool the outside-click guard into closing the panel).
      e.stopPropagation();
      if (e.target.closest(".cat-close")) { closeCatalog(); return; }
      if (e.target.closest(".cat-refresh")) { openCatalog(); return; }
      // SC3-toggle: "afficher les supprimés" re-fetches with ?includeDeleted so the
      // tombstoned cards (and their Restore button) become reachable.
      const showDel = e.target.closest(".cat-show-deleted");
      if (showDel) { catalogIncludeDeleted = !!showDel.checked; openCatalog(); return; }
      // SC3: edit-form controls (save/delete/restore) are async mutations.
      if (e.target.closest(".cat-save, .cat-delete, .cat-restore")) { handleCatalogEdit(e.target); return; }
      const head = e.target.closest(".cat-skill-head");
      if (head) {
        const body = head.parentElement && head.parentElement.querySelector(".cat-skill-body");
        const caret = head.querySelector(".cat-caret");
        if (body) {
          const willShow = body.hidden;
          body.hidden = !willShow;
          head.setAttribute("aria-expanded", willShow ? "true" : "false");
          if (caret) caret.textContent = willShow ? "▾" : "▸";
        }
      }
    });
  }
  // Close the catalog on Escape or a click outside it (but not the toggle button).
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeCatalog(); });
  document.addEventListener("click", (e) => {
    if (!catalogOpen) return;
    if (e.target.closest("#catalog-panel") || e.target.closest("#catalog-toggle")) return;
    closeCatalog();
  });

  // KIOSK PD2 (#kiosk): open the cold decisions overlay on-demand (NOT in the poll).
  const kioskToggle = document.getElementById("kiosk-toggle");
  if (kioskToggle) kioskToggle.addEventListener("click", () => { kioskIncludeArchived = false; openKiosk(); });
  const kioskPanel = document.getElementById("kiosk-panel");
  if (kioskPanel) {
    kioskPanel.addEventListener("click", (e) => {
      // In-panel: stop the document outside-click handler from seeing it (openKiosk
      // replaces innerHTML synchronously, detaching e.target — same trap as the catalogue).
      e.stopPropagation();
      if (e.target.closest(".kk-close")) { closeKiosk(); return; }
      if (e.target.closest(".kk-refresh")) { openKiosk(); return; }
      const showArch = e.target.closest(".kk-show-archived");
      if (showArch) { kioskIncludeArchived = !!showArch.checked; openKiosk(); return; }
      // Detail toggle: a LOCAL UI flip (no server round-trip, no re-fetch — re-rendering
      // would collapse it again). Expand/collapse the long-form body in place.
      const dt = e.target.closest(".kk-detail-toggle");
      if (dt) { toggleDetail(dt); return; }
      // FU2: a code-source ref is copyable text (not a link) — click copies it so the
      // reader can paste it into their editor. Best-effort (no clipboard → silent no-op).
      const ref = e.target.closest(".kk-file-ref");
      if (ref) { copyDecisionFileRef(ref); return; }
      if (e.target.closest(".kk-lu, .kk-send")) { handleKioskAction(e.target); return; }
    });
    // Bug3: Enter in the verdict field submits the ruling (as well as the "Trancher" button).
    kioskPanel.addEventListener("keydown", (e) => {
      const input = e.key === "Enter" && e.target.closest && e.target.closest(".kk-verdict-input");
      if (input) { e.preventDefault(); const card = input.closest(".kk-card"); if (card) submitTranch(card); return; }
      // FU2 a11y: a code-source ref is a role=button — Enter/Space copies it.
      const ref = (e.key === "Enter" || e.key === " ") && e.target.closest && e.target.closest(".kk-file-ref");
      if (ref) { e.preventDefault(); copyDecisionFileRef(ref); }
    });
  }
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeKiosk(); });
  document.addEventListener("click", (e) => {
    if (!kioskOpen) return;
    if (e.target.closest("#kiosk-panel") || e.target.closest("#kiosk-toggle")) return;
    closeKiosk();
  });
  // Seed the badge once at load (on-demand; the 1.5s poll stays clean of this cold source).
  refreshKioskBadge();

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
