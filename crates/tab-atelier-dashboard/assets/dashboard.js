// SPDX-License-Identifier: MPL-2.0
// Harness control panel — Kiosk + Catalogue only (fungal-mode trim).
// ES module: the pure functions are exported so assets/tests/*.test.mjs can
// import them under Node. The DOM bootstrap at the bottom is guarded so the
// import stays side-effect-free off-browser.
"use strict";

// Pure: append the current page's share-token to a viewer URL so a right-click
// "open viewer" carries it. The viewer routes require a token, and the dashboard
// token is now a read-only observability credential for the whole fleet, so the
// page token is exactly what authorises the viewer. Host stays RELATIVE (works
// loopback AND behind a public host like amaury.wdes.eu). No url/token → passthrough.
export function viewerUrlWithToken(url, token) {
  if (!url || !token) return url || "";
  return url + (url.includes("?") ? "&" : "?") + "token=" + encodeURIComponent(token);
}

// The share-token the daemon gated this page on (master or the global dashboard
// token), carried in the page URL's `?token=` exactly like the tab viewer
// (main.js). Sent as `Authorization: Bearer` on every fetch so a remote,
// token-only load authorises. Guarded so importing this module under Node (the
// self-check) — where `location` is undefined — stays side-effect-free.
const TOKEN = typeof location === "undefined" ? "" : new URLSearchParams(location.search).get("token") || "";
const AUTH_HEADERS = TOKEN ? { Authorization: "Bearer " + TOKEN } : {};

function escapeHtml(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, (c) => (
    { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]
  ));
}

// Pure: clip a text to `max` words (Inc9 (2)). Returns the clipped text, whether it
// overflowed, and the FULL text — so the consumer renders a 'voir
// plus' toggle only past the threshold. Null-safe; collapses runs of whitespace.
export function clipWords(text, max = 50) {
  const s = String(text == null ? "" : text);
  const words = s.trim().split(/\s+/).filter(Boolean);
  if (words.length <= max) return { text: s, clipped: false, full: s };
  return { text: words.slice(0, max).join(" ") + "…", clipped: true, full: s };
}

// --- Catalogue #39 SC2 (reconciled to the live SC1 contract) ---
// GET /catalog/list -> { retired, skills }. A skill's fold key is `skill`; metrics
// are `metrics.byMode.{fresh,resume}{spawns,success,problem,tokensAvg,costAvg}`; the
// A/B compare is an OBJECT `freshVsResume{verdict, freshN, resumeN, deliveryDelta,
// tokensRatio}`. The RUST is the single source of the G1 guard (MIN_SAMPLE=3) — the
// web renders the server verdict VERBATIM, never re-gates (no MIN_SAMPLE in JS).
// The catalogue is a COLD source, fetched on-demand (RB2).

// Pure: the read-model's skills -> a deterministic list (sorted by the fold key
// `skill`). Tombstoned skills are already filtered server-side. Null-safe.
export function catalogView(readModel) {
  const skills = readModel && Array.isArray(readModel.skills) ? readModel.skills : [];
  return skills
    .filter((s) => s && s.skill != null)
    .slice()
    .sort((a, b) => String(a.skill).localeCompare(String(b.skill)));
}

// The category-sort MODES. Option A (name/usage/status) = mechanical sort/group on a
// field ALREADY on the ACTIVE skills — pur-vue, zero core, deterministic. Option B3
// "category" = data-backed SEMANTIC grouping by the `skill` a card was distilled from,
// over the COMBINED active-templates + retired distilled-agent cards (`readModel.retired`,
// already served by /catalog/list — still zero core). Upgrade path: when the distillation
// emits a real `type`, re-point catGroupKey below from `skill` to `type` (zero UI rework).
export const CATALOG_SORT_MODES = [
  { value: "name", label: "nom" },
  { value: "usage", label: "usage" },
  { value: "status", label: "statut" },
  { value: "category", label: "catégorie" },
];

// Pure: the skills folded into an ORDERED list of GROUPS `{label, count, skills}` for a
// display `mode`. `label:null` = a flat, header-less list (the default "name" mode = the
// historical alpha view -> non-breaking). Empty groups are dropped. Null-safe.
export function catalogGroups(readModel, mode = "name") {
  const skills = readModel && Array.isArray(readModel.skills) ? readModel.skills : [];
  const live = skills.filter((s) => s && s.skill != null);
  const alpha = (a, b) => String(a.skill).localeCompare(String(b.skill));
  const usageOf = (s) => (s.usageCount != null ? Number(s.usageCount) : 0);
  // "deleted" = the SC3 soft-delete flag ONLY. NOT retiredAt: active templates carry a
  // stale retiredAt timestamp, so it can't gate active-vs-deleted.
  const isDeleted = (s) => s.deleted === true || s.tombstoned === true;
  const withCount = (g) => ({ ...g, count: g.skills.length });

  if (mode === "usage") {
    // Buckets on the existing usageCount; most-used first within each bucket.
    const byUse = (a, b) => usageOf(b) - usageOf(a) || alpha(a, b);
    const buckets = [
      { label: "fréquent (5+)", test: (n) => n >= 5 },
      { label: "rare (1–4)", test: (n) => n >= 1 && n <= 4 },
      { label: "jamais utilisé", test: (n) => n <= 0 },
    ];
    return buckets
      .map((b) => ({ label: b.label, skills: live.filter((s) => b.test(usageOf(s))).sort(byUse) }))
      .filter((g) => g.skills.length)
      .map(withCount);
  }
  if (mode === "status") {
    return [
      { label: "actifs", skills: live.filter((s) => !isDeleted(s)).sort(alpha) },
      { label: "supprimés", skills: live.filter(isDeleted).sort(alpha) },
    ].filter((g) => g.skills.length).map(withCount);
  }
  if (mode === "category") {
    // B3 (data-backed): group the COMBINED active templates + retired distilled-agent
    // cards by the `skill` they were distilled from. `.retired` is already served by
    // /catalog/list -> still pur-vue / zero core. Cards with no skill (one-off distilled
    // agents) fall into "Divers", always last. Biggest cluster first, ties alpha.
    const retired = readModel && Array.isArray(readModel.retired) ? readModel.retired : [];
    const all = live.concat(retired.filter((s) => s && (s.skill != null || s.name != null)));
    const catKey = (s) => (s.skill != null ? String(s.skill) : null); // <- swap to s.type once distillation emits it
    const DIVERS = "Divers / non catégorisé";
    const activeRef = new Set(live); // provenance: active templates head their cluster (retiredAt is set even on active)
    const within = (a, b) =>
      (activeRef.has(b) - activeRef.has(a)) ||
      String(a.name || a.skill || "").localeCompare(String(b.name || b.skill || ""));
    const byKey = new Map();
    for (const s of all) {
      const k = catKey(s) || DIVERS;
      if (!byKey.has(k)) byKey.set(k, []);
      byKey.get(k).push(s);
    }
    return [...byKey.entries()]
      .map(([label, skills]) => ({ label, skills: skills.slice().sort(within) }))
      .sort((a, b) =>
        ((a.label === DIVERS) - (b.label === DIVERS)) ||
        (b.skills.length - a.skills.length) ||
        a.label.localeCompare(b.label))
      .map(withCount);
  }
  // default "name": one flat, header-less group = the historical catalogView list.
  return [{ label: null, count: live.length, skills: catalogView(readModel) }];
}

