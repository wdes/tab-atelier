// GUI acceptance for Increment 7 web slices (docs/dashboard-increment-7.md):
//   S1 — compact org-chart in 4 bands (Méta / Orchestrateurs / Workers / Freelancers)
//        with the orch -> served-repo(s) -> workers chain, more compact than Inc6.
//   S3 — flicker-free refresh (Zoetrope): a non-structural poll patches in place,
//        so node identity + scroll survive (no clear-and-rebuild).
//   S5 — minimap: CONDITIONAL (YAGNI) — a test.fixme skeleton, NO active red.
// Real Chromium against the SHIPPED assets, intercepting /dashboard/state. RED
// today. Builder: web (S1/S3). Non-regression: Inc5/Inc6/I2 accept stay green.
//
// Run:  cd <a dir with playwright installed>; node <repo>/assets/dashboard.inc7.accept.mjs

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { chromium } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const read = (f) => readFileSync(join(HERE, f), "utf8");
const HTML = read("dashboard.html"), JS = read("dashboard.js"), CSS = read("dashboard.css");
const ORIGIN = "http://ta-dash.local", TOKEN = "TESTTOKEN";

let failures = 0;
const ok = (label, cond, detail = "") => {
  if (cond) console.log(`  ✓ ${label}`);
  else { failures++; console.log(`  ✗ ${label}${detail ? ` -- ${detail}` : ""}`); }
};

async function wireRoutes(page, getState) {
  await page.route(`${ORIGIN}/**`, async (route) => {
    const p = new URL(route.request().url()).pathname;
    if (p === "/dashboard") return route.fulfill({ contentType: "text/html; charset=utf-8", body: HTML });
    if (p === "/assets/dashboard.js") return route.fulfill({ contentType: "application/javascript; charset=utf-8", body: JS });
    if (p === "/assets/dashboard.css") return route.fulfill({ contentType: "text/css; charset=utf-8", body: CSS });
    if (p === "/tabs/usage") return route.fulfill({ contentType: "application/json", body: "[]" });
    if (p === "/dashboard/activity") return route.fulfill({ contentType: "application/json", body: "{}" });
    if (p === "/dashboard/state") return route.fulfill({ contentType: "application/json", body: JSON.stringify(getState()) });
    return route.fulfill({ status: 404, body: "" });
  });
}

const pTab = (o) => ({ agentState: "idle", tokens: { input: 1, output: 1 }, ...o, viewerUrl: `/tabs/by-id/${o.id}/view` });
// A fleet with all 4 bands + a multi-repo orchestrator + a mono (FX) orchestrator.
const bandState = (kbLed = "working") => ({
  nodes: [], unmapped: [],
  unassigned: [pTab({ id: "u1", name: "ta-free-1", role: "worker", led: "idle" }), pTab({ id: "u2", name: "ta-free-2", role: "worker", led: "idle" })],
  projects: [
    { name: "kalpin-back", isMeta: false, hasOrchestrator: true, orchestrators: [{ id: "o1", name: "ta-kalpin-lead", item: "delegating", childCount: 2 }],
      nodes: [{ id: "build", rollupLed: kbLed, tabs: [
        pTab({ id: "o1", name: "ta-kalpin-lead", role: "orchestrator", assignment: "kalpin-back:build/orchestrator", led: "idle" }),
        pTab({ id: "w1", name: "ta-w1", role: "implementer", parentTabId: "o1", assignment: "kalpin-back:build/implementer", led: kbLed }),
      ] }], unmapped: [] },
    { name: "kalpin-front", isMeta: false, hasOrchestrator: false, orchestrators: [],
      nodes: [{ id: "build", rollupLed: "idle", tabs: [pTab({ id: "w2", name: "ta-w2", role: "implementer", parentTabId: "o1", assignment: "kalpin-front:build/implementer", led: "idle" })] }], unmapped: [] },
    { name: "fx", isMeta: false, hasOrchestrator: true, orchestrators: [{ id: "o2", name: "ta-fx-lead", item: "solo", childCount: 2 }],
      nodes: [{ id: "build", rollupLed: "idle", tabs: [
        pTab({ id: "o2", name: "ta-fx-lead", role: "orchestrator", assignment: "fx:build/orchestrator", led: "idle" }),
        pTab({ id: "w3", name: "ta-w3", role: "worker", parentTabId: "o2", assignment: "fx:build/worker", led: "idle" }),
        pTab({ id: "w4", name: "ta-w4", role: "worker", parentTabId: "o2", assignment: "fx:build/worker", led: "idle" }),
      ] }], unmapped: [] },
    { name: "méta", isMeta: true, hasOrchestrator: false, orchestrators: [],
      // Live shape: the tichef's real role is "manager" (assignment meta/manager).
      nodes: [], unmapped: [pTab({ id: "tc", name: "ta-tichef", role: "manager", assignment: "meta/manager", led: "idle" }), pTab({ id: "p", name: "ta-planner", role: "planner", led: "idle" })] },
  ],
});

