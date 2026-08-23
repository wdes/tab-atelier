// GUI acceptance for Increment 6 web slices (docs/dashboard-increment-6.md):
//   S2 — org-chart (méta on top / orchestrator = team lead / workers below / a
//        `serving` méta joins the team marked indispo)
//   S4 — service nesting (service -> sub-repos -> teams; mono not over-nested)
//   S6 — activity panel new SEPARATE counters (self_tooling/fixes/issues) + verdict badge
// Real Chromium against the SHIPPED assets, intercepting /dashboard/state +
// /dashboard/activity with fixtures. RED today. Builder: web (S2/S4/S6).
//
// Run:  cd <a dir with playwright installed>; node <repo>/assets/dashboard.inc6.accept.mjs

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

async function wireRoutes(page, { getState, getActivity }) {
  await page.route(`${ORIGIN}/**`, async (route) => {
    const p = new URL(route.request().url()).pathname;
    if (p === "/dashboard") return route.fulfill({ contentType: "text/html; charset=utf-8", body: HTML });
    if (p === "/assets/dashboard.js") return route.fulfill({ contentType: "application/javascript; charset=utf-8", body: JS });
    if (p === "/assets/dashboard.css") return route.fulfill({ contentType: "text/css; charset=utf-8", body: CSS });
    if (p === "/tabs/usage") return route.fulfill({ contentType: "application/json", body: "[]" });
    if (p === "/dashboard/state") return route.fulfill({ contentType: "application/json", body: JSON.stringify(getState ? getState() : {}) });
    if (p === "/dashboard/activity") return route.fulfill({ contentType: "application/json", body: JSON.stringify(getActivity ? getActivity() : {}) });
    return route.fulfill({ status: 404, body: "" });
  });
}

const pTab = (o) => ({ agentState: "thinking", tokens: { input: 10, output: 5 }, ...o, viewerUrl: `/tabs/by-id/${o.id}/view` });

// A repo with a lead + 2 workers + a joined serving méta, plus a solo méta, and
// a service family (kalpin: back+front) + a mono service (tab-atelier).
const kbTeamTabs = () => [
  pTab({ id: "o1", name: "ta-lead", role: "orchestrator", assignment: "kalpin-back:build/orchestrator", item: "delegating", led: "idle" }),
  pTab({ id: "w1", name: "ta-w1", role: "implementer", parentTabId: "o1", item: "wiring", led: "working" }),
  pTab({ id: "w2", name: "ta-w2", role: "worker", parentTabId: "o1", item: "wiring2", led: "idle" }),
  pTab({ id: "sm", name: "ta-serving-planner", role: "planner", assignment: "kalpin-back:plan/planner", serving: "kalpin-back", item: "helping kb", led: "idle" }),
];
const orgState = () => ({
  nodes: [], unmapped: [], unassigned: [],
  projects: [
    { name: "kalpin-back", tabCount: 4, rollupLed: "working", hasOrchestrator: true, isMeta: false,
      orchestrators: [{ id: "o1", name: "ta-lead", item: "delegating", childCount: 2 }],
      nodes: [{ id: "build", rollupLed: "working", tabs: kbTeamTabs() }], unmapped: [] },
    { name: "kalpin-front", tabCount: 1, rollupLed: "idle", hasOrchestrator: true, isMeta: false,
      orchestrators: [{ id: "o3", name: "ta-front-lead", item: "front", childCount: 0 }],
      nodes: [{ id: "build", rollupLed: "idle", tabs: [pTab({ id: "o3", name: "ta-front-lead", role: "orchestrator", assignment: "kalpin-front:build/orchestrator", item: "front", led: "idle" })] }], unmapped: [] },
    { name: "tab-atelier", tabCount: 1, rollupLed: "idle", hasOrchestrator: false, isMeta: false, orchestrators: [],
      nodes: [{ id: "build", rollupLed: "idle", tabs: [pTab({ id: "ta1", name: "ta-solo-impl", role: "implementer", assignment: "tab-atelier:build/implementer", item: "x", led: "idle" })] }], unmapped: [] },
    { name: "méta", tabCount: 1, rollupLed: "idle", hasOrchestrator: false, isMeta: true, orchestrators: [],
      nodes: [], unmapped: [pTab({ id: "p", name: "ta-solo-planner", role: "planner", item: "cross-repo planning", led: "idle" })] },
  ],
  services: [
    { name: "kalpin", rollupLed: "working", projects: ["kalpin-back", "kalpin-front"] },
    { name: "tab-atelier", rollupLed: "idle", projects: ["tab-atelier"] },
  ],
});

const activityFixture = () => ({
  window_hours: 24,
  summary_lines: ["croissance: outils réguliers + autonomie en hausse"],
  record: { label: "go-build", autonomy_minutes: 360 },
  self_improvement_verdict: { verdict: "croissance", autonomy_trend: "up", tooling_rate: 0.5, evidence: ["a new tool/day"] },
  totals: {
    features_implemented: 2, fixes: 3, self_tooling: 1, issues_opened: 4, issues_closed: 5,
    tokens_total: { input: 3500, output: 700, cache_creation: 150, cache_read: 35 },
    tokens_per_feature: 2100, minutes_since_last_human_prompt: 60, aligator_calls: 0, human_prompts: 3,
  },
  per_day: [
    { date: "2026-08-22", features: 1, fixes: 1, self_tooling: 0, tokens_per_feature: 900, autonomy_minutes_max: 120 },
    { date: "2026-08-23", features: 2, fixes: 3, self_tooling: 1, tokens_per_feature: 2100, autonomy_minutes_max: 270 },
  ],
});