const CATALOG_SORT_KEY = "ta-dash.catalog-sort";
// Restore the persisted sort mode (same pattern as LEGEND_KEY / MESH_KEY). Unknown -> name.
function loadCatalogSort() {
  try {
    const v = localStorage.getItem(CATALOG_SORT_KEY);
    return CATALOG_SORT_MODES.some((m) => m.value === v) ? v : "name";
  } catch { return "name"; }
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

// A possibly-long value rendered with a 'voir plus' toggle past 50 words. The
// FULL text rides in data-full so the delegated click handler swaps it in.
function clippedHtml(value) {
  const cw = clipWords(value);
  if (!cw.clipped) return escapeHtml(cw.text);
  // The clipped prefix AND the toggle live in one .ac-clip container carrying the
  // full text, so expanding replaces the whole container — no duplicated prefix.
  return `<span class="ac-clip" data-full="${escapeHtml(cw.full)}">${escapeHtml(cw.text)} <span class="ac-more" role="button" tabindex="0">voir plus</span></span>`;
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
function catalogSkillHtml(skill, editable = true) {
  const p = skillProfileModel(skill);
  const deleted = !!(skill && (skill.deleted === true || skill.tombstoned === true));
  // A distilled AGENT card carries a proper `name` (Colette, Ponytail…) and is surfaced
  // read-only in the "catégorie" mode. `editable` is provenance-driven (in readModel.skills
  // = an active skill-TEMPLATE, keeps the SC3 form) — NOT a field heuristic: even active
  // templates carry a `retiredAt`, so that field can't gate editability. displayName falls
  // back to the card's `name` for distilled/skill==null cards. Active view = unchanged.
  const distilled = !!(skill && skill.name != null);
  const displayName = distilled ? String(skill.name) : (p.name || "(sans nom)");
  const origin = distilled && p.name ? ` <span class="cat-origin" title="distillé depuis">◦ ${escapeHtml(p.name)}</span>` : "";
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
  return `<div class="cat-skill${deleted ? " cat-deleted" : ""}${editable ? "" : " cat-readonly"}" data-skill="${escapeHtml(p.name)}">
    <button class="cat-skill-head" aria-expanded="false"><span class="cat-caret">▸</span> <span class="cat-name">${escapeHtml(displayName)}</span>${ver}${origin}${deleted ? ` <span class="cat-tombstone">supprimé</span>` : ""}</button>
    <div class="cat-skill-body" hidden>
      ${p.specialty ? `<div class="cat-field"><span class="cat-key">specialty</span> ${escapeHtml(p.specialty)}</div>` : ""}
      ${p.prompt ? `<div class="cat-field"><span class="cat-key">prompt</span> <span class="cat-prompt">${clippedHtml(p.prompt)}</span></div>` : ""}
      ${list("conventions", p.conventions)}
      ${list("tools", p.tools)}
      ${list("patterns", p.patterns)}
      ${metricsTableHtml(skill)}
      ${editable ? editForm : ""}
    </div>
  </div>`;
}

function catalogHtml(readModel) {
  const groups = catalogGroups(readModel, catalogMode);
  const total = groups.reduce((n, g) => n + g.count, 0);
  const grouped = groups.length > 1 || (groups[0] && groups[0].label != null);
  // Editability is PROVENANCE-driven: a card is an editable skill-template iff it's in the
  // active `readModel.skills` (object-ref set) — retired/distilled cards surfaced by the
  // "catégorie" mode render read-only. In name/usage/statut modes every card is active ->
  // editable=true -> the SC3 form is unchanged (non-breaking).
  const activeSet = new Set(readModel && Array.isArray(readModel.skills) ? readModel.skills : []);
  const card = (s) => catalogSkillHtml(s, activeSet.has(s));
  // A `label:null` group renders flat (name mode); a labelled group gets a collapsible
  // header `▾ Label (n)` (usage/statut/catégorie modes).
  const groupHtml = (g) =>
    g.label == null
      ? g.skills.map(card).join("")
      : `<div class="cat-group"><button class="cat-group-head" aria-expanded="true"><span class="cat-group-caret">▾</span> <span class="cat-group-label">${escapeHtml(g.label)}</span> <span class="cat-group-count">(${g.count})</span></button><div class="cat-group-body">${g.skills.map(card).join("")}</div></div>`;
  const body = total
    ? groups.map(groupHtml).join("")
    : `<div class="cat-empty">Aucun skill au catalogue.</div>`;
  const opts = CATALOG_SORT_MODES.map((m) => `<option value="${m.value}"${m.value === catalogMode ? " selected" : ""}>${escapeHtml(m.label)}</option>`).join("");
  // SC3-toggle: "afficher les supprimés" -> re-fetch with ?includeDeleted so the
  // tombstoned cards (deleted:true) show, making the Restore button reachable.
  return `<div class="cat-header">
      <span class="cat-title">Catalogue des skills</span>
      <span class="cat-count">${total} skill${total === 1 ? "" : "s"}</span>
      <label class="cat-sort-wrap">tri : <select class="cat-sort" aria-label="trier le catalogue">${opts}</select></label>
      <label class="cat-deleted-toggle"><input type="checkbox" class="cat-show-deleted"${catalogIncludeDeleted ? " checked" : ""}> afficher les supprimés</label>
      <button class="cat-refresh" title="rafraîchir">↻</button>
      <button class="cat-close" title="fermer" aria-label="fermer">×</button>
    </div>
    <div class="cat-list${grouped ? " cat-list-grouped" : ""}">${body}</div>`;
}

let catalogOpen = false;
// SC3-toggle: whether the current fetch asks the server for tombstoned skills.
let catalogIncludeDeleted = false;
// Category-sort (Option A): the active display MODE + the last fetched model, so
// switching the sort re-renders client-side WITHOUT a re-fetch (RB2: catalogue cold).
let catalogMode = loadCatalogSort();
let catalogModel = null;

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
    catalogModel = await res.json(); // cache: the sort selector re-renders from this, no re-fetch
    el.innerHTML = catalogHtml(catalogModel);
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
// on-demand (a separate cold source). The server read-model is
// rendered VERBATIM: state / verdict / visibility are the fold's call, no JS re-gate.
const DECISIONS_URL = "/decisions";
// Volet-2 (#kiosk 3 onglets): the Rapports tab reads the cold report list; the Grille
// d'intention posts a folded intention → a server-named intent-<ts>.md (both same sandbox).
const REPORTS_URL = "/reports";
const INTENT_URL = "/intent";
// The active Kiosk tab persists across reloads (like the catalogue toggle).
const KIOSK_TAB_KEY = "ta-dash.kiosk-tab";
const KIOSK_TABS = ["decisions", "reports", "intent"];
let kioskOpen = false;
let kioskIncludeArchived = false;

// The persisted active tab, defaulting to décisions. Feature-detect: node / no-localStorage
// falls back to "decisions" so the pure render (unit tests) is deterministic.
function readKioskTab() {
  try {
    const v = localStorage.getItem(KIOSK_TAB_KEY);
    return KIOSK_TABS.includes(v) ? v : "decisions";
  } catch { return "decisions"; }
}

// The server read-model -> the decisions array (tolerate {decisions:[…]} or a bare array).
export function kioskView(readModel) {
  if (readModel && Array.isArray(readModel.decisions)) return readModel.decisions;
  return Array.isArray(readModel) ? readModel : [];
}

// state -> a short human label (visual vocabulary distinct from the living-cards).
const DECISION_STATE_LABEL = { open: "à trancher", read: "lu", tranched: "tranché", archived: "archivé" };
// open first, then the read->tranched->archived progression.
const DECISION_STATE_ORDER = { open: 0, read: 1, tranched: 2, archived: 3 };

// One prose segment: escape EVERYTHING (XSS) then re-introduce only <strong> (**bold**),
// clickable http(s) links, and <br> (line breaks). No raw HTML from the payload ever reaches
// the DOM. `compose` folds --link into this prose (`**Lien** — https://…`) so a bare URL must
// become a real <a href>, not inert text (the kiosk "link rendered as text" bug). XSS-safe: we
// only wrap text that ALREADY passed escapeHtml, and `[^\s<]` can't contain a quote/bracket to
// break out of the href attribute (escapeHtml turned any " into &quot;, < into &lt;).
function renderProse(text) {
  return escapeHtml(text)
    .replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>")
    .replace(/https?:\/\/[^\s<]+/g, (u) => {
      const m = /^(.*?)([.,;:!?)\]]*)$/.exec(u); // don't swallow trailing punctuation
      const url = m ? m[1] : u, tail = m ? m[2] : "";
      return `<a href="${url}" target="_blank" rel="noopener">${url}</a>${tail}`;
    })
    .replace(/\r?\n/g, "<br>");
}

