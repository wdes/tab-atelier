// GUI acceptance for Increment 5 web slices (docs/dashboard-increment-5.md):
//   S1 — legend + persistent on/off toggle
//   S4 — "Dernières heures" activity panel (figures + per-day bars)
//   S6 — overview reorg (META band top / orchestrators named under the repo /
//        multi-orchestrator tree / UNASSIGNED band bottom)
// Drives real Chromium against the SHIPPED assets (dashboard.html/.js/.css),
// intercepting /dashboard/state + /dashboard/activity with fixtures so the check
// is deterministic and needs no daemon. The harness rule: a GUI feature is done
// only when the intent is OBSERVED on screen. RED today (nothing renders these).
// Builder: web (S1/S4/S6).
//
// Run:  cd <a dir with playwright installed>; node <repo>/assets/dashboard.inc5.accept.mjs
//   (e.g. /tmp/ta-dash-accept — same host setup as dashboard.accept.mjs).

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { chromium } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const read = (f) => readFileSync(join(HERE, f), "utf8");
const HTML = read("dashboard.html");
const JS = read("dashboard.js");
const CSS = read("dashboard.css");

const ORIGIN = "http://ta-dash.local";
const TOKEN = "TESTTOKEN";

let failures = 0;
const ok = (label, cond, detail = "") => {
  if (cond) console.log(`  ✓ ${label}`);
  else { failures++; console.log(`  ✗ ${label}${detail ? ` -- ${detail}` : ""}`); }
};

// Serve the app + fixtures. `getState`/`getActivity` are thunks so a scenario can
// mutate them between polls. A 401 sentinel models the auth gate.
async function wireRoutes(page, { getState, getActivity }) {
  await page.route(`${ORIGIN}/**`, async (route) => {
    const url = new URL(route.request().url());
    const p = url.pathname;
    if (p === "/dashboard") return route.fulfill({ contentType: "text/html; charset=utf-8", body: HTML });
    if (p === "/assets/dashboard.js") return route.fulfill({ contentType: "application/javascript; charset=utf-8", body: JS });
    if (p === "/assets/dashboard.css") return route.fulfill({ contentType: "text/css; charset=utf-8", body: CSS });
    if (p === "/tabs/usage") return route.fulfill({ contentType: "application/json", body: "[]" });
    if (p === "/dashboard/state") {
      const st = getState ? getState() : { nodes: [], unmapped: [] };
      return route.fulfill({ contentType: "application/json", body: JSON.stringify(st) });
    }
    if (p === "/dashboard/activity") {
      const a = getActivity ? getActivity() : {};
      return route.fulfill({ contentType: "application/json", body: JSON.stringify(a) });
    }
    return route.fulfill({ status: 404, body: "" });
  });
}

// --- Fixtures -------------------------------------------------------------
const nodeLed = (id, led) => ({ id, rollupLed: led, tabs: [] });
// A projects-less diagram carrying every led state, so S1 can match each legend
// swatch colour against the node that actually paints it.
const ledDiagram = () => ({
  nodes: [
    nodeLed("scope", "idle"),
    nodeLed("plan", "unreviewed"),
    nodeLed("build", "working"),
    nodeLed("review", "error"),
    nodeLed("verify", "dead"),
  ],
  unmapped: [],
});
const LED_TO_PHASE = { "led-idle": "scope", "led-unreviewed": "plan", "led-working": "build", "led-error": "review", "led-dead": "verify" };

const activityFixture = () => ({
  window_hours: 24,
  summary_lines: ["3 human prompts in 24h", "2 features shipped", "record vs go-build ~6h"],
  record: { label: "go-build", autonomy_minutes: 360 },
  totals: {
    features_implemented: 2,
    tokens_total: { input: 3500, output: 700, cache_creation: 150, cache_read: 35 },
    tokens_per_feature: 2100,
    minutes_since_last_human_prompt: 60,
    aligator_calls: 0,
    human_prompts: 3,
  },
  per_day: [
    { date: "2026-08-21", features: 0, tokens_per_feature: 0, autonomy_minutes_max: 30 },
    { date: "2026-08-22", features: 1, tokens_per_feature: 900, autonomy_minutes_max: 120 },
    { date: "2026-08-23", features: 2, tokens_per_feature: 2100, autonomy_minutes_max: 270 },
  ],
});

