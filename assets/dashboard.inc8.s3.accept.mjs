// GUI acceptance for Increment 8 Slice 3 (docs/dashboard-increment-8.md):
//   1. RIGHT-CLICK an agent -> its full agent-card view (specialty / orchestrator /
//      objective / bounded permalog / evaluations / evalCriteria). Orchestrators too.
//   2. roundsActive PILL on an orchestrator card: GREEN when active, GREY otherwise.
//   3. META-TRIO: the Méta band shows tichef + Brain + aligator (orchestrator="meta").
// Real Chromium against the SHIPPED assets, intercepting /dashboard/state with a
// fixture whose tabs carry the S1 card fields. RED today. Builder: web.
//
// Run:  cd <a dir with playwright installed>; node <repo>/assets/dashboard.inc8.s3.accept.mjs

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
const card = { specialty: "daemon", objective: "keep the fleet alive", currentTaskLog: ["boot", "tick"] };
// A 60-word objective (starts with the phrase the S3-1 assertion matches) to exercise
// the Inc9 (2) 'voir plus' toggle (clip at 50 words).
const longObjective = "land the parser refactor " + Array.from({ length: 56 }, (_, i) => `detail${i}`).join(" ");
// A fleet with the META-TRIO + two orchestrators (rounds on/off) + a carded worker.
const s3State = () => ({
  nodes: [], unmapped: [], unassigned: [],
  projects: [
    { name: "kalpin-back", isMeta: false, hasOrchestrator: true, orchestrators: [{ id: "o1", name: "ta-lead", childCount: 1 }],
      nodes: [{ id: "build", rollupLed: "working", tabs: [
        pTab({ id: "o1", name: "ta-lead", role: "orchestrator", assignment: "kalpin-back:build/orchestrator", led: "idle",
               specialty: "kalpin lead", objective: "ship inc8", orchestrator: "free", roundsActive: { active: true, lastRoundAt: 111 } }),
        pTab({ id: "w1", name: "ta-w1", role: "implementer", parentTabId: "o1", assignment: "kalpin-back:build/implementer", led: "working",
               specialty: "rust async internals", orchestrator: "o1", objective: longObjective, roundsActive: { active: true, lastRoundAt: 222 },
               currentTaskLog: ["read plan", "wire struct", "add test", "run tests", "fix lint", "review", "commit", "handoff"],
               conventions: ["docs/style.md"], usageCount: 7, lastUsedAt: 1788000000,
               evaluations: [{ evaluator: "olympe", verdict: "ok" }], evalCriteria: ["no panics", "tests green"] }),
      ] }], unmapped: [] },
    { name: "fx", isMeta: false, hasOrchestrator: true, orchestrators: [{ id: "o2", name: "ta-fx-lead", childCount: 0 }],
      nodes: [{ id: "build", rollupLed: "idle", tabs: [
        pTab({ id: "o2", name: "ta-fx-lead", role: "orchestrator", assignment: "fx:build/orchestrator", led: "idle",
               specialty: "fx", objective: "solo", orchestrator: "free", roundsActive: { active: false } }),
      ] }], unmapped: [] },
    { name: "méta", isMeta: true, hasOrchestrator: false, orchestrators: [],
      nodes: [], unmapped: [
        pTab({ id: "tc", name: "ta-tichef", role: "manager", orchestrator: "meta", ...card }),
        pTab({ id: "brain", name: "⛑ brain", agent_kind: "brain", role: "", orchestrator: "meta", ...card }),
        pTab({ id: "alig", name: "🐊 aligator", agent_kind: "aligator", role: "", orchestrator: "meta", ...card }),
        // Inc9 (4): on-demand supporters — must land in the Supporters band, NOT Méta.
        pTab({ id: "jo", name: "Joséphine", role: "", assignment: "meta/guardian", ...card }),
        pTab({ id: "scribe", name: "ta-scribe", role: "", assignment: "meta/scribe", ...card }),
      ] },
  ],
});

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 950 } });
  await page.addInitScript(() => { window.__opened = []; window.open = (u) => { window.__opened.push(u); return null; }; });
  await wireRoutes(page, s3State);
  await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
  await page.waitForSelector('[data-tab-id="w1"]', { timeout: 5000 }).catch(() => {});

  // ---- 3. META-TRIO in the Méta band.
  const metaBand = '[data-band="meta"]';
  for (const [id, who] of [["tc", "tichef"], ["brain", "Brain"], ["alig", "aligator"]]) {
    ok(`S3-3: the Méta band shows ${who}`, (await page.locator(`${metaBand} [data-tab-id="${id}"]`).count()) >= 1);
  }
  ok("S3-3: Brain is NOT in the Freelancers band", (await page.locator('[data-band="freelancers"] [data-tab-id="brain"]').count()) === 0);

  // ---- Inc9 (4): the SUPPORTERS band, distinct from the 3 autonomous metas.
  const supBand = '[data-band="supporters"]';
  for (const [id, who] of [["jo", "Joséphine"], ["scribe", "ta-scribe"]]) {
    ok(`Inc9-(4): the Supporters band shows ${who}`, (await page.locator(`${supBand} [data-tab-id="${id}"]`).count()) >= 1);
    ok(`Inc9-(4): ${who} is NOT in the Méta band`, (await page.locator(`${metaBand} [data-tab-id="${id}"]`).count()) === 0);
  }
  // The 3 autonomous daemons stay in Méta, NOT in Supporters.
  for (const id of ["tc", "brain", "alig"]) {
    ok(`Inc9-(4): the meta daemon ${id} is NOT in the Supporters band`, (await page.locator(`${supBand} [data-tab-id="${id}"]`).count()) === 0);
  }
  ok("Inc9-(4): the Supporters band renders BETWEEN Méta and Orchestrateurs", await page.evaluate(() => {
    const order = [...document.querySelectorAll(".band[data-band]")].map((b) => b.getAttribute("data-band"));
    const m = order.indexOf("meta"), s = order.indexOf("supporters"), o = order.indexOf("orchestrators");
    return m >= 0 && s === m + 1 && o === s + 1;
  }));

  // ---- 2. roundsActive pill on the orchestrator cards.
  ok("S3-2: an active orchestrator shows the GREEN rounds pill",
     (await page.locator('[data-tab-id="o1"] .rounds-pill.rounds-on').count()) >= 1);
  ok("S3-2: an inactive orchestrator shows the GREY rounds pill",
     (await page.locator('[data-tab-id="o2"] .rounds-pill.rounds-off').count()) >= 1);
  ok("S3-2: the active pill is not also grey", (await page.locator('[data-tab-id="o1"] .rounds-pill.rounds-off').count()) === 0);

  // ---- 1. RIGHT-CLICK an agent -> its full card view.
  await page.locator('[data-tab-id="w1"]').click({ button: "right" }).catch(() => {});
  await page.waitForSelector("#agent-card:not([hidden])", { timeout: 3000 }).catch(() => {});
  const cardVisible = await page.locator("#agent-card").isVisible().catch(() => false);
  ok("S3-1: right-click opens the agent-card view", cardVisible);
  if (cardVisible) {
    const txt = (await page.locator("#agent-card").textContent().catch(() => "")) || "";
    ok("S3-1: the card shows the specialty", /rust async internals/.test(txt), txt.slice(0, 80));
    ok("S3-1: the card shows the objective", /land the parser refactor/.test(txt));
    // Permalog is bounded -> the LAST entries show; the oldest ("read plan") is clipped.
    ok("S3-1: the card shows recent permalog tasks (bounded)", /handoff/.test(txt) && !/read plan/.test(txt), txt.slice(0, 120));
    ok("S3-1: the card shows evaluations when present", /olympe/.test(txt));
    ok("S3-1: the card shows evalCriteria", /no panics/.test(txt));
    // ---- Inc9 (2): ALL card data + 'voir plus' past 50 words.
    ok("Inc9-(2): the card shows conventions", /style\.md/.test(txt));
    ok("Inc9-(2): the card shows usage (usageCount/lastUsedAt)", /used 7×/.test(txt));
    ok("Inc9-(2): the card shows roundsActive", /roundsActive/.test(txt) && /active/.test(txt));
    // The 60-word objective is clipped -> a 'voir plus' toggle; the last word is hidden until clicked.
    ok("Inc9-(2): a >50-word field shows a 'voir plus' toggle", (await page.locator("#agent-card .ac-more").count()) >= 1);
    ok("Inc9-(2): the clipped field hides its tail before expanding", !/detail55/.test(txt), "detail55 should be hidden pre-expand");
    await page.locator("#agent-card .ac-more").first().click().catch(() => {});
    await page.waitForTimeout(150);
    const expanded = (await page.locator("#agent-card").textContent().catch(() => "")) || "";
    ok("Inc9-(2): clicking 'voir plus' expands to the FULL text", /detail55/.test(expanded), "detail55 must appear after expand");
    ok("Inc9-(2): the card stays open after 'voir plus' (not closed)", await page.locator("#agent-card").isVisible().catch(() => false));
    // ---- Inc9 (3): open the agent's tab in the browser (remote viewer).
    ok("Inc9-(3): the card shows a ↗ open-tab button", (await page.locator("#agent-card .ac-open").count()) >= 1);
    await page.evaluate(() => { window.__opened = []; });
    await page.locator("#agent-card .ac-open").first().click().catch(() => {});
    await page.waitForTimeout(120);
    const openedByBtn = await page.evaluate(() => window.__opened.slice());
    ok("Inc9-(3): clicking ↗ opens the agent's viewer URL in a new tab", openedByBtn.some((u) => /\/tabs\/by-id\/w1\/view/.test(u)), `opened=${JSON.stringify(openedByBtn)}`);
    // Right-click on a FREE ZONE of the card (the name area, not a button) also opens it.
    await page.evaluate(() => { window.__opened = []; });
    await page.locator("#agent-card .ac-name").click({ button: "right" }).catch(() => {});
    await page.waitForTimeout(120);
    const openedByRclick = await page.evaluate(() => window.__opened.slice());
    ok("Inc9-(3): right-click on the card free zone opens the remote tab", openedByRclick.some((u) => /\/tabs\/by-id\/w1\/view/.test(u)), `opened=${JSON.stringify(openedByRclick)}`);
  }
  // An ORCHESTRATOR gets a card too.
  await page.locator('[data-tab-id="o1"]').click({ button: "right" }).catch(() => {});
  await page.waitForTimeout(300);
  const orchCard = (await page.locator("#agent-card").textContent().catch(() => "")) || "";
  ok("S3-1: an orchestrator gets a card too", /kalpin lead/.test(orchCard) || /ship inc8/.test(orchCard), orchCard.slice(0, 80));

  await browser.close();
  console.log(`\ndashboard.inc8.s3.accept.mjs — Increment 8 Slice 3 GUI acceptance`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: all Inc8-S3 scenarios verified on screen"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.inc8.s3.accept.mjs crashed:", e); process.exit(2); });