// One fenced ```code``` block: a <pre> for the reader PLUS a 📋 button carrying the EXACT
// raw block text in data-copy. Clicking it copies that text verbatim (see copyToClipboard).
// XSS-safe: the code is escaped both in the <pre> and in the attribute, so the copied
// payload is inert TEXT, never interpreted.
function codeBlockHtml(raw) {
  const esc = escapeHtml(raw);
  return `<div class="kk-codeblock"><button type="button" class="kk-copy-code" aria-label="copier le bloc de code" title="copier" data-copy="${esc}">📋</button><pre class="kk-code"><code>${esc}</code></pre></div>`;
}

// Render a decision's long-form `detail` as SAFE simple-markdown for the toggle body. Fenced
// ```code``` blocks become a copyable <pre> + 📋 button (volet a); everything else is prose.
// Feature-detect: a detail with NO fence yields NO button — graceful degradation on plain prose.
export function renderDetail(text) {
  const src = String(text == null ? "" : text);
  // ```[lang]\n …code… ``` — capture the inner text, tolerate an optional language tag.
  const fence = /```[ \t]*[\w+.#-]*[ \t]*\r?\n([\s\S]*?)```/g;
  let out = "", last = 0, m;
  while ((m = fence.exec(src)) !== null) {
    out += renderProse(src.slice(last, m.index));
    out += codeBlockHtml(m[1].replace(/\r?\n$/, "")); // drop the newline before the closing fence
    last = fence.lastIndex;
  }
  out += renderProse(src.slice(last));
  return out;
}

