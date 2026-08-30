// GUI acceptance for Inc9 bricks b2 (context pill) + b3 (compaction badge):
//   b2 — a band node shows a "N% ctx" pill coloured by context_pct
//        (green <70 / amber 70-90 / red >90); no pill when the field is absent.
//   b3 — a discreet ⟳ compaction badge when recently_compacted is true; none otherwise.
// Real Chromium against the SHIPPED assets, intercepting /dashboard/state with a
// fixture whose tabs carry (or omit) context_pct / recently_compacted. Builder: web.
//
// Run:  cd <a dir with playwright installed>; node <repo>/assets/dashboard.inc9.b2b3.accept.mjs
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
const worker = (id, extra) => pTab({ id, name: id, role: "implementer", parentTabId: "o1", assignment: "kalpin-back:build/implementer", led: "idle", ...extra });
// A tichef mounts the 4-band view; workers carry (or omit) the Inc9 b2/b3 fields.
const s = () => ({
  nodes: [], unmapped: [], unassigned: [],
  projects: [
    { name: "kalpin-back", isMeta: false, hasOrchestrator: true, orchestrators: [{ id: "o1", name: "ta-lead", childCount: 5 }],
      nodes: [{ id: "build", rollupLed: "working", tabs: [
        pTab({ id: "o1", name: "ta-lead", role: "orchestrator", assignment: "kalpin-back:build/orchestrator", led: "idle" }),
        worker("w-ok", { context_pct: 55 }),
        worker("w-warn", { context_pct: 80 }),
        worker("w-crit", { context_pct: 96 }),
        worker("w-comp", { context_pct: 40, recently_compacted: true }),
        worker("w-clean", {}), // no context_pct, no recently_compacted
      ] }], unmapped: [] },
    { name: "méta", isMeta: true, hasOrchestrator: false, orchestrators: [],
      nodes: [], unmapped: [pTab({ id: "tc", name: "ta-tichef", role: "manager", orchestrator: "meta", led: "idle" })] },
  ],
});

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 950 } });
  await wireRoutes(page, s);
  await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
  await page.waitForSelector('[data-tab-id="w-ok"]', { timeout: 5000 }).catch(() => {});

  // b2: the pill renders with the threshold colour on each node.
  ok("b2: <70 shows a GREEN ctx pill", (await page.locator('[data-tab-id="w-ok"] .ctx-pill.ctx-ok').count()) === 1);
  ok("b2: 70-90 shows an AMBER ctx pill", (await page.locator('[data-tab-id="w-warn"] .ctx-pill.ctx-warn').count()) === 1);
  ok("b2: >90 shows a RED ctx pill", (await page.locator('[data-tab-id="w-crit"] .ctx-pill.ctx-crit').count()) === 1);
  const okText = (await page.locator('[data-tab-id="w-ok"] .ctx-pill').textContent().catch(() => "")) || "";
  ok("b2: the pill shows 'N% ctx'", /55% ctx/.test(okText), okText);
  // b2 absence: no pill when context_pct is missing.
  ok("b2: no ctx pill when context_pct is absent", (await page.locator('[data-tab-id="w-clean"] .ctx-pill').count()) === 0);

  // b3: the compaction badge shows only for the recently-compacted tab.
  ok("b3: ⟳ compaction badge on the recently-compacted tab", (await page.locator('[data-tab-id="w-comp"] .compact-badge').count()) === 1);
  ok("b3: no compaction badge on a normal tab", (await page.locator('[data-tab-id="w-ok"] .compact-badge').count()) === 0);
  ok("b3: no compaction badge on the clean tab", (await page.locator('[data-tab-id="w-clean"] .compact-badge').count()) === 0);

  await browser.close();
  console.log(`\ndashboard.inc9.b2b3.accept.mjs — Inc9 b2/b3 GUI acceptance`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: all Inc9 b2/b3 scenarios verified on screen"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.inc9.b2b3.accept.mjs crashed:", e); process.exit(2); });
