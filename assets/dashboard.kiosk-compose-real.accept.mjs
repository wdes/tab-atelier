// ⭐ SMOKE-COMPOSE-RÉEL — the anti-built≠wired GUI acceptance for the KIOSK panel rendering
// the ACTUAL output of the `decision compose` TOOL (not a hand-crafted decision object). The
// previous smoke (dashboard.kiosk-copy*.accept.mjs) asserted a CRAFTED decision → the render
// bugs of the REAL compose output slipped through. Here we RUN the real binary, `decision list`
// its folded read-model, serve THAT verbatim, and drive the panel in the REAL http-LAN
// (NON-SECURE, navigator.clipboard=undefined) context. RED before the fixes / GREEN after.
//
// The 6 checks, all on the REAL compose output:
//   (a) --files with 2 refs render as N SEPARATE clickable links (not 1 concatenated 404).
//   (b) a .md under ~/Dev/outbox → the /decisions/file VIEWER (not a github-blob 404).
//   (c) a code-ref (path:line) → a github-blob REPO link.
//   (d) --link → a real clickable <a href> in the detail (not inert prose text).
//   (e) --summary → rendered + VISIBLE under the bold title, above the toggle.
//   (f) 📋 on the compose-generated --command block → legacy execCommand copies the EXACT text + toast.
//
// Run: node assets/dashboard.kiosk-compose-real.accept.mjs
import { readFileSync, mkdtempSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { tmpdir } from "node:os";
import { execFileSync } from "node:child_process";
import { chromium } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = dirname(HERE);
const read = (f) => readFileSync(join(HERE, f), "utf8");
const HTML = read("dashboard.html"); // keep the DEFAULT repo-blob-base meta (we assert blob links)
const JS = read("dashboard.js"), CSS = read("dashboard.css");
const ORIGIN = "http://ta-dash.local", TOKEN = "TESTTOKEN";
const BIN = join(ROOT, "target", "release", "tab-atelier");

let failures = 0;
const ok = (label, cond, detail = "") => {
  if (cond) console.log(`  ✓ ${label}`);
  else { failures++; console.log(`  ✗ ${label}${detail ? ` -- ${detail}` : ""}`); }
};

// ---- STEP 1: push a decision through the REAL `decision compose` tool, then `list` it back. ----
// This is the whole point: the JSON the dashboard renders is the tool's ACTUAL output, byte for
// byte — comma-joined --files, --link folded into the detail, etc. No hand-crafting.
const LINK = "https://github.com/a-biskoazh/tab-atelier-mx/pull/42";
const CMD = "cargo build --release --bin tab-atelier";
const DOC = "~/Dev/outbox/bridge.md";
const CODEREF = "src/cli/decision.rs:741";
const SUMMARY = "Résumé court en 2 lignes pour le smoke. Doit s'afficher sous le titre.";

const tmp = mkdtempSync(join(tmpdir(), "kiosk-compose-"));
const decPath = join(tmp, "decisions.jsonl");
const env = { ...process.env, TAB_ATELIER_DECISIONS_PATH: decPath };
execFileSync(BIN, [
  "decision", "compose", "--id", "smoke1", "--project", "harness",
  "--title", "Titre décision smoke",
  "--summary", SUMMARY,
  "--enjeux", "Prouver le rendu réel de compose.",
  "--reco", "Option A", "--effort", "M",
  "--files", `${DOC},${CODEREF}`, // ⭐ the real PO usage: 2 refs comma-joined in ONE --files
  "--command", CMD,
  "--link", LINK,
], { env });
const listOut = execFileSync(BIN, ["decision", "list"], { env }).toString();
const decisions = JSON.parse(listOut);
const DECISIONS_BODY = JSON.stringify({ decisions });

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 950 } });

  // ⭐ Reproduce the NON-SECURE http-LAN context + spy the legacy execCommand('copy') payload.
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

  await page.route(`${ORIGIN}/**`, async (route) => {
    const p = new URL(route.request().url()).pathname;
    if (p === "/dashboard") return route.fulfill({ contentType: "text/html; charset=utf-8", body: HTML });
    if (p === "/assets/dashboard.js") return route.fulfill({ contentType: "application/javascript; charset=utf-8", body: JS });
    if (p === "/assets/dashboard.css") return route.fulfill({ contentType: "text/css; charset=utf-8", body: CSS });
    if (p === "/tabs/usage") return route.fulfill({ contentType: "application/json", body: "[]" });
    if (p === "/dashboard/activity") return route.fulfill({ contentType: "application/json", body: "{}" });
    if (p === "/dashboard/state") return route.fulfill({ contentType: "application/json", body: '{"nodes":[],"unmapped":[]}' });
    if (p === "/decisions") return route.fulfill({ contentType: "application/json", body: DECISIONS_BODY });
    return route.fulfill({ status: 401, body: "unauthorized" });
  });

  await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
  const hasClipboard = await page.evaluate(() => !!(navigator.clipboard && navigator.clipboard.writeText));
  ok("setup: navigator.clipboard is UNAVAILABLE (real http-LAN non-secure context)", !hasClipboard);

  await page.locator("#kiosk-toggle").click();
  await page.waitForSelector("#kiosk-panel .kk-card", { timeout: 4000 }).catch(() => {});
  const card = '#kiosk-panel .kk-card[data-id="smoke1"]';
  ok("the compose decision rendered a card", (await page.locator(card).count()) === 1);

  // ===== (a) --files: 2 refs → 2 SEPARATE links (not 1 concatenated) =====
  const fileLinks = page.locator(`${card} .kk-files > *`);
  const nFiles = await fileLinks.count();
  ok("(a) ⭐ 2 --files refs render as 2 SEPARATE entries (not 1 concatenated link)", nFiles === 2, `got ${nFiles} entr(y|ies)`);
  const hrefs = await page.locator(`${card} .kk-files a`).evaluateAll((as) => as.map((a) => a.getAttribute("href")));
  const texts = await fileLinks.evaluateAll((els) => els.map((e) => (e.textContent || "").trim()));
  ok("(a) no entry text is the concatenated blob", !texts.some((t) => t.includes(",")), `texts=${JSON.stringify(texts)}`);

  // ===== (b) the .md under ~/Dev/outbox → the /decisions/file VIEWER (not a github blob) =====
  const docHref = hrefs.find((h) => h && h.includes("/decisions/file"));
  ok("(b) ⭐ the .md outbox ref → the /decisions/file VIEWER link", !!docHref && /path=/.test(docHref), `hrefs=${JSON.stringify(hrefs)}`);
  ok("(b) the .md ref is NOT routed to a github blob (would 404)", !hrefs.some((h) => h && h.includes("github.com") && h.includes("bridge.md")), `hrefs=${JSON.stringify(hrefs)}`);

  // ===== (c) the code-ref (path:line) → a github-blob REPO link with the #L anchor =====
  const codeHref = hrefs.find((h) => h && h.includes("github.com") && h.includes("decision.rs"));
  ok("(c) ⭐ the code-ref → a github-blob repo link (#L741)", !!codeHref && /decision\.rs#L741$/.test(codeHref), `hrefs=${JSON.stringify(hrefs)}`);

  // ===== (d) --link → a clickable <a href> in the detail (not inert prose) =====
  await page.locator(`${card} .kk-detail-toggle`).click();
  await page.waitForTimeout(80);
  const linkA = page.locator(`${card} .kk-detail a[href="${LINK}"]`);
  ok("(d) ⭐ --link is a clickable <a href> in the detail (not inert text)", (await linkA.count()) >= 1, `expected an <a href="${LINK}">`);

  // ===== (e) --summary → rendered + VISIBLE under the bold title =====
  const summary = page.locator(`${card} .kk-summary`);
  const sumVisible = (await summary.count()) === 1 && (await summary.first().isVisible());
  const sumText = (await summary.count()) ? ((await summary.first().textContent()) || "").trim() : "";
  ok("(e) ⭐ --summary renders + is VISIBLE under the title", sumVisible && sumText.startsWith("Résumé court"), `visible=${sumVisible} text=${JSON.stringify(sumText)}`);
  // Structural: the summary sits BEFORE the detail toggle body (titre → résumé → toggle → detail).
  const orderOk = await page.locator(card).evaluate((c) => {
    const s = c.querySelector(".kk-summary"), t = c.querySelector(".kk-detail");
    if (!s || !t) return !!s;
    return !!(s.compareDocumentPosition(t) & Node.DOCUMENT_POSITION_FOLLOWING);
  });
  ok("(e) the summary precedes the detail body", orderOk);

  // ===== (f) 📋 on the COMPOSE-generated --command block copies the EXACT text via execCommand =====
  const copyBtn = page.locator(`${card} .kk-detail .kk-copy-code`);
  ok("(f) the 📋 copy button is present on the compose --command block", (await copyBtn.count()) === 1);
  await page.evaluate(() => { window.__legacyCopied = []; });
  await copyBtn.first().click();
  await page.waitForTimeout(60);
  const copied = await page.evaluate(() => window.__legacyCopied);
  ok("(f) ⭐ clicking 📋 legacy-copies the EXACT compose --command (clipboard undefined → execCommand)", copied.length === 1 && copied[0] === CMD, `got=${JSON.stringify(copied)}`);
  const toast = page.locator("#kk-copy-toast");
  ok("(f) a 'copié !' toast confirms the copy", (await toast.count()) === 1 && (await toast.isVisible()) && /copié/i.test((await toast.textContent()) || ""));

  await browser.close();
  console.log(`\ndashboard.kiosk-compose-real.accept.mjs — REAL 'decision compose' output rendered in the http-LAN (non-secure) KIOSK`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: real compose output renders correctly (files split / .md→viewer / code→blob / link→<a> / summary / copy)"}`);
  process.exit(failures ? 1 : 0);
}

main()
  .catch((e) => { console.error("dashboard.kiosk-compose-real.accept.mjs crashed:", e); process.exit(2); })
  .finally(() => { try { rmSync(tmp, { recursive: true, force: true }); } catch { /* best-effort */ } });