async function main() {
  const browser = await chromium.launch();

  // ==================== S1 — 4-band compact org-chart ====================
  {
    const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
    await wireRoutes(page, () => bandState());
    await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
    await page.waitForSelector('[data-band="freelancers"]', { timeout: 5000 }).catch(() => {});

    for (const [band, label] of [["meta", "Méta"], ["orchestrators", "Orchestrateurs"], ["workers", "Workers"], ["freelancers", "Freelancers"]]) {
      ok(`S1: the ${label} band is present`, (await page.locator(`[data-band="${band}"]`).count()) === 1, `data-band=${band}`);
    }
    // The 3-tier chain orch(o1) -> served repo(kalpin-back) -> worker(w1) is rendered.
    ok("S1: multi-repo orchestrator shows its served-repo sub-nodes",
       (await page.locator('[data-orch="o1"] [data-repo="kalpin-back"]').count()) >= 1 &&
       (await page.locator('[data-orch="o1"] [data-repo="kalpin-front"]').count()) >= 1);
    ok("S1: a worker hangs under its served repo (parentTabId chain)",
       (await page.locator('[data-repo="kalpin-back"] [data-tab-id="w1"]').count()) >= 1);
    // Mono-repo orchestrator (FX) points its workers directly.
    ok("S1: mono-repo orchestrator points its 2 workers directly",
       (await page.locator('[data-orch="o2"] [data-tab-id="w3"]').count()) >= 1 &&
       (await page.locator('[data-orch="o2"] [data-tab-id="w4"]').count()) >= 1);
    // Freelancers band holds the unassigned tabs.
    ok("S1: Freelancers band lists the unassigned tabs",
       (await page.locator('[data-band="freelancers"] [data-tab-id="u1"]').count()) >= 1);
    // Compactness: the whole banded chart fits the viewport without vertical overflow
    // on this small fleet (proxy for "hauteur < layout Inc6"). Verified on the real
    // fleet by the builder; here we assert it does not overflow the 900px viewport.
    const bottom = await page.evaluate(() => {
      const b = document.querySelector('[data-band="freelancers"]');
      return b ? Math.round(b.getBoundingClientRect().bottom) : Infinity;
    });
    ok("S1: the 4-band chart is compact (fits the viewport, no vertical overflow)", bottom <= 900, `bottom=${bottom}px`);
    await page.close();
  }

  // ==================== S3 — flicker-free refresh (in-place patch) ====================
  {
    const page = await browser.newPage({ viewport: { width: 1280, height: 700 } });
    let led = "working";
    await wireRoutes(page, () => bandState(led));
    await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
    const worker = '[data-tab-id="w1"]';
    await page.waitForSelector(worker, { timeout: 5000 }).catch(() => {});

    if ((await page.locator(worker).count()) === 0) {
      ok("S3: the stable worker node exists to test identity survival", false, "no [data-tab-id=w1] (needs S1)");
    } else {
      // Tag the live DOM node + scroll, then push a NON-structural change (led flip).
      await page.evaluate((sel) => { document.querySelector(sel).__zoetrope = "kept"; window.scrollTo(0, 120); }, worker);
      led = "error";
      await page.waitForTimeout(2000); // one poll (POLL_MS=1500) + margin
      // (a) same DOM node survived the tick (marker kept => patched in place, not rebuilt).
      const survived = await page.evaluate((sel) => document.querySelector(sel)?.__zoetrope === "kept", worker);
      ok("S3: node identity survives a poll (in-place patch, no clear-and-rebuild)", survived === true);
      // The node still reflects the new led (patched, not stale).
      const cls = await page.getAttribute(worker, "class");
      ok("S3: the patched node reflects the new led", /led-error/.test(cls || ""), `class=${cls}`);
      // (b) scroll position survives the refresh.
      const y = await page.evaluate(() => window.scrollY);
      ok("S3: scroll position survives the refresh", y === 120, `scrollY=${y}`);
    }
    await page.close();
  }

  // ==================== S5 — minimap (CONDITIONAL, test.fixme skeleton) ====================
  // Deferred by design (YAGNI): only build a minimap IF the compact S1 layout still
  // overflows on the REAL fleet. No active red here — this is the skeleton to fill
  // once a real-fleet overflow is measured after S1 ships.
  //
  //   test.fixme("S5: minimap viewport rect tracks graph bounds + is click/drag navigable", () => {
  //     const mm = minimapModel(graphBounds, viewport);   // pure math, no lib
  //     assert.ok(mm.viewportRect within mm.bounds);
  //     assert.equal(clampPan(mm, dragTo).x, expected);
  //   });
  console.log("  · S5: minimap deferred (conditional/YAGNI) — no active red until real-fleet overflow measured.");

  await browser.close();
  console.log(`\ndashboard.inc7.accept.mjs — Increment 7 GUI acceptance`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: all active Increment-7 scenarios verified on screen"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.inc7.accept.mjs crashed:", e); process.exit(2); });