const pTab = (o) => ({ agentState: "thinking", tokens: { input: 10, output: 5 }, ...o, viewerUrl: `/tabs/by-id/${o.id}/view` });
// A reorg fixture: 1 meta expert, a 2-orchestrator repo, a 1-orchestrator repo,
// and 2 legitimate unassigned tabs.
const reorgState = () => ({
  nodes: [], unmapped: [],
  unassigned: [
    pTab({ id: "u1", name: "ta-scratch-1", role: "worker", led: "idle" }),
    pTab({ id: "u2", name: "ta-scratch-2", role: "worker", led: "idle" }),
  ],
  projects: [
    { name: "kalpin-back", tabCount: 4, rollupLed: "working", hasOrchestrator: true, isMeta: false,
      orchestrators: [
        { id: "o1", name: "ta-orch-build", item: "delegating build slices", childCount: 2 },
        { id: "o2", name: "ta-orch-review", item: "delegating review", childCount: 1 },
      ],
      nodes: [{ id: "build", rollupLed: "working", tabs: [
        pTab({ id: "o1", name: "ta-orch-build", role: "orchestrator", assignment: "kalpin-back:build/orchestrator", item: "delegating build slices", led: "idle" }),
        pTab({ id: "o2", name: "ta-orch-review", role: "orchestrator", assignment: "kalpin-back:review/orchestrator", item: "delegating review", led: "idle" }),
        pTab({ id: "w1", name: "ta-impl-1", role: "implementer", parentTabId: "o1", item: "wiring parser", led: "working" }),
        pTab({ id: "w2", name: "ta-impl-2", role: "implementer", parentTabId: "o1", item: "wiring css", led: "working" }),
      ] }], unmapped: [] },
    { name: "kalpin-front", tabCount: 3, rollupLed: "idle", hasOrchestrator: true, isMeta: false,
      orchestrators: [{ id: "o3", name: "ta-orch-front", item: "one orchestrator", childCount: 2 }],
      nodes: [{ id: "build", rollupLed: "idle", tabs: [
        pTab({ id: "o3", name: "ta-orch-front", role: "orchestrator", assignment: "kalpin-front:build/orchestrator", item: "one orchestrator", led: "idle" }),
        pTab({ id: "w3", name: "ta-front-1", role: "worker", parentTabId: "o3", item: "view", led: "idle" }),
        pTab({ id: "w4", name: "ta-front-2", role: "worker", parentTabId: "o3", item: "view2", led: "idle" }),
      ] }], unmapped: [] },
    { name: "méta", tabCount: 1, rollupLed: "idle", hasOrchestrator: false, isMeta: true, orchestrators: [],
      nodes: [], unmapped: [pTab({ id: "m1", name: "ta-planner", role: "planner", item: "cross-repo planning", led: "idle" })] },
  ],
});