// Kiosk deploy seam: base for turning a bare code-source ref into a clickable repo blob
// link. The dashboard can't know the running checkout's remote/branch, so it reads a
// <meta name="repo-blob-base"> (dashboard.html). Bug B (PO « vide-jusqu'au-push ») — the
// DEFAULT is EMPTY: the old a-biskoazh fork was a dead 404 (outbox isn't in the repo), so
// until the durability push lands there is no correct base. Empty disables construction →
// a code ref degrades to honest copyable text (never a dead link). A deploy sets the meta
// to the durable repo (e.g. wdes/tab-atelier:tab-atelier-mx) to re-enable clickable blobs.
const REPO_BLOB_BASE = (function () {
  const dflt = "";
  if (typeof document === "undefined") return dflt; // node (unit tests) → the empty default
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

// volet-3 remote-link deploy seam (SIMPLIFICATION PO): the host a LOCAL report viewer link is
// rewritten onto so the PO can open/copy it OFF-LAN. Mirrors REPO_BLOB_BASE (a <meta> a deploy can
// override) but DEFAULTS to amaury (the PO's already-operational CF tunnel) rather than empty — the
// tunnel + its auth are infra we do NOT build; we only compose the shareable absolute URL.
const REMOTE_BASE = (function () {
  const dflt = "https://amaury.wdes.eu";
  if (typeof document === "undefined") return dflt; // node (unit tests) → the amaury default
  const m = document.querySelector('meta[name="remote-base"]');
  const v = m && m.getAttribute("content");
  return v == null || !v.trim() ? dflt : v.trim(); // absent/empty meta → the amaury default
})();

// Pure: compose the shareable REMOTE viewer URL for a LOCAL report path. We do NOT parse the local
// loopback/LAN address — we prefix the fixed remote host onto the SAME relative viewer path
// (/decisions/file?path=…) + the page token, reusing viewerUrlWithToken (auth = the PO tunnel's job,
// not ours). Segment-encoded (keeps slashes readable, encodes spaces/specials). Empty path → "".
export function toRemoteLink(localPath, token = TOKEN, base = REMOTE_BASE) {
  const path = String(localPath == null ? "" : localPath).trim().replace(/^\.?\//, "");
  if (!path) return "";
  const enc = path.split("/").map(encodeURIComponent).join("/");
  return base.replace(/\/$/, "") + viewerUrlWithToken("/decisions/file?path=" + enc, token);
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
  // The outbox zone in its three shapes: an absolute/`~`-expanded `…/Dev/outbox/…`, OR a
  // BARE `outbox/…` / `_archive/…` (cause-A: decision `--files` are pushed WITHOUT the
  // ~/Dev prefix). The server (GET /decisions/file) sandboxes to the outbox + its `_archive/`
  // subtree, so a bare outbox/_archive path IS servable — it must open the viewer, not a
  // github blob (404). Segment-anchored (`^`/`/`) so `inbox/` and `myoutbox/` don't match.
  const inOutbox =
    raw.startsWith("~/Dev/outbox/") ||
    /(?:^|\/)Dev\/outbox\//.test(raw) ||
    /(?:^|\/)(?:outbox|_archive)\//.test(bare);
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
  // Item 4 (#kiosk): the "Lu" mark-read checkbox was removed (the state tag already shows
  // "lu"/"à trancher", and ruling is the explicit Trancher button) — one less useless notch.
  // Tranché stays as a STATE INDICATOR (always disabled). The read/passive state still folds
  // server-side; the POST /decisions/<id>/read route is untouched for any other caller.
  const ruleDisabled = isTranched || !canRule;
  // Feature-detect: the toggle exists ONLY when a non-empty `detail` was served (a
  // detail-less decision degrades gracefully to no toggle, no empty body).
  const hasDetail = typeof d.detail === "string" && d.detail.trim().length > 0;
  const detailToggle = hasDetail
    ? ` <button type="button" class="kk-detail-toggle" aria-expanded="false" title="déplier le détail">(+)</button>`
    : "";
  const detailBody = hasDetail ? `<div class="kk-detail" hidden>${renderDetail(d.detail)}</div>` : "";
  // Item 2 (#kiosk): a NEW `summary` (2-3 lines) renders UNDER the bold title, above the
  // toggle — the render structure is: titre gras → résumé → toggle (+)/(-) → detail. Absent
  // summary → no block (feature-detect). renderProse escapes + keeps **bold** / newlines.
  const hasSummary = typeof d.summary === "string" && d.summary.trim().length > 0;
  const summaryBlock = hasSummary ? `<div class="kk-summary">${renderProse(d.summary)}</div>` : "";
  return `<div class="kk-card kk-state-${escapeHtml(state)}" data-id="${id}" data-state="${escapeHtml(state)}">
    <div class="kk-head">
      <label class="kk-check"><input type="checkbox" class="kk-tranche" disabled${isTranched ? " checked" : ""}> Tranché</label>
      <span class="kk-title">${escapeHtml(String(d.title || d.id || ""))}</span>
      <span class="kk-state-tag">${escapeHtml(DECISION_STATE_LABEL[state] || state)}</span>${detailToggle}
    </div>
    ${summaryBlock}
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

// Legacy clipboard copy that works in a NON-SECURE context (http://<LAN-IP>, where the async
// navigator.clipboard API is undefined): a temp <textarea>, select it, document.execCommand(
// "copy"), remove it. Returns true on success. This is the path the dashboard actually takes in
// prod — the secure navigator.clipboard branch below only runs on localhost/HTTPS.
function legacyCopy(text) {
  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    // Off-screen (not display:none, which would kill the selection execCommand needs).
    ta.style.position = "fixed";
    ta.style.top = "-9999px";
    ta.style.left = "-9999px";
    document.body.appendChild(ta);
    ta.select();
    ta.setSelectionRange(0, text.length);
    const ok = document.execCommand("copy");
    document.body.removeChild(ta);
    return ok;
  } catch { return false; }
}

let copyToastTimer;
// Show a brief, visible "copié !" toast (feedback that works on every path — secure OR legacy).
// Also keeps the inline title affordance for backwards-compat + screen-reader hint on the element.
function showCopyToast(el) {
  if (el && el.setAttribute) {
    const prev = el.getAttribute("title");
    el.setAttribute("title", "copié ✓");
    setTimeout(() => el.setAttribute("title", prev || ""), 1200);
  }
  let toast = document.getElementById("kk-copy-toast");
  if (!toast) {
    toast = document.createElement("div");
    toast.id = "kk-copy-toast";
    toast.className = "kk-toast";
    toast.setAttribute("role", "status");
    toast.setAttribute("aria-live", "polite");
    document.body.appendChild(toast);
  }
  toast.textContent = "copié !";
  toast.classList.add("kk-toast-show");
  clearTimeout(copyToastTimer);
  copyToastTimer = setTimeout(() => toast.classList.remove("kk-toast-show"), 1400);
}

// Copy an element's data-copy text (a code-source ref OR a fenced code block) to the clipboard.
// Secure-context (localhost/HTTPS) uses the async navigator.clipboard API; in the NON-SECURE
// http-LAN deployment that API is undefined, so we MUST fall back to execCommand — otherwise the
// click silently no-ops (the copy-button bug). On success a "copié !" toast confirms it. The
// payload is TEXT — never interpreted.
function copyToClipboard(el) {
  const text = (el && el.dataset && el.dataset.copy) || (el && el.textContent) || "";
  if (!text) return;
  if (navigator && navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(text)
      .then(() => showCopyToast(el))
      .catch(() => { if (legacyCopy(text)) showCopyToast(el); });
    return;
  }
  if (legacyCopy(text)) showCopyToast(el);
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

// Onglet (a) — the decisions list (grouped by project, open-first). Extracted UNCHANGED from
// the former single-panel kiosk so the panel gains tabs with ZERO regression on the cards.
function kioskDecisionsHtml(readModel) {
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
  return `<div class="kk-subhead">
      <span class="kk-count">${openCount} à trancher</span>
      <label class="kk-archived-toggle"><input type="checkbox" class="kk-show-archived"${kioskIncludeArchived ? " checked" : ""}> afficher les archivées</label>
      <button class="kk-refresh" title="rafraîchir">↻</button>
    </div>
    <div class="kk-list">${body}</div>`;
}

// The server report read-model -> the reports array (tolerate {reports:[…]} or a bare array).
export function reportsView(readModel) {
  if (readModel && Array.isArray(readModel.reports)) return readModel.reports;
  return Array.isArray(readModel) ? readModel : [];
}

// Onglet (b) — one report row: a LOCAL viewer link (the same sandboxed /decisions/file route the
// decisions' docs use) PLUS a volet-3 "Ouvrir en distant" link — the same viewer path composed onto
// the remote host (amaury.wdes.eu) so the PO can open/copy it off-LAN. The remote link is gated on a
// page token (canRule): behind the tunnel the token is required, so a tokenless remote link is useless.
export function reportItemHtml(report, canRule) {
  const path = String((report && report.path) || "");
  const name = String((report && report.name) || path);
  const href = `/decisions/file?path=${encodeURIComponent(path)}${canRule ? `&token=${encodeURIComponent(TOKEN)}` : ""}`;
  const remote = canRule ? toRemoteLink(path, TOKEN) : "";
  // rel=noreferrer so the ?token= never leaks to a third party via the Referer header (design pt 3).
  const remoteLink = remote
    ? `<a class="kk-remote-link" href="${escapeHtml(remote)}" target="_blank" rel="noopener noreferrer">Ouvrir en distant</a>`
    : "";
  return `<div class="kk-report" data-local-path="${escapeHtml(path)}">`
    + `<a class="kk-file" href="${escapeHtml(href)}" target="_blank" rel="noopener">${escapeHtml(name)}</a>`
    + remoteLink
    + `</div>`;
}

// Onglet (b) — RANGER (a) + À LA UNE (b): calqués sur le moteur du CATALOGUE (CATALOG_SORT_MODES /
// catalogGroups / loadCatalogSort). The /reports objects carry ONLY {name, path, mtime} — repo &
// tâche are NOT real fields, so we DERIVE them from the filename (pure-vue, zero core).
export const REPORTS_SORT_MODES = [
  { value: "date", label: "date" },
  { value: "repo", label: "repo" },
  { value: "task", label: "tâche" },
];

// Pure: a report's derived facets {repo, task, day, mtime}. Ponytail ceiling: repo/tâche are a
// FILENAME heuristic (the outbox is flat: path is always `outbox/<name>`). repo = the first path
// dir segment below outbox if any, else the leading name token; tâche = the stem minus a trailing
// version/date/nonce token (a token CONTAINING a digit) so multiple runs of one report collapse.
// Exact once /reports emits real repo/task (read r.repo/r.task here) or reports land in
// outbox/<repo>/… subdirs. Null-safe.
export function reportFacets(report) {
  const r = report || {};
  const path = String(r.path || "");
  const name = String(r.name || path);
  const stem = name.replace(/\.(md|markdown)$/i, "");
  const segs = path.split("/").filter(Boolean);
  const dirs = segs.slice(segs[0] === "outbox" ? 1 : 0, -1); // dir segments between outbox and the file
  const tokens = stem.split(/[-_.\s]+/).filter(Boolean);
  const repo = (dirs.length ? dirs[0] : tokens[0]) || "divers";
  // strip a trailing version/date/nonce token (must contain a digit) — keeps pure-alpha words.
  const task = stem.replace(/[-_.\s]+(\d{4}-\d{2}-\d{2}|[a-z0-9]*\d[a-z0-9]*)$/i, "").trim() || stem || repo;
  const mtime = Number(r.mtime) || 0;
  const day = mtime ? new Date(mtime * 1000).toISOString().slice(0, 10) : "date inconnue";
  return { repo, task, day, mtime };
}

// Pure: the reports folded into an ORDERED list of GROUPS `{label, count, reports}` for a display
// `mode` (repo / task / date). Within a group: newest (mtime) first, ties by name. date mode =
// newest day first; repo/task = biggest cluster first, ties alpha. Null-safe (calque catalogGroups).
export function reportGroups(readModel, mode = "date") {
  const reports = reportsView(readModel).filter((r) => r && (r.name != null || r.path != null));
  const facetOf = new Map(reports.map((r) => [r, reportFacets(r)]));
  const keyOf = { repo: (f) => f.repo, task: (f) => f.task, date: (f) => f.day }[mode] || ((f) => f.day);
  const within = (a, b) =>
    facetOf.get(b).mtime - facetOf.get(a).mtime ||
    String(a.name || a.path).localeCompare(String(b.name || b.path));
  const byKey = new Map();
  for (const r of reports) {
    const k = keyOf(facetOf.get(r)) || "divers";
    if (!byKey.has(k)) byKey.set(k, []);
    byKey.get(k).push(r);
  }
  const groups = [...byKey.entries()].map(([label, rs]) => ({ label, count: rs.length, reports: rs.slice().sort(within) }));
  if (mode === "date") groups.sort((a, b) => String(b.label).localeCompare(String(a.label)));
  else groups.sort((a, b) => b.count - a.count || String(a.label).localeCompare(String(b.label)));
  return groups;
}

// Pure: the "à la une" section — the top-N reports by mtime desc (ties by name). Distinct from the
// ranging/grouping below (b). Null-safe.
export function reportsFeatured(readModel, n = 5) {
  return reportsView(readModel)
    .filter((r) => r && (r.name != null || r.path != null))
    .slice()
    .sort((a, b) => (Number(b.mtime) || 0) - (Number(a.mtime) || 0) ||
      String(a.name || a.path).localeCompare(String(b.name || b.path)))
    .slice(0, n);
}

const REPORTS_SORT_KEY = "ta-dash.reports-sort";
// Restore the persisted ranging mode (same pattern as CATALOG_SORT_KEY). Unknown -> date.
function loadReportsSort() {
  try {
    const v = localStorage.getItem(REPORTS_SORT_KEY);
    return REPORTS_SORT_MODES.some((m) => m.value === v) ? v : "date";
  } catch { return "date"; }
}

// Onglet (b) — the reports panel: an "à la une" section (5 most recent, DISTINCT) + a ranging
// selector (calqué catalogue) + the grouped list REUSING the catalogue group look (cat-group /
// cat-group-head / cat-group-body = same headers & collapse interaction). Empty state when the
// outbox has no report. Each row keeps its LOCAL viewer link + volet-3 "Ouvrir en distant" intact.
export function reportsHtml(readModel, canRule, mode = "date") {
  const reports = reportsView(readModel);
  if (!reports.length) return `<div class="kk-empty">Aucun rapport dans l'outbox.</div>`;
  const item = (r) => reportItemHtml(r, canRule);
  const featured = reportsFeatured(readModel, 5);
  const featuredHtml = `<div class="kk-featured">
      <div class="kk-featured-head">À la une — ${featured.length} récent${featured.length === 1 ? "" : "s"}</div>
      <div class="kk-report-list">${featured.map(item).join("")}</div>
    </div>`;
  const opts = REPORTS_SORT_MODES.map((m) => `<option value="${m.value}"${m.value === mode ? " selected" : ""}>${escapeHtml(m.label)}</option>`).join("");
  const groups = reportGroups(readModel, mode);
  const groupHtml = (g) =>
    `<div class="cat-group"><button class="cat-group-head" aria-expanded="true"><span class="cat-group-caret">▾</span> <span class="cat-group-label">${escapeHtml(String(g.label))}</span> <span class="cat-group-count">(${g.count})</span></button><div class="cat-group-body kk-report-list">${g.reports.map(item).join("")}</div></div>`;
  return `${featuredHtml}
    <div class="kk-reports-sub"><label class="cat-sort-wrap">ranger par : <select class="kk-reports-sort" aria-label="ranger les rapports">${opts}</select></label></div>
    <div class="cat-list cat-list-grouped kk-report-groups">${groups.map(groupHtml).join("")}</div>`;
}

// Onglet (c) — fold the intention grid fields into a markdown artefact. PURE + XSS-neutral:
// the text is stored VERBATIM (the /decisions/file viewer escapes-first on read), so nothing
// here needs to escape. Empty rows are dropped; an all-empty grid yields just the heading.
export function intentMarkdown(fields) {
  const f = fields || {};
  const intent = String(f.intent == null ? "" : f.intent).trim();
  const rows = Array.isArray(f.rows) ? f.rows : [];
  const lines = ["# Intention", ""];
  if (intent) lines.push(intent, "");
  const gwt = rows
    .map((r) => ({
      given: String((r && r.given) || "").trim(),
      when: String((r && r.when) || "").trim(),
      then: String((r && r.then) || "").trim(),
    }))
    .filter((r) => r.given || r.when || r.then);
  if (gwt.length) {
    lines.push("## Acceptance (Given/When/Then)", "");
    for (const r of gwt) lines.push(`- **Given** ${r.given}`, `  **When** ${r.when}`, `  **Then** ${r.then}`, "");
  }
  return `${lines.join("\n").replace(/\n+$/, "")}\n`;
}

// Onglet (c) — one repeatable Given/When/Then row (three auto-grow textareas).
function intentRowHtml() {
  return `<div class="kk-gwt-row">`
    + `<textarea class="kk-gwt-given kk-autogrow" rows="1" placeholder="Given…"></textarea>`
    + `<textarea class="kk-gwt-when kk-autogrow" rows="1" placeholder="When…"></textarea>`
    + `<textarea class="kk-gwt-then kk-autogrow" rows="1" placeholder="Then…"></textarea>`
    + `</div>`;
}

// Onglet (c) — the intention grid: an auto-grow intent textarea + repeatable G/W/T rows + the
// "poser l'intention" button. The textareas grow as the text grows (kk-autogrow, wired on input).
function intentFormHtml() {
  return `<div class="kk-intent">
    <p class="kk-intent-hint">Définir l'intention avec le PO. Les champs grandissent à mesure que le texte grandit.</p>
    <label class="kk-intent-label">Intention
      <textarea class="kk-intent-text kk-autogrow" rows="3" placeholder="Décrire l'intention / le besoin…"></textarea>
    </label>
    <div class="kk-gwt-rows">${intentRowHtml()}</div>
    <button type="button" class="kk-gwt-add">+ ajouter un Given/When/Then</button>
    <div class="kk-intent-actions">
      <button type="button" class="kk-intent-post">poser l'intention</button>
      <span class="kk-intent-msg" role="status"></span>
    </div>
  </div>`;
}

// The 3-tab Kiosk shell: (a) Décisions, (b) Rapports, (c) Grille d'intention. The active tab
// (persisted) is baked into the markup; a global close button lives in the header. Reports load
// lazily on activation; the intent form is static. kioskDecisionsHtml keeps the cards intact.
export function kioskHtml(readModel) {
  const active = readKioskTab();
  const tab = (id, label) =>
    `<button type="button" class="kk-tab" role="tab" data-tab="${id}" aria-selected="${id === active}">${label}</button>`;
  const panel = (id, inner) =>
    `<div class="kk-tabpanel" data-panel="${id}" role="tabpanel"${id === active ? "" : " hidden"}>${inner}</div>`;
  return `<div class="kk-header">
      <span class="kk-panel-title">Kiosk</span>
      <button class="kk-close" title="fermer" aria-label="fermer">×</button>
    </div>
    <div class="kk-tabs" role="tablist">
      ${tab("decisions", "Décisions à prendre")}
      ${tab("reports", "Rapports")}
      ${tab("intent", "Grille d'intention")}
    </div>
    ${panel("decisions", kioskDecisionsHtml(readModel))}
    ${panel("reports", `<div class="kk-reports"><div class="kk-loading">chargement…</div></div>`)}
    ${panel("intent", intentFormHtml())}`;
}

// The badge = nb of OPEN decisions, from any decisions fetch (a cold source —
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
  el.innerHTML = `<div class="kk-header"><span class="kk-panel-title">Kiosk</span></div><div class="kk-loading">chargement…</div>`;
  el.hidden = false;
  try {
    const model = await fetchDecisions();
    el.innerHTML = kioskHtml(model);
    renderKioskBadge(kioskView(model));
    afterKioskRender(el);
  } catch (err) {
    el.innerHTML = `<div class="kk-header"><span class="kk-panel-title">Kiosk</span><button class="kk-close" title="fermer" aria-label="fermer">×</button></div><div class="kk-error">décisions indisponibles (${escapeHtml(err.message)})</div>`;
  }
}

// After a (re)render: load the reports tab if it's the active one, and size the intent
// textareas so a restored active-intent tab isn't a squished single row.
function afterKioskRender(el) {
  const active = readKioskTab();
  if (active === "reports") loadReports(el);
  if (active === "intent") initAutogrow(el);
}

// Onglet (a↔b↔c) — switch the visible panel, persist the choice, lazy-load reports, size the
// intent textareas. Feature-detect: unknown tab is a no-op (graceful degradation).
function switchKioskTab(el, tabId) {
  if (!KIOSK_TABS.includes(tabId)) return;
  try { localStorage.setItem(KIOSK_TAB_KEY, tabId); } catch { /* ignore */ }
  el.querySelectorAll(".kk-tab").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.tab === tabId)));
  el.querySelectorAll(".kk-tabpanel").forEach((p) => { p.hidden = p.dataset.panel !== tabId; });
  if (tabId === "reports") loadReports(el);
  if (tabId === "intent") initAutogrow(el);
}

