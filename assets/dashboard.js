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

const POLL_MS = 1500;
const STATE_URL = "/dashboard/state";

// Live snapshot the popup reads from, refreshed each poll.
let currentNodes = new Map();
let currentUnmapped = [];

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
  currentNodes = nodeMap(state);
  currentUnmapped = state && Array.isArray(state.unmapped) ? state.unmapped : [];

  for (const phase of CANONICAL_PHASES) {
    const el = document.getElementById(`node-${phase}`);
    if (!el) continue;
    const node = currentNodes.get(phase);
    const led = node ? node.rollupLed : null;
    el.setAttribute("class", `node ${ledClass(led)}`);
    const count = node && node.tabs ? node.tabs.length : 0;
    const countEl = el.querySelector(".node-count");
    if (countEl) countEl.textContent = count ? String(count) : "";
  }

  renderUnmapped();
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
    const res = await fetch(STATE_URL, { headers: { accept: "application/json" } });
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
