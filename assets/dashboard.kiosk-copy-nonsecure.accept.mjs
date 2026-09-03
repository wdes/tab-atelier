// GUI acceptance for the KIOSK copy-button in the REAL DEPLOYMENT CONTEXT: http://<LAN-IP>,
// a NON-SECURE origin where `navigator.clipboard` is UNDEFINED. This is the path the PO's
// browser actually takes — and the one the secure-context spy in dashboard.kiosk-copy.accept.mjs
// MASKED (it *defined* navigator.clipboard, so the fence-copy "worked" in CI but no-op'd in prod).
//
// Anti-built≠wired: here we DELETE navigator.clipboard (reproduce non-secure) and spy the LEGACY
// document.execCommand('copy') path, asserting the temp <textarea> carried the EXACT text and a
// "copié !" toast appeared. RED before the fallback+toast fix / GREEN after.
//   (a) click 📋 on a fenced code block  -> legacy copy of the EXACT raw block text + toast.
//   (b) click a non-resolvable filename ref -> legacy copy of the EXACT filename:line + toast.
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { chromium } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const read = (f) => readFileSync(join(HERE, f), "utf8");
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

  // ⭐ Reproduce the NON-SECURE http-LAN context: navigator.clipboard = undefined (as the PO's
  // browser sees it), and spy the LEGACY fallback so we can assert the EXACT copied text. We read
  // document.activeElement.value inside the execCommand('copy') spy — that's the temp <textarea>
  // the fallback selects right before copying, so its .value IS the payload the OS clipboard gets.
  await page.addInitScript(() => {
    try { Object.defineProperty(navigator, "clipboard", { value: undefined, configurable: true }); }
    catch { /* engine locked it — the fallback still triggers on the writeText-missing guard */ }
    window.__legacyCopied = [];
    const realExec = document.execCommand ? document.execCommand.bind(document) : () => true;
    document.execCommand = function (cmd, ...rest) {
      if (cmd === "copy") {
        const a = document.activeElement;
        window.__legacyCopied.push(a && "value" in a ? a.value : String((window.getSelection && window.getSelection()) || ""));
      }
      return realExec(cmd, ...rest);
    };
  });

  const CODE = "cargo build --release\necho \"<done>\"";
  const DETAIL_WITH_CODE = "Lance :\n```sh\n" + CODE + "\n```\nPuis vérifie.";
  const DECS = {
    code: { id: "code", project: "harness", title: "Décision avec code", reco: "A", detail: DETAIL_WITH_CODE, files: ["src/cli/decision.rs:520"], state: "open" },
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
  // Sanity: we really are in a non-secure-like context (no clipboard API), else the test is a lie.
  const hasClipboard = await page.evaluate(() => !!(navigator.clipboard && navigator.clipboard.writeText));
  ok("setup: navigator.clipboard is UNAVAILABLE (real http-LAN non-secure context)", !hasClipboard);

  await page.locator("#kiosk-toggle").click();
  await page.waitForSelector("#kiosk-panel .kk-card", { timeout: 4000 }).catch(() => {});
  const codeCard = '#kiosk-panel .kk-card[data-id="code"]';
  await page.locator(`${codeCard} .kk-detail-toggle`).click();
  await page.waitForTimeout(80);

  // ===== ⭐ (a) click 📋 -> legacy fallback copies the EXACT raw code (NOT a silent no-op) =====
  const copyBtn = page.locator(`${codeCard} .kk-detail .kk-copy-code`);
  ok("(a) the 📋 copy button is present", (await copyBtn.count()) === 1);
  await page.evaluate(() => { window.__legacyCopied = []; });
  await copyBtn.first().click();
  await page.waitForTimeout(60);
  const copied = await page.evaluate(() => window.__legacyCopied);
  ok("(a) ⭐ clicking 📋 triggers the legacy execCommand('copy') fallback exactly once", copied.length === 1, `copies=${JSON.stringify(copied)}`);
  ok("(a) ⭐ the fallback copied the EXACT raw block text", copied[0] === CODE, `got=${JSON.stringify(copied[0])}`);
  // ===== ⭐ (b) TOAST : a visible "copié !" confirmation appeared =====
  const toast = page.locator("#kk-copy-toast");
  ok("(b) ⭐ a 'copié !' toast is shown after copy", (await toast.count()) === 1 && (await toast.isVisible()) && /copié/i.test((await toast.textContent()) || ""), `toast=${(await toast.count()) ? await toast.textContent() : "<absent>"}`);

  // ===== ⭐ (c) filename ref (non-resolvable) also copies via the legacy path =====
  const ref = page.locator(`${codeCard} .kk-file-ref`);
  ok("(c) a non-resolvable code ref is a copyable affordance", (await ref.count()) === 1);
  await page.evaluate(() => { window.__legacyCopied = []; });
  await ref.first().click();
  await page.waitForTimeout(60);
  const refCopied = await page.evaluate(() => window.__legacyCopied);
  ok("(c) ⭐ clicking the filename ref legacy-copies the EXACT filename:line", refCopied.length === 1 && refCopied[0] === "src/cli/decision.rs:520", `got=${JSON.stringify(refCopied)}`);

  await browser.close();
  console.log(`\ndashboard.kiosk-copy-nonsecure.accept.mjs — copy-button in the REAL http-LAN (non-secure) context`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: clipboard undefined -> execCommand fallback copies EXACT text + 'copié !' toast (code block & filename ref)"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.kiosk-copy-nonsecure.accept.mjs crashed:", e); process.exit(2); });