async function main() {
  const browser = await chromium.launch();

  // ==================== S2 — org-chart ====================
  {
    const page = await browser.newPage();
    await wireRoutes(page, { getState: orgState, getActivity: () => ({}) });
    await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
    await page.waitForSelector(".team", { timeout: 5000 }).catch(() => {});

    ok("S2: an org-chart with a .team for kalpin-back exists", (await page.locator('.team[data-repo="kalpin-back"]').count()) === 1);
    const team = '.team[data-repo="kalpin-back"]';
    ok("S2: the orchestrator is shown as the team LEAD", (await page.locator(`${team} .team-lead`).count()) === 1);
    ok("S2: the 2 workers hang BELOW the lead", (await page.locator(`${team} .worker`).count()) === 2, `count=${await page.locator(`${team} .worker`).count()}`);
    // The serving méta JOINED the team, marked indispo (.serving).
    const servingInTeam = await page.locator(`${team} .serving`).count();
    ok("S2: the serving méta joined the team, marked .serving (indispo)", servingInTeam === 1, `count=${servingInTeam}`);
    // The solo méta stays on TOP, not inside any team.
    const soloTop = await page.locator('.meta-top').filter({ hasText: "ta-solo-planner" }).count();
    ok("S2: the solo méta stays on top (.meta-top)", soloTop >= 1, `count=${soloTop}`);
    const soloInTeam = await page.locator('.team').filter({ hasText: "ta-solo-planner" }).count();
    ok("S2: the solo méta is NOT inside a team", soloInTeam === 0, `count=${soloInTeam}`);
    await page.close();
  }

  // ==================== S4 — service nesting ====================
  {
    const page = await browser.newPage();
    await wireRoutes(page, { getState: orgState, getActivity: () => ({}) });
    await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
    await page.waitForSelector(".service", { timeout: 5000 }).catch(() => {});

    const kalpin = '.service[data-service="kalpin"]';
    ok("S4: a service block wraps the kalpin family", (await page.locator(kalpin).count()) === 1);
    ok("S4: the kalpin service nests its 2 sub-repo teams",
       (await page.locator(`${kalpin} .team[data-repo="kalpin-back"]`).count()) === 1 &&
       (await page.locator(`${kalpin} .team[data-repo="kalpin-front"]`).count()) === 1);
    // Mono service is not over-nested (a marker or a single team, no family wrapper bloat).
    const mono = '.service[data-service="tab-atelier"]';
    ok("S4: the mono service (tab-atelier) renders without extra nesting",
       (await page.locator(`${mono}[data-mono="true"]`).count()) === 1 || (await page.locator(`${mono} .team`).count()) === 1);
    // Deterministic order across reloads.
    const order1 = await page.$$eval(".service", (els) => els.map((e) => e.getAttribute("data-service")));
    await page.reload({ waitUntil: "networkidle" });
    await page.waitForSelector(".service", { timeout: 5000 }).catch(() => {});
    const order2 = await page.$$eval(".service", (els) => els.map((e) => e.getAttribute("data-service")));
    ok("S4: service order is stable across reloads", JSON.stringify(order1) === JSON.stringify(order2), JSON.stringify([order1, order2]));
    await page.close();
  }

  // ==================== S6 — activity counters + verdict badge ====================
  {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    await wireRoutes(page, { getState: orgState, getActivity: activityFixture });
    await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
    await page.waitForSelector('#activity [data-figure="fixes"]', { timeout: 5000 }).catch(() => {});

    const figure = async (key) => (await page.locator(`#activity [data-figure="${key}"]`).first().textContent().catch(() => ""))?.replace(/\s/g, "") || "";
    ok("S6: features shown (undivided)", (await figure("features_implemented")).includes("2"));
    ok("S6: fixes shown separately = 3", (await figure("fixes")).includes("3"));
    ok("S6: self_tooling shown separately = 1", (await figure("self_tooling")).includes("1"));
    ok("S6: issues_opened shown = 4", (await figure("issues_opened")).includes("4"));
    ok("S6: issues_closed shown = 5", (await figure("issues_closed")).includes("5"));
    // Verdict badge with the verdict text.
    const badge = await page.locator("#activity .verdict-badge, #activity [data-verdict]").first().textContent().catch(() => "");
    ok("S6: a verdict badge shows the maturity/growth verdict", /croissance/i.test(badge || ""), `badge=${badge}`);
    ok("S6: no uncaught JS error rendering the extended panel", errors.length === 0, errors.join("; "));
    await page.close();
  }

  await browser.close();
  console.log(`\ndashboard.inc6.accept.mjs — Increment 6 GUI acceptance`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: all Increment-6 scenarios verified on screen"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.inc6.accept.mjs crashed:", e); process.exit(2); });
