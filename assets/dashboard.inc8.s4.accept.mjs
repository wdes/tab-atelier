// GUI acceptance for Increment 8 Slice 4 (docs/dashboard-increment-8.md):
//   the right-click agent card renders evaluations[] — the last evals + the latest
//   verdict — and an INDICATOR when an auto-improvement trigger is armed (errors
//   over the 1/1M budget, or >=3 errors in the last 1M tokens). Signal only (S5 acts).
// Real Chromium against the SHIPPED assets, intercepting /dashboard/state with a
// fixture whose tabs carry evaluations records. RED today. Builder: web.
//
// Run:  cd <a dir with playwright installed>; node <repo>/assets/dashboard.inc8.s4.accept.mjs

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
const ev = (errors, tin, tout, verdict) =>
  ({ evaluator: "olympe", at: 0, taskRef: "t", tokens: { in: tin, out: tout }, scores: { relevance: 8, errors, omissions: 0 }, verdict });

const s4State = () => ({
  nodes: [], unmapped: [], unassigned: [],
  projects: [
    { name: "kalpin-back", isMeta: false, hasOrchestrator: true, orchestrators: [{ id: "o1", name: "ta-lead", childCount: 2 }],
      nodes: [{ id: "build", rollupLed: "error", tabs: [
        pTab({ id: "o1", name: "ta-lead", role: "orchestrator", assignment: "kalpin-back:build/orchestrator", led: "idle" }),
        // ARMED agent: 3 errors within the last 1M tokens (burst) -> trigger armed.
        pTab({ id: "hot", name: "ta-hot", role: "implementer", parentTabId: "o1", assignment: "kalpin-back:build/implementer", led: "error",
               objective: "risky refactor",
               evaluations: [ev(1, 50_000, 50_000, "warn"), ev(1, 50_000, 50_000, "warn"), ev(1, 50_000, 50_000, "regression")] }),
        // CLEAN agent: 1 error over 2M tokens -> under budget, not armed.
        pTab({ id: "cool", name: "ta-cool", role: "implementer", parentTabId: "o1", assignment: "kalpin-back:build/implementer", led: "working",
               objective: "steady work", evaluations: [ev(1, 1_000_000, 1_000_000, "ok")] }),
      ] }], unmapped: [] },
    { name: "méta", isMeta: true, hasOrchestrator: false, orchestrators: [],
      nodes: [], unmapped: [pTab({ id: "tc", name: "ta-tichef", role: "manager", orchestrator: "meta", led: "idle" })] },
  ],
});

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 950 } });
  await page.addInitScript(() => { window.__opened = []; window.open = (u) => { window.__opened.push(u); return null; }; });
  await wireRoutes(page, s4State);
  await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
  await page.waitForSelector('[data-tab-id="hot"]', { timeout: 5000 }).catch(() => {});

  // ARMED agent: right-click -> card shows the evals + latest verdict + an armed indicator.
  await page.locator('[data-tab-id="hot"]').click({ button: "right" }).catch(() => {});
  await page.waitForSelector("#agent-card:not([hidden])", { timeout: 3000 }).catch(() => {});
  const hotVisible = await page.locator("#agent-card").isVisible().catch(() => false);
  ok("S4: right-click opens the card for an evaluated agent", hotVisible);
  if (hotVisible) {
    const txt = (await page.locator("#agent-card").textContent().catch(() => "")) || "";
    ok("S4: the card shows the latest verdict", /regression/.test(txt), txt.slice(0, 100));
    ok("S4: an armed-trigger indicator is shown for the over-threshold agent",
       (await page.locator("#agent-card .eval-trigger.armed, #agent-card [data-trigger=\"armed\"]").count()) >= 1);
  }

  // CLEAN agent: right-click -> card has NO armed indicator.
  await page.locator('[data-tab-id="cool"]').click({ button: "right" }).catch(() => {});
  await page.waitForTimeout(300);
  const coolTxt = (await page.locator("#agent-card").textContent().catch(() => "")) || "";
  ok("S4: the clean agent's card shows its verdict", /ok/.test(coolTxt), coolTxt.slice(0, 80));
  ok("S4: no armed-trigger indicator for the within-budget agent",
     (await page.locator("#agent-card .eval-trigger.armed, #agent-card [data-trigger=\"armed\"]").count()) === 0);

  await browser.close();
  console.log(`\ndashboard.inc8.s4.accept.mjs — Increment 8 Slice 4 GUI acceptance`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: all Inc8-S4 scenarios verified on screen"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.inc8.s4.accept.mjs crashed:", e); process.exit(2); });
