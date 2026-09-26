// GUI acceptance for the KIOSK copy-button (feat/kiosk-copy-button), driven on the REAL
// browser path. Anti-built≠wired: a 📋 that merely EXISTS proves nothing — we SPY on
// navigator.clipboard.writeText and assert the EXACT text it was called with (not "a button
// exists"). Four requirements, RED before / GREEN after:
//   (a) COPIE RÉELLE  : click 📋 on a fenced code block -> writeText(<exact raw block text>).
//   (2) FEATURE-DETECT: a detail WITH a fence -> button present; a detail WITHOUT code -> none.
//   (b) VOLET b       : a bare filename ref whose repo URL is NOT constructible (empty
//                       repo-blob-base) -> copyable affordance -> writeText(<exact filename:line>).
//   (4) A11Y          : the copy button is keyboard-focusable + has an ARIA label; the copied
//                       payload is TEXT (XSS-safe — never interpreted).
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { chromium } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const read = (f) => readFileSync(join(HERE, f), "utf8");
// Empty the repo-blob-base meta so a code ref is NOT resolvable -> it degrades to the
// copyable filename affordance (volet b's exact scenario), instead of a repo <a> link.
const HTML = read("dashboard.html").replace(
  /<meta name="repo-blob-base"[^>]*>/,
  '<meta name="repo-blob-base" content="">'
);
const JS = read("dashboard.js"), CSS = read("dashboard.css");
const ORIGIN = "http://ta-dash.local", TOKEN = "TESTTOKEN";

let failures = 0;
const ok = (label, cond, detail = "") => {
  if (cond) console.log(`  ✓ ${label}`);
  else { failures++; console.log(`  ✗ ${label}${detail ? ` -- ${detail}` : ""}`); }
};

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 950 } });

  // SPY on the clipboard BEFORE any script runs: record every writeText argument into
  // window.__copied so we can assert the EXACT text (the real UX, not "a button exists").
  await page.addInitScript(() => {
    window.__copied = [];
    const spy = { writeText: (t) => { window.__copied.push(String(t)); return Promise.resolve(); } };
    try { Object.defineProperty(navigator, "clipboard", { value: spy, configurable: true }); }
    catch { /* some engines lock it — best effort */ }
  });

  const CODE = "cargo build --release\necho \"<done>\"";
  const DETAIL_WITH_CODE = "Lance :\n```sh\n" + CODE + "\n```\nPuis vérifie.";
  const DECS = {
    code:  { id: "code",  project: "harness", title: "Décision avec code", reco: "A", detail: DETAIL_WITH_CODE, files: ["src/cli/decision.rs:520"], state: "open" },
    plain: { id: "plain", project: "harness", title: "Décision prose nue", reco: "GO", detail: "**Enjeux** — rien à copier.\nReco : GO.", files: [], state: "open" },
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

  const codeCard = '#kiosk-panel .kk-card[data-id="code"]';
  const plainCard = '#kiosk-panel .kk-card[data-id="plain"]';

  // Expand the code-carrying decision's detail (the copy button lives in the detail body).
  await page.locator(`${codeCard} .kk-detail-toggle`).click();
  await page.waitForTimeout(80);

  // ===== ⭐ (a) COPIE RÉELLE : click 📋 -> writeText(EXACT raw code) =====
  const copyBtn = page.locator(`${codeCard} .kk-detail .kk-copy-code`);
  ok("(a) a 📋 copy button is present next to the code block", (await copyBtn.count()) === 1);
  await page.evaluate(() => { window.__copied = []; });
  await copyBtn.first().click();
  await page.waitForTimeout(50);
  const copied = await page.evaluate(() => window.__copied);
  ok("(a) ⭐ clicking 📋 calls navigator.clipboard.writeText exactly once", copied.length === 1, `calls=${JSON.stringify(copied)}`);
  ok("(a) ⭐ writeText got the EXACT raw block text (fence/lang stripped)", copied[0] === CODE, `got=${JSON.stringify(copied[0])}`);
  // Brief visual feedback (✓/copié) after the click.
  const fb = (await copyBtn.first().getAttribute("title")) || (await copyBtn.first().textContent()) || "";
  ok("(a) brief visual feedback after copy (✓/copié)", /copié|✓/.test(fb), `feedback=${fb}`);

  // ===== ⭐ (2) FEATURE-DETECT : prose-only detail -> NO copy button =====
  await page.locator(`${plainCard} .kk-detail-toggle`).click();
  await page.waitForTimeout(80);
  ok("(2) ⭐ NO copy button on a code-less (prose) detail", (await page.locator(`${plainCard} .kk-copy-code`).count()) === 0);
  ok("(2) the prose detail still renders (no regression)", (await page.locator(`${plainCard} .kk-detail`).isVisible()));

  // ===== ⭐ (b) VOLET b : non-resolvable filename ref -> copyable -> exact filename:line =====
  const ref = page.locator(`${codeCard} .kk-file-ref`);
  ok("(b) a non-resolvable code ref degrades to a copyable affordance (not a dead link)", (await ref.count()) === 1);
  await page.evaluate(() => { window.__copied = []; });
  await ref.first().click();
  await page.waitForTimeout(50);
  const refCopied = await page.evaluate(() => window.__copied);
  ok("(b) ⭐ clicking the filename ref calls writeText with the EXACT filename:line", refCopied.length === 1 && refCopied[0] === "src/cli/decision.rs:520", `got=${JSON.stringify(refCopied)}`);

  // ===== ⭐ (4) A11Y : focusable + ARIA label ; XSS-safe (payload is TEXT) =====
  const btnAria = await copyBtn.first().getAttribute("aria-label");
  ok("(4) the 📋 button has an ARIA label", !!btnAria && btnAria.length > 0, `aria-label=${btnAria}`);
  // Keyboard-focusable: a real <button> is; assert it can hold focus.
  await copyBtn.first().focus();
  ok("(4) the 📋 button is keyboard-focusable", await copyBtn.first().evaluate((el) => el === document.activeElement));
  // The filename ref is a role=button, keyboard-operable (Enter copies).
  await page.evaluate(() => { window.__copied = []; });
  await ref.first().focus();
  await page.keyboard.press("Enter");
  await page.waitForTimeout(50);
  const kbdCopied = await page.evaluate(() => window.__copied);
  ok("(4) the filename ref copies via keyboard (Enter)", kbdCopied.length === 1 && kbdCopied[0] === "src/cli/decision.rs:520", `got=${JSON.stringify(kbdCopied)}`);
  // XSS-safe: the code contained <done> — assert it stayed TEXT (escaped in the DOM, and the
  // copied payload is the raw string, never an executed node).
  const codeHtml = await page.locator(`${codeCard} .kk-code`).innerHTML();
  ok("(4) XSS-safe: angle brackets in the code are escaped in the DOM", /&lt;done&gt;/.test(codeHtml) && !/<done>/.test(codeHtml), `html=${codeHtml}`);

  await browser.close();
  console.log(`\ndashboard.kiosk-copy.accept.mjs — KIOSK copy-button (real clipboard spy, browser path)`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: click 📋 -> writeText(exact code); prose -> no button; filename ref -> writeText(exact ref); a11y + XSS-safe"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.kiosk-copy.accept.mjs crashed:", e); process.exit(2); });