// Onglet (b) — the active ranging mode + the last fetched model/canRule, so switching the sort
// re-renders client-side WITHOUT a re-fetch (reports are a cold source, same as the catalogue).
let reportsMode = loadReportsSort();
let reportsModel = null;
let reportsCanRule = false;

// Onglet (b) — fetch + render the reports panel (cold source, on-demand).
async function loadReports(el) {
  const host = el.querySelector('[data-panel="reports"] .kk-reports');
  if (!host) return;
  host.innerHTML = `<div class="kk-loading">chargement…</div>`;
  try {
    const res = await fetch(REPORTS_URL, { headers: { accept: "application/json", ...AUTH_HEADERS } });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    reportsCanRule = typeof TOKEN === "string" && TOKEN.length > 0;
    reportsModel = await res.json(); // cache: the ranging selector re-renders from this, no re-fetch
    host.innerHTML = reportsHtml(reportsModel, reportsCanRule, reportsMode);
  } catch (err) {
    host.innerHTML = `<div class="kk-error">rapports indisponibles (${escapeHtml(err.message)})</div>`;
  }
}

// Onglet (c) — auto-grow: fit a textarea's height to its content. Feature-detect (no scrollHeight
// / no style → no-op). Called on input and once after render for any pre-filled field.
function autogrow(ta) {
  if (!ta || !ta.style) return;
  ta.style.height = "auto";
  ta.style.height = `${ta.scrollHeight}px`;
}
function initAutogrow(el) {
  el.querySelectorAll(".kk-autogrow").forEach(autogrow);
}

