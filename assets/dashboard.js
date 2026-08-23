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
  const orch = project && project.hasOrchestrator
    ? ` <span class="orch-badge" title="has an orchestrator">◆</span>`
    : "";
  const count = Number((project && project.tabCount) || 0);
  return `<button class="project-card ${led}${meta}" data-project="${esc(name)}">
    <span class="card-name">${esc(name)}${orch}</span>
    <span class="card-count">${count} tab${count === 1 ? "" : "s"}</span>
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

const POLL_MS = 1500;
const STATE_URL = "/dashboard/state";

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

// One tab entry inside the popup (or the unmapped list). data-viewer carries the
// viewerUrl for the right-click handler.
function tabEntryHtml(tab) {
  return `<li class="popup-tab" data-viewer="${escapeHtml(tab.viewerUrl || "")}">
    <span class="tab-name">${escapeHtml(tab.name)}</span>
    <span class="tab-role">${escapeHtml(tab.role || "—")}</span>
    <span class="tab-item">${escapeHtml(tab.item || "—")}</span>
    <span class="tab-state">${escapeHtml(tab.agentState || "—")}</span>
    <span class="tab-tokens">${escapeHtml(fmtTokens(tab.tokens))}</span>
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
  if (view.mode === "grid") renderGrid(view.projects);
  else renderDiagram(view);
  setViewChrome(view);
}

function renderGrid(projects) {
  const grid = document.getElementById("project-grid");
  if (!grid) return;
  // Server order, no client re-sort -> stable positions across reloads.
  grid.innerHTML = projects.map((p) => renderProjectCard(p, escapeHtml)).join("");
  currentNodes = new Map();
  currentUnmapped = [];
  renderUnmapped();
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

  renderUnmapped();
}

// Show/hide the level-0 grid vs the level-1 diagram (+ back button).
function setViewChrome(view) {
  const grid = document.getElementById("project-grid");
  const flow = document.getElementById("flow");
  const back = document.getElementById("back-btn");
  const isGrid = view.mode === "grid";
  if (grid) grid.hidden = !isGrid;
  if (flow) flow.hidden = isGrid;
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
  list.innerHTML = currentUnmapped.map(tabEntryHtml).join("");
}

let hideTimer = null;

function positionPopup(popup, anchorRect) {
  popup.style.left = `${Math.round(anchorRect.left + window.scrollX)}px`;
  popup.style.top = `${Math.round(anchorRect.bottom + window.scrollY + 8)}px`;
}

function showPopupFor(phase, anchorEl) {
  const popup = document.getElementById("popup");
  const node = currentNodes.get(phase);
  if (!popup || !node || !node.tabs || !node.tabs.length) return;
  clearTimeout(hideTimer);
  popup.innerHTML =
    `<div class="popup-title">${escapeHtml(phase)} · ${escapeHtml(node.rollupLed || "—")}</div>
     <ul class="popup-tabs">${node.tabs.map(tabEntryHtml).join("")}</ul>
     <div class="popup-hint">right-click a tab to open its viewer</div>`;
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
  if (url) window.open(url, "_blank", "noopener");
  return true;
}

async function poll() {
  const status = document.getElementById("status");
  try {
    const res = await fetch(STATE_URL, { headers: { accept: "application/json", ...AUTH_HEADERS } });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    applyState(await res.json());
    if (status) status.textContent = "live";
    if (status) status.className = "status ok";
  } catch (err) {
    if (status) status.textContent = `offline (${err.message})`;
    if (status) status.className = "status err";
  }
}

function bootstrap() {
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
  }
  // Back to the grid.
  const back = document.getElementById("back-btn");
  if (back) back.addEventListener("click", () => navigateTo(null, true));
  // Browser back/forward moves between grid and drilled project.
  window.addEventListener("popstate", () => {
    currentProject = readProjectParam(location.search);
    render();
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
