// GUI acceptance for the Inc8 `conventions` fold (docs/dashboard-increment-8.md):
//   the right-click agent card renders the DECLARED conventions[] list, and FLAGS
//   an agent that declared none (the free-bot-style "no conventions" check).
// Real Chromium against the SHIPPED assets, intercepting /dashboard/state with a
// fixture whose tabs carry (or omit) `conventions`. RED today. Builder: web.
//
// Run:  cd <a dir with playwright installed>; node <repo>/assets/dashboard.inc8.conventions.accept.mjs

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
const cState = () => ({
  nodes: [], unmapped: [], unassigned: [],
  projects: [
    { name: "kalpin-back", isMeta: false, hasOrchestrator: true, orchestrators: [{ id: "o1", name: "ta-lead", childCount: 2 }],
      nodes: [{ id: "build", rollupLed: "working", tabs: [
        pTab({ id: "o1", name: "ta-lead", role: "orchestrator", assignment: "kalpin-back:build/orchestrator", led: "idle" }),
        // DECLARED conventions:
        pTab({ id: "good", name: "ta-good", role: "implementer", parentTabId: "o1", assignment: "kalpin-back:build/implementer", led: "working",
               objective: "by the book", conventions: ["AGENTS.md", "docs/dashboard.md"] }),
        // NO conventions declared -> flagged:
        pTab({ id: "bare", name: "ta-bare", role: "implementer", parentTabId: "o1", assignment: "kalpin-back:build/implementer", led: "idle",
               objective: "cowboy" }),
      ] }], unmapped: [] },
    { name: "méta", isMeta: true, hasOrchestrator: false, orchestrators: [],
      nodes: [], unmapped: [pTab({ id: "tc", name: "ta-tichef", role: "manager", orchestrator: "meta", led: "idle" })] },
  ],
});

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 950 } });
  await page.addInitScript(() => { window.__opened = []; window.open = (u) => { window.__opened.push(u); return null; }; });
  await wireRoutes(page, cState);
  await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
  await page.waitForSelector('[data-tab-id="good"]', { timeout: 5000 }).catch(() => {});

  // Agent WITH conventions: card lists them, no missing flag.
  await page.locator('[data-tab-id="good"]').click({ button: "right" }).catch(() => {});
  await page.waitForSelector("#agent-card:not([hidden])", { timeout: 3000 }).catch(() => {});
  const goodVisible = await page.locator("#agent-card").isVisible().catch(() => false);
  ok("conv: right-click opens the card", goodVisible);
  if (goodVisible) {
    const txt = (await page.locator("#agent-card").textContent().catch(() => "")) || "";
    ok("conv: the card lists the declared conventions", /AGENTS\.md/.test(txt) && /docs\/dashboard\.md/.test(txt), txt.slice(0, 120));
    ok("conv: a compliant agent is NOT flagged",
       (await page.locator('#agent-card .conventions-missing, #agent-card [data-conventions="missing"]').count()) === 0);
  }

  // Agent WITHOUT conventions: card flags the emptiness.
  await page.locator('[data-tab-id="bare"]').click({ button: "right" }).catch(() => {});
  await page.waitForTimeout(300);
  const bareVisible = await page.locator("#agent-card").isVisible().catch(() => false);
  ok("conv: right-click opens the bare agent's card", bareVisible);
  if (bareVisible) {
    ok("conv: an agent with NO declared conventions is FLAGGED",
       (await page.locator('#agent-card .conventions-missing, #agent-card [data-conventions="missing"]').count()) >= 1);
  }

  await browser.close();
  console.log(`\ndashboard.inc8.conventions.accept.mjs — Inc8 conventions fold GUI acceptance`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: all conventions scenarios verified on screen"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.inc8.conventions.accept.mjs crashed:", e); process.exit(2); });