async function main() {
  const browser = await chromium.launch();

  // ============================ S1 — legend + toggle ============================
  {
    const ctx = await browser.newContext({ colorScheme: "light" });
    const page = await ctx.newPage();
    await wireRoutes(page, { getState: ledDiagram, getActivity: () => ({}) });
    await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });

    const hasLegend = await page.locator("#legend").count();
    ok("S1: a #legend section exists", hasLegend === 1);
    if (hasLegend) {
      // Positioned AFTER the phase-flow (below the graph).
      const afterFlow = await page.evaluate(() => {
        const flow = document.getElementById("flow");
        const legend = document.getElementById("legend");
        return !!(flow && legend) && (flow.compareDocumentPosition(legend) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;
      });
      ok("S1: the legend sits below the graph", afterFlow);

      // Each led swatch paints the SAME colour as the node that uses that led.
      for (const [cls, phase] of Object.entries(LED_TO_PHASE)) {
        const swatchBg = await page.$eval(`#legend .legend-swatch.${cls}`, (el) => getComputedStyle(el).backgroundColor).catch(() => null);
        const nodeFill = await page.$eval(`#node-${phase} .node-box`, (el) => getComputedStyle(el).fill).catch(() => null);
        ok(`S1: swatch ${cls} matches the ${phase} node colour`, !!swatchBg && swatchBg === nodeFill, `${swatchBg} vs ${nodeFill}`);
      }
      ok("S1: legend explains the orchestrator accent", (await page.locator("#legend .legend-swatch.orchestrator").count()) >= 1);
      ok("S1: legend explains the lineage arrow", (await page.locator("#legend [class*=lineage]").count()) >= 1);

      // Toggle hides the legend AND the state persists across a reload.
      const toggle = page.locator("#legend-toggle");
      ok("S1: a #legend-toggle control exists", (await toggle.count()) === 1);
      if ((await toggle.count()) === 1) {
        await toggle.click();
        await page.waitForFunction(() => document.getElementById("legend")?.hasAttribute("hidden"), null, { timeout: 3000 }).catch(() => {});
        ok("S1: clicking the toggle hides the legend", await page.locator("#legend").isHidden());
        await page.reload({ waitUntil: "networkidle" });
        ok("S1: the hidden state PERSISTS across a reload (localStorage)", await page.locator("#legend").isHidden());
        await page.locator("#legend-toggle").click();
        await page.waitForFunction(() => !document.getElementById("legend")?.hasAttribute("hidden"), null, { timeout: 3000 }).catch(() => {});
        ok("S1: re-clicking the toggle shows the legend again", await page.locator("#legend").isVisible());
      }
    }
    await ctx.close();
  }

  // ======================= S4 — "Dernières heures" panel =======================
  {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    await wireRoutes(page, { getState: reorgState, getActivity: activityFixture });
    await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });

    const hasPanel = await page.locator("#activity").count();
    ok("S4: an #activity panel exists", hasPanel === 1);
    if (hasPanel) {
      await page.waitForSelector('#activity [data-figure="features_implemented"]', { timeout: 4000 }).catch(() => {});
      const figure = async (key) => (await page.locator(`#activity [data-figure="${key}"]`).first().textContent().catch(() => ""))?.replace(/\s/g, "") || "";
      ok("S4: features_implemented = 2 shown", (await figure("features_implemented")).includes("2"));
      ok("S4: tokens_per_feature = 2100 shown", (await figure("tokens_per_feature")).includes("2100"));
      ok("S4: minutes_since_last_human_prompt = 60 shown", (await figure("minutes_since_last_human_prompt")).includes("60"));
      ok("S4: aligator_calls = 0 shown", (await figure("aligator_calls")).includes("0"));
      ok("S4: human_prompts = 3 shown", (await figure("human_prompts")).includes("3"));
      // Three per-day series, each with one bar per day (3 days -> 3 bars).
      for (const series of ["features", "tokens_per_feature", "autonomy"]) {
        const bars = await page.locator(`#activity [data-series="${series}"] .activity-bar`).count();
        ok(`S4: ${series} mini-graph has one bar per day (3)`, bars === 3, `bars=${bars}`);
      }
      ok("S4: no uncaught JS error while rendering the panel", errors.length === 0, errors.join("; "));
    }
    await page.close();
  }

  // S4 empty: activity.json absent -> panel renders an empty state, no JS error.
  {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    await wireRoutes(page, { getState: reorgState, getActivity: () => ({}) });
    await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
    const hasPanel = await page.locator("#activity").count();
    ok("S4(empty): #activity panel still present with no data", hasPanel === 1);
    if (hasPanel) {
      const bars = await page.locator("#activity .activity-bar").count();
      ok("S4(empty): no bars drawn when there is no data", bars === 0, `bars=${bars}`);
      ok("S4(empty): no uncaught JS error on empty payload", errors.length === 0, errors.join("; "));
    }
    await page.close();
  }

  // ==================== S6 — overview reorg (META/repos/UNASSIGNED) ====================
  {
    const page = await browser.newPage();
    await wireRoutes(page, { getState: reorgState, getActivity: () => ({}) });
    await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
    await page.waitForSelector(".altitude-band", { timeout: 5000 }).catch(() => {});

    const bands = await page.$$eval(".altitude-band", (els) => els.map((e) => e.getAttribute("data-band-label")));
    ok("S6: META band is FIRST", bands[0] === "META", JSON.stringify(bands));
    ok("S6: UNASSIGNED band is LAST", bands[bands.length - 1] === "UNASSIGNED", JSON.stringify(bands));

    // META band holds the transverse meta expert.
    const metaHasPlanner = await page.locator('.altitude-band[data-band-label="META"] .project-card[data-project="méta"]').count();
    ok("S6: the META band contains the méta project", metaHasPlanner === 1);

    // Each repo names its orchestrators under the repo name.
    const kbOrchNames = await page.locator('.project-card[data-project="kalpin-back"] .orch-name').count();
    const kfOrchNames = await page.locator('.project-card[data-project="kalpin-front"] .orch-name').count();
    ok("S6: kalpin-back names its 2 orchestrators under the repo", kbOrchNames === 2, `count=${kbOrchNames}`);
    ok("S6: kalpin-front names its 1 orchestrator", kfOrchNames === 1, `count=${kfOrchNames}`);

    // A repo with >1 orchestrator shows a TREE; a single-orchestrator one does not.
    const kbTree = await page.locator('.project-card[data-project="kalpin-back"] .orch-tree').count();
    const kfTree = await page.locator('.project-card[data-project="kalpin-front"] .orch-tree').count();
    ok("S6: the 2-orchestrator repo renders a tree", kbTree >= 1);
    ok("S6: the 1-orchestrator repo is NOT a tree", kfTree === 0);

    // UNASSIGNED band holds the 2 loose tabs, NOT marked as errors (#90).
    const unassignedTabs = page.locator('.altitude-band[data-band-label="UNASSIGNED"] .unassigned-tab');
    ok("S6: the UNASSIGNED band lists the 2 loose tabs", (await unassignedTabs.count()) === 2, `count=${await unassignedTabs.count()}`);
    const anyError = await page.locator('.altitude-band[data-band-label="UNASSIGNED"] .led-error').count();
    ok("S6: unassigned tabs are NOT flagged as errors (#90)", anyError === 0);
    await page.close();
  }

  await browser.close();
  console.log(`\ndashboard.inc5.accept.mjs — Increment 5 GUI acceptance`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: all Increment-5 scenarios verified on screen"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => {
  console.error("dashboard.inc5.accept.mjs crashed:", e);
  process.exit(2);
});