// Onglet (c) — gather the grid fields, fold to markdown, POST /intent (server writes a
// server-named intent-<ts>.md). On success, show a viewer link to the created artefact.
async function postIntent(el) {
  const panel = el.querySelector('[data-panel="intent"]');
  if (!panel) return;
  const msg = panel.querySelector(".kk-intent-msg");
  const setMsg = (html, cls) => { if (msg) { msg.innerHTML = html; msg.className = `kk-intent-msg ${cls}`; } };
  const intent = panel.querySelector(".kk-intent-text")?.value || "";
  const rows = [...panel.querySelectorAll(".kk-gwt-row")].map((r) => ({
    given: r.querySelector(".kk-gwt-given")?.value || "",
    when: r.querySelector(".kk-gwt-when")?.value || "",
    then: r.querySelector(".kk-gwt-then")?.value || "",
  }));
  // Require SOMETHING (mirrors the server's non-empty-content 400) — a lone heading is not an intention.
  const hasContent = intent.trim() || rows.some((r) => `${r.given}${r.when}${r.then}`.trim());
  if (!hasContent) { setMsg("saisir une intention ou un Given/When/Then", "err"); return; }
  const content = intentMarkdown({ intent, rows });
  const canRule = typeof TOKEN === "string" && TOKEN.length > 0;
  if (!canRule) { setMsg("lecture seule — ouvrez le dashboard avec un token pour poser une intention", "err"); return; }
  try {
    const res = await fetch(INTENT_URL, {
      method: "POST",
      headers: { "content-type": "application/json", accept: "application/json", ...AUTH_HEADERS },
      body: JSON.stringify({ content }),
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const out = await res.json();
    const href = `/decisions/file?path=${encodeURIComponent(out.path || "")}&token=${encodeURIComponent(TOKEN)}`;
    setMsg(`intention posée ✓ — <a class="kk-file kk-intent-created" href="${escapeHtml(href)}" target="_blank" rel="noopener">${escapeHtml(String(out.name || ""))}</a>`, "ok");
  } catch (err) {
    setMsg(`échec : ${escapeHtml(err.message)}`, "err");
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
  // Item 4 (#kiosk): the "Lu" mark-read action was removed from the panel — the only
  // remaining Kiosk action is Trancher (the explicit ruling).
  if (target.closest(".kk-send")) { submitTranch(card); return true; }
  return false;
}

// --- DOM wiring (defined always, executed only in a browser) ---

function bootstrap() {
  // Catalogue #39 SC2: open the cold catalog overlay on-demand.
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
      // category-sort: collapse/expand a group (usage/statut modes). Pure DOM, no fetch.
      const ghead = e.target.closest(".cat-group-head");
      if (ghead) {
        const gbody = ghead.parentElement && ghead.parentElement.querySelector(".cat-group-body");
        const gcaret = ghead.querySelector(".cat-group-caret");
        if (gbody) {
          const willShow = gbody.hidden;
          gbody.hidden = !willShow;
          ghead.setAttribute("aria-expanded", willShow ? "true" : "false");
          if (gcaret) gcaret.textContent = willShow ? "▾" : "▸";
        }
        return;
      }
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
    // category-sort: the sort <select> fires "change". Re-render from the CACHED model
    // (RB2: catalogue stays cold, no re-fetch); persist the mode like the catalogue sort.
    catPanel.addEventListener("change", (e) => {
      const sortSel = e.target.closest(".cat-sort");
      if (!sortSel) return;
      e.stopPropagation();
      catalogMode = CATALOG_SORT_MODES.some((m) => m.value === sortSel.value) ? sortSel.value : "name";
      try { localStorage.setItem(CATALOG_SORT_KEY, catalogMode); } catch { /* ignore */ }
      if (catalogModel) catPanel.innerHTML = catalogHtml(catalogModel);
    });
  }
  // Close the catalog on Escape or a click outside it (but not the toggle button).
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeCatalog(); });
  document.addEventListener("click", (e) => {
    if (!catalogOpen) return;
    if (e.target.closest("#catalog-panel") || e.target.closest("#catalog-toggle")) return;
    closeCatalog();
  });

  // KIOSK PD2 (#kiosk): open the cold decisions overlay on-demand.
  const kioskToggle = document.getElementById("kiosk-toggle");
  if (kioskToggle) kioskToggle.addEventListener("click", () => { kioskIncludeArchived = false; openKiosk(); });
  const kioskPanel = document.getElementById("kiosk-panel");
  if (kioskPanel) {
    kioskPanel.addEventListener("click", (e) => {
      // In-panel: stop the document outside-click handler from seeing it (openKiosk
      // replaces innerHTML synchronously, detaching e.target — same trap as the catalogue).
      e.stopPropagation();
      if (e.target.closest(".kk-close")) { closeKiosk(); return; }
      // Volet-2: the tab bar switches the visible panel (a/b/c) + persists the choice.
      const tabBtn = e.target.closest(".kk-tab");
      if (tabBtn) { switchKioskTab(kioskPanel, tabBtn.dataset.tab); return; }
      // Volet-2 (c): grow the grid by one repeatable Given/When/Then row.
      if (e.target.closest(".kk-gwt-add")) {
        const rows = kioskPanel.querySelector(".kk-gwt-rows");
        if (rows) { rows.insertAdjacentHTML("beforeend", intentRowHtml()); }
        return;
      }
      // Volet-2 (c): persist the intention grid to outbox/intent-<ts>.md via POST /intent.
      if (e.target.closest(".kk-intent-post")) { postIntent(kioskPanel); return; }
      if (e.target.closest(".kk-refresh")) { openKiosk(); return; }
      const showArch = e.target.closest(".kk-show-archived");
      if (showArch) { kioskIncludeArchived = !!showArch.checked; openKiosk(); return; }
      // Detail toggle: a LOCAL UI flip (no server round-trip, no re-fetch — re-rendering
      // would collapse it again). Expand/collapse the long-form body in place.
      const dt = e.target.closest(".kk-detail-toggle");
      if (dt) { toggleDetail(dt); return; }
      // Volet (b) ranger: collapse/expand a report GROUP (reuses the catalogue cat-group interaction).
      const rghead = e.target.closest(".cat-group-head");
      if (rghead) {
        const gbody = rghead.parentElement && rghead.parentElement.querySelector(".cat-group-body");
        const gcaret = rghead.querySelector(".cat-group-caret");
        if (gbody) {
          const willShow = gbody.hidden;
          gbody.hidden = !willShow;
          rghead.setAttribute("aria-expanded", willShow ? "true" : "false");
          if (gcaret) gcaret.textContent = willShow ? "▾" : "▸";
        }
        return;
      }
      // Volet (a): 📋 on a fenced code block copies its raw text (a real <button> — Enter/
      // Space fire a click natively, so no separate keydown branch is needed for it).
      const copyBtn = e.target.closest(".kk-copy-code");
      if (copyBtn) { copyToClipboard(copyBtn); return; }
      // FU2 / volet (b): a non-resolvable code-source ref is copyable text (not a link) —
      // click copies it so the reader can paste it. Best-effort (no clipboard → silent no-op).
      const ref = e.target.closest(".kk-file-ref");
      if (ref) { copyToClipboard(ref); return; }
      if (e.target.closest(".kk-send")) { handleKioskAction(e.target); return; }
    });
    // Bug3: Enter in the verdict field submits the ruling (as well as the "Trancher" button).
    kioskPanel.addEventListener("keydown", (e) => {
      const input = e.key === "Enter" && e.target.closest && e.target.closest(".kk-verdict-input");
      if (input) { e.preventDefault(); const card = input.closest(".kk-card"); if (card) submitTranch(card); return; }
      // FU2 a11y: a code-source ref is a role=button — Enter/Space copies it.
      const ref = (e.key === "Enter" || e.key === " ") && e.target.closest && e.target.closest(".kk-file-ref");
      if (ref) { e.preventDefault(); copyToClipboard(ref); }
    });
    // Volet-2 (c): the intention textareas grow as the text grows (auto-grow on input).
    kioskPanel.addEventListener("input", (e) => {
      const ta = e.target.closest && e.target.closest(".kk-autogrow");
      if (ta) autogrow(ta);
    });
    // Volet (b) ranger: the ranging <select> fires "change" — re-render from the CACHED model (cold
    // source, no re-fetch); persist the mode like the catalogue sort.
    kioskPanel.addEventListener("change", (e) => {
      const sortSel = e.target.closest(".kk-reports-sort");
      if (!sortSel) return;
      e.stopPropagation();
      reportsMode = REPORTS_SORT_MODES.some((m) => m.value === sortSel.value) ? sortSel.value : "date";
      try { localStorage.setItem(REPORTS_SORT_KEY, reportsMode); } catch { /* ignore */ }
      const host = kioskPanel.querySelector('[data-panel="reports"] .kk-reports');
      if (host && reportsModel) host.innerHTML = reportsHtml(reportsModel, reportsCanRule, reportsMode);
    });
  }
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeKiosk(); });
  document.addEventListener("click", (e) => {
    if (!kioskOpen) return;
    if (e.target.closest("#kiosk-panel") || e.target.closest("#kiosk-toggle")) return;
    closeKiosk();
  });
  // Seed the badge once at load (on-demand, cold source).
  refreshKioskBadge();
}

if (typeof document !== "undefined") {
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", bootstrap);
  } else {
    bootstrap();
  }
}
