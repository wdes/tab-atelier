// GUI acceptance for Increment 8 Slice 1 (docs/dashboard-increment-8.md):
//   the self-declared "agent card" on a tab — objective + currentTask rendered on
//   the card, and a 'libre' badge when orchestrator === "free".
// Real Chromium against the SHIPPED assets, intercepting /dashboard/state with a
// fixture whose tabs carry the new card fields. RED today. Builder: web (card render).
//
// Run:  cd <a dir with playwright installed>; node <repo>/assets/dashboard.inc8.accept.mjs

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
// A fleet that mounts the 4-band view (a tichef is present), carrying agent-card
// fields on two tabs: a FREE freelancer + an OWNED worker.
const cardState = () => ({
  nodes: [], unmapped: [],
  unassigned: [
    pTab({
      id: "free1", name: "ta-free-1", role: "worker", led: "idle",
      // Inc8 S1 agent-card fields on the tab:
      specialty: "rust async internals", orchestrator: "free",
      objective: "land the parser refactor",
      currentTask: ["read the plan", "wire the struct", "run the tests"],
    }),
  ],
  projects: [
    { name: "kalpin-back", isMeta: false, hasOrchestrator: true, orchestrators: [{ id: "o1", name: "ta-lead", childCount: 1 }],
      nodes: [{ id: "build", rollupLed: "working", tabs: [
        pTab({ id: "o1", name: "ta-lead", role: "orchestrator", assignment: "kalpin-back:build/orchestrator", led: "idle" }),
        pTab({ id: "w1", name: "ta-w1", role: "implementer", parentTabId: "o1", assignment: "kalpin-back:build/implementer", led: "working",
               orchestrator: "o1", objective: "impl the struct", currentTask: ["wiring now"] }),
      ] }], unmapped: [] },
    { name: "méta", isMeta: true, hasOrchestrator: false, orchestrators: [],
      nodes: [], unmapped: [pTab({ id: "tc", name: "ta-tichef", role: "manager", assignment: "meta/manager", led: "idle" })] },
  ],
});

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  await wireRoutes(page, cardState);
  await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
  await page.waitForSelector('[data-tab-id="free1"]', { timeout: 5000 }).catch(() => {});

  const free = '[data-tab-id="free1"]';
  ok("Inc8-S1: the free agent's node is rendered", (await page.locator(free).count()) >= 1);
  // Objective + latest currentTask are shown ON the card.
  const objText = await page.locator(`${free} .agent-objective`).first().textContent().catch(() => "");
  ok("Inc8-S1: the card shows the objective", /land the parser refactor/.test(objText || ""), `objective=${objText}`);
  const taskText = await page.locator(`${free} .agent-task`).first().textContent().catch(() => "");
  ok("Inc8-S1: the card shows the latest currentTask phrase", /run the tests/.test(taskText || ""), `task=${taskText}`);
  // The 'libre' badge fires for orchestrator === "free".
  const badge = await page.locator(`${free} .free-badge`).first().textContent().catch(() => "");
  ok("Inc8-S1: a 'libre' badge is shown for a free agent", /libre/i.test(badge || ""), `badge=${badge}`);

  // An OWNED worker (orchestrator = a uuid) does NOT get the 'libre' badge.
  const owned = '[data-tab-id="w1"]';
  if ((await page.locator(owned).count()) >= 1) {
    ok("Inc8-S1: an owned agent has NO 'libre' badge", (await page.locator(`${owned} .free-badge`).count()) === 0);
  } else {
    ok("Inc8-S1: the owned worker node exists to check the badge is absent", false, "no [data-tab-id=w1]");
  }

  await browser.close();
  console.log(`\ndashboard.inc8.accept.mjs — Increment 8 Slice 1 GUI acceptance`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: all Inc8-S1 scenarios verified on screen"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.inc8.accept.mjs crashed:", e); process.exit(2); });
