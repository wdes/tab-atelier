// ELARGIE UX acceptance for the KIOSK follow-up fix (feat/kiosk-fix-links-presentation).
// LESSON from FU2 (c4591c4): its test only checked "a toggle exists" + "no /decisions/file"
// — it never asserted the UX the PO actually USES (clickability), so it let two regressions
// through: code refs became DEAD copyable text, and the reader lost the click. This test
// asserts the REAL UX on the browser path — the actual href + the actual DOM structure,
// not the mere presence of a toggle:
//   (a) a DOC .md (served outbox zone) -> a clickable <a> VIEWER link (assert its href);
//   (b) a CODE-REF (auth.rs:76-78, src/…:line) -> a clickable <a> BLOB REPO link
//       (assert the CONSTRUCTED repo URL + #L anchor — NOT dead text, NO /decisions/file 404);
//   (c) PRESENTATION: the short summary is always visible + clicking (+) EXPANDS the long
//       detail, clicking (-) COLLAPSES it (assert the real expanded/collapsed state).
// RED before the fix (code ref is a dead span with no href), GREEN after.
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

  const DETAIL_BODY = "**Enjeux** — perte upstream possible.\nOption A : PR maintenant.\nReco : A.";
  // ONE decision carries: a servable doc, a code ref with a range, a code ref with a full
  // path + single line, AND a long-form detail — so the three UX facets ride one card.
  const DEC = {
    id: "uxfix", project: "harness", title: "Résumé court toujours visible",
    reco: "A", detail: DETAIL_BODY, state: "open",
    files: ["~/Dev/outbox/rep.md", "auth.rs:76-78", "src/cli/decision.rs:520"],
  };

  await page.route(`${ORIGIN}/**`, async (route) => {
    const p = new URL(route.request().url()).pathname;
    if (p === "/dashboard") return route.fulfill({ contentType: "text/html; charset=utf-8", body: HTML });
    if (p === "/assets/dashboard.js") return route.fulfill({ contentType: "application/javascript; charset=utf-8", body: JS });
    if (p === "/assets/dashboard.css") return route.fulfill({ contentType: "text/css; charset=utf-8", body: CSS });
    if (p === "/tabs/usage") return route.fulfill({ contentType: "application/json", body: "[]" });
    if (p === "/dashboard/activity") return route.fulfill({ contentType: "application/json", body: "{}" });
    if (p === "/dashboard/state") return route.fulfill({ contentType: "application/json", body: '{"nodes":[],"unmapped":[]}' });
    if (p === "/decisions") return route.fulfill({ contentType: "application/json", body: JSON.stringify({ decisions: [DEC] }) });
    return route.fulfill({ status: 401, body: "unauthorized" });
  });

  await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
  await page.locator("#kiosk-toggle").click();
  await page.waitForSelector("#kiosk-panel .kk-card", { timeout: 4000 }).catch(() => {});

  const card = '#kiosk-panel .kk-card[data-id="uxfix"]';
  ok("the card renders", (await page.locator(card).count()) === 1);

  // ===== (a) DOC .md -> clickable VIEWER link, assert the real href =====
  const docLink = page.locator(`${card} a.kk-file[href*="/decisions/file"]`);
  ok("(a) doc .md is a clickable <a> (not dead text)", (await docLink.count()) === 1);
  const docHref = (await docLink.getAttribute("href")) || "";
  ok("(a) the doc href points at the sandboxed viewer with the encoded path", /\/decisions\/file\?path=~%2FDev%2Foutbox%2Frep\.md/.test(docHref), `href=${docHref}`);
  ok("(a) the doc href carries the page token", /token=TESTTOKEN/.test(docHref), `href=${docHref}`);

  // ===== (b) CODE-REF -> clickable BLOB REPO link, assert the constructed URL + anchor =====
  // Range ref: auth.rs:76-78 -> …/auth.rs#L76-L78 ; NOT a dead span, NOT /decisions/file.
  const codeLinks = page.locator(`${card} a.kk-file-repo`);
  ok("(b) code refs are clickable <a> repo links (not dead spans)", (await codeLinks.count()) === 2, `count=${await codeLinks.count()}`);
  const hrefs = await codeLinks.evaluateAll((els) => els.map((e) => e.getAttribute("href")));
  const rangeHref = hrefs.find((h) => /auth\.rs/.test(h)) || "";
  ok("(b) range ref builds a blob URL with a #L76-L78 anchor", /^https?:\/\/.+\/auth\.rs#L76-L78$/.test(rangeHref), `href=${rangeHref}`);
  ok("(b) the blob URL targets a real repo host (github/gitlab)", /github\.com|gitlab/.test(rangeHref), `href=${rangeHref}`);
  const pathHref = hrefs.find((h) => /decision\.rs/.test(h)) || "";
  ok("(b) full-path ref builds a blob URL with a #L520 anchor", /\/src\/cli\/decision\.rs#L520$/.test(pathHref), `href=${pathHref}`);
  // The incident must be gone: NO code ref routes through /decisions/file (that 404s), and
  // NO code ref is a dead copyable span anymore.
  ok("(b) NO code ref routes through /decisions/file (no 404)", !hrefs.some((h) => /\/decisions\/file/.test(h)), `hrefs=${JSON.stringify(hrefs)}`);
  ok("(b) code refs are no longer dead copyable spans", (await page.locator(`${card} .kk-file-ref`).count()) === 0);

  // ===== (c) PRESENTATION: short summary always visible + (+) expands / (-) collapses =====
  ok("(c) the short summary (title) is always visible", (await page.locator(`${card} .kk-title`).isVisible()));
  const toggle = page.locator(`${card} .kk-detail-toggle`);
  ok("(c) a (+) toggle is present and collapsed by default", (await toggle.count()) === 1 && (await toggle.textContent()) === "(+)");
  ok("(c) the long detail is HIDDEN by default", !(await page.locator(`${card} .kk-detail`).isVisible()));
  await toggle.click();
  await page.waitForTimeout(80);
  ok("(c) clicking (+) EXPANDS the detail and flips to (-)", (await toggle.textContent()) === "(-)" && (await page.locator(`${card} .kk-detail`).isVisible()));
  const detailText = (await page.locator(`${card} .kk-detail`).textContent()) || "";
  ok("(c) the expanded detail shows the long-form body", detailText.includes("Enjeux") && detailText.includes("Option A"), `text=${detailText}`);
  await toggle.click();
  await page.waitForTimeout(80);
  ok("(c) clicking (-) COLLAPSES the detail again", (await toggle.textContent()) === "(+)" && !(await page.locator(`${card} .kk-detail`).isVisible()));

  await browser.close();
  console.log(`\ndashboard.kiosk.uxlinks.accept.mjs — ELARGIE UX (doc href + code blob href + toggle expand/collapse)`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: doc -> viewer link, code ref -> clickable blob repo link (#L anchor), summary+toggle expand/collapse"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.kiosk.uxlinks.accept.mjs crashed:", e); process.exit(2); });
