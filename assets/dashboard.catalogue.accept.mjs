// GUI acceptance for Catalogue #39 SC2 (web VUE read-only), reconciled to the LIVE
// SC1 contract: GET /catalog/list -> { retired, skills[{skill, ...,
// metrics.byMode.{fresh,resume}{spawns,success,problem,tokensAvg,costAvg},
// freshVsResume{verdict, freshN, resumeN}}] }. The verdict is rendered VERBATIM
// (server owns G1/MIN_SAMPLE=3) — no JS re-gate.
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

const longPrompt = "You are a rigorous code reviewer. " + Array.from({ length: 60 }, (_, i) => `w${i}`).join(" ");
const mode = (spawns, success, problem, tokensAvg, costAvg) => ({ spawns, success, problem, tokensAvg, costAvg });
// The camelCase read-model the live /catalog/list serves.
const catalog = () => ({
  retired: [],
  skills: [
    // n=2/1 per arm < MIN_SAMPLE(3) -> the SERVER already returns insufficientSample.
    { skill: "deep-research", prompt: "Research across sources.", specialty: "web research", conventions: [], tools: ["WebSearch"], patterns: [], promptVersion: 2, usageCount: 3,
      metrics: { byMode: { fresh: mode(2, 2, 0, 5000, 3), resume: mode(1, 1, 0, 5200, 3) } },
      freshVsResume: { verdict: "insufficientSample", freshN: 2, resumeN: 1, deliveryDelta: null, tokensRatio: null } },
    { skill: "code-reviewer", prompt: longPrompt, specialty: "review diffs", conventions: ["AGENTS.md", "docs/dashboard.md"], tools: ["Read", "Grep"], patterns: ["adversarial-verify"], promptVersion: 4, usageCount: 30,
      metrics: { byMode: { fresh: mode(15, 12, 3, 8000, 5), resume: mode(15, 7, 8, 9000, 6) } },
      freshVsResume: { verdict: "freshFavored", freshN: 15, resumeN: 15, deliveryDelta: 0.33, tokensRatio: 0.89 } },
  ],
});

async function wireRoutes(page, seen) {
  await page.route(`${ORIGIN}/**`, async (route) => {
    const p = new URL(route.request().url()).pathname;
    if (p === "/dashboard") return route.fulfill({ contentType: "text/html; charset=utf-8", body: HTML });
    if (p === "/assets/dashboard.js") return route.fulfill({ contentType: "application/javascript; charset=utf-8", body: JS });
    if (p === "/assets/dashboard.css") return route.fulfill({ contentType: "text/css; charset=utf-8", body: CSS });
    if (p === "/tabs/usage") return route.fulfill({ contentType: "application/json", body: "[]" });
    if (p === "/dashboard/activity") return route.fulfill({ contentType: "application/json", body: "{}" });
    if (p === "/dashboard/state") return route.fulfill({ contentType: "application/json", body: "{\"nodes\":[],\"unmapped\":[]}" });
    if (p === "/catalog/list") { seen.count++; return route.fulfill({ contentType: "application/json", body: JSON.stringify(catalog()) }); }
    return route.fulfill({ status: 404, body: "" });
  });
}

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 950 } });
  const seen = { count: 0 };
  await wireRoutes(page, seen);
  await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });

  ok("SC2: the Catalogue button is in the topbar", (await page.locator(".topbar #catalog-toggle").count()) === 1);
  ok("SC2: the panel is hidden on load", await page.locator("#catalog-panel").isHidden());
  await page.waitForTimeout(1800);
  ok("SC2: the catalog is NOT fetched by the poll loop (cold source)", seen.count === 0, `fetches=${seen.count}`);

  await page.locator("#catalog-toggle").click();
  await page.waitForSelector("#catalog-panel .cat-skill", { timeout: 4000 }).catch(() => {});
  ok("SC2: clicking Catalogue opens the panel", await page.locator("#catalog-panel").isVisible());
  ok("SC2: the catalog is fetched on-demand from /catalog/list", seen.count === 1, `fetches=${seen.count}`);
  const names = await page.$$eval("#catalog-panel .cat-name", (els) => els.map((e) => e.textContent.trim()));
  ok("SC2: skills listed by proper name (skill), sorted", JSON.stringify(names) === JSON.stringify(["code-reviewer", "deep-research"]), JSON.stringify(names));

  // Expand code-reviewer -> full profile + byMode ledger + directional verdict.
  const cr = '#catalog-panel .cat-skill[data-skill="code-reviewer"]';
  await page.locator(`${cr} .cat-skill-head`).click();
  await page.waitForTimeout(120);
  const crTxt = (await page.locator(`${cr} .cat-skill-body`).textContent().catch(() => "")) || "";
  ok("SC2: profile shows specialty", /review diffs/.test(crTxt));
  ok("SC2: profile shows conventions/tools/patterns", /AGENTS\.md/.test(crTxt) && /Grep/.test(crTxt) && /adversarial-verify/.test(crTxt));
  ok("SC2: a long prompt shows the 'voir plus' fold", (await page.locator(`${cr} .cat-prompt .ac-more`).count()) >= 1);
  ok("SC2: byMode table has fresh + resume rows (spawns/success/problem/tokensAvg/costAvg)", (await page.locator(`${cr} .metrics-table tbody tr`).count()) === 2);
  const crv = (await page.locator(`${cr} .fvr-verdict`).getAttribute("data-verdict").catch(() => "")) || "";
  const crvTxt = (await page.locator(`${cr} .fvr-verdict`).textContent().catch(() => "")) || "";
  ok("SC2: the server verdict is rendered VERBATIM (freshFavored)", crv === "freshFavored", crv);
  ok("SC2: the verdict surfaces the per-arm sample sizes (freshN/resumeN)", /fresh n=15/.test(crvTxt) && /resume n=15/.test(crvTxt), crvTxt);

  // deep-research: the SERVER's insufficientSample is shown verbatim (no JS re-gate).
  const dr = '#catalog-panel .cat-skill[data-skill="deep-research"]';
  await page.locator(`${dr} .cat-skill-head`).click();
  await page.waitForTimeout(120);
  const drv = (await page.locator(`${dr} .fvr-verdict`).getAttribute("data-verdict").catch(() => "")) || "";
  const drvTxt = (await page.locator(`${dr} .fvr-verdict`).textContent().catch(() => "")) || "";
  ok("SC2: insufficientSample verdict shown verbatim from the server", drv === "insufficientSample", drv);
  ok("SC2: InsufficientSample is explicit ('échantillon trop petit')", /échantillon trop petit/.test(drvTxt), drvTxt);

  // Never a pass/fail per-task verdict anywhere.
  const passFail = await page.locator("#catalog-panel .pass, #catalog-panel .fail, #catalog-panel [data-verdict=\"pass\"], #catalog-panel [data-verdict=\"fail\"]").count();
  ok("SC2: NO pass/fail marker (direction only)", passFail === 0, `passFail=${passFail}`);

  await page.locator("#catalog-panel .cat-refresh").click();
  await page.waitForTimeout(200);
  ok("SC2: refresh re-fetches /catalog/list", seen.count === 2, `fetches=${seen.count}`);
  await page.locator("#catalog-panel .cat-close").click();
  ok("SC2: the × button closes the panel", await page.locator("#catalog-panel").isHidden());

  await browser.close();
  console.log(`\ndashboard.catalogue.accept.mjs — Catalogue #39 SC2 GUI acceptance (live contract)`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: all SC2 catalogue scenarios verified on screen"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.catalogue.accept.mjs crashed:", e); process.exit(2); });
