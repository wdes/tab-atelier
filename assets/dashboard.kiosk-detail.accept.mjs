// GUI acceptance for the KIOSK detail-toggle (feat/kiosk-detail-toggle), wired to the
// LIVE render + the real /decisions read-model. Anti-built≠wired at the BROWSER level:
// a toggle that merely EXISTS in the DOM proves nothing — we drive the two states on the
// real browser path and assert the wiring:
//   STATE 1 (detail present)  -> a (+) toggle is VISIBLE, the body is HIDDEN by default;
//                                clicking (+) EXPANDS it and shows the detail (with simple
//                                markdown: **bold** -> <strong>, newlines -> <br>);
//                                clicking (-) COLLAPSES it again.
//   STATE 2 (detail absent)   -> NO toggle, NO body — graceful degradation, zero regression
//                                (the card still renders: title, Lu, Trancher…).
// RED on the pre-toggle build (no .kk-detail-toggle, no expand), GREEN after.
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

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 950 } });

  // Two decisions in the SAME project: one carries a long-form `detail`, one does not.
  const DETAIL_BODY = "**Enjeux** — perte upstream possible.\nOption A : PR maintenant.\nReco : A.";
  const DECS = {
    rich: { id: "rich", project: "harness", title: "Orphelines mx→upstream", reco: "A", detail: DETAIL_BODY, files: [], state: "open" },
    plain: { id: "plain", project: "harness", title: "Décision sans détail", reco: "GO", files: [], state: "open" },
  };

  await page.route(`${ORIGIN}/**`, async (route) => {
    const p = new URL(route.request().url()).pathname;
    if (p === "/dashboard") return route.fulfill({ contentType: "text/html; charset=utf-8", body: HTML });
    if (p === "/assets/dashboard.js") return route.fulfill({ contentType: "application/javascript; charset=utf-8", body: JS });
    if (p === "/assets/dashboard.css") return route.fulfill({ contentType: "text/css; charset=utf-8", body: CSS });
    if (p === "/tabs/usage") return route.fulfill({ contentType: "application/json", body: "[]" });
    if (p === "/dashboard/activity") return route.fulfill({ contentType: "application/json", body: "{}" });
    if (p === "/dashboard/state") return route.fulfill({ contentType: "application/json", body: '{"nodes":[],"unmapped":[]}' });
    if (p === "/decisions") return route.fulfill({ contentType: "application/json", body: JSON.stringify({ decisions: Object.values(DECS) }) });
    return route.fulfill({ status: 401, body: "unauthorized" });
  });

  await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
  await page.locator("#kiosk-toggle").click();
  await page.waitForSelector("#kiosk-panel .kk-card", { timeout: 4000 }).catch(() => {});

  const rich = '#kiosk-panel .kk-card[data-id="rich"]';
  const plain = '#kiosk-panel .kk-card[data-id="plain"]';

  // ===== ⭐ STATE 1: detail PRESENT -> toggle visible, collapsed by default =====
  ok("state1: the detail-carrying card renders", (await page.locator(rich).count()) === 1);
  const toggle = page.locator(`${rich} .kk-detail-toggle`);
  ok("state1: a (+)/(-) toggle is VISIBLE when detail is present", (await toggle.count()) === 1 && (await toggle.isVisible()));
  ok("state1: the toggle starts collapsed (+)", (await toggle.textContent()) === "(+)", `text=${await toggle.textContent().catch(() => "")}`);
  ok("state1: the detail body is HIDDEN by default", (await page.locator(`${rich} .kk-detail`).count()) === 1 && !(await page.locator(`${rich} .kk-detail`).isVisible()));

  // Click (+) -> EXPANDS and shows the detail content, simple-markdown rendered.
  await toggle.click();
  await page.waitForTimeout(80);
  ok("state1: after click the toggle flips to (-)", (await toggle.textContent()) === "(-)", `text=${await toggle.textContent().catch(() => "")}`);
  ok("state1: after click the detail body is NOW visible", await page.locator(`${rich} .kk-detail`).isVisible());
  const detailText = (await page.locator(`${rich} .kk-detail`).textContent()) || "";
  ok("state1: the expanded body shows the detail text", detailText.includes("Enjeux") && detailText.includes("Option A") && detailText.includes("Reco : A"), `text=${detailText}`);
  const detailHtml = (await page.locator(`${rich} .kk-detail`).innerHTML()) || "";
  ok("state1: simple markdown is rendered (**bold** -> <strong>, newlines -> <br>)", /<strong>Enjeux<\/strong>/.test(detailHtml) && /<br\s*\/?>/.test(detailHtml), `html=${detailHtml}`);

  // Click (-) -> COLLAPSES again.
  await toggle.click();
  await page.waitForTimeout(80);
  ok("state1: clicking (-) collapses the body again", (await toggle.textContent()) === "(+)" && !(await page.locator(`${rich} .kk-detail`).isVisible()));

  // ===== ⭐ STATE 2: detail ABSENT -> no toggle, no body, no regression =====
  ok("state2: the detail-less card still renders (no regression)", (await page.locator(plain).count()) === 1);
  ok("state2: NO toggle when detail is absent (feature-detect)", (await page.locator(`${plain} .kk-detail-toggle`).count()) === 0);
  ok("state2: NO detail body when detail is absent", (await page.locator(`${plain} .kk-detail`).count()) === 0);
  // Zero regression: the Trancher button is intact; the Lu checkbox was removed (item 4).
  ok("state2: Trancher button intact, no Lu checkbox (item 4)", (await page.locator(`${plain} .kk-send`).count()) === 1 && (await page.locator(`${plain} .kk-lu`).count()) === 0);

  await browser.close();
  console.log(`\ndashboard.kiosk-detail.accept.mjs — KIOSK detail-toggle (2 états, browser path)`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: detail present -> visible toggle, (+) expands + renders markdown, (-) collapses; detail absent -> no toggle, no regression"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.kiosk-detail.accept.mjs crashed:", e); process.exit(2); });
