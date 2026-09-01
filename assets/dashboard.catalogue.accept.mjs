// GUI acceptance for Catalogue #39 SC2 (web VUE read-only).
//   Topbar "Catalogue" button -> #catalog-panel overlay (cold source, ON-DEMAND,
//   NOT in the 1.5s poll). Lists skills by proper name; expandable profile (prompt
//   via 'voir plus', specialty, conventions/tools/patterns); byMode metrics table +
//   DIRECTIONAL fresh_vs_resume + n; InsufficientSample explicit; NEVER pass/fail.
// Real Chromium against the SHIPPED assets, intercepting GET /catalog with a fixture.
//
// Run:  cd <a dir with playwright installed>; node <repo>/assets/dashboard.catalogue.accept.mjs
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
// A camelCase read-model (the SC1 contract): the tombstoned skills are already
// filtered server-side; the view just renders what it gets.
const catalog = () => ({
  skills: [
    { name: "deep-research", prompt: "Research across sources.", specialty: "web research", conventions: [], tools: ["WebSearch"], patterns: [], promptVersion: 2,
      // thin sample on one mode -> InsufficientSample (borne 6).
      metrics: { byMode: { fresh: { success: 1, problem: 0, tokensAvg: 10, costAvg: 1, n: 2 }, resume: { success: 1, problem: 0, tokensAvg: 10, costAvg: 1, n: 1 } } } },
    { name: "code-reviewer", prompt: longPrompt, specialty: "review diffs", conventions: ["AGENTS.md", "docs/dashboard.md"], tools: ["Read", "Grep"], patterns: ["adversarial-verify"], promptVersion: 4,
      metrics: { byMode: { fresh: { success: 12, problem: 3, tokensAvg: 8000, costAvg: 5, n: 15 }, resume: { success: 7, problem: 8, tokensAvg: 9000, costAvg: 6, n: 15 } } }, fresh_vs_resume: "FreshFavored" },
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
    if (p === "/catalog") { seen.count++; return route.fulfill({ contentType: "application/json", body: JSON.stringify(catalog()) }); }
    return route.fulfill({ status: 404, body: "" });
  });
}

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 950 } });
  const seen = { count: 0 };
  await wireRoutes(page, seen);
  await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });

  // ON-DEMAND: the panel is closed and NOT fetched until the button is clicked.
  ok("SC2: the Catalogue button is in the topbar", (await page.locator(".topbar #catalog-toggle").count()) === 1);
  ok("SC2: the panel is hidden on load", await page.locator("#catalog-panel").isHidden());
  await page.waitForTimeout(1800); // one poll cycle+
  ok("SC2: the catalog is NOT fetched by the poll loop (cold source)", seen.count === 0, `fetches=${seen.count}`);

  // Open -> fetch once -> panel visible with the skills.
  await page.locator("#catalog-toggle").click();
  await page.waitForSelector("#catalog-panel .cat-skill", { timeout: 4000 }).catch(() => {});
  ok("SC2: clicking Catalogue opens the panel", await page.locator("#catalog-panel").isVisible());
  ok("SC2: the catalog is fetched on-demand", seen.count === 1, `fetches=${seen.count}`);
  const names = await page.$$eval("#catalog-panel .cat-name", (els) => els.map((e) => e.textContent.trim()));
  ok("SC2: skills listed by proper name, sorted", JSON.stringify(names) === JSON.stringify(["code-reviewer", "deep-research"]), JSON.stringify(names));

  // Expand code-reviewer -> full profile.
  const cr = '#catalog-panel .cat-skill[data-skill="code-reviewer"]';
  await page.locator(`${cr} .cat-skill-head`).click();
  await page.waitForTimeout(120);
  const crTxt = (await page.locator(`${cr} .cat-skill-body`).textContent().catch(() => "")) || "";
  ok("SC2: profile shows specialty", /review diffs/.test(crTxt), crTxt.slice(0, 80));
  ok("SC2: profile shows conventions/tools/patterns", /AGENTS\.md/.test(crTxt) && /Grep/.test(crTxt) && /adversarial-verify/.test(crTxt));
  ok("SC2: a long prompt shows the 'voir plus' fold", (await page.locator(`${cr} .cat-prompt .ac-more`).count()) >= 1);
  // Metrics table (fresh + resume rows) + n.
  ok("SC2: byMode metrics table has fresh + resume rows", (await page.locator(`${cr} .metrics-table tbody tr`).count()) === 2);
  ok("SC2: the fresh_vs_resume verdict is DIRECTIONAL (FreshFavored) + n",
     /FreshFavored/.test(await page.locator(`${cr} .fvr-verdict`).getAttribute("data-verdict").catch(() => "")) &&
     /n=30/.test((await page.locator(`${cr} .fvr-verdict`).textContent().catch(() => "")) || ""));

  // Expand deep-research -> InsufficientSample explicit (borne 6).
  const dr = '#catalog-panel .cat-skill[data-skill="deep-research"]';
  await page.locator(`${dr} .cat-skill-head`).click();
  await page.waitForTimeout(120);
  const drVerdict = (await page.locator(`${dr} .fvr-verdict`).textContent().catch(() => "")) || "";
  ok("SC2: a thin sample shows InsufficientSample explicitly", /échantillon trop petit/.test(drVerdict), drVerdict);
  ok("SC2: InsufficientSample carries the data-verdict", (await page.locator(`${dr} .fvr-verdict[data-verdict="InsufficientSample"]`).count()) === 1);

  // BORNE 6 — never a pass/fail per-task verdict anywhere in the panel.
  const passFail = await page.locator("#catalog-panel .pass, #catalog-panel .fail, #catalog-panel [data-verdict=\"pass\"], #catalog-panel [data-verdict=\"fail\"]").count();
  ok("SC2: NO pass/fail marker in the catalog (direction only)", passFail === 0, `passFail=${passFail}`);

  // Refresh re-fetches; close hides.
  await page.locator("#catalog-panel .cat-refresh").click();
  await page.waitForTimeout(200);
  ok("SC2: refresh re-fetches the read-model", seen.count === 2, `fetches=${seen.count}`);
  await page.locator("#catalog-panel .cat-close").click();
  ok("SC2: the × button closes the panel", await page.locator("#catalog-panel").isHidden());

  await browser.close();
  console.log(`\ndashboard.catalogue.accept.mjs — Catalogue #39 SC2 GUI acceptance`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: all SC2 catalogue scenarios verified on screen"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.catalogue.accept.mjs crashed:", e); process.exit(2); });
